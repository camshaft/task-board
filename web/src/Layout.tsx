import { useCallback, useMemo, useState } from 'react'
import { Link, Outlet, useOutletContext, useParams } from 'react-router-dom'
import { useLiveUpdates } from './live'
import { WikiLinkContext, type WikiResolver } from './markdown'
import { createProject, eventHref, useEvents, useProjects, useWiki } from './resources'
import { relTime } from './ui'

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
  const { data: events = [] } = useEvents()
  // Wiki path -> doc index, so [[wiki-links]] in any rendered markdown resolve app-wide (and
  // live-update as pages are filed). A miss renders as a dangling red-link.
  const { data: wikiDocs = [] } = useWiki()
  const wikiByPath = useMemo(() => {
    const m = new Map<string, { id: number; title: string }>()
    for (const d of wikiDocs) if (d.path) m.set(d.path, { id: d.id, title: d.title })
    return m
  }, [wikiDocs])
  const resolveWikiLink = useCallback<WikiResolver>((path) => wikiByPath.get(path) ?? null, [wikiByPath])
  const [showArchived, setShowArchived] = useState(false)
  // The left sidebar is an off-canvas drawer on small screens (toggled from the header) and a
  // static column on lg+. Navigating from a drawer link closes it so the content is visible.
  const [sidebarOpen, setSidebarOpen] = useState(false)
  const closeSidebar = () => setSidebarOpen(false)
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
      <header className="flex flex-wrap items-center gap-x-4 gap-y-2 border-b border-[var(--color-border)] px-4 py-3 sm:px-5">
        <button
          onClick={() => setSidebarOpen((v) => !v)}
          aria-label="Toggle sidebar"
          aria-expanded={sidebarOpen}
          className="-ml-1 rounded-md p-1.5 text-[var(--color-muted)] hover:bg-[var(--color-panel-2)] lg:hidden"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden>
            <line x1="3" y1="6" x2="21" y2="6" />
            <line x1="3" y1="12" x2="21" y2="12" />
            <line x1="3" y1="18" x2="21" y2="18" />
          </svg>
        </button>
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
        <Link
          to="/documents"
          className="text-xs text-[var(--color-muted)] underline decoration-dotted underline-offset-2 hover:text-sky-300"
        >
          Docs
        </Link>
        <Link
          to="/wiki"
          className="text-xs text-[var(--color-muted)] underline decoration-dotted underline-offset-2 hover:text-sky-300"
        >
          Wiki
        </Link>
        <Link
          to="/channels"
          className="text-xs text-[var(--color-muted)] underline decoration-dotted underline-offset-2 hover:text-sky-300"
        >
          Channels
        </Link>
        <Link
          to="/agents"
          className="text-xs text-[var(--color-muted)] underline decoration-dotted underline-offset-2 hover:text-sky-300"
        >
          Agents
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

      <div className="relative flex min-h-0 flex-1">
        {/* Backdrop behind the off-canvas sidebar on small screens. */}
        {sidebarOpen && (
          <div
            className="absolute inset-0 z-30 bg-black/50 lg:hidden"
            onClick={closeSidebar}
            aria-hidden
          />
        )}
        {/* Sidebar: projects + agents. Off-canvas drawer below lg, static column at lg+. */}
        <aside
          className={`absolute inset-y-0 left-0 z-40 flex w-64 shrink-0 flex-col border-r border-[var(--color-border)] bg-[var(--color-panel)] shadow-xl transition-transform duration-200 lg:static lg:z-auto lg:translate-x-0 lg:shadow-none ${
            sidebarOpen ? 'translate-x-0' : '-translate-x-full'
          }`}
        >
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
                  onClick={closeSidebar}
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
                      onClick={closeSidebar}
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
        </aside>

        {/* Whatever the URL points at: the board for a project, plus the task drawer. Wrapped so
            [[wiki-links]] in any markdown below resolve against the live wiki. */}
        <WikiLinkContext.Provider value={resolveWikiLink}>
          <Outlet context={{ actor } satisfies BoardContext} />
        </WikiLinkContext.Provider>

        {/* Event feed */}
        <aside className="hidden w-72 shrink-0 flex-col border-l border-[var(--color-border)] bg-[var(--color-panel)] xl:flex">
          <div className="px-4 py-3">
            <span className="text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
              Activity
            </span>
          </div>
          <div className="flex-1 overflow-y-auto px-3 pb-3">
            <ul className="space-y-2">
              {events.map((e) => {
                const href = eventHref(e)
                const body = (
                  <>
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
                  </>
                )
                return (
                  <li key={e.seq} className="text-xs">
                    {href ? (
                      <Link
                        to={href}
                        className="-mx-1 block rounded px-1 hover:bg-[var(--color-panel-2)]"
                      >
                        {body}
                      </Link>
                    ) : (
                      body
                    )}
                  </li>
                )
              })}
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
