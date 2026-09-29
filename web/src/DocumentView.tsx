import { useEffect, useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { api, type Document, ipfsUrl } from './api'
import { DocStatusChip } from './Documents'
import { relTime } from './ui'

// Read-only document viewer: metadata, the tasks it backs, and its immutable version history
// with links that resolve each CID through the IPFS gateway (client-side). In-app markdown
// rendering, select-to-comment, and the review actions (approve / request-changes / resolve)
// are follow-up slices of task 101.
export default function DocumentView() {
  const { documentId } = useParams()
  const id = Number(documentId)
  const [doc, setDoc] = useState<Document | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let alive = true
    setLoading(true)
    api
      .getDocument(id)
      .then((d) => alive && setDoc(d))
      .catch((e) => alive && setError((e as Error).message))
      .finally(() => alive && setLoading(false))
    return () => {
      alive = false
    }
  }, [id])

  const tags = Array.isArray(doc?.metadata?.tags) ? (doc!.metadata.tags as unknown[]) : []

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="flex items-center gap-3 border-b border-[var(--color-border)] px-5 py-3">
        <Link to="/documents" className="text-xs text-[var(--color-muted)] hover:text-sky-300">
          ← Documents
        </Link>
        <h1 className="truncate text-sm font-semibold">{doc?.title ?? `Document #${id}`}</h1>
        {doc && <DocStatusChip status={doc.status} />}
      </div>

      {error && (
        <div className="border-b border-rose-500/30 bg-rose-500/10 px-5 py-2 text-sm text-rose-300">
          {error}
        </div>
      )}
      {loading && <p className="px-5 py-3 text-sm text-[var(--color-muted)]">Loading…</p>}

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

          {/* Tasks this document backs. (Deep-linking to the task drawer needs the task's
              project_id, which the attachment summary doesn't carry yet — shown as text for now.) */}
          {doc.attached_tasks.length > 0 && (
            <div className="mb-4 text-xs text-[var(--color-muted)]">
              <span>Backs: </span>
              {doc.attached_tasks.map((t) => `#${t.id} ${t.title}`).join(', ')}
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
        </div>
      )}
    </main>
  )
}
