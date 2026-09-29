import { useEffect, useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { type DocumentComment, type DocumentVersion, ipfsUrl } from './api'
import { DocStatusChip } from './Documents'
import { useBoardContext } from './Layout'
import { Markdown } from './markdown'
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
import { AuthorLabel, relTime } from './ui'

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

  async function addComment() {
    const body = draft.trim()
    if (!body) return
    await act(() =>
      commentDocument(id, { body, author: actor, reply_to: replyTo ?? undefined }),
    )
    setDraft('')
    setReplyTo(null)
  }

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
            <div className="mb-6">
              <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
                Current version
                <span className="ml-2 font-mono text-[10px] normal-case tracking-normal">
                  {doc.current_version.content_type ?? 'text/markdown'}
                </span>
              </h2>
              <DocContent key={doc.current_version.id} version={doc.current_version} />
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
                    href={ipfsUrl(v.cid)}
                    target="_blank"
                    rel="noreferrer"
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

          {/* Review comments: threaded (top-level + one-level replies), each open comment
              resolvable. Region-anchored comments show the version they target. */}
          <h2 className="mb-2 mt-6 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            Comments ({comments.length})
          </h2>
          <ul className="space-y-2">
            {topLevel.map((c) => (
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

          {/* Composer. Replies target the selected comment; otherwise a doc-level comment. */}
          <div className="mt-3 flex gap-2">
            <input
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => e.key === 'Enter' && addComment()}
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
type DocKind = 'markdown' | 'image' | 'pdf' | 'json' | 'text' | 'other'
function kindOf(contentType: string | null): DocKind {
  const t = (contentType ?? 'text/markdown').toLowerCase().split(';')[0].trim()
  if (t.startsWith('image/')) return 'image'
  if (t === 'application/pdf') return 'pdf'
  if (t === 'text/markdown' || t === 'text/x-markdown' || t === '') return 'markdown'
  if (t === 'application/json') return 'json'
  if (t.startsWith('text/')) return 'text'
  return 'other'
}

// Render one document version's content inline, dispatched by its content_type. Text-shaped
// kinds (markdown/json/text) are fetched from the IPFS gateway as text; binary kinds (image/pdf)
// are pointed at the gateway URL directly. Every path degrades gracefully to a raw link if the
// gateway is unreachable or the type is unknown — a missing gateway never breaks the view.
function DocContent({ version }: { version: DocumentVersion }) {
  const kind = kindOf(version.content_type)
  const needsText = kind === 'markdown' || kind === 'json' || kind === 'text'
  const url = ipfsUrl(version.cid)
  const [text, setText] = useState<string | null>(null)
  const [status, setStatus] = useState<'idle' | 'loading' | 'error'>(needsText ? 'loading' : 'idle')

  // Keyed on version id by the parent, so a version change remounts with fresh initial state —
  // no synchronous reset here; the effect just fetches (a real external-system sync).
  useEffect(() => {
    if (!needsText) return
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
        }
      })
      .catch(() => {
        if (!cancelled) setStatus('error')
      })
    return () => {
      cancelled = true
    }
  }, [url, needsText])

  const raw = (
    <a
      href={url}
      target="_blank"
      rel="noreferrer"
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
  if (kind === 'markdown') {
    return (
      <div className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] p-4">
        <Markdown source={text} className="text-sm" />
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
}: {
  c: DocumentComment
  versionNo: number | null
  busy: boolean
  resolveExternal: (id: string) => string
  onResolve: () => void
  onReply?: () => void
  replying?: boolean
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
      <Markdown source={c.body} className="text-sm" />
    </div>
  )
}
