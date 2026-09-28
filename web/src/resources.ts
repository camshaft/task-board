// Typed resource hooks over the reference-counted store. Components declare the data they
// use (useProjects, useTask(id), …) and re-render automatically when it changes; they never
// fetch or branch on event types themselves. Mutations and (later) SSE events funnel through
// `touched(...)` — the ONE place that maps a changed entity to the resource keys to refresh.

import { api, type Agent, type EventRow, type Project, type Task, type TaskSummary } from './api'
import { invalidate, invalidateMatching, useResource } from './store'

// Resource keys. Keep these in one place so producers (touched) and consumers (hooks) agree.
const keys = {
  projects: 'projects',
  agents: 'agents',
  events: 'events',
  tasks: (projectId: number) => `tasks:${projectId}`,
  task: (taskId: number) => `task:${taskId}`,
}

export function useProjects() {
  return useResource<Project[]>(keys.projects, () => api.listProjects())
}

export function useAgents() {
  return useResource<Agent[]>(keys.agents, () => api.listAgents())
}

export function useEvents(limit = 30) {
  return useResource<EventRow[]>(keys.events, () =>
    api.getEvents(0, limit).then((es) => es.reverse()),
  )
}

export function useTasks(projectId: number) {
  return useResource<TaskSummary[]>(keys.tasks(projectId), () =>
    api.listTasks({ project_id: projectId }),
  )
}

export function useTask(taskId: number) {
  return useResource<Task>(keys.task(taskId), () => api.getTask(taskId))
}

/**
 * Announce that some data changed, so every dependent resource refetches. This is the
 * single choke point for reactivity — the ONE place that maps a changed entity to the
 * resource keys to refresh. The mutation wrappers below call it, and the SSE stream will
 * call it per server event later; components never touch it.
 *
 * `activity: true` also refreshes the projects list (task counts) and the activity feed,
 * which nearly every write touches.
 */
export function touched(opts: { projectId?: number; taskId?: number; activity?: boolean } = {}) {
  if (opts.taskId != null) invalidate(keys.task(opts.taskId))
  if (opts.projectId != null) invalidate(keys.tasks(opts.projectId))
  else if (opts.taskId != null) {
    // Task changed but we don't know its project here — refresh all loaded task lists.
    invalidateMatching('tasks:')
  }
  if (opts.activity !== false) {
    invalidate(keys.projects)
    invalidate(keys.events)
  }
}

// Mutations. Components call these instead of `api.*` for writes: each performs the request
// and then funnels through touched(), deriving the affected scope from the response (which
// carries project_id / task id), so components never remember to invalidate anything. api.ts
// stays pure transport; this is the reactive abstraction the UI actually uses.

export async function createProject(b: Parameters<typeof api.createProject>[0]) {
  const p = await api.createProject(b)
  touched({ activity: true }) // new project appears in the sidebar + feed
  return p
}

export async function createTask(b: Parameters<typeof api.createTask>[0]) {
  const t = await api.createTask(b)
  touched({ projectId: t.project_id, activity: true })
  return t
}

export async function updateTask(id: number, b: Parameters<typeof api.updateTask>[1]) {
  const t = await api.updateTask(id, b)
  // Status/assignee moves shift the task between board columns and change counts.
  touched({ taskId: t.id, projectId: t.project_id, activity: true })
  return t
}

export async function commentTask(id: number, b: Parameters<typeof api.commentTask>[1]) {
  const r = await api.commentTask(id, b)
  touched({ taskId: id, activity: true })
  return r
}

// The compact event the SSE feed pushes (mirrors sse::StreamEvent on the server), plus the
// synthetic resync signal the server sends when a client fell too far behind to replay.
export interface StreamEvent {
  seq?: number
  type: string
  project_id?: number | null
  task_id?: number | null
}

/**
 * Apply one server-sent event by funneling it through the SAME touched() choke point the
 * local mutations use — so a change made by any other client/agent refreshes exactly the
 * resources it affects, with no per-component wiring. `resync` means "we couldn't tell you
 * what changed" (buffer overrun / gap too large): drop every cache entry so subscribed
 * components refetch from scratch.
 */
export function applyStreamEvent(ev: StreamEvent) {
  if (ev.type === 'resync') {
    invalidateMatching('') // every key startsWith '' — invalidate all loaded resources
    return
  }
  touched({
    projectId: ev.project_id ?? undefined,
    taskId: ev.task_id ?? undefined,
    activity: true,
  })
}
