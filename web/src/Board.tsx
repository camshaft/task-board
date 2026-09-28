import { useCallback, useEffect, useState } from 'react'
import { Link, Outlet, useParams } from 'react-router-dom'
import { api, type TaskSummary } from './api'
import { useBoardContext } from './Layout'
import { PriorityDot, STATUS_LABEL, TASK_COLUMNS } from './ui'

// The kanban board for one project (from the :projectId route param). Renders its own
// <Outlet/> so the task drawer (a nested route) layers over the board.
export default function Board() {
  const { projectId } = useParams()
  const project = Number(projectId)
  const { actor, projects, refreshChrome } = useBoardContext()
  const [tasks, setTasks] = useState<TaskSummary[]>([])
  const [error, setError] = useState<string | null>(null)

  const refreshTasks = useCallback(async () => {
    try {
      setTasks(await api.listTasks({ project_id: project }))
    } catch (e) {
      setError((e as Error).message)
    }
  }, [project])

  useEffect(() => {
    refreshTasks()
  }, [refreshTasks])

  async function newTask() {
    const title = window.prompt('Task title:')
    if (!title?.trim()) return
    const assignee = window.prompt('Assignee (agent id, optional):') || undefined
    try {
      await api.createTask({ project_id: project, title: title.trim(), assignee, created_by: actor })
      refreshTasks()
      refreshChrome()
    } catch (e) {
      setError((e as Error).message)
    }
  }

  const current = projects.find((p) => p.id === project)

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="flex items-center gap-3 border-b border-[var(--color-border)] px-5 py-3">
        <h2 className="truncate text-sm font-semibold">{current?.name ?? `Project #${project}`}</h2>
        <button
          onClick={newTask}
          className="rounded-md bg-sky-600 px-2.5 py-1 text-xs font-medium text-white hover:bg-sky-500"
        >
          + task
        </button>
      </div>

      {error && (
        <div className="border-b border-rose-500/30 bg-rose-500/10 px-5 py-2 text-sm text-rose-300">
          {error}
        </div>
      )}

      <div className="flex min-h-0 flex-1 gap-3 overflow-x-auto p-4">
        {TASK_COLUMNS.map((col) => {
          const items = tasks.filter((t) => t.status === col)
          return (
            <div key={col} className="flex w-72 shrink-0 flex-col">
              <div className="mb-2 flex items-center justify-between px-1">
                <span className="text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
                  {STATUS_LABEL[col]}
                </span>
                <span className="text-xs text-[var(--color-muted)]">{items.length}</span>
              </div>
              <div className="flex flex-1 flex-col gap-2 rounded-lg bg-[var(--color-panel)]/40 p-2">
                {items.map((t) => (
                  <Link
                    key={t.id}
                    to={`tasks/${t.id}`}
                    className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] p-3 text-left transition hover:border-sky-500/40"
                  >
                    <div className="mb-2 flex items-start gap-2">
                      <PriorityDot priority={t.priority} />
                      <span className="text-sm leading-snug">{t.title}</span>
                    </div>
                    <div className="flex items-center justify-between text-xs text-[var(--color-muted)]">
                      <span className="font-mono">#{t.id}</span>
                      {t.assignee && <span className="font-mono">{t.assignee}</span>}
                    </div>
                  </Link>
                ))}
                {items.length === 0 && (
                  <div className="px-1 py-2 text-xs text-[var(--color-muted)]/60">—</div>
                )}
              </div>
            </div>
          )
        })}
      </div>

      {/* The task drawer, when the URL is …/tasks/:taskId. It needs to refetch the board
          (status moves) and chrome (counts) on mutation, so hand those down via context. */}
      <Outlet context={{ actor, refreshBoard: refreshTasks, refreshChrome }} />
    </main>
  )
}
