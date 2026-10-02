import { useEffect, useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { useScrollRestoration } from './scrollRestore'
import { api, type TaskStatus, type TaskSummary } from './api'
import { useBoardContext } from './Layout'
import { commentTask, updateTask, useProjects } from './resources'
import { Identity, STATUS_LABEL, StatusChip, TASK_COLUMNS, relTime } from './ui'

// Cross-project search + a personal "my tasks" view. Unlike the per-project Board, this queries
// the WHOLE board (no project_id) so you can find or triage tasks anywhere — filtered by free
// text, assignee (defaults to you), and status (e.g. blocked) — with inline status + comment
// actions. It fetches on demand (search is transient) rather than living in the resource store,
// and re-runs after each mutation so the list stays current.
export default function Search() {
  const scrollRef = useScrollRestoration()
  const { actor } = useBoardContext()
  const { data: projects = [] } = useProjects()
  const projectName = (id?: number) =>
    projects.find((p) => p.id === id)?.name ?? (id != null ? `#${id}` : '')

  // Filters are hydrated from the URL query so a filtered view is bookmarkable + shareable
  // (task 524). A bare /search (no params) defaults to your own tasks (the my-tasks view); once
  // ANY filter param is present the URL is authoritative — so `?status=blocked` with no assignee
  // means "anyone blocked", and `?assignee=<you>&status=blocked` is the bookmarkable "blocked on me".
  const [searchParams, setSearchParams] = useSearchParams()
  const hadParams = ['q', 'assignee', 'status'].some((k) => searchParams.has(k))
  const [q, setQ] = useState(() => searchParams.get('q') ?? '')
  const [assignee, setAssignee] = useState(() =>
    searchParams.has('assignee') ? (searchParams.get('assignee') ?? '') : hadParams ? '' : actor,
  )
  const [status, setStatus] = useState<'' | TaskStatus>(() => {
    const s = searchParams.get('status')
    return s && (TASK_COLUMNS as string[]).includes(s) ? (s as TaskStatus) : ''
  })
  const [results, setResults] = useState<TaskSummary[] | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // Reflect the active filters into the URL query (omitting empties) so the view is a stable,
  // bookmarkable link. Called when the user runs a search (submit / status change / a shortcut),
  // not on every keystroke or on mount, so a bare /search stays bare until you act.
  function syncUrl(vals: { q: string; assignee: string; status: '' | TaskStatus }) {
    const next: Record<string, string> = {}
    if (vals.q.trim()) next.q = vals.q.trim()
    if (vals.assignee.trim()) next.assignee = vals.assignee.trim()
    if (vals.status) next.status = vals.status
    setSearchParams(next, { replace: true })
  }

  async function run(override?: { q?: string; assignee?: string; status?: '' | TaskStatus }) {
    const qq = override?.q ?? q
    const aa = override?.assignee ?? assignee
    const ss = override?.status ?? status
    setLoading(true)
    setError(null)
    try {
      setResults(
        await api.listTasks({
          q: qq.trim() || undefined,
          assignee: aa.trim() || undefined,
          status: ss || undefined,
        }),
      )
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setLoading(false)
    }
  }

  // Load "my tasks" on first mount (assignee defaults to you).
  useEffect(() => {
    void run()
    // Intentionally run once on mount; subsequent runs are user- or mutation-driven.
  }, [])

  async function setTaskStatus(id: number, s: TaskStatus) {
    try {
      await updateTask(id, { status: s, principal: actor })
      await run()
    } catch (e) {
      window.alert((e as Error).message)
    }
  }

  async function addComment(id: number) {
    const body = window.prompt('Comment:')
    if (!body?.trim()) return
    try {
      await commentTask(id, { body: body.trim(), principal: actor })
      await run()
    } catch (e) {
      window.alert((e as Error).message)
    }
  }

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="border-b border-[var(--color-border)] px-5 py-3">
        <h1 className="text-sm font-semibold">Search &amp; my tasks</h1>
        <form
          className="mt-2 flex flex-wrap items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault()
            syncUrl({ q, assignee, status })
            void run()
          }}
        >
          <input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="search title or description…"
            className="w-64 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-sm outline-none focus:border-sky-500/50"
          />
          <input
            value={assignee}
            onChange={(e) => setAssignee(e.target.value)}
            placeholder="assignee (blank = anyone)"
            className="w-44 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 font-mono text-xs outline-none focus:border-sky-500/50"
          />
          <select
            value={status}
            onChange={(e) => {
              const s = e.target.value as '' | TaskStatus
              setStatus(s)
              syncUrl({ q, assignee, status: s })
              void run({ status: s })
            }}
            className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-sm outline-none focus:border-sky-500/50"
          >
            <option value="">any status</option>
            {TASK_COLUMNS.map((s) => (
              <option key={s} value={s}>
                {STATUS_LABEL[s]}
              </option>
            ))}
          </select>
          <button
            type="submit"
            className="rounded-md bg-sky-500/20 px-3 py-1 text-sm text-sky-200 hover:bg-sky-500/30"
          >
            Search
          </button>
          <Link
            to="/awaiting"
            title="Everything awaiting your decision: blocked-on-you tasks + questions routed to you (team-expanded, not just tasks assigned to you)"
            className="rounded-md px-2 py-1 text-xs text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
          >
            awaiting you
          </Link>
        </form>
      </div>

      <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto px-5 py-3">
        {error && <p className="text-sm text-rose-700 dark:text-rose-300">{error}</p>}
        {loading && <p className="text-sm text-[var(--color-muted)]">Searching…</p>}
        {!loading && results && results.length === 0 && (
          <p className="text-sm text-[var(--color-muted)]">No matching tasks.</p>
        )}
        <ul className="space-y-1.5">
          {results?.map((t) => (
            <li
              key={t.id}
              className="flex items-center gap-3 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-3 py-2"
            >
              <StatusChip status={t.status} />
              <Link
                to={`/projects/${t.project_id}/tasks/${t.id}`}
                className="min-w-0 flex-1 truncate text-sm hover:text-sky-300"
              >
                {t.title}
              </Link>
              <span className="hidden text-xs text-[var(--color-muted)] sm:inline">
                {projectName(t.project_id)}
              </span>
              {t.assignee && (
                <Identity
                  id={t.assignee}
                  className="font-mono text-[11px] text-[var(--color-muted)]"
                />
              )}
              <span className="text-[11px] text-[var(--color-muted)]">{relTime(t.updated_at)}</span>
              <select
                value={t.status}
                onChange={(e) => void setTaskStatus(t.id, e.target.value as TaskStatus)}
                title="change status"
                className="rounded border border-[var(--color-border)] bg-[var(--color-panel-2)] px-1 py-0.5 text-xs outline-none"
              >
                {TASK_COLUMNS.map((s) => (
                  <option key={s} value={s}>
                    {STATUS_LABEL[s]}
                  </option>
                ))}
              </select>
              <button
                onClick={() => void addComment(t.id)}
                className="rounded px-1.5 py-0.5 text-xs text-sky-400 hover:bg-[var(--color-panel-2)]"
              >
                comment
              </button>
            </li>
          ))}
        </ul>
      </div>
    </main>
  )
}
