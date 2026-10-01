//! REST API for humans (and the web UI), in addition to the MCP surface. Same core
//! operations, exposed as JSON over HTTP. Auth is deliberately absent for now (LAN,
//! trust-on-first-use) but the router is structured so a middleware layer can be added
//! cleanly later.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use schemars::{schema_for, JsonSchema};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core;
use crate::db::Pool;
use crate::ipfs;
use crate::sse::{self, StreamEvent};
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct AppState {
    pub pool: Pool,
    /// Live activity bus: the SSE tailer publishes here, `GET /api/stream` subscribes.
    pub events_tx: broadcast::Sender<StreamEvent>,
    /// Optional IPFS HTTP API for server-side content-addressing of raw document `content`.
    /// `None` keeps the board CID-only. See `crate::ipfs` and `config::Settings::ipfs_api_url`.
    pub ipfs_api_url: Option<String>,
}

/// Map an anyhow error to a JSON HTTP response. "no project/task ..." -> 404/400.
struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = self.0.to_string();
        let code = if msg.starts_with("no project")
            || msg.starts_with("no task")
            || msg.starts_with("no agent")
            || msg.starts_with("no document")
            || msg.starts_with("no comment")
            || msg.starts_with("no parent task")
            || msg.starts_with("no secret request")
            || msg.starts_with("no review")
            || msg == "not found"
        {
            StatusCode::NOT_FOUND
        } else if msg.starts_with("invalid submit token")
            || msg.starts_with("invalid fulfiller token")
        {
            // A capability token that doesn't match — not authorized for this action.
            StatusCode::FORBIDDEN
        } else if msg.starts_with("give ")
            || msg.starts_with("cannot move task")
            || msg.starts_with("a task cannot be its own parent")
            || msg.starts_with("reparenting would create a cycle")
            || msg.contains("is in a different project")
            || msg.starts_with("banned phrase")
            || msg.starts_with("non-ASCII")
            || msg.starts_with("ambiguous bare reference")
            || msg.starts_with("submit link already used")
            || msg.contains("is not awaiting submission")
            || msg.contains("has no ciphertext to pull")
            || msg.starts_with("unknown review status")
            || msg.starts_with("unknown review log type")
        {
            // Client-input validation errors (bad request), not server faults.
            StatusCode::BAD_REQUEST
        } else if msg.starts_with("no IPFS backend") {
            // The board has no IPFS backend configured, so server-side content-addressing
            // is unavailable — the deployment hasn't enabled it (set ipfs_api_url).
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (code, Json(json!({ "error": msg }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(e)
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

/// 404 for a null (not-found) core result, else 200 with the JSON body.
fn found(v: Value) -> ApiResult {
    if v.is_null() {
        Err(ApiError(anyhow::anyhow!("not found")))
    } else {
        Ok(Json(v))
    }
}

/// Parse a resource id from a URL path segment (task 504), accepting three interchangeable forms:
/// the bare integer (`472`), the `#472` shorthand, or the typed canonical form `<kind>_472` (e.g.
/// `task_472`). `kind` is the route's own resource prefix ("task" | "doc" | "project" | "channel").
/// A typed form whose prefix does NOT match the route (e.g. `doc_5` on a task route) is rejected so
/// an id can't cross resource types. Returns None on anything else (non-numeric, id <= 0, ...).
fn parse_ref(kind: &str, seg: &str) -> Option<i64> {
    let s = seg.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    // Typed form: exactly "<kind>_<digits>". A wrong-kind prefix falls through and fails to parse.
    if let Some(rest) = s.strip_prefix(kind).and_then(|r| r.strip_prefix('_')) {
        return rest.parse::<i64>().ok().filter(|n| *n > 0);
    }
    s.parse::<i64>().ok().filter(|n| *n > 0)
}

/// Generate a `Path`-extractable newtype that accepts a resource id in bare / `#N` / typed
/// (`<kind>_N`) form via [`parse_ref`], deserializing to the plain `i64`. Handlers destructure it
/// (`Path(TaskRef(task_id))`) so their bodies still see an `i64` and need no other change (task 504).
macro_rules! path_ref {
    ($name:ident, $kind:literal) => {
        #[derive(Debug, Clone, Copy)]
        struct $name(i64);
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                parse_ref($kind, &s)
                    .map($name)
                    .ok_or_else(|| serde::de::Error::custom(format!("invalid {} id: {s}", $kind)))
            }
        }
    };
}
path_ref!(TaskRef, "task");
path_ref!(ProjectRef, "project");
path_ref!(ChannelRef, "channel");
path_ref!(DocRef, "doc");

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/tunnels", get(tunnels))
        .route("/meta", get(meta))
        .route("/agents", get(list_agents).post(register_agent))
        .route("/agents/{agent_id}", get(get_agent).patch(update_agent))
        .route("/agents/{agent_id}/status", post(set_status))
        .route(
            "/agents/{agent_id}/request-stand-down",
            post(request_stand_down),
        )
        .route("/agents/{agent_id}/notifications", get(get_notifications))
        .route("/agents/{agent_id}/messages", get(get_messages))
        .route("/projects", get(list_projects).post(create_project))
        .route(
            "/projects/{project_id}",
            get(get_project).patch(update_project),
        )
        .route("/tasks", get(list_tasks).post(create_task))
        .route("/tasks/{task_id}", get(get_task).patch(update_task))
        .route("/tasks/{task_id}/comments", post(comment_task))
        .route("/comments/{comment_id}", get(get_comment))
        .route("/tasks/{task_id}/questions", post(pose_question))
        .route("/comments/{comment_id}/answer", post(answer_question))
        .route("/comments/{comment_id}/decline", post(decline_question))
        .route("/comments/{comment_id}/cancel", post(cancel_question))
        .route("/comments/{comment_id}/supersede", post(supersede_question))
        .route("/tasks/blocking-me", get(list_tasks_blocking_me))
        .route("/tasks/{task_id}/props", patch(set_task_props))
        .route("/tasks/{task_id}/move", post(move_task))
        .route("/tasks/{task_id}/archive", post(archive_task))
        .route("/tasks/{task_id}/restore", post(restore_task))
        .route("/tasks/{task_id}/mute", post(mute_task))
        .route("/tasks/{task_id}/unmute", post(unmute_task))
        .route("/subscriptions", post(subscribe).delete(unsubscribe))
        .route("/channels", get(list_channels).post(create_channel))
        .route("/channels/{channel_id}", get(get_channel))
        .route(
            "/channels/{channel_id}/posts",
            get(get_channel_posts).post(post_to_channel),
        )
        .route("/channels/{channel_id}/props", patch(set_channel_props))
        .route(
            "/channels/{channel_id}/auto-join",
            post(set_channel_auto_join),
        )
        .route(
            "/channels/{channel_id}/promote-thread",
            post(promote_thread),
        )
        .route("/channels/{channel_id}/invites", post(invite_to_channel))
        .route("/messages", post(send_message))
        .route("/dms", post(open_dm))
        .route("/events", get(get_events))
        .route(
            "/external-identities",
            get(list_external_identities).post(upsert_external_identity),
        )
        .route(
            "/external-links",
            get(list_external_links).post(upsert_external_link),
        )
        .route(
            "/workspace-kinds",
            get(list_workspace_kinds).post(set_workspace_kind),
        )
        .route(
            "/workspace-kinds/{name}",
            get(get_workspace_kind).delete(delete_workspace_kind),
        )
        .route("/lint", post(lint_text))
        .route(
            "/banned-phrases",
            get(list_banned_phrases).post(add_banned_phrase),
        )
        .route(
            "/banned-phrases/{phrase}",
            axum::routing::delete(remove_banned_phrase),
        )
        .route(
            "/identity-aliases",
            get(list_identity_aliases).post(set_identity_alias),
        )
        .route("/people", get(list_people).post(create_person))
        .route("/people/{id}", delete(delete_person))
        .route("/teams", get(list_teams).post(create_team))
        .route("/teams/{team_id}", get(get_team).delete(delete_team))
        .route(
            "/teams/{team_id}/members",
            post(add_team_member).delete(remove_team_member),
        )
        .route(
            "/secret-requests",
            get(list_secret_requests).post(create_secret_request),
        )
        .route("/secret-requests/{id}", get(get_secret_request))
        .route("/secret-requests/{id}/submit", post(submit_secret))
        .route(
            "/secret-requests/{id}/ciphertext",
            get(get_secret_ciphertext),
        )
        .route("/secret-requests/{id}/fulfill", post(fulfill_secret))
        .route("/secret-requests/{id}/cancel", post(cancel_secret_request))
        .route("/reviews", get(list_reviews).post(create_review))
        .route("/reviews/trend", get(review_trend))
        .route("/reviews/{review_id}", get(get_review))
        .route("/reviews/{review_id}/status", post(set_review_status))
        .route("/reviews/{review_id}/vetted", post(set_review_vetted))
        .route("/reviews/{review_id}/log", post(append_review_log))
        .route("/ipfs/add", post(ipfs_add))
        .route("/ipfs/{cid}", get(ipfs_cat))
        .route("/wiki", get(list_wiki))
        .route("/documents", get(list_documents).post(create_document))
        .route(
            "/documents/{document_id}",
            get(get_document).patch(update_document),
        )
        .route(
            "/documents/{document_id}/content",
            get(read_document_content),
        )
        .route("/documents/{document_id}/path", post(set_document_path))
        .route(
            "/documents/{document_id}/versions",
            get(get_document_versions).post(publish_version),
        )
        .route(
            "/documents/{document_id}/comments",
            get(get_document_comments).post(comment_document),
        )
        .route(
            "/documents/{document_id}/comments/{comment_id}/resolve",
            post(resolve_comment),
        )
        .route(
            "/documents/{document_id}/submit-review",
            post(submit_for_review),
        )
        .route(
            "/documents/{document_id}/submit-to-operator-review",
            post(submit_to_operator_review),
        )
        .route(
            "/documents/{document_id}/request-changes",
            post(request_changes),
        )
        .route("/documents/{document_id}/approve", post(approve_document))
        .route("/documents/{document_id}/attach", post(attach_document))
        .route("/documents/{document_id}/detach", post(detach_document))
        .route("/documents/{document_id}/archive", post(archive_document))
        .route("/documents/{document_id}/restore", post(restore_document))
        .route("/stream", get(stream))
        // Unknown /api/* paths return a JSON 404, not the SPA's index.html.
        .fallback(api_not_found)
        .with_state(state)
}

async fn api_not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
}

/// Health beacon: a cheap liveness+readiness probe agents check before a full tick, instead of
/// discovering an outage by burning a heavier call. A `200 {"ok":true,"db":true}` means the
/// board process is up AND its database is reachable. A `503 {"ok":false,"db":false}` means the
/// process is up but the database is not ready. When the origin itself is down (e.g. mid-redeploy)
/// the request never reaches here and the proxy returns 502 — so a caller should treat ANY
/// non-200 (502 or 503) as "board not ready: back off and retry", and a 200 as "safe to proceed".
async fn health(State(st): State<AppState>) -> Response {
    match sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&st.pool)
        .await
    {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "db": true, "commit": BUILD_COMMIT })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({ "ok": false, "db": false, "commit": BUILD_COMMIT, "error": e.to_string() }),
            ),
        )
            .into_response(),
    }
}

/// The git rev this binary was built from, baked in by the nix build (package.nix sets
/// `TASK_BOARD_COMMIT` = the flake rev). `"unknown"` for a non-nix build (e.g. `cargo test`).
/// Reported at `/api/health` so an agent that just merged + auto-deployed can poll and confirm its
/// own commit is the live one, instead of blind-polling the new behavior until it stops 404-ing.
const BUILD_COMMIT: &str = match option_env!("TASK_BOARD_COMMIT") {
    Some(c) => c,
    None => "unknown",
};

/// Diagnostic: which agents currently have a live reverse tunnel (so the board can push a wake
/// instead of the agent polling). Used to bisect a lost-wake regression — an agent absent here
/// has no live tunnel, so its wakes fall back to the durable inbox + poll, and the break is in
/// the daemon/proxy layer rather than the board's emit path.
async fn tunnels() -> Json<Value> {
    Json(json!({ "tunnels": crate::tunnel::live_agents() }))
}

/// The blessed status vocabularies the UI renders (columns, presence dots, ...).
async fn meta() -> Json<Value> {
    Json(json!({
        "task_statuses": crate::config::TASK_STATUSES,
        "project_statuses": crate::config::PROJECT_STATUSES,
        "agent_statuses": crate::config::AGENT_STATUSES,
    }))
}

// --- Discovery ---

/// One row in the endpoint catalog: HTTP method, path template, a one-line summary, and
/// (for calls that take a JSON body) the name of the schema in the `schemas` map.
struct Endpoint {
    method: &'static str,
    path: &'static str,
    summary: &'static str,
    /// Query-string params, for GET endpoints that take them.
    query: &'static str,
    /// Key into the `schemas` object for the request-body JSON Schema, if any.
    body: Option<&'static str>,
}

/// The hand-curated map of every REST endpoint. Kept next to `router()` so the two stay
/// in sync. `body` names a struct whose JSON Schema is generated below.
const ENDPOINTS: &[Endpoint] = &[
    Endpoint { method: "GET", path: "/api", summary: "This discovery index: every endpoint with its request schema.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/tunnels", summary: "Diagnostic: which agents currently have a live reverse tunnel (so the board can push a wake rather than the agent polling). An agent absent here has no live tunnel — its wakes fall back to the inbox + poll.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/health", summary: "Health beacon: cheap liveness+readiness probe. 200 {ok:true,db:true} when the process is up and the database is reachable; 503 {ok:false} when the database is not ready. Check before a full tick and treat any non-200 (incl a 502 from the origin when it is down) as back-off-and-retry.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/meta", summary: "Status vocabularies (task/project/agent).", query: "", body: None },
    Endpoint { method: "GET", path: "/api/agents", summary: "List agents as a lightweight roster: compact {id, display_name, status, metadata} by default (the small metadata bag is kept for filtering, e.g. metadata.native; only the heavy charter is dropped to stay under the token cap). Pass verbose=true for full objects (incl charter), or GET /api/agents/{id} for one. Filters: status, q (id+display_name substring), meta_key+meta_value (scalar metadata match). Bounded by limit (default 200, max 1000) + offset.", query: "status=str&q=str&meta_key=str&meta_value=str&verbose=bool&limit=int&offset=int", body: None },
    Endpoint { method: "POST", path: "/api/agents", summary: "Register (or update) an agent, trust-on-first-use.", query: "", body: Some("RegisterAgentBody") },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}", summary: "Fetch a single agent (including its charter + metadata).", query: "", body: None },
    Endpoint { method: "PATCH", path: "/api/agents/{agent_id}", summary: "Update an agent's fields + metadata (the board agent list as a registry). Merge-PATCH: an omitted/null field is left unchanged; to reset a nullable field to null, name it in `clear` (e.g. [\"webhook_url\"]).", query: "", body: Some("UpdateAgentBody") },
    Endpoint { method: "POST", path: "/api/agents/{agent_id}/status", summary: "Set an agent's presence status.", query: "", body: Some("SetStatusBody") },
    Endpoint { method: "POST", path: "/api/agents/{agent_id}/request-stand-down", summary: "Request that an agent gracefully wind down: records the request (who/why/when, shown on the agent's page) and notifies the agent so it stands down on its own terms. A SIGNAL — never changes the agent's status and never kills a live agent. Cleared when the agent goes offline.", query: "", body: Some("RequestStandDownBody") },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}/notifications", summary: "Drain an agent's inbox (event notifications).", query: "mark_read=bool&limit=int", body: None },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}/messages", summary: "Read direct messages sent to an agent.", query: "mark_read=bool&limit=int", body: None },
    Endpoint { method: "GET", path: "/api/projects", summary: "List projects (with task counts).", query: "status=str", body: None },
    Endpoint { method: "POST", path: "/api/projects", summary: "Create a project.", query: "", body: Some("CreateProjectBody") },
    Endpoint { method: "GET", path: "/api/projects/{project_id}", summary: "Fetch one project.", query: "", body: None },
    Endpoint { method: "PATCH", path: "/api/projects/{project_id}", summary: "Update a project (rename, archive, description, metadata).", query: "", body: Some("UpdateProjectBody") },
    Endpoint { method: "GET", path: "/api/tasks", summary: "List/search tasks, optionally filtered. Archived tasks are hidden unless include_archived=true.", query: "project_id=int&status=str&assignee=str&unassigned=bool&parent_id=int&top_level=bool&q=str&blocked_on_kind=str&blocked_on_ref=str&meta_key=str&meta_value=str&include_archived=bool", body: None },
    Endpoint { method: "POST", path: "/api/tasks", summary: "Create a task.", query: "", body: Some("CreateTaskBody") },
    Endpoint { method: "GET", path: "/api/tasks/{task_id}", summary: "Fetch one task (with comments).", query: "", body: None },
    Endpoint { method: "PATCH", path: "/api/tasks/{task_id}", summary: "Update task fields (status, assignee, ...).", query: "", body: Some("UpdateTaskBody") },
    Endpoint { method: "POST", path: "/api/tasks/{task_id}/comments", summary: "Add a comment to a task.", query: "", body: Some("CommentBody") },
    Endpoint { method: "GET", path: "/api/comments/{comment_id}", summary: "Read one comment by id, with its type (plain/question/answer), parsed payload, lifecycle state, and reply_to/supersedes links.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/tasks/{task_id}/questions", summary: "Pose a structured question on a task (kind yes_no/multiple_choice/select_all/fill_in_the_blank/rank_list), routed to a principal; blocking by default. Returns the question comment.", query: "", body: Some("PoseQuestionBody") },
    Endpoint { method: "POST", path: "/api/comments/{comment_id}/answer", summary: "Answer an open question. A framed answer (shape matching the kind) marks it answered; a text answer to a non-text kind is the out-of-frame escape (answered-outside-frame). Returns the answer comment.", query: "", body: Some("AnswerQuestionBody") },
    Endpoint { method: "POST", path: "/api/comments/{comment_id}/decline", summary: "Decline an open question with feedback (an explicit refusal, distinct from an out-of-frame answer).", query: "", body: Some("DeclineQuestionBody") },
    Endpoint { method: "POST", path: "/api/comments/{comment_id}/cancel", summary: "Cancel an open question you posed (the asker withdraws it).", query: "", body: Some("CancelQuestionBody") },
    Endpoint { method: "POST", path: "/api/comments/{comment_id}/supersede", summary: "Supersede an open question with a replacement (doc_33 A6): the old is kept immutable + linked, the new copies its payload with a new prompt. Asker-only.", query: "", body: Some("SupersedeQuestionBody") },
    Endpoint { method: "GET", path: "/api/tasks/blocking-me", summary: "Tasks with an open blocking question routed to `viewer` (team-expanded) -- the question-based waiting-on-me view.", query: "viewer=str&project_id=int&include_archived=bool", body: None },
    Endpoint { method: "PATCH", path: "/api/tasks/{task_id}/props", summary: "Merge a JSON object into a task's metadata.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/tasks/{task_id}/move", summary: "Move a task to a different project.", query: "", body: Some("MoveTaskBody") },
    Endpoint { method: "POST", path: "/api/tasks/{task_id}/archive", summary: "Soft-archive a task: hide it from the default list_tasks view (still fetchable by id and with include_archived). Orthogonal to status; reversible with restore.", query: "", body: Some("ArchiveTaskBody") },
    Endpoint { method: "POST", path: "/api/tasks/{task_id}/restore", summary: "Restore an archived task so it reappears in the default list_tasks view.", query: "", body: Some("ArchiveTaskBody") },
    Endpoint { method: "POST", path: "/api/tasks/{task_id}/mute", summary: "Mute a task for an agent: detach them from its event fan-out (stop FYI notifications).", query: "", body: Some("MuteTaskBody") },
    Endpoint { method: "POST", path: "/api/tasks/{task_id}/unmute", summary: "Unmute a task for an agent (rejoin its fan-out).", query: "", body: Some("MuteTaskBody") },
    Endpoint { method: "POST", path: "/api/subscriptions", summary: "Subscribe to a task, project, channel, document, or the whole board (board=true). Optional event_classes (e.g. [\"created\"]) makes it a delivery-gated filtered subscription (only those classes reach the inbox and wake you); omit for every event. Idempotent (re-subscribe updates the class set). Pass thread_root (a channel post's event seq) to subscribe to a THREAD and be delivered+woken on in-thread follow-ups (reply_to=that root) without a re-mention.", query: "", body: Some("SubscribeBody") },
    Endpoint { method: "DELETE", path: "/api/subscriptions", summary: "Unsubscribe from a task, project, channel, document, the whole board (board=true), or a thread (thread_root=the root post seq).", query: "", body: Some("SubscribeBody") },
    Endpoint { method: "GET", path: "/api/channels", summary: "List channels (public, or a member's incl. private/DM).", query: "member=str", body: None },
    Endpoint { method: "POST", path: "/api/channels", summary: "Create (or get) a named channel.", query: "", body: Some("CreateChannelBody") },
    Endpoint { method: "GET", path: "/api/channels/{channel_id}", summary: "Fetch one channel with its members.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/channels/{channel_id}/posts", summary: "Read a channel's post history. order=desc returns the latest N (newest-first) for a chat view; default asc is oldest-first for scrollback. before_seq pages earlier.", query: "since_seq=int&limit=int&before_seq=int&order=asc|desc", body: None },
    Endpoint { method: "POST", path: "/api/channels/{channel_id}/posts", summary: "Post a message to a channel.", query: "", body: Some("PostToChannelBody") },
    Endpoint { method: "PATCH", path: "/api/channels/{channel_id}/props", summary: "Merge props into a channel's metadata (e.g. the outbound reflect-back policy).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/channels/{channel_id}/auto-join", summary: "Set/clear a channel's auto_join flag — a fleet-wide broadcast channel every agent belongs to (enabling joins all current agents + auto-joins future ones on register).", query: "", body: Some("SetChannelAutoJoinBody") },
    Endpoint { method: "POST", path: "/api/channels/{channel_id}/promote-thread", summary: "Promote a channel thread into a task (root→description, replies→comments); idempotent.", query: "", body: Some("PromoteThreadBody") },
    Endpoint { method: "POST", path: "/api/channels/{channel_id}/invites", summary: "Invite an agent into a channel (auto-join + notify).", query: "", body: Some("InviteChannelBody") },
    Endpoint { method: "POST", path: "/api/messages", summary: "Send a direct message between agents.", query: "", body: Some("SendMessageBody") },
    Endpoint { method: "POST", path: "/api/dms", summary: "Get (or create) the private 1:1 DM channel for a pair of agents, returning the channel + members. Idempotent and order-independent — lets a client open/link a DM before any message is sent.", query: "", body: Some("OpenDmBody") },
    Endpoint { method: "GET", path: "/api/events", summary: "Read the append-only event log (optionally filtered to one actor). order=desc returns the latest N (newest-first) for a live feed; default asc is oldest-first for incremental pollers.", query: "since_seq=int&limit=int&actor=str&order=asc|desc", body: None },
    Endpoint { method: "GET", path: "/api/external-identities", summary: "List external (bridged) identities, optionally filtered by source.", query: "source=str", body: None },
    Endpoint { method: "POST", path: "/api/external-identities", summary: "Register/update an external identity (a bridged human/actor, e.g. slack:U123).", query: "", body: Some("UpsertExternalIdentityBody") },
    Endpoint { method: "GET", path: "/api/external-links", summary: "List bridged links (channel-map / issue↔task / thread↔task), filter by source/board_kind/board_id.", query: "source=str&board_kind=str&board_id=int", body: None },
    Endpoint { method: "POST", path: "/api/external-links", summary: "Map a board entity (channel|task|thread) to an external one; idempotent on (source, external_id).", query: "", body: Some("UpsertExternalLinkBody") },
    Endpoint { method: "GET", path: "/api/workspace-kinds", summary: "List custom workspace kinds (named env setup definitions fleet spin-up materializes from board data).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/workspace-kinds", summary: "Define/update a workspace kind (setup_script + config an agent is configured with); idempotent on name, config merges.", query: "", body: Some("SetWorkspaceKindBody") },
    Endpoint { method: "GET", path: "/api/workspace-kinds/{name}", summary: "Fetch one workspace kind (setup_script + config) by name — what fleet spin-up reads to materialize a workspace.", query: "", body: None },
    Endpoint { method: "DELETE", path: "/api/workspace-kinds/{name}", summary: "Retire a workspace kind by name.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/lint", summary: "Dry-run the pre-submit content lint on arbitrary text without writing: returns {clean, banned_phrases, non_ascii} against the authoritative live banned-phrases list + ASCII-only rule. Use this to pre-check content (incl. before a CID publish, which the write-path gate does not cover) instead of a drift-prone local copy.", query: "", body: Some("LintTextBody") },
    Endpoint { method: "GET", path: "/api/banned-phrases", summary: "List the fleet banned-phrases list (what the pre-submit content lint checks docs and comments against).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/banned-phrases", summary: "Add a phrase to the banned-phrases list (idempotent on the phrase, stored lowercased).", query: "", body: Some("AddBannedPhraseBody") },
    Endpoint { method: "DELETE", path: "/api/banned-phrases/{phrase}", summary: "Remove a phrase from the banned-phrases list.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/identity-aliases", summary: "List the identity aliases (alias -> canonical identity, e.g. operator -> cameron). A small config table consumers/UI use to resolve or display a floating name as the canonical identity across assignee, blocked_on, and @-mentions.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/people", summary: "List people (first-class human identities, multi-operator model doc_26). A separate registry from agents; resolved together with agents at read time.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/people", summary: "Create or upsert a person by stable string id (e.g. cameron).", query: "", body: Some("CreatePersonBody") },
    Endpoint { method: "DELETE", path: "/api/people/{id}", summary: "Delete a person and drop their team memberships.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/teams", summary: "List teams (addressable groups whose members are people OR other teams).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/teams", summary: "Create or upsert a team by stable string id (e.g. operator).", query: "", body: Some("CreateTeamBody") },
    Endpoint { method: "GET", path: "/api/teams/{team_id}", summary: "Get a team with its direct members and its fully-resolved person AND agent sets (resolved_people + resolved_agents, kept separate; nested teams expanded, cycle-guarded).", query: "", body: None },
    Endpoint { method: "DELETE", path: "/api/teams/{team_id}", summary: "Delete a team and drop its memberships (its members and its membership in parent teams).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/teams/{team_id}/members", summary: "Add a person or team as a member (idempotent). Rejects a sub-team add that would create a membership cycle.", query: "", body: Some("TeamMemberBody") },
    Endpoint { method: "DELETE", path: "/api/teams/{team_id}/members", summary: "Remove a member (person or team) from a team (idempotent).", query: "", body: Some("TeamMemberBody") },
    Endpoint { method: "POST", path: "/api/identity-aliases", summary: "Upsert an identity alias (alias -> canonical). Idempotent on the alias (repoints an existing one); alias is stored lowercased.", query: "", body: Some("SetIdentityAliasBody") },
    Endpoint { method: "GET", path: "/api/secret-requests", summary: "List secret requests (metadata only — never the ciphertext or tokens). The board is an ephemeral request broker, not a secret store.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/secret-requests", summary: "File a named secret request (carries the age recipient pubkeys + instructions). Returns a single-use submit_url the operator opens to submit the value encrypted in-browser, plus the fulfiller_token.", query: "", body: Some("CreateSecretRequestBody") },
    Endpoint { method: "GET", path: "/api/secret-requests/{id}", summary: "Fetch one secret request as metadata (name, instructions, recipients, status) — drives the submit page. Never returns ciphertext or tokens.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/secret-requests/{id}/submit", summary: "Submit the browser-encrypted ciphertext for a request (single-use submit token). Flips it to submitted with a TTL and directly notifies the fulfiller.", query: "", body: Some("SubmitSecretBody") },
    Endpoint { method: "GET", path: "/api/secret-requests/{id}/ciphertext", summary: "Fulfiller pulls the ciphertext once to relocate it into durable storage (fulfiller token via ?token=). The one place ciphertext leaves the board.", query: "token=str", body: None },
    Endpoint { method: "POST", path: "/api/secret-requests/{id}/fulfill", summary: "Fulfill a request: the secret is relocated, so the board deletes the row + its transient ciphertext. Idempotent; fulfiller-token-gated while the row exists.", query: "", body: Some("FulfillSecretBody") },
    Endpoint { method: "POST", path: "/api/secret-requests/{id}/cancel", summary: "Cancel (delete) a pending secret request. Idempotent.", query: "", body: Some("CancelSecretBody") },
    Endpoint { method: "GET", path: "/api/reviews", summary: "List reviews (newest-touched first), optionally filtered by status/kind/assignee. Without logs.", query: "status=str&kind=str&assignee=str", body: None },
    Endpoint { method: "POST", path: "/api/reviews", summary: "Create a review over an artifact (document|code|design|agent-session|task). Starts in `open` unless a status is seeded; records a `submitted` log entry. Pass external_link for idempotent ingest (a review already linked on (source, external_id) is returned created:false).", query: "", body: Some("CreateReviewBody") },
    Endpoint { method: "GET", path: "/api/reviews/trend", summary: "Improvement trend derived from review logs (no stored counter): findings-per-review with an earlier-vs-later trend, overall + sliced by kind and by producing area, counterbalanced by an escaped-defect signal (post-approval findings, re-opens, lineage follow-ups). A slice where findings fell while escaped defects rose is flagged.", query: "kind=str&area=str", body: None },
    Endpoint { method: "GET", path: "/api/reviews/{review_id}", summary: "Fetch one review with its full append-only log (findings are the entries of type `finding`).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/reviews/{review_id}/status", summary: "Transition a review's A2 status (open/in_review/changes_requested/approved/closed). Same status = idempotent no-op. Emits review.status_changed (+ opened_for_review / terminal).", query: "", body: Some("SetReviewStatusBody") },
    Endpoint { method: "POST", path: "/api/reviews/{review_id}/vetted", summary: "Set/clear a review's vetted gate (adversarial review run + addressed). Audit-only per D17: records the actor + logs the change (a decision entry), emits review.vetted_changed. Same value = idempotent no-op.", query: "", body: Some("SetReviewVettedBody") },
    Endpoint { method: "POST", path: "/api/reviews/{review_id}/log", summary: "Append a log entry (comment / finding / decision / ...). Pass external_id for idempotent ingest (a bridge replaying an upstream item returns appended:false).", query: "", body: Some("AppendReviewLogBody") },
    Endpoint { method: "POST", path: "/api/ipfs/add", summary: "Content-address raw `content` server-side (add-only) and return its CID. Requires ipfs_api_url.", query: "", body: Some("IpfsAddBody") },
    Endpoint { method: "GET", path: "/api/ipfs/{cid}", summary: "Read content by CID through the IPFS backend (scoped, read-only). Pass ?content_type= to label the response. Requires ipfs_api_url.", query: "content_type=str", body: None },
    Endpoint { method: "GET", path: "/api/documents", summary: "List documents for discovery (filter by project/status/tag/task_id/author; archived hidden unless include_archived=true).", query: "project_id=int&status=str&tag=str&task_id=int&author=str&include_archived=bool", body: None },
    Endpoint { method: "POST", path: "/api/documents", summary: "Create a versioned document (content is a bare IPFS CID; the board never resolves it).", query: "", body: Some("CreateDocumentBody") },
    Endpoint { method: "GET", path: "/api/wiki", summary: "List path-filed documents as a wiki tree (optionally under a path prefix), ordered by path; archived hidden unless include_archived=true.", query: "prefix=str&include_archived=bool", body: None },
    Endpoint { method: "GET", path: "/api/documents/{document_id}", summary: "Fetch one document with its current version + version list. Pass ?include_body=true to also inline the current version's markdown (resolved server-side from its CID; body:null + body_error on fetch failure).", query: "include_body=bool", body: None },
    Endpoint { method: "PATCH", path: "/api/documents/{document_id}", summary: "Rename a document (set its title; metadata-only — versions/content/path/status untouched). Emits document.updated.", query: "", body: Some("UpdateDocumentBody") },
    Endpoint { method: "GET", path: "/api/documents/{document_id}/content", summary: "Read a document's body inline (resolves the version CID through the IPFS backend). Pass ?version_no= for a specific version. Requires ipfs_api_url.", query: "version_no=int", body: None },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/path", summary: "Set (or clear, with an empty path) a document's wiki path; unique among filed docs.", query: "", body: Some("SetDocumentPathBody") },
    Endpoint { method: "GET", path: "/api/documents/{document_id}/versions", summary: "List a document's immutable versions (newest first).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/versions", summary: "Publish a new immutable version (bare CID).", query: "", body: Some("PublishVersionBody") },
    Endpoint { method: "GET", path: "/api/documents/{document_id}/comments", summary: "List a document's comments (filter by version_id/status).", query: "version_id=int&status=str", body: None },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/comments", summary: "Comment on a document, optionally region-anchored to a version.", query: "", body: Some("CommentDocumentBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/comments/{comment_id}/resolve", summary: "Mark a document comment resolved.", query: "", body: Some("ResolveCommentBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/submit-review", summary: "Submit a document for review (status -> in_review).", query: "", body: Some("DocumentActorBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/submit-to-operator-review", summary: "Submit a document into the operator's review queue (status -> operator_review) -- the single gated chokepoint before the operator sees it. Rejected unless a template attestation (template_followed or template_waiver_reason) is given AND a design-conformance review has run against the current version with zero open findings.", query: "", body: Some("SubmitToOperatorReviewBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/request-changes", summary: "Request changes on a document (status -> changes_requested).", query: "", body: Some("RequestChangesBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/approve", summary: "Approve a document (stamps the current version, status -> approved).", query: "", body: Some("DocumentActorBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/attach", summary: "Attach a document to a task (notifies both sides).", query: "", body: Some("AttachDocumentBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/detach", summary: "Detach a document from a task.", query: "", body: Some("AttachDocumentBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/archive", summary: "Soft-archive (retire) a document: hidden from listings by default, reversible, history preserved.", query: "", body: Some("DocumentActorBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/restore", summary: "Restore a previously archived document (clears the archive stamp).", query: "", body: Some("DocumentActorBody") },
    Endpoint { method: "GET", path: "/api/stream", summary: "Server-Sent Events feed of live board activity.", query: "last_event_id=int", body: None },
];

/// Build the JSON Schemas for every referenced request body, keyed by struct name.
fn body_schemas() -> Value {
    macro_rules! schemas {
        ($($t:ty),* $(,)?) => {{
            let mut m = serde_json::Map::new();
            $( m.insert(stringify!($t).into(), serde_json::to_value(schema_for!($t)).unwrap()); )*
            Value::Object(m)
        }};
    }
    schemas!(
        RegisterAgentBody,
        UpdateAgentBody,
        SetStatusBody,
        RequestStandDownBody,
        CreateProjectBody,
        UpdateProjectBody,
        CreateTaskBody,
        UpdateTaskBody,
        MoveTaskBody,
        ArchiveTaskBody,
        MuteTaskBody,
        CommentBody,
        SubscribeBody,
        CreateChannelBody,
        PostToChannelBody,
        InviteChannelBody,
        SendMessageBody,
        OpenDmBody,
        CreateDocumentBody,
        PublishVersionBody,
        SetDocumentPathBody,
        CommentDocumentBody,
        ResolveCommentBody,
        DocumentActorBody,
        SubmitToOperatorReviewBody,
        PoseQuestionBody,
        AnswerQuestionBody,
        DeclineQuestionBody,
        CancelQuestionBody,
        SupersedeQuestionBody,
        RequestChangesBody,
        AttachDocumentBody,
        IpfsAddBody,
        UpsertExternalIdentityBody,
        UpsertExternalLinkBody,
        PromoteThreadBody,
        SetWorkspaceKindBody,
        AddBannedPhraseBody,
        LintTextBody,
        UpdateDocumentBody,
        CreateSecretRequestBody,
        SubmitSecretBody,
        FulfillSecretBody,
        CancelSecretBody,
        SetChannelAutoJoinBody,
        CreateReviewBody,
        SetReviewStatusBody,
        SetReviewVettedBody,
        AppendReviewLogBody,
        SetIdentityAliasBody,
        CreatePersonBody,
        CreateTeamBody,
        TeamMemberBody,
    )
}

/// The machine-readable discovery document (also drives the HTML page).
fn discovery_doc() -> Value {
    let endpoints: Vec<Value> = ENDPOINTS
        .iter()
        .map(|e| {
            json!({
                "method": e.method,
                "path": e.path,
                "summary": e.summary,
                "query": e.query,
                "body_schema": e.body,
            })
        })
        .collect();
    json!({
        "service": "task-board",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "REST API for the agent coordination board. There is also an MCP surface at /mcp.",
        "endpoints": endpoints,
        "schemas": body_schemas(),
    })
}

/// `GET /api` — the discovery root. Content-negotiates: browsers (Accept: text/html) get a
/// clickable, documented page; API clients get the JSON discovery document.
async fn index(headers: HeaderMap) -> Response {
    let doc = discovery_doc();
    let wants_html = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("text/html"))
        .unwrap_or(false);
    if wants_html {
        // Behind a reverse proxy on a sub-path (e.g. /board) the proxy strips the prefix
        // before we see the request, so our own routes are still rooted at /. The proxy
        // advertises the external mount via X-Forwarded-Prefix; honor it so the page's
        // links resolve for the browser. Absent (direct access) => no prefix.
        let prefix = forwarded_prefix(&headers);
        Html(render_index_html(&doc, &prefix)).into_response()
    } else {
        Json(doc).into_response()
    }
}

/// The external path prefix this request arrived under, from `X-Forwarded-Prefix`, with
/// any trailing slash trimmed (so `""` or `"/board"`). Empty when direct / unset.
fn forwarded_prefix(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-prefix")
        .and_then(|v| v.to_str().ok())
        .map(|p| p.trim_end_matches('/').to_string())
        .unwrap_or_default()
}

/// Render the discovery document as a standalone HTML page (no build step, no JS deps).
/// `prefix` is the external mount path (e.g. "/board" or ""), prepended to every link so
/// the page works whether served at the origin root or behind a sub-path proxy.
fn render_index_html(doc: &Value, prefix: &str) -> String {
    let mut rows = String::new();
    for e in doc["endpoints"].as_array().unwrap() {
        let method = e["method"].as_str().unwrap_or("");
        let path = e["path"].as_str().unwrap_or("");
        let summary = html_escape(e["summary"].as_str().unwrap_or(""));
        let query = e["query"].as_str().unwrap_or("");
        // GET endpoints with no path params are directly clickable.
        let is_get = method == "GET";
        let clickable = is_get && !path.contains('{');
        let path_cell = if clickable {
            // Link target carries the external prefix; the displayed text stays the clean
            // origin-rooted path so the docs read the same regardless of mount point.
            format!(
                "<a href=\"{href}\">{p}</a>",
                href = html_escape(&format!("{prefix}{path}")),
                p = html_escape(path),
            )
        } else {
            format!("<span>{}</span>", html_escape(path))
        };
        let query_html = if query.is_empty() {
            String::new()
        } else {
            format!("<div class=\"q\">?{}</div>", html_escape(query))
        };
        let body_html = match e["body_schema"].as_str() {
            Some(name) => format!(
                "<a class=\"schema\" href=\"#schema-{n}\">{n}</a>",
                n = html_escape(name)
            ),
            None => "<span class=\"muted\">—</span>".into(),
        };
        rows.push_str(&format!(
            "<tr><td><code class=\"m m-{ml}\">{method}</code></td><td class=\"path\"><code>{path_cell}</code>{query_html}</td><td>{summary}</td><td>{body_html}</td></tr>",
            ml = method.to_lowercase(),
        ));
    }

    let mut schema_blocks = String::new();
    if let Some(schemas) = doc["schemas"].as_object() {
        let mut names: Vec<&String> = schemas.keys().collect();
        names.sort();
        for name in names {
            let pretty = serde_json::to_string_pretty(&schemas[name]).unwrap_or_default();
            schema_blocks.push_str(&format!(
                "<section id=\"schema-{n}\"><h3>{n}</h3><pre><code>{body}</code></pre></section>",
                n = html_escape(name),
                body = html_escape(&pretty),
            ));
        }
    }

    let version = doc["version"].as_str().unwrap_or("");
    format!(
        r#"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>task-board API</title>
<style>
  :root {{ color-scheme: dark; }}
  body {{ margin: 0; background:#0b0d10; color:#e6e8eb; font:14px/1.5 ui-sans-serif,system-ui,-apple-system,sans-serif; }}
  .wrap {{ max-width: 960px; margin: 0 auto; padding: 2rem 1.25rem 4rem; }}
  h1 {{ font-size: 1.4rem; margin: 0 0 .25rem; }}
  h1 .sky {{ color:#38bdf8; }}
  .lede {{ color:#9aa4af; margin:0 0 1.5rem; }}
  a {{ color:#7dd3fc; text-decoration: none; }}
  a:hover {{ text-decoration: underline; }}
  table {{ width:100%; border-collapse: collapse; }}
  th,td {{ text-align:left; padding:.5rem .6rem; border-bottom:1px solid #1c2128; vertical-align: top; }}
  th {{ color:#9aa4af; font-size:.72rem; text-transform:uppercase; letter-spacing:.04em; }}
  code {{ font-family: ui-monospace,SFMono-Regular,Menlo,monospace; font-size:.82rem; }}
  .path code {{ color:#e6e8eb; }}
  .q {{ color:#6b7684; font-size:.72rem; margin-top:.1rem; }}
  .m {{ font-weight:600; padding:.05rem .4rem; border-radius:.3rem; font-size:.72rem; }}
  .m-get {{ background:#0e2a3a; color:#7dd3fc; }}
  .m-post {{ background:#0f2e1c; color:#86efac; }}
  .m-patch {{ background:#2e2410; color:#fcd34d; }}
  .m-delete {{ background:#2e1414; color:#fca5a5; }}
  .muted {{ color:#4b5563; }}
  .schema {{ font-family: ui-monospace,monospace; font-size:.8rem; }}
  section {{ margin-top:1.25rem; }}
  section h3 {{ font-size:.9rem; margin:0 0 .4rem; color:#cbd5e1; }}
  pre {{ background:#11151a; border:1px solid #1c2128; border-radius:.5rem; padding:.9rem 1rem; overflow:auto; }}
  .top {{ display:flex; align-items:baseline; gap:.75rem; flex-wrap:wrap; }}
  .badge {{ color:#6b7684; font-size:.75rem; }}
  hr {{ border:0; border-top:1px solid #1c2128; margin:2rem 0 1rem; }}
</style></head>
<body><div class="wrap">
  <div class="top">
    <h1><span class="sky">task</span>-board API</h1>
    <span class="badge">v{version}</span>
    <span class="badge">· <a href="{prefix}/">web UI</a> · MCP at <code>{prefix}/mcp</code></span>
  </div>
  <p class="lede">REST surface for the agent coordination board. GET links are live — click to try them. This page is also available as JSON (send <code>Accept: application/json</code> or fetch <code>{prefix}/api</code>).</p>
  <table>
    <thead><tr><th>Method</th><th>Path</th><th>Summary</th><th>Body</th></tr></thead>
    <tbody>{rows}</tbody>
  </table>
  <hr>
  <h2 style="font-size:1rem;color:#cbd5e1;">Request body schemas</h2>
  {schema_blocks}
</div></body></html>"#,
    )
}

/// Minimal HTML escaping for text interpolated into the discovery page.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// --- Agents ---

#[derive(Deserialize)]
struct ListAgentsQuery {
    status: Option<String>,
    q: Option<String>,
    meta_key: Option<String>,
    meta_value: Option<String>,
    #[serde(default)]
    verbose: bool,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_agents(
    State(st): State<AppState>,
    Query(query): Query<ListAgentsQuery>,
) -> ApiResult {
    Ok(Json(
        core::list_agents(
            &st.pool,
            query.status.as_deref(),
            query.q.as_deref(),
            query.meta_key.as_deref(),
            query.meta_value.as_deref(),
            query.verbose,
            query.limit,
            query.offset,
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct RegisterAgentBody {
    agent_id: String,
    display_name: Option<String>,
    kind: Option<String>,
    /// Free-form charter (role/mission/scope). Editable; omitting it keeps the existing one.
    charter: Option<String>,
    /// Arbitrary registry props (role, model, effort, interval, worktree, area, and
    /// `repos: [{repo, branch}, ...]` — an agent may span several repos, each checked out
    /// in its own workspace). MERGED into any existing bag, not replaced.
    metadata: Option<Value>,
    webhook_url: Option<String>,
}

async fn register_agent(State(st): State<AppState>, Json(b): Json<RegisterAgentBody>) -> ApiResult {
    Ok(Json(
        core::register_agent(
            &st.pool,
            &b.agent_id,
            b.display_name.as_deref(),
            b.kind.as_deref(),
            b.charter.as_deref(),
            b.metadata,
            b.webhook_url.as_deref(),
        )
        .await?,
    ))
}

async fn get_agent(State(st): State<AppState>, Path(agent_id): Path<String>) -> ApiResult {
    Ok(Json(core::get_agent(&st.pool, &agent_id).await?))
}

#[derive(Deserialize, JsonSchema)]
struct UpdateAgentBody {
    display_name: Option<String>,
    kind: Option<String>,
    charter: Option<String>,
    status: Option<String>,
    status_message: Option<String>,
    webhook_url: Option<String>,
    /// MERGED into the agent's registry bag, not replaced.
    metadata: Option<Value>,
    /// Field names to CLEAR to null (a merge-PATCH leaves an omitted/null field unchanged, so this
    /// is the only way to reset a nullable field, e.g. ["webhook_url"]). Clearable: display_name,
    /// kind, charter, status_message, webhook_url. An explicit value for a field wins over clearing.
    #[serde(default)]
    clear: Option<Vec<String>>,
    /// Return the full agent (including `charter`) in the response. Default false — the response
    /// omits the charter to keep a looping caller's context light; fetch it via GET /api/agents/{id}.
    #[serde(default)]
    verbose: Option<bool>,
}

async fn update_agent(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(b): Json<UpdateAgentBody>,
) -> ApiResult {
    let out = core::update_agent(
        &st.pool,
        &agent_id,
        b.display_name.as_deref(),
        b.kind.as_deref(),
        b.charter.as_deref(),
        b.status.as_deref(),
        b.status_message.as_deref(),
        b.webhook_url.as_deref(),
        b.metadata,
        b.clear.as_deref(),
    )
    .await?;
    Ok(Json(if b.verbose.unwrap_or(false) {
        out
    } else {
        core::strip_field(out, "charter")
    }))
}

#[derive(Deserialize, JsonSchema)]
struct SetStatusBody {
    /// Roster presence, one of: online, idle, busy, blocked, away, offline. Other/free-form text is
    /// coerced to the nearest presence (the original is salvaged into status_message).
    status: String,
    status_message: Option<String>,
}

async fn set_status(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(b): Json<SetStatusBody>,
) -> ApiResult {
    // Presence fields only — a looping caller re-ingests this every tick (task #416).
    let out = core::set_status(&st.pool, &agent_id, &b.status, b.status_message.as_deref()).await?;
    Ok(Json(core::presence_projection(out)))
}

#[derive(Deserialize, JsonSchema)]
struct RequestStandDownBody {
    /// Who is asking (for the audit event + the agent's page).
    requested_by: Option<String>,
    /// Optional reason shown to the agent.
    reason: Option<String>,
}

async fn request_stand_down(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(b): Json<RequestStandDownBody>,
) -> ApiResult {
    Ok(Json(
        core::request_stand_down(
            &st.pool,
            &agent_id,
            b.requested_by.as_deref(),
            b.reason.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct NotificationsQuery {
    #[serde(default = "default_true")]
    mark_read: bool,
    #[serde(default = "default_limit")]
    limit: i64,
}

async fn get_notifications(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Query(q): Query<NotificationsQuery>,
) -> ApiResult {
    Ok(Json(
        core::check_notifications(&st.pool, &agent_id, q.mark_read, q.limit, None).await?,
    ))
}

async fn get_messages(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Query(q): Query<NotificationsQuery>,
) -> ApiResult {
    Ok(Json(
        core::get_messages(&st.pool, &agent_id, q.mark_read, q.limit).await?,
    ))
}

// --- Projects ---

#[derive(Deserialize, JsonSchema)]
struct ListProjectsQuery {
    status: Option<String>,
}

async fn list_projects(
    State(st): State<AppState>,
    Query(q): Query<ListProjectsQuery>,
) -> ApiResult {
    Ok(Json(
        core::list_projects(&st.pool, q.status.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct CreateProjectBody {
    name: String,
    description: Option<String>,
    created_by: Option<String>,
    metadata: Option<Value>,
}

async fn create_project(State(st): State<AppState>, Json(b): Json<CreateProjectBody>) -> ApiResult {
    Ok(Json(
        core::create_project(
            &st.pool,
            &b.name,
            b.description.as_deref(),
            b.created_by.as_deref(),
            b.metadata,
        )
        .await?,
    ))
}

async fn get_project(
    State(st): State<AppState>,
    Path(ProjectRef(project_id)): Path<ProjectRef>,
) -> ApiResult {
    found(core::get_project(&st.pool, project_id).await?)
}

#[derive(Deserialize, JsonSchema)]
struct UpdateProjectBody {
    name: Option<String>,
    description: Option<String>,
    /// active / archived. Archiving hides it from the sidebar; fully reversible.
    status: Option<String>,
    /// MERGED into the project's props (e.g. a repo link), not replaced.
    metadata: Option<Value>,
    actor: Option<String>,
}

async fn update_project(
    State(st): State<AppState>,
    Path(ProjectRef(project_id)): Path<ProjectRef>,
    Json(b): Json<UpdateProjectBody>,
) -> ApiResult {
    Ok(Json(
        core::update_project(
            &st.pool,
            project_id,
            b.name.as_deref(),
            b.description.as_deref(),
            b.status.as_deref(),
            b.metadata,
            b.actor.as_deref(),
        )
        .await?,
    ))
}

// --- Tasks ---

#[derive(Deserialize, JsonSchema)]
struct ListTasksQuery {
    project_id: Option<i64>,
    status: Option<String>,
    assignee: Option<String>,
    /// Only tasks with no assignee (assignee IS NULL). Takes precedence over `assignee`.
    unassigned: Option<bool>,
    /// Only the direct children of this task. Takes precedence over `top_level`.
    parent_id: Option<i64>,
    /// Only top-level tasks (no parent).
    top_level: Option<bool>,
    /// Free-text search over title + description (across all projects when project_id omitted).
    q: Option<String>,
    /// What blocked tasks are waiting on: filter by blocked_on kind (task|agent|team|operator|external).
    blocked_on_kind: Option<String>,
    /// Filter by blocked_on ref (a blocking task id or agent id) — e.g. "what is blocked on me".
    blocked_on_ref: Option<String>,
    /// Filter to tasks whose metadata has this key (a JSON path under `$.`, e.g. "observes").
    /// Pair with `meta_value`; both must be set for the filter to apply.
    meta_key: Option<String>,
    /// The value `meta_key` must equal (matched against `json_extract(metadata, '$.'||key)`).
    meta_value: Option<String>,
    /// Include archived tasks. Archived tasks are hidden by default; set true to list them too.
    include_archived: Option<bool>,
}

async fn list_tasks(State(st): State<AppState>, Query(query): Query<ListTasksQuery>) -> ApiResult {
    Ok(Json(
        core::list_tasks(
            &st.pool,
            query.project_id,
            query.status.as_deref(),
            query.assignee.as_deref(),
            query.unassigned.unwrap_or(false),
            query.parent_id,
            query.top_level.unwrap_or(false),
            query.q.as_deref(),
            query.blocked_on_kind.as_deref(),
            query.blocked_on_ref.as_deref(),
            query.meta_key.as_deref(),
            query.meta_value.as_deref(),
            query.include_archived.unwrap_or(false),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct CreateTaskBody {
    project_id: i64,
    title: String,
    description: Option<String>,
    assignee: Option<String>,
    priority: Option<String>,
    created_by: Option<String>,
    metadata: Option<Value>,
    /// Optional parent task (makes this a child/subtask). Must be in the same project.
    parent_id: Option<i64>,
    /// Optional external reference for idempotent ingest: if a task is already linked on
    /// (source, external_id), the existing task is returned (`created:false`) instead of a
    /// duplicate. Lets a bridge adapter create-from-external exactly-once.
    external_link: Option<core::ExternalRef>,
}

async fn create_task(State(st): State<AppState>, Json(b): Json<CreateTaskBody>) -> ApiResult {
    Ok(Json(
        core::create_task(
            &st.pool,
            b.project_id,
            &b.title,
            b.description.as_deref(),
            b.assignee.as_deref(),
            b.priority.as_deref(),
            b.created_by.as_deref(),
            b.metadata,
            b.parent_id,
            b.external_link,
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct GetTaskQuery {
    /// Bound the inlined `comments` to the most-recent N (chronological within the slice). Omit for
    /// the whole thread; 0 for metadata-only. The response carries `comment_count` +
    /// `comments_truncated`. Mirrors the MCP `get_task` bounding (task #511).
    #[serde(default)]
    comments_limit: Option<i64>,
}

async fn get_task(
    State(st): State<AppState>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Query(q): Query<GetTaskQuery>,
) -> ApiResult {
    found(core::get_task_limited(&st.pool, task_id, q.comments_limit).await?)
}

/// What a blocked task is waiting on. `kind` is task | agent | team | operator | external (or "none"/""
/// to clear). `target` is the blocking task id, agent id, or team id (ignored for operator/external).
/// A blocked task must carry one. When kind=team, every person the team resolves to is notified.
#[derive(Deserialize, JsonSchema)]
struct BlockedOnBody {
    kind: String,
    target: Option<String>,
    note: Option<String>,
}

/// Map an optional blocked_on body into the value core expects: None = leave unchanged,
/// Value::Null = clear, an object = set.
fn blocked_on_value(b: Option<BlockedOnBody>) -> Option<Value> {
    b.map(|bo| {
        if bo.kind.is_empty() || bo.kind == "none" || bo.kind == "clear" {
            Value::Null
        } else {
            json!({ "kind": bo.kind, "target": bo.target, "note": bo.note })
        }
    })
}

#[derive(Deserialize, JsonSchema)]
struct UpdateTaskBody {
    status: Option<String>,
    /// New owner's agent id. To clear the owner (unassign), set `unassign: true` rather than
    /// sending an empty string here — some clients can't serialize "".
    assignee: Option<String>,
    /// Clear the task's owner (set it to no assignee). Takes precedence over `assignee`; the
    /// reliable, client-safe way to unassign.
    unassign: Option<bool>,
    title: Option<String>,
    description: Option<String>,
    priority: Option<String>,
    actor: Option<String>,
    metadata: Option<Value>,
    /// Reparent: a parent task id (same project), or 0 to clear the parent (make top-level).
    parent_id: Option<i64>,
    /// What this task is blocked on (required when setting status=blocked). Omit to leave
    /// unchanged; pass kind="none" to clear.
    blocked_on: Option<BlockedOnBody>,
    /// Return the full task (including `description`) in the response. Default false — the response
    /// omits the description to keep a looping caller's context light; fetch it via GET /api/tasks/{id}.
    #[serde(default)]
    verbose: Option<bool>,
}

async fn update_task(
    State(st): State<AppState>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Json(b): Json<UpdateTaskBody>,
) -> ApiResult {
    // `unassign: true` clears the owner via the core empty-string sentinel and wins over `assignee`.
    let assignee = if b.unassign.unwrap_or(false) {
        Some("")
    } else {
        b.assignee.as_deref()
    };
    let out = core::update_task(
        &st.pool,
        task_id,
        b.status.as_deref(),
        assignee,
        b.title.as_deref(),
        b.description.as_deref(),
        b.priority.as_deref(),
        b.actor.as_deref(),
        b.metadata,
        b.parent_id,
        blocked_on_value(b.blocked_on),
    )
    .await?;
    Ok(Json(if b.verbose.unwrap_or(false) {
        out
    } else {
        core::strip_field(out, "description")
    }))
}

#[derive(Deserialize, JsonSchema)]
struct CommentBody {
    body: String,
    author: Option<String>,
    /// Optional external identity id (e.g. "slack:U123") this comment is attributed to — for an
    /// ingested human author. `author` stays the fleet agent that performed the write.
    external_author: Option<String>,
    /// Optional external reference for idempotent ingest: if a comment is already linked on
    /// (source, external_id), the existing comment is returned (`created:false`) instead of a
    /// duplicate. Lets a bridge adapter mirror an external comment exactly-once.
    external_link: Option<core::ExternalRef>,
    /// Submit even if the body contains a banned phrase (the pre-submit lint otherwise rejects it).
    acknowledge_banned: Option<bool>,
}

async fn comment_task(
    State(st): State<AppState>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Json(b): Json<CommentBody>,
) -> ApiResult {
    core::check_content(&st.pool, &b.body, b.acknowledge_banned.unwrap_or(false)).await?;
    Ok(Json(
        core::comment_task(
            &st.pool,
            task_id,
            &b.body,
            b.author.as_deref(),
            b.external_author.as_deref(),
            b.external_link,
        )
        .await?,
    ))
}

async fn get_comment(State(st): State<AppState>, Path(comment_id): Path<i64>) -> ApiResult {
    Ok(Json(core::get_comment(&st.pool, comment_id).await?))
}

#[derive(Deserialize, JsonSchema)]
struct PoseQuestionBody {
    /// One of: yes_no, multiple_choice, select_all, fill_in_the_blank, rank_list.
    kind: String,
    prompt: String,
    /// Options as [{id, label}] -- required for multiple_choice / select_all / rank_list.
    options: Option<Value>,
    /// The principal (person/team/agent id) the question routes to; "operator" is the seeded team.
    routed_to: String,
    /// Whether the question blocks its task while open (default true).
    blocking: Option<bool>,
    /// Non-blocking only: the presumed answer the asker proceeds on.
    default: Option<Value>,
    /// Non-blocking only: wait this many seconds before proceeding on the default (requires default).
    wait_period_seconds: Option<i64>,
    actor: Option<String>,
}

async fn pose_question(
    State(st): State<AppState>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Json(b): Json<PoseQuestionBody>,
) -> ApiResult {
    Ok(Json(
        core::pose_question(
            &st.pool,
            task_id,
            &b.kind,
            &b.prompt,
            b.options,
            &b.routed_to,
            b.blocking.unwrap_or(true),
            b.default,
            b.wait_period_seconds,
            b.actor.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct AnswerQuestionBody {
    /// bool / choice / text / ranked. Use text for an out-of-frame answer to a non-text kind.
    shape: String,
    /// The answer value per shape (boolean; array of option ids; string; or ids in order).
    value: Value,
    actor: Option<String>,
}

async fn answer_question(
    State(st): State<AppState>,
    Path(comment_id): Path<i64>,
    Json(b): Json<AnswerQuestionBody>,
) -> ApiResult {
    Ok(Json(
        core::answer_question(&st.pool, comment_id, &b.shape, b.value, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct DeclineQuestionBody {
    feedback: String,
    actor: Option<String>,
}

async fn decline_question(
    State(st): State<AppState>,
    Path(comment_id): Path<i64>,
    Json(b): Json<DeclineQuestionBody>,
) -> ApiResult {
    Ok(Json(
        core::decline_question(&st.pool, comment_id, &b.feedback, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct CancelQuestionBody {
    actor: Option<String>,
}

async fn cancel_question(
    State(st): State<AppState>,
    Path(comment_id): Path<i64>,
    Json(b): Json<CancelQuestionBody>,
) -> ApiResult {
    Ok(Json(
        core::cancel_question(&st.pool, comment_id, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct SupersedeQuestionBody {
    /// The prompt for the replacement question (the old one is kept immutable + linked).
    new_prompt: String,
    actor: Option<String>,
}

async fn supersede_question(
    State(st): State<AppState>,
    Path(comment_id): Path<i64>,
    Json(b): Json<SupersedeQuestionBody>,
) -> ApiResult {
    Ok(Json(
        core::supersede_question(&st.pool, comment_id, &b.new_prompt, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct BlockingMeQuery {
    /// The principal to view for; a team-routed question surfaces for its members.
    viewer: String,
    project_id: Option<i64>,
    #[serde(default)]
    include_archived: bool,
}

async fn list_tasks_blocking_me(
    State(st): State<AppState>,
    Query(q): Query<BlockingMeQuery>,
) -> ApiResult {
    Ok(Json(
        core::list_tasks_blocking_me(&st.pool, &q.viewer, q.project_id, q.include_archived).await?,
    ))
}

async fn set_task_props(
    State(st): State<AppState>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Json(props): Json<Value>,
) -> ApiResult {
    Ok(Json(core::set_task_props(&st.pool, task_id, props).await?))
}

#[derive(Deserialize, JsonSchema)]
struct MoveTaskBody {
    to_project_id: i64,
    actor: Option<String>,
}

async fn move_task(
    State(st): State<AppState>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Json(b): Json<MoveTaskBody>,
) -> ApiResult {
    Ok(Json(
        core::move_task(&st.pool, task_id, b.to_project_id, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct ArchiveTaskBody {
    /// The agent performing the archive/restore (for the event actor).
    actor: Option<String>,
}

async fn archive_task(
    State(st): State<AppState>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Json(b): Json<ArchiveTaskBody>,
) -> ApiResult {
    Ok(Json(
        core::set_task_archived(&st.pool, task_id, true, b.actor.as_deref()).await?,
    ))
}

async fn restore_task(
    State(st): State<AppState>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Json(b): Json<ArchiveTaskBody>,
) -> ApiResult {
    Ok(Json(
        core::set_task_archived(&st.pool, task_id, false, b.actor.as_deref()).await?,
    ))
}

// --- Subscriptions ---

#[derive(Deserialize, JsonSchema)]
struct SubscribeBody {
    subscriber: String,
    task_id: Option<i64>,
    project_id: Option<i64>,
    /// Subscribe to a channel (join it). Give exactly one of task_id / project_id / channel_id /
    /// document_id, or set `board: true`.
    channel_id: Option<i64>,
    /// Subscribe to a document (its versions + review activity).
    document_id: Option<i64>,
    /// Whole-board firehose: subscribe to EVERY event on the board (for a coordinator/auto-assigner).
    board: Option<bool>,
    /// Optional event-class filter (#462): a subset of ["created", "done", "blocked", "status",
    /// "comment", "assigned", "review", "doc"]. When given, this subscription is delivery-gated to
    /// just those classes (only they reach the inbox and wake the subscriber); omit for every event.
    /// Applies to any target. (Ignored by unsubscribe.)
    event_classes: Option<Vec<String>>,
    /// Subscribe to a channel THREAD (#438): the root is a channel post's event seq. Delivers +
    /// wakes on in-thread follow-ups (reply_to = this root) without a re-mention. When set, takes
    /// precedence over the other targets (and is the target for unsubscribe too).
    thread_root: Option<i64>,
}

async fn subscribe(State(st): State<AppState>, Json(b): Json<SubscribeBody>) -> ApiResult {
    let board = b.board.unwrap_or(false);
    let out = match (b.thread_root, b.event_classes.as_deref()) {
        (Some(root), _) => core::subscribe_thread(&st.pool, &b.subscriber, root).await?,
        (None, Some(ec)) if !ec.is_empty() => {
            core::subscribe_classed(
                &st.pool,
                &b.subscriber,
                b.task_id,
                b.project_id,
                b.channel_id,
                b.document_id,
                board,
                ec,
            )
            .await?
        }
        _ => {
            core::subscribe(
                &st.pool,
                &b.subscriber,
                b.task_id,
                b.project_id,
                b.channel_id,
                b.document_id,
                board,
            )
            .await?
        }
    };
    Ok(Json(out))
}

async fn unsubscribe(State(st): State<AppState>, Json(b): Json<SubscribeBody>) -> ApiResult {
    if let Some(root) = b.thread_root {
        return Ok(Json(
            core::unsubscribe_thread(&st.pool, &b.subscriber, root).await?,
        ));
    }
    Ok(Json(
        core::unsubscribe(
            &st.pool,
            &b.subscriber,
            b.task_id,
            b.project_id,
            b.channel_id,
            b.document_id,
            b.board.unwrap_or(false),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct MuteTaskBody {
    /// The agent muting/unmuting the task (detaches this agent from the task's fan-out).
    agent: String,
}

async fn mute_task(
    State(st): State<AppState>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Json(b): Json<MuteTaskBody>,
) -> ApiResult {
    Ok(Json(core::mute_task(&st.pool, &b.agent, task_id).await?))
}

async fn unmute_task(
    State(st): State<AppState>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Json(b): Json<MuteTaskBody>,
) -> ApiResult {
    Ok(Json(core::unmute_task(&st.pool, &b.agent, task_id).await?))
}

// --- Channels ---

#[derive(Deserialize, JsonSchema)]
struct ListChannelsQuery {
    /// If set, list channels this agent is a member of (incl. private/DM).
    member: Option<String>,
}

async fn list_channels(
    State(st): State<AppState>,
    Query(q): Query<ListChannelsQuery>,
) -> ApiResult {
    Ok(Json(
        core::list_channels(&st.pool, q.member.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct CreateChannelBody {
    name: String,
    topic: Option<String>,
    created_by: Option<String>,
    metadata: Option<Value>,
}

async fn create_channel(State(st): State<AppState>, Json(b): Json<CreateChannelBody>) -> ApiResult {
    Ok(Json(
        core::create_channel(
            &st.pool,
            &b.name,
            b.topic.as_deref(),
            b.created_by.as_deref(),
            b.metadata,
        )
        .await?,
    ))
}

async fn get_channel(
    State(st): State<AppState>,
    Path(ChannelRef(channel_id)): Path<ChannelRef>,
) -> ApiResult {
    found(core::get_channel(&st.pool, channel_id).await?)
}

#[derive(Deserialize, JsonSchema)]
struct ChannelPostsQuery {
    #[serde(default)]
    since_seq: i64,
    #[serde(default = "default_events_limit")]
    limit: i64,
    /// Upper bound: only posts with `seq < before_seq`. For a "load earlier" page, pass the oldest
    /// seq you already have (with `order=desc`) to get the N posts just before it.
    before_seq: Option<i64>,
    /// `asc` (default, oldest-first — scrollback / incremental pollers) or `desc` (newest-first, so
    /// `since_seq=0&limit=N` returns the LATEST N posts — a chat view).
    order: Option<String>,
}

async fn get_channel_posts(
    State(st): State<AppState>,
    Path(ChannelRef(channel_id)): Path<ChannelRef>,
    Query(q): Query<ChannelPostsQuery>,
) -> ApiResult {
    let desc = q.order.as_deref() == Some("desc");
    Ok(Json(
        core::get_channel_posts(
            &st.pool,
            channel_id,
            q.since_seq,
            q.before_seq,
            q.limit,
            desc,
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct PostToChannelBody {
    sender: String,
    body: String,
    reply_to: Option<i64>,
    /// Optional external identity id (e.g. "slack:U123") this post is attributed to — for an
    /// ingested human author. `sender` stays the fleet agent that performed the write.
    external_author: Option<String>,
    /// Optional per-post metadata bag stored on the post (e.g. a bridge's {slack_ts, slack_channel,
    /// thread_ts}). Surfaced on the post + on channel.outbound_reflect; a reply's reflect also
    /// carries the parent post's metadata as `parent_metadata` for stateless threading.
    metadata: Option<Value>,
}

async fn post_to_channel(
    State(st): State<AppState>,
    Path(ChannelRef(channel_id)): Path<ChannelRef>,
    Json(b): Json<PostToChannelBody>,
) -> ApiResult {
    Ok(Json(
        core::post_to_channel_meta(
            &st.pool,
            channel_id,
            &b.sender,
            &b.body,
            b.reply_to,
            b.external_author.as_deref(),
            b.metadata,
        )
        .await?,
    ))
}

async fn set_channel_props(
    State(st): State<AppState>,
    Path(ChannelRef(channel_id)): Path<ChannelRef>,
    Json(props): Json<Value>,
) -> ApiResult {
    Ok(Json(
        core::set_channel_props(&st.pool, channel_id, props).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct SetChannelAutoJoinBody {
    /// true = every agent is a member (existing joined now + new agents auto-join on register);
    /// false = stop auto-joining (existing members stay).
    auto_join: bool,
    actor: Option<String>,
}

async fn set_channel_auto_join(
    State(st): State<AppState>,
    Path(ChannelRef(channel_id)): Path<ChannelRef>,
    Json(b): Json<SetChannelAutoJoinBody>,
) -> ApiResult {
    Ok(Json(
        core::set_channel_auto_join(&st.pool, channel_id, b.auto_join, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct PromoteThreadBody {
    /// Seq of the thread's root post; its direct replies (reply_to == this) become comments.
    root_post_seq: i64,
    /// Project the new task is created in.
    project_id: i64,
    actor: Option<String>,
}

async fn promote_thread(
    State(st): State<AppState>,
    Path(ChannelRef(channel_id)): Path<ChannelRef>,
    Json(b): Json<PromoteThreadBody>,
) -> ApiResult {
    Ok(Json(
        core::promote_thread(
            &st.pool,
            channel_id,
            b.root_post_seq,
            b.project_id,
            b.actor.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct InviteChannelBody {
    agent_id: String,
    invited_by: Option<String>,
}

async fn invite_to_channel(
    State(st): State<AppState>,
    Path(ChannelRef(channel_id)): Path<ChannelRef>,
    Json(b): Json<InviteChannelBody>,
) -> ApiResult {
    Ok(Json(
        core::invite_to_channel(&st.pool, channel_id, &b.agent_id, b.invited_by.as_deref()).await?,
    ))
}

// --- Messages / events ---

#[derive(Deserialize, JsonSchema)]
struct SendMessageBody {
    from_agent: String,
    to_agent: String,
    body: String,
}

async fn send_message(State(st): State<AppState>, Json(b): Json<SendMessageBody>) -> ApiResult {
    Ok(Json(
        core::send_message(&st.pool, &b.from_agent, &b.to_agent, &b.body).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct OpenDmBody {
    /// One side of the 1:1 DM.
    agent_a: String,
    /// The other side. Order doesn't matter — (a,b) resolves to the same channel as (b,a).
    agent_b: String,
}

async fn open_dm(State(st): State<AppState>, Json(b): Json<OpenDmBody>) -> ApiResult {
    Ok(Json(
        core::get_or_create_dm(&st.pool, &b.agent_a, &b.agent_b).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct EventsQuery {
    #[serde(default)]
    since_seq: i64,
    #[serde(default = "default_events_limit")]
    limit: i64,
    /// Only events whose `actor` matches — a complete per-agent activity feed.
    actor: Option<String>,
    /// `asc` (default, oldest-first — incremental pollers) or `desc` (newest-first, so
    /// `since_seq=0&limit=N` returns the LATEST N events — a live activity feed).
    order: Option<String>,
}

async fn get_events(State(st): State<AppState>, Query(q): Query<EventsQuery>) -> ApiResult {
    let desc = q.order.as_deref() == Some("desc");
    Ok(Json(
        core::get_events(&st.pool, q.since_seq, q.limit, q.actor.as_deref(), desc).await?,
    ))
}

// --- External identities (bridged actors) ---

#[derive(Deserialize)]
struct ListExternalIdentitiesQuery {
    source: Option<String>,
}

async fn list_external_identities(
    State(st): State<AppState>,
    Query(q): Query<ListExternalIdentitiesQuery>,
) -> ApiResult {
    Ok(Json(
        core::list_external_identities(&st.pool, q.source.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct UpsertExternalIdentityBody {
    /// Namespaced id `source:handle`, e.g. "slack:U123ABC". Idempotent upsert.
    id: String,
    /// Originating system, e.g. "slack" or "github".
    source: String,
    display_name: Option<String>,
    /// Arbitrary props (avatar, real name, ...). MERGED into any existing bag.
    metadata: Option<Value>,
}

async fn upsert_external_identity(
    State(st): State<AppState>,
    Json(b): Json<UpsertExternalIdentityBody>,
) -> ApiResult {
    Ok(Json(
        core::upsert_external_identity(
            &st.pool,
            &b.id,
            &b.source,
            b.display_name.as_deref(),
            b.metadata,
        )
        .await?,
    ))
}

async fn list_workspace_kinds(State(st): State<AppState>) -> ApiResult {
    Ok(Json(core::list_workspace_kinds(&st.pool).await?))
}

#[derive(Deserialize, JsonSchema)]
struct SetWorkspaceKindBody {
    /// The kind key an agent's `metadata.workspace_kind` references.
    name: String,
    /// The script fleet spin-up runs to materialize the workspace. Omit to keep the stored one.
    setup_script: Option<String>,
    /// Hints the consumer reads. Canonical keys: `cwd` (launch dir after setup), `pre_trust`
    /// (extra trusted paths), `env` (env map for the launched agent); other keys are free-form.
    /// MERGED into any existing bag.
    config: Option<Value>,
    description: Option<String>,
    created_by: Option<String>,
}

async fn set_workspace_kind(
    State(st): State<AppState>,
    Json(b): Json<SetWorkspaceKindBody>,
) -> ApiResult {
    Ok(Json(
        core::set_workspace_kind(
            &st.pool,
            &b.name,
            b.setup_script.as_deref(),
            b.config,
            b.description.as_deref(),
            b.created_by.as_deref(),
        )
        .await?,
    ))
}

async fn get_workspace_kind(State(st): State<AppState>, Path(name): Path<String>) -> ApiResult {
    found(core::get_workspace_kind(&st.pool, &name).await?)
}

async fn delete_workspace_kind(State(st): State<AppState>, Path(name): Path<String>) -> ApiResult {
    Ok(Json(core::delete_workspace_kind(&st.pool, &name).await?))
}

#[derive(Deserialize, JsonSchema)]
struct AddBannedPhraseBody {
    /// The phrase to ban (stored lowercased; matched case-insensitively, whole-phrase).
    phrase: String,
    /// Optional note: why it's banned, or what to write instead.
    note: Option<String>,
    created_by: Option<String>,
}

async fn add_banned_phrase(
    State(st): State<AppState>,
    Json(b): Json<AddBannedPhraseBody>,
) -> ApiResult {
    Ok(Json(
        core::add_banned_phrase(
            &st.pool,
            &b.phrase,
            b.note.as_deref(),
            b.created_by.as_deref(),
        )
        .await?,
    ))
}

async fn list_banned_phrases(State(st): State<AppState>) -> ApiResult {
    Ok(Json(core::list_banned_phrases(&st.pool).await?))
}

#[derive(Deserialize, JsonSchema)]
struct LintTextBody {
    /// The text to check against the live content gate (banned-phrase list + ASCII-only rule).
    text: String,
}

/// Dry-run the pre-submit content lint without writing anything: returns every finding
/// (`{clean, banned_phrases, non_ascii}`) against the authoritative live list, so authors verify
/// here instead of a hand-maintained local copy that drifts.
async fn lint_text(State(st): State<AppState>, Json(b): Json<LintTextBody>) -> ApiResult {
    Ok(Json(core::lint_text(&st.pool, &b.text).await?))
}

// --- Identity aliases (task 532) ---

#[derive(Deserialize, JsonSchema)]
struct SetIdentityAliasBody {
    /// The alias to map (stored lowercased; the lookup key), e.g. "operator".
    alias: String,
    /// The canonical identity it resolves to, e.g. "cameron".
    canonical: String,
    created_by: Option<String>,
}

async fn list_identity_aliases(State(st): State<AppState>) -> ApiResult {
    Ok(Json(core::list_identity_aliases(&st.pool).await?))
}

async fn set_identity_alias(
    State(st): State<AppState>,
    Json(b): Json<SetIdentityAliasBody>,
) -> ApiResult {
    Ok(Json(
        core::set_identity_alias(&st.pool, &b.alias, &b.canonical, b.created_by.as_deref()).await?,
    ))
}

// --- People / teams (multi-operator model, task 542 Phase 1) ---

#[derive(Deserialize, JsonSchema)]
struct CreatePersonBody {
    /// Stable string handle for the person (e.g. "cameron"). Upserts if it already exists.
    id: String,
    display_name: Option<String>,
    created_by: Option<String>,
    metadata: Option<Value>,
}

async fn list_people(State(st): State<AppState>) -> ApiResult {
    Ok(Json(core::list_people(&st.pool).await?))
}

async fn create_person(State(st): State<AppState>, Json(b): Json<CreatePersonBody>) -> ApiResult {
    Ok(Json(
        core::create_person(
            &st.pool,
            &b.id,
            b.display_name.as_deref(),
            b.created_by.as_deref(),
            b.metadata,
        )
        .await?,
    ))
}

async fn delete_person(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(core::delete_person(&st.pool, &id).await?))
}

#[derive(Deserialize, JsonSchema)]
struct CreateTeamBody {
    /// Stable string handle for the team (e.g. "operator"). Upserts if it already exists.
    id: String,
    display_name: Option<String>,
    created_by: Option<String>,
    metadata: Option<Value>,
}

async fn list_teams(State(st): State<AppState>) -> ApiResult {
    Ok(Json(core::list_teams(&st.pool).await?))
}

async fn create_team(State(st): State<AppState>, Json(b): Json<CreateTeamBody>) -> ApiResult {
    Ok(Json(
        core::create_team(
            &st.pool,
            &b.id,
            b.display_name.as_deref(),
            b.created_by.as_deref(),
            b.metadata,
        )
        .await?,
    ))
}

async fn get_team(State(st): State<AppState>, Path(team_id): Path<String>) -> ApiResult {
    Ok(Json(core::get_team(&st.pool, &team_id).await?))
}

async fn delete_team(State(st): State<AppState>, Path(team_id): Path<String>) -> ApiResult {
    Ok(Json(core::delete_team(&st.pool, &team_id).await?))
}

#[derive(Deserialize, JsonSchema)]
struct TeamMemberBody {
    /// The member's id: a person id, a team id, or an agent id (per member_kind).
    member_id: String,
    /// "person", "team", or "agent" (team-scoped agents, task 542).
    member_kind: String,
    created_by: Option<String>,
}

async fn add_team_member(
    State(st): State<AppState>,
    Path(team_id): Path<String>,
    Json(b): Json<TeamMemberBody>,
) -> ApiResult {
    Ok(Json(
        core::add_team_member(
            &st.pool,
            &team_id,
            &b.member_id,
            &b.member_kind,
            b.created_by.as_deref(),
        )
        .await?,
    ))
}

async fn remove_team_member(
    State(st): State<AppState>,
    Path(team_id): Path<String>,
    Json(b): Json<TeamMemberBody>,
) -> ApiResult {
    Ok(Json(
        core::remove_team_member(&st.pool, &team_id, &b.member_id, &b.member_kind).await?,
    ))
}

async fn remove_banned_phrase(State(st): State<AppState>, Path(phrase): Path<String>) -> ApiResult {
    Ok(Json(core::remove_banned_phrase(&st.pool, &phrase).await?))
}

// --- Secret requests (ephemeral secret-request broker, task 272) ---

#[derive(Deserialize, JsonSchema)]
struct CreateSecretRequestBody {
    /// The secret's name (e.g. the durable filename it will land as).
    name: String,
    /// Age recipient public keys (non-secret) the browser encrypts the value to. For a
    /// host-bound secret, include the recovery/user keys too, not just the host key.
    #[serde(default)]
    recipients: Vec<String>,
    /// Human instructions shown on the submit page (what the value is, where to obtain it).
    instructions: Option<String>,
    /// Advisory placement hint for the fulfiller (the durable path + any wiring note).
    target: Option<String>,
    /// The agent to directly notify on submit + whose token gates the ciphertext pull.
    fulfiller: Option<String>,
    /// The requesting agent (for the audit event).
    requested_by: Option<String>,
}

async fn create_secret_request(
    State(st): State<AppState>,
    Json(b): Json<CreateSecretRequestBody>,
) -> ApiResult {
    Ok(Json(
        core::create_secret_request(
            &st.pool,
            &b.name,
            &b.recipients,
            b.instructions.as_deref(),
            b.target.as_deref(),
            b.fulfiller.as_deref(),
            b.requested_by.as_deref(),
        )
        .await?,
    ))
}

async fn list_secret_requests(State(st): State<AppState>) -> ApiResult {
    Ok(Json(core::list_secret_requests(&st.pool).await?))
}

async fn get_secret_request(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    Ok(Json(core::get_secret_request(&st.pool, id).await?))
}

#[derive(Deserialize, JsonSchema)]
struct SubmitSecretBody {
    /// The single-use submit capability token from the submit link.
    token: String,
    /// The browser-encrypted ciphertext (the board never receives plaintext).
    ciphertext: String,
}

async fn submit_secret(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Json(b): Json<SubmitSecretBody>,
) -> ApiResult {
    Ok(Json(
        core::submit_secret(&st.pool, id, &b.token, &b.ciphertext).await?,
    ))
}

#[derive(Deserialize)]
struct FulfillerTokenQuery {
    token: String,
}

async fn get_secret_ciphertext(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<FulfillerTokenQuery>,
) -> ApiResult {
    Ok(Json(
        core::get_secret_ciphertext(&st.pool, id, &q.token).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct FulfillSecretBody {
    /// The fulfiller capability token.
    token: String,
}

async fn fulfill_secret(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Json(b): Json<FulfillSecretBody>,
) -> ApiResult {
    Ok(Json(core::fulfill_secret(&st.pool, id, &b.token).await?))
}

#[derive(Deserialize, JsonSchema)]
struct CancelSecretBody {
    /// The agent cancelling the request (for the audit event).
    actor: Option<String>,
}

async fn cancel_secret_request(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Json(b): Json<CancelSecretBody>,
) -> ApiResult {
    Ok(Json(
        core::cancel_secret_request(&st.pool, id, b.actor.as_deref()).await?,
    ))
}

// --- Reviews (Document #5, increment 1: a typed review over an artifact, A2 lifecycle + log) ---

#[derive(Deserialize, JsonSchema)]
struct CreateReviewBody {
    /// What is being reviewed: document | code | design | agent-session | task.
    kind: String,
    /// Where the artifact lives (board-document, github-pull-request, url, agent-session, task).
    /// Metadata — the board never dereferences it.
    source: Option<String>,
    /// A pointer to the artifact within its source (a doc id, a PR url, a change-request id, ...).
    target_ref: Option<String>,
    /// A short title for the review.
    title: Option<String>,
    /// Initial A2 status; defaults to `open`. open / in_review / changes_requested / approved / closed.
    status: Option<String>,
    /// The agent that created/produced the review.
    created_by: Option<String>,
    /// The reviewer(s) assigned (a single agent id in increment 1).
    assignee: Option<String>,
    /// Arbitrary properties: producing agent id, predecessor review id, tags, ...
    metadata: Option<Value>,
    /// Optional external reference for idempotent ingest: a review already linked on
    /// (source, external_id) is returned (`created:false`) instead of a duplicate.
    external_link: Option<core::ExternalRef>,
}

async fn create_review(State(st): State<AppState>, Json(b): Json<CreateReviewBody>) -> ApiResult {
    Ok(Json(
        core::create_review(
            &st.pool,
            &b.kind,
            b.source.as_deref(),
            b.target_ref.as_deref(),
            b.title.as_deref(),
            b.status.as_deref(),
            b.created_by.as_deref(),
            b.assignee.as_deref(),
            b.metadata,
            b.external_link,
        )
        .await?,
    ))
}

#[derive(Deserialize)]
struct ListReviewsQuery {
    status: Option<String>,
    kind: Option<String>,
    assignee: Option<String>,
}

async fn list_reviews(State(st): State<AppState>, Query(q): Query<ListReviewsQuery>) -> ApiResult {
    Ok(Json(
        core::list_reviews(
            &st.pool,
            q.status.as_deref(),
            q.kind.as_deref(),
            q.assignee.as_deref(),
        )
        .await?,
    ))
}

async fn get_review(State(st): State<AppState>, Path(review_id): Path<i64>) -> ApiResult {
    found(core::get_review(&st.pool, review_id).await?)
}

#[derive(Deserialize)]
struct ReviewTrendQuery {
    kind: Option<String>,
    area: Option<String>,
}

async fn review_trend(State(st): State<AppState>, Query(q): Query<ReviewTrendQuery>) -> ApiResult {
    Ok(Json(
        core::review_improvement_trend(&st.pool, q.kind.as_deref(), q.area.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct SetReviewStatusBody {
    /// The new A2 status: open / in_review / changes_requested / approved / closed. Re-applying
    /// the current status is an idempotent no-op.
    status: String,
    /// The agent making the transition.
    actor: Option<String>,
    /// An optional note recorded on the state-change log entry.
    note: Option<String>,
}

async fn set_review_status(
    State(st): State<AppState>,
    Path(review_id): Path<i64>,
    Json(b): Json<SetReviewStatusBody>,
) -> ApiResult {
    Ok(Json(
        core::set_review_status(
            &st.pool,
            review_id,
            &b.status,
            b.actor.as_deref(),
            b.note.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct SetReviewVettedBody {
    /// true = mark the review vetted (adversarial review run + addressed); false = clear it.
    vetted: bool,
    /// The agent setting the flag (recorded for audit).
    actor: Option<String>,
    /// An optional note recorded on the audit log entry.
    note: Option<String>,
}

async fn set_review_vetted(
    State(st): State<AppState>,
    Path(review_id): Path<i64>,
    Json(b): Json<SetReviewVettedBody>,
) -> ApiResult {
    Ok(Json(
        core::set_review_vetted(
            &st.pool,
            review_id,
            b.vetted,
            b.actor.as_deref(),
            b.note.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct AppendReviewLogBody {
    /// The entry type: submitted / revised / finding / finding_resolved / comment / state_change /
    /// adversarial_review / decision.
    entry_type: String,
    /// The entry text.
    body: Option<String>,
    /// The author of this entry.
    author: Option<String>,
    /// For an actionable `finding`: the id of the child task tracking the fix.
    task_id: Option<i64>,
    /// Optional external id for idempotent ingest: an entry already logged under this external_id
    /// on the review is returned (`appended:false`) instead of a duplicate.
    external_id: Option<String>,
}

async fn append_review_log(
    State(st): State<AppState>,
    Path(review_id): Path<i64>,
    Json(b): Json<AppendReviewLogBody>,
) -> ApiResult {
    Ok(Json(
        core::append_review_log(
            &st.pool,
            review_id,
            &b.entry_type,
            b.body.as_deref(),
            b.author.as_deref(),
            b.task_id,
            b.external_id.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize)]
struct ListExternalLinksQuery {
    source: Option<String>,
    board_kind: Option<String>,
    board_id: Option<i64>,
}

async fn list_external_links(
    State(st): State<AppState>,
    Query(q): Query<ListExternalLinksQuery>,
) -> ApiResult {
    Ok(Json(
        core::list_external_links(
            &st.pool,
            q.source.as_deref(),
            q.board_kind.as_deref(),
            q.board_id,
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct UpsertExternalLinkBody {
    /// Originating system, e.g. "slack" or "github".
    source: String,
    /// The external system's canonical key (Slack channel id, thread ts, issue url, ...).
    external_id: String,
    /// Optional external container (e.g. the Slack channel of a thread).
    external_parent_id: Option<String>,
    /// Board entity kind: "channel", "task", or "thread".
    board_kind: String,
    /// Board-side id (channel id / task id / thread root post seq).
    board_id: i64,
    /// Arbitrary props. MERGED into any existing bag.
    metadata: Option<Value>,
}

async fn upsert_external_link(
    State(st): State<AppState>,
    Json(b): Json<UpsertExternalLinkBody>,
) -> ApiResult {
    Ok(Json(
        core::upsert_external_link(
            &st.pool,
            &b.source,
            &b.external_id,
            b.external_parent_id.as_deref(),
            &b.board_kind,
            b.board_id,
            b.metadata,
        )
        .await?,
    ))
}

// --- Content-addressing ---

#[derive(Deserialize, JsonSchema)]
struct IpfsAddBody {
    /// Raw content to content-address. The board pins it via the configured IPFS backend and
    /// returns the resulting CID — so a client with no local IPFS can obtain a CID to hand to
    /// create_document / publish_version. Requires the deployment to set `ipfs_api_url`.
    content: String,
}

/// `POST /api/ipfs/add` — content-address raw `content` server-side and return its CID.
///
/// This is a deliberately *scoped, add-only* capability over the configured IPFS backend: the
/// only operation exposed is "pin these bytes, give me the CID". It never proxies the raw Kubo
/// RPC (which also carries pin-management / config / shutdown), so exposing this publicly is
/// just an open *add* endpoint, not an open node. Requires `ipfs_api_url`; without a backend it
/// returns 503 (the deployment hasn't enabled server-side content-addressing).
async fn ipfs_add(State(st): State<AppState>, Json(b): Json<IpfsAddBody>) -> ApiResult {
    let Some(url) = st.ipfs_api_url.as_deref() else {
        return Err(ApiError(anyhow::anyhow!(
            "no IPFS backend configured (set ipfs_api_url); this board can't content-address content server-side"
        )));
    };
    let cid = ipfs::add(url, b.content.into_bytes()).await?;
    Ok(Json(json!({ "cid": cid })))
}

#[derive(Deserialize)]
struct IpfsCatQuery {
    /// Content-Type to label the response with — the board never sniffs bytes; the client
    /// knows the type from the document version's `content_type`. Default application/octet-stream.
    content_type: Option<String>,
}

/// Cap on a single read-gateway response, so the board never buffers a runaway blob.
const IPFS_READ_CAP_BYTES: usize = 25 * 1024 * 1024;

/// `GET /api/ipfs/{cid}` — read content back by CID through the configured IPFS backend. The
/// READ half of the scoped CID-only exception (see `ipfs_add`): it lets the same-origin web app
/// fetch a document's bytes to render them, with no separate IPFS gateway or CORS. Deliberately
/// scoped to `cat` by CID — it never proxies the node's RPC. 503 without a backend, 400 on a
/// junk CID. CIDs are immutable, so the response is aggressively cacheable. The caller passes the
/// content-type it already knows via `?content_type=` (the board does not sniff bytes).
async fn ipfs_cat(
    State(st): State<AppState>,
    Path(cid): Path<String>,
    Query(q): Query<IpfsCatQuery>,
) -> Result<Response, ApiError> {
    let Some(url) = st.ipfs_api_url.as_deref() else {
        return Err(ApiError(anyhow::anyhow!(
            "no IPFS backend configured (set ipfs_api_url); this board can't read content by CID"
        )));
    };
    if !ipfs::is_probable_cid(&cid) {
        return Err(ApiError(anyhow::anyhow!(
            "give a valid `cid` (a bare content id)"
        )));
    }
    let bytes = ipfs::cat(url, &cid, IPFS_READ_CAP_BYTES).await?;
    let ct = q
        .content_type
        .as_deref()
        .and_then(|s| axum::http::HeaderValue::from_str(s).ok())
        .unwrap_or_else(|| axum::http::HeaderValue::from_static("application/octet-stream"));
    let mut resp = Response::new(axum::body::Body::from(bytes));
    resp.headers_mut()
        .insert(axum::http::header::CONTENT_TYPE, ct);
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    Ok(resp)
}

// --- Documents ---

#[derive(Deserialize)]
struct ListDocumentsQuery {
    project_id: Option<i64>,
    status: Option<String>,
    tag: Option<String>,
    task_id: Option<i64>,
    author: Option<String>,
    /// Include archived (retired) documents; hidden by default.
    #[serde(default)]
    include_archived: bool,
}

#[derive(Deserialize)]
struct WikiQuery {
    prefix: Option<String>,
    /// Include archived (retired) documents in the tree; hidden by default.
    #[serde(default)]
    include_archived: bool,
}

async fn list_wiki(State(st): State<AppState>, Query(q): Query<WikiQuery>) -> ApiResult {
    Ok(Json(
        core::list_wiki(&st.pool, q.prefix.as_deref(), q.include_archived).await?,
    ))
}

async fn list_documents(
    State(st): State<AppState>,
    Query(q): Query<ListDocumentsQuery>,
) -> ApiResult {
    Ok(Json(
        core::list_documents(
            &st.pool,
            q.project_id,
            q.status.as_deref(),
            q.tag.as_deref(),
            q.task_id,
            q.author.as_deref(),
            q.include_archived,
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct CreateDocumentBody {
    title: String,
    /// Bare IPFS content id for version 1. Stored verbatim; the board never resolves it.
    /// Optional if `content` is given (and the board has an IPFS backend configured).
    cid: Option<String>,
    /// Raw content for version 1, content-addressed server-side when no `cid` is given. Needs
    /// a configured IPFS backend; lets a client with no local IPFS author a document. Supply
    /// exactly one of `cid` / `content`.
    content: Option<String>,
    project_id: Option<i64>,
    summary: Option<String>,
    created_by: Option<String>,
    metadata: Option<Value>,
    /// MIME type of v1's bytes (default text/markdown). The board records only the label.
    content_type: Option<String>,
    /// Submit even if the content contains a banned phrase (the pre-submit lint otherwise rejects
    /// it). Text content is scanned; non-text content is not.
    acknowledge_banned: Option<bool>,
}

async fn create_document(
    State(st): State<AppState>,
    Json(b): Json<CreateDocumentBody>,
) -> ApiResult {
    if let Some(c) = b.content.as_deref() {
        if core::is_text_content_type(b.content_type.as_deref().unwrap_or("text/markdown")) {
            core::check_content(&st.pool, c, b.acknowledge_banned.unwrap_or(false)).await?;
        }
    }
    let cid = ipfs::resolve_cid(
        b.cid.as_deref(),
        b.content.as_deref(),
        st.ipfs_api_url.as_deref(),
    )
    .await?;
    // Published by CID (no inline content the check above could see): fetch + gate the bytes (task 564).
    if b.content.is_none() {
        core::check_cid_content(
            &st.pool,
            st.ipfs_api_url.as_deref(),
            &cid,
            b.content_type.as_deref().unwrap_or("text/markdown"),
            b.acknowledge_banned.unwrap_or(false),
        )
        .await?;
    }
    Ok(Json(
        core::create_document(
            &st.pool,
            &b.title,
            b.project_id,
            &cid,
            b.summary.as_deref(),
            b.created_by.as_deref(),
            b.metadata,
            b.content_type.as_deref(),
            b.content.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct GetDocumentQuery {
    /// When true, also inline the current version's markdown, fetched server-side from its pinned
    /// CID. Omit/false for metadata only. A fetch failure leaves `body: null` + a `body_error`.
    #[serde(default)]
    include_body: bool,
}

async fn get_document(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Query(q): Query<GetDocumentQuery>,
) -> ApiResult {
    Ok(Json(
        core::get_document_with_body(
            &st.pool,
            st.ipfs_api_url.as_deref(),
            document_id,
            q.include_body,
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct UpdateDocumentBody {
    /// New title — a short, specific noun phrase; the viewer renders the title as the page header.
    title: String,
    actor: Option<String>,
}

async fn update_document(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<UpdateDocumentBody>,
) -> ApiResult {
    Ok(Json(
        core::update_document(&st.pool, document_id, &b.title, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct DocumentContentQuery {
    /// Which version's body to read. Omit for the current version.
    version_no: Option<i64>,
}

/// `GET /api/documents/{id}/content` — read a document's body inline (resolves the version's CID
/// through the board's IPFS backend server-side). The agent-usable read path: no local IPFS or
/// separate gateway. Text content comes back as `content`; binary content returns a null `content`
/// + the CID to fetch via `/api/ipfs/{cid}`. Requires `ipfs_api_url` (503 without one).
async fn read_document_content(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Query(q): Query<DocumentContentQuery>,
) -> ApiResult {
    Ok(Json(
        core::read_document_content(
            &st.pool,
            st.ipfs_api_url.as_deref(),
            document_id,
            q.version_no,
        )
        .await?,
    ))
}

async fn get_document_versions(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
) -> ApiResult {
    Ok(Json(
        core::get_document_versions(&st.pool, document_id).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct PublishVersionBody {
    /// Bare IPFS content id for the new version. Stored verbatim; the board never resolves it.
    /// Optional if `content` is given (and the board has an IPFS backend configured).
    cid: Option<String>,
    /// Raw content for the new version, content-addressed server-side when no `cid` is given.
    /// Supply exactly one of `cid` / `content`.
    content: Option<String>,
    summary: Option<String>,
    created_by: Option<String>,
    /// MIME type of this version's bytes (default text/markdown). The board records only the label.
    content_type: Option<String>,
    /// Submit even if the content contains a banned phrase (the pre-submit lint otherwise rejects
    /// it). Text content is scanned; non-text content is not.
    acknowledge_banned: Option<bool>,
}

async fn publish_version(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<PublishVersionBody>,
) -> ApiResult {
    if let Some(c) = b.content.as_deref() {
        if core::is_text_content_type(b.content_type.as_deref().unwrap_or("text/markdown")) {
            core::check_content(&st.pool, c, b.acknowledge_banned.unwrap_or(false)).await?;
        }
    }
    let cid = ipfs::resolve_cid(
        b.cid.as_deref(),
        b.content.as_deref(),
        st.ipfs_api_url.as_deref(),
    )
    .await?;
    // Published by CID (no inline content the check above could see): fetch + gate the bytes (task 564).
    if b.content.is_none() {
        core::check_cid_content(
            &st.pool,
            st.ipfs_api_url.as_deref(),
            &cid,
            b.content_type.as_deref().unwrap_or("text/markdown"),
            b.acknowledge_banned.unwrap_or(false),
        )
        .await?;
    }
    Ok(Json(
        core::publish_version(
            &st.pool,
            document_id,
            &cid,
            b.summary.as_deref(),
            b.created_by.as_deref(),
            b.content_type.as_deref(),
            b.content.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct SetDocumentPathBody {
    /// The wiki path to file this document under (e.g. architecture/board/events). An empty
    /// string clears the path (unfiles the doc). Must be unique among filed documents.
    path: String,
    actor: Option<String>,
}

async fn set_document_path(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<SetDocumentPathBody>,
) -> ApiResult {
    Ok(Json(
        core::set_document_path(&st.pool, document_id, &b.path, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize)]
struct DocumentCommentsQuery {
    version_id: Option<i64>,
    status: Option<String>,
}

async fn get_document_comments(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Query(q): Query<DocumentCommentsQuery>,
) -> ApiResult {
    Ok(Json(
        core::get_document_comments(&st.pool, document_id, q.version_id, q.status.as_deref())
            .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct CommentDocumentBody {
    body: String,
    /// The version this comment is written against (anchors the region to immutable content).
    version_id: Option<i64>,
    author: Option<String>,
    /// Free-form JSON anchor (e.g. W3C/Hypothesis selectors). Omit for a doc-level comment.
    region: Option<Value>,
    /// Thread this comment under another (one-level).
    reply_to: Option<i64>,
    /// Optional external identity id (e.g. "slack:U123") this comment is attributed to — for an
    /// ingested human author. `author` stays the fleet agent that performed the write.
    external_author: Option<String>,
    /// Submit even if the body contains a banned phrase (the pre-submit lint otherwise rejects it).
    acknowledge_banned: Option<bool>,
}

async fn comment_document(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<CommentDocumentBody>,
) -> ApiResult {
    core::check_content(&st.pool, &b.body, b.acknowledge_banned.unwrap_or(false)).await?;
    Ok(Json(
        core::comment_document(
            &st.pool,
            document_id,
            b.version_id,
            b.author.as_deref(),
            &b.body,
            b.region,
            b.reply_to,
            b.external_author.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct ResolveCommentBody {
    actor: Option<String>,
}

async fn resolve_comment(
    State(st): State<AppState>,
    Path((DocRef(_document_id), comment_id)): Path<(DocRef, i64)>,
    Json(b): Json<ResolveCommentBody>,
) -> ApiResult {
    Ok(Json(
        core::resolve_comment(&st.pool, comment_id, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct DocumentActorBody {
    actor: Option<String>,
}

async fn submit_for_review(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<DocumentActorBody>,
) -> ApiResult {
    Ok(Json(
        core::submit_for_review(&st.pool, document_id, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct SubmitToOperatorReviewBody {
    actor: Option<String>,
    /// The doc template you read and followed. Required unless `template_waiver_reason` is given.
    template_followed: Option<String>,
    /// If no template applies, a non-empty reason why. Required only when `template_followed` is absent.
    template_waiver_reason: Option<String>,
}

async fn submit_to_operator_review(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<SubmitToOperatorReviewBody>,
) -> ApiResult {
    Ok(Json(
        core::submit_to_operator_review(
            &st.pool,
            document_id,
            b.actor.as_deref(),
            b.template_followed.as_deref(),
            b.template_waiver_reason.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct RequestChangesBody {
    actor: Option<String>,
    /// Optional note explaining what needs to change.
    note: Option<String>,
}

async fn request_changes(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<RequestChangesBody>,
) -> ApiResult {
    Ok(Json(
        core::request_changes(&st.pool, document_id, b.actor.as_deref(), b.note.as_deref()).await?,
    ))
}

async fn approve_document(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<DocumentActorBody>,
) -> ApiResult {
    Ok(Json(
        core::approve_document(&st.pool, document_id, b.actor.as_deref()).await?,
    ))
}

async fn archive_document(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<DocumentActorBody>,
) -> ApiResult {
    Ok(Json(
        core::set_document_archived(&st.pool, document_id, true, b.actor.as_deref()).await?,
    ))
}

async fn restore_document(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<DocumentActorBody>,
) -> ApiResult {
    Ok(Json(
        core::set_document_archived(&st.pool, document_id, false, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct AttachDocumentBody {
    task_id: i64,
    actor: Option<String>,
}

async fn attach_document(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<AttachDocumentBody>,
) -> ApiResult {
    Ok(Json(
        core::attach_document(&st.pool, document_id, b.task_id, b.actor.as_deref()).await?,
    ))
}

async fn detach_document(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<AttachDocumentBody>,
) -> ApiResult {
    Ok(Json(
        core::detach_document(&st.pool, document_id, b.task_id, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct StreamQuery {
    /// Fallback for `Last-Event-ID` — EventSource can't set headers on the initial
    /// connection, so a client resuming a known position may pass it here instead.
    last_event_id: Option<i64>,
}

/// `GET /api/stream` — Server-Sent Events feed of board activity. Subscribe FIRST, then let
/// the sse layer compute replay, so no event is lost in the gap between the replay snapshot
/// and going live. A reconnecting browser sends the last seq it saw via the `Last-Event-ID`
/// header (native EventSource behavior); we also accept `?last_event_id=` for clients that
/// can't set it.
async fn stream(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<StreamQuery>,
) -> Response {
    let rx = st.events_tx.subscribe();
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<i64>().ok())
        .or(q.last_event_id);
    sse::stream(st.pool.clone(), rx, last_event_id).await
}

fn default_true() -> bool {
    true
}
fn default_limit() -> i64 {
    50
}
fn default_events_limit() -> i64 {
    100
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The router must actually build — guards against an axum route-overlap panic (e.g. the
    /// static `/ipfs/add` vs the param `/ipfs/{cid}`), which would otherwise only surface when
    /// the server boots. Building it here fails the test instead.
    #[tokio::test]
    async fn router_builds_without_route_conflicts() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let (events_tx, _rx) = broadcast::channel(16);
        let _app = router(AppState {
            pool,
            events_tx,
            ipfs_api_url: None,
        });
        Ok(())
    }

    /// Every request-body struct named in `ENDPOINTS` must have a generated schema.
    #[test]
    fn every_body_ref_has_a_schema() {
        let schemas = body_schemas();
        let schemas = schemas.as_object().unwrap();
        for e in ENDPOINTS {
            if let Some(name) = e.body {
                assert!(
                    schemas.contains_key(name),
                    "endpoint {} {} references body schema `{name}`, but body_schemas() has none",
                    e.method,
                    e.path,
                );
            }
        }
    }

    /// `parse_ref` accepts the bare int, the `#N` shorthand, and the typed `<kind>_N` form, and
    /// rejects a wrong-kind prefix / non-numeric / non-positive input (task 504).
    #[test]
    fn parse_ref_accepts_bare_hash_and_typed_forms() {
        assert_eq!(parse_ref("task", "472"), Some(472));
        assert_eq!(parse_ref("task", "#472"), Some(472));
        assert_eq!(parse_ref("task", "task_472"), Some(472));
        assert_eq!(parse_ref("doc", "doc_23"), Some(23));
        assert_eq!(parse_ref("project", "project_16"), Some(16));
        assert_eq!(parse_ref("channel", "channel_123"), Some(123));
        // Wrong-kind typed prefix is rejected so an id can't cross resource types.
        assert_eq!(parse_ref("task", "doc_5"), None);
        assert_eq!(parse_ref("doc", "task_5"), None);
        // Junk / non-positive / partial forms.
        assert_eq!(parse_ref("task", "task_"), None);
        assert_eq!(parse_ref("task", "task_abc"), None);
        assert_eq!(parse_ref("task", "0"), None);
        assert_eq!(parse_ref("task", "-3"), None);
        assert_eq!(parse_ref("task", "abc"), None);
    }

    /// A by-id GET route accepts the id in bare AND typed (`task_<n>`) form via the newtype Path
    /// extractor, and the response carries the typed canonical `ref`; a wrong-kind typed id 404s
    /// (the extractor rejects it -> no route match) (task 504).
    #[tokio::test]
    async fn get_task_accepts_typed_id_and_returns_ref() -> anyhow::Result<()> {
        use tower::ServiceExt;
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = core::create_project(&pool, "P", None, Some("a"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = core::create_task(
            &pool,
            pid,
            "T",
            None,
            None,
            None,
            Some("a"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        let (events_tx, _rx) = broadcast::channel(16);
        let state = AppState {
            pool,
            events_tx,
            ipfs_api_url: None,
        };

        let get = |uri: String| {
            let app = router(state.clone());
            async move {
                app.oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        // Bare int form: 200 + ref == "task_<id>".
        let resp = get(format!("/tasks/{tid}")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
        let v: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(v["id"].as_i64(), Some(tid), "int id retained");
        assert_eq!(v["ref"].as_str(), Some(format!("task_{tid}").as_str()));

        // Typed form: same resource.
        let resp = get(format!("/tasks/task_{tid}")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
        let v: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(v["id"].as_i64(), Some(tid));

        // Wrong-kind typed id on the task route: the extractor rejects it, so it never resolves to
        // a resource -> a 4xx client error (a path-deserialize rejection), never a 200.
        let resp = get(format!("/tasks/doc_{tid}")).await;
        assert!(resp.status().is_client_error(), "got {}", resp.status());
        Ok(())
    }

    /// The health beacon reports 200 when the database is reachable — the signal an agent checks
    /// before a full tick. (When the origin is down the request never reaches this handler and the
    /// proxy returns 502; both non-200s mean "back off".)
    #[tokio::test]
    async fn health_beacon_reports_db_reachable() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let (events_tx, _rx) = broadcast::channel(16);
        let state = AppState {
            pool,
            events_tx,
            ipfs_api_url: None,
        };
        let resp = health(State(state)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        Ok(())
    }

    /// A "no IPFS backend" error (the /api/ipfs/add guard when ipfs_api_url is unset) maps to
    /// 503, not a generic 500 — the feature is unavailable, not faulted.
    #[test]
    fn no_ipfs_backend_maps_to_503() {
        let resp = ApiError(anyhow::anyhow!(
            "no IPFS backend configured (set ipfs_api_url)"
        ))
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// The discovery catalog (`ENDPOINTS`) must match the actual axum router exactly.
    /// axum doesn't expose its route table, so we parse the `.route(...)` lines out of
    /// this file's own source. A new route with no catalog entry — or a stale catalog
    /// entry for a route that's gone — fails the test.
    #[test]
    fn catalog_matches_router() {
        let src = include_str!("api.rs");

        // Slice out the body of `pub fn router(...) { ... }`.
        let start = src.find("pub fn router").expect("router fn");
        let body = &src[start..];
        let end = body.find("\n}").expect("router fn end");
        let body = &body[..end];

        // Each `.route("PATH", get(..).post(..))` contributes one (METHOD, PATH) pair per
        // HTTP-method combinator it names. Split on `.route(` rather than parsing per line so this
        // tolerates rustfmt wrapping a route's path and method combinators across several lines:
        // each segment runs from one `.route(` up to the next, so its method combinators are bounded
        // to that route.
        let mut from_router: BTreeSet<(String, String)> = BTreeSet::new();
        for seg in body.split(".route(").skip(1) {
            let after = seg.split_once('"').expect("opening quote on route path").1;
            let (path, rest) = after.split_once('"').expect("closing quote on route path");
            let full = if path == "/" {
                "/api".to_string()
            } else {
                format!("/api{path}")
            };
            for (kw, method) in [
                ("get(", "GET"),
                ("post(", "POST"),
                ("patch(", "PATCH"),
                ("delete(", "DELETE"),
                ("put(", "PUT"),
            ] {
                if rest.contains(kw) {
                    from_router.insert((method.to_string(), full.clone()));
                }
            }
        }

        let from_catalog: BTreeSet<(String, String)> = ENDPOINTS
            .iter()
            .map(|e| (e.method.to_string(), e.path.to_string()))
            .collect();

        let missing: Vec<_> = from_router.difference(&from_catalog).collect();
        let stale: Vec<_> = from_catalog.difference(&from_router).collect();
        assert!(
            missing.is_empty() && stale.is_empty(),
            "ENDPOINTS is out of sync with router().\n  routes missing from catalog: {missing:?}\n  stale catalog entries: {stale:?}",
        );
    }
}
