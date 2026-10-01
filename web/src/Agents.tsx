import { Link } from 'react-router-dom'
import { useScrollRestoration } from './scrollRestore'
import { type Agent, type AgentStatus } from './api'
import { useAgents } from './resources'
import { AGENT_DOT, relTime } from './ui'

// Order agents by liveness first (online > busy > away > offline), then most-recently-seen.
const STATUS_RANK: Record<AgentStatus, number> = { online: 0, busy: 1, away: 2, offline: 3 }

function seenMs(a: Agent): number {
  const t = a.last_seen ? new Date(a.last_seen).getTime() : 0
  return Number.isNaN(t) ? 0 : t
}

// The agents roster (/agents): a full-width grid of every registered agent — presence, kind,
// last-seen, charter snippet, and repos — so agents are a first-class view rather than the
// cramped sidebar panel. Each card drills into the per-agent page. Backed by the resource
// store, so presence live-updates.
export default function Agents() {
  const scrollRef = useScrollRestoration()
  const { data: agents = [], loading } = useAgents()
  const sorted = [...agents].sort(
    (a, b) => STATUS_RANK[a.status] - STATUS_RANK[b.status] || seenMs(b) - seenMs(a),
  )
  const liveCount = agents.filter((a) => a.status === 'online' || a.status === 'busy').length

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="flex items-center gap-3 border-b border-[var(--color-border)] px-5 py-3">
        <h1 className="text-sm font-semibold">Agents</h1>
        <span className="text-xs text-[var(--color-muted)]">
          {agents.length} registered · {liveCount} active
        </span>
      </div>
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
        {loading && agents.length === 0 && (
          <p className="text-sm text-[var(--color-muted)]">Loading…</p>
        )}
        {!loading && agents.length === 0 && (
          <p className="text-sm text-[var(--color-muted)]">No agents registered yet.</p>
        )}
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-3">
          {sorted.map((a) => {
            const repos = Array.isArray(a.metadata?.repos)
              ? (a.metadata.repos as { repo?: string }[])
              : []
            return (
              <Link
                key={a.id}
                to={`/agents/${encodeURIComponent(a.id)}`}
                className="flex flex-col rounded-lg border border-[var(--color-border)] bg-[var(--color-panel)] p-3 hover:border-sky-500/40"
              >
                <div className="flex items-center gap-2">
                  <span
                    className={`size-2.5 shrink-0 rounded-full ${AGENT_DOT[a.status] ?? 'bg-zinc-600'}`}
                    title={a.status}
                  />
                  <span className="min-w-0 flex-1 truncate font-mono text-sm">
                    {a.display_name || a.id}
                  </span>
                  <span className="shrink-0 text-[11px] text-[var(--color-muted)]">
                    {relTime(a.last_seen)}
                  </span>
                </div>
                <div className="mt-0.5 flex items-center gap-2 pl-4 text-[11px] text-[var(--color-muted)]">
                  <span>{a.status}</span>
                  {a.kind && <span>· {a.kind}</span>}
                  {repos.length > 0 && (
                    <span>· {repos.length} repo{repos.length === 1 ? '' : 's'}</span>
                  )}
                </div>
                {a.status_message && (
                  <p className="mt-1 pl-4 text-xs italic text-[var(--color-muted)] line-clamp-1">
                    {a.status_message}
                  </p>
                )}
                {a.charter && (
                  <p className="mt-1 pl-4 text-xs leading-snug text-[var(--color-muted)] line-clamp-3">
                    {a.charter}
                  </p>
                )}
              </Link>
            )
          })}
        </div>
      </div>
    </main>
  )
}
