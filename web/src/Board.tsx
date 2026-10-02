import { useState } from 'react'
import { Link, Outlet, useParams } from 'react-router-dom'
import { type TaskSummary } from './api'
import { useBoardContext } from './Layout'
import { createTask, updateProject, updateTask, useIceboxTasks, useProjects, useTasks } from './resources'
import { Identity, PriorityDot, StatusChip, STATUS_LABEL, TASK_COLUMNS } from './ui'

// The kanban board for one project (from the :projectId route param). Renders its own
// <Outlet/> so the task drawer (a nested route) layers over the board.
export default function Board() {
  const { projectId } = useParams()
  const project = Number(projectId)
  const { actor } = useBoardContext()
  const { data: tasks = [], error: tasksError } = useTasks(project)
  // Icebox (task_1221): fetched separately since the default task view hides status=icebox. Always
  // fetched (cheap) so the toggle can show a count; the list only renders when expanded.
  const { data: iceboxed = [] } = useIceboxTasks(project)
  const [showIcebox, setShowIcebox] = useState(false)
  const { data: projects = [] } = useProjects()
  const [error, setError] = useState<string | null>(null)
  const [expanded, setExpanded] = useState<Set<number>>(new Set())
  // Inline new-task composer (replaces the old chained window.prompts).
  const [composing, setComposing] = useState(false)
  const [newTitle, setNewTitle] = useState('')
  const [newAssignee, setNewAssignee] = useState('')
  const [creating, setCreating] = useState(false)

  const toggleExpand = (id: number) =>
    setExpanded((s) => {
      const n = new Set(s)
      if (n.has(id)) n.delete(id)
      else n.add(id)
      return n
    })

  // Order every column (and each epic's subtasks) by most-recent activity — updated_at desc.
  // This is the general, consistent ordering the operator asked for: whatever moved most
  // recently floats to the top, so e.g. the done column reads newest-completed-first. ISO-8601
  // timestamps sort lexicographically, so a plain string compare is chronological.
  const byRecent = (a: TaskSummary, b: TaskSummary) =>
    (b.updated_at ?? '').localeCompare(a.updated_at ?? '')

  // Nesting: group children under their parent and show only top-level tasks on the board, so
  // an epic collapses its subtasks instead of flooding the columns. The list already carries
  // parent_id, so this is a pure client-side partition — no extra fetch.
  const childrenByParent = new Map<number, TaskSummary[]>()
  for (const t of tasks) {
    if (t.parent_id != null) {
      const arr = childrenByParent.get(t.parent_id) ?? []
      arr.push(t)
      childrenByParent.set(t.parent_id, arr)
    }
  }
  for (const arr of childrenByParent.values()) arr.sort(byRecent)
  const topLevel = tasks.filter((t) => t.parent_id == null)

  function openComposer() {
    setError(null)
    setComposing(true)
  }

  function cancelComposer() {
    setComposing(false)
    setNewTitle('')
    setNewAssignee('')
  }

  async function submitNewTask() {
    const title = newTitle.trim()
    if (!title || creating) return
    setCreating(true)
    try {
      await createTask({
        project_id: project,
        title,
        assignee: newAssignee.trim() || undefined,
        principal: actor,
      })
      cancelComposer()
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setCreating(false)
    }
  }

  const current = projects.find((p) => p.id === project)
  const repo = typeof current?.metadata?.repo === 'string' ? current.metadata.repo : null

  async function renameProject() {
    if (!current) return
    const name = window.prompt('Project name:', current.name)
    if (name == null || !name.trim() || name.trim() === current.name) return
    try {
      await updateProject(project, { name: name.trim(), principal: actor })
    } catch (e) {
      setError((e as Error).message)
    }
  }

  async function editRepo() {
    if (!current) return
    const url = window.prompt('Repository URL (blank to clear):', repo ?? '')
    if (url == null) return
    try {
      await updateProject(project, { metadata: { repo: url.trim() }, principal: actor })
    } catch (e) {
      setError((e as Error).message)
    }
  }

  async function archiveProject() {
    if (!current) return
    if (!window.confirm(`Archive “${current.name}”? It's hidden from the board but not deleted — you can restore it anytime.`)) return
    try {
      await updateProject(project, { status: 'archived', principal: actor })
    } catch (e) {
      setError((e as Error).message)
    }
  }

  async function restoreProject() {
    if (!current) return
    try {
      await updateProject(project, { status: 'active', principal: actor })
    } catch (e) {
      setError((e as Error).message)
    }
  }

  // One-click restore of an iceboxed task back to the active backlog (task_1221). Restore-to-todo
  // only, per board-pm: an iceboxed item's prior dependency may have shifted, so todo forces a fresh
  // re-triage. Both the icebox list and the active board refresh via the touched() in updateTask.
  async function restoreFromIcebox(id: number) {
    try {
      await updateTask(id, { status: 'todo', principal: actor })
    } catch (e) {
      setError((e as Error).message)
    }
  }

  const shownError = error ?? tasksError?.message ?? null

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="flex items-center gap-3 border-b border-[var(--color-border)] px-5 py-3">
        <h2 className="truncate text-sm font-semibold">{current?.name ?? `Project #${project}`}</h2>
        {current?.status === 'archived' && (
          <span className="rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide text-amber-800 dark:bg-amber-500/20 dark:text-amber-300">
            archived
          </span>
        )}
        {repo && (
          <a
            href={repo}
            className="max-w-[16rem] truncate text-xs text-sky-700 underline decoration-dotted underline-offset-2 hover:text-sky-800 dark:text-sky-400 dark:hover:text-sky-300"
            title={repo}
          >
            {repo.replace(/^https?:\/\//, '')}
          </a>
        )}
        <button
          onClick={openComposer}
          className="rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white hover:bg-sky-600"
        >
          + task
        </button>
        {/* Project actions. Kept as small text buttons so the board stays the focus. */}
        <div className="ml-auto flex items-center gap-1 text-xs text-[var(--color-muted)]">
          {iceboxed.length > 0 && (
            <button
              onClick={() => setShowIcebox((v) => !v)}
              title="Iceboxed tasks — kept, but not now"
              className={`rounded px-2 py-1 hover:bg-[var(--color-panel-2)] ${showIcebox ? 'text-cyan-700 dark:text-cyan-300' : ''}`}
            >
              {showIcebox ? '▾' : '▸'} Icebox ({iceboxed.length})
            </button>
          )}
          <Link to={`/projects/${project}/access`} className="rounded px-2 py-1 hover:bg-[var(--color-panel-2)]">
            Access
          </Link>
          <button onClick={renameProject} className="rounded px-2 py-1 hover:bg-[var(--color-panel-2)]">
            Rename
          </button>
          <button onClick={editRepo} className="rounded px-2 py-1 hover:bg-[var(--color-panel-2)]">
            {repo ? 'Edit repo' : '+ repo'}
          </button>
          {current?.status === 'archived' ? (
            <button
              onClick={restoreProject}
              className="rounded px-2 py-1 text-sky-700 hover:bg-[var(--color-panel-2)] dark:text-sky-400"
            >
              Restore
            </button>
          ) : (
            <button
              onClick={archiveProject}
              className="rounded px-2 py-1 text-rose-700 hover:bg-[var(--color-panel-2)] dark:text-rose-400"
            >
              Archive
            </button>
          )}
        </div>
      </div>

      {composing && (
        <div className="flex flex-wrap items-center gap-2 border-b border-[var(--color-border)] bg-[var(--color-panel)]/40 px-4 py-2">
          <input
            autoFocus
            value={newTitle}
            disabled={creating}
            onChange={(e) => setNewTitle(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') void submitNewTask()
              else if (e.key === 'Escape') cancelComposer()
            }}
            placeholder="Task title"
            className="min-w-0 flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-3 py-1.5 text-sm outline-none focus:border-sky-500/50"
          />
          <input
            value={newAssignee}
            disabled={creating}
            onChange={(e) => setNewAssignee(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') void submitNewTask()
              else if (e.key === 'Escape') cancelComposer()
            }}
            placeholder="assignee (optional)"
            className="w-40 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-3 py-1.5 font-mono text-xs outline-none focus:border-sky-500/50"
          />
          <button
            onClick={submitNewTask}
            disabled={creating || !newTitle.trim()}
            className="rounded-md bg-sky-700 px-3 py-1.5 text-xs font-medium text-white hover:bg-sky-600 disabled:opacity-40"
          >
            Add task
          </button>
          <button
            onClick={cancelComposer}
            className="rounded-md px-2.5 py-1.5 text-xs text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
          >
            Cancel
          </button>
        </div>
      )}

      {shownError && (
        <div className="border-b border-rose-500/30 bg-rose-500/10 px-5 py-2 text-sm text-rose-800 dark:text-rose-300">
          {shownError}
        </div>
      )}

      {showIcebox && iceboxed.length > 0 && (
        // On-demand icebox view (task_1221): kept out of the active kanban (not a TASK_COLUMNS
        // column), revealed only via the header toggle. Each item one-click restores to To do.
        <div className="border-b border-[var(--color-border)] bg-[var(--color-panel)]/40 px-4 py-3">
          <p className="mb-2 text-xs font-semibold uppercase tracking-wide text-cyan-700 dark:text-cyan-300">
            Icebox — kept, not now ({iceboxed.length})
          </p>
          <ul className="flex flex-wrap gap-2">
            {iceboxed.sort(byRecent).map((t) => (
              <li
                key={t.id}
                className="flex items-center gap-2 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2.5 py-1.5"
              >
                <PriorityDot priority={t.priority} />
                <Link to={`tasks/${t.id}`} className="max-w-xs truncate text-sm hover:text-sky-700 dark:hover:text-sky-300">
                  {t.title}
                </Link>
                <span className="font-mono text-[11px] text-[var(--color-muted)]">#{t.id}</span>
                <button
                  onClick={() => restoreFromIcebox(t.id)}
                  className="rounded px-1.5 py-0.5 text-xs text-sky-700 hover:bg-[var(--color-panel)] hover:text-sky-800 dark:text-sky-400 dark:hover:text-sky-300"
                  title="Restore to To do"
                >
                  Restore
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}

      <div className="flex min-h-0 flex-1 gap-3 overflow-x-auto p-4">
        {TASK_COLUMNS.map((col) => {
          const items = topLevel.filter((t) => t.status === col).sort(byRecent)
          return (
            <div key={col} className="flex w-72 shrink-0 flex-col">
              <div className="mb-2 flex items-center justify-between px-1">
                <span className="text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
                  {STATUS_LABEL[col]}
                </span>
                <span className="text-xs text-[var(--color-muted)]">{items.length}</span>
              </div>
              <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto rounded-lg bg-[var(--color-panel)]/40 p-2">
                {items.map((t) => {
                  const children = childrenByParent.get(t.id) ?? []
                  const isEpic = children.length > 0
                  const done = children.filter((c) => c.status === 'done').length
                  const isOpen = expanded.has(t.id)
                  return (
                    <div
                      key={t.id}
                      className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] transition hover:border-sky-500/40"
                    >
                      <Link to={`tasks/${t.id}`} className="block p-3 text-left">
                        <div className="mb-2 flex items-start gap-2">
                          <PriorityDot priority={t.priority} />
                          {/* min-w-0 + break-words so a super-long / unbroken title wraps inside
                              the fixed-width column instead of forcing a horizontal scrollbar (task 518). */}
                          <span className="min-w-0 flex-1 break-words text-sm leading-snug">
                            {t.title}
                          </span>
                        </div>
                        <div className="flex items-center justify-between text-xs text-[var(--color-muted)]">
                          <span className="flex items-center gap-1.5 font-mono">
                            #{t.id}
                            {/* Liveness-monitor exemption marker (task 520 family / #506 guard). */}
                            {t.monitor_exempt && (
                              <span
                                title="Exempt from the liveness monitor / nudge daemon"
                                className="rounded bg-amber-500/15 px-1 py-0.5 text-[9px] font-medium uppercase tracking-wide text-amber-800 dark:bg-amber-500/20 dark:text-amber-300"
                              >
                                exempt
                              </span>
                            )}
                          </span>
                          {t.assignee && <Identity id={t.assignee} className="font-mono" />}
                        </div>
                      </Link>
                      {/* Epic: a roll-up badge + a toggle to reveal the (one level of) subtasks
                          inline, each linking to its own drawer. */}
                      {isEpic && (
                        <button
                          onClick={() => toggleExpand(t.id)}
                          className="flex w-full items-center gap-1.5 border-t border-[var(--color-border)] px-3 py-1.5 text-left text-xs text-[var(--color-muted)] hover:bg-[var(--color-panel)]/60"
                        >
                          <span>{isOpen ? '▾' : '▸'}</span>
                          <span className="rounded bg-[var(--color-panel)] px-1.5 py-0.5 font-mono text-[10px]">
                            {done}/{children.length}
                          </span>
                          <span>{isOpen ? 'hide subtasks' : 'subtasks'}</span>
                        </button>
                      )}
                      {isEpic && isOpen && (
                        <ul className="space-y-1 border-t border-[var(--color-border)] p-2">
                          {children.map((c) => (
                            <li key={c.id}>
                              <Link
                                to={`tasks/${c.id}`}
                                className="flex items-center gap-2 rounded px-1.5 py-1 hover:bg-[var(--color-panel)]/60"
                              >
                                <StatusChip status={c.status} />
                                <span className="min-w-0 flex-1 truncate text-xs">{c.title}</span>
                                <span className="font-mono text-[10px] text-[var(--color-muted)]">
                                  #{c.id}
                                </span>
                              </Link>
                            </li>
                          ))}
                        </ul>
                      )}
                    </div>
                  )
                })}
                {items.length === 0 && (
                  <div className="px-1 py-2 text-xs text-[var(--color-muted)]/60">—</div>
                )}
              </div>
            </div>
          )
        })}
      </div>

      {/* The task drawer, when the URL is …/tasks/:taskId. It subscribes to its own task
          resource and funnels mutations through touched(), so it just needs the actor. */}
      <Outlet context={{ actor }} />
    </main>
  )
}
