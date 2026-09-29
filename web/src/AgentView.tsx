import { Link, useParams } from 'react-router-dom'
import { useAgent, useAgentTasks, useProjects } from './resources'
import { AGENT_DOT, relTime, StatusChip } from './ui'

// Per-agent page (/agents/:agentId): identity + presence, charter, registry metadata (repos),
// and the tasks currently assigned to this agent across every project. Read-only; backed by the
// resource store, so the assigned-task list live-updates as tasks change (it's keyed under the
// `tasks:` prefix the mutation choke point already invalidates).
export default function AgentView() {
  const { agentId } = useParams()
  const id = agentId ?? ''
  const { data: agent, error, loading } = useAgent(id)
  const { data: tasks = [] } = useAgentTasks(id)
  const { data: projects = [] } = useProjects()
  const projectName = (pid: number | null | undefined) =>
    pid == null ? '' : (projects.find((p) => p.id === pid)?.name ?? `#${pid}`)

  // Registry repos, if present: metadata.repos = [{ repo, branch }, ...].
  const repos = Array.isArray(agent?.metadata?.repos)
    ? (agent!.metadata.repos as { repo?: string; branch?: string }[])
    : []

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="flex items-center gap-3 border-b border-[var(--color-border)] px-5 py-3">
        <Link to="/" className="text-xs text-[var(--color-muted)] hover:text-sky-300">
          ← Home
        </Link>
        {agent && (
          <span
            className={`size-2 rounded-full ${AGENT_DOT[agent.status] ?? 'bg-zinc-600'}`}
            title={agent.status}
          />
        )}
        <h1 className="truncate text-sm font-semibold">
          {agent?.display_name || id}
        </h1>
        <span className="font-mono text-xs text-[var(--color-muted)]">{id}</span>
      </div>

      {error && (
        <div className="border-b border-rose-500/30 bg-rose-500/10 px-5 py-2 text-sm text-rose-300">
          {error.message}
        </div>
      )}
      {loading && !agent && <p className="px-5 py-3 text-sm text-[var(--color-muted)]">Loading…</p>}

      {agent && (
        <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
          <dl className="mb-5 grid grid-cols-3 gap-y-2 text-sm">
            <dt className="text-[var(--color-muted)]">Status</dt>
            <dd className="col-span-2">
              {agent.status}
              {agent.status_message && (
                <span className="text-[var(--color-muted)]"> · {agent.status_message}</span>
              )}
            </dd>
            <dt className="text-[var(--color-muted)]">Kind</dt>
            <dd className="col-span-2">{agent.kind ?? '—'}</dd>
            <dt className="text-[var(--color-muted)]">Last seen</dt>
            <dd className="col-span-2">{relTime(agent.last_seen)}</dd>
            <dt className="text-[var(--color-muted)]">Registered</dt>
            <dd className="col-span-2">{relTime(agent.created_at)}</dd>
            {agent.webhook_url && (
              <>
                <dt className="text-[var(--color-muted)]">Webhook</dt>
                <dd className="col-span-2 truncate font-mono text-xs">{agent.webhook_url}</dd>
              </>
            )}
          </dl>

          {repos.length > 0 && (
            <div className="mb-5">
              <div className="mb-1 text-xs text-[var(--color-muted)]">Repos</div>
              <ul className="space-y-1">
                {repos.map((r, i) => (
                  <li key={i} className="font-mono text-xs">
                    {r.repo ?? '?'}
                    {r.branch && <span className="text-[var(--color-muted)]"> @ {r.branch}</span>}
                  </li>
                ))}
              </ul>
            </div>
          )}

          {agent.charter && (
            <div className="mb-5">
              <div className="mb-1 text-xs text-[var(--color-muted)]">Charter</div>
              <p className="whitespace-pre-wrap text-sm">{agent.charter}</p>
            </div>
          )}

          <div>
            <div className="mb-2 text-xs text-[var(--color-muted)]">
              Assigned tasks ({tasks.length})
            </div>
            <ul className="space-y-1.5">
              {tasks.map((t) => (
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
              {tasks.length === 0 && (
                <li className="text-sm text-[var(--color-muted)]">No assigned tasks.</li>
              )}
            </ul>
          </div>
        </div>
      )}
    </main>
  )
}
