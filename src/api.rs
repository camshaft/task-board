//! REST API for humans (and the web UI), in addition to the MCP surface. Same core
//! operations, exposed as JSON over HTTP. Auth is deliberately absent for now (LAN,
//! trust-on-first-use) but the router is structured so a middleware layer can be added
//! cleanly later.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core;
use crate::db::Pool;

#[derive(Clone)]
pub struct AppState {
    pub pool: Pool,
}

/// Map an anyhow error to a JSON HTTP response. "no project/task ..." -> 404/400.
struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = self.0.to_string();
        let code = if msg.starts_with("no project") || msg.starts_with("no task") {
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
        .route("/health", get(health))
        .route("/meta", get(meta))
        .route("/agents", get(list_agents).post(register_agent))
        .route("/agents/{agent_id}/status", post(set_status))
        .route("/agents/{agent_id}/notifications", get(get_notifications))
        .route("/agents/{agent_id}/messages", get(get_messages))
        .route("/projects", get(list_projects).post(create_project))
        .route("/projects/{project_id}", get(get_project))
        .route("/tasks", get(list_tasks).post(create_task))
        .route("/tasks/{task_id}", get(get_task).patch(update_task))
        .route("/tasks/{task_id}/comments", post(comment_task))
        .route("/tasks/{task_id}/props", patch(set_task_props))
        .route("/subscriptions", post(subscribe).delete(unsubscribe))
        .route("/messages", post(send_message))
        .route("/events", get(get_events))
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

// --- Agents ---

async fn list_agents(State(st): State<AppState>) -> ApiResult {
    Ok(Json(core::list_agents(&st.pool).await?))
}

#[derive(Deserialize)]
struct RegisterAgentBody {
    agent_id: String,
    display_name: Option<String>,
    kind: Option<String>,
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
            b.webhook_url.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize)]
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

#[derive(Deserialize)]
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

#[derive(Deserialize)]
struct ListProjectsQuery {
    status: Option<String>,
}

async fn list_projects(
    State(st): State<AppState>,
    Query(q): Query<ListProjectsQuery>,
) -> ApiResult {
    Ok(Json(core::list_projects(&st.pool, q.status.as_deref()).await?))
}

#[derive(Deserialize)]
struct CreateProjectBody {
    name: String,
    description: Option<String>,
    created_by: Option<String>,
}

async fn create_project(
    State(st): State<AppState>,
    Json(b): Json<CreateProjectBody>,
) -> ApiResult {
    Ok(Json(
        core::create_project(&st.pool, &b.name, b.description.as_deref(), b.created_by.as_deref())
            .await?,
    ))
}

async fn get_project(State(st): State<AppState>, Path(project_id): Path<i64>) -> ApiResult {
    found(core::get_project(&st.pool, project_id).await?)
}

// --- Tasks ---

#[derive(Deserialize)]
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

#[derive(Deserialize)]
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

#[derive(Deserialize)]
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

#[derive(Deserialize)]
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

// --- Subscriptions ---

#[derive(Deserialize)]
struct SubscribeBody {
    subscriber: String,
    task_id: Option<i64>,
    project_id: Option<i64>,
}

async fn subscribe(State(st): State<AppState>, Json(b): Json<SubscribeBody>) -> ApiResult {
    Ok(Json(
        core::subscribe(&st.pool, &b.subscriber, b.task_id, b.project_id).await?,
    ))
}

async fn unsubscribe(State(st): State<AppState>, Json(b): Json<SubscribeBody>) -> ApiResult {
    Ok(Json(
        core::unsubscribe(&st.pool, &b.subscriber, b.task_id, b.project_id).await?,
    ))
}

// --- Messages / events ---

#[derive(Deserialize)]
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

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    since_seq: i64,
    #[serde(default = "default_events_limit")]
    limit: i64,
}

async fn get_events(State(st): State<AppState>, Query(q): Query<EventsQuery>) -> ApiResult {
    Ok(Json(core::get_events(&st.pool, q.since_seq, q.limit).await?))
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
