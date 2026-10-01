import { useState } from 'react'
import { Link } from 'react-router-dom'
import { useScrollRestoration } from './scrollRestore'
import { useDocumentList, useProjects } from './resources'
import { relTime } from './ui'

// Status filter options. The values use the operator vocabulary the backend accepts (pending-review
// / published map server-side); the raw lifecycle statuses are offered too (task 725).
const STATUS_OPTIONS: { label: string; value: string }[] = [
  { label: 'All statuses', value: '' },
  { label: 'Pending review', value: 'pending-review' },
  { label: 'Published', value: 'published' },
  { label: 'Draft', value: 'draft' },
  { label: 'In review', value: 'in_review' },
  { label: 'Changes requested', value: 'changes_requested' },
]

// The documents list: every document with its status + project, linking to the viewer. Backed
// by the reference-counted resource store, so it live-updates as documents are created, get
// new versions, or change review status (the SSE feed funnels document.* through touched()).

const DOC_STATUS_CHIP: Record<string, string> = {
  draft: 'bg-slate-500/15 text-slate-300 ring-slate-500/30',
  in_review: 'bg-sky-500/15 text-sky-300 ring-sky-500/30',
  changes_requested: 'bg-amber-500/15 text-amber-300 ring-amber-500/30',
  approved: 'bg-emerald-500/15 text-emerald-300 ring-emerald-500/30',
}

export function DocStatusChip({ status }: { status: string }) {
  const chip = DOC_STATUS_CHIP[status] ?? 'bg-zinc-500/15 text-zinc-400 ring-zinc-500/30'
  return (
    <span
      className={`inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium ring-1 ring-inset ${chip}`}
    >
      {status.replace(/_/g, ' ')}
    </span>
  )
}

export default function Documents() {
  const scrollRef = useScrollRestoration()
  const { data: projects = [] } = useProjects()
  const projectName = (id: number | null) =>
    id == null ? '' : (projects.find((p) => p.id === id)?.name ?? `#${id}`)

  const [status, setStatus] = useState('')
  const [tag, setTag] = useState('')
  const [showAll, setShowAll] = useState(false)
  const { data: docs, error, loading } = useDocumentList({ status, tag, showAll })
  // The default view hides charters unless pending-review; it only applies with no explicit filter.
  const defaultHideActive = !showAll && !status && !tag

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="border-b border-[var(--color-border)] px-5 py-3">
        <h1 className="text-sm font-semibold">Documents</h1>
        <p className="mt-0.5 text-xs text-[var(--color-muted)]">
          Versioned, content-addressed documents. Content lives on IPFS; the board stores the CID.
        </p>
        <div className="mt-2 flex flex-wrap items-center gap-2 text-xs">
          <select
            value={status}
            onChange={(e) => setStatus(e.target.value)}
            className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1"
          >
            {STATUS_OPTIONS.map((o) => (
              <option key={o.value} value={o.value}>
                {o.label}
              </option>
            ))}
          </select>
          <input
            value={tag}
            onChange={(e) => setTag(e.target.value)}
            placeholder="filter by tag"
            className="w-32 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1"
          />
          <label className="flex items-center gap-1.5 text-[var(--color-muted)]">
            <input type="checkbox" checked={showAll} onChange={(e) => setShowAll(e.target.checked)} />
            Show all (incl. charters)
          </label>
          {defaultHideActive && (
            <span className="text-[var(--color-muted)]">
              · charters hidden unless pending review
            </span>
          )}
          {(status || tag) && (
            <button
              onClick={() => {
                setStatus('')
                setTag('')
              }}
              className="text-sky-400 hover:text-sky-300"
            >
              clear
            </button>
          )}
        </div>
      </div>
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto px-5 py-3">
        {error && <p className="text-sm text-rose-300">{error.message}</p>}
        {loading && !docs && <p className="text-sm text-[var(--color-muted)]">Loading…</p>}
        {docs && docs.length === 0 && (
          <p className="text-sm text-[var(--color-muted)]">No documents yet.</p>
        )}
        <ul className="space-y-1.5">
          {docs?.map((d) => (
            <li
              key={d.id}
              className="flex items-center gap-3 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-3 py-2"
            >
              <DocStatusChip status={d.status} />
              {d.deprecated_at && (
                <span
                  title={d.superseded_by != null ? `Deprecated, superseded by document ${d.superseded_by}` : 'Deprecated'}
                  className="rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide text-amber-300"
                >
                  deprecated
                </span>
              )}
              <Link
                to={`/documents/${d.id}`}
                className="min-w-0 flex-1 truncate text-sm hover:text-sky-300"
              >
                {d.title}
              </Link>
              {d.project_id != null && (
                <span className="hidden text-xs text-[var(--color-muted)] sm:inline">
                  {projectName(d.project_id)}
                </span>
              )}
              {d.created_by && (
                <span className="font-mono text-[11px] text-[var(--color-muted)]">
                  {d.created_by}
                </span>
              )}
              <span className="text-[11px] text-[var(--color-muted)]">{relTime(d.updated_at)}</span>
            </li>
          ))}
        </ul>
      </div>
    </main>
  )
}
