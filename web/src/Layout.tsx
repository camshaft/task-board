import { useState } from 'react'
import { Link, Outlet, useOutletContext, useParams } from 'react-router-dom'
import { useLiveUpdates } from './live'
import { createProject, useAgents, useEvents, useProjects } from './resources'
import { AGENT_DOT, relTime } from './ui'

// The only thing nested routes still need handed down is the current actor (per-user
// localStorage identity, not a server resource). All server data comes from the store hooks.
export interface BoardContext {
  actor: string
}

export function useBoardContext() {
  return useOutletContext<BoardContext>()
}

// Your identity on the board. Persisted so actions (comments, status changes) are
// attributed and you aren't notified of your own changes. Trust-on-first-use, no auth.
function useActor(): [string, (v: string) => void] {
  const [actor, setActor] = useState(() => localStorage.getItem('tb-actor') || 'human')
  const set = (v: string) => {
    const id = v.trim() || 'human'
    localStorage.setItem('tb-actor', id)
    setActor(id)
  }
  return [actor, set]
}

// The persistent chrome — header, project/agent sidebar, activity feed — around an
// <Outlet/> that renders whichever project/task the URL points at. Every data panel here
// subscribes to a store resource, so it re-renders on its own when that data changes.
export default function Layout() {
  const { projectId } = useParams()
  const selectedProject = projectId != null ? Number(projectId) : null
  const [actor, setActor] = useActor()
  useLiveUpdates() // one SSE connection makes every subscribed panel live
  const { data: projects = [], error: projectsError } = useProjects()
  const { data: agents = [] } = useAgents()
  const { data: events = [] } = useEvents()
  const [showArchived, setShowArchived] = useState(false)
  const activeProjects = projects.filter((p) => p.status !== 'archived')
  const archivedProjects = projects.filter((p) => p.status === 'archived')

  async function newProject() {
    const name = window.prompt('Project name:')
    if (!name?.trim()) return
    try {
      await createProject({ name: name.trim(), created_by: actor })
    } catch (e) {
      window.alert((e as Error).message)
    }
  }

  return (
    <div className="flex h-full flex-col">
      <header className="flex items-center gap-4 border-b border-[var(--color-border)] px-5 py-3">
        <Link to="/" className="text-base font-semibold tracking-tight">
          <span className="text-sky-400">task</span>-board
        </Link>
        <span className="hidden text-xs text-[var(--color-muted)] sm:inline">
          agent coordination · MCP + REST
        </span>
        <Link
          to="/search"
          className="text-xs text-[var(--color-muted)] underline decoration-dotted underline-offset-2 hover:text-sky-300"
        >
          Search
        </Link>
        <a
          href={new URL('api', document.baseURI).href}
          target="_blank"
          rel="noreferrer"
          className="text-xs text-[var(--color-muted)] underline decoration-dotted underline-offset-2 hover:text-sky-300"
        >
          API docs
        </a>
        <div className="ml-auto flex items-center gap-2 text-sm">
          <label className="text-[var(--color-muted)]">you are</label>
          <input
            defaultValue={actor}
            onBlur={(e) => setActor(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
            className="w-32 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 font-mono text-xs outline-none focus:border-sky-500/50"
          />
        </div>
      </header>

      {projectsError && (
        <div className="border-b border-rose-500/30 bg-rose-500/10 px-5 py-2 text-sm text-rose-300">
          {projectsError.message}
        </div>
      )}

      <div className="flex min-h-0 flex-1">
        {/* Sidebar: projects + agents */}
        <aside className="flex w-64 shrink-0 flex-col border-r border-[var(--color-border)] bg-[var(--color-panel)]">
          <div className="flex items-center justify-between px-4 py-3">
            <span className="text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
              Projects
            </span>
            <button
              onClick={newProject}
              className="rounded px-1.5 text-sm text-sky-400 hover:bg-[var(--color-panel-2)]"
            >
              + new
            </button>
          </div>
          <nav className="flex-1 overflow-y-auto px-2">
            {activeProjects.map((p) => {
              const total = Object.values(p.task_counts ?? {}).reduce((a, b) => a + b, 0)
              return (
                <Link
                  key={p.id}
                  to={`projects/${p.id}`}
                  className={`mb-1 flex w-full items-center justify-between rounded-md px-3 py-2 text-left text-sm ${
                    p.id === selectedProject
                      ? 'bg-sky-500/15 text-sky-100'
                      : 'hover:bg-[var(--color-panel-2)]'
                  }`}
                >
                  <span className="truncate">{p.name}</span>
                  <span className="ml-2 text-xs text-[var(--color-muted)]">{total}</span>
                </Link>
              )
            })}
            {activeProjects.length === 0 && (
              <p className="px-3 py-2 text-sm text-[var(--color-muted)]">No projects yet.</p>
            )}

            {/* Archived projects: collapsed by default, but reachable so they can be
                restored (or their tasks moved out). */}
            {archivedProjects.length > 0 && (
              <div className="mt-2">
                <button
                  onClick={() => setShowArchived((v) => !v)}
                  className="flex w-full items-center gap-1 rounded px-3 py-1.5 text-left text-[11px] uppercase tracking-wide text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
                >
                  <span>{showArchived ? '▾' : '▸'}</span>
                  <span>Archived</span>
                  <span className="ml-auto">{archivedProjects.length}</span>
                </button>
                {showArchived &&
                  archivedProjects.map((p) => (
                    <Link
                      key={p.id}
                      to={`projects/${p.id}`}
                      className={`mb-1 flex w-full items-center justify-between rounded-md px-3 py-2 text-left text-sm ${
                        p.id === selectedProject
                          ? 'bg-sky-500/15 text-sky-100'
                          : 'text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]'
                      }`}
                    >
                      <span className="truncate italic">{p.name}</span>
                    </Link>
                  ))}
              </div>
            )}
          </nav>

          <div className="border-t border-[var(--color-border)] px-4 py-3">
            <span className="text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
              Agents
            </span>
          </div>
          <div className="max-h-64 overflow-y-auto px-2 pb-3">
            {agents.map((a) => (
              <div
                key={a.id}
                className="rounded-md px-3 py-1.5 text-sm"
                title={a.charter ?? a.status_message ?? a.status}
              >
                <div className="flex items-center gap-2">
                  <span className={`size-2 rounded-full ${AGENT_DOT[a.status] ?? 'bg-zinc-600'}`} />
                  <span className="truncate font-mono text-xs">{a.id}</span>
                  <span className="ml-auto text-[10px] text-[var(--color-muted)]">
                    {relTime(a.last_seen)}
                  </span>
                </div>
                {a.charter && (
                  <p className="mt-0.5 pl-4 text-[11px] leading-snug text-[var(--color-muted)] line-clamp-2">
                    {a.charter}
                  </p>
                )}
              </div>
            ))}
            {agents.length === 0 && (
              <p className="px-3 py-1.5 text-sm text-[var(--color-muted)]">None online.</p>
            )}
          </div>
        </aside>

        {/* Whatever the URL points at: the board for a project, plus the task drawer. */}
        <Outlet context={{ actor } satisfies BoardContext} />

        {/* Event feed */}
        <aside className="hidden w-72 shrink-0 flex-col border-l border-[var(--color-border)] bg-[var(--color-panel)] xl:flex">
          <div className="px-4 py-3">
            <span className="text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
              Activity
            </span>
          </div>
          <div className="flex-1 overflow-y-auto px-3 pb-3">
            <ul className="space-y-2">
              {events.map((e) => (
                <li key={e.seq} className="text-xs">
                  <div className="flex items-center gap-1.5">
                    <span className="rounded bg-[var(--color-panel-2)] px-1.5 py-0.5 font-mono text-[10px] text-sky-300">
                      {e.type}
                    </span>
                    <span className="text-[var(--color-muted)]">{relTime(e.created_at)}</span>
                  </div>
                  <div className="mt-0.5 text-[var(--color-muted)]">
                    {e.actor && <span className="font-mono">{e.actor}</span>}
                    {typeof e.data?.title === 'string' && <> · {e.data.title as string}</>}
                  </div>
                </li>
              ))}
              {events.length === 0 && (
                <li className="text-xs text-[var(--color-muted)]">No activity yet.</li>
              )}
            </ul>
          </div>
        </aside>
      </div>
    </div>
  )
}
