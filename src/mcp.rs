//! MCP server exposing the task board to agents over streamable-HTTP.
//!
//! Identity is trust-on-first-use (LAN, no auth yet): tools that act on someone's behalf
//! take an explicit agent id (you pass your own handle). Reads return pretty JSON; writes
//! return the new/affected ids. A faithful port of the Python `board.server` tool surface.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::Deserialize;
use serde_json::Value;

use crate::core;
use crate::db::Pool;

/// A free-form JSON object argument (task metadata / props). We type these as a map rather
/// than a bare `serde_json::Value` so schemars emits a concrete `{"type":"object"}` schema:
/// a bare Value serializes to a boolean/empty schema that some strict MCP clients (incl.
/// Claude Code) reject when it appears as a named property, which fails the whole
/// tools/list. Callers only ever pass objects here anyway (merged key/value maps).
type JsonObject = serde_json::Map<String, Value>;

#[derive(Clone)]
pub struct Board {
    pool: Pool,
    // Populated and consumed by the #[tool_router]/#[tool_handler] macros.
    #[allow(dead_code)]
    tool_router: ToolRouter<Board>,
}

/// Pretty-print like the Python `_j` (indent=2, default=str).
fn j(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn ok(v: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(j(&v))]))
}

fn err(e: anyhow::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

// --- Parameter structs (one per tool that takes args) ---

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RegisterAgentArgs {
    /// Stable handle others address you by (e.g. 'agent:fixer-3').
    pub agent_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    /// Free-form charter: your role, mission, and scope on this board. Editable over time;
    /// re-registering without it won't erase an existing charter.
    #[serde(default)]
    pub charter: Option<String>,
    /// Arbitrary registry props (role, model, effort, interval, worktree, area, and
    /// `repos: [{repo, branch}, ...]` — an agent may span several repos, each checked out
    /// in its own workspace). MERGED into any existing bag, not replaced.
    #[serde(default)]
    pub metadata: Option<JsonObject>,
    /// If set, every event delivered to your inbox is also POSTed here (best-effort).
    #[serde(default)]
    pub webhook_url: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetAgentArgs {
    pub agent_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateAgentArgs {
    pub agent_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub charter: Option<String>,
    /// online / busy / away / offline
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub status_message: Option<String>,
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// MERGED into the agent's registry bag, not replaced.
    #[serde(default)]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetStatusArgs {
    pub agent_id: String,
    /// online / busy / away / offline
    pub status: String,
    #[serde(default)]
    pub status_message: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateProjectArgs {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
    /// Arbitrary project properties (e.g. {"repo": "https://github.com/org/repo"}).
    #[serde(default)]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateProjectArgs {
    pub project_id: i64,
    /// New name (must be unique case-insensitively).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// active / archived. Archiving hides it from the sidebar; fully reversible.
    #[serde(default)]
    pub status: Option<String>,
    /// MERGED into the project's props (e.g. a repo link), not replaced.
    #[serde(default)]
    pub metadata: Option<JsonObject>,
    /// Set to your agent id so you aren't notified of your own change.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveTaskArgs {
    pub task_id: i64,
    /// The project to move the task into.
    pub to_project_id: i64,
    /// Set to your agent id so you aren't notified of your own change.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListProjectsArgs {
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetProjectArgs {
    pub project_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateTaskArgs {
    pub project_id: i64,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
    /// Arbitrary properties (pipeline state, source, ipfs_cid, target collection, ...).
    #[serde(default)]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateTaskArgs {
    pub task_id: i64,
    /// todo / in_progress / blocked / done / cancelled
    #[serde(default)]
    pub status: Option<String>,
    /// New owner's agent id. Pass "" (empty string) to unassign (clear the owner).
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    /// Set to your agent id so you aren't notified of your own change.
    #[serde(default)]
    pub actor: Option<String>,
    /// MERGED into the task's props.
    #[serde(default)]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetTaskPropsArgs {
    pub task_id: i64,
    /// Key/value properties to merge into the task's metadata.
    pub props: JsonObject,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetTaskArgs {
    pub task_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListTasksArgs {
    #[serde(default)]
    pub project_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    /// Only tasks with no assignee (assignee IS NULL). Takes precedence over `assignee`.
    #[serde(default)]
    pub unassigned: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CommentTaskArgs {
    pub task_id: i64,
    pub body: String,
    #[serde(default)]
    pub author: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SubscribeArgs {
    pub subscriber: String,
    #[serde(default)]
    pub task_id: Option<i64>,
    #[serde(default)]
    pub project_id: Option<i64>,
    /// Subscribe to a channel (join it). Give exactly one of task_id / project_id / channel_id /
    /// document_id, or set `board: true`.
    #[serde(default)]
    pub channel_id: Option<i64>,
    /// Subscribe to a document (its versions + review activity).
    #[serde(default)]
    pub document_id: Option<i64>,
    /// Whole-board firehose: subscribe to EVERY event on the board (for a coordinator/auto-assigner).
    #[serde(default)]
    pub board: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateChannelArgs {
    /// Channel name (unique case-insensitively; e.g. 'general', 'planning').
    pub name: String,
    #[serde(default)]
    pub topic: Option<String>,
    /// Your agent id — auto-joined as the first member.
    #[serde(default)]
    pub created_by: Option<String>,
    /// Arbitrary channel properties.
    #[serde(default)]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListChannelsArgs {
    /// If set, list channels this agent belongs to (incl. private/DM). Omit for public only.
    #[serde(default)]
    pub member: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetChannelArgs {
    pub channel_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PostToChannelArgs {
    pub channel_id: i64,
    /// Your agent id (the poster).
    pub sender: String,
    pub body: String,
    /// Optional parent post seq to reply under (one-level threading).
    #[serde(default)]
    pub reply_to: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetChannelPostsArgs {
    pub channel_id: i64,
    #[serde(default)]
    pub since_seq: i64,
    #[serde(default = "default_events_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InviteToChannelArgs {
    pub channel_id: i64,
    /// The agent to invite (auto-joined).
    pub agent_id: String,
    /// Your agent id (the inviter).
    #[serde(default)]
    pub invited_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CheckNotificationsArgs {
    pub agent_id: String,
    #[serde(default = "default_true")]
    pub mark_read: bool,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendMessageArgs {
    pub from_agent: String,
    pub to_agent: String,
    pub body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetMessagesArgs {
    pub agent_id: String,
    #[serde(default = "default_true")]
    pub mark_read: bool,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetEventsArgs {
    #[serde(default)]
    pub since_seq: i64,
    #[serde(default = "default_events_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateDocumentArgs {
    pub title: String,
    /// Bare IPFS content id for version 1. Stored verbatim; the board never resolves it.
    pub cid: String,
    /// Optionally attach the document to a project.
    #[serde(default)]
    pub project_id: Option<i64>,
    /// Short note describing this version.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
    /// Arbitrary props (tags, etc). MERGED is not applicable on create — set the initial bag.
    #[serde(default)]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PublishVersionArgs {
    pub document_id: i64,
    /// Bare IPFS content id for the new version. Stored verbatim; the board never resolves it.
    pub cid: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetDocumentArgs {
    pub document_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListDocumentsArgs {
    #[serde(default)]
    pub project_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CommentDocumentArgs {
    pub document_id: i64,
    pub body: String,
    /// The version this comment is written against (anchors the region to immutable content).
    #[serde(default)]
    pub version_id: Option<i64>,
    #[serde(default)]
    pub author: Option<String>,
    /// Free-form JSON anchor (e.g. W3C/Hypothesis selectors). Stored verbatim; omit for a
    /// doc-level comment.
    #[serde(default)]
    pub region: Option<JsonObject>,
    /// Thread this comment under another (one-level).
    #[serde(default)]
    pub reply_to: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResolveCommentArgs {
    pub comment_id: i64,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetDocumentCommentsArgs {
    pub document_id: i64,
    #[serde(default)]
    pub version_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocumentActorArgs {
    pub document_id: i64,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RequestChangesArgs {
    pub document_id: i64,
    #[serde(default)]
    pub actor: Option<String>,
    /// Optional note explaining what needs to change.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AttachDocumentArgs {
    pub document_id: i64,
    pub task_id: i64,
    #[serde(default)]
    pub actor: Option<String>,
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

fn s(o: &Option<String>) -> Option<&str> {
    o.as_deref()
}

#[tool_router]
impl Board {
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            tool_router: Self::tool_router(),
        }
    }

    // --- Agents / presence ---
    #[tool(
        description = "Register (or update) yourself and mark yourself online. `agent_id` is the stable handle others address you by (e.g. 'agent:fixer-3'). `charter` is your role/mission (free-form). `metadata` is an optional dict of registry props (role, model, effort, interval, worktree, area, and `repos: [{repo, branch}, ...]` for the repos you work in — one workspace checkout each), MERGED into any existing bag. If you set `webhook_url`, every event delivered to your inbox is also POSTed there (best-effort)."
    )]
    async fn register_agent(
        &self,
        Parameters(a): Parameters<RegisterAgentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::register_agent(
            &self.pool,
            &a.agent_id,
            s(&a.display_name),
            s(&a.kind),
            s(&a.charter),
            a.metadata.map(Value::Object),
            s(&a.webhook_url),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "Set your presence: online / busy / away / offline (+ an optional note).")]
    async fn set_status(
        &self,
        Parameters(a): Parameters<SetStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::set_status(&self.pool, &a.agent_id, &a.status, s(&a.status_message))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "List all registered agents with their presence, charter, metadata, and last-seen time.")]
    async fn list_agents(&self) -> Result<CallToolResult, McpError> {
        core::list_agents(&self.pool).await.map_err(err).and_then(ok)
    }

    #[tool(description = "Get one agent by id, including its charter and metadata bag. O(1) vs filtering list_agents.")]
    async fn get_agent(
        &self,
        Parameters(a): Parameters<GetAgentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_agent(&self.pool, &a.agent_id).await.map_err(err).and_then(ok)
    }

    #[tool(
        description = "Update an existing agent's fields + metadata WITHOUT re-registering (this is the registry-write path: the board agent list serves as the fleet registry). Pass only the fields you're changing. `metadata` is MERGED into the existing bag, not replaced. Unlike register_agent this does not force status online and fails if the agent doesn't exist."
    )]
    async fn update_agent(
        &self,
        Parameters(a): Parameters<UpdateAgentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::update_agent(
            &self.pool,
            &a.agent_id,
            s(&a.display_name),
            s(&a.kind),
            s(&a.charter),
            s(&a.status),
            s(&a.status_message),
            s(&a.webhook_url),
            a.metadata.map(Value::Object),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    // --- Projects ---
    #[tool(
        description = "Create a project (a container for tasks). `metadata` is an optional dict of arbitrary properties (e.g. {\"repo\": \"https://github.com/org/repo\"}). Returns the new project, incl. its id."
    )]
    async fn create_project(
        &self,
        Parameters(a): Parameters<CreateProjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::create_project(
            &self.pool,
            &a.name,
            s(&a.description),
            s(&a.created_by),
            a.metadata.map(Value::Object),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "List projects (optionally filtered by status) with per-status task counts.")]
    async fn list_projects(
        &self,
        Parameters(a): Parameters<ListProjectsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_projects(&self.pool, s(&a.status)).await.map_err(err).and_then(ok)
    }

    #[tool(description = "Get one project and its tasks.")]
    async fn get_project(
        &self,
        Parameters(a): Parameters<GetProjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_project(&self.pool, a.project_id).await.map_err(err).and_then(ok)
    }

    #[tool(
        description = "Update a project: rename it, edit its description, set metadata (MERGED, e.g. a repo link), or change its status. Set status='archived' to hide it from the board (reversible; set 'active' to restore). Pass only the fields you're changing. Set `actor` to your agent id so you aren't notified of your own change."
    )]
    async fn update_project(
        &self,
        Parameters(a): Parameters<UpdateProjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::update_project(
            &self.pool,
            a.project_id,
            s(&a.name),
            s(&a.description),
            s(&a.status),
            a.metadata.map(Value::Object),
            s(&a.actor),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    // --- Tasks ---
    #[tool(
        description = "Create a task in a project. The creator and assignee are auto-subscribed, so they get notified of future changes. `metadata` is an optional dict of arbitrary properties (pipeline state, source, ipfs_cid, target collection, ...). Returns the new task incl. its id."
    )]
    async fn create_task(
        &self,
        Parameters(a): Parameters<CreateTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::create_task(
            &self.pool,
            a.project_id,
            &a.title,
            s(&a.description),
            s(&a.assignee),
            s(&a.priority),
            s(&a.created_by),
            a.metadata.map(Value::Object),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Update a task. Pass only the fields you're changing. Statuses: todo / in_progress / blocked / done / cancelled. Set `assignee` to \"\" (empty string) to unassign (clear the owner) — this emits task.unassigned; setting a non-empty owner emits task.assigned. `metadata` is MERGED into the task's props. Set `actor` to your agent id so you aren't notified of your own change. Notifies subscribers on status/assignee changes (e.g. reassign to hand a ticket to the next pipeline stage)."
    )]
    async fn update_task(
        &self,
        Parameters(a): Parameters<UpdateTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::update_task(
            &self.pool,
            a.task_id,
            s(&a.status),
            s(&a.assignee),
            s(&a.title),
            s(&a.description),
            s(&a.priority),
            s(&a.actor),
            a.metadata.map(Value::Object),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Merge arbitrary key/value properties into a task's metadata (JSON) without touching its status/assignee — e.g. {\"ipfs_cid\": \"bafy...\", \"collection\": \"crate.tokio.1.53\"}. Returns the merged metadata."
    )]
    async fn set_task_props(
        &self,
        Parameters(a): Parameters<SetTaskPropsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::set_task_props(&self.pool, a.task_id, Value::Object(a.props))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Move a task to a different project. Notifies the task's subscribers. Set `actor` to your agent id so you aren't notified of your own change."
    )]
    async fn move_task(
        &self,
        Parameters(a): Parameters<MoveTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::move_task(&self.pool, a.task_id, a.to_project_id, s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Get one task with its comments and subscribers.")]
    async fn get_task(
        &self,
        Parameters(a): Parameters<GetTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_task(&self.pool, a.task_id).await.map_err(err).and_then(ok)
    }

    #[tool(description = "List tasks, optionally filtered by project, status, and/or assignee. Pass `unassigned: true` to list only tasks with no assignee (takes precedence over `assignee`).")]
    async fn list_tasks(
        &self,
        Parameters(a): Parameters<ListTasksArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_tasks(&self.pool, a.project_id, s(&a.status), s(&a.assignee), a.unassigned.unwrap_or(false))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Add a comment to a task. Notifies the task's subscribers/assignee (except you).")]
    async fn comment_task(
        &self,
        Parameters(a): Parameters<CommentTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::comment_task(&self.pool, a.task_id, &a.body, s(&a.author))
            .await
            .map_err(err)
            .and_then(ok)
    }

    // --- Subscriptions ---
    #[tool(description = "Subscribe an agent to a task, a project, a channel, a document, OR the whole board so it's notified of activity there. Give exactly one of task_id / project_id / channel_id / document_id, or set `board: true` for the whole-board firehose (every event — for a coordinator/auto-assigner). Subscribing to a channel joins it.")]
    async fn subscribe(
        &self,
        Parameters(a): Parameters<SubscribeArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::subscribe(&self.pool, &a.subscriber, a.task_id, a.project_id, a.channel_id, a.document_id, a.board.unwrap_or(false))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Stop notifying an agent about a task, project, channel (leaving a channel), document, or the whole board (board: true).")]
    async fn unsubscribe(
        &self,
        Parameters(a): Parameters<SubscribeArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::unsubscribe(&self.pool, &a.subscriber, a.task_id, a.project_id, a.channel_id, a.document_id, a.board.unwrap_or(false))
            .await
            .map_err(err)
            .and_then(ok)
    }

    // --- Channels ---
    #[tool(
        description = "Create (or get) a named channel: a discussion topic agents post to and subscribe to. The creator auto-joins. `topic` is a free-form description; `metadata` an optional props dict. Returns the channel incl. its id and members."
    )]
    async fn create_channel(
        &self,
        Parameters(a): Parameters<CreateChannelArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::create_channel(
            &self.pool,
            &a.name,
            s(&a.topic),
            s(&a.created_by),
            a.metadata.map(Value::Object),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "List channels. Public channels are always shown; private channels (incl. DMs) only when `member` is set to an agent that belongs to them. Set `member` to your id to list just the channels you're in.")]
    async fn list_channels(
        &self,
        Parameters(a): Parameters<ListChannelsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_channels(&self.pool, s(&a.member)).await.map_err(err).and_then(ok)
    }

    #[tool(description = "Get one channel with its member list.")]
    async fn get_channel(
        &self,
        Parameters(a): Parameters<GetChannelArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_channel(&self.pool, a.channel_id).await.map_err(err).and_then(ok)
    }

    #[tool(
        description = "Post a message to a channel. You're auto-joined on posting. Every member's inbox gets it (drain with check_notifications). `reply_to` optionally threads under a parent post's seq (one level)."
    )]
    async fn post_to_channel(
        &self,
        Parameters(a): Parameters<PostToChannelArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::post_to_channel(&self.pool, a.channel_id, &a.sender, &a.body, a.reply_to)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Read a channel's post history after `since_seq` (oldest first). Use this to catch up on a channel you just joined — the inbox only holds what arrived after you joined.")]
    async fn get_channel_posts(
        &self,
        Parameters(a): Parameters<GetChannelPostsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_channel_posts(&self.pool, a.channel_id, a.since_seq, a.limit)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Invite another agent into a channel: they're auto-joined and get a channel.invite in their inbox (no accept step). They can unsubscribe to leave.")]
    async fn invite_to_channel(
        &self,
        Parameters(a): Parameters<InviteToChannelArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::invite_to_channel(&self.pool, a.channel_id, &a.agent_id, s(&a.invited_by))
            .await
            .map_err(err)
            .and_then(ok)
    }

    // --- Notifications / direct messages ---
    #[tool(
        description = "Drain your inbox: the unread events on things you're subscribed to, plus direct messages sent to you. This is the primary way to 'get notified' — call it when you check in. Marks them read unless mark_read=false."
    )]
    async fn check_notifications(
        &self,
        Parameters(a): Parameters<CheckNotificationsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::check_notifications(&self.pool, &a.agent_id, a.mark_read, a.limit, None)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Send a direct message to another agent (lands in their inbox; webhook-pushed if they registered one)."
    )]
    async fn send_message(
        &self,
        Parameters(a): Parameters<SendMessageArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::send_message(&self.pool, &a.from_agent, &a.to_agent, &a.body)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Just your direct messages (a filtered view of your inbox).")]
    async fn get_messages(
        &self,
        Parameters(a): Parameters<GetMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_messages(&self.pool, &a.agent_id, a.mark_read, a.limit)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Read the raw append-only event log after `since_seq` (the audit trail of everything).")]
    async fn get_events(
        &self,
        Parameters(a): Parameters<GetEventsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_events(&self.pool, a.since_seq, a.limit).await.map_err(err).and_then(ok)
    }

    // --- Documents ---
    #[tool(
        description = "Create a versioned document. Content lives on IPFS: pass `cid` (a bare content id) — the board stores the identifier only and NEVER resolves it or composes a URL (the client does). Optionally attach it to a `project_id` and set `metadata` (tags, etc). Creates version 1. Returns the document with its current version + version list."
    )]
    async fn create_document(
        &self,
        Parameters(a): Parameters<CreateDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::create_document(
            &self.pool,
            &a.title,
            a.project_id,
            &a.cid,
            s(&a.summary),
            s(&a.created_by),
            a.metadata.map(Value::Object),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Publish a new immutable version of a document from a `cid` (bare content id; the board does not resolve it). Appends the version, advances the current pointer, and returns the updated document. A new version drops an approved/changes_requested doc back to in_review."
    )]
    async fn publish_version(
        &self,
        Parameters(a): Parameters<PublishVersionArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::publish_version(&self.pool, a.document_id, &a.cid, s(&a.summary), s(&a.created_by))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Get one document with its current version and full version list (each version is a bare CID + summary).")]
    async fn get_document(
        &self,
        Parameters(a): Parameters<GetDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_document(&self.pool, a.document_id).await.map_err(err).and_then(ok)
    }

    #[tool(description = "List a document's versions (immutable), newest first.")]
    async fn get_document_versions(
        &self,
        Parameters(a): Parameters<GetDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_document_versions(&self.pool, a.document_id).await.map_err(err).and_then(ok)
    }

    #[tool(description = "List documents, optionally filtered by project and/or status (draft / in_review / approved / changes_requested).")]
    async fn list_documents(
        &self,
        Parameters(a): Parameters<ListDocumentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_documents(&self.pool, a.project_id, s(&a.status)).await.map_err(err).and_then(ok)
    }

    #[tool(
        description = "Comment on a document, optionally anchored to a region of a specific version. `region` is a free-form JSON selector object (e.g. W3C/Hypothesis TextQuote + TextPosition) stored verbatim — omit it for a doc-level comment. `reply_to` threads under another comment. Auto-subscribes you to the document and notifies its subscribers."
    )]
    async fn comment_document(
        &self,
        Parameters(a): Parameters<CommentDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::comment_document(
            &self.pool,
            a.document_id,
            a.version_id,
            s(&a.author),
            &a.body,
            a.region.map(Value::Object),
            a.reply_to,
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "Mark a document comment resolved (open -> resolved). Notifies the document's subscribers.")]
    async fn resolve_comment(
        &self,
        Parameters(a): Parameters<ResolveCommentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::resolve_comment(&self.pool, a.comment_id, s(&a.actor)).await.map_err(err).and_then(ok)
    }

    #[tool(description = "List a document's comments (oldest first), optionally filtered by version_id and/or status (open / resolved).")]
    async fn get_document_comments(
        &self,
        Parameters(a): Parameters<GetDocumentCommentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_document_comments(&self.pool, a.document_id, a.version_id, s(&a.status))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Submit a document for review (status -> in_review). Notifies the document's subscribers.")]
    async fn submit_for_review(
        &self,
        Parameters(a): Parameters<DocumentActorArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::submit_for_review(&self.pool, a.document_id, s(&a.actor)).await.map_err(err).and_then(ok)
    }

    #[tool(description = "Request changes on a document (status -> changes_requested), with an optional note. Notifies the author + subscribers; the author then publishes a new version.")]
    async fn request_changes(
        &self,
        Parameters(a): Parameters<RequestChangesArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::request_changes(&self.pool, a.document_id, s(&a.actor), s(&a.note))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Approve a document: stamps the current version as approved (approved_version_id + approved_by) and sets status=approved. Not a lock — publishing a new version reopens review. Notifies subscribers.")]
    async fn approve_document(
        &self,
        Parameters(a): Parameters<DocumentActorArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::approve_document(&self.pool, a.document_id, s(&a.actor)).await.map_err(err).and_then(ok)
    }

    #[tool(description = "Attach a document to a task (many-to-many). Notifies both the document's and the task's subscribers, so a task watcher learns a design doc landed. Idempotent.")]
    async fn attach_document(
        &self,
        Parameters(a): Parameters<AttachDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::attach_document(&self.pool, a.document_id, a.task_id, s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Detach a document from a task. Notifies both sides if a link existed.")]
    async fn detach_document(
        &self,
        Parameters(a): Parameters<AttachDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::detach_document(&self.pool, a.document_id, a.task_id, s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
    }
}

#[tool_handler]
impl ServerHandler for Board {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Task-board: a coordination board for agents. Register yourself, create \
                 projects/tasks, comment, subscribe, and drain your inbox with \
                 check_notifications."
                    .to_string(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::schemars::schema_for;

    // A free-form JSON object property must generate a concrete `{"type":"object"}` schema.
    // A bare `serde_json::Value` instead yields a boolean/empty schema, which strict MCP
    // clients (incl. Claude Code) reject as a named property — failing the whole
    // tools/list. This guards that regression for the metadata/props args.
    fn prop_type_is_object(schema: serde_json::Value, prop: &str) {
        let ty = schema
            .pointer(&format!("/properties/{prop}/type"))
            .unwrap_or_else(|| panic!("{prop}: no `type` in schema — {schema}"));
        // Required fields render as "object"; optional ones as ["object","null"]. Either
        // is a concrete object schema — what matters is it's NOT a bare boolean/empty
        // schema (which has no `type` at all and trips strict clients).
        let is_object = ty == "object"
            || ty.as_array().is_some_and(|a| a.iter().any(|t| t == "object"));
        assert!(is_object, "{prop} should be an object schema, got {schema}");
    }

    #[test]
    fn free_form_json_args_have_object_schemas() {
        prop_type_is_object(serde_json::to_value(schema_for!(SetTaskPropsArgs)).unwrap(), "props");
        prop_type_is_object(serde_json::to_value(schema_for!(CreateTaskArgs)).unwrap(), "metadata");
        prop_type_is_object(serde_json::to_value(schema_for!(UpdateTaskArgs)).unwrap(), "metadata");
        prop_type_is_object(serde_json::to_value(schema_for!(CreateChannelArgs)).unwrap(), "metadata");
    }
}
