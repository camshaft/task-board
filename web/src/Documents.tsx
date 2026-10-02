import { useState } from 'react'
import { Link } from 'react-router-dom'
import { useScrollRestoration } from './scrollRestore'
import { type DocumentSummary } from './api'
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
  // The gated operator-approval queue state; distinct from the earlier agent in_review stage.
  operator_review: 'bg-violet-500/15 text-violet-300 ring-violet-500/30',
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
  const [query, setQuery] = useState('')
  const [showAll, setShowAll] = useState(false)
  const { data: docs, error, loading } = useDocumentList({ status, tag, showAll })
  // The default view hides charters unless pending-review; it only applies with no explicit filter.
  const defaultHideActive = !showAll && !status && !tag

  // Group the flat list by project so a crowded board reads as per-project sections instead of one
  // undifferentiated run (task_1058: "docs are a huge mess"). A client-side title filter narrows
  // within that. Projects are name-ordered (matching the sidebar/dashboard); unfiled docs sort last
  // under "No project"; docs within a group are most-recently-updated first.
  const q = query.trim().toLowerCase()
  const filtered = (docs ?? []).filter((d) => !q || d.title.toLowerCase().includes(q))
  const groups = new Map<number | null, DocumentSummary[]>()
  for (const d of filtered) {
    const k = d.project_id ?? null
    const arr = groups.get(k)
    if (arr) arr.push(d)
    else groups.set(k, [d])
  }
  for (const arr of groups.values())
    arr.sort((a, b) => (b.updated_at ?? '').localeCompare(a.updated_at ?? ''))
  const orderedKeys = [...groups.keys()].sort((a, b) => {
    if (a === null) return 1
    if (b === null) return -1
    return projectName(a).localeCompare(projectName(b), undefined, { sensitivity: 'base' })
  })

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
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="filter by title"
            className="w-40 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1"
          />
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
          {(status || tag || query) && (
            <button
              onClick={() => {
                setStatus('')
                setTag('')
                setQuery('')
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
        {docs && docs.length > 0 && filtered.length === 0 && (
          <p className="text-sm text-[var(--color-muted)]">No documents match the filter.</p>
        )}
        <div className="space-y-5">
          {orderedKeys.map((key) => {
            const groupDocs = groups.get(key) ?? []
            return (
              <section key={key ?? 'none'}>
                {/* Sticky project header so the section a doc belongs to stays visible while scrolling
                    a long grouped list. */}
                <h2 className="sticky top-0 z-10 -mx-1 mb-1.5 flex items-center gap-2 bg-[var(--color-bg)]/95 px-1 py-1 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)] backdrop-blur">
                  {key == null ? (
                    <span>No project</span>
                  ) : (
                    <Link to={`/projects/${key}`} className="hover:text-sky-300">
                      {projectName(key)}
                    </Link>
                  )}
                  <span className="font-mono text-[10px] normal-case">{groupDocs.length}</span>
                </h2>
                <ul className="space-y-1.5">
                  {groupDocs.map((d) => (
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
                      {d.created_by && (
                        <span className="font-mono text-[11px] text-[var(--color-muted)]">
                          {d.created_by}
                        </span>
                      )}
                      <span className="text-[11px] text-[var(--color-muted)]">{relTime(d.updated_at)}</span>
                    </li>
                  ))}
                </ul>
              </section>
            )
          })}
        </div>
      </div>
    </main>
  )
}
