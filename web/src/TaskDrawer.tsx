import { useState } from 'react'
import { Link, useNavigate, useOutletContext, useParams } from 'react-router-dom'
import { type TaskStatus } from './api'
import { DocStatusChip } from './Documents'
import {
  commentTask,
  createTask,
  moveTask,
  reparentTask,
  updateTask,
  useProjects,
  useTask,
  useTasks,
} from './resources'
import { relTime, StatusChip, STATUS_LABEL, TASK_COLUMNS } from './ui'

// Context handed down by the Board route (the parent <Outlet/>).
interface DrawerContext {
  actor: string
}

// A slide-over panel showing one task (from the :taskId route param): fields, editable
// status/assignee, comments. Closing is just navigating back to the project.
export function TaskDrawer() {
  const { projectId, taskId } = useParams()
  const navigate = useNavigate()
  const { actor } = useOutletContext<DrawerContext>()
  const id = Number(taskId)
  const { data: task, error: loadError } = useTask(id)
  const { data: projects = [] } = useProjects()
  // Same-project tasks, for the reparent picker. Keyed on the task's project once it loads.
  const { data: siblings = [] } = useTasks(task?.project_id ?? -1)
  const [error, setError] = useState<string | null>(null)
  const [comment, setComment] = useState('')
  const [busy, setBusy] = useState(false)

  const onClose = () => navigate(`/projects/${projectId}`)

  async function setStatus(status: TaskStatus) {
    if (!task || status === task.status) return
    setBusy(true)
    try {
      await updateTask(task.id, { status, actor })
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  async function reassign() {
    if (!task) return
    const assignee = window.prompt('Assign to (agent id):', task.assignee ?? '')
    if (assignee == null) return
    setBusy(true)
    try {
      await updateTask(task.id, { assignee, actor })
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  async function move() {
    if (!task) return
    // A tiny prompt-based picker: list the other projects by id so the user can pick one.
    const others = projects.filter((p) => p.id !== task.project_id)
    if (others.length === 0) {
      window.alert('No other project to move this task to.')
      return
    }
    const menu = others.map((p) => `${p.id}: ${p.name}`).join('\n')
    const answer = window.prompt(`Move task to which project? Enter its id:\n\n${menu}`)
    if (answer == null) return
    const to = Number(answer.trim())
    if (!Number.isInteger(to) || !others.some((p) => p.id === to)) {
      setError(`'${answer}' isn't one of the listed project ids.`)
      return
    }
    setBusy(true)
    try {
      await moveTask(task.id, { to_project_id: to, actor })
      // The task left this board; follow it to its new project so the drawer stays valid.
      navigate(`/projects/${to}/tasks/${task.id}`)
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  async function addComment() {
    const body = comment.trim()
    if (!body || !task) return
    setBusy(true)
    try {
      await commentTask(task.id, { body, author: actor })
      setComment('')
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  async function addSubtask() {
    if (!task) return
    const title = window.prompt('Subtask title:')
    if (!title?.trim()) return
    setBusy(true)
    try {
      await createTask({
        project_id: task.project_id,
        title: title.trim(),
        parent_id: task.id,
        created_by: actor,
      })
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  async function reparent() {
    if (!task) return
    // Candidates: same-project tasks, excluding self and this task's own children (the obvious
    // cycle; the server guards deeper ones and cross-project).
    const childIds = new Set((task.children ?? []).map((c) => c.id))
    const candidates = siblings.filter((s) => s.id !== task.id && !childIds.has(s.id))
    if (candidates.length === 0) {
      window.alert('No other task in this project to nest under.')
      return
    }
    const menu = candidates.map((s) => `${s.id}: ${s.title}`).join('\n')
    const answer = window.prompt(`Nest under which task? Enter its id:\n\n${menu}`)
    if (answer == null || !answer.trim()) return
    const to = Number(answer.trim())
    if (!Number.isInteger(to) || !candidates.some((s) => s.id === to)) {
      setError(`'${answer}' isn't one of the listed task ids.`)
      return
    }
    setBusy(true)
    try {
      await reparentTask(task.id, to, actor)
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  async function clearParent() {
    if (!task) return
    setBusy(true)
    try {
      await reparentTask(task.id, 0, actor) // 0 = clear parent (back to top-level)
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  const shownError = error ?? loadError?.message ?? null

  return (
    <div className="fixed inset-0 z-40 flex justify-end">
      <div
        className="absolute inset-0 bg-black/50"
        onClick={onClose}
        aria-hidden
      />
      <aside className="relative z-50 flex h-full w-full max-w-xl flex-col border-l border-[var(--color-border)] bg-[var(--color-panel)] shadow-2xl">
        {!task ? (
          <div className="p-6 text-[var(--color-muted)]">
            {shownError ? `Error: ${shownError}` : 'Loading…'}
          </div>
        ) : (
          <>
            <header className="flex items-start gap-3 border-b border-[var(--color-border)] p-5">
              <div className="min-w-0 flex-1">
                <div className="mb-1 text-xs text-[var(--color-muted)]">
                  task #{task.id}
                </div>
                <h2 className="text-lg font-semibold leading-snug">{task.title}</h2>
              </div>
              <button
                onClick={onClose}
                className="rounded-md px-2 py-1 text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
              >
                ✕
              </button>
            </header>

            <div className="flex-1 overflow-y-auto p-5">
              {shownError && (
                <div className="mb-3 rounded-md bg-rose-500/15 px-3 py-2 text-sm text-rose-300">
                  {shownError}
                </div>
              )}

              <div className="mb-4 flex flex-wrap items-center gap-2">
                {TASK_COLUMNS.map((s) => (
                  <button
                    key={s}
                    disabled={busy}
                    onClick={() => setStatus(s)}
                    className={`rounded-md px-2.5 py-1 text-xs ring-1 ring-inset transition ${
                      s === task.status
                        ? 'bg-sky-500/20 text-sky-200 ring-sky-500/40'
                        : 'text-[var(--color-muted)] ring-[var(--color-border)] hover:bg-[var(--color-panel-2)]'
                    }`}
                  >
                    {STATUS_LABEL[s]}
                  </button>
                ))}
              </div>

              <dl className="mb-5 grid grid-cols-3 gap-y-2 text-sm">
                <dt className="text-[var(--color-muted)]">Status</dt>
                <dd className="col-span-2">
                  <StatusChip status={task.status} />
                </dd>
                <dt className="text-[var(--color-muted)]">Assignee</dt>
                <dd className="col-span-2">
                  <button
                    onClick={reassign}
                    className="rounded px-1.5 py-0.5 font-mono text-xs hover:bg-[var(--color-panel-2)]"
                  >
                    {task.assignee ?? '— assign —'}
                  </button>
                </dd>
                <dt className="text-[var(--color-muted)]">Project</dt>
                <dd className="col-span-2">
                  <button
                    onClick={move}
                    disabled={busy}
                    className="rounded px-1.5 py-0.5 text-xs hover:bg-[var(--color-panel-2)]"
                    title="Move this task to another project"
                  >
                    {projects.find((p) => p.id === task.project_id)?.name ?? `#${task.project_id}`}
                    <span className="ml-1 text-[var(--color-muted)]">move →</span>
                  </button>
                </dd>
                <dt className="text-[var(--color-muted)]">Parent</dt>
                <dd className="col-span-2">
                  {task.parent_id != null ? (
                    <span className="flex items-center gap-2">
                      <Link
                        to={`/projects/${task.project_id}/tasks/${task.parent_id}`}
                        className="text-sky-400 hover:text-sky-300"
                      >
                        #{task.parent_id} {task.parent_title ?? ''}
                      </Link>
                      <button
                        onClick={clearParent}
                        disabled={busy}
                        className="text-xs text-[var(--color-muted)] hover:text-rose-300"
                      >
                        clear
                      </button>
                    </span>
                  ) : (
                    <button
                      onClick={reparent}
                      disabled={busy}
                      className="text-xs text-[var(--color-muted)] hover:text-sky-300"
                    >
                      — set parent —
                    </button>
                  )}
                </dd>
                <dt className="text-[var(--color-muted)]">Priority</dt>
                <dd className="col-span-2">{task.priority ?? '—'}</dd>
                <dt className="text-[var(--color-muted)]">Created by</dt>
                <dd className="col-span-2 font-mono text-xs">{task.created_by ?? '—'}</dd>
                <dt className="text-[var(--color-muted)]">Subscribers</dt>
                <dd className="col-span-2 font-mono text-xs">
                  {task.subscribers.length ? task.subscribers.join(', ') : '—'}
                </dd>
              </dl>

              {task.description && (
                <div className="mb-5">
                  <div className="mb-1 text-xs text-[var(--color-muted)]">Description</div>
                  <p className="whitespace-pre-wrap text-sm">{task.description}</p>
                </div>
              )}

              {/* Subtasks (one level of nesting in the UI). Offered on top-level tasks so an
                  epic can gather children; a task that already has a parent stays a leaf here. */}
              {task.parent_id == null && (
                <div className="mb-5">
                  <div className="mb-1 flex items-center justify-between text-xs text-[var(--color-muted)]">
                    <span>
                      Subtasks
                      {task.child_rollup && task.child_rollup.total > 0
                        ? ` (${task.child_rollup.done}/${task.child_rollup.total} done)`
                        : ''}
                    </span>
                    <button
                      onClick={addSubtask}
                      disabled={busy}
                      className="rounded px-1.5 py-0.5 text-sky-400 hover:bg-[var(--color-panel-2)]"
                    >
                      + subtask
                    </button>
                  </div>
                  {task.children && task.children.length > 0 ? (
                    <ul className="space-y-1.5">
                      {task.children.map((c) => (
                        <li key={c.id}>
                          <Link
                            to={`/projects/${task.project_id}/tasks/${c.id}`}
                            className="flex items-center gap-2 rounded px-1.5 py-1 hover:bg-[var(--color-panel-2)]"
                          >
                            <StatusChip status={c.status} />
                            <span className="min-w-0 flex-1 truncate text-sm">{c.title}</span>
                            <span className="font-mono text-[11px] text-[var(--color-muted)]">
                              #{c.id}
                            </span>
                          </Link>
                        </li>
                      ))}
                    </ul>
                  ) : (
                    <p className="text-xs text-[var(--color-muted)]">No subtasks yet.</p>
                  )}
                </div>
              )}

              {task.attached_documents && task.attached_documents.length > 0 && (
                <div className="mb-5">
                  <div className="mb-1 text-xs text-[var(--color-muted)]">Documents</div>
                  <ul className="space-y-1.5">
                    {task.attached_documents.map((d) => (
                      <li key={d.id} className="flex items-center gap-2">
                        <DocStatusChip status={d.status} />
                        <Link
                          to={`/documents/${d.id}`}
                          className="min-w-0 flex-1 truncate text-sm text-sky-400 hover:text-sky-300"
                        >
                          {d.title}
                        </Link>
                      </li>
                    ))}
                  </ul>
                </div>
              )}

              {Object.keys(task.metadata ?? {}).length > 0 && (
                <div className="mb-5">
                  <div className="mb-1 text-xs text-[var(--color-muted)]">Metadata</div>
                  <pre className="overflow-x-auto rounded-md bg-[var(--color-panel-2)] p-3 text-xs">
                    {JSON.stringify(task.metadata, null, 2)}
                  </pre>
                </div>
              )}

              <div>
                <div className="mb-2 text-xs text-[var(--color-muted)]">
                  Comments ({task.comments.length})
                </div>
                <ul className="space-y-3">
                  {task.comments.map((c) => (
                    <li
                      key={c.id}
                      className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] p-3"
                    >
                      <div className="mb-1 flex items-center justify-between text-xs text-[var(--color-muted)]">
                        <span className="font-mono">{c.author ?? 'anon'}</span>
                        <span>{relTime(c.created_at)}</span>
                      </div>
                      <p className="whitespace-pre-wrap text-sm">{c.body}</p>
                    </li>
                  ))}
                  {task.comments.length === 0 && (
                    <li className="text-sm text-[var(--color-muted)]">No comments yet.</li>
                  )}
                </ul>
              </div>
            </div>

            <footer className="border-t border-[var(--color-border)] p-4">
              <div className="flex gap-2">
                <input
                  value={comment}
                  onChange={(e) => setComment(e.target.value)}
                  onKeyDown={(e) => e.key === 'Enter' && addComment()}
                  placeholder={`Comment as ${actor}…`}
                  className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-3 py-2 text-sm outline-none focus:border-sky-500/50"
                />
                <button
                  onClick={addComment}
                  disabled={busy || !comment.trim()}
                  className="rounded-md bg-sky-600 px-3 py-2 text-sm font-medium text-white disabled:opacity-40"
                >
                  Send
                </button>
              </div>
            </footer>
          </>
        )}
      </aside>
    </div>
  )
}
