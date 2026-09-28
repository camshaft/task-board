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
 * single choke point for reactivity: mutation helpers call it now, and the SSE stream will
 * call it per server event later — so the "what does this change affect?" logic lives here,
 * not scattered across components.
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
