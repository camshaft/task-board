import { Link } from 'react-router-dom'
import { useBoardContext } from './Layout'
import { useAgents, useAgentTasks, useEvents, useProjects } from './resources'
import { AGENT_DOT, relTime, StatusChip, STATUS_CHIP, STATUS_LABEL, TASK_COLUMNS } from './ui'

// The index route (`/`): an at-a-glance fleet dashboard — cross-project task totals, the
// current actor's open work, a per-project rollup, and agent presence. Built entirely from the
// existing store hooks (projects carry task_counts in one query; agents/events already live via
// SSE), so the whole page live-updates with no extra backend. Recent activity is in the Layout
// rail, so it isn't duplicated here.
export default function Home() {
  const { actor } = useBoardContext()
  const { data: projects = [], loading } = useProjects()
  const { data: agents = [] } = useAgents()
  const { data: events = [] } = useEvents()
  const { data: myTasks = [] } = useAgentTasks(actor)

  if (!loading && projects.length === 0) {
    return (
      <main className="flex flex-1 items-center justify-center text-sm text-[var(--color-muted)]">
        No projects yet — create one from the sidebar.
      </main>
    )
  }

  const active = projects.filter((p) => p.status !== 'archived')

  // Fleet-wide task totals per status, summed from each project's counts.
  const totals: Record<string, number> = {}
  let allTasks = 0
  for (const p of projects) {
    for (const [status, n] of Object.entries(p.task_counts ?? {})) {
      totals[status] = (totals[status] ?? 0) + n
      allTasks += n
    }
  }
  const openTasks = allTasks - (totals.done ?? 0) - (totals.cancelled ?? 0)
  const onlineAgents = agents.filter((a) => a.status !== 'offline').length
  const myOpen = myTasks.filter((t) => t.status !== 'done' && t.status !== 'cancelled')

  const projectName = (id: number) => projects.find((p) => p.id === id)?.name ?? `#${id}`

  return (
    <main className="min-h-0 flex-1 overflow-y-auto p-5">
      <h1 className="mb-4 text-sm font-semibold">Fleet dashboard</h1>

      {/* Headline stats. */}
      <div className="mb-6 grid grid-cols-2 gap-3 sm:grid-cols-4">
        <StatCard label="Open tasks" value={openTasks} sub={`${allTasks} total`} />
        <StatCard label="In progress" value={totals.in_progress ?? 0} sub={`${totals.blocked ?? 0} blocked`} />
        <StatCard label="Projects" value={active.length} sub={`${projects.length - active.length} archived`} />
        <StatCard label="Agents online" value={onlineAgents} sub={`${agents.length} total`} />
      </div>

      {/* Cross-project status breakdown. */}
      <div className="mb-6 flex flex-wrap gap-2">
        {TASK_COLUMNS.map((s) => (
          <span
            key={s}
            className={`inline-flex items-center gap-1.5 rounded-full px-2.5 py-1 text-xs font-medium ring-1 ring-inset ${STATUS_CHIP[s]}`}
          >
            {STATUS_LABEL[s]}
            <span className="font-mono">{totals[s] ?? 0}</span>
          </span>
        ))}
      </div>

      <div className="grid gap-6 lg:grid-cols-2">
        {/* My open work. */}
        <section>
          <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            My open tasks — <span className="font-mono normal-case">{actor}</span> ({myOpen.length})
          </h2>
          <ul className="space-y-1.5">
            {myOpen.map((t) => (
              <li key={t.id}>
                <Link
                  to={t.project_id != null ? `/projects/${t.project_id}/tasks/${t.id}` : '#'}
                  className="flex items-center gap-2 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-3 py-2 hover:border-sky-500/40"
                >
                  <StatusChip status={t.status} />
                  <span className="min-w-0 flex-1 truncate text-sm">{t.title}</span>
                  {t.project_id != null && (
                    <span className="hidden text-xs text-[var(--color-muted)] sm:inline">
                      {projectName(t.project_id)}
                    </span>
                  )}
                  <span className="font-mono text-[11px] text-[var(--color-muted)]">#{t.id}</span>
                </Link>
              </li>
            ))}
            {myOpen.length === 0 && (
              <li className="text-sm text-[var(--color-muted)]">Nothing assigned to you.</li>
            )}
          </ul>
        </section>

        {/* Per-project rollup. */}
        <section>
          <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            Projects ({active.length})
          </h2>
          <ul className="space-y-1.5">
            {active.map((p) => {
              const counts = p.task_counts ?? {}
              const open = TASK_COLUMNS.filter((s) => s !== 'done' && s !== 'cancelled').reduce(
                (a, s) => a + (counts[s] ?? 0),
                0,
              )
              return (
                <li key={p.id}>
                  <Link
                    to={`/projects/${p.id}`}
                    className="flex items-center gap-2 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-3 py-2 hover:border-sky-500/40"
                  >
                    <span className="min-w-0 flex-1 truncate text-sm">{p.name}</span>
                    <span className="text-[11px] text-[var(--color-muted)]" title="open / in-progress / blocked">
                      {open} open
                      {(counts.in_progress ?? 0) > 0 && ` · ${counts.in_progress} wip`}
                      {(counts.blocked ?? 0) > 0 && (
                        <span className="text-rose-300"> · {counts.blocked} blocked</span>
                      )}
                    </span>
                    <span className="text-[11px] text-[var(--color-muted)]">{relTime(p.updated_at)}</span>
                  </Link>
                </li>
              )
            })}
          </ul>
        </section>
      </div>

      {/* Agent presence. */}
      <section className="mt-6">
        <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
          Agents ({onlineAgents}/{agents.length} online)
        </h2>
        <div className="flex flex-wrap gap-2">
          {agents.map((a) => (
            <Link
              key={a.id}
              to={`/agents/${encodeURIComponent(a.id)}`}
              className="flex items-center gap-1.5 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-2.5 py-1 text-xs hover:border-sky-500/40"
              title={a.status_message ?? a.status}
            >
              <span className={`size-2 rounded-full ${AGENT_DOT[a.status] ?? 'bg-zinc-600'}`} />
              <span className="font-mono">{a.id}</span>
            </Link>
          ))}
          {agents.length === 0 && <span className="text-sm text-[var(--color-muted)]">None.</span>}
        </div>
      </section>

      {/* A short recent-activity strip for narrow screens (the Layout rail is xl-only). */}
      <section className="mt-6 xl:hidden">
        <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
          Recent activity
        </h2>
        <ul className="space-y-1">
          {events.slice(0, 10).map((e) => (
            <li key={e.seq} className="flex items-center gap-2 text-xs">
              <span className="rounded bg-[var(--color-panel-2)] px-1.5 py-0.5 font-mono text-[10px] text-sky-300">
                {e.type}
              </span>
              {e.actor && <span className="font-mono text-[var(--color-muted)]">{e.actor}</span>}
              <span className="ml-auto text-[var(--color-muted)]">{relTime(e.created_at)}</span>
            </li>
          ))}
          {events.length === 0 && <li className="text-sm text-[var(--color-muted)]">No activity yet.</li>}
        </ul>
      </section>
    </main>
  )
}

function StatCard({ label, value, sub }: { label: string; value: number; sub?: string }) {
  return (
    <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-panel)] px-4 py-3">
      <div className="text-2xl font-semibold tabular-nums">{value}</div>
      <div className="text-xs text-[var(--color-muted)]">{label}</div>
      {sub && <div className="mt-0.5 text-[11px] text-[var(--color-muted)]/70">{sub}</div>}
    </div>
  )
}
