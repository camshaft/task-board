// Typed client for the task-board REST API. Request URLs are resolved against the page's
// base URI, which reflects the <base href> the backend injects from X-Forwarded-Prefix —
// so the same build works at the origin root or behind a sub-path proxy (e.g. /board)
// with no build-time configuration. Vite proxies /api to the backend in dev; the backend
// serves this app in prod.

// e.g. baseURI 'https://h/board/' -> '.../board/api'; 'https://h/' -> '.../api'. Trailing
// slash on baseURI matters, so the backend always emits <base href="{prefix}/">.
const API_ROOT = new URL('api', document.baseURI).href

export type TaskStatus = 'todo' | 'in_progress' | 'blocked' | 'done' | 'cancelled'
export type AgentStatus = 'online' | 'busy' | 'away' | 'offline'

export interface Agent {
  id: string
  display_name: string | null
  kind: string | null
  status: AgentStatus
  status_message: string | null
  webhook_url: string | null
  created_at: string
  last_seen: string | null
}

export interface Project {
  id: number
  name: string
  description: string | null
  status: string
  created_by: string | null
  created_at: string
  updated_at: string
  task_counts?: Record<string, number>
  tasks?: TaskSummary[]
}

export interface TaskSummary {
  id: number
  title: string
  status: TaskStatus
  assignee: string | null
  priority: string | null
  project_id?: number
  updated_at?: string
}

export interface Comment {
  id: number
  author: string | null
  body: string
  created_at: string
}

export interface Task {
  id: number
  project_id: number
  title: string
  description: string | null
  status: TaskStatus
  priority: string | null
  assignee: string | null
  created_by: string | null
  metadata: Record<string, unknown>
  created_at: string
  updated_at: string
  comments: Comment[]
  subscribers: string[]
}

export interface EventRow {
  seq: number
  type: string
  actor: string | null
  project_id: number | null
  task_id: number | null
  data: Record<string, unknown>
  created_at: string
}

export interface Meta {
  task_statuses: TaskStatus[]
  project_statuses: string[]
  agent_statuses: AgentStatus[]
}

async function req<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`${API_ROOT}${path}`, {
    method,
    headers: body ? { 'content-type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  })
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`
    try {
      const j = await res.json()
      if (j?.error) msg = j.error
    } catch {
      /* non-JSON body */
    }
    throw new Error(msg)
  }
  return res.json() as Promise<T>
}

export const api = {
  meta: () => req<Meta>('GET', '/meta'),

  listAgents: () => req<Agent[]>('GET', '/agents'),
  registerAgent: (b: {
    agent_id: string
    display_name?: string
    kind?: string
    webhook_url?: string
  }) => req<Agent>('POST', '/agents', b),

  listProjects: (status?: string) =>
    req<Project[]>('GET', `/projects${status ? `?status=${encodeURIComponent(status)}` : ''}`),
  getProject: (id: number) => req<Project>('GET', `/projects/${id}`),
  createProject: (b: { name: string; description?: string; created_by?: string }) =>
    req<Project>('POST', '/projects', b),

  listTasks: (q: { project_id?: number; status?: string; assignee?: string } = {}) => {
    const p = new URLSearchParams()
    if (q.project_id != null) p.set('project_id', String(q.project_id))
    if (q.status) p.set('status', q.status)
    if (q.assignee) p.set('assignee', q.assignee)
    const qs = p.toString()
    return req<TaskSummary[]>('GET', `/tasks${qs ? `?${qs}` : ''}`)
  },
  getTask: (id: number) => req<Task>('GET', `/tasks/${id}`),
  createTask: (b: {
    project_id: number
    title: string
    description?: string
    assignee?: string
    priority?: string
    created_by?: string
    metadata?: Record<string, unknown>
  }) => req<Task>('POST', '/tasks', b),
  updateTask: (
    id: number,
    b: {
      status?: TaskStatus
      assignee?: string
      title?: string
      description?: string
      priority?: string
      actor?: string
      metadata?: Record<string, unknown>
    },
  ) => req<Task>('PATCH', `/tasks/${id}`, b),
  commentTask: (id: number, b: { body: string; author?: string }) =>
    req<{ comment_id: number; task_id: number }>('POST', `/tasks/${id}/comments`, b),

  getEvents: (since_seq = 0, limit = 100) =>
    req<EventRow[]>('GET', `/events?since_seq=${since_seq}&limit=${limit}`),
}
