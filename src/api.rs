//! REST API for humans (and the web UI), in addition to the MCP surface. Same core
//! operations, exposed as JSON over HTTP. Auth is deliberately absent for now (LAN,
//! trust-on-first-use) but the router is structured so a middleware layer can be added
//! cleanly later.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use schemars::{schema_for, JsonSchema};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core;
use crate::db::Pool;
use crate::sse::{self, StreamEvent};
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct AppState {
    pub pool: Pool,
    /// Live activity bus: the SSE tailer publishes here, `GET /api/stream` subscribes.
    pub events_tx: broadcast::Sender<StreamEvent>,
}

/// Map an anyhow error to a JSON HTTP response. "no project/task ..." -> 404/400.
struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = self.0.to_string();
        let code = if msg.starts_with("no project") || msg.starts_with("no task") || msg.starts_with("no agent") {
            StatusCode::NOT_FOUND
        } else if msg.starts_with("give ") {
            StatusCode::BAD_REQUEST
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

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/meta", get(meta))
        .route("/agents", get(list_agents).post(register_agent))
        .route("/agents/{agent_id}", get(get_agent).patch(update_agent))
        .route("/agents/{agent_id}/status", post(set_status))
        .route("/agents/{agent_id}/notifications", get(get_notifications))
        .route("/agents/{agent_id}/messages", get(get_messages))
        .route("/projects", get(list_projects).post(create_project))
        .route("/projects/{project_id}", get(get_project).patch(update_project))
        .route("/tasks", get(list_tasks).post(create_task))
        .route("/tasks/{task_id}", get(get_task).patch(update_task))
        .route("/tasks/{task_id}/comments", post(comment_task))
        .route("/tasks/{task_id}/props", patch(set_task_props))
        .route("/tasks/{task_id}/move", post(move_task))
        .route("/subscriptions", post(subscribe).delete(unsubscribe))
        .route("/channels", get(list_channels).post(create_channel))
        .route("/channels/{channel_id}", get(get_channel))
        .route("/channels/{channel_id}/posts", get(get_channel_posts).post(post_to_channel))
        .route("/channels/{channel_id}/invites", post(invite_to_channel))
        .route("/messages", post(send_message))
        .route("/events", get(get_events))
        .route("/stream", get(stream))
        // Unknown /api/* paths return a JSON 404, not the SPA's index.html.
        .fallback(api_not_found)
        .with_state(state)
}

async fn api_not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
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
    Endpoint { method: "GET", path: "/api/health", summary: "Liveness probe.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/meta", summary: "Status vocabularies (task/project/agent).", query: "", body: None },
    Endpoint { method: "GET", path: "/api/agents", summary: "List all known agents.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/agents", summary: "Register (or update) an agent, trust-on-first-use.", query: "", body: Some("RegisterAgentBody") },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}", summary: "Fetch a single agent (including its charter + metadata).", query: "", body: None },
    Endpoint { method: "PATCH", path: "/api/agents/{agent_id}", summary: "Update an agent's fields + metadata (the board agent list as a registry).", query: "", body: Some("UpdateAgentBody") },
    Endpoint { method: "POST", path: "/api/agents/{agent_id}/status", summary: "Set an agent's presence status.", query: "", body: Some("SetStatusBody") },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}/notifications", summary: "Drain an agent's inbox (event notifications).", query: "mark_read=bool&limit=int", body: None },
    Endpoint { method: "GET", path: "/api/agents/{agent_id}/messages", summary: "Read direct messages sent to an agent.", query: "mark_read=bool&limit=int", body: None },
    Endpoint { method: "GET", path: "/api/projects", summary: "List projects (with task counts).", query: "status=str", body: None },
    Endpoint { method: "POST", path: "/api/projects", summary: "Create a project.", query: "", body: Some("CreateProjectBody") },
    Endpoint { method: "GET", path: "/api/projects/{project_id}", summary: "Fetch one project.", query: "", body: None },
    Endpoint { method: "PATCH", path: "/api/projects/{project_id}", summary: "Update a project (rename, archive, description, metadata).", query: "", body: Some("UpdateProjectBody") },
    Endpoint { method: "GET", path: "/api/tasks", summary: "List tasks, optionally filtered.", query: "project_id=int&status=str&assignee=str", body: None },
    Endpoint { method: "POST", path: "/api/tasks", summary: "Create a task.", query: "", body: Some("CreateTaskBody") },
    Endpoint { method: "GET", path: "/api/tasks/{task_id}", summary: "Fetch one task (with comments).", query: "", body: None },
    Endpoint { method: "PATCH", path: "/api/tasks/{task_id}", summary: "Update task fields (status, assignee, ...).", query: "", body: Some("UpdateTaskBody") },
    Endpoint { method: "POST", path: "/api/tasks/{task_id}/comments", summary: "Add a comment to a task.", query: "", body: Some("CommentBody") },
    Endpoint { method: "PATCH", path: "/api/tasks/{task_id}/props", summary: "Merge a JSON object into a task's metadata.", query: "", body: None },
    Endpoint { method: "POST", path: "/api/tasks/{task_id}/move", summary: "Move a task to a different project.", query: "", body: Some("MoveTaskBody") },
    Endpoint { method: "POST", path: "/api/subscriptions", summary: "Subscribe to a task, project, or channel.", query: "", body: Some("SubscribeBody") },
    Endpoint { method: "DELETE", path: "/api/subscriptions", summary: "Unsubscribe from a task, project, or channel.", query: "", body: Some("SubscribeBody") },
    Endpoint { method: "GET", path: "/api/channels", summary: "List channels (public, or a member's incl. private/DM).", query: "member=str", body: None },
    Endpoint { method: "POST", path: "/api/channels", summary: "Create (or get) a named channel.", query: "", body: Some("CreateChannelBody") },
    Endpoint { method: "GET", path: "/api/channels/{channel_id}", summary: "Fetch one channel with its members.", query: "", body: None },
    Endpoint { method: "GET", path: "/api/channels/{channel_id}/posts", summary: "Read a channel's post history.", query: "since_seq=int&limit=int", body: None },
    Endpoint { method: "POST", path: "/api/channels/{channel_id}/posts", summary: "Post a message to a channel.", query: "", body: Some("PostToChannelBody") },
    Endpoint { method: "POST", path: "/api/channels/{channel_id}/invites", summary: "Invite an agent into a channel (auto-join + notify).", query: "", body: Some("InviteChannelBody") },
    Endpoint { method: "POST", path: "/api/messages", summary: "Send a direct message between agents.", query: "", body: Some("SendMessageBody") },
    Endpoint { method: "GET", path: "/api/events", summary: "Read the append-only event log.", query: "since_seq=int&limit=int", body: None },
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
        CreateProjectBody,
        UpdateProjectBody,
        CreateTaskBody,
        UpdateTaskBody,
        MoveTaskBody,
        CommentBody,
        SubscribeBody,
        CreateChannelBody,
        PostToChannelBody,
        InviteChannelBody,
        SendMessageBody,
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
            Some(name) => format!("<a class=\"schema\" href=\"#schema-{n}\">{n}</a>", n = html_escape(name)),
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

async fn list_agents(State(st): State<AppState>) -> ApiResult {
    Ok(Json(core::list_agents(&st.pool).await?))
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

async fn register_agent(
    State(st): State<AppState>,
    Json(b): Json<RegisterAgentBody>,
) -> ApiResult {
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
}

async fn update_agent(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(b): Json<UpdateAgentBody>,
) -> ApiResult {
    Ok(Json(
        core::update_agent(
            &st.pool,
            &agent_id,
            b.display_name.as_deref(),
            b.kind.as_deref(),
            b.charter.as_deref(),
            b.status.as_deref(),
            b.status_message.as_deref(),
            b.webhook_url.as_deref(),
            b.metadata,
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct SetStatusBody {
    status: String,
    status_message: Option<String>,
}

async fn set_status(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(b): Json<SetStatusBody>,
) -> ApiResult {
    Ok(Json(
        core::set_status(&st.pool, &agent_id, &b.status, b.status_message.as_deref()).await?,
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
    Ok(Json(core::list_projects(&st.pool, q.status.as_deref()).await?))
}

#[derive(Deserialize, JsonSchema)]
struct CreateProjectBody {
    name: String,
    description: Option<String>,
    created_by: Option<String>,
    metadata: Option<Value>,
}

async fn create_project(
    State(st): State<AppState>,
    Json(b): Json<CreateProjectBody>,
) -> ApiResult {
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

async fn get_project(State(st): State<AppState>, Path(project_id): Path<i64>) -> ApiResult {
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
    Path(project_id): Path<i64>,
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
}

async fn list_tasks(State(st): State<AppState>, Query(q): Query<ListTasksQuery>) -> ApiResult {
    Ok(Json(
        core::list_tasks(&st.pool, q.project_id, q.status.as_deref(), q.assignee.as_deref()).await?,
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
        )
        .await?,
    ))
}

async fn get_task(State(st): State<AppState>, Path(task_id): Path<i64>) -> ApiResult {
    found(core::get_task(&st.pool, task_id).await?)
}

#[derive(Deserialize, JsonSchema)]
struct UpdateTaskBody {
    status: Option<String>,
    assignee: Option<String>,
    title: Option<String>,
    description: Option<String>,
    priority: Option<String>,
    actor: Option<String>,
    metadata: Option<Value>,
}

async fn update_task(
    State(st): State<AppState>,
    Path(task_id): Path<i64>,
    Json(b): Json<UpdateTaskBody>,
) -> ApiResult {
    Ok(Json(
        core::update_task(
            &st.pool,
            task_id,
            b.status.as_deref(),
            b.assignee.as_deref(),
            b.title.as_deref(),
            b.description.as_deref(),
            b.priority.as_deref(),
            b.actor.as_deref(),
            b.metadata,
        )
        .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct CommentBody {
    body: String,
    author: Option<String>,
}

async fn comment_task(
    State(st): State<AppState>,
    Path(task_id): Path<i64>,
    Json(b): Json<CommentBody>,
) -> ApiResult {
    Ok(Json(
        core::comment_task(&st.pool, task_id, &b.body, b.author.as_deref()).await?,
    ))
}

async fn set_task_props(
    State(st): State<AppState>,
    Path(task_id): Path<i64>,
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
    Path(task_id): Path<i64>,
    Json(b): Json<MoveTaskBody>,
) -> ApiResult {
    Ok(Json(
        core::move_task(&st.pool, task_id, b.to_project_id, b.actor.as_deref()).await?,
    ))
}

// --- Subscriptions ---

#[derive(Deserialize, JsonSchema)]
struct SubscribeBody {
    subscriber: String,
    task_id: Option<i64>,
    project_id: Option<i64>,
    /// Subscribe to a channel (join it). Give exactly one of task_id / project_id / channel_id.
    channel_id: Option<i64>,
}

async fn subscribe(State(st): State<AppState>, Json(b): Json<SubscribeBody>) -> ApiResult {
    Ok(Json(
        core::subscribe(&st.pool, &b.subscriber, b.task_id, b.project_id, b.channel_id).await?,
    ))
}

async fn unsubscribe(State(st): State<AppState>, Json(b): Json<SubscribeBody>) -> ApiResult {
    Ok(Json(
        core::unsubscribe(&st.pool, &b.subscriber, b.task_id, b.project_id, b.channel_id).await?,
    ))
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
    Ok(Json(core::list_channels(&st.pool, q.member.as_deref()).await?))
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

async fn get_channel(State(st): State<AppState>, Path(channel_id): Path<i64>) -> ApiResult {
    found(core::get_channel(&st.pool, channel_id).await?)
}

#[derive(Deserialize, JsonSchema)]
struct ChannelPostsQuery {
    #[serde(default)]
    since_seq: i64,
    #[serde(default = "default_events_limit")]
    limit: i64,
}

async fn get_channel_posts(
    State(st): State<AppState>,
    Path(channel_id): Path<i64>,
    Query(q): Query<ChannelPostsQuery>,
) -> ApiResult {
    Ok(Json(
        core::get_channel_posts(&st.pool, channel_id, q.since_seq, q.limit).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct PostToChannelBody {
    sender: String,
    body: String,
    reply_to: Option<i64>,
}

async fn post_to_channel(
    State(st): State<AppState>,
    Path(channel_id): Path<i64>,
    Json(b): Json<PostToChannelBody>,
) -> ApiResult {
    Ok(Json(
        core::post_to_channel(&st.pool, channel_id, &b.sender, &b.body, b.reply_to).await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct InviteChannelBody {
    agent_id: String,
    invited_by: Option<String>,
}

async fn invite_to_channel(
    State(st): State<AppState>,
    Path(channel_id): Path<i64>,
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
struct EventsQuery {
    #[serde(default)]
    since_seq: i64,
    #[serde(default = "default_events_limit")]
    limit: i64,
}

async fn get_events(State(st): State<AppState>, Query(q): Query<EventsQuery>) -> ApiResult {
    Ok(Json(core::get_events(&st.pool, q.since_seq, q.limit).await?))
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

        // Each `.route("PATH", get(..).post(..))` line contributes one (METHOD, PATH)
        // pair per HTTP-method combinator it names.
        let mut from_router: BTreeSet<(String, String)> = BTreeSet::new();
        for line in body.lines() {
            let Some(after) = line.split_once(".route(\"").map(|x| x.1) else {
                continue;
            };
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
