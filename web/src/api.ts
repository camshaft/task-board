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
  // Set while a graceful stand-down has been requested (a signal, not a status change; the
  // agent observes it in its loop). Cleared automatically once the agent honors it by going
  // offline. Surfaced so the agent page can render the pending request (who / why / when).
  stand_down_requested_at?: string | null
  stand_down_requested_by?: string | null
  stand_down_reason?: string | null
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
  // Derived from metadata.monitor_exempt (default false): the task is exempt from the liveness
  // monitor / nudge daemon (task 520 family / #506 guard).
  monitor_exempt?: boolean
}
// One row of the unified "awaiting you" queue (task_860 + task_873): a flat, discriminated queue of
// everything awaiting a principal's decision. A `task` item is a task blocked_on the principal
// (blocked_on_principal + note) and/or carrying open blocking questions routed to it; a `document`
// item is a doc awaiting the operator's approval (status=operator_review), emitted only when the
// viewer resolves to the operator. Keyed independent of assignee, team-expanded.
export interface AwaitingTaskItem {
  kind: 'task'
  task_id: number
  task_title: string
  project_id: number | null
  status: TaskStatus
  updated_at?: string
  blocked_on_principal: boolean
  blocked_on_note: string | null
  questions: Comment[]
}
export interface AwaitingDocItem {
  kind: 'document'
  document_id: number
  title: string
  // Review status (operator_review for a doc awaiting the operator's approval).
  status: string
  // The doc's current (pending-approval) version number.
  version_no: number
  updated_at?: string
  // Slash-separated wiki path when filed, else null.
  path: string | null
}
export type AwaitingItem = AwaitingTaskItem | AwaitingDocItem

// Operator-questions (task_628 / doc_33). A comment is a plain note, a structured question, or an
// answer replying to one. The backend stores `payload` + `state` + `type` + `reply_to` verbatim and
// returns them inline on every task's comments array, so the UI renders questions without a second
// fetch.
export type CommentType = 'plain' | 'question' | 'answer'
export type QuestionKind =
  | 'yes_no'
  | 'multiple_choice'
  | 'select_all'
  | 'fill_in_the_blank'
  | 'rank_list'
// `open` is the only non-terminal state.
export type QuestionState =
  | 'open'
  | 'answered'
  | 'answered_outside_frame'
  | 'declined'
  | 'cancelled'
  | 'superseded'
export interface QuestionOption {
  id: string
  label: string
}
// One open blocking question, as summarized by a single-task fetch's derived block (doc_33 A7).
// `kind` is null for a CID-keyed question (its element is named by ui.element_schema_cid instead).
export interface BlockingQuestion {
  comment_id: number
  kind: QuestionKind | null
  routed_to: string | null
  blocking: boolean
  prompt: string
}
export interface QuestionPayload {
  kind: QuestionKind
  // Present for multiple_choice / select_all / rank_list.
  options?: QuestionOption[]
  // Principal (person / team / agent) the question is routed to; "operator" is the seeded team.
  routed_to?: string
  blocking?: boolean
  // default + wait_period_seconds apply to non-blocking questions only.
  default?: unknown
  wait_period_seconds?: number | null
  // Inline JSON Schema (schema-driven questions): the source of truth for a usable answer form.
  response_schema?: unknown
  // Pass-through UI descriptor: an opaque element key, its props, and the CAS content id of the
  // reusable element schema (resolved best-effort via GET /api/ipfs/{cid}). Progressive enhancement
  // over the inline response_schema.
  ui?: { element?: string; props?: Record<string, unknown>; element_schema_cid?: string }
}
export interface AnswerPayload {
  // choice | bool | text | ranked (matches the question kind, or `text` for an out-of-frame answer).
  shape?: string
  value?: unknown
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
  external_author_name?: string | null
  origin_ref?: string | null
  // Operator-questions fields (default type 'plain'); payload is a QuestionPayload on a question,
  // an AnswerPayload on an answer.
  type?: CommentType
  state?: QuestionState | string | null
  payload?: QuestionPayload | AnswerPayload | Record<string, unknown>
  reply_to?: number | null
  supersedes?: number | null
  superseded_by?: number | null
}

// An identity alias (task 532): a name that resolves to a canonical identity (e.g. operator ->
// cameron). Display-only resolution on the client — stored assignee/mentions are never rewritten.
export interface IdentityAlias {
  alias: string
  canonical: string
  created_at?: string
  created_by?: string | null
}

// Multi-operator identity model (doc_26, task 542 / task 595). A Person is a first-class human
// identity (a separate registry from agents); a Team is an addressable group whose members are
// people or other teams. Both are keyed by a stable string handle (e.g. "cameron", "operator").
export interface Person {
  id: string
  display_name: string | null
  created_by: string | null
  created_at: string
  // Stored as a JSON string in the row (like Task.metadata); unused by the current UI.
  metadata?: unknown
}

export interface Team {
  id: string
  display_name: string | null
  created_by: string | null
  created_at: string
  metadata?: unknown
}

// A team's direct membership edge: a person id or a nested team id.
export interface TeamMember {
  member_id: string
  member_kind: 'person' | 'team'
}

// GET /teams/{id}: the team row plus its direct members and the fully-resolved person set with
// nested teams expanded (cycle-guarded server-side).
export interface TeamDetail extends Team {
  members: TeamMember[]
  resolved_people: string[]
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

// A link mapping a board entity (document / task / channel / ...) to an entity in a bridged
// external system (e.g. a chorus doc URL on a document). Keyed by (source, external_id); the
// human-facing URL, when present, lives in metadata.url. (task 707)
export interface ExternalLink {
  id: number
  source: string
  external_id: string
  external_parent_id: string | null
  board_kind: string
  board_id: number
  metadata: { url?: string } & Record<string, unknown>
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
  // Derived from metadata.monitor_exempt (default false): exempt from the liveness monitor / nudge
  // daemon (#506 guard). Editable via the metadata merge (set metadata.monitor_exempt).
  monitor_exempt?: boolean
  children?: { id: number; title: string; status: TaskStatus }[]
  child_rollup?: { done: number; total: number }
  // Derived question-block (doc_33 A7 / task_628), present on a single-task fetch. effectively_blocked
  // is the scalar blocked_on OR any open blocking question; blocking_questions lists the open ones
  // (sorted by comment id) and question_blocked_on is the union of their routed_to principals.
  effectively_blocked?: boolean
  question_blocked?: boolean
  question_blocked_on?: string[]
  blocking_questions?: BlockingQuestion[]
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
  // MIME type of this version's bytes (default text/markdown). The board records only the label;
  // the client dispatches a renderer on it and resolves the CID through the IPFS gateway.
  content_type: string | null
  summary: string | null
  created_by: string | null
  created_at: string
}

// The row shape returned by listDocuments (no versions/metadata).
export interface DocumentSummary {
  id: number
  title: string
  slug: string | null
  // Slash-separated wiki path this doc is filed under, or null when unfiled. Drives the wiki tree.
  path: string | null
  project_id: number | null
  status: string
  current_version_id: number | null
  approved_version_id: number | null
  created_by: string | null
  updated_at: string
  // Deprecate/supersede (task 722): a deprecated doc stays visible but carries a banner; when it
  // was superseded, superseded_by points at the replacing document. Both null when not deprecated.
  deprecated_at?: string | null
  superseded_by?: number | null
}

// An outbound wiki link from a document ([[target_path]] / [[target_path|label]] in its content).
// target_* are null when the link dangles — nothing is filed at that path yet (render as a red-link).
export interface OutboundLink {
  target_path: string
  label: string | null
  target_document_id: number | null
  target_title: string | null
  target_status: string | null
}

// A backlink: a document whose content links to THIS document's path ("what links here").
export interface Backlink {
  id: number
  title: string
  path: string | null
  status: string
  label: string | null
}

// An outbound transclusion (![[path]] / ![[path@vN]] / ![[path#region]]) from a document.
// target_version_id set = pinned to that immutable version; null = floats to current. region is
// a raw fragment for a partial embed. target_* are null when the embed dangles.
export interface Embed {
  target_path: string
  label: string | null
  target_version_id: number | null
  region: string | null
  target_document_id: number | null
  target_title: string | null
  target_status: string | null
}

// A document that transcludes THIS document ("what embeds this" — the dependents view).
export interface EmbeddedBy {
  id: number
  title: string
  path: string | null
  status: string
  label: string | null
  region: string | null
}

// A full document (getDocument): metadata + resolved current version + full version list +
// the tasks it's attached to + its wiki link graph (outbound links + backlinks).
export interface Document extends DocumentSummary {
  metadata: Record<string, unknown>
  approved_by: string | null
  created_at: string
  current_version: DocumentVersion | null
  versions: DocumentVersion[]
  attached_tasks: { id: number; title: string; status: TaskStatus; project_id: number | null }[]
  outbound_links: OutboundLink[]
  backlinks: Backlink[]
  embeds: Embed[]
  embedded_by: EmbeddedBy[]
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
  // The event log also carries channel_id / document_id columns (populated for channel/document
  // events); the API returns them, so the activity feed can deep-link to the right target.
  channel_id?: number | null
  document_id?: number | null
  data: Record<string, unknown>
  created_at: string
}

export interface Meta {
  task_statuses: TaskStatus[]
  project_statuses: string[]
  agent_statuses: AgentStatus[]
}

// A review over an artifact (BUILD 6 / task 377). The A2 `status` drives the lifecycle header;
// the append-only `log` (present on getReview, omitted from the list) is the timeline. `source`
// names the artifact kind (board_doc / github_pr / url / …) and `target_ref`
// locates it — rendered per source. `vetted` is the adversarial-review gate.
export type ReviewStatus = 'open' | 'in_review' | 'changes_requested' | 'approved' | 'closed'

export interface ReviewLogEntry {
  id: number
  review_id: number
  // submitted / revised / finding / finding_resolved / comment / state_change /
  // adversarial_review / decision
  entry_type: string
  body: string | null
  author: string | null
  external_id: string | null
  // For an actionable `finding`: the child task tracking the fix.
  task_id: number | null
  created_at: string
}

export interface Review {
  id: number
  kind: string
  source: string | null
  target_ref: string | null
  status: ReviewStatus
  title: string | null
  vetted: boolean
  created_by: string | null
  assignee: string | null
  metadata: Record<string, unknown>
  created_at: string
  updated_at: string
  // Present on getReview (the timeline); omitted from listReviews.
  log?: ReviewLogEntry[]
}

// The review improvement trend (GET /reviews/trend, task 376): findings-per-review with an
// earlier-vs-later trend, counterbalanced by an escaped-defect signal, derived from review logs.
export interface ReviewTrendSlice {
  reviews: number
  findings: number
  findings_per_review: number
  escaped_defects: {
    total: number
    post_approval_findings: number
    reopens: number
    lineage_followups: number
  }
  earlier: { reviews: number; findings_per_review: number; escaped_per_review: number } | null
  later: { reviews: number; findings_per_review: number; escaped_per_review: number } | null
  // improving | worsening | flat | insufficient_data
  findings_trend: string
  // rising | falling | flat | insufficient_data
  escaped_trend: string
  // findings fell while escaped defects rose — a slice worth a second look.
  flagged: boolean
  kind?: string
  area?: string
}

export interface ReviewTrend {
  filters: { kind: string | null; area: string | null }
  overall: ReviewTrendSlice
  by_kind: ReviewTrendSlice[]
  by_area: ReviewTrendSlice[]
}

// A secret request as safe metadata (never the ciphertext or the capability tokens). The board
// is an ephemeral request broker: this drives the submit page, which encrypts the value to
// `recipients` in the browser and posts only ciphertext.
export interface SecretRequest {
  id: number
  name: string
  requested_by: string | null
  fulfiller: string | null
  status: string
  recipients: string[]
  instructions: string | null
  target: string | null
  created_at: string | null
  submitted_at: string | null
  expires_at: string | null
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

// A client-side crash report (task_879). POSTed to /api/crash-reports, where the backend dedups
// by signature (build + top stack frames) and files/bumps an investigation task. All fields but
// `message` are optional so a bare onerror with nothing else still reports.
export interface CrashReport {
  kind: 'error' | 'unhandledrejection'
  message: string
  stack?: string
  component_stack?: string
  url?: string
  build?: string
  user_agent?: string
  occurred_at?: string
}

export const api = {
  meta: () => req<Meta>('GET', '/meta'),

  // Fire-and-forget UI crash telemetry; the backend returns the task it filed/bumped. Callers
  // (the crash reporter) swallow failures so reporting a crash can never itself crash the app.
  createCrashReport: (b: CrashReport) =>
    req<{ task_id: number; created: boolean; occurrences: number }>('POST', '/crash-reports', b),

  // Absolute URL of the SSE activity feed, resolved against the same base as every request
  // so it works at the origin root or behind a sub-path proxy. Consumed by useLiveUpdates.
  streamUrl: () => `${API_ROOT}/stream`,

  // The roster page renders charter + metadata per agent, so it asks for the full objects.
  // (Agents/MCP callers get the compact {id,display_name,status} default to stay under the token cap.)
  listAgents: () => req<Agent[]>('GET', '/agents?verbose=true'),
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
  // Request a graceful stand-down: records the request on the agent + drops an observable
  // notification into its inbox. A signal only — never changes status / kills the agent.
  requestStandDown: (id: string, b: { requested_by?: string; reason?: string } = {}) =>
    req<Agent>('POST', `/agents/${encodeURIComponent(id)}/request-stand-down`, b),

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
  // The unified "awaiting you" queue (task_860): tasks blocked_on `viewer` UNION tasks with an open
  // blocking question routed to it, keyed independent of assignee + team-expanded + deduped. Each row
  // is task-centric with its open questions nested as full comment objects (so they render + answer
  // inline).
  listAwaiting: (viewer: string, q: { project_id?: number; include_archived?: boolean } = {}) => {
    const p = new URLSearchParams({ viewer })
    if (q.project_id != null) p.set('project_id', String(q.project_id))
    if (q.include_archived) p.set('include_archived', 'true')
    return req<AwaitingItem[]>('GET', `/tasks/awaiting?${p.toString()}`)
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

  // Operator-questions actions (task_628 / task_629). Pose is agent-side (not surfaced in the UI
  // yet); the UI drives answer / decline / cancel on a question comment. The backend validates the
  // answer shape against the kind (and the inline response_schema when present) and transitions the
  // question's lifecycle state.
  answerQuestion: (
    commentId: number,
    b: { shape: string; value: unknown; actor?: string },
  ) => req<Comment>('POST', `/comments/${commentId}/answer`, b),
  declineQuestion: (commentId: number, b: { feedback: string; actor?: string }) =>
    req<Comment>('POST', `/comments/${commentId}/decline`, b),
  cancelQuestion: (commentId: number, b: { actor?: string } = {}) =>
    req<Comment>('POST', `/comments/${commentId}/cancel`, b),
  // Re-pose an OPEN question with a new prompt (asker-only; the old is kept immutable + linked).
  supersedeQuestion: (commentId: number, b: { new_prompt: string; actor?: string }) =>
    req<Comment>('POST', `/comments/${commentId}/supersede`, b),

  // order='desc' returns the LATEST `limit` events, newest-first (for an activity feed); the
  // default 'asc' returns oldest-first above `since_seq` (for incremental tailing). Both compose
  // with `since_seq` and `actor`.
  getEvents: (since_seq = 0, limit = 100, actor?: string, order?: 'asc' | 'desc') =>
    req<EventRow[]>(
      'GET',
      `/events?since_seq=${since_seq}&limit=${limit}${actor ? `&actor=${encodeURIComponent(actor)}` : ''}${order ? `&order=${order}` : ''}`,
    ),

  listDocuments: (
    q: {
      project_id?: number
      // A single status, or a comma-separated set (match any). Accepts the operator vocabulary
      // pending-review / published (mapped server-side), per task 724.
      status?: string
      tag?: string
      exclude_tag?: string
      task_id?: number
      author?: string
      include_archived?: boolean
    } = {},
  ) => {
    const p = new URLSearchParams()
    if (q.project_id != null) p.set('project_id', String(q.project_id))
    if (q.status) p.set('status', q.status)
    if (q.tag) p.set('tag', q.tag)
    if (q.exclude_tag) p.set('exclude_tag', q.exclude_tag)
    if (q.task_id != null) p.set('task_id', String(q.task_id))
    if (q.author) p.set('author', q.author)
    if (q.include_archived) p.set('include_archived', 'true')
    const qs = p.toString()
    return req<DocumentSummary[]>('GET', `/documents${qs ? `?${qs}` : ''}`)
  },
  getDocument: (id: number) => req<Document>('GET', `/documents/${id}`),
  // Path-filed documents as a wiki tree (optionally under a path prefix), ordered by path.
  listWiki: (prefix?: string) =>
    req<DocumentSummary[]>(
      'GET',
      `/wiki${prefix ? `?prefix=${encodeURIComponent(prefix)}` : ''}`,
    ),
  // Set (or clear, with an empty string) a document's wiki path. Unique among filed docs.
  setDocumentPath: (id: number, b: { path: string; actor?: string }) =>
    req<Document>('POST', `/documents/${id}/path`, b),
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
  // order='desc' returns the LATEST `limit` posts newest-first (for the initial chat view — render
  // reversed). `before_seq` (with desc) pages backward: the posts immediately older than that seq.
  // Default asc + `since_seq` is the forward/live poll. Bounds compose: seq>since_seq, seq<before_seq.
  getChannelPosts: (
    id: number,
    q: { since_seq?: number; limit?: number; order?: 'asc' | 'desc'; before_seq?: number } = {},
  ) => {
    const p = new URLSearchParams()
    if (q.since_seq != null) p.set('since_seq', String(q.since_seq))
    if (q.limit != null) p.set('limit', String(q.limit))
    if (q.order) p.set('order', q.order)
    if (q.before_seq != null) p.set('before_seq', String(q.before_seq))
    const qs = p.toString()
    return req<ChannelPost[]>('GET', `/channels/${id}/posts${qs ? `?${qs}` : ''}`)
  },
  postToChannel: (id: number, b: { sender: string; body: string; reply_to?: number }) =>
    req<{ seq: number }>('POST', `/channels/${id}/posts`, b),
  inviteToChannel: (id: number, b: { agent_id: string; invited_by?: string }) =>
    req<Channel>('POST', `/channels/${id}/invites`, b),
  sendMessage: (b: { from_agent: string; to_agent: string; body: string }) =>
    req<{ seq: number }>('POST', '/messages', b),
  // Resolve-or-create the private 1:1 DM channel for a pair (idempotent, order-independent).
  // Resolving is silent — no event fires until an actual message is posted.
  openDm: (b: { agent_a: string; agent_b: string }) => req<Channel>('POST', '/dms', b),

  // Reviews (task 377). listReviews omits each review's log; getReview includes the full log.
  listReviews: (q: { status?: string; kind?: string; assignee?: string } = {}) => {
    const p = new URLSearchParams()
    if (q.status) p.set('status', q.status)
    if (q.kind) p.set('kind', q.kind)
    if (q.assignee) p.set('assignee', q.assignee)
    const qs = p.toString()
    return req<{ reviews: Review[] }>('GET', `/reviews${qs ? `?${qs}` : ''}`).then((r) => r.reviews)
  },
  getReview: (id: number) => req<Review>('GET', `/reviews/${id}`),
  reviewTrend: (q: { kind?: string; area?: string } = {}) => {
    const p = new URLSearchParams()
    if (q.kind) p.set('kind', q.kind)
    if (q.area) p.set('area', q.area)
    const qs = p.toString()
    return req<ReviewTrend>('GET', `/reviews/trend${qs ? `?${qs}` : ''}`)
  },
  setReviewStatus: (id: number, b: { status: ReviewStatus; actor?: string; note?: string }) =>
    req<Review>('POST', `/reviews/${id}/status`, b),
  // The adversarial-review gate (task 428). Server records the actor + logs a `decision` entry and
  // emits review.vetted_changed; same-value is an idempotent no-op. Returns the updated review.
  setReviewVetted: (id: number, b: { vetted: boolean; actor?: string; note?: string }) =>
    req<Review>('POST', `/reviews/${id}/vetted`, b),
  appendReviewLog: (
    id: number,
    b: { entry_type: string; body?: string; author?: string; task_id?: number; external_id?: string },
  ) =>
    req<{ review_id: number; entry_id: number; appended: boolean; entry_type: string }>(
      'POST',
      `/reviews/${id}/log`,
      b,
    ),

  listExternalIdentities: (source?: string) =>
    req<ExternalIdentity[]>(
      'GET',
      `/external-identities${source ? `?source=${encodeURIComponent(source)}` : ''}`,
    ),

  // External links bridging a board entity to an external system (task 707). Filter by any of
  // source / board_kind / board_id; the doc view uses board_kind=document&board_id=<id>.
  listExternalLinks: (q: { source?: string; board_kind?: string; board_id?: number } = {}) => {
    const p = new URLSearchParams()
    if (q.source) p.set('source', q.source)
    if (q.board_kind) p.set('board_kind', q.board_kind)
    if (q.board_id != null) p.set('board_id', String(q.board_id))
    const qs = p.toString()
    return req<ExternalLink[]>('GET', `/external-links${qs ? `?${qs}` : ''}`)
  },

  // Identity aliases (task 532): the alias -> canonical map, resolved client-side for display.
  listIdentityAliases: () => req<IdentityAlias[]>('GET', '/identity-aliases'),

  // Multi-operator people/teams (doc_26, task 542 Phase 1 backend / task 595 UI). People and teams
  // are upserted by stable string id; a team member is a person or a nested team. The server
  // cycle-guards nested-team resolution and rejects a sub-team add that would create a cycle.
  listPeople: () => req<Person[]>('GET', '/people'),
  createPerson: (b: { id: string; display_name?: string; created_by?: string }) =>
    req<Person>('POST', '/people', b),
  deletePerson: (id: string) =>
    req<{ deleted: string }>('DELETE', `/people/${encodeURIComponent(id)}`),
  listTeams: () => req<Team[]>('GET', '/teams'),
  createTeam: (b: { id: string; display_name?: string; created_by?: string }) =>
    req<Team>('POST', '/teams', b),
  getTeam: (id: string) => req<TeamDetail>('GET', `/teams/${encodeURIComponent(id)}`),
  deleteTeam: (id: string) =>
    req<{ deleted: string }>('DELETE', `/teams/${encodeURIComponent(id)}`),
  addTeamMember: (teamId: string, b: { member_id: string; member_kind: 'person' | 'team'; created_by?: string }) =>
    req<TeamDetail>('POST', `/teams/${encodeURIComponent(teamId)}/members`, b),
  removeTeamMember: (teamId: string, b: { member_id: string; member_kind: 'person' | 'team' }) =>
    req<TeamDetail>('DELETE', `/teams/${encodeURIComponent(teamId)}/members`, b),

  getSecretRequest: (id: number) => req<SecretRequest>('GET', `/secret-requests/${id}`),
  submitSecret: (id: number, b: { token: string; ciphertext: string }) =>
    req<SecretRequest>('POST', `/secret-requests/${id}/submit`, b),
}

// Resolve a bare content id to a same-origin URL served by the board's scoped read-through
// gateway (GET /api/ipfs/:cid — reads the bytes from the board's IPFS backend and streams them
// back; 503 when no backend is configured). Pass the version's content_type so the response is
// labeled for the browser/renderer (the board doesn't sniff bytes). Resolved against the page
// base so it's correct at the origin root or behind a sub-path proxy.
export function ipfsUrl(cid: string, contentType?: string | null): string {
  const u = new URL(`api/ipfs/${encodeURIComponent(cid)}`, document.baseURI)
  if (contentType) u.searchParams.set('content_type', contentType)
  return u.href
}
