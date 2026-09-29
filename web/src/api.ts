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
  charter: string | null
  metadata: Record<string, unknown>
  webhook_url: string | null
  created_at: string
  last_seen: string | null
}

export interface Project {
  id: number
  name: string
  description: string | null
  status: string
  metadata: Record<string, unknown>
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
  parent_id?: number | null
  updated_at?: string
}

export interface Comment {
  id: number
  author: string | null
  body: string
  created_at: string
  // When set, an external (bridged) identity id this comment is attributed to (e.g.
  // "slack:U123"); `author` is then the fleet agent that ingested it. origin_ref is the
  // source item's origin id for imported/synced comments.
  external_author?: string | null
  origin_ref?: string | null
}

// A bridged human/actor (from listExternalIdentities), distinct from a fleet Agent.
export interface ExternalIdentity {
  id: string
  source: string
  display_name: string | null
  metadata: Record<string, unknown>
  created_at: string
  updated_at: string
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
  parent_id?: number | null
  parent_title?: string | null
  children?: { id: number; title: string; status: TaskStatus }[]
  child_rollup?: { done: number; total: number }
  attached_documents?: {
    id: number
    title: string
    status: string
    slug: string | null
    project_id: number | null
  }[]
}

export interface DocumentVersion {
  id: number
  document_id: number
  version_no: number
  cid: string
  summary: string | null
  created_by: string | null
  created_at: string
}

// The row shape returned by listDocuments (no versions/metadata).
export interface DocumentSummary {
  id: number
  title: string
  slug: string | null
  project_id: number | null
  status: string
  current_version_id: number | null
  approved_version_id: number | null
  created_by: string | null
  updated_at: string
}

// A full document (getDocument): metadata + resolved current version + full version list +
// the tasks it's attached to.
export interface Document extends DocumentSummary {
  metadata: Record<string, unknown>
  approved_by: string | null
  created_at: string
  current_version: DocumentVersion | null
  versions: DocumentVersion[]
  attached_tasks: { id: number; title: string; status: TaskStatus; project_id: number | null }[]
}

export interface DocumentComment {
  id: number
  document_id: number
  version_id: number | null
  author: string | null
  body: string
  region: unknown
  status: string
  reply_to: number | null
  created_at: string
  // Bridged human this comment is attributed to (e.g. "slack:U123"); author is then the ingester.
  external_author?: string | null
}

export interface Channel {
  id: number
  name: string | null
  topic: string | null
  status: string
  private: boolean
  dm_key: string | null
  metadata: Record<string, unknown>
  members?: string[]
  member_count?: number
  created_at: string
}

// A post from get_channel_posts: a channel.post (named channel) or message.direct (DM) event.
// The author is data.from (falls back to the event actor); reply_to threads one level.
export interface ChannelPost {
  seq: number
  type: string
  actor: string | null
  channel_id: number
  data: { from?: string; body?: string; reply_to?: number; external_author?: string }
  created_at: string
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

  // Absolute URL of the SSE activity feed, resolved against the same base as every request
  // so it works at the origin root or behind a sub-path proxy. Consumed by useLiveUpdates.
  streamUrl: () => `${API_ROOT}/stream`,

  listAgents: () => req<Agent[]>('GET', '/agents'),
  getAgent: (id: string) => req<Agent>('GET', `/agents/${encodeURIComponent(id)}`),
  registerAgent: (b: {
    agent_id: string
    display_name?: string
    kind?: string
    charter?: string
    metadata?: Record<string, unknown>
    webhook_url?: string
  }) => req<Agent>('POST', '/agents', b),
  updateAgent: (
    id: string,
    b: {
      display_name?: string
      kind?: string
      charter?: string
      status?: string
      status_message?: string
      webhook_url?: string
      metadata?: Record<string, unknown>
    },
  ) => req<Agent>('PATCH', `/agents/${encodeURIComponent(id)}`, b),

  listProjects: (status?: string) =>
    req<Project[]>('GET', `/projects${status ? `?status=${encodeURIComponent(status)}` : ''}`),
  getProject: (id: number) => req<Project>('GET', `/projects/${id}`),
  createProject: (b: {
    name: string
    description?: string
    created_by?: string
    metadata?: Record<string, unknown>
  }) => req<Project>('POST', '/projects', b),
  updateProject: (
    id: number,
    b: {
      name?: string
      description?: string
      status?: string
      metadata?: Record<string, unknown>
      actor?: string
    },
  ) => req<Project>('PATCH', `/projects/${id}`, b),

  listTasks: (
    q: {
      project_id?: number
      status?: string
      assignee?: string
      unassigned?: boolean
      parent_id?: number
      top_level?: boolean
      q?: string
    } = {},
  ) => {
    const p = new URLSearchParams()
    if (q.project_id != null) p.set('project_id', String(q.project_id))
    if (q.status) p.set('status', q.status)
    if (q.assignee) p.set('assignee', q.assignee)
    if (q.unassigned) p.set('unassigned', 'true')
    if (q.parent_id != null) p.set('parent_id', String(q.parent_id))
    if (q.top_level) p.set('top_level', 'true')
    if (q.q) p.set('q', q.q)
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
    parent_id?: number
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
      // Reparent: a task id nests under that epic, 0 clears the parent (back to top-level),
      // omitted leaves it unchanged. Same-project / self / cycle guards are enforced server-side.
      parent_id?: number
      metadata?: Record<string, unknown>
    },
  ) => req<Task>('PATCH', `/tasks/${id}`, b),
  commentTask: (id: number, b: { body: string; author?: string }) =>
    req<{ comment_id: number; task_id: number }>('POST', `/tasks/${id}/comments`, b),
  moveTask: (id: number, b: { to_project_id: number; actor?: string }) =>
    req<Task>('POST', `/tasks/${id}/move`, b),

  getEvents: (since_seq = 0, limit = 100, actor?: string) =>
    req<EventRow[]>(
      'GET',
      `/events?since_seq=${since_seq}&limit=${limit}${actor ? `&actor=${encodeURIComponent(actor)}` : ''}`,
    ),

  listDocuments: (
    q: { project_id?: number; status?: string; tag?: string; task_id?: number; author?: string } = {},
  ) => {
    const p = new URLSearchParams()
    if (q.project_id != null) p.set('project_id', String(q.project_id))
    if (q.status) p.set('status', q.status)
    if (q.tag) p.set('tag', q.tag)
    if (q.task_id != null) p.set('task_id', String(q.task_id))
    if (q.author) p.set('author', q.author)
    const qs = p.toString()
    return req<DocumentSummary[]>('GET', `/documents${qs ? `?${qs}` : ''}`)
  },
  getDocument: (id: number) => req<Document>('GET', `/documents/${id}`),
  getDocumentVersions: (id: number) =>
    req<DocumentVersion[]>('GET', `/documents/${id}/versions`),

  getDocumentComments: (id: number, q: { version_id?: number; status?: string } = {}) => {
    const p = new URLSearchParams()
    if (q.version_id != null) p.set('version_id', String(q.version_id))
    if (q.status) p.set('status', q.status)
    const qs = p.toString()
    return req<DocumentComment[]>('GET', `/documents/${id}/comments${qs ? `?${qs}` : ''}`)
  },
  commentDocument: (
    id: number,
    b: { body: string; version_id?: number; author?: string; region?: unknown; reply_to?: number },
  ) => req<DocumentComment>('POST', `/documents/${id}/comments`, b),
  resolveComment: (id: number, commentId: number, b: { actor?: string } = {}) =>
    req<DocumentComment>('POST', `/documents/${id}/comments/${commentId}/resolve`, b),
  submitDocumentForReview: (id: number, b: { actor?: string } = {}) =>
    req<Document>('POST', `/documents/${id}/submit-review`, b),
  requestDocumentChanges: (id: number, b: { actor?: string; note?: string } = {}) =>
    req<Document>('POST', `/documents/${id}/request-changes`, b),
  approveDocument: (id: number, b: { actor?: string } = {}) =>
    req<Document>('POST', `/documents/${id}/approve`, b),

  // Channels + DMs. `member` returns that agent's channels (incl. private/DMs); omitted lists
  // public channels only.
  listChannels: (member?: string) =>
    req<Channel[]>(
      'GET',
      `/channels${member ? `?member=${encodeURIComponent(member)}` : ''}`,
    ),
  getChannel: (id: number) => req<Channel>('GET', `/channels/${id}`),
  createChannel: (b: {
    name: string
    topic?: string
    created_by?: string
    metadata?: Record<string, unknown>
  }) => req<Channel>('POST', '/channels', b),
  getChannelPosts: (id: number, q: { since_seq?: number; limit?: number } = {}) => {
    const p = new URLSearchParams()
    if (q.since_seq != null) p.set('since_seq', String(q.since_seq))
    if (q.limit != null) p.set('limit', String(q.limit))
    const qs = p.toString()
    return req<ChannelPost[]>('GET', `/channels/${id}/posts${qs ? `?${qs}` : ''}`)
  },
  postToChannel: (id: number, b: { sender: string; body: string; reply_to?: number }) =>
    req<{ seq: number }>('POST', `/channels/${id}/posts`, b),
  inviteToChannel: (id: number, b: { agent_id: string; invited_by?: string }) =>
    req<Channel>('POST', `/channels/${id}/invites`, b),
  sendMessage: (b: { from_agent: string; to_agent: string; body: string }) =>
    req<{ seq: number }>('POST', '/messages', b),

  listExternalIdentities: (source?: string) =>
    req<ExternalIdentity[]>(
      'GET',
      `/external-identities${source ? `?source=${encodeURIComponent(source)}` : ''}`,
    ),
}

// Resolve a bare content id to a viewable URL via the deployment's IPFS gateway. The board
// stores only the CID (location-independent); composing the path is the CLIENT's job. Resolved
// against the page base so it's correct at the origin root or behind a sub-path proxy.
export function ipfsUrl(cid: string): string {
  return new URL(`ipfs/${cid}`, document.baseURI).href
}
