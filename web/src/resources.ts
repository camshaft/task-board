// Typed resource hooks over the reference-counted store. Components declare the data they
// use (useProjects, useTask(id), …) and re-render automatically when it changes; they never
// fetch or branch on event types themselves. Mutations and (later) SSE events funnel through
// `touched(...)` — the ONE place that maps a changed entity to the resource keys to refresh.

import {
  api,
  type Agent,
  type Document,
  type DocumentComment,
  type DocumentSummary,
  type EventRow,
  type Project,
  type Task,
  type TaskSummary,
} from './api'
import { invalidate, invalidateMatching, useResource } from './store'

// Resource keys. Keep these in one place so producers (touched) and consumers (hooks) agree.
const keys = {
  projects: 'projects',
  agents: 'agents',
  events: 'events',
  tasks: (projectId: number) => `tasks:${projectId}`,
  task: (taskId: number) => `task:${taskId}`,
  documents: 'documents',
  document: (documentId: number) => `document:${documentId}`,
  documentComments: (documentId: number) => `documentComments:${documentId}`,
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

export function useDocuments() {
  return useResource<DocumentSummary[]>(keys.documents, () => api.listDocuments())
}

export function useDocument(documentId: number) {
  return useResource<Document>(keys.document(documentId), () => api.getDocument(documentId))
}

export function useDocumentComments(documentId: number) {
  return useResource<DocumentComment[]>(keys.documentComments(documentId), () =>
    api.getDocumentComments(documentId),
  )
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
export function touched(
  opts: { projectId?: number; taskId?: number; documentId?: number; activity?: boolean } = {},
) {
  if (opts.taskId != null) invalidate(keys.task(opts.taskId))
  if (opts.projectId != null) invalidate(keys.tasks(opts.projectId))
  else if (opts.taskId != null) {
    // Task changed but we don't know its project here — refresh all loaded task lists.
    invalidateMatching('tasks:')
  }
  if (opts.documentId != null) {
    // A document's own view + its comments, and the documents list (a new version, status
    // change, or fresh doc all show there).
    invalidate(keys.document(opts.documentId))
    invalidate(keys.documentComments(opts.documentId))
    invalidate(keys.documents)
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

export async function updateProject(id: number, b: Parameters<typeof api.updateProject>[1]) {
  const p = await api.updateProject(id, b)
  // Rename/archive/metadata all change the sidebar and the project's own view.
  touched({ projectId: id, activity: true })
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

export async function moveTask(id: number, b: Parameters<typeof api.moveTask>[1]) {
  const t = await api.moveTask(id, b)
  // A move touches TWO task lists — source and destination. The response only names the
  // destination, so refresh every loaded task list (taskId-only path) plus the destination.
  touched({ taskId: t.id, activity: true })
  touched({ projectId: t.project_id, activity: false })
  return t
}

// Document mutations. Same pattern as the task ones: perform the request, then funnel through
// touched() keyed on the document so its viewer, comment panel, and the documents list all
// refresh — locally and (via the SSE path below) when another client makes the change.

export async function commentDocument(id: number, b: Parameters<typeof api.commentDocument>[1]) {
  const r = await api.commentDocument(id, b)
  touched({ documentId: id, activity: true })
  return r
}

export async function resolveDocumentComment(
  id: number,
  commentId: number,
  b: Parameters<typeof api.resolveComment>[2] = {},
) {
  const r = await api.resolveComment(id, commentId, b)
  touched({ documentId: id, activity: true })
  return r
}

export async function submitDocumentForReview(
  id: number,
  b: Parameters<typeof api.submitDocumentForReview>[1] = {},
) {
  const d = await api.submitDocumentForReview(id, b)
  touched({ documentId: id, activity: true })
  return d
}

export async function requestDocumentChanges(
  id: number,
  b: Parameters<typeof api.requestDocumentChanges>[1] = {},
) {
  const d = await api.requestDocumentChanges(id, b)
  touched({ documentId: id, activity: true })
  return d
}

export async function approveDocument(
  id: number,
  b: Parameters<typeof api.approveDocument>[1] = {},
) {
  const d = await api.approveDocument(id, b)
  touched({ documentId: id, activity: true })
  return d
}

// The compact event the SSE feed pushes (mirrors sse::StreamEvent on the server), plus the
// synthetic resync signal the server sends when a client fell too far behind to replay.
export interface StreamEvent {
  seq?: number
  type: string
  project_id?: number | null
  task_id?: number | null
  document_id?: number | null
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
  // A move leaves one project and joins another, but the compact event carries only the
  // destination project_id — so drop the destination hint and let touched()'s taskId-only
  // path refresh EVERY loaded task list (source board included).
  const projectId = ev.type === 'task.moved' ? undefined : (ev.project_id ?? undefined)
  touched({
    projectId,
    taskId: ev.task_id ?? undefined,
    // document.* events (and attach/detach, which also carry a task_id) refresh the doc views.
    documentId: ev.document_id ?? undefined,
    activity: true,
  })
}
