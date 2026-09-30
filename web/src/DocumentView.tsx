import { type CSSProperties, useEffect, useRef, useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { type DocumentComment, type DocumentVersion, ipfsUrl } from './api'
import { DocStatusChip } from './Documents'
import { useBoardContext } from './Layout'
import { Markdown, Mermaid, VegaLite } from './markdown'
import {
  approveDocument,
  commentDocument,
  requestDocumentChanges,
  resolveDocumentComment,
  setDocumentPath,
  submitDocumentForReview,
  useDocument,
  useDocumentComments,
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
  const { data: comments = [] } = useDocumentComments(id)
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
    const sel = window.getSelection()
    if (!el || !sel || sel.isCollapsed || !sel.anchorNode || !el.contains(sel.anchorNode)) return
    const exact = sel.toString().trim()
    if (exact.length < 2) return
    const full = el.textContent ?? ''
    const idx = full.indexOf(exact)
    // Anchor the inline compose popover to the selection's vertical position within the wrapper,
    // and close any open thread so the two popovers never stack.
    const wrap = wrapRef.current
    const rng = sel.rangeCount > 0 ? sel.getRangeAt(0) : null
    if (wrap && rng) setSelTop(rng.getBoundingClientRect().top - wrap.getBoundingClientRect().top)
    setOpenKey(null)
    setRegion({
      type: 'text-quote',
      exact,
      prefix: idx > 0 ? full.slice(Math.max(0, idx - 32), idx) : '',
      suffix: idx >= 0 ? full.slice(idx + exact.length, idx + exact.length + 32) : '',
    })
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
    await act(() => commentDocument(id, { body, author: actor, reply_to: replyTo ?? undefined }))
    setDraft('')
    setReplyTo(null)
  }

  // Post a comment anchored to the current text selection (from the inline SelectionComposer),
  // scoped to the current version so the highlight matches what the author saw.
  async function addAnchoredComment(body: string) {
    await act(() =>
      commentDocument(id, {
        body,
        author: actor,
        region: region ?? undefined,
        version_id: doc?.current_version?.id ?? undefined,
      }),
    )
    setRegion(null)
    setSelTop(null)
  }

  // Post a reply from inside a thread popover (attaches to the region's top-level comment).
  async function addReply(parentId: number, body: string) {
    await act(() => commentDocument(id, { body, author: actor, reply_to: parentId }))
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

  function requestChanges() {
    const note = window.prompt('What needs to change? (optional note)') ?? undefined
    void act(() => requestDocumentChanges(id, { actor, note: note || undefined }))
  }

  async function savePath() {
    if (editPath === null) return
    // Normalize: trim, drop leading/trailing slashes, collapse doubles. Empty clears the filing.
    const path = editPath.trim().replace(/^\/+|\/+$/g, '').replace(/\/{2,}/g, '/')
    await act(() => setDocumentPath(id, { path, actor }))
    setEditPath(null)
  }

  const error = actionError ?? docError?.message ?? null
  const tags = Array.isArray(doc?.metadata?.tags) ? (doc!.metadata.tags as unknown[]) : []

  // Available review actions depend on status: a draft (or one with changes requested) can be
  // submitted; a doc in review can be approved or bounced back.
  const status = doc?.status
  const canSubmit = status === 'draft' || status === 'changes_requested'
  const inReview = status === 'in_review'

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
                onClick={() => void act(() => submitDocumentForReview(id, { actor }))}
                className="rounded-md bg-sky-600 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40"
              >
                Submit for review
              </button>
            )}
            {inReview && (
              <>
                <button
                  disabled={busy}
                  onClick={() => void act(() => approveDocument(id, { actor }))}
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
          </div>

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
                  className="rounded-md bg-sky-600 px-2 py-1 font-medium text-white disabled:opacity-40"
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
                      onResolve={(cid) => void act(() => resolveDocumentComment(id, cid, { actor }))}
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
              {showDiff && <DocDiff versions={doc.versions} currentId={doc.current_version_id} />}
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
                      onResolve={() => void act(() => resolveDocumentComment(id, c.id, { actor }))}
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
                              onResolve={() => void act(() => resolveDocumentComment(id, r.id, { actor }))}
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
              className="rounded-md bg-sky-600 px-3 py-2 text-sm font-medium text-white disabled:opacity-40"
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

// A client-defined text-quote region selector (the board stores comment.region as opaque JSON).
export interface RegionQuote {
  type: 'text-quote'
  exact: string
  prefix?: string
  suffix?: string
}

// Narrow an opaque comment.region to a text-quote selector (with a usable `exact`), else null.
function asQuote(region: unknown): RegionQuote | null {
  if (region && typeof region === 'object') {
    const r = region as Record<string, unknown>
    if (r.type === 'text-quote' && typeof r.exact === 'string' && r.exact.length > 0) {
      return {
        type: 'text-quote',
        exact: r.exact,
        prefix: typeof r.prefix === 'string' ? r.prefix : undefined,
        suffix: typeof r.suffix === 'string' ? r.suffix : undefined,
      }
    }
  }
  return null
}

// Locate a quote (optionally disambiguated by its preceding prefix) in a container's rendered
// text and return a DOM Range spanning it — walking text nodes so a match that spans elements
// still resolves. Returns null when the quote isn't present in the shown content.
function findQuoteRange(container: HTMLElement, exact: string, prefix?: string): Range | null {
  const walker = document.createTreeWalker(container, NodeFilter.SHOW_TEXT)
  const nodes: Text[] = []
  const starts: number[] = []
  let full = ''
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    starts.push(full.length)
    nodes.push(n as Text)
    full += (n as Text).data
  }
  if (nodes.length === 0) return null
  let exactAt = -1
  if (prefix) {
    const withPrefix = full.indexOf(prefix + exact)
    if (withPrefix >= 0) exactAt = withPrefix + prefix.length
  }
  if (exactAt < 0) exactAt = full.indexOf(exact)
  if (exactAt < 0) return null
  const locate = (pos: number) => {
    for (let i = nodes.length - 1; i >= 0; i--) {
      if (starts[i] <= pos) return { node: nodes[i], offset: Math.min(pos - starts[i], nodes[i].length) }
    }
    return { node: nodes[0], offset: 0 }
  }
  const s = locate(exactAt)
  const e = locate(exactAt + exact.length)
  const range = document.createRange()
  try {
    range.setStart(s.node, s.offset)
    range.setEnd(e.node, e.offset)
  } catch {
    return null
  }
  return range
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
          className="rounded-md bg-sky-600 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40"
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
          className="rounded-md bg-sky-600 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40"
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

// Compare two document versions: pick a base + a compare version (default previous → current),
// fetch both through the IPFS gateway, and render an added/removed/context line diff client-side.
function DocDiff({
  versions,
  currentId,
}: {
  versions: DocumentVersion[]
  currentId: number | null
}) {
  const sorted = [...versions].sort((a, b) => a.version_no - b.version_no)
  const current = sorted.find((v) => v.id === currentId) ?? sorted[sorted.length - 1]
  const curIdx = sorted.indexOf(current)
  const prev = sorted[curIdx - 1] ?? sorted[0]
  const [baseId, setBaseId] = useState<number>(prev.id)
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
              v{v.version_no}
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
              v{v.version_no}
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
      {loading ? (
        <p className="text-xs text-[var(--color-muted)]">Computing diff…</p>
      ) : result.error ? (
        <p className="text-xs text-[var(--color-muted)]">Couldn't load versions: {result.error}</p>
      ) : baseId === cmpId ? (
        <p className="text-xs text-[var(--color-muted)]">Pick two different versions to compare.</p>
      ) : (
        <pre className="max-h-[60vh] overflow-auto rounded bg-[var(--color-panel-2)] p-2 text-xs leading-relaxed">
          {result.lines!.map((l, idx) => (
            <div
              key={idx}
              className={
                l.type === 'add'
                  ? 'bg-emerald-500/10 text-emerald-300'
                  : l.type === 'del'
                    ? 'bg-red-500/10 text-red-300'
                    : 'text-[var(--color-muted)]'
              }
            >
              <span className="select-none opacity-60">
                {l.type === 'add' ? '+ ' : l.type === 'del' ? '− ' : '  '}
              </span>
              {l.text || ' '}
            </div>
          ))}
        </pre>
      )}
    </div>
  )
}
