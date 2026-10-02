import { useEffect, useRef, useState } from 'react'
import {
  Link,
  Navigate,
  useLocation,
  useNavigate,
  useOutletContext,
  useParams,
} from 'react-router-dom'
import { type TaskStatus } from './api'
import { DocStatusChip } from './Documents'
import {
  answerQuestion,
  cancelQuestion,
  commentTask,
  createTask,
  declineQuestion,
  moveTask,
  supersedeQuestion,
  reparentTask,
  updateTask,
  useExternalNameResolver,
  useProjects,
  useTask,
  useTasks,
} from './resources'
import { Markdown } from './markdown'
import { CommentAnnotations } from './CommentAnnotations'
import { QuestionComment } from './questions'
import {
  AuthorLabel,
  AutoGrowTextarea,
  Identity,
  relTime,
  StatusChip,
  STATUS_LABEL,
  TASK_COLUMNS,
} from './ui'

// Context handed down by the Board route (the parent <Outlet/>).
interface DrawerContext {
  actor: string
}

// Track the *visual* viewport (the region not covered by an on-screen keyboard). A fixed
// inset-0 overlay is sized to the layout viewport, so on mobile the keyboard shoves the whole
// panel up and the comment footer ends up behind it. Sizing the overlay to visualViewport
// instead keeps the footer pinned just above the keyboard — it "pops over" rather than pushing
// the page. On desktop (and where the API is absent) this is just innerHeight at top 0.
function useVisualViewport(): { top: number; height: number } {
  const [vp, setVp] = useState(() => ({
    top: window.visualViewport?.offsetTop ?? 0,
    height: window.visualViewport?.height ?? window.innerHeight,
  }))
  useEffect(() => {
    const v = window.visualViewport
    if (!v) return
    const onChange = () => setVp({ top: v.offsetTop, height: v.height })
    v.addEventListener('resize', onChange)
    v.addEventListener('scroll', onChange)
    return () => {
      v.removeEventListener('resize', onChange)
      v.removeEventListener('scroll', onChange)
    }
  }, [])
  return vp
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
  const extName = useExternalNameResolver()
  const [error, setError] = useState<string | null>(null)
  const [comment, setComment] = useState('')
  const [busy, setBusy] = useState(false)
  // Inline edit drafts: null = not editing that field, string = editing with this draft.
  const [editTitle, setEditTitle] = useState<string | null>(null)
  const [editDesc, setEditDesc] = useState<string | null>(null)
  const [editMeta, setEditMeta] = useState<string | null>(null)
  // Long comment threads collapse the older ones behind a "show N earlier" button, keeping the
  // latest few (and the compose box) in view — the operator usually only cares about recent replies.
  const [showAllComments, setShowAllComments] = useState(false)

  const onClose = () => navigate(`/projects/${projectId}`)
  const vp = useVisualViewport()
  const location = useLocation()

  // Deep-link to a specific comment: /tasks/:id#comment-<id> scrolls it into view and briefly
  // flashes a ring. The comment list renders async (after the task loads), so the browser's own
  // fragment scroll misses it — we scroll once the element exists. We can't use the :target CSS
  // pseudo because the id-only route redirects via the History API (which doesn't update :target),
  // so the flash is a transient Web Animations API ring that reverts on its own (no lingering
  // state). A ref guards against re-running on later background task refreshes.
  const flashedHash = useRef<string | null>(null)
  useEffect(() => {
    const m = /^#comment-(\d+)$/.exec(location.hash)
    if (!m || !task || flashedHash.current === location.hash) return
    // A deep-linked comment that would otherwise be collapsed is force-shown during render (see
    // the comments list below), so by the time this runs the element exists.
    const el = document.getElementById(`comment-${m[1]}`)
    if (el) {
      el.scrollIntoView({ block: 'center' })
      el.animate(
        [
          { boxShadow: '0 0 0 2px rgba(56, 189, 248, 0.7)' },
          { boxShadow: '0 0 0 2px rgba(56, 189, 248, 0.7)', offset: 0.6 },
          { boxShadow: '0 0 0 2px rgba(56, 189, 248, 0)' },
        ],
        { duration: 2200, easing: 'ease-out' },
      )
      flashedHash.current = location.hash
    }
  }, [location.hash, task])

  async function setStatus(status: TaskStatus) {
    if (!task || status === task.status) return
    setBusy(true)
    try {
      await updateTask(task.id, { status, principal: actor })
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
      await updateTask(task.id, { assignee, principal: actor })
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  async function move(to: number) {
    if (!task || to === task.project_id) return
    setBusy(true)
    try {
      await moveTask(task.id, { to_project_id: to, principal: actor })
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
      await commentTask(task.id, { body, principal: actor })
      setComment('')
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  // Operator-questions actions (task_629 slice 2): answer / decline / cancel a question comment.
  async function answerQ(commentId: number, shape: string, value: unknown) {
    if (!task) return
    setBusy(true)
    try {
      await answerQuestion(task.id, commentId, { shape, value, principal: actor })
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }
  async function declineQ(commentId: number) {
    if (!task) return
    const feedback = window.prompt('Decline this question — a short note on why / what to do instead:')
    if (feedback == null) return
    setBusy(true)
    try {
      await declineQuestion(task.id, commentId, { feedback, principal: actor })
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }
  async function cancelQ(commentId: number) {
    if (!task || !window.confirm('Cancel this question? It will be marked cancelled.')) return
    setBusy(true)
    try {
      await cancelQuestion(task.id, commentId, { principal: actor })
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }
  async function supersedeQ(commentId: number) {
    if (!task) return
    const new_prompt = window.prompt(
      'Re-pose this question with a new prompt (the old one is kept and linked):',
    )
    if (new_prompt == null || !new_prompt.trim()) return
    setBusy(true)
    try {
      await supersedeQuestion(task.id, commentId, { new_prompt, principal: actor })
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
        principal: actor,
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

  // Apply a field edit through the same updateTask choke point the rest of the drawer uses.
  async function save(patch: Parameters<typeof updateTask>[1]) {
    if (!task) return
    setBusy(true)
    try {
      await updateTask(task.id, { ...patch, principal: actor })
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  function commitTitle() {
    const v = (editTitle ?? '').trim()
    setEditTitle(null)
    if (task && v && v !== task.title) void save({ title: v })
  }

  async function saveMeta() {
    if (editMeta === null) return
    let parsed: unknown
    try {
      parsed = JSON.parse(editMeta)
    } catch {
      setError('Metadata must be valid JSON.')
      return
    }
    if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) {
      setError('Metadata must be a JSON object.')
      return
    }
    setEditMeta(null)
    await save({ metadata: parsed as Record<string, unknown> })
  }

  const shownError = error ?? loadError?.message ?? null

  return (
    <div
      className="fixed inset-x-0 z-40 flex justify-end"
      style={{ top: vp.top, height: vp.height }}
    >
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
                <div className="mb-1 flex flex-wrap items-center gap-2 text-xs text-[var(--color-muted)]">
                  <span>task #{task.id}</span>
                  {/* Liveness-monitor exemption (task 520 family / #506 guard): surfaced + toggleable
                      here. Amber when exempt (visible); a subtle "monitored" affordance otherwise.
                      Toggling merges metadata.monitor_exempt server-side. */}
                  <button
                    onClick={() => void save({ metadata: { monitor_exempt: !task.monitor_exempt } })}
                    disabled={busy}
                    title={
                      task.monitor_exempt
                        ? 'Exempt from the liveness monitor / nudge daemon. Click to put it back under monitoring.'
                        : 'Under the liveness monitor / nudge daemon. Click to mark it exempt (e.g. a long-running or intentionally-idle task).'
                    }
                    className={`rounded px-1.5 py-0.5 ring-1 ring-inset transition disabled:opacity-40 ${
                      task.monitor_exempt
                        ? 'bg-amber-500/15 text-amber-300 ring-amber-500/30'
                        : 'text-[var(--color-muted)] ring-[var(--color-border)] hover:bg-[var(--color-panel-2)]'
                    }`}
                  >
                    {task.monitor_exempt ? 'monitor exempt' : 'monitored'}
                  </button>
                </div>
                {editTitle === null ? (
                  <h2
                    onClick={() => setEditTitle(task.title)}
                    title="click to edit"
                    className="cursor-text text-lg font-semibold leading-snug hover:text-sky-200"
                  >
                    {task.title}
                  </h2>
                ) : (
                  <input
                    autoFocus
                    value={editTitle}
                    disabled={busy}
                    onChange={(e) => setEditTitle(e.target.value)}
                    onBlur={commitTitle}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') commitTitle()
                      else if (e.key === 'Escape') setEditTitle(null)
                    }}
                    className="w-full rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-lg font-semibold outline-none focus:border-sky-500/50"
                  />
                )}
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
                    {task.assignee ? <Identity id={task.assignee} /> : '— assign —'}
                  </button>
                </dd>
                <dt className="text-[var(--color-muted)]">Project</dt>
                <dd className="col-span-2">
                  {(() => {
                    // Move targets: active projects, plus the current one guaranteed present
                    // (so it stays the selected option even if it's since been archived).
                    const targets = projects.filter(
                      (p) => p.status !== 'archived' || p.id === task.project_id,
                    )
                    if (!targets.some((p) => p.id === task.project_id)) {
                      targets.unshift({
                        ...(projects.find((p) => p.id === task.project_id) ?? ({} as never)),
                        id: task.project_id,
                        name: `#${task.project_id}`,
                      })
                    }
                    return (
                      <select
                        value={task.project_id}
                        disabled={busy}
                        onChange={(e) => void move(Number(e.target.value))}
                        title="Move this task to another project"
                        className="rounded border border-[var(--color-border)] bg-[var(--color-panel-2)] px-1.5 py-0.5 text-xs outline-none focus:border-sky-500/50"
                      >
                        {targets.map((p) => (
                          <option key={p.id} value={p.id}>
                            {p.name}
                          </option>
                        ))}
                      </select>
                    )
                  })()}
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
                <dd className="col-span-2">
                  <select
                    value={task.priority ?? ''}
                    disabled={busy}
                    onChange={(e) => void save({ priority: e.target.value })}
                    className="rounded border border-[var(--color-border)] bg-[var(--color-panel-2)] px-1.5 py-0.5 text-xs outline-none focus:border-sky-500/50"
                  >
                    <option value="">—</option>
                    <option value="low">low</option>
                    <option value="normal">normal</option>
                    <option value="high">high</option>
                    {task.priority && !['low', 'normal', 'high'].includes(task.priority) && (
                      <option value={task.priority}>{task.priority}</option>
                    )}
                  </select>
                </dd>
                <dt className="text-[var(--color-muted)]">Created by</dt>
                <dd className="col-span-2 font-mono text-xs">{task.created_by ?? '—'}</dd>
                {/* Created / last-updated timestamps (task 748): relative for scanning, with the
                    exact local time on hover. */}
                <dt className="text-[var(--color-muted)]">Created</dt>
                <dd className="col-span-2 text-xs" title={new Date(task.created_at).toLocaleString()}>
                  {relTime(task.created_at)}
                </dd>
                <dt className="text-[var(--color-muted)]">Updated</dt>
                <dd className="col-span-2 text-xs" title={new Date(task.updated_at).toLocaleString()}>
                  {relTime(task.updated_at)}
                </dd>
                <dt className="text-[var(--color-muted)]">Subscribers</dt>
                <dd className="col-span-2 font-mono text-xs">
                  {task.subscribers.length ? task.subscribers.join(', ') : '—'}
                </dd>
              </dl>

              <div className="mb-5">
                <div className="mb-1 flex items-center justify-between text-xs text-[var(--color-muted)]">
                  <span>Description</span>
                  {editDesc === null && (
                    <button
                      onClick={() => setEditDesc(task.description ?? '')}
                      disabled={busy}
                      className="rounded px-1.5 py-0.5 text-sky-400 hover:bg-[var(--color-panel-2)]"
                    >
                      edit
                    </button>
                  )}
                </div>
                {editDesc === null ? (
                  task.description ? (
                    <Markdown source={task.description} className="text-sm" />
                  ) : (
                    <p className="text-sm text-[var(--color-muted)]">— none —</p>
                  )
                ) : (
                  <div className="space-y-2">
                    <textarea
                      autoFocus
                      value={editDesc}
                      rows={6}
                      onChange={(e) => setEditDesc(e.target.value)}
                      className="w-full rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-sm outline-none focus:border-sky-500/50"
                    />
                    <div className="flex gap-2">
                      <button
                        disabled={busy}
                        onClick={async () => {
                          const v = editDesc
                          setEditDesc(null)
                          await save({ description: v })
                        }}
                        className="rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40"
                      >
                        Save
                      </button>
                      <button
                        onClick={() => setEditDesc(null)}
                        className="rounded-md px-2.5 py-1 text-xs text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
                      >
                        Cancel
                      </button>
                    </div>
                  </div>
                )}
              </div>

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

              <div className="mb-5">
                <div className="mb-1 flex items-center justify-between text-xs text-[var(--color-muted)]">
                  <span>Metadata</span>
                  {editMeta === null && (
                    <button
                      onClick={() => setEditMeta(JSON.stringify(task.metadata ?? {}, null, 2))}
                      disabled={busy}
                      className="rounded px-1.5 py-0.5 text-sky-400 hover:bg-[var(--color-panel-2)]"
                    >
                      edit
                    </button>
                  )}
                </div>
                {editMeta === null ? (
                  Object.keys(task.metadata ?? {}).length > 0 ? (
                    <pre className="overflow-x-auto rounded-md bg-[var(--color-panel-2)] p-3 text-xs">
                      {JSON.stringify(task.metadata, null, 2)}
                    </pre>
                  ) : (
                    <p className="text-sm text-[var(--color-muted)]">— none —</p>
                  )
                ) : (
                  <div className="space-y-2">
                    <textarea
                      autoFocus
                      value={editMeta}
                      rows={6}
                      onChange={(e) => setEditMeta(e.target.value)}
                      className="w-full rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] p-2 font-mono text-xs outline-none focus:border-sky-500/50"
                    />
                    <p className="text-[11px] text-[var(--color-muted)]">
                      Keys are merged into the task's existing metadata server-side; this can't
                      remove a key.
                    </p>
                    <div className="flex gap-2">
                      <button
                        disabled={busy}
                        onClick={saveMeta}
                        className="rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40"
                      >
                        Save
                      </button>
                      <button
                        onClick={() => setEditMeta(null)}
                        className="rounded-md px-2.5 py-1 text-xs text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
                      >
                        Cancel
                      </button>
                    </div>
                  </div>
                )}
              </div>

              {task.blocking_questions && task.blocking_questions.length > 0 && (
                <div className="rounded-md border border-amber-500/30 bg-amber-500/10 p-3">
                  <div className="mb-1.5 text-xs font-medium text-amber-300">
                    Blocked by {task.blocking_questions.length} open question
                    {task.blocking_questions.length > 1 ? 's' : ''}
                    {task.question_blocked_on && task.question_blocked_on.length > 0 && (
                      <span className="font-normal text-amber-300/70">
                        {' '}
                        (awaiting {task.question_blocked_on.join(', ')})
                      </span>
                    )}
                  </div>
                  <ul className="space-y-1">
                    {task.blocking_questions.map((q) => (
                      <li key={q.comment_id}>
                        <button
                          onClick={() => navigate({ hash: `comment-${q.comment_id}` })}
                          className="text-left text-xs text-amber-200/90 hover:text-amber-100 hover:underline"
                        >
                          {q.prompt.length > 90 ? `${q.prompt.slice(0, 90)}...` : q.prompt}
                          {q.routed_to && (
                            <span className="text-amber-300/70"> - {q.routed_to}</span>
                          )}
                        </button>
                      </li>
                    ))}
                  </ul>
                </div>
              )}

              <div>
                <div className="mb-2 text-xs text-[var(--color-muted)]">
                  Comments ({task.comments.length})
                </div>
                {(() => {
                  // Keep the latest few in view; collapse older ones behind an expander once the
                  // thread is long enough that hiding is worthwhile (chronological order is kept).
                  const VISIBLE = 3
                  const hidden = task.comments.length - VISIBLE
                  // Force-expand when a #comment-<id> deep-link targets one of the older (hidden)
                  // comments, so the link lands (derived here rather than via setState in an effect).
                  const targetId = /^#comment-(\d+)$/.exec(location.hash)?.[1]
                  const targetHidden =
                    targetId != null &&
                    task.comments.slice(0, Math.max(0, hidden)).some((c) => String(c.id) === targetId)
                  const collapsed = !showAllComments && hidden >= 2 && !targetHidden
                  const shown = collapsed ? task.comments.slice(-VISIBLE) : task.comments
                  // Operator-questions (task_629): group answers under their question, and track
                  // which comments are in view so a nested answer isn't also rendered standalone.
                  const answersByQ = new Map<number, typeof task.comments>()
                  for (const c of task.comments) {
                    if (c.type === 'answer' && c.reply_to != null) {
                      const arr = answersByQ.get(c.reply_to) ?? []
                      arr.push(c)
                      answersByQ.set(c.reply_to, arr)
                    }
                  }
                  const shownIds = new Set(shown.map((c) => c.id))
                  return (
                    <ul className="space-y-3">
                      {collapsed && (
                        <li>
                          <button
                            onClick={() => setShowAllComments(true)}
                            className="w-full rounded-md border border-dashed border-[var(--color-border)] px-3 py-2 text-xs text-[var(--color-muted)] hover:border-sky-500/40 hover:text-sky-300"
                          >
                            Show {hidden} earlier comment{hidden === 1 ? '' : 's'}
                          </button>
                        </li>
                      )}
                      {shown.map((c) => {
                        // An answer renders nested under its question when that question is in
                        // view; skip it at the top level to avoid showing it twice.
                        if (c.type === 'answer' && c.reply_to != null && shownIds.has(c.reply_to)) {
                          return null
                        }
                        return (
                          <li
                            key={c.id}
                            id={`comment-${c.id}`}
                            className="scroll-mt-4 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] p-3"
                          >
                            {c.type === 'question' ? (
                              <QuestionComment
                                comment={c}
                                answers={answersByQ.get(c.id) ?? []}
                                resolveExternal={extName}
                                actor={actor}
                                busy={busy}
                                onAnswer={(shape, value) => void answerQ(c.id, shape, value)}
                                onDecline={() => void declineQ(c.id)}
                                onCancel={() => void cancelQ(c.id)}
                                onSupersede={() => void supersedeQ(c.id)}
                              />
                            ) : (
                              <>
                                <div className="mb-1 flex items-center justify-between text-xs text-[var(--color-muted)]">
                                  <AuthorLabel
                                    author={c.author}
                                    externalAuthor={c.external_author}
                                    resolveExternal={extName}
                                  />
                                  <span>{relTime(c.created_at)}</span>
                                </div>
                                <CommentAnnotations
                                  commentId={c.id}
                                  actor={actor}
                                  body={c.body}
                                  resolveExternal={extName}
                                />
                              </>
                            )}
                          </li>
                        )
                      })}
                      {task.comments.length === 0 && (
                        <li className="text-sm text-[var(--color-muted)]">No comments yet.</li>
                      )}
                    </ul>
                  )
                })()}
              </div>
            </div>

            <footer className="border-t border-[var(--color-border)] p-4">
              {/* Surface a submit failure right at the composer (task_1201): the API's validation
                  reason (non-ASCII, banned phrase, ...) is in the thrown error message, but the
                  shared error banner is at the top of the drawer -- off-screen when you are typing
                  down here, so a failed comment looked like nothing happened. */}
              {error && (
                <p className="mb-2 rounded-md border border-rose-500/30 bg-rose-500/10 px-3 py-2 text-xs text-rose-300">
                  {error}
                </p>
              )}
              <div className="flex items-end gap-2">
                <AutoGrowTextarea
                  value={comment}
                  onChange={setComment}
                  onSubmit={addComment}
                  placeholder={`Comment as ${actor}…`}
                  className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-3 py-2 text-sm outline-none focus:border-sky-500/50"
                />
                <button
                  onClick={addComment}
                  disabled={busy || !comment.trim()}
                  className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white disabled:opacity-40"
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

// Bare /tasks/:taskId deep-link: the task drawer lives under a project route, so a link that
// only knows the task id resolves the task, then redirects to the canonical nested URL —
// preserving any #comment-<id> fragment. Lets an id-only link (e.g. /board/tasks/42) just work.
export function TaskRedirect() {
  const { taskId } = useParams()
  const location = useLocation()
  const { data: task, error } = useTask(Number(taskId))
  if (error) {
    return <div className="p-6 text-[var(--color-muted)]">Error: {error.message}</div>
  }
  if (!task) {
    return <div className="p-6 text-[var(--color-muted)]">Loading…</div>
  }
  return <Navigate to={`/projects/${task.project_id}/tasks/${task.id}${location.hash}`} replace />
}
