//! REST API for humans (and the web UI), in addition to the MCP surface. Same core
//! operations, exposed as JSON over HTTP. Auth is deliberately absent for now (LAN,
//! trust-on-first-use) but the router is structured so a middleware layer can be added
//! cleanly later.

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Extension, Json, Router};
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
    /// Cancelled when the process begins a graceful shutdown, so long-lived `GET /api/stream`
    /// SSE responses end instead of holding `graceful_shutdown` open until the systemd
    /// stop-timeout + SIGKILL (task_753). SSE clients reconnect and replay via Last-Event-ID.
    pub shutdown: tokio_util::sync::CancellationToken,
    /// Path to the SQLite database file, so the DB-snapshot endpoint can write its `VACUUM INTO`
    /// copy beside it (same filesystem, same owning user). See [`AppState::db_snapshot`].
    pub db_path: String,
    /// Config for the authenticated DB-snapshot download endpoint (`GET /api/admin/db-snapshot`).
    /// `enabled` false (the default) 404s the endpoint; when enabled, `user`/`password` gate it with
    /// HTTP Basic auth. See `config::Settings::db_snapshot_enabled`.
    pub db_snapshot: DbSnapshotCfg,
    /// Per-host trusted-front-door auth, keyed by lowercased hostname (port stripped): hostname ->
    /// optional header to force the acting user from. A host mapped to `Some(header)` forces the
    /// identity from that header on writes; an unlisted host, or one mapped to `None`, is permissive
    /// (the client sets its own actor). Empty (default) trusts the client everywhere. Arc so the
    /// per-request `State` clone is cheap. See [`force_trusted_user`] and `config::Settings::hosts`.
    pub host_auth: std::sync::Arc<std::collections::HashMap<String, Option<String>>>,
    /// Deployment-defined link-tag rules (task_1243), served read-only at `GET /api/system/link-rules`
    /// for the UI to linkify custom refs (e.g. CR-NNNN) in rendered content. Empty (default) means
    /// no custom rules. Arc so the per-request `State` clone stays cheap. See `config::Settings::link_rules`.
    pub link_rules: std::sync::Arc<Vec<crate::config::LinkRule>>,
}

/// Config for the authenticated DB-snapshot download endpoint, carried on [`AppState`].
#[derive(Clone, Debug)]
pub struct DbSnapshotCfg {
    pub enabled: bool,
    pub user: Option<String>,
    pub password: Option<String>,
}

impl DbSnapshotCfg {
    /// The dormant config: endpoint disabled, no credential (used in tests + the default wiring).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            user: None,
            password: None,
        }
    }
}

/// Map an anyhow error to a JSON HTTP response. "no project/task ..." -> 404/400.
struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = self.0.to_string();
        // task_751: a typed core::BoardError carries its intended status explicitly -- map it
        // directly, no string-matching. The string classifier below stays as the fallback for bails
        // not yet converted to BoardError (incremental strangler migration).
        let code = if let Some(be) = self.0.downcast_ref::<core::BoardError>() {
            match be.status {
                core::ErrorStatus::BadRequest => StatusCode::BAD_REQUEST,
                core::ErrorStatus::NotFound => StatusCode::NOT_FOUND,
                core::ErrorStatus::Forbidden => StatusCode::FORBIDDEN,
                core::ErrorStatus::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            }
        } else if msg.starts_with("no project")
            || msg.starts_with("no task")
            || msg.starts_with("no agent")
            || msg.starts_with("no document")
            || msg.starts_with("no comment")
            || msg.starts_with("no parent task")
            || msg.starts_with("no review")
            || msg == "not found"
        {
            StatusCode::NOT_FOUND
        } else if msg.starts_with("give ")
            || msg.starts_with("cannot move task")
            || msg.starts_with("a task cannot be its own parent")
            || msg.starts_with("reparenting would create a cycle")
            || msg.contains("is in a different project")
            || msg.starts_with("banned phrase")
            || msg.starts_with("caps-for-emphasis")
            || msg.starts_with("non-ASCII")
            || msg.starts_with("ambiguous bare reference")
            || msg.starts_with("unknown review status")
            || msg.starts_with("unknown review log type")
            // Structured-question answer / schema validation (task_1093): a bad answer is a client
            // error, not a server fault -- so the client sees a clean 400, not an opaque 500.
            || msg.starts_with("answer does not satisfy")
            || msg.starts_with("an out-of-frame text answer")
            || msg.starts_with("unknown answer shape")
            || msg.starts_with("answer shape")
            || msg.starts_with("invalid `response_schema`")
            || msg.starts_with("invalid `default`")
        {
            // Client-input validation errors (bad request), not server faults.
            StatusCode::BAD_REQUEST
        } else if msg.starts_with("no IPFS backend") || msg.starts_with("ipfs backend unavailable")
        {
            // Either the deployment has no IPFS backend configured (set ipfs_api_url), or the
            // backend was transiently unavailable through the bounded add retries (task_851).
            // Both are retryable-from-the-client, so 503 rather than a hard 500.
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        // Log the detail on a server fault so a 5xx is diagnosable from the journal rather than an
        // opaque status (the gap behind the task_1093 answer-500 incident: tower_http logged only
        // the status code, never the error message).
        if code.is_server_error() {
            tracing::error!(error = %msg, "api request failed with a server error");
        }
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
    // Per-process request metrics (task_1380): one registry shared by the recording middleware
    // (added below as a route_layer so MatchedPath is populated) and the GET /api/metrics handler
    // (via an Extension). Created here rather than threaded through AppState, so every test that
    // builds a router gets an isolated registry.
    let metrics = std::sync::Arc::new(crate::metrics::Metrics::new());
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/tunnels", get(tunnels))
        .route("/meta", get(meta))
        .route("/metrics", get(metrics_endpoint))
        .route("/admin/db-snapshot", get(db_snapshot))
        .route("/agents", get(list_agents).post(register_agent))
        .route("/resolve-agent", get(resolve_agent))
        .route("/agents/{agent_id}", get(get_agent).patch(update_agent))
        .route("/agents/{agent_id}/mandate", get(assemble_mandate))
        .route("/agents/{agent_id}/status", post(set_status))
        .route(
            "/agents/{agent_id}/request-stand-down",
            post(request_stand_down),
        )
        .route("/agents/{agent_id}/retire", post(retire_agent))
        .route("/agents/{agent_id}/restore", post(restore_agent))
        .route(
            "/agents/{agent_id}/lifecycle-intent",
            post(set_lifecycle_intent),
        )
        .route("/agents/{agent_id}/notifications", get(get_notifications))
        .route("/agents/{agent_id}/messages", get(get_messages))
        .route("/agents/{agent_id}/recall", get(recall))
        .route("/agents/{agent_id}/check-stop", post(check_stop))
        .route(
            "/agents/{agent_id}/config/{config_kind}",
            get(list_agent_config).post(set_agent_config_entry),
        )
        .route(
            "/agents/{agent_id}/config/{config_kind}/{entry_id}",
            delete(remove_agent_config_entry),
        )
        .route(
            "/sessions/{session_id}/transcript-chunks",
            get(list_transcript_chunks).post(append_transcript_chunk),
        )
        .route(
            "/decider-episodes",
            get(list_decider_episodes).post(submit_decider_episode),
        )
        .route("/budgets", post(set_budget))
        .route("/agents/{agent_id}/budget", get(get_budget))
        .route("/agents/{agent_id}/spend", post(report_spend))
        .route("/projects", get(list_projects).post(create_project))
        .route(
            "/projects/{project_id}",
            get(get_project).patch(update_project),
        )
        .route(
            "/projects/{project_id}/teams",
            get(list_project_teams)
                .post(attach_project_team)
                .delete(detach_project_team),
        )
        .route(
            "/projects/{project_id}/archive-done",
            post(archive_done_proposals),
        )
        .route(
            "/projects/{project_id}/age-out-todos",
            post(archive_stale_todos),
        )
        .route(
            "/projects/{project_id}/duplicates",
            get(find_duplicate_tasks),
        )
        .route("/projects/{project_id}/metrics", get(project_queue_metrics))
        .route("/system/link-rules", get(system_link_rules))
        .route("/enforcement/preflight", get(enforcement_preflight))
        .route("/tasks", get(list_tasks).post(create_task))
        .route("/tasks/{task_id}", get(get_task).patch(update_task))
        .route("/tasks/{task_id}/comments", post(comment_task))
        .route("/comments/{comment_id}", get(get_comment))
        .route("/tasks/{task_id}/questions", post(pose_question))
        .route("/comments/{comment_id}/answer", post(answer_question))
        .route("/comments/{comment_id}/decline", post(decline_question))
        .route("/comments/{comment_id}/cancel", post(cancel_question))
        .route("/comments/{comment_id}/supersede", post(supersede_question))
        .route(
            "/comments/{comment_id}/annotations",
            get(get_comment_annotations).post(annotate_comment),
        )
        .route(
            "/comment-annotations/{annotation_id}/resolve",
            post(resolve_comment_annotation),
        )
        .route("/tasks/awaiting", get(list_awaiting))
        .route("/tasks/{task_id}/props", patch(set_task_props))
        .route("/tasks/{task_id}/move", post(move_task))
        .route("/tasks/{task_id}/archive", post(archive_task))
        .route("/tasks/{task_id}/restore", post(restore_task))
        .route("/tasks/{task_id}/mute", post(mute_task))
        .route("/tasks/{task_id}/unmute", post(unmute_task))
        .route("/subscriptions", post(subscribe).delete(unsubscribe))
        .route("/channels", get(list_channels).post(create_channel))
        .route("/channels/{channel_id}", get(get_channel))
        .route("/channels/{channel_id}/read", post(mark_channel_read))
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
        .route("/external-entity-tasks", post(ensure_external_entity_task))
        .route("/external-entity-tasks/block", post(block_on_external))
        .route(
            "/workspace-kinds",
            get(list_workspace_kinds).post(set_workspace_kind),
        )
        .route(
            "/workspace-kinds/{name}",
            get(get_workspace_kind).delete(delete_workspace_kind),
        )
        .route("/lint", post(lint_text))
        .route("/crash-reports", post(ingest_crash_report))
        .route("/grade-document", post(grade_document))
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
        .route("/operators/bindings", get(list_operator_bindings))
        .route(
            "/operators/{person}/binding",
            get(get_operator_binding)
                .post(bind_operator)
                .delete(unbind_operator),
        )
        .route("/teams", get(list_teams).post(create_team))
        .route("/teams/{team_id}", get(get_team).delete(delete_team))
        .route(
            "/teams/{team_id}/members",
            post(add_team_member).delete(remove_team_member),
        )
        .route("/reviews", get(list_reviews).post(create_review))
        .route("/reviews/trend", get(review_trend))
        .route("/reviews/{review_id}", get(get_review))
        .route("/reviews/{review_id}/status", post(set_review_status))
        .route("/reviews/{review_id}/vetted", post(set_review_vetted))
        .route("/reviews/{review_id}/metadata", post(set_review_metadata))
        .route("/reviews/{review_id}/log", post(append_review_log))
        .route("/reviews/{review_id}/assignees", post(add_review_assignee))
        .route(
            "/reviews/{review_id}/assignees/remove",
            post(remove_review_assignee),
        )
        .route("/reviews/{review_id}/approve", post(approve_review))
        .route("/ipfs/add", post(ipfs_add))
        .route("/ipfs/{cid}", get(ipfs_cat))
        .route("/wiki", get(list_wiki))
        .route("/documents", get(list_documents).post(create_document))
        .route(
            "/documents/{document_id}",
            get(get_document)
                .patch(update_document)
                .delete(delete_document),
        )
        .route(
            "/documents/{document_id}/content",
            get(read_document_content),
        )
        .route("/documents/{document_id}/path", post(set_document_path))
        .route("/documents/{document_id}/props", patch(set_document_props))
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
        .route(
            "/documents/{document_id}/deprecate",
            post(deprecate_document),
        )
        .route("/stream", get(stream))
        // Unknown /api/* paths return a JSON 404, not the SPA's index.html.
        .fallback(api_not_found)
        // Per-endpoint request metrics (task_1380): a route_layer so each request already carries
        // its MatchedPath when timed. route_layer skips the fallback, so random unmatched 404 URLs
        // are not timed at all, and matched requests key by route template (not by id).
        .route_layer(axum::middleware::from_fn({
            let metrics = metrics.clone();
            move |req, next| {
                let metrics = metrics.clone();
                async move { crate::metrics::record_request_metrics(metrics, req, next).await }
            }
        }))
        // Make the registry available to the GET /api/metrics handler.
        .layer(Extension(metrics))
        // Trusted-front-door identity: force the acting user from a configured header on
        // non-loopback writes (task_1030). A no-op when `trusted_user_header` is unset.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            force_trusted_user,
        ))
        .with_state(state)
}

async fn api_not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
}

/// `GET /api/metrics` — a JSON snapshot of per-endpoint request metrics (task_1380): latency
/// p50/p90/p99 (ms, approximate), request count + process start (a throughput basis), the
/// in-flight / max-in-flight concurrency gauge, and 2xx/4xx/5xx status-class counts, plus an
/// overall rollup. Aggregate numbers only (no request content), so it is an unauthenticated read
/// like the other GETs. The registry is injected by `router()` as an [`Extension`].
async fn metrics_endpoint(
    Extension(metrics): Extension<std::sync::Arc<crate::metrics::Metrics>>,
) -> Response {
    Json(metrics.snapshot()).into_response()
}

/// Acting-principal field names a write body may carry — the fields that attribute WHO is acting,
/// enumerated from an audit of every REST write handler. When the trusted-user header forces the
/// identity, these are overwritten. TARGET fields that name who a write is ABOUT (assignee,
/// routed_to, to_agent, blocked_on_ref, agent_id, member_id, to_project_id) are deliberately NOT
/// here, so forcing the actor never rewrites the subject. `subscriber` (POST /subscriptions) is
/// intentionally omitted too: it names who gets subscribed, so forcing it would break subscribing
/// another agent on their behalf — treated as a target, not the actor.
const ACTING_FIELDS: &[&str] = &[
    "principal",
    "actor",
    "author",
    "sender",
    "created_by",
    "from_agent",
    "invited_by",
    "requested_by",
];

/// The hostname of a `Host` header authority, lowercased, with the port stripped and an IPv6
/// literal unwrapped from its brackets — the key used to look a request up in the per-host auth
/// map. "board.example.com:8079" -> "board.example.com"; "[::1]:8079" -> "::1".
fn host_authority_name(host: &str) -> String {
    let host = host.trim();
    let host = host.split('%').next().unwrap_or(host); // strip any IPv6 zone id
    let host_part = if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal: "[::1]" or "[::1]:8079".
        rest.split(']').next().unwrap_or(rest)
    } else {
        // "name:port" or "name" — a bare name/IPv4 has at most one colon.
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    host_part.to_ascii_lowercase()
}

/// HTML-escape a string for use inside a double-quoted attribute value.
fn html_escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// The RAW trusted-front-door username on this request: `Some(value)` when the request arrives on a
/// host configured to force identity (task_1030) AND carries that host's trusted header; `None` on a
/// permissive/unlisted host or when the header is absent. The caller resolves it through the alias
/// table (so a tunnel's "bythewc" becomes "cameron") before using it.
pub(crate) fn trusted_user_header_value(
    headers: &HeaderMap,
    host_auth: &std::collections::HashMap<String, Option<String>>,
) -> Option<String> {
    if host_auth.is_empty() {
        return None;
    }
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let header_name = host_auth.get(&host_authority_name(host))?.as_deref()?;
    headers
        .get(header_name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Build the `<meta name="board-user" content="...">` tag the web app reads on boot (task_1036
/// server half) for an already-resolved username. The content is HTML-escaped.
pub(crate) fn board_user_meta_html(user: &str) -> String {
    format!(
        "<meta name=\"board-user\" content=\"{}\">",
        html_escape_attr(user)
    )
}

/// Overwrite every present [`ACTING_FIELDS`] key in a JSON object body with `user`. Only existing
/// keys are changed (so `deny_unknown_fields` is never tripped by an injected key, and an endpoint
/// without an acting field is untouched). Non-object / non-JSON bodies pass through unchanged.
fn rewrite_acting_fields(bytes: &[u8], user: &str) -> Vec<u8> {
    let Ok(mut v) = serde_json::from_slice::<Value>(bytes) else {
        return bytes.to_vec();
    };
    let Some(obj) = v.as_object_mut() else {
        return bytes.to_vec();
    };
    let mut changed = false;
    for f in ACTING_FIELDS {
        if let Some(slot) = obj.get_mut(*f) {
            *slot = Value::String(user.to_string());
            changed = true;
        }
    }
    if changed {
        serde_json::to_vec(&v).unwrap_or_else(|_| bytes.to_vec())
    } else {
        bytes.to_vec()
    }
}

/// The resolved trusted viewer for a request on a forcing host (task_542 B5b): `force_trusted_user`
/// stashes it as a request extension so READ handlers can scope results to the authenticated caller.
/// Absent when the host is permissive or no trusted header was present (then read-scoping treats the
/// caller as unidentified -> fail-closed once enforcement is enabled; a no-op while it is off).
#[derive(Clone)]
struct ForcedViewer(String);

/// Middleware: force the acting user from a per-host trusted header (task_1030, operator request).
/// The behavior is driven entirely by the `[hosts.'<name>']` config matched on the request's `Host`:
/// - no per-host config at all → pass through (client-set actor trusted everywhere, legacy).
/// - request `Host` not listed, or listed with no `auth_header` (e.g. `[hosts.'127.0.0.1']`) →
///   pass through (that host is permissive; the client sets its own actor — covers localhost/dev).
/// - matched host WITH an `auth_header`, read (GET/HEAD/OPTIONS) → pass through (reads don't
///   attribute an actor).
/// - matched host WITH an `auth_header`, write MISSING the header → 401 (a request fronted by that
///   host's tunnel must carry it; a missing header is a misconfig or a bypass attempt).
/// - matched host WITH an `auth_header`, write WITH the header → the acting-principal fields in the
///   JSON body are overwritten with the header value, so the client cannot attribute the write to
///   anyone else.
async fn force_trusted_user(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    if st.host_auth.is_empty() {
        return next.run(req).await;
    }
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // This host forces identity only if it is listed WITH an auth_header; otherwise permissive.
    let Some(header_name) = st
        .host_auth
        .get(&host_authority_name(&host))
        .and_then(|h| h.as_deref())
    else {
        return next.run(req).await;
    };
    let is_write = matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    let forced_raw = req
        .headers()
        .get(header_name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let Some(forced_raw) = forced_raw else {
        // A write on a forcing host MUST carry the header (a missing header is a misconfig or a
        // bypass attempt).
        if is_write {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": format!("missing trusted user header `{header_name}` on a non-loopback write")
                })),
            )
                .into_response();
        }
        // A read without the header passes through UNSTAMPED: read-scoping then treats the caller as
        // unidentified (fail-closed under enforcement; a no-op while enforcement is off).
        return next.run(req).await;
    };
    // Resolve the front-door username through the identity-alias table (task_1030): a tunnel's
    // "bythewc" forces as its canonical "cameron", so the request acts as the SAME principal the board
    // already keys ownership / subscriptions / operator-routing / access to. No alias => the raw value.
    let forced = core::resolve_identity_alias(&st.pool, &forced_raw).await;
    // task_542 B5b: stash the resolved viewer so READ handlers can scope results to the authenticated
    // caller (a read carries no acting-field body to rewrite). Writes ALSO rewrite the acting fields
    // below; the extension rides on the request parts across the into_parts/from_parts rebuild.
    req.extensions_mut().insert(ForcedViewer(forced.clone()));
    if !is_write {
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    // API JSON bodies are small; buffer to rewrite the acting fields. 16 MiB cap guards against a
    // runaway body (a document `content` is well under this).
    let bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "could not read request body" })),
            )
                .into_response()
        }
    };
    let new_bytes = rewrite_acting_fields(&bytes, &forced);
    let mut parts = parts;
    // The body length changed; drop the stale Content-Length so the downstream extractor reads the
    // rewritten body in full (axum/hyper will frame it correctly).
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    let req = Request::from_parts(parts, axum::body::Body::from(new_bytes));
    next.run(req).await
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

/// `GET /api/admin/db-snapshot` -- download a consistent copy of the SQLite database behind HTTP
/// Basic auth. The daemon owns `db_path`, so this authed GET is the extraction primitive for host
/// migration + DR: no stop-the-daemon, no cross-user root file-copy. Fail-closed at every step:
/// disabled by default (404, hiding its existence), 503 if enabled without a credential, 401 on a
/// missing/bad credential. The served file is produced via `VACUUM INTO` (point-in-time consistent
/// even under WAL -- never a torn mid-write stream) and is integrity-checked before it is served.
async fn db_snapshot(State(st): State<AppState>, headers: HeaderMap) -> Response {
    // Dormant unless explicitly enabled; a 404 hides the endpoint's existence when off.
    if !st.db_snapshot.enabled {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    // Enabled but un-credentialed is a misconfiguration -- refuse rather than serve unauthenticated.
    let (Some(user), Some(pass)) = (
        st.db_snapshot.user.as_deref().filter(|s| !s.is_empty()),
        st.db_snapshot.password.as_deref().filter(|s| !s.is_empty()),
    ) else {
        tracing::error!(
            "db-snapshot endpoint is enabled but has no credential configured; refusing to serve"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "db-snapshot endpoint is misconfigured",
        )
            .into_response();
    };
    if !basic_auth_ok(&headers, user, pass) {
        return (
            StatusCode::UNAUTHORIZED,
            [(
                axum::http::header::WWW_AUTHENTICATE,
                "Basic realm=\"task-board db-snapshot\"",
            )],
            "unauthorized",
        )
            .into_response();
    }

    match produce_db_snapshot(&st.pool, &st.db_path).await {
        Ok((body, filename)) => (
            StatusCode::OK,
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "application/octet-stream".to_string(),
                ),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{filename}\""),
                ),
            ],
            body,
        )
            .into_response(),
        Err(e) => {
            tracing::error!("db-snapshot failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "snapshot failed").into_response()
        }
    }
}

/// Validate an HTTP Basic `Authorization` header against the configured credential. Both fields are
/// compared in constant time so a mismatch does not leak position via timing. A missing or malformed
/// header is a plain `false` (the caller returns 401).
fn basic_auth_ok(headers: &HeaderMap, user: &str, pass: &str) -> bool {
    use base64::Engine as _;
    let Some(raw) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(b64) = raw
        .strip_prefix("Basic ")
        .or_else(|| raw.strip_prefix("basic "))
    else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return false;
    };
    let Ok(creds) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((u, p)) = creds.split_once(':') else {
        return false;
    };
    // Non-short-circuiting `&` so both comparisons always run.
    ct_eq(u.as_bytes(), user.as_bytes()) & ct_eq(p.as_bytes(), pass.as_bytes())
}

/// Constant-time byte-slice equality (for the two equal-length comparands differ-fast is avoided).
/// A length difference returns early -- the length of a secret is a far weaker leak than its bytes.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Produce a point-in-time-consistent snapshot of the database and return it as a streaming body.
/// `VACUUM INTO` writes a fresh, self-consistent copy (correct even under WAL), which is then
/// integrity-checked; the temp file is opened and immediately UNLINKED so its inode is reclaimed the
/// moment the stream ends (or the client disconnects), never leaving a full-fleet-data copy on disk.
async fn produce_db_snapshot(
    pool: &Pool,
    db_path: &str,
) -> anyhow::Result<(axum::body::Body, String)> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    // Beside the DB: same filesystem + owned by the service user, so VACUUM INTO can always write it.
    let dir = std::path::Path::new(db_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let tmp = dir.join(format!(".board-snapshot-{pid}-{nanos}.db"));
    let tmp_str = tmp.to_string_lossy().to_string();

    // The path is process-generated (pid + nanos) with no quote/backslash, so formatting it into the
    // VACUUM INTO string literal is injection-safe. VACUUM INTO cannot target an existing file.
    if tmp_str.contains('\'') || tmp_str.contains('\\') {
        anyhow::bail!("refusing to snapshot: unexpected characters in temp path");
    }
    let _ = tokio::fs::remove_file(&tmp).await;
    sqlx::query(&format!("VACUUM INTO '{tmp_str}'"))
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("VACUUM INTO snapshot failed: {e}"))?;

    // Fail-closed on a corrupt copy: verify it opens clean before serving.
    if let Err(e) = verify_snapshot_integrity(&tmp_str).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }

    let file = tokio::fs::File::open(&tmp)
        .await
        .map_err(|e| anyhow::anyhow!("opening snapshot: {e}"))?;
    // Unlink now; the open fd keeps the inode alive for the stream, then it is auto-reclaimed.
    let _ = tokio::fs::remove_file(&tmp).await;
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file));
    Ok((body, format!("task-board-{nanos}.db")))
}

/// Open the snapshot read-only and run `PRAGMA integrity_check`, erroring unless it reports "ok".
async fn verify_snapshot_integrity(path: &str) -> anyhow::Result<()> {
    use sqlx::{ConnectOptions, Connection};
    let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("opening snapshot for integrity check: {e}"))?;
    let result: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&mut conn)
        .await
        .map_err(|e| anyhow::anyhow!("integrity_check failed: {e}"))?;
    let _ = conn.close().await;
    if result != "ok" {
        anyhow::bail!("snapshot failed integrity_check: {result}");
    }
    Ok(())
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
    Endpoint { method: "GET", path: "/api/metrics", summary: "Request metrics for optimizing board/fleet performance (task_1380): per-endpoint latency p50/p90/p99 in ms (approximate, from coarse log-spaced buckets), request count + process start (a throughput basis), the in-flight / max-in-flight concurrency gauge (a hang shows as a stuck-high in_flight), and 2xx/4xx/5xx status-class counts, plus an overall rollup. Keys are METHOD + route template relative to the /api mount. Aggregate numbers only, mutates nothing.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/admin/db-snapshot", summary: "Download a point-in-time-consistent copy of the SQLite database (VACUUM INTO, integrity-checked), behind HTTP Basic auth. Disabled by default (404 when off); the deployment keeps it loopback/LAN-bound and off the public tunnel. The extraction primitive for host migration + DR.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/agents", summary: "List agents as a lightweight roster: compact {id, display_name, status, metadata, retired, lifecycle_intent, priority} by default (the small metadata bag is kept for filtering, e.g. metadata.native; lifecycle_intent + priority are the declared-config fields the reconciler reads; only the heavy charter is dropped to stay under the token cap). Pass verbose=true for full objects (incl charter), or GET /api/agents/{id} for one. Filters: status, q (id+display_name substring), meta_key+meta_value (scalar metadata match), lifecycle_intent (run|paused|retired declared desired-fleet-state, independent of live presence; lifecycle_intent=run is the desired-live set), retired (true=only terminally-gone agents, false=only live). Bounded by limit (default 200, max 1000) + offset.", query: "status=str&q=str&meta_key=str&meta_value=str&lifecycle_intent=str&verbose=bool&retired=bool&limit=int&offset=int", body: None },
    Endpoint { method: "POST", path: "/api/agents", summary: "Register (or update) an agent, trust-on-first-use.", query: "", body: Some("RegisterAgentBody") },
    Endpoint { method: "GET", path: "/api/resolve-agent", summary: "Resolve an agent name to one exact agent id (task_1251): an exact id wins (never ambiguous even when it is a prefix of a longer id), a unique case-insensitive substring resolves, an ambiguous substring is refused (400) with the sorted candidate ids. The safe recipient-resolver vs picking the first row of a q= search. Returns {name, id, match}.", query: "name=str", body: None },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}", summary: "Fetch a single agent (including its charter + metadata).", query: "", body: None },
    Endpoint { method: "PATCH", path: "/api/agents/{agent_id}", summary: "Update an agent's fields + metadata (the board agent list as a registry). Merge-PATCH: an omitted/null field is left unchanged; to reset a nullable field to null, name it in `clear` (e.g. [\"webhook_url\"]).", query: "", body: Some("UpdateAgentBody") },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}/mandate", summary: "Assemble an agent's session-start mandate from board data in one read (task_1457, doc_3426 ask 3): charter, role prompt, applicable standing directives (tenets/*), and applicable recipes (recipes/*), in the deterministic order charter, role, directives, recipes. Applicability is matched server-side; a version_fingerprint + component_document_ids drive the harness's subscribe-and-recompare hot-reload.", query: "contexts=csv&include_content=bool", body: None },
    Endpoint { method: "POST", path: "/api/agents/{agent_id}/status", summary: "Set an agent's presence status.", query: "", body: Some("SetStatusBody") },
    Endpoint { method: "POST", path: "/api/agents/{agent_id}/request-stand-down", summary: "Request that an agent gracefully wind down: records the request (who/why/when, shown on the agent's page) and notifies the agent so it stands down on its own terms. A SIGNAL — never changes the agent's status and never kills a live agent. Cleared when the agent goes offline.", query: "", body: Some("RequestStandDownBody") },
    Endpoint { method: "POST", path: "/api/agents/{agent_id}/retire", summary: "Terminally retire an agent (task_1363): mark it permanently gone (distinct from offline/stand-down, which are resumable) and run the auto-disposition sweep — every task blocked on it is cleared back to todo (keeping the assignee) and task.blocker_retired is emitted to board-pm for re-homing, so no dependent silently strands. Operator or board-pm only; reversible via /restore.", query: "", body: Some("RetireAgentBody") },
    Endpoint { method: "POST", path: "/api/agents/{agent_id}/restore", summary: "Reverse a terminal retirement (task_1363): clear the gone marker so a mis-marked agent is live again. Does not un-sweep already-disposed tasks. Operator or board-pm only.", query: "", body: Some("RestoreAgentBody") },
    Endpoint { method: "POST", path: "/api/agents/{agent_id}/lifecycle-intent", summary: "Set an agent's DECLARED lifecycle intent (task_1455): run or paused — the desired state the reconciler drives on, distinct from live presence. 'retired' is set via /retire (guarded auto-disposition sweep) and reversed via /restore, not here. Emits agent.intent_changed so a subscribed reconciler acts without a re-fetch.", query: "", body: Some("SetLifecycleIntentBody") },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}/notifications", summary: "Drain an agent's inbox (event notifications).", query: "mark_read=bool&limit=int", body: None },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}/messages", summary: "Read direct messages sent to an agent.", query: "mark_read=bool&limit=int", body: None },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}/recall", summary: "Board-state-recall bundle (task_1464, doc_3426 ask 9): ONE round trip that rebuilds an agent's working context at session (re)start -- its desired config + lifecycle_intent (task_1455/1456), its open assigned tasks with status + blocked_on, its bounded unread inbox (NOT marked read), and, when session_id is given, a handle to that session's transcript-chunk log (task_1463). The board is the recovery authority: recall first, transcript rehydration second. Bounded + efficient per start.", query: "session_id=str&task_limit=int&activity_limit=int", body: None },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}/config/{config_kind}", summary: "Read an agent's per-config_kind editable-config set (task_1477, doc_3426 ask 11): one read returns {agent_id, config_kind, version, count, entries} in dispatch order (position then id). config_kind namespaces the shared per-agent editable-config surface -- 'decider' (ask 11: payload = {kind, criteria, bands}, scope = applies_to_call_types) and 'tool' (ask 4). Each entry is the {id, enabled, scope, payload} envelope + position. The harness reads at session start and re-reads on an agent.config_changed wake (subscribe board + [\"agent\"]); version is the watchable key.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/agents/{agent_id}/config/{config_kind}", summary: "Upsert one entry in an agent's config set (task_1477): add a new entry or edit/toggle an existing one by entry_id. An omitted field keeps the existing value (edit) or the default (add: enabled=true, scope=null=all-call-types, payload={}, position appended). scope=null clears to all call types. Bumps the version + emits agent.config_changed so a subscribed harness hot-reloads. Returns the full updated set.", query: "", body: Some("SetAgentConfigEntryBody") },
    Endpoint { method: "DELETE", path: "/api/agents/{agent_id}/config/{config_kind}/{entry_id}", summary: "Remove one entry from an agent's config set (task_1477). Bumps the version + emits agent.config_changed like an edit. Returns the shrunk set.", query: "actor=str", body: None },
    Endpoint { method: "POST", path: "/api/agents/{agent_id}/check-stop", summary: "Board-authoritative stop-condition check (task_1479, doc_3426 ask 13): the harness calls this before letting an agent stop. Returns {decision: accept} only when the agent's lifecycle_intent is paused/retired (meant offline); a run-intent agent is never hard-accepted -- {decision: reject, reason, directive} with directive {kind: take, task_ref} if it holds an open actionable (todo/in_progress, non-blocked, non-monitor-exempt) task, else {kind: park} (stay online, idle, wait for a wake). A pure idempotent read on board state (desired-fleet-state + open work), not the agent self-report; the reason is fed to the model verbatim. stop_context is advisory.", query: "", body: Some("CheckStopBody") },
    Endpoint { method: "POST", path: "/api/sessions/{session_id}/transcript-chunks", summary: "Append a transcript-window pointer to a session's durable chunk log (task_1463): the harness checkpoints a context window to IPFS and records the CID + metadata here (never the bytes). position is assigned server-side (per-session, monotonic across generations) and returned. kind is window | compaction-boundary; append-only + history-preserving.", query: "", body: Some("AppendTranscriptChunkBody") },
    Endpoint { method: "GET", path: "/api/sessions/{session_id}/transcript-chunks", summary: "List a session's transcript-chunk pointers in order (task_1463), ACROSS generations so a respawned session rehydrates its whole history. since_position gives the incremental form; the response carries last_position as the next cursor. Returns CIDs + metadata; resolve bytes from IPFS.", query: "since_position=int", body: None },
    Endpoint { method: "POST", path: "/api/decider-episodes", summary: "Append a decider fail-retry-pass mini-transcript to the training-corpus ingest log (task_1478): keyed by agent/decider/call-type, content-addressed (content_id = IPFS CID), with the structured relabel record (inputs, each verdict + band per retry step, final pass) stored inline. Lightweight no-event append — fire it async off the agent hot path. Downstream consumer: the decider corpus (task_1471).", query: "", body: Some("SubmitDeciderEpisodeBody") },
    Endpoint { method: "GET", path: "/api/decider-episodes", summary: "Aggregate decider episodes for the corpus relabel/retrain work (task_1478): filter by decider_id, call_type, and a [since, until) created_at window (all optional), ordered by append order, bounded by limit (default 200, max 1000). Returns each episode's inline structured relabel record + CID.", query: "decider_id=str&call_type=str&since=iso&until=iso&limit=int", body: None },
    Endpoint { method: "POST", path: "/api/budgets", summary: "Set (upsert) an editable budget cap (task_1461, doc_3426 ask 6) scoped to an agent or role over a rolling window (hour|day|week|month|total). Bumps a version and emits budget.updated so affected agents hot-reload. Config data; the admit/defer decision stays harness-side.", query: "", body: Some("SetBudgetBody") },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}/budget", summary: "One-read budget admission surface (task_1461): an agent's effective cap (role base merged with agent override), current spend over the window, session priority, and a would_admit convenience, so the harness admits or defers a turn in one round trip. Deterministic.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/agents/{agent_id}/spend", summary: "Report one turn's realized cost to the spend ledger (task_1461): append-only, lightweight, fire off the hot path. The board accumulates current spend over the rolling window. Returns current spend after the append.", query: "", body: Some("ReportSpendBody") },
    Endpoint { method: "GET", path: "/api/projects", summary: "List projects (with task counts).", query: "status=str", body: None },
    Endpoint { method: "POST", path: "/api/projects", summary: "Create a project.", query: "", body: Some("CreateProjectBody") },
    Endpoint { method: "GET", path: "/api/projects/{project_id}", summary: "Fetch one project.", query: "", body: None },
    Endpoint { method: "PATCH", path: "/api/projects/{project_id}", summary: "Update a project (rename, archive, description, metadata).", query: "", body: Some("UpdateProjectBody") },
    Endpoint { method: "GET", path: "/api/projects/{project_id}/teams", summary: "Get a project's team grants (visibility + roles) + the resolved principal access map (strongest role wins; nested teams expanded for a cascade grant; implicit creator admin). Recording layer, task 542 Phase 3.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/projects/{project_id}/teams", summary: "Grant a team access to a project with a role (admin/read-write/read), idempotent. cascade (default true) extends to nested sub-teams.", query: "", body: Some("ProjectTeamBody") },
    Endpoint { method: "DELETE", path: "/api/projects/{project_id}/teams", summary: "Revoke a team's grant on a project (idempotent).", query: "", body: Some("ProjectTeamBody") },
    Endpoint { method: "POST", path: "/api/projects/{project_id}/archive-done", summary: "Retention sweep (task_1228): soft-archive every task in the project that has been done AND untouched for at least older_than_days days (default 7; 0 = no retention window). Reversible (restore), idempotent, iceboxed tasks exempt. Returns {project_id, older_than_days, archived, task_ids}.", query: "", body: Some("ArchiveDoneProposalsBody") },
    Endpoint { method: "POST", path: "/api/projects/{project_id}/age-out-todos", summary: "Age-out sweep (task_1215 sibling): soft-archive stale untriaged todos (status=todo, untouched for at least older_than_days days; default 14; 0 = no window). Scoped to status=todo, so blocked/in_progress/done/iceboxed are exempt. Reversible, idempotent. Returns {project_id, older_than_days, archived, task_ids}.", query: "", body: Some("ArchiveStaleTodosBody") },
    Endpoint { method: "GET", path: "/api/projects/{project_id}/duplicates", summary: "Report-only duplicate detector (task_1215 dedup sibling): active tasks (todo/in_progress/blocked, non-archived) clustered by normalized title (case/whitespace-insensitive), returning clusters of 2+. Mutates nothing. Done/cancelled/iceboxed excluded. Returns {project_id, groups:[{title_key, count, tasks:[...]}]}.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/projects/{project_id}/metrics", summary: "Read-only queue-time metrics (task_1265): derived entirely from the events stream, no new tables. pickup_latency_secs (task creation -> first assignment), time_in_todo_secs (summed dwell in status todo), time_blocked_secs (summed dwell in status blocked, sampled over actually-blocked tasks only). Each is {count, p50, p90, max, mean} in whole seconds (nearest-rank percentiles). Mutates nothing. Returns {project_id, task_count, pickup_latency_secs, time_in_todo_secs, time_blocked_secs}.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/system/link-rules", summary: "The deployment-configured link-tag rules (task_1243): a read-only list of {pattern, url_template} the UI uses to linkify custom references (e.g. CR-NNNN) in rendered content, generalizing the built-in typed-ref linkification. Patterns live in the deployment TOML (never in source), so each deployment customizes its own tags; empty when none are configured. Returns {link_rules:[{pattern, url_template}]}. Mutates nothing.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/enforcement/preflight", summary: "Fail-closed preflight for enabling per-operator ACCESS enforcement -- checks WORKSPACE/PROJECT GRANTS, NOT document conformance (use grade_document, the doc_7 A8 rubric, for a doc's conformance). doc_26 A5: enablable=true only when the fleet-coordination team exists and holds its standing grant on every project, so the coordination fleet is never stranded when enforcement flips on. Reports fleet_coordination_team_exists, projects_total, projects_missing_grant, blockers. Read-only.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/tasks", summary: "List/search tasks, optionally filtered. Archived tasks are hidden unless include_archived=true. monitor_exempt=bool filters by the derived monitor-exempt flag (audit the no-hiding-behind-exempt invariant).", query: "project_id=int&status=str&assignee=str&unassigned=bool&parent_id=int&top_level=bool&q=str&blocked_on_kind=str&blocked_on_ref=str&meta_key=str&meta_value=str&include_archived=bool&monitor_exempt=bool", body: None },
    Endpoint { method: "POST", path: "/api/tasks", summary: "Create a task.", query: "", body: Some("CreateTaskBody") },
    Endpoint { method: "GET", path: "/api/tasks/{task_id}", summary: "Fetch one task (with comments).", query: "", body: None },
    Endpoint { method: "PATCH", path: "/api/tasks/{task_id}", summary: "Update task fields (status, assignee, ...).", query: "", body: Some("UpdateTaskBody") },
    Endpoint { method: "POST", path: "/api/tasks/{task_id}/comments", summary: "Add a comment to a task.", query: "", body: Some("CommentBody") },
    Endpoint { method: "GET", path: "/api/comments/{comment_id}", summary: "Read one comment by id, with its type (plain/question/answer), parsed payload, lifecycle state, and reply_to/supersedes links.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/tasks/{task_id}/questions", summary: "Pose a structured question on a task, routed to a principal; blocking by default. Give EITHER a legacy kind (yes_no/multiple_choice/select_all/fill_in_the_blank/rank_list/point_allocation) OR omit kind for a CID-keyed question carrying an inline response_schema + ui.element_schema_cid (its canonical type id). Answers validated against the response_schema generically. point_allocation (doc_3371 entry 8) needs options + config={budget:N}; its answer is an object {option_id: integer_points} summing to the budget. quiz (doc_3371 entry 10) needs options + config={answer:[option_id,...], explanation?}; its answer is a choice scored server-side against the stored key (redacted from the question on read, revealed with the score on the answer). Returns the question comment.", query: "", body: Some("PoseQuestionBody") },
    Endpoint { method: "POST", path: "/api/comments/{comment_id}/answer", summary: "Answer an open question. For a kind-based question a framed answer (shape matching the kind) marks it answered and a text answer to a non-text kind is the out-of-frame escape; for a schema-driven question the value is validated against its response_schema generically. Returns the answer comment.", query: "", body: Some("AnswerQuestionBody") },
    Endpoint { method: "POST", path: "/api/comments/{comment_id}/decline", summary: "Decline an open question with feedback (an explicit refusal, distinct from an out-of-frame answer).", query: "", body: Some("DeclineQuestionBody") },
    Endpoint { method: "POST", path: "/api/comments/{comment_id}/cancel", summary: "Cancel an open question you posed (the asker withdraws it).", query: "", body: Some("CancelQuestionBody") },
    Endpoint { method: "POST", path: "/api/comments/{comment_id}/supersede", summary: "Supersede an open question with a replacement (doc_33 A6): the old is kept immutable + linked, the new copies its payload with a new prompt. Asker-only.", query: "", body: Some("SupersedeQuestionBody") },
    Endpoint { method: "GET", path: "/api/comments/{comment_id}/annotations", summary: "List a task comment's annotations (oldest first), optionally filtered by status (open/resolved).", query: "status=str", body: None },
    Endpoint { method: "POST", path: "/api/comments/{comment_id}/annotations", summary: "Annotate a task comment, optionally anchored to a highlighted span (region = free-form JSON selector, e.g. W3C/Hypothesis TextQuote+TextPosition; omit to annotate the whole comment). reply_to threads one level. Notifies the parent task's watchers.", query: "", body: Some("AnnotateCommentBody") },
    Endpoint { method: "POST", path: "/api/comment-annotations/{annotation_id}/resolve", summary: "Mark a comment annotation resolved (open -> resolved).", query: "", body: Some("ResolveCommentBody") },
    Endpoint { method: "GET", path: "/api/tasks/awaiting", summary: "The unified 'awaiting you' queue (task_860 + task_873): everything awaiting a decision from `viewer`, keyed INDEPENDENT of assignee, team-expanded, deduped, as a FLAT array of discriminated items. kind='task' {task_id, task_title, project_id, status, updated_at, blocked_on_principal, blocked_on_note, questions:[full question comment objects]} for a task blocked_on the principal OR carrying an open blocking question routed to it. kind='document' {document_id, title, status, version_no, updated_at, path} for a doc awaiting the operator's approval (status operator_review) -- emitted only when the viewer resolves to the operator.", query: "viewer=str&project_id=int&include_archived=bool", body: None },
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
    Endpoint { method: "GET", path: "/api/channels/{channel_id}", summary: "Fetch one channel with its members. Pass viewer to also get that viewer's unread_count + has_unread (task_1067).", query: "viewer=str", body: None },
    Endpoint { method: "POST", path: "/api/channels/{channel_id}/read", summary: "Mark a channel read for the caller (principal) up to a post seq (default: everything currently in the channel), clearing its unread dot. Advances the per-(subscriber,channel) last-read pointer and emits a silent channel.read event for cross-tab dot-clearing. Returns {channel_id, last_read_seq, unread_count}.", query: "", body: Some("ChannelReadBody") },
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
    Endpoint { method: "GET", path: "/api/external-links", summary: "List bridged links (channel-map / issue↔task / thread↔task), filter by source/board_kind/board_id. Task-kind rows carry the linked task's board_status, so a sync can enumerate the live external-entity-task set and filter to non-terminal.", query: "source=str&board_kind=str&board_id=int", body: None },
    Endpoint { method: "POST", path: "/api/external-links", summary: "Map a board entity (channel|task|thread) to an external one; idempotent on (source, external_id).", query: "", body: Some("UpsertExternalLinkBody") },
    Endpoint { method: "POST", path: "/api/external-entity-tasks", summary: "Create-or-reuse an external-entity-task for an external wait on a CR/PR (idempotent on source + lowercased owner/repo#number). State lives in metadata.external_entity; the bridge syncs it and sets it done on resolution, which auto-unblocks waiters.", query: "", body: Some("EnsureExternalEntityTaskBody") },
    Endpoint { method: "POST", path: "/api/external-entity-tasks/block", summary: "Make a task wait on an external CR/PR: ensure the shared entity-task E and set the waiter's blocked_on={kind:task,target:E}. Collapses into blocked-on-task and auto-unblocks when the bridge resolves E.", query: "", body: Some("BlockOnExternalBody") },
    Endpoint { method: "GET", path: "/api/workspace-kinds", summary: "List custom workspace kinds (named env setup definitions fleet spin-up materializes from board data).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/workspace-kinds", summary: "Define/update a workspace kind (setup_script + config an agent is configured with); idempotent on name, config merges.", query: "", body: Some("SetWorkspaceKindBody") },
    Endpoint { method: "GET", path: "/api/workspace-kinds/{name}", summary: "Fetch one workspace kind (setup_script + config) by name — what fleet spin-up reads to materialize a workspace.", query: "", body: None },
    Endpoint { method: "DELETE", path: "/api/workspace-kinds/{name}", summary: "Retire a workspace kind by name.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/lint", summary: "Dry-run the pre-submit content lint on arbitrary text without writing: returns {clean, banned_phrases, non_ascii, bare_refs} against the authoritative live banned-phrases list, the ASCII-only rule, and the ambiguous bare-#N typed-ref rule. Use this to pre-check content (incl. before a CID publish, which the write-path gate does not cover) instead of a drift-prone local copy.", query: "", body: Some("LintTextBody") },
    Endpoint { method: "POST", path: "/api/crash-reports", summary: "Ingest a UI crash report (task_879): auto-file (or bump) an investigation task for an uncaught browser exception. Body {message, kind?, stack?, component_stack?, url?, build?, user_agent?, occurred_at?}. Deduped by build + stack signature -- a recurring crash bumps one open task's occurrence count rather than spawning duplicates; a new signature files an unassigned task in intake (project 29) for board-triage to route. Returns {task_id, created, occurrences}.", query: "", body: Some("CrashReportBody") },
    Endpoint { method: "POST", path: "/api/grade-document", summary: "Grade a design document against the mechanical doc_7 A8 conformance rubric (ascii, required-sections-in-order, banned-phrases, title/heading rules, body-hygiene, status/provenance, caps-emphasis, body-length). Returns {clean, has_hard_fail, findings:[{check, severity, line, message}]} with actionable-remedy messages. The single grading source of truth: the board submit path and any client (fleet check-doc, the reviewer) call this one endpoint.", query: "", body: Some("GradeDocumentBody") },
    Endpoint { method: "GET", path: "/api/banned-phrases", summary: "List the fleet banned-phrases list (what the pre-submit content lint checks docs and comments against).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/banned-phrases", summary: "Add a phrase to the banned-phrases list (idempotent on the phrase, stored lowercased).", query: "", body: Some("AddBannedPhraseBody") },
    Endpoint { method: "DELETE", path: "/api/banned-phrases/{phrase}", summary: "Remove a phrase from the banned-phrases list.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/identity-aliases", summary: "List the identity aliases (alias -> canonical identity, e.g. operator -> cameron). A small config table consumers/UI use to resolve or display a floating name as the canonical identity across assignee, blocked_on, and @-mentions.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/people", summary: "List people (first-class human identities, multi-operator model doc_26). A separate registry from agents; resolved together with agents at read time.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/people", summary: "Create or upsert a person by stable string id (e.g. cameron).", query: "", body: Some("CreatePersonBody") },
    Endpoint { method: "DELETE", path: "/api/people/{id}", summary: "Delete a person and drop their team memberships.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/operators/bindings", summary: "List all per-operator concierge bindings (person -> handling agent, task_1259) -- the operator routing map.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/operators/{person}/binding", summary: "Read an operator's per-operator-concierge binding (person -> handling agent, task_1259), or null if unbound (default handling).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/operators/{person}/binding", summary: "Bind an operator (person) to the agent that handles them (e.g. zyork -> concierge-zyork). Idempotent upsert; a question routed to a bound person wakes that agent, an unbound person is unchanged.", query: "", body: Some("BindOperatorBody") },
    Endpoint { method: "DELETE", path: "/api/operators/{person}/binding", summary: "Clear an operator's handling-agent binding (idempotent); the person falls back to default handling.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/teams", summary: "List teams (addressable groups whose members are people OR other teams).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/teams", summary: "Create or upsert a team by stable string id (e.g. operator).", query: "", body: Some("CreateTeamBody") },
    Endpoint { method: "GET", path: "/api/teams/{team_id}", summary: "Get a team with its direct members and its fully-resolved person AND agent sets (resolved_people + resolved_agents, kept separate; nested teams expanded, cycle-guarded).", query: "", body: None },
    Endpoint { method: "DELETE", path: "/api/teams/{team_id}", summary: "Delete a team and drop its memberships (its members and its membership in parent teams).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/teams/{team_id}/members", summary: "Add a person or team as a member (idempotent). Rejects a sub-team add that would create a membership cycle.", query: "", body: Some("TeamMemberBody") },
    Endpoint { method: "DELETE", path: "/api/teams/{team_id}/members", summary: "Remove a member (person or team) from a team (idempotent).", query: "", body: Some("TeamMemberBody") },
    Endpoint { method: "POST", path: "/api/identity-aliases", summary: "Upsert an identity alias (alias -> canonical). Idempotent on the alias (repoints an existing one); alias is stored lowercased.", query: "", body: Some("SetIdentityAliasBody") },
    Endpoint { method: "GET", path: "/api/reviews", summary: "List reviews (newest-touched first), optionally filtered by status/kind/assignee. Without logs.", query: "status=str&kind=str&assignee=str", body: None },
    Endpoint { method: "POST", path: "/api/reviews", summary: "Create a review over an artifact (document|code|design|agent-session|task). Starts in `open` unless a status is seeded; records a `submitted` log entry. Pass external_link for idempotent ingest (a review already linked on (source, external_id) is returned created:false).", query: "", body: Some("CreateReviewBody") },
    Endpoint { method: "GET", path: "/api/reviews/trend", summary: "Improvement trend derived from review logs (no stored counter): findings-per-review with an earlier-vs-later trend, overall + sliced by kind and by producing area, counterbalanced by an escaped-defect signal (post-approval findings, re-opens, lineage follow-ups). A slice where findings fell while escaped defects rose is flagged.", query: "kind=str&area=str", body: None },
    Endpoint { method: "GET", path: "/api/reviews/{review_id}", summary: "Fetch one review with its full append-only log (findings are the entries of type `finding`).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/reviews/{review_id}/status", summary: "Transition a review's A2 status (open/in_review/changes_requested/approved/closed). Same status = idempotent no-op. Emits review.status_changed (+ opened_for_review / terminal).", query: "", body: Some("SetReviewStatusBody") },
    Endpoint { method: "POST", path: "/api/reviews/{review_id}/vetted", summary: "Set/clear a review's vetted gate (adversarial review run + addressed). Audit-only per D17: records the actor + logs the change (a decision entry), emits review.vetted_changed. Same value = idempotent no-op.", query: "", body: Some("SetReviewVettedBody") },
    Endpoint { method: "POST", path: "/api/reviews/{review_id}/metadata", summary: "Post-hoc setter for a review's metadata bag (recovery path for a review created with empty/incomplete metadata). MERGES the given properties (incoming keys overwrite), logs a decision entry naming the keys set, emits review.metadata_changed. Most important use: set reviewed_version on a conformance review that lacks it (the key the operator-submit gate reads). Empty object = idempotent no-op.", query: "", body: Some("SetReviewMetadataBody") },
    Endpoint { method: "POST", path: "/api/reviews/{review_id}/log", summary: "Append a log entry (comment / finding / decision / ...). Pass external_id for idempotent ingest (a bridge replaying an upstream item returns appended:false).", query: "", body: Some("AppendReviewLogBody") },
    Endpoint { method: "POST", path: "/api/reviews/{review_id}/assignees", summary: "Assign an ADDITIONAL reviewer (task_1323 multi-reviewer). The reviewer set is the primary assignee UNION the added reviewers; all are woken on review events. Logs assignee_added, emits review.assignee_added. Idempotent. Returns the review with its assignees + derived approval_state.", query: "", body: Some("ReviewAssigneeBody") },
    Endpoint { method: "POST", path: "/api/reviews/{review_id}/assignees/remove", summary: "Remove an added reviewer (task_1323). The single primary assignee set at create time is not removed by this. Logs assignee_removed, emits review.assignee_removed to the pre-removal reviewer set. Idempotent.", query: "", body: Some("ReviewAssigneeBody") },
    Endpoint { method: "POST", path: "/api/reviews/{review_id}/approve", summary: "Record a reviewer's approval (task_1323). Appends an approval log entry; approval_state is derived from all assignees' approvals against metadata.approval_policy (all [default] / any / k_of_n via metadata.approval_k). Emits review.approved_by.", query: "", body: Some("ApproveReviewBody") },
    Endpoint { method: "POST", path: "/api/ipfs/add", summary: "Content-address raw `content` server-side (add-only) and return its CID. Requires ipfs_api_url.", query: "", body: Some("IpfsAddBody") },
    Endpoint { method: "GET", path: "/api/ipfs/{cid}", summary: "Read content by CID through the IPFS backend (scoped, read-only). Pass ?content_type= to label the response. Requires ipfs_api_url.", query: "content_type=str", body: None },
    Endpoint { method: "GET", path: "/api/documents", summary: "List documents for discovery (filter by project/status/tag/exclude_tag/task_id/author; archived hidden unless include_archived=true). status accepts a comma-separated set + the operator vocabulary (pending-review/published); exclude_tag hides a tag (default-hide primitive). Agent-memory docs (the reserved agent-memory tag, or the repos/ and agents/ path prefixes) are hidden unless include_memory=true.", query: "project_id=int&status=str&tag=str&exclude_tag=str&task_id=int&author=str&include_archived=bool&include_memory=bool", body: None },
    Endpoint { method: "POST", path: "/api/documents", summary: "Create a versioned document (content is a bare IPFS CID; the board never resolves it).", query: "", body: Some("CreateDocumentBody") },
    Endpoint { method: "GET", path: "/api/wiki", summary: "List path-filed documents as a wiki tree (optionally under a path prefix), ordered by path; archived hidden unless include_archived=true.", query: "prefix=str&include_archived=bool", body: None },
    Endpoint { method: "GET", path: "/api/documents/{document_id}", summary: "Fetch one document with its current version + version list. Pass ?include_body=true to also inline the current version's markdown (resolved server-side from its CID; body:null + body_error on fetch failure).", query: "include_body=bool", body: None },
    Endpoint { method: "PATCH", path: "/api/documents/{document_id}", summary: "Rename a document (set its title; metadata-only — versions/content/path/status untouched). Emits document.updated.", query: "", body: Some("UpdateDocumentBody") },
    Endpoint { method: "PATCH", path: "/api/documents/{document_id}/props", summary: "Merge a JSON object into a document's metadata (description, type, tags, provenance) without cutting a content version. The list + wiki index project metadata.description. Emits document.updated.", query: "", body: Some("SetDocumentPropsBody") },
    Endpoint { method: "DELETE", path: "/api/documents/{document_id}", summary: "HARD-DELETE a document + all dependents (versions/comments/attachments/links/embeds). IRREVERSIBLE; requires the doc be archived first. Use only for true garbage; prefer archive otherwise. Emits document.deleted.", query: "", body: Some("DocumentActorBody") },
    Endpoint { method: "GET", path: "/api/documents/{document_id}/content", summary: "Read a document's body inline (resolves the version CID through the IPFS backend). Pass ?version_no= for a specific version, or ?approved=true for the operator-approved version (404 'no approved version' when there is none -- never a silent fallback to the current draft). The response carries an ETag of the served version's CID; a matching If-None-Match returns 304. Requires ipfs_api_url.", query: "version_no=int&approved=bool", body: None },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/path", summary: "Set (or clear, with an empty path) a document's wiki path; unique among filed docs.", query: "", body: Some("SetDocumentPathBody") },
    Endpoint { method: "GET", path: "/api/documents/{document_id}/versions", summary: "List a document's immutable versions (newest first).", query: "", body: None },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/versions", summary: "Publish a new immutable version (bare CID).", query: "", body: Some("PublishVersionBody") },
    Endpoint { method: "GET", path: "/api/documents/{document_id}/comments", summary: "List a document's comments (filter by version_id/status).", query: "version_id=int&status=str", body: None },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/comments", summary: "Comment on a document, optionally region-anchored to a version.", query: "", body: Some("CommentDocumentBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/comments/{comment_id}/resolve", summary: "Mark a document comment resolved. task_1418 soft-gate: a text-quote-anchored comment whose exact anchored text is still present verbatim in the current version is rejected with a warning (likely a premature 'addressed' claim); pass acknowledge=true to resolve a legitimately-rephrased-in-place anchor. Best-effort: skipped when there is no anchor or the body cannot be fetched.", query: "", body: Some("ResolveCommentBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/submit-review", summary: "Submit a document for review (status -> in_review).", query: "", body: Some("DocumentActorBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/submit-to-operator-review", summary: "Submit a document into the operator's review queue (status -> operator_review) -- the single gated chokepoint before the operator sees it. Rejected unless a template attestation (template_followed or template_waiver_reason) is given AND a design-conformance review has run against the current version with zero open findings. A design-doc submission also requires the read-the-guide attestation (the read_guide_attested field, or the legacy in-body marker).", query: "", body: Some("SubmitToOperatorReviewBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/request-changes", summary: "Request changes on a document (status -> changes_requested).", query: "", body: Some("RequestChangesBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/approve", summary: "Approve a document (stamps the current version, status -> approved).", query: "", body: Some("DocumentActorBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/attach", summary: "Attach a document to a task (notifies both sides).", query: "", body: Some("AttachDocumentBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/detach", summary: "Detach a document from a task.", query: "", body: Some("AttachDocumentBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/archive", summary: "Soft-archive (retire) a document: hidden from listings by default, reversible, history preserved.", query: "", body: Some("DocumentActorBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/restore", summary: "Restore a previously archived document (clears the archive stamp).", query: "", body: Some("DocumentActorBody") },
    Endpoint { method: "POST", path: "/api/documents/{document_id}/deprecate", summary: "Mark a document deprecated (optionally superseded_by a replacing document id), or deprecated=false to clear it. Orthogonal to archive: a deprecated doc stays VISIBLE (clients show a banner) rather than hidden. deprecated defaults to true.", query: "", body: Some("DeprecateDocumentBody") },
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
        RetireAgentBody,
        RestoreAgentBody,
        SetLifecycleIntentBody,
        AppendTranscriptChunkBody,
        CheckStopBody,
        SubmitDeciderEpisodeBody,
        SetAgentConfigEntryBody,
        SetBudgetBody,
        ReportSpendBody,
        CreateProjectBody,
        UpdateProjectBody,
        CreateTaskBody,
        UpdateTaskBody,
        CrashReportBody,
        MoveTaskBody,
        ArchiveTaskBody,
        ArchiveDoneProposalsBody,
        ArchiveStaleTodosBody,
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
        SetDocumentPropsBody,
        CommentDocumentBody,
        ResolveCommentBody,
        AnnotateCommentBody,
        DocumentActorBody,
        DeprecateDocumentBody,
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
        EnsureExternalEntityTaskBody,
        BlockOnExternalBody,
        PromoteThreadBody,
        SetWorkspaceKindBody,
        AddBannedPhraseBody,
        LintTextBody,
        GradeDocumentBody,
        UpdateDocumentBody,
        SetChannelAutoJoinBody,
        ChannelReadBody,
        CreateReviewBody,
        SetReviewStatusBody,
        SetReviewVettedBody,
        SetReviewMetadataBody,
        AppendReviewLogBody,
        ReviewAssigneeBody,
        ApproveReviewBody,
        SetIdentityAliasBody,
        CreatePersonBody,
        BindOperatorBody,
        CreateTeamBody,
        TeamMemberBody,
        ProjectTeamBody,
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
        // Render the query string as one wrapping chip per `key=type` param, not a single long
        // unbroken string -- a long query (e.g. GET /api/tasks) otherwise overflowed horizontally
        // and pushed the whole page past the viewport (task_1223). Each chip stays on one line; the
        // row of chips wraps within the Path cell.
        let query_html = if query.is_empty() {
            String::new()
        } else {
            let chips: String = query
                .split('&')
                .filter(|p| !p.is_empty())
                .map(|p| format!("<span class=\"qp\">{}</span>", html_escape(p)))
                .collect();
            format!("<div class=\"q\">{chips}</div>")
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
  th {{ color:#9aa4af; font-size:.72rem; text-transform:uppercase; letter-spacing:.04em; white-space:nowrap; }}
  code {{ font-family: ui-monospace,SFMono-Regular,Menlo,monospace; font-size:.82rem; }}
  /* Let a long path wrap within its cell (no spaces to break on) so a narrow viewport does not
     overflow horizontally; scoped to the path so the Method badge column is never squeezed. */
  .path code {{ color:#e6e8eb; overflow-wrap:anywhere; }}
  /* A dense 4-column table cannot shrink below its columns' min-content; scroll it inside its own
     box at narrow widths so the PAGE never gains horizontal overflow (task_1223). No-op at desktop
     width, where the table fits the 960px wrap. */
  .tablewrap {{ overflow-x:auto; }}
  .q {{ display:flex; flex-wrap:wrap; gap:.25rem; margin-top:.3rem; }}
  .qp {{ color:#9aa4af; background:#11151a; border:1px solid #1c2128; border-radius:.3rem; padding:.03rem .35rem; font-family: ui-monospace,SFMono-Regular,Menlo,monospace; font-size:.72rem; white-space:nowrap; }}
  .m {{ font-weight:600; padding:.05rem .4rem; border-radius:.3rem; font-size:.72rem; white-space:nowrap; }}
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
  <div class="tablewrap">
  <table>
    <thead><tr><th>Method</th><th>Path</th><th>Summary</th><th>Body</th></tr></thead>
    <tbody>{rows}</tbody>
  </table>
  </div>
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
    /// task_1456: filter by declared lifecycle_intent (run|paused|retired). The desired-fleet-state
    /// read the reconciler drives on -- lifecycle_intent=run is the desired-live set in one round
    /// trip, independent of live presence. A real column, so it filters in SQL (unlike the derived
    /// `retired` audit flag below, which post-filters the projection).
    lifecycle_intent: Option<String>,
    #[serde(default)]
    verbose: bool,
    /// task_1363: true = only retired/gone agents, false = only live. Omit for all.
    retired: Option<bool>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_agents(
    State(st): State<AppState>,
    Query(query): Query<ListAgentsQuery>,
) -> ApiResult {
    let agents = core::list_agents(
        &st.pool,
        query.status.as_deref(),
        query.q.as_deref(),
        query.meta_key.as_deref(),
        query.meta_value.as_deref(),
        query.lifecycle_intent.as_deref(),
        query.verbose,
        query.limit,
        query.offset,
    )
    .await?;
    // Audit filter on the derived retired flag (task_1363); a no-op when omitted.
    Ok(Json(core::filter_agents_retired(agents, query.retired)))
}

#[derive(Deserialize, JsonSchema)]
struct RetireAgentBody {
    /// Who is retiring the agent (must be an operator or board-pm).
    #[serde(rename = "principal", alias = "retired_by", alias = "actor")]
    retired_by: Option<String>,
    /// Optional reason recorded on the agent + each swept task's note.
    reason: Option<String>,
}

async fn retire_agent(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(b): Json<RetireAgentBody>,
) -> ApiResult {
    let retired_by = b.retired_by.as_deref().unwrap_or_default();
    Ok(Json(
        core::retire_agent(&st.pool, &agent_id, retired_by, b.reason.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct RestoreAgentBody {
    /// Who is restoring the agent (must be an operator or board-pm).
    #[serde(rename = "principal", alias = "actor", alias = "restored_by")]
    actor: Option<String>,
}

async fn restore_agent(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(b): Json<RestoreAgentBody>,
) -> ApiResult {
    let actor = b.actor.as_deref().unwrap_or_default();
    Ok(Json(core::restore_agent(&st.pool, &agent_id, actor).await?))
}

#[derive(Deserialize, JsonSchema)]
struct SetLifecycleIntentBody {
    /// The desired intent: run or paused (retired is set via /retire, run-from-retired via /restore).
    intent: String,
    /// Who is setting it (recorded on the agent + the agent.intent_changed event).
    #[serde(rename = "principal", alias = "intent_by", alias = "actor")]
    intent_by: Option<String>,
    /// Optional reason.
    reason: Option<String>,
}

async fn set_lifecycle_intent(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(b): Json<SetLifecycleIntentBody>,
) -> ApiResult {
    Ok(Json(
        core::set_lifecycle_intent(
            &st.pool,
            &agent_id,
            &b.intent,
            b.intent_by.as_deref(),
            b.reason.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct AppendTranscriptChunkBody {
    /// The session generation this chunk belongs to (one session's history spans generations).
    generation: i64,
    /// The IPFS CID the checkpointed context window was published to (pointer only, never bytes).
    #[serde(rename = "content_id", alias = "cid")]
    content_id: String,
    /// window (default) or compaction-boundary (an in-order marker; pre-boundary chunks retained).
    kind: Option<String>,
    /// Optional first turn index covered by this window.
    turn_start: Option<i64>,
    /// Optional last turn index covered by this window.
    turn_end: Option<i64>,
    /// Optional byte size of the checkpointed window.
    size_bytes: Option<i64>,
    /// Optional arbitrary metadata recorded with the entry.
    metadata: Option<serde_json::Value>,
}

async fn append_transcript_chunk(
    State(st): State<AppState>,
    Path(session_id): Path<String>,
    Json(b): Json<AppendTranscriptChunkBody>,
) -> ApiResult {
    Ok(Json(
        core::append_transcript_chunk(
            &st.pool,
            &session_id,
            b.generation,
            &b.content_id,
            b.kind.as_deref(),
            b.turn_start,
            b.turn_end,
            b.size_bytes,
            b.metadata,
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct ListTranscriptChunksQuery {
    /// Incremental cursor: return only entries strictly after this position (omit/0 = from start).
    since_position: Option<i64>,
}

async fn list_transcript_chunks(
    State(st): State<AppState>,
    Path(session_id): Path<String>,
    Query(q): Query<ListTranscriptChunksQuery>,
) -> ApiResult {
    Ok(Json(
        core::list_transcript_chunks(&st.pool, &session_id, q.since_position).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct RecallQuery {
    /// The session being recovered; when present the bundle includes a handle to that session's
    /// transcript-chunk log (task_1463). Omit to recall board state only.
    session_id: Option<String>,
    /// Bound the open-task list (default 50, max 500).
    task_limit: Option<i64>,
    /// Bound the unread-inbox list (default 50, max 500).
    activity_limit: Option<i64>,
}

async fn recall(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Query(q): Query<RecallQuery>,
) -> ApiResult {
    Ok(Json(
        core::recall_bundle(
            &st.pool,
            &agent_id,
            q.session_id.as_deref(),
            q.task_limit.unwrap_or(50),
            q.activity_limit.unwrap_or(50),
        )
        .await?,
    ))
}

async fn list_agent_config(
    State(st): State<AppState>,
    Path((agent_id, config_kind)): Path<(String, String)>,
) -> ApiResult {
    Ok(Json(
        core::list_agent_config_entries(&st.pool, &agent_id, &config_kind).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct SetAgentConfigEntryBody {
    /// Stable entry id, unique per (agent, config_kind). Re-POSTing the same id edits/toggles it.
    entry_id: String,
    /// Toggle without removing. Omit on an edit to keep the current value; defaults true on add.
    enabled: Option<bool>,
    /// Call-type-tag array the entry applies to; null/empty = all call types. Omit to keep existing.
    scope: Option<Value>,
    /// Opaque per-kind config body (decider = {kind, criteria, bands}). Omit to keep existing.
    payload: Option<Value>,
    /// Dispatch order; omit to append after the current max (add) or keep (edit).
    position: Option<i64>,
    #[serde(rename = "actor", alias = "principal", alias = "by")]
    actor: Option<String>,
}

async fn set_agent_config_entry(
    State(st): State<AppState>,
    Path((agent_id, config_kind)): Path<(String, String)>,
    Json(b): Json<SetAgentConfigEntryBody>,
) -> ApiResult {
    Ok(Json(
        core::set_agent_config_entry(
            &st.pool,
            &agent_id,
            &config_kind,
            &b.entry_id,
            b.enabled,
            b.scope,
            b.payload,
            b.position,
            b.actor.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct RemoveAgentConfigEntryQuery {
    #[serde(rename = "actor", alias = "principal", alias = "by")]
    actor: Option<String>,
}

async fn remove_agent_config_entry(
    State(st): State<AppState>,
    Path((agent_id, config_kind, entry_id)): Path<(String, String, String)>,
    Query(q): Query<RemoveAgentConfigEntryQuery>,
) -> ApiResult {
    Ok(Json(
        core::remove_agent_config_entry(
            &st.pool,
            &agent_id,
            &config_kind,
            &entry_id,
            q.actor.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct CheckStopBody {
    /// Advisory stop context (reason tag + free text, open-work summary, last-activity marker). The
    /// board decides on its own authority (desired-fleet-state + open work), so this is accepted for
    /// the loop contract but does not change the decision.
    stop_context: Option<Value>,
}

async fn check_stop(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(b): Json<CheckStopBody>,
) -> ApiResult {
    Ok(Json(
        core::check_stop(&st.pool, &agent_id, b.stop_context).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct SubmitDeciderEpisodeBody {
    /// The agent whose decider loop produced the episode.
    agent_id: String,
    /// The decider that gated the retries.
    decider_id: String,
    /// The call type the decider gated (e.g. code-review).
    call_type: String,
    /// IPFS CID of the full fail-retry-pass episode payload (pointer + inline record, not bytes).
    #[serde(rename = "content_id", alias = "cid")]
    content_id: String,
    /// Structured relabel record stored inline: inputs, each verdict + band per retry step, final
    /// pass -- so the corpus (task_1471) relabels without a CID fetch.
    episode: Option<serde_json::Value>,
}

async fn submit_decider_episode(
    State(st): State<AppState>,
    Json(b): Json<SubmitDeciderEpisodeBody>,
) -> ApiResult {
    Ok(Json(
        core::submit_decider_episode(
            &st.pool,
            &b.agent_id,
            &b.decider_id,
            &b.call_type,
            &b.content_id,
            b.episode,
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct ListDeciderEpisodesQuery {
    /// Filter to one decider (omit to span all).
    decider_id: Option<String>,
    /// Filter to one call type (omit to span all).
    call_type: Option<String>,
    /// Lower bound on created_at (ISO 8601, inclusive).
    since: Option<String>,
    /// Upper bound on created_at (ISO 8601, exclusive).
    until: Option<String>,
    /// Max episodes (default 200, max 1000).
    limit: Option<i64>,
}

async fn list_decider_episodes(
    State(st): State<AppState>,
    Query(q): Query<ListDeciderEpisodesQuery>,
) -> ApiResult {
    Ok(Json(
        core::list_decider_episodes(
            &st.pool,
            q.decider_id.as_deref(),
            q.call_type.as_deref(),
            q.since.as_deref(),
            q.until.as_deref(),
            q.limit,
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct SetBudgetBody {
    /// The cap scope: agent or role.
    scope: String,
    /// The agent id (scope=agent) or role name (scope=role) the cap applies to.
    scope_id: String,
    /// The spend cap over the window (tokens or cost units; non-negative).
    cap: f64,
    /// Rolling window: hour | day | week | month | total (default day).
    window_kind: Option<String>,
    #[serde(rename = "principal", alias = "updated_by", alias = "actor")]
    updated_by: Option<String>,
}

async fn set_budget(State(st): State<AppState>, Json(b): Json<SetBudgetBody>) -> ApiResult {
    Ok(Json(
        core::set_budget(
            &st.pool,
            &b.scope,
            &b.scope_id,
            b.cap,
            b.window_kind.as_deref(),
            b.updated_by.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct ReportSpendBody {
    /// The realized cost of the turn to add to the rolling spend window (non-negative).
    cost: f64,
}

async fn report_spend(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(b): Json<ReportSpendBody>,
) -> ApiResult {
    Ok(Json(core::report_spend(&st.pool, &agent_id, b.cost).await?))
}

async fn get_budget(State(st): State<AppState>, Path(agent_id): Path<String>) -> ApiResult {
    Ok(Json(core::get_budget(&st.pool, &agent_id).await?))
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
struct MandateQuery {
    /// Comma-separated context tags that narrow context-scoped directives/recipes. Omit for the
    /// context-independent set.
    #[serde(default)]
    contexts: Option<String>,
    /// When true, inline each component's body content (server-side via the IPFS backend). Omit/false
    /// for the cheap refs-only probe (refs + versions + fingerprint only).
    #[serde(default)]
    include_content: Option<bool>,
}

/// `GET /api/agents/{agent_id}/mandate` (task_1457, doc_3426 ask 3) -- assemble the agent's
/// session-start mandate (charter, role, applicable directives + recipes) from board data in one read.
async fn assemble_mandate(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Query(q): Query<MandateQuery>,
) -> ApiResult {
    let contexts = q.contexts.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    Ok(Json(
        core::assemble_mandate(
            &st.pool,
            st.ipfs_api_url.as_deref(),
            &agent_id,
            contexts,
            q.include_content.unwrap_or(false),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct ResolveAgentQuery {
    /// The agent name or id to resolve to a single exact agent id.
    name: String,
}

/// `GET /api/resolve-agent?name=<name>` — resolve a name to one exact agent id (task_1251): an exact
/// id wins, a unique substring resolves, an ambiguous substring is refused with the candidates.
async fn resolve_agent(
    State(st): State<AppState>,
    Query(q): Query<ResolveAgentQuery>,
) -> ApiResult {
    Ok(Json(core::resolve_agent(&st.pool, &q.name).await?))
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
    /// Per-session scheduling priority/tier (task_1455): high | normal | low (a weighted floor).
    priority: Option<String>,
    /// Who is making the change (your agent id). Required to set `webhook_url` (task_1492): the
    /// outbound-push target may only be set by the owning agent, so this must equal the path id.
    #[serde(rename = "principal", alias = "actor", alias = "by")]
    actor: Option<String>,
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
        b.priority.as_deref(),
        b.actor.as_deref(),
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
    #[serde(rename = "principal", alias = "requested_by")]
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
    viewer: Option<Extension<ForcedViewer>>,
    Query(q): Query<ListProjectsQuery>,
) -> ApiResult {
    // task_542 B5b: filter to the authenticated caller's readable projects when enforcement is
    // enabled (fail-closed; a no-op while off). The viewer is stamped by force_trusted_user.
    let viewer = viewer.map(|Extension(ForcedViewer(v))| v);
    Ok(Json(
        core::list_projects_scoped(&st.pool, q.status.as_deref(), viewer.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct CreateProjectBody {
    name: String,
    description: Option<String>,
    #[serde(rename = "principal", alias = "created_by")]
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
    viewer: Option<Extension<ForcedViewer>>,
    Path(ProjectRef(project_id)): Path<ProjectRef>,
) -> ApiResult {
    // task_542 B5b: scope the read to the authenticated caller when enforcement is enabled
    // (fail-closed; a no-op while enforcement is off). The viewer is stamped by force_trusted_user.
    let viewer = viewer.map(|Extension(ForcedViewer(v))| v);
    found(core::get_project_scoped(&st.pool, project_id, viewer.as_deref()).await?)
}

#[derive(Deserialize, JsonSchema)]
struct UpdateProjectBody {
    name: Option<String>,
    description: Option<String>,
    /// active / archived. Archiving hides it from the sidebar; fully reversible.
    status: Option<String>,
    /// MERGED into the project's props (e.g. a repo link), not replaced.
    metadata: Option<Value>,
    #[serde(rename = "principal", alias = "actor")]
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

#[derive(Deserialize, JsonSchema)]
struct ProjectTeamBody {
    /// The team handle to grant/revoke (e.g. "operator").
    team_id: String,
    /// Role for the grant: "admin", "read-write", or "read". Required on attach; ignored on detach.
    role: Option<String>,
    /// Cascade the grant to the team's nested sub-teams (default true). Ignored on detach.
    cascade: Option<bool>,
    #[serde(rename = "principal", alias = "created_by")]
    created_by: Option<String>,
}

/// GET the project's team grants + resolved principal access map (task 542 Phase 3).
async fn list_project_teams(
    State(st): State<AppState>,
    Path(ProjectRef(project_id)): Path<ProjectRef>,
) -> ApiResult {
    Ok(Json(core::project_access(&st.pool, project_id).await?))
}

/// GET the fail-closed enforcement-enablement preflight (task 542 Phase 3 Part B, doc_26 A5).
async fn enforcement_preflight(State(st): State<AppState>) -> ApiResult {
    Ok(Json(core::enforcement_preflight(&st.pool).await?))
}

/// Attach a team to a project with a role (admin / read-write / read), idempotent.
async fn attach_project_team(
    State(st): State<AppState>,
    Path(ProjectRef(project_id)): Path<ProjectRef>,
    Json(b): Json<ProjectTeamBody>,
) -> ApiResult {
    let role = b.role.as_deref().ok_or_else(|| {
        anyhow::anyhow!("role is required: \"admin\", \"read-write\", or \"read\"")
    })?;
    Ok(Json(
        core::attach_project_team(
            &st.pool,
            project_id,
            &b.team_id,
            role,
            b.cascade.unwrap_or(true),
            b.created_by.as_deref(),
        )
        .await?,
    ))
}

/// Detach a team's grant from a project (idempotent).
async fn detach_project_team(
    State(st): State<AppState>,
    Path(ProjectRef(project_id)): Path<ProjectRef>,
    Json(b): Json<ProjectTeamBody>,
) -> ApiResult {
    Ok(Json(
        core::detach_project_team(&st.pool, project_id, &b.team_id).await?,
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
    /// Filter by the DERIVED monitor_exempt flag (task_1326): true = only monitor-exempt tasks
    /// (metadata.monitor_exempt truthy OR status=icebox), false = only non-exempt. Omit for all.
    monitor_exempt: Option<bool>,
}

async fn list_tasks(
    State(st): State<AppState>,
    viewer: Option<Extension<ForcedViewer>>,
    Query(query): Query<ListTasksQuery>,
) -> ApiResult {
    let tasks = core::list_tasks(
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
    .await?;
    // Audit filter on the derived monitor_exempt flag (task_1326); applied before the readable-
    // projects filter. A no-op when the query param is omitted.
    let tasks = core::filter_tasks_monitor_exempt(tasks, query.monitor_exempt);
    // task_542 B5b: filter to the authenticated caller's readable projects when enforcement is
    // enabled (fail-closed; a no-op while off). The viewer is stamped by force_trusted_user.
    let viewer = viewer.map(|Extension(ForcedViewer(v))| v);
    Ok(Json(
        core::filter_tasks_to_readable(&st.pool, tasks, viewer.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct CreateTaskBody {
    /// The project to create the task in. Optional when `parent_id` is given (a child inherits its
    /// parent's project); required for a top-level task.
    #[serde(default)]
    project_id: Option<i64>,
    title: String,
    description: Option<String>,
    assignee: Option<String>,
    priority: Option<String>,
    #[serde(rename = "principal", alias = "created_by")]
    created_by: Option<String>,
    metadata: Option<Value>,
    /// Optional parent task (makes this a child/subtask). Must be in the same project.
    parent_id: Option<i64>,
    /// Optional external reference for idempotent ingest: if a task is already linked on
    /// (source, external_id), the existing task is returned (`created:false`) instead of a
    /// duplicate. Lets a bridge adapter create-from-external exactly-once.
    external_link: Option<core::ExternalRef>,
}

async fn create_task(
    State(st): State<AppState>,
    viewer: Option<Extension<ForcedViewer>>,
    Json(b): Json<CreateTaskBody>,
) -> ApiResult {
    // project_id is optional when parent_id is given (task 708): inherit the parent's project.
    let project_id = core::resolve_create_project(&st.pool, b.project_id, b.parent_id).await?;
    // task_542 B5c: gate the write on the authenticated caller's project access when enforcement is
    // enabled (fail-closed; a no-op while off). The actor is the forced-trusted-user viewer.
    let actor = viewer.map(|Extension(ForcedViewer(v))| v);
    core::ensure_can_write_project(&st.pool, actor.as_deref(), project_id).await?;
    Ok(Json(
        core::create_task(
            &st.pool,
            project_id,
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
    viewer: Option<Extension<ForcedViewer>>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Query(q): Query<GetTaskQuery>,
) -> ApiResult {
    // task_542 B5b: scope the read to the authenticated caller when enforcement is enabled
    // (fail-closed; a no-op while enforcement is off). The viewer is stamped by force_trusted_user.
    let viewer = viewer.map(|Extension(ForcedViewer(v))| v);
    found(core::get_task_scoped(&st.pool, task_id, q.comments_limit, viewer.as_deref()).await?)
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
    #[serde(rename = "principal", alias = "actor")]
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
    viewer: Option<Extension<ForcedViewer>>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Json(b): Json<UpdateTaskBody>,
) -> ApiResult {
    // task_542 B5c: gate the write on the authenticated caller's access to the task's project when
    // enforcement is enabled (fail-closed; a no-op while off). The actor is the forced viewer.
    let actor = viewer.map(|Extension(ForcedViewer(v))| v);
    core::ensure_can_write_task(&st.pool, actor.as_deref(), task_id).await?;
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
    #[serde(rename = "principal", alias = "author")]
    author: Option<String>,
    /// Optional external identity id (e.g. "slack:U123") this comment is attributed to — for an
    /// ingested human author. `author` stays the fleet agent that performed the write.
    external_author: Option<String>,
    /// Optional external reference for idempotent ingest: if a comment is already linked on
    /// (source, external_id), the existing comment is returned (`created:false`) instead of a
    /// duplicate. Lets a bridge adapter mirror an external comment exactly-once.
    external_link: Option<core::ExternalRef>,
    /// Optional parent comment id to thread this reply under (one level). The parent must be a
    /// comment on the SAME task; a cross-task or unknown parent is rejected (task_1449).
    reply_to: Option<i64>,
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
            b.reply_to,
        )
        .await?,
    ))
}

async fn get_comment(State(st): State<AppState>, Path(comment_id): Path<i64>) -> ApiResult {
    Ok(Json(core::get_comment(&st.pool, comment_id).await?))
}

#[derive(Deserialize, JsonSchema)]
struct PoseQuestionBody {
    /// Legacy kind (yes_no / multiple_choice / select_all / fill_in_the_blank / rank_list / point_allocation / quiz). Omit for a CID-keyed question carrying its own response_schema + ui.element_schema_cid (the canonical type id).
    #[serde(default)]
    kind: Option<String>,
    prompt: String,
    /// Options as [{id, label}] -- required for multiple_choice / select_all / rank_list / point_allocation / quiz.
    options: Option<Value>,
    /// The principal (person/team/agent id) the question routes to; "operator" is the seeded team.
    routed_to: String,
    /// Whether the question blocks its task while open (default true).
    blocking: Option<bool>,
    /// Non-blocking only: the presumed answer the asker proceeds on.
    default: Option<Value>,
    /// Non-blocking only: wait this many seconds before proceeding on the default (requires default).
    wait_period_seconds: Option<i64>,
    /// Optional inline JSON Schema the framed answer must satisfy (the schema-driven model); answers are then validated against it generically rather than by kind.
    response_schema: Option<Value>,
    /// Optional UI descriptor stored verbatim (element name, props, element-schema CID); resolved by the client, not the board.
    ui: Option<Value>,
    /// Per-kind config. point_allocation: {"budget": N} -- the constant sum (integer >= 1) an answer's points must total. quiz: {"answer": [option_id, ...], "explanation"?: string} -- the correct option id(s) scored against (server-side; redacted from the question on read, revealed with the score on the answer). Not accepted by kinds that take no config.
    #[serde(default)]
    config: Option<Value>,
    #[serde(rename = "principal", alias = "actor")]
    actor: Option<String>,
}

async fn pose_question(
    State(st): State<AppState>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Json(b): Json<PoseQuestionBody>,
) -> ApiResult {
    Ok(Json(
        core::pose_question_configured(
            &st.pool,
            task_id,
            b.kind.as_deref(),
            &b.prompt,
            b.options,
            &b.routed_to,
            b.blocking.unwrap_or(true),
            b.default,
            b.wait_period_seconds,
            b.response_schema,
            b.ui,
            b.config,
            b.actor.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct AnswerQuestionBody {
    /// bool / choice / text / ranked / allocation. Use text for an out-of-frame answer to a non-text kind.
    shape: String,
    /// The answer value per shape (boolean; array of option ids; string; ids in order; or an object {option_id: integer_points} summing to the budget for allocation).
    value: Value,
    #[serde(rename = "principal", alias = "actor")]
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
    #[serde(rename = "principal", alias = "actor")]
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
    #[serde(rename = "principal", alias = "actor")]
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
    #[serde(rename = "principal", alias = "actor")]
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
struct AwaitingQuery {
    /// The principal whose awaiting-decision queue to return (e.g. "operator"). A team-targeted
    /// block or a team-routed question surfaces for the team's members.
    viewer: String,
    project_id: Option<i64>,
    #[serde(default)]
    include_archived: bool,
}

/// `GET /api/tasks/awaiting?viewer=<principal>` — the unified "awaiting you" queue (task_860):
/// tasks blocked_on the principal UNION tasks with an open blocking question routed to the
/// principal, keyed independent of assignee, deduped to one task-centric row each with
/// `blocked_on_principal` + full-payload `questions[]`.
async fn list_awaiting(State(st): State<AppState>, Query(q): Query<AwaitingQuery>) -> ApiResult {
    Ok(Json(
        core::list_awaiting(&st.pool, &q.viewer, q.project_id, q.include_archived).await?,
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
    /// Move the task's whole subtree with it (epic + descendants, links preserved). Required for a
    /// task that has children or a parent; default false.
    #[serde(default)]
    cascade: Option<bool>,
    #[serde(rename = "principal", alias = "actor")]
    actor: Option<String>,
}

async fn move_task(
    State(st): State<AppState>,
    Path(TaskRef(task_id)): Path<TaskRef>,
    Json(b): Json<MoveTaskBody>,
) -> ApiResult {
    Ok(Json(
        core::move_task(
            &st.pool,
            task_id,
            b.to_project_id,
            b.cascade.unwrap_or(false),
            b.actor.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct ArchiveTaskBody {
    /// The agent performing the archive/restore (for the event actor).
    #[serde(rename = "principal", alias = "actor")]
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

#[derive(Deserialize, JsonSchema)]
struct ArchiveDoneProposalsBody {
    /// Archive a task only after it has been done AND untouched for at least this many days.
    /// Defaults to 7; 0 archives every eligible done task immediately (no retention window).
    #[serde(default)]
    older_than_days: Option<i64>,
    /// The agent performing the sweep (event actor).
    #[serde(rename = "principal", alias = "actor", default)]
    actor: Option<String>,
}

async fn archive_done_proposals(
    State(st): State<AppState>,
    Path(ProjectRef(project_id)): Path<ProjectRef>,
    Json(b): Json<ArchiveDoneProposalsBody>,
) -> ApiResult {
    Ok(Json(
        core::archive_done_proposals(
            &st.pool,
            project_id,
            b.older_than_days.unwrap_or(7),
            b.actor.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct ArchiveStaleTodosBody {
    /// Archive a todo only after it has been untouched for at least this many days.
    /// Defaults to 14; 0 archives every todo immediately (no age-out window).
    #[serde(default)]
    older_than_days: Option<i64>,
    /// The agent performing the sweep (event actor).
    #[serde(rename = "principal", alias = "actor", default)]
    actor: Option<String>,
}

async fn archive_stale_todos(
    State(st): State<AppState>,
    Path(ProjectRef(project_id)): Path<ProjectRef>,
    Json(b): Json<ArchiveStaleTodosBody>,
) -> ApiResult {
    Ok(Json(
        core::archive_stale_todos(
            &st.pool,
            project_id,
            b.older_than_days.unwrap_or(14),
            b.actor.as_deref(),
        )
        .await?,
    ))
}

/// `GET /api/projects/{project_id}/duplicates` — report-only duplicate detector (task_1215): active
/// tasks clustered by normalized title. Mutates nothing.
async fn find_duplicate_tasks(
    State(st): State<AppState>,
    Path(ProjectRef(project_id)): Path<ProjectRef>,
) -> ApiResult {
    Ok(Json(
        core::find_duplicate_tasks(&st.pool, project_id).await?,
    ))
}

/// `GET /api/projects/{project_id}/metrics` — read-only queue-time metrics (task_1265): pickup
/// latency, time in todo, and time blocked, derived from the events stream. Mutates nothing.
async fn project_queue_metrics(
    State(st): State<AppState>,
    Path(ProjectRef(project_id)): Path<ProjectRef>,
) -> ApiResult {
    Ok(Json(
        core::project_queue_metrics(&st.pool, project_id).await?,
    ))
}

/// `GET /api/system/link-rules` — the deployment-configured link-tag rules (task_1243): a read-only
/// list of {pattern, url_template} the UI linkifies in rendered content. Mutates nothing.
async fn system_link_rules(State(st): State<AppState>) -> ApiResult {
    Ok(Json(json!({ "link_rules": &*st.link_rules })))
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
    /// Optional event-class filter (#462): a subset of ["created", "moved", "done", "blocked",
    /// "status", "comment", "assigned", "review", "doc"]. When given, this subscription is
    /// delivery-gated to just those classes (only they reach the inbox and wake the subscriber); omit
    /// for every event. Applies to any target. (Ignored by unsubscribe.)
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
    #[serde(rename = "principal", alias = "created_by")]
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

#[derive(Deserialize)]
struct GetChannelQuery {
    /// If set, include this viewer's unread_count + has_unread (task_1067).
    viewer: Option<String>,
}

async fn get_channel(
    State(st): State<AppState>,
    Path(ChannelRef(channel_id)): Path<ChannelRef>,
    Query(q): Query<GetChannelQuery>,
) -> ApiResult {
    found(core::get_channel(&st.pool, channel_id, q.viewer.as_deref()).await?)
}

#[derive(Deserialize, JsonSchema)]
struct ChannelReadBody {
    #[serde(rename = "principal", alias = "agent_id", alias = "subscriber")]
    principal: Option<String>,
    /// Advance the last-read pointer to this post seq. Omit to mark everything currently in the
    /// channel read.
    up_to_seq: Option<i64>,
}

/// POST a channel's read marker: advance the caller's last-read pointer (task_1067).
async fn mark_channel_read(
    State(st): State<AppState>,
    Path(ChannelRef(channel_id)): Path<ChannelRef>,
    Json(b): Json<ChannelReadBody>,
) -> ApiResult {
    let who = b
        .principal
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApiError(anyhow::anyhow!(
                "a subscriber (principal) is required to mark a channel read"
            ))
        })?;
    Ok(Json(
        core::mark_channel_read(&st.pool, channel_id, who, b.up_to_seq).await?,
    ))
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
    #[serde(rename = "principal", alias = "actor")]
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
    #[serde(rename = "principal", alias = "actor")]
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
    #[serde(rename = "principal", alias = "invited_by")]
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
#[serde(deny_unknown_fields)]
struct EventsQuery {
    #[serde(default)]
    since_seq: i64,
    #[serde(default = "default_events_limit")]
    limit: i64,
    /// Only events whose `actor` matches — a complete per-agent activity feed.
    #[serde(rename = "principal", alias = "actor")]
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
    #[serde(rename = "principal", alias = "created_by")]
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
    #[serde(rename = "principal", alias = "created_by")]
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
struct CrashReportBody {
    /// "error" (uncaught exception) or "unhandledrejection" (rejected promise). Defaults to "error".
    kind: Option<String>,
    /// The uncaught error message (e.g. "TypeError: undefined is not a function").
    message: String,
    /// The error (or rejection reason) stack trace, if any.
    stack: Option<String>,
    /// React's component stack, when the crash was caught by the top-level ErrorBoundary.
    component_stack: Option<String>,
    /// The URL / route where the crash occurred (location.href, e.g. "/awaiting").
    url: Option<String>,
    /// The build hash of the bundle that crashed (e.g. "index-DZQnJBiy.js"), for dedup + triage.
    build: Option<String>,
    /// The reporting browser's user agent.
    user_agent: Option<String>,
    /// The client-side ISO8601 timestamp of the crash.
    occurred_at: Option<String>,
}

/// `POST /api/crash-reports` — ingest a UI crash report (task_879): auto-file (or bump) an
/// investigation task for an uncaught browser exception, deduped by build + stack signature so a
/// recurring crash updates one task rather than spawning duplicates. Open to the board's own UI
/// (unauthenticated); not content-gated (a stack trace is arbitrary text).
async fn ingest_crash_report(
    State(st): State<AppState>,
    Json(b): Json<CrashReportBody>,
) -> ApiResult {
    Ok(Json(
        core::ingest_crash_report(
            &st.pool,
            &core::CrashReport {
                kind: b.kind.as_deref(),
                message: &b.message,
                stack: b.stack.as_deref(),
                component_stack: b.component_stack.as_deref(),
                url: b.url.as_deref(),
                build: b.build.as_deref(),
                user_agent: b.user_agent.as_deref(),
                occurred_at: b.occurred_at.as_deref(),
            },
        )
        .await?,
    ))
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

#[derive(Deserialize, JsonSchema)]
struct GradeDocumentBody {
    /// The document body (markdown) to grade against the mechanical doc_7 A8 rubric.
    content: String,
    /// The document title, graded separately from the body (A1 title/heading rules). Optional;
    /// defaults to empty (title-specific checks are skipped when absent).
    #[serde(default)]
    title: Option<String>,
    /// Override the main-body prose-word budget (A8 #8). Defaults to the locked ~700-word basis.
    #[serde(default)]
    body_length_budget_words: Option<i64>,
}

/// Grade a design document against the mechanical doc_7 A8 conformance rubric without writing
/// anything: returns `{clean, has_hard_fail, findings:[{check, severity, line, message}]}`.
async fn grade_document(State(st): State<AppState>, Json(b): Json<GradeDocumentBody>) -> ApiResult {
    Ok(Json(
        core::grade_document(
            &st.pool,
            &b.content,
            b.title.as_deref().unwrap_or(""),
            b.body_length_budget_words,
        )
        .await?,
    ))
}

// --- Identity aliases (task 532) ---

#[derive(Deserialize, JsonSchema)]
struct SetIdentityAliasBody {
    /// The alias to map (stored lowercased; the lookup key), e.g. "operator".
    alias: String,
    /// The canonical identity it resolves to, e.g. "cameron".
    canonical: String,
    #[serde(rename = "principal", alias = "created_by")]
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
    #[serde(rename = "principal", alias = "created_by")]
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

// --- Per-operator concierge binding (task_1259): person -> handling agent. ---

#[derive(Deserialize, JsonSchema)]
struct BindOperatorBody {
    /// The agent that handles this operator (e.g. "concierge-zyork").
    handling_agent: String,
    #[serde(rename = "principal", alias = "actor")]
    actor: Option<String>,
}

async fn list_operator_bindings(State(st): State<AppState>) -> ApiResult {
    Ok(Json(core::list_operator_bindings(&st.pool).await?))
}

async fn get_operator_binding(State(st): State<AppState>, Path(person): Path<String>) -> ApiResult {
    Ok(Json(
        core::get_operator_binding(&st.pool, &person)
            .await?
            .unwrap_or(Value::Null),
    ))
}

async fn bind_operator(
    State(st): State<AppState>,
    Path(person): Path<String>,
    Json(b): Json<BindOperatorBody>,
) -> ApiResult {
    Ok(Json(
        core::bind_operator(&st.pool, &person, &b.handling_agent, b.actor.as_deref()).await?,
    ))
}

async fn unbind_operator(State(st): State<AppState>, Path(person): Path<String>) -> ApiResult {
    Ok(Json(core::unbind_operator(&st.pool, &person, None).await?))
}

#[derive(Deserialize, JsonSchema)]
struct CreateTeamBody {
    /// Stable string handle for the team (e.g. "operator"). Upserts if it already exists.
    id: String,
    display_name: Option<String>,
    #[serde(rename = "principal", alias = "created_by")]
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
    #[serde(rename = "principal", alias = "created_by")]
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
    #[serde(rename = "principal", alias = "created_by")]
    created_by: Option<String>,
    /// The PRIMARY reviewer assigned (back-compat single assignee).
    assignee: Option<String>,
    /// Additional reviewers for a multi-reviewer review (task_1323), added on top of the primary.
    /// Approval state is derived against metadata.approval_policy (all | any | k_of_n; default all).
    assignees: Option<Vec<String>>,
    /// Arbitrary properties: producing agent id, predecessor review id, tags, approval_policy, ...
    metadata: Option<Value>,
    /// Optional external reference for idempotent ingest: a review already linked on
    /// (source, external_id) is returned (`created:false`) instead of a duplicate.
    external_link: Option<core::ExternalRef>,
}

async fn create_review(State(st): State<AppState>, Json(b): Json<CreateReviewBody>) -> ApiResult {
    let review = core::create_review(
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
    .await?;
    // Multi-reviewer (task_1323): layer any extra assignees on top of the single-primary core create.
    if let (Some(extra), Some(rid)) = (&b.assignees, review.get("id").and_then(Value::as_i64)) {
        if !extra.is_empty() {
            core::add_review_assignees(&st.pool, rid, extra, b.created_by.as_deref()).await?;
            return Ok(Json(core::get_review(&st.pool, rid).await?));
        }
    }
    Ok(Json(review))
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
    #[serde(rename = "principal", alias = "actor")]
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
    #[serde(rename = "principal", alias = "actor")]
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
struct SetReviewMetadataBody {
    /// Properties to MERGE into the review's metadata bag (incoming keys overwrite, untouched keys
    /// preserved). For a conformance review this is where `reviewed_version` (an integer) belongs.
    metadata: Value,
    /// The agent setting the metadata (recorded for audit).
    #[serde(rename = "principal", alias = "actor")]
    actor: Option<String>,
}

async fn set_review_metadata(
    State(st): State<AppState>,
    Path(review_id): Path<i64>,
    Json(b): Json<SetReviewMetadataBody>,
) -> ApiResult {
    Ok(Json(
        core::set_review_metadata(&st.pool, review_id, b.metadata, b.actor.as_deref()).await?,
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
    #[serde(rename = "principal", alias = "author")]
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

#[derive(Deserialize, JsonSchema)]
struct ReviewAssigneeBody {
    /// The reviewer's agent id to add/remove.
    assignee: String,
    /// The agent making the change.
    #[serde(rename = "principal", alias = "actor")]
    actor: Option<String>,
}

async fn add_review_assignee(
    State(st): State<AppState>,
    Path(review_id): Path<i64>,
    Json(b): Json<ReviewAssigneeBody>,
) -> ApiResult {
    Ok(Json(
        core::add_review_assignee(&st.pool, review_id, &b.assignee, b.actor.as_deref()).await?,
    ))
}

async fn remove_review_assignee(
    State(st): State<AppState>,
    Path(review_id): Path<i64>,
    Json(b): Json<ReviewAssigneeBody>,
) -> ApiResult {
    Ok(Json(
        core::remove_review_assignee(&st.pool, review_id, &b.assignee, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct ApproveReviewBody {
    /// The reviewer approving.
    #[serde(rename = "principal", alias = "reviewer", alias = "actor")]
    reviewer: Option<String>,
    /// An optional note recorded on the approval log entry.
    note: Option<String>,
}

async fn approve_review(
    State(st): State<AppState>,
    Path(review_id): Path<i64>,
    Json(b): Json<ApproveReviewBody>,
) -> ApiResult {
    Ok(Json(
        core::record_review_approval(
            &st.pool,
            review_id,
            b.reviewer.as_deref(),
            b.note.as_deref(),
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

#[derive(Deserialize, JsonSchema)]
struct EnsureExternalEntityTaskBody {
    /// Originating system, e.g. "github".
    source: String,
    /// Repo reference: "owner/repo" or a full URL (canonicalized to lowercased owner/repo).
    repo: String,
    /// The CR/PR/issue number.
    number: i64,
    url: Option<String>,
    /// Entity kind, e.g. "cr" | "pr" | "issue".
    kind: Option<String>,
    /// Project the entity-task lives in.
    project_id: i64,
    created_by: Option<String>,
}

async fn ensure_external_entity_task(
    State(st): State<AppState>,
    Json(b): Json<EnsureExternalEntityTaskBody>,
) -> ApiResult {
    Ok(Json(
        core::ensure_external_entity_task(
            &st.pool,
            &b.source,
            &b.repo,
            b.number,
            b.url.as_deref(),
            b.kind.as_deref(),
            b.project_id,
            b.created_by.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct BlockOnExternalBody {
    /// The waiter task that should block on the external CR/PR.
    task_id: i64,
    source: String,
    repo: String,
    number: i64,
    url: Option<String>,
    kind: Option<String>,
    actor: Option<String>,
}

async fn block_on_external(
    State(st): State<AppState>,
    Json(b): Json<BlockOnExternalBody>,
) -> ApiResult {
    Ok(Json(
        core::block_on_external(
            &st.pool,
            b.task_id,
            &b.source,
            &b.repo,
            b.number,
            b.url.as_deref(),
            b.kind.as_deref(),
            b.actor.as_deref(),
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
    let upstream = ipfs::fetch(url, &cid).await?;
    let ct = q
        .content_type
        .as_deref()
        .and_then(|s| axum::http::HeaderValue::from_str(s).ok())
        .unwrap_or_else(|| axum::http::HeaderValue::from_static("application/octet-stream"));
    // Stream the body straight through (task_757): the board holds one chunk at a time, so a large
    // blob never buffers in memory here. The content-type is the one the client already knows.
    let mut resp = Response::new(axum::body::Body::from_stream(upstream.bytes_stream()));
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
    /// A single status or a comma-separated set (match any); accepts the operator vocabulary
    /// (pending-review -> operator_review, published -> approved).
    status: Option<String>,
    tag: Option<String>,
    /// Exclude documents carrying this tag (the default-hide primitive, e.g. exclude_tag=charter).
    exclude_tag: Option<String>,
    task_id: Option<i64>,
    #[serde(rename = "principal", alias = "author")]
    author: Option<String>,
    /// Include archived (retired) documents; hidden by default.
    #[serde(default)]
    include_archived: bool,
    /// Include agent-memory documents -- those carrying the reserved `agent-memory` tag, and those
    /// filed under the reserved repos/ or agents/ path prefixes. Hidden from the default feed by
    /// default (task_826). Browse memory via list_wiki with a prefix.
    #[serde(default)]
    include_memory: bool,
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
    let statuses = q
        .status
        .as_deref()
        .map(core::parse_status_filter)
        .unwrap_or_default();
    Ok(Json(
        core::list_documents_filtered(
            &st.pool,
            &core::DocListFilter {
                project_id: q.project_id,
                statuses,
                tag: q.tag.as_deref(),
                exclude_tag: q.exclude_tag.as_deref(),
                task_id: q.task_id,
                author: q.author.as_deref(),
                include_archived: q.include_archived,
                include_memory: q.include_memory,
            },
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
    /// A one-line change-note for this version (what changed + what to look for); the operator reads
    /// it as the review caption on the version list + diff. For v1, a short note on what the doc is.
    summary: Option<String>,
    #[serde(rename = "principal", alias = "created_by")]
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
) -> Result<Response, ApiError> {
    let value = core::get_document_with_body(
        &st.pool,
        st.ipfs_api_url.as_deref(),
        document_id,
        q.include_body,
    )
    .await?;
    Ok(no_cache(Json(value).into_response()))
}

#[derive(Deserialize, JsonSchema)]
struct UpdateDocumentBody {
    /// New title — a short, specific noun phrase; the viewer renders the title as the page header.
    title: String,
    #[serde(rename = "principal", alias = "actor")]
    actor: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct SetDocumentPropsBody {
    /// Key/value properties to shallow-merge into the document's metadata (e.g. description, type,
    /// tags, provenance). Keys overwrite; unmentioned keys are left as-is.
    #[serde(default)]
    props: serde_json::Map<String, Value>,
    #[serde(rename = "principal", alias = "actor")]
    actor: Option<String>,
}

async fn set_document_props(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<SetDocumentPropsBody>,
) -> ApiResult {
    Ok(Json(
        core::set_document_props(
            &st.pool,
            document_id,
            Value::Object(b.props),
            b.actor.as_deref(),
        )
        .await?,
    ))
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
    /// Read the operator-APPROVED version instead of the current one. When the document has no
    /// approved version yet, the read returns 404 `{"error":"no approved version", ...}` (a
    /// distinct not-available signal) rather than silently falling back to the current draft, so a
    /// caller can keep its own fallback until an approved version lands. Mutually exclusive with
    /// `version_no`.
    #[serde(default)]
    approved: Option<bool>,
}

/// Attach `Cache-Control: no-cache` to a response. `no-cache` lets the client/proxy STORE the body
/// but forces revalidation before every reuse; combined with the ETag the document reads already
/// emit, revalidation stays a cheap conditional 304 while guaranteeing an operator_review view
/// never renders a stale version against a freshly published one (task_1360). Without it, an
/// intermediary (the reverse proxy/CDN) or the browser applies heuristic caching and can serve a
/// stale copy on a soft reload. The by-CID `/api/ipfs/{cid}` body stays `immutable` and is untouched
/// -- a CID never changes, so that path is correctly cached forever.
fn no_cache(mut resp: Response) -> Response {
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache"),
    );
    resp
}

/// Does an `If-None-Match` header match our ETag? Handles `*`, a comma-separated list, and a weak
/// (`W/"..."`) validator prefix (we only emit strong ETags, but a client may echo a weak one).
fn if_none_match_matches(header: &str, etag: &str) -> bool {
    let strong = etag.trim_start_matches("W/");
    header.split(',').any(|tok| {
        let tok = tok.trim();
        tok == "*" || tok.trim_start_matches("W/") == strong
    })
}

/// `GET /api/documents/{id}/content` — read a document's body inline (resolves the version's CID
/// through the board's IPFS backend server-side). The agent-usable read path: no local IPFS or
/// separate gateway. Text content comes back as `content`; binary content returns a null `content`
/// + the CID to fetch via `/api/ipfs/{cid}`. Requires `ipfs_api_url` (503 without one).
///
/// `?approved=true` reads the operator-approved version (404 `no approved version` when there is
/// none — never a silent fallback to the current draft). The response carries an `ETag` of the
/// served version's CID; a matching `If-None-Match` returns `304 Not Modified`. In approved mode
/// the ETag is the approved version's CID, so an in-review draft advancing the current version
/// never flips it — a conditional read stays 304 until a NEW version is approved.
async fn read_document_content(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Query(q): Query<DocumentContentQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let value = if q.approved.unwrap_or(false) {
        if q.version_no.is_some() {
            return Err(ApiError(anyhow::anyhow!(
                "give either `approved=true` or `version_no`, not both"
            )));
        }
        match core::read_approved_document_content(
            &st.pool,
            st.ipfs_api_url.as_deref(),
            document_id,
        )
        .await?
        {
            Some(v) => v,
            None => {
                // Distinct not-available: the document exists but has no operator-approved version
                // yet. A caller (the task_815 contract materialize) keys its git-archive fallback
                // off this 404 instead of projecting an unapproved draft.
                return Ok((
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "no approved version", "document_id": document_id })),
                )
                    .into_response());
            }
        }
    } else {
        core::read_document_content(
            &st.pool,
            st.ipfs_api_url.as_deref(),
            document_id,
            q.version_no,
        )
        .await?
    };

    // Conditional read: the ETag is the CID of the version actually served (the approved CID in
    // approved mode), so a client can revalidate cheaply with If-None-Match.
    let etag = value
        .get("cid")
        .and_then(|c| c.as_str())
        .map(|c| format!("\"{c}\""));
    let Some(etag) = etag else {
        return Ok(no_cache(Json(value).into_response()));
    };
    let not_modified = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|inm| if_none_match_matches(inm, &etag));
    let mut resp = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Json(value).into_response()
    };
    if let Ok(hv) = axum::http::HeaderValue::from_str(&etag) {
        resp.headers_mut().insert(axum::http::header::ETAG, hv);
    }
    Ok(no_cache(resp))
}

async fn get_document_versions(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
) -> Result<Response, ApiError> {
    let value = core::get_document_versions(&st.pool, document_id).await?;
    Ok(no_cache(Json(value).into_response()))
}

#[derive(Deserialize, JsonSchema)]
struct PublishVersionBody {
    /// Bare IPFS content id for the new version. Stored verbatim; the board never resolves it.
    /// Optional if `content` is given (and the board has an IPFS backend configured).
    cid: Option<String>,
    /// Raw content for the new version, content-addressed server-side when no `cid` is given.
    /// Supply exactly one of `cid` / `content`.
    content: Option<String>,
    /// A one-line change-note for THIS revision: what changed since the previous version and what to
    /// look for. The operator reads it as the review caption (version list + diff), so always fill
    /// it on a publish -- a blank change-note makes the operator's review slower.
    summary: Option<String>,
    #[serde(rename = "principal", alias = "created_by")]
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
    #[serde(rename = "principal", alias = "actor")]
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
    #[serde(rename = "principal", alias = "author")]
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
    #[serde(rename = "principal", alias = "actor")]
    actor: Option<String>,
    /// Resolve even when the comment is anchored to a text quote still present verbatim in the current
    /// version (task_1418). Default false; pass true to resolve a legitimately-rephrased-in-place
    /// anchor. (Ignored by the annotation-resolve route, which shares this body.)
    #[serde(default)]
    acknowledge: Option<bool>,
}

async fn resolve_comment(
    State(st): State<AppState>,
    Path((DocRef(_document_id), comment_id)): Path<(DocRef, i64)>,
    Json(b): Json<ResolveCommentBody>,
) -> ApiResult {
    Ok(Json(
        core::resolve_comment(
            &st.pool,
            comment_id,
            b.actor.as_deref(),
            b.acknowledge.unwrap_or(false),
            st.ipfs_api_url.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize)]
struct CommentAnnotationsQuery {
    status: Option<String>,
}

async fn get_comment_annotations(
    State(st): State<AppState>,
    Path(comment_id): Path<i64>,
    Query(q): Query<CommentAnnotationsQuery>,
) -> ApiResult {
    Ok(Json(
        core::get_comment_annotations(&st.pool, comment_id, q.status.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct AnnotateCommentBody {
    body: String,
    #[serde(rename = "principal", alias = "author")]
    author: Option<String>,
    /// Free-form JSON anchor selecting a span of the comment (e.g. W3C/Hypothesis selectors). Omit
    /// to annotate the whole comment.
    region: Option<Value>,
    /// Thread this annotation under another (one-level).
    reply_to: Option<i64>,
    /// Optional external identity id (e.g. "slack:U123") this annotation is attributed to.
    external_author: Option<String>,
    /// Submit even if the body contains a banned phrase (the pre-submit lint otherwise rejects it).
    acknowledge_banned: Option<bool>,
}

async fn annotate_comment(
    State(st): State<AppState>,
    Path(comment_id): Path<i64>,
    Json(b): Json<AnnotateCommentBody>,
) -> ApiResult {
    core::check_content(&st.pool, &b.body, b.acknowledge_banned.unwrap_or(false)).await?;
    Ok(Json(
        core::annotate_comment(
            &st.pool,
            comment_id,
            b.author.as_deref(),
            &b.body,
            b.region,
            b.reply_to,
            b.external_author.as_deref(),
        )
        .await?,
    ))
}

async fn resolve_comment_annotation(
    State(st): State<AppState>,
    Path(annotation_id): Path<i64>,
    Json(b): Json<ResolveCommentBody>,
) -> ApiResult {
    Ok(Json(
        core::resolve_comment_annotation(&st.pool, annotation_id, b.actor.as_deref()).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct DocumentActorBody {
    #[serde(rename = "principal", alias = "actor")]
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
    #[serde(rename = "principal", alias = "actor")]
    actor: Option<String>,
    /// The doc template you read and followed. Required unless `template_waiver_reason` is given.
    template_followed: Option<String>,
    /// If no template applies, a non-empty reason why. Required only when `template_followed` is absent.
    template_waiver_reason: Option<String>,
    /// Read-the-guide attestation (the agent id). On a design-doc submission, required unless the
    /// legacy in-body marker is present; stamped into the doc metadata (task_1056).
    read_guide_attested: Option<String>,
    /// Acknowledge-override the placeholder/unfinished-draft check (task_1038); does not skip the
    /// read-the-guide attestation.
    acknowledge: Option<bool>,
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
            b.read_guide_attested.as_deref(),
            b.acknowledge.unwrap_or(false),
            st.ipfs_api_url.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct RequestChangesBody {
    #[serde(rename = "principal", alias = "actor")]
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
struct DeprecateDocumentBody {
    /// Mark deprecated (default true) or, when false, clear the deprecation + supersede link.
    #[serde(default)]
    deprecated: Option<bool>,
    /// The document that supersedes this one (recorded only when deprecating; must exist).
    #[serde(default)]
    superseded_by: Option<i64>,
    #[serde(rename = "principal", alias = "actor")]
    actor: Option<String>,
}

async fn delete_document(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<DocumentActorBody>,
) -> ApiResult {
    Ok(Json(
        core::delete_document(&st.pool, document_id, b.actor.as_deref()).await?,
    ))
}

async fn deprecate_document(
    State(st): State<AppState>,
    Path(DocRef(document_id)): Path<DocRef>,
    Json(b): Json<DeprecateDocumentBody>,
) -> ApiResult {
    Ok(Json(
        core::set_document_deprecated(
            &st.pool,
            document_id,
            b.deprecated.unwrap_or(true),
            b.superseded_by,
            b.actor.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct AttachDocumentBody {
    task_id: i64,
    #[serde(rename = "principal", alias = "actor")]
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
    sse::stream(st.pool.clone(), rx, last_event_id, st.shutdown.clone()).await
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
            shutdown: tokio_util::sync::CancellationToken::new(),
            db_path: String::new(),
            db_snapshot: DbSnapshotCfg::disabled(),
            host_auth: Default::default(),
            link_rules: Default::default(),
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
            shutdown: tokio_util::sync::CancellationToken::new(),
            db_path: String::new(),
            db_snapshot: DbSnapshotCfg::disabled(),
            host_auth: Default::default(),
            link_rules: Default::default(),
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

    /// task_751: an ambiguous resolve_agent returns 400 via the typed core::BoardError downcast at
    /// the boundary (its string-classifier arm was removed in the same change), proving the typed
    /// path maps the status without string-matching.
    #[tokio::test]
    async fn resolve_agent_ambiguous_is_400_via_board_error() -> anyhow::Result<()> {
        use tower::ServiceExt;
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        core::register_agent(&pool, "v-foo", None, None, None, None, None).await?;
        core::register_agent(&pool, "v-foo-helper", None, None, None, None, None).await?;
        let (events_tx, _rx) = broadcast::channel(16);
        let state = AppState {
            pool,
            events_tx,
            ipfs_api_url: None,
            shutdown: tokio_util::sync::CancellationToken::new(),
            db_path: String::new(),
            db_snapshot: DbSnapshotCfg::disabled(),
            host_auth: Default::default(),
            link_rules: Default::default(),
        };
        // "foo" substring-matches BOTH v-foo and v-foo-helper and is not an exact id -> ambiguous.
        let resp = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/resolve-agent?name=foo")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "ambiguous resolve_agent must be a 400 via BoardError"
        );
        Ok(())
    }

    /// task_1243: GET /system/link-rules serves the deployment-configured link-tag rules read-only.
    #[tokio::test]
    async fn system_link_rules_serves_configured_rules() -> anyhow::Result<()> {
        use tower::ServiceExt;
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let (events_tx, _rx) = broadcast::channel(16);
        let state = AppState {
            pool,
            events_tx,
            ipfs_api_url: None,
            shutdown: tokio_util::sync::CancellationToken::new(),
            db_path: String::new(),
            db_snapshot: DbSnapshotCfg::disabled(),
            host_auth: Default::default(),
            link_rules: std::sync::Arc::new(vec![crate::config::LinkRule {
                pattern: "CR-(\\d+)".to_string(),
                url_template: "https://code.example.com/reviews/CR-$1".to_string(),
            }]),
        };
        let resp = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/system/link-rules")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
        let v: Value = serde_json::from_slice(&bytes)?;
        let rules = v["link_rules"].as_array().expect("link_rules array");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["pattern"], json!("CR-(\\d+)"));
        assert_eq!(
            rules[0]["url_template"],
            json!("https://code.example.com/reviews/CR-$1")
        );
        Ok(())
    }

    #[test]
    fn host_authority_name_strips_port_and_unwraps_v6() {
        assert_eq!(
            host_authority_name("board.example.com:8079"),
            "board.example.com"
        );
        assert_eq!(
            host_authority_name("Board.Example.COM"),
            "board.example.com"
        );
        assert_eq!(host_authority_name("127.0.0.1:8079"), "127.0.0.1");
        assert_eq!(host_authority_name("[::1]:8079"), "::1");
        assert_eq!(host_authority_name("localhost"), "localhost");
    }

    #[test]
    fn trusted_user_header_value_only_on_forcing_host_with_header() {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "board.example.com".to_string(),
            Some("x-tunnel-user".to_string()),
        );
        map.insert("127.0.0.1".to_string(), None); // listed but permissive
        let val = |host: &str, user: Option<&str>| {
            let mut h = HeaderMap::new();
            h.insert("host", host.parse().unwrap());
            if let Some(u) = user {
                h.insert("x-tunnel-user", u.parse().unwrap());
            }
            trusted_user_header_value(&h, &map)
        };
        // Forcing host + header -> the raw value (resolution happens in the caller).
        assert_eq!(
            val("board.example.com:8079", Some("bythewc")),
            Some("bythewc".to_string())
        );
        // Permissive host (listed, no auth_header) -> none.
        assert_eq!(val("127.0.0.1:8079", Some("bythewc")), None);
        // Unlisted host -> none.
        assert_eq!(val("other.example.com", Some("bythewc")), None);
        // Forcing host, header absent -> none.
        assert_eq!(val("board.example.com", None), None);
        // No per-host config at all -> never fires.
        assert_eq!(
            trusted_user_header_value(&HeaderMap::new(), &std::collections::HashMap::new()),
            None
        );
    }

    #[test]
    fn board_user_meta_html_escapes_the_username() {
        assert_eq!(
            board_user_meta_html("cameron"),
            r#"<meta name="board-user" content="cameron">"#
        );
        // No attribute/script breakout: &, <, >, " are escaped.
        assert_eq!(
            board_user_meta_html("a\"<b>&"),
            r#"<meta name="board-user" content="a&quot;&lt;b&gt;&amp;">"#
        );
    }

    #[test]
    fn rewrite_acting_fields_overwrites_only_present_acting_keys() {
        // Present acting fields are overwritten; a target field (assignee) and an unrelated field
        // are left untouched; a missing acting field is NOT added (keeps deny_unknown_fields safe).
        let out = rewrite_acting_fields(
            br#"{"author":"evil","requested_by":"evil","assignee":"bob","subscriber":"carol","body":"hi"}"#,
            "alice",
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["author"], json!("alice"));
        assert_eq!(
            v["requested_by"],
            json!("alice"),
            "requested_by acting field forced"
        );
        assert_eq!(v["assignee"], json!("bob"), "target field untouched");
        assert_eq!(
            v["subscriber"],
            json!("carol"),
            "subscriber is a target, not forced"
        );
        assert_eq!(v["body"], json!("hi"));
        assert!(v.get("actor").is_none(), "absent acting field not injected");
        // Non-JSON / non-object bodies pass through unchanged.
        assert_eq!(rewrite_acting_fields(b"not json", "alice"), b"not json");
        assert_eq!(rewrite_acting_fields(b"[1,2]", "alice"), b"[1,2]");
    }

    /// task_1030: a per-host `auth_header` forces the acting user from that header on writes to that
    /// host (overriding a client-set author), a listed host without `auth_header` stays permissive,
    /// and a forcing host missing the header is rejected 401.
    #[tokio::test]
    async fn force_trusted_user_overrides_actor_per_host() -> anyhow::Result<()> {
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
        let mut map = std::collections::HashMap::new();
        map.insert(
            "board.example.com".to_string(),
            Some("x-tunnel-user".to_string()),
        );
        map.insert("127.0.0.1".to_string(), None); // listed but permissive (no auth_header)
        let state = AppState {
            pool,
            events_tx,
            ipfs_api_url: None,
            shutdown: tokio_util::sync::CancellationToken::new(),
            db_path: String::new(),
            db_snapshot: DbSnapshotCfg::disabled(),
            host_auth: std::sync::Arc::new(map),
            link_rules: Default::default(),
        };
        let post = |host: &'static str, user: Option<&'static str>, author: &'static str| {
            let app = router(state.clone());
            let body = serde_json::to_vec(&json!({ "body": "hi", "author": author })).unwrap();
            async move {
                let mut b = axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/tasks/{tid}/comments"))
                    .header("host", host)
                    .header("content-type", "application/json");
                if let Some(u) = user {
                    b = b.header("x-tunnel-user", u);
                }
                app.oneshot(b.body(axum::body::Body::from(body)).unwrap())
                    .await
                    .unwrap()
            }
        };
        // comment_task returns {comment_id,...}; read the stored comment back to see its author.
        let author_of = |resp_body: &Value| {
            let cid = resp_body["comment_id"].as_i64().unwrap();
            let app = router(state.clone());
            async move {
                let resp = app
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(format!("/comments/{cid}"))
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let v: Value = serde_json::from_slice(
                    &axum::body::to_bytes(resp.into_body(), usize::MAX)
                        .await
                        .unwrap(),
                )
                .unwrap();
                v["author"].as_str().map(str::to_string)
            }
        };
        // Forcing host WITH header: the author is forced to the header value, not the body's "evil".
        let resp = post("board.example.com:8079", Some("alice"), "evil").await;
        assert_eq!(resp.status(), StatusCode::OK, "forced write should succeed");
        let v: Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await?)?;
        assert_eq!(
            author_of(&v).await.as_deref(),
            Some("alice"),
            "actor forced from header"
        );
        // Permissive host (listed, no auth_header): the client-set author is preserved.
        let resp = post("127.0.0.1:8079", None, "evil").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v: Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await?)?;
        assert_eq!(
            author_of(&v).await.as_deref(),
            Some("evil"),
            "permissive host keeps the client actor"
        );
        // Forcing host MISSING the header: rejected 401.
        let resp = post("board.example.com", None, "evil").await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "missing trusted header is rejected"
        );
        Ok(())
    }

    /// task_542 B5b: a REST read on a forcing host is scoped to the trusted-header viewer once
    /// enforcement is enabled -- the project creator reads it, a stranger gets not-found (scoped
    /// out, indistinguishable from "no such project"); while enforcement is OFF, both read it.
    #[tokio::test]
    async fn get_project_rest_scopes_to_trusted_viewer_under_enforcement() -> anyhow::Result<()> {
        use tower::ServiceExt;
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let pid = core::create_project(&pool, "P", None, Some("boss"), None).await?["id"]
            .as_i64()
            .unwrap();
        let (events_tx, _rx) = broadcast::channel(16);
        let mut map = std::collections::HashMap::new();
        map.insert(
            "board.example.com".to_string(),
            Some("x-tunnel-user".to_string()),
        );
        let state = AppState {
            pool: pool.clone(),
            events_tx,
            ipfs_api_url: None,
            shutdown: tokio_util::sync::CancellationToken::new(),
            db_path: String::new(),
            db_snapshot: DbSnapshotCfg::disabled(),
            host_auth: std::sync::Arc::new(map),
            link_rules: Default::default(),
        };
        let get = |user: &'static str| {
            let app = router(state.clone());
            async move {
                app.oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/projects/{pid}"))
                        .header("host", "board.example.com:8079")
                        .header("x-tunnel-user", user)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        // Enforcement OFF: both the creator and a stranger read the project.
        assert_eq!(get("boss").await.status(), StatusCode::OK);
        assert_eq!(get("stranger").await.status(), StatusCode::OK);

        // Enable enforcement (the freshly seeded state is enablable).
        core::set_enforcement_enabled(&pool, true, Some("boss")).await?;

        // Creator still reads; the stranger is scoped out to a not-found.
        assert_eq!(
            get("boss").await.status(),
            StatusCode::OK,
            "creator reads under enforcement"
        );
        assert_ne!(
            get("stranger").await.status(),
            StatusCode::OK,
            "a stranger is scoped out under enforcement"
        );
        Ok(())
    }

    /// The approved-version read convenience (task_842): `?approved=true` on a document with no
    /// approved version returns a DISTINCT 404 `no approved version` (never a 503 and never a silent
    /// 200 fall-through to the current draft), and `approved=true` together with `version_no` is a
    /// 400. The 200 + ETag/304 path needs an IPFS backend, exercised at the core layer in
    /// `approved_version_read_is_distinct_and_tracks_approval`; the header-matching logic is
    /// unit-tested in `if_none_match_matches_handles_star_list_and_weak`.
    #[tokio::test]
    async fn content_approved_query_is_distinct_not_available() -> anyhow::Result<()> {
        use tower::ServiceExt;
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = core::create_document(
            &pool,
            "Contract",
            None,
            "bafyv1",
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();
        let (events_tx, _rx) = broadcast::channel(16);
        let state = AppState {
            pool,
            events_tx,
            ipfs_api_url: None,
            shutdown: tokio_util::sync::CancellationToken::new(),
            db_path: String::new(),
            db_snapshot: DbSnapshotCfg::disabled(),
            host_auth: Default::default(),
            link_rules: Default::default(),
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

        // No approved version yet -> a distinct 404 with an explicit no-approved-version body, not a
        // 503 (backend) and not a silent 200 fall-through to the current draft.
        let resp = get(format!("/documents/{did}/content?approved=true")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
        let v: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(v["error"], json!("no approved version"));
        assert_eq!(v["document_id"].as_i64(), Some(did));

        // approved=true AND version_no is a client error (mutually exclusive selectors).
        let resp = get(format!(
            "/documents/{did}/content?approved=true&version_no=1"
        ))
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    /// ETag / If-None-Match matching (task_842): `*` matches anything, a comma list matches any
    /// member, a weak validator matches its strong form, and a non-member misses.
    #[test]
    fn if_none_match_matches_handles_star_list_and_weak() {
        let etag = "\"bafyv1\"";
        assert!(if_none_match_matches("*", etag));
        assert!(if_none_match_matches("\"bafyv1\"", etag));
        assert!(if_none_match_matches("\"other\", \"bafyv1\"", etag));
        assert!(if_none_match_matches("W/\"bafyv1\"", etag));
        assert!(!if_none_match_matches("\"other\"", etag));
        assert!(!if_none_match_matches("\"bafyv2\"", etag));
    }

    /// Dynamic document reads carry `Cache-Control: no-cache` so a reverse proxy / browser must
    /// revalidate before reuse and an operator_review view never renders a stale version against a
    /// freshly published one (task_1360). Asserted on the metadata read and the version-list read
    /// (no IPFS backend needed); `read_document_content` sets the same header on every branch
    /// alongside its existing ETag.
    #[tokio::test]
    async fn document_reads_are_no_cache() -> anyhow::Result<()> {
        use tower::ServiceExt;
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = core::create_document(
            &pool,
            "Contract",
            None,
            "bafyv1",
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();
        let (events_tx, _rx) = broadcast::channel(16);
        let state = AppState {
            pool,
            events_tx,
            ipfs_api_url: None,
            shutdown: tokio_util::sync::CancellationToken::new(),
            db_path: String::new(),
            db_snapshot: DbSnapshotCfg::disabled(),
            host_auth: Default::default(),
            link_rules: Default::default(),
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

        for uri in [
            format!("/documents/{did}"),
            format!("/documents/{did}/versions"),
        ] {
            let resp = get(uri.clone()).await;
            assert_eq!(resp.status(), StatusCode::OK, "{uri} reads OK");
            assert_eq!(
                resp.headers()
                    .get(axum::http::header::CACHE_CONTROL)
                    .and_then(|v| v.to_str().ok()),
                Some("no-cache"),
                "{uri} must be no-cache so a stale version is never served on a soft reload"
            );
        }
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
            shutdown: tokio_util::sync::CancellationToken::new(),
            db_path: String::new(),
            db_snapshot: DbSnapshotCfg::disabled(),
            host_auth: Default::default(),
            link_rules: Default::default(),
        };
        let resp = health(State(state)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        Ok(())
    }

    /// basic_auth_ok accepts exactly the configured credential and rejects a missing, malformed,
    /// wrong-scheme, or wrong-value header.
    #[test]
    fn basic_auth_ok_accepts_only_the_configured_credential() {
        use base64::Engine as _;
        let hdr = |val: &str| {
            let mut h = HeaderMap::new();
            h.insert(
                axum::http::header::AUTHORIZATION,
                val.parse().expect("valid header value"),
            );
            h
        };
        let basic = |u: &str, p: &str| {
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))
            )
        };
        // Correct credential.
        assert!(basic_auth_ok(
            &hdr(&basic("ops", "s3cret")),
            "ops",
            "s3cret"
        ));
        // Wrong password / wrong user.
        assert!(!basic_auth_ok(&hdr(&basic("ops", "nope")), "ops", "s3cret"));
        assert!(!basic_auth_ok(
            &hdr(&basic("eve", "s3cret")),
            "ops",
            "s3cret"
        ));
        // Missing header, wrong scheme, non-base64, no colon.
        assert!(!basic_auth_ok(&HeaderMap::new(), "ops", "s3cret"));
        assert!(!basic_auth_ok(&hdr("Bearer abc"), "ops", "s3cret"));
        assert!(!basic_auth_ok(&hdr("Basic !!!notb64"), "ops", "s3cret"));
        assert!(!basic_auth_ok(
            &hdr(&format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("nocolon")
            )),
            "ops",
            "s3cret"
        ));
    }

    /// The DB-snapshot endpoint is fail-closed and auth-gated: 404 when disabled, 401 without/with a
    /// bad credential, and 200 + a valid SQLite file (correct magic header) with the right credential.
    #[tokio::test]
    async fn db_snapshot_endpoint_is_gated_and_serves_a_valid_db() -> anyhow::Result<()> {
        use base64::Engine as _;
        use tower::ServiceExt;
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("b.db").to_str().unwrap().to_string();
        let pool = crate::db::init(&db_path).await?;
        // Some real content so the snapshot is a non-trivial, consistent DB.
        core::create_project(&pool, "P", None, Some("a"), None).await?;
        let (events_tx, _rx) = broadcast::channel(16);

        let get = |state: AppState, auth: Option<String>| {
            let mut b = axum::http::Request::builder().uri("/admin/db-snapshot");
            if let Some(a) = auth {
                b = b.header(axum::http::header::AUTHORIZATION, a);
            }
            let req = b.body(axum::body::Body::empty()).unwrap();
            async move { router(state).oneshot(req).await.unwrap() }
        };
        let enabled = AppState {
            pool: pool.clone(),
            events_tx: events_tx.clone(),
            ipfs_api_url: None,
            shutdown: tokio_util::sync::CancellationToken::new(),
            db_path: db_path.clone(),
            db_snapshot: DbSnapshotCfg {
                enabled: true,
                user: Some("ops".into()),
                password: Some("s3cret".into()),
            },
            host_auth: Default::default(),
            link_rules: Default::default(),
        };
        let disabled = AppState {
            pool: pool.clone(),
            events_tx,
            ipfs_api_url: None,
            shutdown: tokio_util::sync::CancellationToken::new(),
            db_path: db_path.clone(),
            db_snapshot: DbSnapshotCfg::disabled(),
            host_auth: Default::default(),
            link_rules: Default::default(),
        };
        let good = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("ops:s3cret")
        );
        let bad = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("ops:wrong")
        );

        // Disabled -> 404 (existence hidden).
        assert_eq!(
            get(disabled, Some(good.clone())).await.status(),
            StatusCode::NOT_FOUND
        );
        // Enabled but no/bad credential -> 401.
        assert_eq!(
            get(enabled.clone(), None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get(enabled.clone(), Some(bad)).await.status(),
            StatusCode::UNAUTHORIZED
        );
        // Correct credential -> 200 + a real SQLite file.
        let resp = get(enabled, Some(good)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
        assert!(
            bytes.starts_with(b"SQLite format 3\0"),
            "served body must be a valid SQLite database (magic header)"
        );
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

    fn metrics_test_state(pool: Pool) -> AppState {
        let (events_tx, _rx) = broadcast::channel(16);
        AppState {
            pool,
            events_tx,
            ipfs_api_url: None,
            shutdown: tokio_util::sync::CancellationToken::new(),
            db_path: String::new(),
            db_snapshot: DbSnapshotCfg::disabled(),
            host_auth: Default::default(),
            link_rules: Default::default(),
        }
    }

    /// GET /api/metrics returns a 200 JSON snapshot with the expected top-level shape (task_1380).
    #[tokio::test]
    async fn metrics_endpoint_returns_snapshot_shape() -> anyhow::Result<()> {
        use tower::ServiceExt;
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let resp = router(metrics_test_state(pool))
            .oneshot(
                axum::http::Request::builder()
                    .uri("/metrics")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
        let v: Value = serde_json::from_slice(&bytes)?;
        assert!(v["overall"].is_object(), "has overall rollup");
        assert!(v["endpoints"].is_object(), "has per-endpoint map");
        assert!(v["started_unix"].is_u64(), "has process start");
        assert!(v["uptime_secs"].is_u64(), "has uptime");
        Ok(())
    }

    /// The recording middleware counts a request against its matched-route key, leaves the
    /// in-flight gauge back at zero once it completes (with max_in_flight observed >= 1), and
    /// populates a latency percentile. Uses ONE router instance (cloned per request) so both calls
    /// share the same metrics registry (task_1380).
    #[tokio::test]
    async fn metrics_records_count_and_in_flight_settles() -> anyhow::Result<()> {
        use tower::ServiceExt;
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let app = router(metrics_test_state(pool));

        // A real matched GET; list_projects works against an empty board.
        let r1 = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/projects")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        // Drain the body so the response fully completes before we read metrics.
        let _ = axum::body::to_bytes(r1.into_body(), usize::MAX).await?;

        let r2 = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/metrics")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(r2.into_body(), usize::MAX).await?;
        let v: Value = serde_json::from_slice(&bytes)?;
        let ep = &v["endpoints"]["GET /projects"];
        assert!(
            ep["count"].as_u64().unwrap_or(0) >= 1,
            "GET /projects recorded at least once: {v:#}"
        );
        assert_eq!(ep["in_flight"].as_i64(), Some(0), "in-flight settles to 0");
        assert!(
            ep["max_in_flight"].as_i64().unwrap_or(0) >= 1,
            "max in-flight observed at least 1"
        );
        assert!(ep["p50_ms"].is_u64(), "a latency percentile is present");
        Ok(())
    }
}
