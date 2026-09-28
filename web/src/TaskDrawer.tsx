import { useState } from 'react'
import { useNavigate, useOutletContext, useParams } from 'react-router-dom'
import { type TaskStatus } from './api'
import { commentTask, updateTask, useTask } from './resources'
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
