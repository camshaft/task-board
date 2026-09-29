import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { api, type TaskStatus, type TaskSummary } from './api'
import { useBoardContext } from './Layout'
import { commentTask, updateTask, useProjects } from './resources'
import { STATUS_LABEL, StatusChip, TASK_COLUMNS, relTime } from './ui'

// Cross-project search + a personal "my tasks" view. Unlike the per-project Board, this queries
// the WHOLE board (no project_id) so you can find or triage tasks anywhere — filtered by free
// text, assignee (defaults to you), and status (e.g. blocked) — with inline status + comment
// actions. It fetches on demand (search is transient) rather than living in the resource store,
// and re-runs after each mutation so the list stays current.
export default function Search() {
  const { actor } = useBoardContext()
  const { data: projects = [] } = useProjects()
  const projectName = (id?: number) =>
    projects.find((p) => p.id === id)?.name ?? (id != null ? `#${id}` : '')

  const [q, setQ] = useState('')
  const [assignee, setAssignee] = useState(actor)
  const [status, setStatus] = useState<'' | TaskStatus>('')
  const [results, setResults] = useState<TaskSummary[] | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

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
      await updateTask(id, { status: s, actor })
      await run()
    } catch (e) {
      window.alert((e as Error).message)
    }
  }

  async function addComment(id: number) {
    const body = window.prompt('Comment:')
    if (!body?.trim()) return
    try {
      await commentTask(id, { body: body.trim(), author: actor })
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
            onChange={(e) => setStatus(e.target.value as '' | TaskStatus)}
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
          <button
            type="button"
            onClick={() => {
              setQ('')
              setAssignee(actor)
              setStatus('blocked')
              void run({ q: '', assignee: actor, status: 'blocked' })
            }}
            className="rounded-md px-2 py-1 text-xs text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
          >
            blocked on me
          </button>
        </form>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-5 py-3">
        {error && <p className="text-sm text-rose-300">{error}</p>}
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
                <span className="font-mono text-[11px] text-[var(--color-muted)]">{t.assignee}</span>
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
