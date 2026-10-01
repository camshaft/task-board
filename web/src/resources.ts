// Typed resource hooks over the reference-counted store. Components declare the data they
// use (useProjects, useTask(id), …) and re-render automatically when it changes; they never
// fetch or branch on event types themselves. Mutations and (later) SSE events funnel through
// `touched(...)` — the ONE place that maps a changed entity to the resource keys to refresh.

import {
  api,
  type Agent,
  type Channel,
  type ChannelPost,
  type Document,
  type DocumentComment,
  type DocumentSummary,
  type EventRow,
  type ExternalIdentity,
  type ExternalLink,
  type IdentityAlias,
  type Person,
  type Project,
  type Team,
  type TeamDetail,
  type Review,
  type ReviewTrend,
  type Task,
  type TaskSummary,
} from './api'
import { invalidate, invalidateMatching, useResource } from './store'

// Resource keys. Keep these in one place so producers (touched) and consumers (hooks) agree.
const keys = {
  projects: 'projects',
  agents: 'agents',
  // Keyed by window size so a wide per-agent feed (AgentView) and the compact chrome feed
  // (Layout/Home) don't share one cache entry and clobber each other's limit.
  events: (limit: number) => `events:${limit}`,
  // Per-agent activity: the server-filtered events for one actor. Shares the `events:` prefix
  // so touched()'s activity invalidation refreshes it too.
  agentActivity: (id: string) => `events:actor:${id}`,
  tasks: (projectId: number) => `tasks:${projectId}`,
  task: (taskId: number) => `task:${taskId}`,
  documents: 'documents',
  // Wiki tree, keyed by path prefix ('' = whole tree). Its own prefix so touched() can refresh
  // every loaded subtree on any document change (a path set/clear reshapes the tree).
  wiki: (prefix?: string) => `wiki:${prefix ?? ''}`,
  document: (documentId: number) => `document:${documentId}`,
  documentComments: (documentId: number) => `documentComments:${documentId}`,
  agent: (id: string) => `agent:${id}`,
  // Keyed under the `tasks:` prefix so touched()'s task-list invalidation refreshes it too.
  agentTasks: (id: string) => `tasks:agent:${id}`,
  channels: (member?: string) => (member ? `channels:${member}` : 'channels'),
  channel: (id: number) => `channel:${id}`,
  channelPosts: (id: number) => `channelPosts:${id}`,
  externalIdentities: 'externalIdentities',
  documentExternalLinks: (documentId: number) => `externalLinks:document:${documentId}`,
  identityAliases: 'identityAliases',
  people: 'people',
  teams: 'teams',
  team: (id: string) => `team:${id}`,
  reviews: 'reviews',
  review: (id: number) => `review:${id}`,
  reviewTrend: 'reviewTrend',
}

export function useProjects() {
  return useResource<Project[]>(keys.projects, () => api.listProjects())
}

export function useAgents() {
  return useResource<Agent[]>(keys.agents, () => api.listAgents())
}

export function useAgent(id: string) {
  return useResource<Agent>(keys.agent(id), () => api.getAgent(id))
}

// An agent's assigned tasks across every project (assignee filter, no project_id).
export function useAgentTasks(id: string) {
  return useResource<TaskSummary[]>(keys.agentTasks(id), () => api.listTasks({ assignee: id }))
}

// Channels: public list, or a member's list (incl. private/DMs) when `member` is given.
export function useChannels(member?: string) {
  return useResource<Channel[]>(keys.channels(member), () => api.listChannels(member))
}

export function useChannel(id: number) {
  return useResource<Channel>(keys.channel(id), () => api.getChannel(id))
}

// The latest `limit` posts, returned in chat order (oldest→newest). Fetches newest-first
// (order=desc) so a busy channel shows its RECENT tail, not the oldest N; reversed for display.
// Older history loads on demand via getChannelPosts({ order:'desc', before_seq }) in the view.
export function useChannelPosts(id: number, limit = 100) {
  return useResource<ChannelPost[]>(keys.channelPosts(id), () =>
    api.getChannelPosts(id, { order: 'desc', limit }).then((ps) => ps.reverse()),
  )
}

// Bridged external identities, keyed by id → display name for attribution rendering. Reference
// data that changes rarely; a component that shows an external author looks up its display name
// here (falling back to the bare id when absent).
export function useExternalIdentities() {
  return useResource<ExternalIdentity[]>(keys.externalIdentities, () =>
    api.listExternalIdentities(),
  )
}

// Identity aliases (task 532): the alias -> canonical map. Reference data that changes rarely;
// resolved client-side for DISPLAY only (assignee labels, @-mentions) — stored data is untouched.
export function useIdentityAliases() {
  return useResource<IdentityAlias[]>(keys.identityAliases, () => api.listIdentityAliases())
}

// Multi-operator people/teams (doc_26, task 595). People + teams are reference registries; a team
// detail (useTeam) carries its direct members and the fully-resolved person set.
export function usePeople() {
  return useResource<Person[]>(keys.people, () => api.listPeople())
}

export function useTeams() {
  return useResource<Team[]>(keys.teams, () => api.listTeams())
}

export function useTeam(id: string) {
  return useResource<TeamDetail>(keys.team(id), () => api.getTeam(id))
}

// A document's external links (task 707): bridged URLs attached to the doc (e.g. a chorus page),
// surfaced in the doc view. Server-filtered to this document.
export function useDocumentExternalLinks(documentId: number) {
  return useResource<ExternalLink[]>(keys.documentExternalLinks(documentId), () =>
    api.listExternalLinks({ board_kind: 'document', board_id: documentId }),
  )
}

// A resolver from an external identity id (e.g. "slack:U123") to its display name, falling back
// to the bare id. Used with <AuthorLabel> to render ingested (bridged) authors as the person.
export function useExternalNameResolver(): (id: string) => string {
  const { data } = useExternalIdentities()
  return (id: string) => data?.find((e) => e.id === id)?.display_name ?? id
}

// The in-app deep-link for an activity-feed event → the underlying thing that happened, so the
// operator can click through to details (task 275). Most specific target wins; null when there's
// nothing linkable. Uses the bare `/tasks/:id` redirect route (it resolves the project itself),
// and the `#comment-<id>` anchor for a comment. channel_id/document_id come straight off the
// event row; comment_id lives in `data`.
export function eventHref(e: EventRow): string | null {
  const commentId = typeof e.data?.comment_id === 'number' ? e.data.comment_id : undefined
  if (e.type.startsWith('document.') && e.document_id != null) return `/documents/${e.document_id}`
  if (e.channel_id != null) return `/channels/${e.channel_id}`
  if (e.task_id != null)
    return `/tasks/${e.task_id}${e.type === 'task.commented' && commentId != null ? `#comment-${commentId}` : ''}`
  if (e.document_id != null) return `/documents/${e.document_id}`
  if (e.project_id != null) return `/projects/${e.project_id}`
  return null
}

export function useEvents(limit = 30) {
  // order='desc' → the server returns the LATEST `limit` events, newest-first, so the feed
  // tracks recent activity (an SSE event invalidates this key, and the refetch surfaces the new
  // event at the top). Not the oldest window with a client-side reverse.
  return useResource<EventRow[]>(keys.events(limit), () => api.getEvents(0, limit, undefined, 'desc'))
}

// One agent's own activity, server-filtered by actor so the feed is complete (not truncated to
// a client-side window). Newest first (order='desc'), matching useEvents.
export function useAgentActivity(agentId: string, limit = 100) {
  return useResource<EventRow[]>(keys.agentActivity(agentId), () =>
    api.getEvents(0, limit, agentId, 'desc'),
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

// The documents list with filtering + default-hide (task 725). Keyed per filter under the
// `documents:` prefix so touched() refreshes it on any document change.
// - an explicit status and/or tag filter -> a single server query;
// - showAll -> every (non-archived) document, charters included;
// - default (no filter) -> hide charters UNLESS pending-review: the union of everything that is
//   not a charter with the pending-review charters (the composition v-task-board recommended; the
//   backend gives the exclude_tag / multi-status primitives, the default-hide rule lives here).
export function useDocumentList(opts: { status?: string; tag?: string; showAll?: boolean } = {}) {
  const status = opts.status || ''
  const tag = opts.tag || ''
  const showAll = !!opts.showAll
  const key = `documents:${showAll ? 'all' : 'default'}:${status}:${tag}`
  return useResource<DocumentSummary[]>(key, async () => {
    if (status || tag) {
      return api.listDocuments({ status: status || undefined, tag: tag || undefined })
    }
    if (showAll) {
      return api.listDocuments({})
    }
    const [nonCharter, pendingCharters] = await Promise.all([
      api.listDocuments({ exclude_tag: 'charter' }),
      api.listDocuments({ tag: 'charter', status: 'pending-review' }),
    ])
    const seen = new Set(nonCharter.map((d) => d.id))
    return [...nonCharter, ...pendingCharters.filter((d) => !seen.has(d.id))]
  })
}

export function useWiki(prefix?: string) {
  return useResource<DocumentSummary[]>(keys.wiki(prefix), () => api.listWiki(prefix))
}

export function useDocument(documentId: number) {
  return useResource<Document>(keys.document(documentId), () => api.getDocument(documentId))
}

export function useDocumentComments(documentId: number) {
  return useResource<DocumentComment[]>(keys.documentComments(documentId), () =>
    api.getDocumentComments(documentId),
  )
}

// Reviews (task 377). The list omits each review's log; useReview fetches one with its timeline.
export function useReviews() {
  return useResource<Review[]>(keys.reviews, () => api.listReviews())
}

export function useReview(id: number) {
  return useResource<Review>(keys.review(id), () => api.getReview(id))
}

export function useReviewTrend() {
  return useResource<ReviewTrend>(keys.reviewTrend, () => api.reviewTrend())
}

// Review mutations. Each writes the log + (status/vetted) the lifecycle or gate and can shift the
// trend, so refresh the review, the list, and the trend.
export async function setReviewStatus(id: number, b: Parameters<typeof api.setReviewStatus>[1]) {
  const r = await api.setReviewStatus(id, b)
  invalidate(keys.review(id))
  invalidate(keys.reviews)
  invalidate(keys.reviewTrend)
  return r
}

export async function setReviewVetted(id: number, b: Parameters<typeof api.setReviewVetted>[1]) {
  const r = await api.setReviewVetted(id, b)
  invalidate(keys.review(id))
  invalidate(keys.reviews)
  invalidate(keys.reviewTrend)
  return r
}

export async function appendReviewLog(id: number, b: Parameters<typeof api.appendReviewLog>[1]) {
  const r = await api.appendReviewLog(id, b)
  invalidate(keys.review(id))
  invalidate(keys.reviews)
  invalidate(keys.reviewTrend)
  return r
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
  opts: {
    projectId?: number
    taskId?: number
    documentId?: number
    channelId?: number
    activity?: boolean
  } = {},
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
    invalidateMatching('documents') // the plain list + every filtered documents: list
    invalidateMatching('wiki:') // a path set/clear or new version reshapes the tree
  }
  if (opts.channelId != null) {
    // A channel's posts + its own view, and every loaded channel list (public + per-member).
    invalidate(keys.channelPosts(opts.channelId))
    invalidate(keys.channel(opts.channelId))
    invalidateMatching('channels')
  }
  if (opts.activity !== false) {
    invalidate(keys.projects)
    invalidateMatching('events:') // every windowed events feed
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
  // A subtask also changes its parent's children list / roll-up, so refresh that too.
  touched({ projectId: t.project_id, taskId: b.parent_id, activity: true })
  return t
}

// Reparent a task under an epic (parentId = target id), or clear its parent (parentId = 0)
// back to top-level. Refreshes the task, its board (grouping), and the target parent's view.
export async function reparentTask(id: number, parentId: number, actor?: string) {
  const t = await api.updateTask(id, { parent_id: parentId, actor })
  touched({ taskId: id, projectId: t.project_id, activity: true })
  if (parentId > 0) touched({ taskId: parentId, activity: false })
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

// Operator-questions actions (task_629). Each acts on a question comment and transitions its state,
// so refresh the task the comment lives on (its comments array + the derived blocking fields).
export async function answerQuestion(
  taskId: number,
  commentId: number,
  b: Parameters<typeof api.answerQuestion>[1],
) {
  const r = await api.answerQuestion(commentId, b)
  touched({ taskId, activity: true })
  return r
}

export async function declineQuestion(
  taskId: number,
  commentId: number,
  b: Parameters<typeof api.declineQuestion>[1],
) {
  const r = await api.declineQuestion(commentId, b)
  touched({ taskId, activity: true })
  return r
}

export async function cancelQuestion(
  taskId: number,
  commentId: number,
  b: Parameters<typeof api.cancelQuestion>[1] = {},
) {
  const r = await api.cancelQuestion(commentId, b)
  touched({ taskId, activity: true })
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

// Agent mutations. metadata is MERGED server-side (like tasks), so this can add/overwrite keys
// but not remove one. Refresh the agent's own view and the roster (status/name show there).
export async function updateAgent(id: string, b: Parameters<typeof api.updateAgent>[1]) {
  const a = await api.updateAgent(id, b)
  invalidate(keys.agent(id))
  invalidate(keys.agents)
  return a
}

// Request a graceful stand-down. The response carries the pending-request fields, so refresh
// the agent's own view (to show pending state) and the roster.
export async function requestStandDown(
  id: string,
  b: Parameters<typeof api.requestStandDown>[1] = {},
) {
  const a = await api.requestStandDown(id, b)
  invalidate(keys.agent(id))
  invalidate(keys.agents)
  return a
}

// Document mutations. Same pattern as the task ones: perform the request, then funnel through
// touched() keyed on the document so its viewer, comment panel, and the documents list all
// refresh — locally and (via the SSE path below) when another client makes the change.

// File / rename / clear a document's wiki path. touched() refreshes the doc + the wiki tree(s).
export async function setDocumentPath(id: number, b: Parameters<typeof api.setDocumentPath>[1]) {
  const d = await api.setDocumentPath(id, b)
  touched({ documentId: id, activity: true })
  return d
}

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

// Channel mutations. Same pattern: perform the request, then funnel through touched() keyed on
// the channel so its post pane, its own view, and the channel lists refresh — locally and (via
// the SSE path below) when another client/agent posts.

export async function postToChannel(id: number, b: Parameters<typeof api.postToChannel>[1]) {
  const r = await api.postToChannel(id, b)
  touched({ channelId: id, activity: true })
  return r
}

export async function createChannel(b: Parameters<typeof api.createChannel>[0]) {
  const c = await api.createChannel(b)
  touched({ channelId: c.id, activity: true })
  return c
}

export async function inviteToChannel(id: number, b: Parameters<typeof api.inviteToChannel>[1]) {
  const c = await api.inviteToChannel(id, b)
  touched({ channelId: id, activity: true })
  return c
}

// Resolve-or-create the private DM channel for a pair. A new DM should surface in the channel
// lists, so refresh them (resolving an existing one is a cheap no-op refresh).
export async function openDm(b: Parameters<typeof api.openDm>[0]) {
  const c = await api.openDm(b)
  invalidateMatching('channels')
  return c
}

// A direct message to another agent (the server creates or reuses the private DM channel).
// Refresh the channel lists so a freshly-created DM surfaces (e.g. the agent page's DM link),
// plus the activity feed. The specific DM channel id isn't in the response, so refresh all lists.
export async function sendDirectMessage(b: Parameters<typeof api.sendMessage>[0]) {
  const r = await api.sendMessage(b)
  invalidateMatching('channels')
  touched({ activity: true })
  return r
}

// Multi-operator people/teams mutations (doc_26, task 595). People/teams are not yet on the SSE
// stream, so these invalidate the affected keys directly: a person change refreshes the people
// list (and could change any team's resolved_people, so refresh loaded team details too); a team
// or membership change refreshes the teams list and that team's detail.
export async function createPerson(b: Parameters<typeof api.createPerson>[0]) {
  const p = await api.createPerson(b)
  invalidate(keys.people)
  return p
}

export async function deletePerson(id: string) {
  const r = await api.deletePerson(id)
  invalidate(keys.people)
  invalidateMatching('team:') // a deleted person drops out of every team's membership/resolution
  invalidate(keys.teams)
  return r
}

export async function createTeam(b: Parameters<typeof api.createTeam>[0]) {
  const t = await api.createTeam(b)
  invalidate(keys.teams)
  return t
}

export async function deleteTeam(id: string) {
  const r = await api.deleteTeam(id)
  invalidate(keys.teams)
  invalidateMatching('team:') // the team itself + any parent team that nested it
  return r
}

export async function addTeamMember(
  teamId: string,
  b: Parameters<typeof api.addTeamMember>[1],
) {
  const t = await api.addTeamMember(teamId, b)
  invalidate(keys.team(teamId))
  invalidateMatching('team:') // a nested-team add changes parent teams' resolved_people too
  invalidate(keys.teams)
  return t
}

export async function removeTeamMember(
  teamId: string,
  b: Parameters<typeof api.removeTeamMember>[1],
) {
  const t = await api.removeTeamMember(teamId, b)
  invalidate(keys.team(teamId))
  invalidateMatching('team:')
  invalidate(keys.teams)
  return t
}

// The compact event the SSE feed pushes (mirrors sse::StreamEvent on the server), plus the
// synthetic resync signal the server sends when a client fell too far behind to replay.
export interface StreamEvent {
  seq?: number
  type: string
  project_id?: number | null
  task_id?: number | null
  document_id?: number | null
  channel_id?: number | null
  // review.* events (created / status_changed / log_appended / vetted_changed / opened_for_review
  // / terminal) carry the review they concern; null on non-review events.
  review_id?: number | null
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
    // channel.post / message.direct carry channel_id → refresh that channel's post pane.
    channelId: ev.channel_id ?? undefined,
    activity: true,
  })
  // review.* events carry review_id → refresh that review, the list, and the trend, so a status
  // transition / vetted toggle / new finding by another client shows live (task 409). Mirrors the
  // local review mutation wrappers, which invalidate the same three keys.
  if (ev.review_id != null) {
    invalidate(keys.review(ev.review_id))
    invalidate(keys.reviews)
    invalidate(keys.reviewTrend)
  }
}
