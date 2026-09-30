import { useCallback, useMemo, useState } from 'react'
import { Link, Outlet, useOutletContext, useParams } from 'react-router-dom'
import { useLiveUpdates } from './live'
import {
  AgentMentionContext,
  type AgentResolver,
  WikiLinkContext,
  type WikiResolver,
} from './markdown'
import {
  createProject,
  eventHref,
  useAgents,
  useEvents,
  useIdentityAliases,
  useProjects,
  useWiki,
} from './resources'
import { useConnectionHealth } from './store'
import { type ThemePref, useTheme } from './theme'
import { relTime } from './ui'

// A thin top banner shown while the backend is unreachable (e.g. the 502 window during a backend
// deploy) or the live stream is down. The board keeps its last-good content and auto-retries in
// the background (see store.ts); this just tells the user what's happening instead of leaving a
// silently stale/blank screen. (task 549)
function ConnectionBanner() {
  const { retrying, streamDown } = useConnectionHealth()
  if (!retrying && !streamDown) return null
  const msg = retrying
    ? 'Reconnecting to the server...'
    : 'Live updates paused, reconnecting...'
  return (
    <div
      role="status"
      aria-live="polite"
      className="flex items-center justify-center gap-2 bg-amber-500/15 px-4 py-1 text-center text-xs text-amber-300"
    >
      <span
        className="inline-block size-1.5 animate-pulse rounded-full bg-amber-400"
        aria-hidden
      />
      {msg}
    </div>
  )
}

// Handed down to nested routes: the current actor (per-user localStorage identity, not a server
// resource) + its setter, and the theme preference + setter (the settings page drives both). All
// server data comes from the store hooks.
export interface BoardContext {
  actor: string
  setActor: (id: string) => void
  theme: { pref: ThemePref; setPref: (p: ThemePref) => void }
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
  const theme = useTheme() // single theme source of truth; applied app-wide, shared via context
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
  // Known agent ids, so @mentions in any rendered markdown link only to real agents (an unknown
  // @word stays plain text). Live-updates as agents register.
  const { data: agents = [] } = useAgents()
  const agentIds = useMemo(() => new Set(agents.map((a) => a.id)), [agents])
  // Identity aliases (task 532): a mention of an alias (@operator) links to its canonical identity
  // (/agents/cameron). Curated data, so we link even if the canonical has no agent row yet.
  const { data: aliases = [] } = useIdentityAliases()
  const aliasMap = useMemo(() => new Map(aliases.map((a) => [a.alias, a.canonical])), [aliases])
  const resolveMention = useCallback<AgentResolver>(
    (id) => (agentIds.has(id) ? id : (aliasMap.get(id) ?? null)),
    [agentIds, aliasMap],
  )
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
      <ConnectionBanner />
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
          to="/reviews"
          className="text-xs text-[var(--color-muted)] underline decoration-dotted underline-offset-2 hover:text-sky-300"
        >
          Reviews
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
          className="text-xs text-[var(--color-muted)] underline decoration-dotted underline-offset-2 hover:text-sky-300"
        >
          API docs
        </a>
        {/* Identity + settings entry point: a link to the settings page (home for the actor
            identity, theme, and future per-user prefs) replacing the old inline "you are" input. */}
        <Link
          to="/settings"
          title="Settings"
          className="ml-auto flex items-center gap-1.5 rounded-md px-2 py-1 text-sm text-[var(--color-muted)] hover:bg-[var(--color-panel-2)] hover:text-sky-300"
        >
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden>
            <circle cx="12" cy="12" r="3" />
            <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
          </svg>
          <span className="hidden font-mono text-xs sm:inline">{actor}</span>
        </Link>
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
          <AgentMentionContext.Provider value={resolveMention}>
            <Outlet context={{ actor, setActor, theme } satisfies BoardContext} />
          </AgentMentionContext.Provider>
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
