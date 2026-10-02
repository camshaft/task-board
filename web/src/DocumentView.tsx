import { type CSSProperties, useEffect, useRef, useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { type DocumentComment, type DocumentVersion, ipfsUrl } from './api'
import { DocStatusChip } from './Documents'
import { useBoardContext } from './Layout'
import { Markdown, Mermaid, VegaLite } from './markdown'
import { asQuote, captureSelectionQuote, findQuoteRange, type RegionQuote } from './annotations'
import {
  approveDocument,
  commentDocument,
  requestDocumentChanges,
  resolveDocumentComment,
  setDocumentPath,
  submitDocumentForReview,
  useDocument,
  useDocumentComments,
  useDocumentContent,
  useDocumentExternalLinks,
  useExternalNameResolver,
} from './resources'
import { AuthorLabel, AutoGrowTextarea, relTime } from './ui'

// Read-only document viewer plus the review surface: metadata, the tasks it backs, its
// immutable version history (each CID resolved through the IPFS gateway client-side), review
// actions (submit-for-review / approve / request-changes) driven by the current status, and a
// threaded comment panel with resolve + one-level replies. Backed by the resource store, so it
// live-updates as review actions / comments land from any client (document.* SSE → touched()).
// In-app markdown rendering of the current version is a follow-up slice (needs a markdown dep).
export default function DocumentView() {
  const { documentId } = useParams()
  const { actor } = useBoardContext()
  const id = Number(documentId)
  const { data: doc, error: docError, loading } = useDocument(id)
  const { data: content } = useDocumentContent(id)
  const { data: comments = [] } = useDocumentComments(id)
  const { data: externalLinks = [] } = useDocumentExternalLinks(id)
  const extName = useExternalNameResolver()
  const [actionError, setActionError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [draft, setDraft] = useState('')
  const [replyTo, setReplyTo] = useState<number | null>(null)
  // Wiki-path filing: null = not editing, string = editing this path draft ('' clears/unfiles).
  const [editPath, setEditPath] = useState<string | null>(null)
  // Select-to-comment: the rendered current-version content, a pending region from a text
  // selection over it, and a nonce bumped when the content finishes loading (so the highlight
  // pass runs once the DOM is populated).
  const contentRef = useRef<HTMLDivElement>(null)
  const [region, setRegion] = useState<RegionQuote | null>(null)
  const [contentNonce, setContentNonce] = useState(0)
  // Inline anchored-comment pins: the wrapper the pins/popovers position against, the computed
  // pins (one per anchored region present in the shown version, keyed by quoted text), which
  // thread's popover is open, and the vertical offset of the pending-selection compose popover.
  // `layout` is bumped on resize so pin offsets recompute against the new geometry.
  const wrapRef = useRef<HTMLDivElement>(null)
  const [pins, setPins] = useState<{ key: string; top: number; count: number }[]>([])
  const [openKey, setOpenKey] = useState<string | null>(null)
  const [selTop, setSelTop] = useState<number | null>(null)
  const [layout, setLayout] = useState(0)
  // Long threads collapse older top-level comments behind a "show N earlier" button, keeping the
  // latest few in view (their replies stay with them). Recent replies are what usually matter.
  const [showAllComments, setShowAllComments] = useState(false)
  // Version diff panel (what changed between two versions), toggled under the version list.
  const [showDiff, setShowDiff] = useState(false)

  // Capture a text selection inside the rendered content as a text-quote region (exact + a little
  // prefix/suffix context, for disambiguation + highlight matching against the shown version).
  function captureSelection() {
    const el = contentRef.current
    if (!el) return
    const quote = captureSelectionQuote(el)
    if (!quote) return
    // Anchor the inline compose popover to the selection's vertical position within the wrapper,
    // and close any open thread so the two popovers never stack.
    const sel = window.getSelection()
    const wrap = wrapRef.current
    const rng = sel && sel.rangeCount > 0 ? sel.getRangeAt(0) : null
    if (wrap && rng) setSelTop(rng.getBoundingClientRect().top - wrap.getBoundingClientRect().top)
    setOpenKey(null)
    setRegion(quote)
  }

  // Run a mutation and surface its error. The wrappers invalidate the document + its comments
  // through touched(), so the subscribed hooks refetch and this component re-renders — no
  // manual reload needed.
  async function act(fn: () => Promise<unknown>) {
    setBusy(true)
    setActionError(null)
    try {
      await fn()
    } catch (e) {
      setActionError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  // The bottom composer posts a doc-level comment or a reply (region-anchored comments now come
  // from the inline selection popover, so this path never anchors).
  async function addComment() {
    const body = draft.trim()
    if (!body) return
    await act(() => commentDocument(id, { body, principal: actor, reply_to: replyTo ?? undefined }))
    setDraft('')
    setReplyTo(null)
  }

  // Post a comment anchored to the current text selection (from the inline SelectionComposer),
  // scoped to the current version so the highlight matches what the author saw.
  async function addAnchoredComment(body: string) {
    await act(() =>
      commentDocument(id, {
        body,
        principal: actor,
        region: region ?? undefined,
        version_id: doc?.current_version?.id ?? undefined,
      }),
    )
    setRegion(null)
    setSelTop(null)
  }

  // Post a reply from inside a thread popover (attaches to the region's top-level comment).
  async function addReply(parentId: number, body: string) {
    await act(() => commentDocument(id, { body, principal: actor, reply_to: parentId }))
  }

  // Highlight every region-anchored comment's quote in the shown content via the CSS Custom
  // Highlight API — no DOM surgery, so it layers over the rendered markdown. Re-runs when the
  // comments change or the content (re)loads. Degrades to nothing where the API is absent.
  useEffect(() => {
    const el = contentRef.current
    const highlights = (globalThis.CSS as unknown as { highlights?: Map<string, unknown> })?.highlights
    const HighlightCtor = (globalThis as unknown as { Highlight?: new () => { add: (r: Range) => void; size: number } }).Highlight
    if (!el || !highlights || !HighlightCtor) return
    const hl = new HighlightCtor()
    for (const c of comments) {
      const q = asQuote(c.region)
      if (!q) continue
      const r = findQuoteRange(el, q.exact, q.prefix)
      if (r) hl.add(r)
    }
    if (hl.size > 0) highlights.set('tb-region', hl as unknown)
    else highlights.delete('tb-region')
    return () => {
      highlights.delete('tb-region')
    }
  }, [comments, contentNonce])

  // Position an inline comment pin for every region-anchored top-level comment whose quote is
  // present in the shown content. Grouped by quoted text so several comments on the same passage
  // share one pin (its count includes replies). Offsets are measured against the wrapper, so the
  // pins sit in the right gutter aligned to the highlighted line and scroll with the content.
  useEffect(() => {
    const el = contentRef.current
    const wrap = wrapRef.current
    if (!el || !wrap) return
    const wrapTop = wrap.getBoundingClientRect().top
    const byQuote = new Map<string, { top: number; count: number }>()
    for (const c of comments) {
      if (c.reply_to != null) continue
      const q = asQuote(c.region)
      if (!q) continue
      const replies = comments.filter((x) => x.reply_to === c.id).length
      const existing = byQuote.get(q.exact)
      if (existing) {
        existing.count += 1 + replies
        continue
      }
      const r = findQuoteRange(el, q.exact, q.prefix)
      if (!r) continue
      byQuote.set(q.exact, { top: r.getBoundingClientRect().top - wrapTop, count: 1 + replies })
    }
    setPins([...byQuote.entries()].map(([key, v]) => ({ key, top: v.top, count: v.count })))
  }, [comments, contentNonce, layout])

  // Recompute pin geometry when the viewport resizes (line wrapping shifts vertical offsets).
  useEffect(() => {
    const onResize = () => setLayout((n) => n + 1)
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [])

  // A document URL opened directly with a #section fragment (e.g. a doc deep-link posted to
  // Slack, task 790) should land scrolled to that heading, not just at the top. Mirrors the
  // in-app anchor click's target lookup (markdown.tsx's scrollToFragment), but runs once the
  // content has actually rendered (contentNonce) instead of on a click, since nothing was
  // clicked to trigger it.
  useEffect(() => {
    const frag = window.location.hash.slice(1)
    if (!frag) return
    let target: HTMLElement | null = null
    try {
      target = document.getElementById(decodeURIComponent(frag))
    } catch {
      target = document.getElementById(frag)
    }
    target?.scrollIntoView({ block: 'start' })
  }, [documentId, contentNonce])

  function requestChanges() {
    const note = window.prompt('What needs to change? (optional note)') ?? undefined
    void act(() => requestDocumentChanges(id, { principal: actor, note: note || undefined }))
  }

  async function savePath() {
    if (editPath === null) return
    // Normalize: trim, drop leading/trailing slashes, collapse doubles. Empty clears the filing.
    const path = editPath.trim().replace(/^\/+|\/+$/g, '').replace(/\/{2,}/g, '/')
    await act(() => setDocumentPath(id, { path, principal: actor }))
    setEditPath(null)
  }

  const error = actionError ?? docError?.message ?? null
  const tags = Array.isArray(doc?.metadata?.tags) ? (doc!.metadata.tags as unknown[]) : []
  // Show the doc_7 A8 main-body word-count badge only on design docs -- the ~700-word budget is the
  // design-doc convention, so it would mislead on a charter/tenet/note (task_933). Design docs carry
  // the "design" tag (the established doc-type tag convention); the count itself rides along on the
  // content read, so no extra fetch beyond useDocumentContent.
  const isDesignDoc = tags.some((t) => typeof t === 'string' && t.toLowerCase() === 'design')
  const wordCount = content?.main_body_word_count
  const wordBudget = content?.main_body_word_budget ?? 700
  const overBudget = wordCount != null && wordCount > wordBudget

  // Available review actions depend on status: a draft (or one with changes requested) can be
  // submitted; a doc in review can be approved or bounced back. `operator_review` is the gated
  // operator-approval state (submit-to-operator-review -> operator_review -> approved); it is the
  // state cameron's pending docs sit in, so the Approve / Request-changes controls must show there
  // too, not only in the earlier agent `in_review` stage (task_881 -- the control had gone missing
  // for the entire operator-approval queue).
  const status = doc?.status
  const canSubmit = status === 'draft' || status === 'changes_requested'
  const canReview = status === 'in_review' || status === 'operator_review'

  // Thread the comments: top-level ones in order, each followed by its (one-level) replies.
  const topLevel = comments.filter((c) => c.reply_to == null)
  const repliesOf = (cid: number) => comments.filter((c) => c.reply_to === cid)

  // Anchored top-level comments grouped by quoted text — one inline pin + thread popover per
  // region. Keyed by `exact` to match the pins computed in the position effect above.
  const anchoredGroups = new Map<string, DocumentComment[]>()
  for (const c of topLevel) {
    const q = asQuote(c.region)
    if (!q) continue
    const arr = anchoredGroups.get(q.exact) ?? []
    arr.push(c)
    anchoredGroups.set(q.exact, arr)
  }
  const versionNo = (vid: number | null) =>
    vid == null ? null : (doc?.versions.find((v) => v.id === vid)?.version_no ?? null)

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="flex items-center gap-3 border-b border-[var(--color-border)] px-5 py-3">
        <Link to="/documents" className="text-xs text-[var(--color-muted)] hover:text-sky-300">
          ← Documents
        </Link>
        <h1 className="truncate text-sm font-semibold">{doc?.title ?? `Document #${id}`}</h1>
        {doc && <DocStatusChip status={doc.status} />}
        {/* Review actions, right-aligned. */}
        {doc && (
          <div className="ml-auto flex items-center gap-2">
            {canSubmit && (
              <button
                disabled={busy}
                onClick={() => void act(() => submitDocumentForReview(id, { principal: actor }))}
                className="rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40"
              >
                Submit for review
              </button>
            )}
            {canReview && (
              <>
                <button
                  disabled={busy}
                  onClick={() => void act(() => approveDocument(id, { principal: actor }))}
                  className="rounded-md bg-emerald-600 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40"
                >
                  Approve
                </button>
                <button
                  disabled={busy}
                  onClick={requestChanges}
                  className="rounded-md px-2.5 py-1 text-xs ring-1 ring-inset ring-amber-500/40 text-amber-300 hover:bg-amber-500/10 disabled:opacity-40"
                >
                  Request changes
                </button>
              </>
            )}
          </div>
        )}
      </div>

      {error && (
        <div className="border-b border-rose-500/30 bg-rose-500/10 px-5 py-2 text-sm text-rose-300">
          {error}
        </div>
      )}
      {loading && !doc && <p className="px-5 py-3 text-sm text-[var(--color-muted)]">Loading…</p>}

      {doc && (
        <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
          {/* Meta line: author, tags, approval. */}
          <div className="mb-4 flex flex-wrap items-center gap-2 text-xs text-[var(--color-muted)]">
            {doc.created_by && (
              <span>
                by <span className="font-mono">{doc.created_by}</span>
              </span>
            )}
            <span>· updated {relTime(doc.updated_at)}</span>
            {doc.approved_version_id != null && (
              <span className="text-emerald-300">
                · approved v
                {doc.versions.find((v) => v.id === doc.approved_version_id)?.version_no ?? '?'}
                {doc.approved_by ? ` by ${doc.approved_by}` : ''}
              </span>
            )}
            {tags.map((t, i) => (
              <span key={i} className="rounded bg-[var(--color-panel-2)] px-1.5 py-0.5">
                #{String(t)}
              </span>
            ))}
            {isDesignDoc && wordCount != null && (
              <span
                title={
                  overBudget
                    ? `Main body is ${wordCount} prose words, over the ~${wordBudget}-word doc_7 A8 budget`
                    : `Main-body word count (doc_7 A8), budget ~${wordBudget}`
                }
                className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 font-medium ring-1 ring-inset ${
                  overBudget
                    ? 'bg-amber-500/15 text-amber-300 ring-amber-500/30'
                    : 'bg-[var(--color-panel-2)] text-[var(--color-muted)] ring-[var(--color-border)]'
                }`}
              >
                {wordCount} / {wordBudget} words
              </span>
            )}
          </div>

          {/* External links (task 707): bridged URLs attached to this doc (e.g. a chorus page),
              from the external-links model. Open in a new tab; labeled by source. */}
          {externalLinks.some((l) => l.metadata?.url) && (
            <div className="mb-4 flex flex-wrap items-center gap-2 text-xs">
              <span className="text-[var(--color-muted)]">External link</span>
              {externalLinks
                .filter((l) => l.metadata?.url)
                .map((l) => (
                  <a
                    key={l.id}
                    href={l.metadata.url}
                    target="_blank"
                    rel="noreferrer noopener"
                    title={`${l.source}: ${l.metadata.url}`}
                    className="inline-flex items-center gap-1 rounded bg-[var(--color-panel-2)] px-2 py-0.5 font-mono text-sky-400 hover:text-sky-300"
                  >
                    {l.source}
                    <span aria-hidden>↗</span>
                  </a>
                ))}
            </div>
          )}

          {/* Wiki filing: the slash-path this doc lives under in the /wiki tree. Editable here;
              empty clears the filing. */}
          <div className="mb-4 flex flex-wrap items-center gap-2 text-xs">
            <span className="text-[var(--color-muted)]">Wiki path</span>
            {editPath === null ? (
              <>
                {doc.path ? (
                  <Link to="/wiki" className="font-mono text-sky-400 hover:text-sky-300">
                    {doc.path}
                  </Link>
                ) : (
                  <span className="text-[var(--color-muted)]">— not filed —</span>
                )}
                <button
                  disabled={busy}
                  onClick={() => setEditPath(doc.path ?? '')}
                  className="rounded px-1.5 py-0.5 text-sky-400 hover:bg-[var(--color-panel-2)] disabled:opacity-40"
                >
                  {doc.path ? 'edit' : 'file'}
                </button>
              </>
            ) : (
              <>
                <input
                  autoFocus
                  value={editPath}
                  disabled={busy}
                  onChange={(e) => setEditPath(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') void savePath()
                    else if (e.key === 'Escape') setEditPath(null)
                  }}
                  placeholder="e.g. architecture/board/events"
                  className="w-64 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 font-mono outline-none focus:border-sky-500/50"
                />
                <button
                  disabled={busy}
                  onClick={savePath}
                  className="rounded-md bg-sky-700 px-2 py-1 font-medium text-white disabled:opacity-40"
                >
                  Save
                </button>
                <button
                  onClick={() => setEditPath(null)}
                  className="rounded-md px-2 py-1 text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
                >
                  Cancel
                </button>
              </>
            )}
          </div>

          {/* Tasks this document backs. Each links into its board + task drawer via the
              summary's project_id; a task with no project (shouldn't happen) degrades to text. */}
          {doc.attached_tasks.length > 0 && (
            <div className="mb-4 flex flex-wrap items-center gap-x-1 gap-y-1 text-xs text-[var(--color-muted)]">
              <span>Backs:</span>
              {doc.attached_tasks.map((t, i) => (
                <span key={t.id}>
                  {t.project_id != null ? (
                    <Link
                      to={`/projects/${t.project_id}/tasks/${t.id}`}
                      className="text-sky-400 hover:text-sky-300"
                    >
                      #{t.id} {t.title}
                    </Link>
                  ) : (
                    <span>
                      #{t.id} {t.title}
                    </span>
                  )}
                  {i < doc.attached_tasks.length - 1 && <span>,</span>}
                </span>
              ))}
            </div>
          )}

          {/* Deprecated / superseded banner (task 722/725): a deprecated doc stays visible but
              carries this notice, linking to its replacement when superseded. */}
          {doc.deprecated_at && (
            <div className="mb-4 rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-300">
              <span className="font-semibold uppercase tracking-wide">Deprecated</span>
              {doc.superseded_by != null && (
                <>
                  {' · superseded by '}
                  <Link
                    to={`/documents/${doc.superseded_by}`}
                    className="underline decoration-dotted underline-offset-2 hover:text-amber-200"
                  >
                    document {doc.superseded_by}
                  </Link>
                </>
              )}
              <span className="ml-1 opacity-70"> · {relTime(doc.deprecated_at)}</span>
            </div>
          )}

          {/* Current version, rendered inline by its content_type (markdown / image / pdf / code
              / download fallback). Content resolves through the IPFS gateway client-side. */}
          {doc.current_version && (
            <div ref={wrapRef} className="relative mb-6 flex">
              <div className="min-w-0 flex-1">
                <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
                  Current version
                  <span className="ml-2 font-mono text-[10px] normal-case tracking-normal">
                    {doc.current_version.content_type ?? 'text/markdown'}
                  </span>
                </h2>
                <div ref={contentRef} onMouseUp={captureSelection}>
                  <DocContent
                    key={doc.current_version.id}
                    version={doc.current_version}
                    onLoaded={() => setContentNonce((n) => n + 1)}
                  />
                </div>
              </div>
              {/* Right gutter reserving space for the inline comment pins. */}
              <div className="w-8 shrink-0" aria-hidden />
              {/* One pin per anchored region present in this version; click toggles its thread. */}
              {pins.map((p) => (
                <button
                  key={p.key}
                  style={{ top: p.top }}
                  onClick={() => setOpenKey(openKey === p.key ? null : p.key)}
                  title="View inline comment thread"
                  className={`absolute right-0 flex h-6 items-center gap-0.5 rounded-full border border-amber-500/40 bg-[var(--color-panel)] px-1.5 text-[11px] leading-none text-amber-300 shadow-sm hover:bg-amber-500/10 ${
                    openKey === p.key ? 'ring-1 ring-amber-400' : ''
                  }`}
                >
                  <span aria-hidden>💬</span>
                  {p.count > 1 && <span className="font-mono">{p.count}</span>}
                </button>
              ))}
              {openKey &&
                (() => {
                  const pin = pins.find((p) => p.key === openKey)
                  const group = anchoredGroups.get(openKey)
                  if (!pin || !group || group.length === 0) return null
                  return (
                    <ThreadPopover
                      style={{ top: pin.top }}
                      group={group}
                      repliesOf={repliesOf}
                      busy={busy}
                      resolveExternal={extName}
                      onResolve={(cid) => void act(() => resolveDocumentComment(id, cid, { principal: actor }))}
                      onReply={addReply}
                      onClose={() => setOpenKey(null)}
                    />
                  )
                })()}
              {region && selTop != null && (
                <SelectionComposer
                  style={{ top: selTop }}
                  quote={region}
                  actor={actor}
                  onSubmit={addAnchoredComment}
                  onCancel={() => {
                    setRegion(null)
                    setSelTop(null)
                  }}
                />
              )}
            </div>
          )}

          {/* Version history. Each CID resolves through the IPFS gateway (client-side). */}
          <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            Versions
          </h2>
          <ul className="space-y-1.5">
            {doc.versions.map((v) => {
              const isCurrent = v.id === doc.current_version_id
              const isApproved = v.id === doc.approved_version_id
              return (
                <li
                  key={v.id}
                  className="flex items-center gap-3 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-3 py-2 text-sm"
                >
                  <span className="font-mono text-xs text-[var(--color-muted)]">v{v.version_no}</span>
                  {isCurrent && <span className="text-[10px] uppercase text-sky-300">current</span>}
                  {isApproved && (
                    <span className="text-[10px] uppercase text-emerald-300">approved</span>
                  )}
                  {v.content_type && v.content_type !== 'text/markdown' && (
                    <span className="rounded bg-[var(--color-panel-2)] px-1.5 py-0.5 font-mono text-[10px] text-[var(--color-muted)]">
                      {v.content_type}
                    </span>
                  )}
                  <a
                    href={ipfsUrl(v.cid, v.content_type)}
                    className="min-w-0 flex-1 truncate font-mono text-xs text-sky-400 underline decoration-dotted underline-offset-2 hover:text-sky-300"
                    title={`open ${v.cid}`}
                  >
                    {v.cid}
                  </a>
                  {v.summary && (
                    <span className="hidden truncate text-xs text-[var(--color-muted)] md:inline">
                      {v.summary}
                    </span>
                  )}
                  {v.created_by && (
                    <span className="font-mono text-[11px] text-[var(--color-muted)]">
                      {v.created_by}
                    </span>
                  )}
                  <span className="text-[11px] text-[var(--color-muted)]">{relTime(v.created_at)}</span>
                </li>
              )
            })}
          </ul>

          {/* Diff mode: compare any two versions to see what changed (fetched + diffed client-side). */}
          {doc.versions.length >= 2 && (
            <div className="mt-2">
              <button
                onClick={() => setShowDiff((s) => !s)}
                className="text-xs text-sky-400 hover:text-sky-300"
              >
                {showDiff ? 'Hide diff' : 'Compare versions →'}
              </button>
              {showDiff && (
                <DocDiff
                  versions={doc.versions}
                  currentId={doc.current_version_id}
                  approvedId={doc.approved_version_id}
                />
              )}
            </div>
          )}

          {/* Wiki link graph: pages THIS doc links to ([[path]] in its content) and pages that
              link back to it. Outbound targets that aren't filed yet render as dangling red-links.
              Populated when versions are published with raw content (the server indexes the links). */}
          {(doc.outbound_links.length > 0 || doc.backlinks.length > 0) && (
            <div className="mt-6 grid grid-cols-1 gap-4 sm:grid-cols-2">
              <div>
                <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
                  Links to ({doc.outbound_links.length})
                </h2>
                <ul className="space-y-1">
                  {doc.outbound_links.map((l, i) => (
                    <li key={i} className="text-sm">
                      {l.target_document_id != null ? (
                        <Link
                          to={`/documents/${l.target_document_id}`}
                          className="text-sky-400 hover:text-sky-300"
                          title={l.target_path}
                        >
                          {l.label ?? l.target_title ?? l.target_path}
                        </Link>
                      ) : (
                        <Link
                          to="/wiki"
                          className="text-rose-400/90 hover:text-rose-300"
                          title={`No page filed at "${l.target_path}" yet`}
                        >
                          {l.label ?? l.target_path}
                        </Link>
                      )}
                      <span className="ml-1 font-mono text-[11px] text-[var(--color-muted)]">
                        {l.target_path}
                      </span>
                    </li>
                  ))}
                  {doc.outbound_links.length === 0 && (
                    <li className="text-xs text-[var(--color-muted)]">None.</li>
                  )}
                </ul>
              </div>
              <div>
                <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
                  Linked from ({doc.backlinks.length})
                </h2>
                <ul className="space-y-1">
                  {doc.backlinks.map((b) => (
                    <li key={b.id} className="flex items-center gap-2 text-sm">
                      <DocStatusChip status={b.status} />
                      <Link
                        to={`/documents/${b.id}`}
                        className="min-w-0 flex-1 truncate hover:text-sky-300"
                      >
                        {b.title}
                      </Link>
                      {b.path && (
                        <span className="font-mono text-[11px] text-[var(--color-muted)]">
                          {b.path}
                        </span>
                      )}
                    </li>
                  ))}
                  {doc.backlinks.length === 0 && (
                    <li className="text-xs text-[var(--color-muted)]">Nothing links here yet.</li>
                  )}
                </ul>
              </div>
            </div>
          )}

          {/* Transclusion graph: docs THIS one embeds (![[path]]) and docs that embed it (the
              "dependents before you change it" view). Inline embed COMPOSITION is a later slice
              (needs the content gateway); this is the reference view. */}
          {(doc.embeds.length > 0 || doc.embedded_by.length > 0) && (
            <div className="mt-6 grid grid-cols-1 gap-4 sm:grid-cols-2">
              <div>
                <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
                  Embeds ({doc.embeds.length})
                </h2>
                <ul className="space-y-1">
                  {doc.embeds.map((e, i) => (
                    <li key={i} className="text-sm">
                      {e.target_document_id != null ? (
                        <Link
                          to={`/documents/${e.target_document_id}`}
                          className="text-sky-400 hover:text-sky-300"
                          title={e.target_path}
                        >
                          {e.label ?? e.target_title ?? e.target_path}
                        </Link>
                      ) : (
                        <Link
                          to="/wiki"
                          className="text-rose-400/90 hover:text-rose-300"
                          title={`No page filed at "${e.target_path}" yet`}
                        >
                          {e.label ?? e.target_path}
                        </Link>
                      )}
                      {e.target_version_id != null && (
                        <span className="ml-1 text-[10px] uppercase text-[var(--color-muted)]">pinned</span>
                      )}
                      {e.region && (
                        <span className="ml-1 font-mono text-[11px] text-[var(--color-muted)]">
                          #{e.region}
                        </span>
                      )}
                    </li>
                  ))}
                </ul>
              </div>
              <div>
                <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
                  Embedded by ({doc.embedded_by.length})
                </h2>
                <ul className="space-y-1">
                  {doc.embedded_by.map((e) => (
                    <li key={e.id} className="flex items-center gap-2 text-sm">
                      <DocStatusChip status={e.status} />
                      <Link
                        to={`/documents/${e.id}`}
                        className="min-w-0 flex-1 truncate hover:text-sky-300"
                      >
                        {e.title}
                      </Link>
                      {e.path && (
                        <span className="font-mono text-[11px] text-[var(--color-muted)]">
                          {e.path}
                        </span>
                      )}
                    </li>
                  ))}
                </ul>
              </div>
            </div>
          )}

          {/* Review comments: threaded (top-level + one-level replies), each open comment
              resolvable. Region-anchored comments show the version they target. */}
          <h2 className="mb-2 mt-6 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            Comments ({comments.length})
          </h2>
          {(() => {
            // Keep the latest few threads in view; collapse older top-level comments behind an
            // expander once the thread is long enough to be worth hiding (chronological order kept).
            const VISIBLE = 3
            const hidden = topLevel.length - VISIBLE
            const collapsed = !showAllComments && hidden >= 2
            const shown = collapsed ? topLevel.slice(-VISIBLE) : topLevel
            return (
              <ul className="space-y-2">
                {collapsed && (
                  <li>
                    <button
                      onClick={() => setShowAllComments(true)}
                      className="w-full rounded-md border border-dashed border-[var(--color-border)] px-3 py-2 text-xs text-[var(--color-muted)] hover:border-sky-500/40 hover:text-sky-300"
                    >
                      Show {hidden} earlier comment{hidden === 1 ? '' : 's'}
                    </button>
                  </li>
                )}
                {shown.map((c) => (
                  <li key={c.id}>
                    <CommentCard
                      c={c}
                      versionNo={versionNo(c.version_id)}
                      busy={busy}
                      resolveExternal={extName}
                      onResolve={() => void act(() => resolveDocumentComment(id, c.id, { principal: actor }))}
                      onReply={() => setReplyTo(replyTo === c.id ? null : c.id)}
                      replying={replyTo === c.id}
                    />
                    {repliesOf(c.id).length > 0 && (
                      <ul className="mt-1.5 space-y-1.5 border-l border-[var(--color-border)] pl-4">
                        {repliesOf(c.id).map((r) => (
                          <li key={r.id}>
                            <CommentCard
                              c={r}
                              versionNo={versionNo(r.version_id)}
                              busy={busy}
                              resolveExternal={extName}
                              onResolve={() => void act(() => resolveDocumentComment(id, r.id, { principal: actor }))}
                            />
                          </li>
                        ))}
                      </ul>
                    )}
                  </li>
                ))}
                {comments.length === 0 && (
                  <li className="text-sm text-[var(--color-muted)]">No comments yet.</li>
                )}
              </ul>
            )
          })()}

          {/* Composer. Replies target the selected comment; otherwise a doc-level comment. A text
              selection over the content opens the inline SelectionComposer popover instead. */}
          {replyTo == null && comments.length === 0 && (
            <p className="mt-3 text-[11px] text-[var(--color-muted)]">
              Tip: select text in the content above to comment on it inline.
            </p>
          )}
          {/* Show a submit failure right at the composer (task_1201): the API validation reason,
              not just an opaque 4xx. */}
          {actionError && (
            <p className="mt-2 rounded-md border border-rose-500/30 bg-rose-500/10 px-3 py-2 text-xs text-rose-300">
              {actionError}
            </p>
          )}
          <div className="mt-2 flex items-end gap-2">
            <AutoGrowTextarea
              value={draft}
              onChange={setDraft}
              onSubmit={addComment}
              placeholder={
                replyTo != null ? `Reply to #${replyTo} as ${actor}…` : `Comment as ${actor}…`
              }
              className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-3 py-2 text-sm outline-none focus:border-sky-500/50"
            />
            {replyTo != null && (
              <button
                onClick={() => setReplyTo(null)}
                className="rounded-md px-2 py-2 text-xs text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
              >
                cancel reply
              </button>
            )}
            <button
              onClick={addComment}
              disabled={busy || !draft.trim()}
              className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white disabled:opacity-40"
            >
              Send
            </button>
          </div>
        </div>
      )}
    </main>
  )
}

// Which renderer a MIME type maps to. Absent/unknown text defaults to markdown (the board's
// own default), so a plain doc still renders richly.
type DocKind = 'markdown' | 'mermaid' | 'vega' | 'image' | 'pdf' | 'json' | 'text' | 'other'
function kindOf(contentType: string | null): DocKind {
  const t = (contentType ?? 'text/markdown').toLowerCase().split(';')[0].trim()
  if (t.startsWith('image/')) return 'image'
  if (t === 'application/pdf') return 'pdf'
  if (t === 'text/vnd.mermaid' || t === 'text/x-mermaid') return 'mermaid'
  if (t === 'application/vnd.vegalite+json' || t === 'application/vnd.vega+json') return 'vega'
  if (t === 'text/markdown' || t === 'text/x-markdown' || t === '') return 'markdown'
  if (t === 'application/json') return 'json'
  if (t.startsWith('text/')) return 'text'
  return 'other'
}

// Render one document version's content inline, dispatched by its content_type. Text-shaped
// kinds (markdown/json/text) are fetched from the IPFS gateway as text; binary kinds (image/pdf)
// are pointed at the gateway URL directly. Every path degrades gracefully to a raw link if the
// gateway is unreachable or the type is unknown — a missing gateway never breaks the view.
function DocContent({ version, onLoaded }: { version: DocumentVersion; onLoaded?: () => void }) {
  const kind = kindOf(version.content_type)
  const needsText =
    kind === 'markdown' || kind === 'json' || kind === 'text' || kind === 'mermaid' || kind === 'vega'
  const url = ipfsUrl(version.cid, version.content_type)
  const [text, setText] = useState<string | null>(null)
  const [status, setStatus] = useState<'idle' | 'loading' | 'error'>(needsText ? 'loading' : 'idle')

  // Keyed on version id by the parent, so a version change remounts with fresh initial state —
  // no synchronous reset here; the effect just fetches (a real external-system sync). onLoaded
  // lets the parent re-run its highlight pass once the content DOM is populated.
  useEffect(() => {
    if (!needsText) {
      onLoaded?.()
      return
    }
    let cancelled = false
    fetch(url)
      .then((r) => {
        if (!r.ok) throw new Error(`gateway ${r.status}`)
        return r.text()
      })
      .then((t) => {
        if (!cancelled) {
          setText(t)
          setStatus('idle')
          onLoaded?.()
        }
      })
      .catch(() => {
        if (!cancelled) setStatus('error')
      })
    return () => {
      cancelled = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [url, needsText])

  const raw = (
    <a
      href={url}
      className="font-mono text-xs text-sky-400 underline decoration-dotted underline-offset-2 hover:text-sky-300"
    >
      open raw ({version.cid})
    </a>
  )

  if (kind === 'image') {
    return (
      <img
        src={url}
        alt={`document version ${version.version_no}`}
        className="max-h-[70vh] rounded-md border border-[var(--color-border)]"
      />
    )
  }
  if (kind === 'pdf') {
    return (
      <iframe
        src={url}
        title={`document version ${version.version_no}`}
        className="h-[70vh] w-full rounded-md border border-[var(--color-border)]"
      />
    )
  }
  if (kind === 'other') {
    return (
      <div className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] p-3 text-sm text-[var(--color-muted)]">
        No inline preview for this content type. {raw}
      </div>
    )
  }
  // Text-shaped kinds.
  if (status === 'loading') {
    return <p className="text-sm text-[var(--color-muted)]">Loading content…</p>
  }
  if (status === 'error' || text === null) {
    return (
      <div className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] p-3 text-sm text-[var(--color-muted)]">
        Couldn't load content from the gateway. {raw}
      </div>
    )
  }
  if (kind === 'mermaid') {
    return <Mermaid code={text} />
  }
  if (kind === 'vega') {
    return <VegaLite code={text} />
  }
  if (kind === 'markdown') {
    return (
      <div className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] p-4">
        {/* anchors: headings get a linkable slug id + a `#`-depth margin marker (section links). */}
        <Markdown source={text} className="text-sm" anchors />
      </div>
    )
  }
  // json (pretty-printed if valid) / other text → code block.
  let body = text
  if (kind === 'json') {
    try {
      body = JSON.stringify(JSON.parse(text), null, 2)
    } catch {
      // Not valid JSON after all — show it verbatim.
    }
  }
  return (
    <pre className="max-h-[70vh] overflow-auto rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] p-3 font-mono text-xs">
      <code>{body}</code>
    </pre>
  )
}

function CommentCard({
  c,
  versionNo,
  busy,
  resolveExternal,
  onResolve,
  onReply,
  replying,
  hideQuote,
}: {
  c: DocumentComment
  versionNo: number | null
  busy: boolean
  resolveExternal: (id: string) => string
  onResolve: () => void
  onReply?: () => void
  replying?: boolean
  hideQuote?: boolean
}) {
  const resolved = c.status === 'resolved'
  return (
    <div
      className={`rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] p-3 ${
        resolved ? 'opacity-60' : ''
      }`}
    >
      <div className="mb-1 flex items-center gap-2 text-xs text-[var(--color-muted)]">
        <AuthorLabel
          author={c.author}
          externalAuthor={c.external_author}
          resolveExternal={resolveExternal}
        />
        {versionNo != null && <span>· on v{versionNo}</span>}
        {c.region != null && <span title="region-anchored">· 📌</span>}
        <span>· {relTime(c.created_at)}</span>
        {resolved && <span className="text-emerald-300">· resolved</span>}
        <span className="ml-auto flex items-center gap-2">
          {onReply && (
            <button
              onClick={onReply}
              className={`hover:text-sky-300 ${replying ? 'text-sky-300' : ''}`}
            >
              reply
            </button>
          )}
          {!resolved && (
            <button disabled={busy} onClick={onResolve} className="hover:text-emerald-300">
              resolve
            </button>
          )}
        </span>
      </div>
      {/* Region-anchored comment: show the quoted excerpt it targets, so the anchor is visible
          even where the inline highlight can't match (e.g. an older version). Suppressed inside
          the thread popover, which already shows the quote once in its header. */}
      {!hideQuote && asQuote(c.region) && (
        <blockquote className="mb-1.5 border-l-2 border-amber-500/40 pl-2 text-xs italic text-[var(--color-muted)]">
          “{asQuote(c.region)!.exact}”
        </blockquote>
      )}
      <Markdown source={c.body} className="text-sm" />
    </div>
  )
}

// The inline thread popover anchored beside a highlighted region: the region's comment(s) and
// their replies, an inline reply box, and per-comment resolve — so a reader sees and continues
// the conversation right where the text is, without scrolling to the feed below.
function ThreadPopover({
  group,
  repliesOf,
  busy,
  resolveExternal,
  onResolve,
  onReply,
  onClose,
  style,
}: {
  group: DocumentComment[]
  repliesOf: (cid: number) => DocumentComment[]
  busy: boolean
  resolveExternal: (id: string) => string
  onResolve: (cid: number) => void
  onReply: (parentId: number, body: string) => Promise<void>
  onClose: () => void
  style?: CSSProperties
}) {
  const [draft, setDraft] = useState('')
  const [posting, setPosting] = useState(false)
  const quote = asQuote(group[0].region)
  const parentId = group[0].id

  async function send() {
    const body = draft.trim()
    if (!body || posting) return
    setPosting(true)
    try {
      await onReply(parentId, body)
      setDraft('')
    } finally {
      setPosting(false)
    }
  }

  return (
    <div
      style={style}
      className="absolute right-8 z-20 w-80 max-w-[calc(100%-3rem)] rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] shadow-lg"
    >
      <div className="flex items-start gap-2 border-b border-[var(--color-border)] px-3 py-2">
        {quote && (
          <blockquote className="line-clamp-2 min-w-0 flex-1 border-l-2 border-amber-500/40 pl-2 text-xs italic text-[var(--color-muted)]">
            “{quote.exact}”
          </blockquote>
        )}
        <button
          onClick={onClose}
          aria-label="Close thread"
          className="shrink-0 text-[var(--color-muted)] hover:text-rose-300"
        >
          ×
        </button>
      </div>
      <div className="max-h-72 space-y-2 overflow-y-auto p-2">
        {group.map((c) => (
          <div key={c.id}>
            <CommentCard
              c={c}
              versionNo={null}
              busy={busy}
              resolveExternal={resolveExternal}
              onResolve={() => onResolve(c.id)}
              hideQuote
            />
            {repliesOf(c.id).length > 0 && (
              <ul className="mt-1.5 space-y-1.5 border-l border-[var(--color-border)] pl-3">
                {repliesOf(c.id).map((r) => (
                  <li key={r.id}>
                    <CommentCard
                      c={r}
                      versionNo={null}
                      busy={busy}
                      resolveExternal={resolveExternal}
                      onResolve={() => onResolve(r.id)}
                      hideQuote
                    />
                  </li>
                ))}
              </ul>
            )}
          </div>
        ))}
      </div>
      <div className="flex items-end gap-2 border-t border-[var(--color-border)] p-2">
        <AutoGrowTextarea
          value={draft}
          onChange={setDraft}
          onSubmit={send}
          placeholder="Reply…"
          className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-xs outline-none focus:border-sky-500/50"
        />
        <button
          onClick={send}
          disabled={posting || !draft.trim()}
          className="rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40"
        >
          Reply
        </button>
      </div>
    </div>
  )
}

// The inline compose popover shown at a fresh text selection: the quoted excerpt plus a small
// composer, so a comment is written right at the passage instead of at the bottom of the page.
function SelectionComposer({
  quote,
  actor,
  onSubmit,
  onCancel,
  style,
}: {
  quote: RegionQuote
  actor: string
  onSubmit: (body: string) => Promise<void>
  onCancel: () => void
  style?: CSSProperties
}) {
  const [draft, setDraft] = useState('')
  const [posting, setPosting] = useState(false)

  async function send() {
    const body = draft.trim()
    if (!body || posting) return
    setPosting(true)
    try {
      await onSubmit(body)
    } finally {
      setPosting(false)
    }
  }

  return (
    <div
      style={style}
      className="absolute right-8 z-20 w-80 max-w-[calc(100%-3rem)] rounded-md border border-amber-500/40 bg-[var(--color-panel)] shadow-lg"
    >
      <div className="border-b border-[var(--color-border)] px-3 py-2">
        <blockquote className="line-clamp-2 border-l-2 border-amber-500/40 pl-2 text-xs italic text-[var(--color-muted)]">
          “{quote.exact}”
        </blockquote>
      </div>
      <div className="flex items-end gap-2 p-2">
        <AutoGrowTextarea
          value={draft}
          onChange={setDraft}
          onSubmit={send}
          placeholder={`Comment on selection as ${actor}…`}
          className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-xs outline-none focus:border-sky-500/50"
        />
        <button
          onClick={send}
          disabled={posting || !draft.trim()}
          className="rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40"
        >
          Comment
        </button>
        <button
          onClick={onCancel}
          aria-label="Cancel"
          className="rounded-md px-1.5 py-1 text-xs text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
        >
          ×
        </button>
      </div>
    </div>
  )
}

type DiffLine = { type: 'ctx' | 'add' | 'del'; text: string }

// Line-level diff via a longest-common-subsequence table. O(n·m) — fine for documents (hundreds
// of lines). Lines present in both are context; lines only in `a` are deletions, only in `b`
// additions. Not a minimal Myers diff, but stable and dependency-free.
function lineDiff(a: string[], b: string[]): DiffLine[] {
  const n = a.length
  const m = b.length
  const dp: number[][] = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0))
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1])
    }
  }
  const out: DiffLine[] = []
  let i = 0
  let j = 0
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.push({ type: 'ctx', text: a[i] })
      i++
      j++
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      out.push({ type: 'del', text: a[i++] })
    } else {
      out.push({ type: 'add', text: b[j++] })
    }
  }
  while (i < n) out.push({ type: 'del', text: a[i++] })
  while (j < m) out.push({ type: 'add', text: b[j++] })
  return out
}

// One run of a line, flagged `changed` when it differs between the paired versions. Rendering
// gives changed runs a stronger inline highlight so a reader sees the altered spans within a
// modified block, not the whole line.
type DiffPart = { text: string; changed: boolean }

// Split into word and whitespace runs so a reconstruction is loss-free (whitespace is preserved
// as its own token rather than collapsed).
function tokenize(s: string): string[] {
  return s.match(/\s+|\S+/g) ?? []
}

// Word-level diff of two single lines via an LCS over tokens. Shared tokens are unchanged on both
// sides; a token present only in `a` is a changed run on the removed side, only in `b` a changed
// run on the added side. Adjacent same-flag tokens are merged so the markup stays compact.
function tokenDiff(a: string, b: string): { aParts: DiffPart[]; bParts: DiffPart[] } {
  const ta = tokenize(a)
  const tb = tokenize(b)
  const n = ta.length
  const m = tb.length
  const dp: number[][] = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0))
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = ta[i] === tb[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1])
    }
  }
  const push = (arr: DiffPart[], text: string, changed: boolean) => {
    const last = arr[arr.length - 1]
    if (last && last.changed === changed) last.text += text
    else arr.push({ text, changed })
  }
  const aParts: DiffPart[] = []
  const bParts: DiffPart[] = []
  let i = 0
  let j = 0
  while (i < n && j < m) {
    if (ta[i] === tb[j]) {
      push(aParts, ta[i], false)
      push(bParts, tb[j], false)
      i++
      j++
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      push(aParts, ta[i++], true)
    } else {
      push(bParts, tb[j++], true)
    }
  }
  while (i < n) push(aParts, ta[i++], true)
  while (j < m) push(bParts, tb[j++], true)
  return { aParts, bParts }
}

// A rendered diff row: a context line, or a removed/added line carried as parts. Within a change
// block, each removed line is paired with the corresponding added line and word-diffed so only
// the altered spans highlight; unpaired removals/insertions carry the whole line as one part.
type DiffRow =
  | { kind: 'ctx'; text: string }
  | { kind: 'del' | 'add'; parts: DiffPart[] }

// Turn a flat line diff into rows, pairing removals with insertions inside each change block so a
// modified paragraph renders as a del line + an add line with inline chunk highlights, instead of
// one whole replaced line (task 747).
function buildRows(lines: DiffLine[]): DiffRow[] {
  const rows: DiffRow[] = []
  let i = 0
  while (i < lines.length) {
    if (lines[i].type === 'ctx') {
      rows.push({ kind: 'ctx', text: lines[i].text })
      i++
      continue
    }
    // A maximal run of consecutive non-context lines is one change block.
    const dels: string[] = []
    const adds: string[] = []
    while (i < lines.length && lines[i].type !== 'ctx') {
      if (lines[i].type === 'del') dels.push(lines[i].text)
      else adds.push(lines[i].text)
      i++
    }
    const pairs = Math.min(dels.length, adds.length)
    for (let k = 0; k < pairs; k++) {
      const { aParts, bParts } = tokenDiff(dels[k], adds[k])
      rows.push({ kind: 'del', parts: aParts })
      rows.push({ kind: 'add', parts: bParts })
    }
    for (let k = pairs; k < dels.length; k++) rows.push({ kind: 'del', parts: [{ text: dels[k], changed: false }] })
    for (let k = pairs; k < adds.length; k++) rows.push({ kind: 'add', parts: [{ text: adds[k], changed: false }] })
  }
  return rows
}

// Compare two document versions: pick a base + a compare version (default previous → current),
// fetch both through the IPFS gateway, and render an added/removed/context line diff client-side.
function DocDiff({
  versions,
  currentId,
  approvedId,
}: {
  versions: DocumentVersion[]
  currentId: number | null
  approvedId?: number | null
}) {
  const sorted = [...versions].sort((a, b) => a.version_no - b.version_no)
  const current = sorted.find((v) => v.id === currentId) ?? sorted[sorted.length - 1]
  const curIdx = sorted.indexOf(current)
  const prev = sorted[curIdx - 1] ?? sorted[0]
  // Default baseline: the approved version vs current (what review actually changed since sign-off),
  // falling back to the previous version when the doc has no approval or current *is* the approved
  // one (so the diff is never empty by default). The picker can still choose any pair (task 747).
  const approved = approvedId != null ? sorted.find((v) => v.id === approvedId) : undefined
  const defaultBase = approved && approved.id !== current.id ? approved : prev
  const [baseId, setBaseId] = useState<number>(defaultBase.id)
  const [cmpId, setCmpId] = useState<number>(current.id)
  const key = `${baseId}:${cmpId}`
  // Keyed so `loading` is derived (result.key !== key) rather than set synchronously in the effect.
  const [result, setResult] = useState<{ key: string; lines?: DiffLine[]; error?: string }>({
    key: '',
  })
  useEffect(() => {
    let cancelled = false
    const bv = versions.find((v) => v.id === baseId)
    const cv = versions.find((v) => v.id === cmpId)
    if (!bv || !cv) return
    const grab = (v: DocumentVersion) =>
      fetch(ipfsUrl(v.cid, v.content_type)).then((r) =>
        r.ok ? r.text() : Promise.reject(new Error(`gateway ${r.status}`)),
      )
    Promise.all([grab(bv), grab(cv)])
      .then(([a, b]) => {
        if (!cancelled) setResult({ key, lines: lineDiff(a.split('\n'), b.split('\n')) })
      })
      .catch((e) => {
        if (!cancelled) setResult({ key, error: (e as Error).message })
      })
    return () => {
      cancelled = true
    }
  }, [key, baseId, cmpId, versions])

  const loading = result.key !== key
  const adds = result.lines?.filter((l) => l.type === 'add').length ?? 0
  const dels = result.lines?.filter((l) => l.type === 'del').length ?? 0

  // Annotate the approved / current versions in the picker so the default baseline reads clearly.
  const label = (v: DocumentVersion) =>
    `v${v.version_no}${v.id === approvedId ? ' (approved)' : ''}${v.id === currentId ? ' (current)' : ''}`
  // The compare (newer) version's publish summary captions the diff -- "what changed in this
  // revision / what to look for" when reviewing it (task 758; reuses the existing version summary).
  const cmpVersion = sorted.find((v) => v.id === cmpId)

  return (
    <div className="mt-2 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] p-3">
      <div className="mb-2 flex flex-wrap items-center gap-2 text-xs text-[var(--color-muted)]">
        <span>Compare</span>
        <select
          value={baseId}
          onChange={(e) => setBaseId(Number(e.target.value))}
          className="rounded border border-[var(--color-border)] bg-[var(--color-panel-2)] px-1.5 py-1 text-xs"
        >
          {sorted.map((v) => (
            <option key={v.id} value={v.id}>
              {label(v)}
            </option>
          ))}
        </select>
        <span>→</span>
        <select
          value={cmpId}
          onChange={(e) => setCmpId(Number(e.target.value))}
          className="rounded border border-[var(--color-border)] bg-[var(--color-panel-2)] px-1.5 py-1 text-xs"
        >
          {sorted.map((v) => (
            <option key={v.id} value={v.id}>
              {label(v)}
            </option>
          ))}
        </select>
        {!loading && !result.error && (
          <span className="ml-auto font-mono">
            <span className="text-emerald-300">+{adds}</span>{' '}
            <span className="text-red-300">−{dels}</span>
          </span>
        )}
      </div>
      {cmpVersion?.summary && baseId !== cmpId && (
        <div className="mb-2 rounded border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1.5 text-xs">
          <span className="text-[var(--color-muted)]">What changed in v{cmpVersion.version_no}: </span>
          {cmpVersion.summary}
        </div>
      )}
      {loading ? (
        <p className="text-xs text-[var(--color-muted)]">Computing diff…</p>
      ) : result.error ? (
        <p className="text-xs text-[var(--color-muted)]">Couldn't load versions: {result.error}</p>
      ) : baseId === cmpId ? (
        <p className="text-xs text-[var(--color-muted)]">Pick two different versions to compare.</p>
      ) : (
        // Word-wrapped so long lines stay in the viewport (no horizontal scroll); each row carries
        // its parts, with changed spans given a stronger inline highlight for per-block chunk diffs.
        <div className="max-h-[60vh] overflow-y-auto overflow-x-hidden rounded bg-[var(--color-panel-2)] p-2 font-mono text-xs leading-relaxed">
          {buildRows(result.lines!).map((r, idx) => (
            <div
              key={idx}
              className={`flex gap-1 whitespace-pre-wrap break-words ${
                r.kind === 'add'
                  ? 'bg-emerald-500/10 text-emerald-300'
                  : r.kind === 'del'
                    ? 'bg-red-500/10 text-red-300'
                    : 'text-[var(--color-muted)]'
              }`}
            >
              <span className="shrink-0 select-none opacity-60">
                {r.kind === 'add' ? '+' : r.kind === 'del' ? '−' : ' '}
              </span>
              <span className="min-w-0 flex-1">
                {r.kind === 'ctx'
                  ? r.text || ' '
                  : r.parts.map((p, pi) =>
                      p.changed ? (
                        <span
                          key={pi}
                          className={
                            r.kind === 'add'
                              ? 'rounded-sm bg-emerald-500/30 text-emerald-200'
                              : 'rounded-sm bg-red-500/30 text-red-200'
                          }
                        >
                          {p.text}
                        </span>
                      ) : (
                        <span key={pi}>{p.text}</span>
                      ),
                    )}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}
