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
use std::sync::{Arc, Mutex};

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
    /// Optional IPFS HTTP API for server-side content-addressing of raw document `content`.
    /// `None` keeps the board CID-only. See `crate::ipfs`.
    ipfs_api_url: Option<String>,
    /// Per-session identity: the agent this MCP session registered as. The streamable-HTTP
    /// transport creates one Board per session (via the service factory), so this binds an
    /// `Mcp-Session-Id` to an agent id without a separate map. register_agent sets it; other
    /// tools default `created_by`/`assignee`/`author`/`agent_id` from it when omitted. A
    /// reconnect/restart mints a fresh session (identity gone) — recover by re-registering.
    identity: Arc<Mutex<Option<String>>>,
    // Populated and consumed by the #[tool_router]/#[tool_handler] macros.
    #[allow(dead_code)]
    tool_router: ToolRouter<Board>,
}

impl Board {
    /// Resolve an identity param: an explicit non-empty value wins; otherwise fall back to the
    /// agent this session registered as. `None` if neither is available.
    fn me_opt(&self, explicit: Option<&str>) -> Option<String> {
        match explicit.map(str::trim).filter(|s| !s.is_empty()) {
            Some(x) => Some(x.to_string()),
            None => self.identity.lock().unwrap().clone(),
        }
    }

    /// Like `me_opt`, but required: a clear error when there's no explicit value and no session
    /// identity, rather than acting as nobody.
    fn me_req(&self, explicit: Option<&str>) -> Result<String, McpError> {
        self.me_opt(explicit).ok_or_else(|| {
            McpError::invalid_params(
                "no identity for this session; call register_agent first, or pass the id explicitly"
                    .to_string(),
                None,
            )
        })
    }
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
    /// Defaults to the agent this session registered as; pass to act for another.
    #[serde(default)]
    pub agent_id: Option<String>,
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
    /// Optional parent task (makes this a child/subtask). The parent must be in the same project.
    #[serde(default)]
    pub parent_id: Option<i64>,
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
    /// Reparent: set a parent task id (same project), or 0 to clear the parent (make top-level).
    #[serde(default)]
    pub parent_id: Option<i64>,
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
    /// Only the direct children of this task (an epic's subtasks). Takes precedence over `top_level`.
    #[serde(default)]
    pub parent_id: Option<i64>,
    /// Only top-level tasks (no parent) — epics + loose tasks, the default board view.
    #[serde(default)]
    pub top_level: Option<bool>,
    /// Free-text search over task title + description (case-insensitive substring). With no
    /// project_id it searches across every project.
    #[serde(default)]
    pub q: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CommentTaskArgs {
    pub task_id: i64,
    pub body: String,
    #[serde(default)]
    pub author: Option<String>,
    /// Optional external identity id (e.g. "slack:U123") to attribute this comment to — for an
    /// ingested human. `author` stays the fleet agent (you) that performed the write.
    #[serde(default)]
    pub external_author: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SubscribeArgs {
    /// The agent to (un)subscribe. Defaults to the agent this session registered as.
    #[serde(default)]
    pub subscriber: Option<String>,
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
    /// The poster. Defaults to the agent this session registered as.
    #[serde(default)]
    pub sender: Option<String>,
    pub body: String,
    /// Optional parent post seq to reply under (one-level threading).
    #[serde(default)]
    pub reply_to: Option<i64>,
    /// Optional external identity id (e.g. "slack:U123") to attribute this post to — for an
    /// ingested human. `sender` stays the fleet agent (you) that performed the write.
    #[serde(default)]
    pub external_author: Option<String>,
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
pub struct UpsertExternalIdentityArgs {
    /// Namespaced id `source:handle`, e.g. "slack:U123ABC". Idempotent upsert (re-registering
    /// refreshes the display name / metadata).
    pub id: String,
    /// Originating system, e.g. "slack" or "github".
    pub source: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Arbitrary props (avatar, real name, ...). MERGED into any existing bag.
    #[serde(default)]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListExternalIdentitiesArgs {
    /// Filter by originating system (e.g. "slack"). Omit to list all.
    #[serde(default)]
    pub source: Option<String>,
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
    /// Whose inbox to drain. Defaults to the agent this session registered as.
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default = "default_true")]
    pub mark_read: bool,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendMessageArgs {
    /// Sender. Defaults to the agent this session registered as.
    #[serde(default)]
    pub from_agent: Option<String>,
    pub to_agent: String,
    pub body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetMessagesArgs {
    /// Whose messages. Defaults to the agent this session registered as.
    #[serde(default)]
    pub agent_id: Option<String>,
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
    /// Optional if `content` is given (and the board has an IPFS backend configured).
    #[serde(default)]
    pub cid: Option<String>,
    /// Raw content for version 1, content-addressed server-side when no `cid` is given (needs
    /// a configured IPFS backend). Lets a client with no local IPFS author a document. Supply
    /// exactly one of `cid` / `content`.
    #[serde(default)]
    pub content: Option<String>,
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
    /// Optional if `content` is given (and the board has an IPFS backend configured).
    #[serde(default)]
    pub cid: Option<String>,
    /// Raw content for the new version, content-addressed server-side when no `cid` is given.
    /// Supply exactly one of `cid` / `content`.
    #[serde(default)]
    pub content: Option<String>,
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
    /// A value in the document's metadata.tags array.
    #[serde(default)]
    pub tag: Option<String>,
    /// Only documents attached to this task.
    #[serde(default)]
    pub task_id: Option<i64>,
    /// Only documents created by this author (created_by).
    #[serde(default)]
    pub author: Option<String>,
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
    pub fn new(pool: Pool, ipfs_api_url: Option<String>) -> Self {
        Self {
            pool,
            ipfs_api_url,
            identity: Arc::new(Mutex::new(None)),
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
        let out = core::register_agent(
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
        .and_then(ok)?;
        // Bind this session to the registered agent (idempotent: re-registering just refreshes
        // it). Other tools then default their identity params from this when omitted.
        *self.identity.lock().unwrap() = Some(a.agent_id);
        Ok(out)
    }

    #[tool(description = "Set your presence: online / busy / away / offline (+ an optional note).")]
    async fn set_status(
        &self,
        Parameters(a): Parameters<SetStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let me = self.me_req(a.agent_id.as_deref())?;
        core::set_status(&self.pool, &me, &a.status, s(&a.status_message))
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
        let created_by = self.me_opt(s(&a.created_by));
        core::create_project(
            &self.pool,
            &a.name,
            s(&a.description),
            created_by.as_deref(),
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
        description = "Create a task in a project. The creator and assignee are auto-subscribed, so they get notified of future changes. `metadata` is an optional dict of arbitrary properties (pipeline state, source, ipfs_cid, target collection, ...). Pass `parent_id` to nest it under an epic (same project). Returns the new task incl. its id."
    )]
    async fn create_task(
        &self,
        Parameters(a): Parameters<CreateTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let created_by = self.me_opt(s(&a.created_by));
        core::create_task(
            &self.pool,
            a.project_id,
            &a.title,
            s(&a.description),
            s(&a.assignee),
            s(&a.priority),
            created_by.as_deref(),
            a.metadata.map(Value::Object),
            a.parent_id,
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Update a task. Pass only the fields you're changing. Statuses: todo / in_progress / blocked / done / cancelled. Set `assignee` to \"\" (empty string) to unassign (clear the owner) — this emits task.unassigned; setting a non-empty owner emits task.assigned. `metadata` is MERGED into the task's props. Set `actor` to your agent id so you aren't notified of your own change. Notifies subscribers on status/assignee changes (e.g. reassign to hand a ticket to the next pipeline stage). Pass `parent_id` to reparent under an epic (same project), or 0 to clear the parent."
    )]
    async fn update_task(
        &self,
        Parameters(a): Parameters<UpdateTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.me_opt(s(&a.actor));
        core::update_task(
            &self.pool,
            a.task_id,
            s(&a.status),
            s(&a.assignee),
            s(&a.title),
            s(&a.description),
            s(&a.priority),
            actor.as_deref(),
            a.metadata.map(Value::Object),
            a.parent_id,
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
        let actor = self.me_opt(s(&a.actor));
        core::move_task(&self.pool, a.task_id, a.to_project_id, actor.as_deref())
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

    #[tool(description = "List tasks, optionally filtered by project, status, and/or assignee. Pass `unassigned: true` to list only tasks with no assignee. Nesting: `parent_id` lists an epic's direct children; `top_level: true` lists only unparented tasks (epics + loose tasks — the default board view). `q` is a free-text search over title + description (across all projects when project_id is omitted).")]
    async fn list_tasks(
        &self,
        Parameters(a): Parameters<ListTasksArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_tasks(&self.pool, a.project_id, s(&a.status), s(&a.assignee), a.unassigned.unwrap_or(false), a.parent_id, a.top_level.unwrap_or(false), s(&a.q))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Add a comment to a task. Notifies the task's subscribers/assignee (except you).")]
    async fn comment_task(
        &self,
        Parameters(a): Parameters<CommentTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let author = self.me_opt(s(&a.author));
        core::comment_task(&self.pool, a.task_id, &a.body, author.as_deref(), s(&a.external_author))
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
        let sub = self.me_req(a.subscriber.as_deref())?;
        core::subscribe(&self.pool, &sub, a.task_id, a.project_id, a.channel_id, a.document_id, a.board.unwrap_or(false))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Stop notifying an agent about a task, project, channel (leaving a channel), document, or the whole board (board: true).")]
    async fn unsubscribe(
        &self,
        Parameters(a): Parameters<SubscribeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let sub = self.me_req(a.subscriber.as_deref())?;
        core::unsubscribe(&self.pool, &sub, a.task_id, a.project_id, a.channel_id, a.document_id, a.board.unwrap_or(false))
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
        let created_by = self.me_opt(s(&a.created_by));
        core::create_channel(
            &self.pool,
            &a.name,
            s(&a.topic),
            created_by.as_deref(),
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
        let sender = self.me_req(a.sender.as_deref())?;
        core::post_to_channel(&self.pool, a.channel_id, &sender, &a.body, a.reply_to, s(&a.external_author))
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

    // --- External identities (bridged actors) ---
    #[tool(
        description = "Register or update an external identity — a human/actor from a bridged system (Slack, GitHub, ...), kept distinct from fleet agents. `id` is namespaced source:handle (e.g. slack:U123ABC). Idempotent: re-registering refreshes display_name/metadata. Attribute an ingested post/comment to it via the `external_author` field so it renders as that person, not you."
    )]
    async fn upsert_external_identity(
        &self,
        Parameters(a): Parameters<UpsertExternalIdentityArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::upsert_external_identity(
            &self.pool,
            &a.id,
            &a.source,
            s(&a.display_name),
            a.metadata.map(Value::Object),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "List external (bridged) identities, optionally filtered by `source` (e.g. 'slack'). Newest-updated first.")]
    async fn list_external_identities(
        &self,
        Parameters(a): Parameters<ListExternalIdentitiesArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_external_identities(&self.pool, s(&a.source)).await.map_err(err).and_then(ok)
    }

    #[tool(description = "Invite another agent into a channel: they're auto-joined and get a channel.invite in their inbox (no accept step). They can unsubscribe to leave.")]
    async fn invite_to_channel(
        &self,
        Parameters(a): Parameters<InviteToChannelArgs>,
    ) -> Result<CallToolResult, McpError> {
        let invited_by = self.me_opt(s(&a.invited_by));
        core::invite_to_channel(&self.pool, a.channel_id, &a.agent_id, invited_by.as_deref())
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
        let me = self.me_req(a.agent_id.as_deref())?;
        core::check_notifications(&self.pool, &me, a.mark_read, a.limit, None)
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
        let from = self.me_req(a.from_agent.as_deref())?;
        core::send_message(&self.pool, &from, &a.to_agent, &a.body)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Just your direct messages (a filtered view of your inbox).")]
    async fn get_messages(
        &self,
        Parameters(a): Parameters<GetMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let me = self.me_req(a.agent_id.as_deref())?;
        core::get_messages(&self.pool, &me, a.mark_read, a.limit)
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
        description = "Create a versioned document. Content lives on IPFS. Normally pass `cid` (a bare content id) — the board stores the identifier only and NEVER resolves it or composes a URL. If you have no local IPFS, pass raw `content` instead and the board content-addresses it server-side (requires the deployment to configure an IPFS backend; otherwise you get a clear error asking for a `cid`). Optionally attach it to a `project_id` and set `metadata` (tags, etc). Creates version 1. Returns the document with its current version + version list."
    )]
    async fn create_document(
        &self,
        Parameters(a): Parameters<CreateDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cid = crate::ipfs::resolve_cid(a.cid.as_deref(), a.content.as_deref(), self.ipfs_api_url.as_deref())
            .await
            .map_err(err)?;
        core::create_document(
            &self.pool,
            &a.title,
            a.project_id,
            &cid,
            s(&a.summary),
            s(&a.created_by),
            a.metadata.map(Value::Object),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Publish a new immutable version of a document. Pass `cid` (bare content id; the board does not resolve it) or, with no local IPFS, raw `content` to content-address server-side (requires a configured IPFS backend). Appends the version, advances the current pointer, and returns the updated document. A new version drops an approved/changes_requested doc back to in_review."
    )]
    async fn publish_version(
        &self,
        Parameters(a): Parameters<PublishVersionArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cid = crate::ipfs::resolve_cid(a.cid.as_deref(), a.content.as_deref(), self.ipfs_api_url.as_deref())
            .await
            .map_err(err)?;
        core::publish_version(&self.pool, a.document_id, &cid, s(&a.summary), s(&a.created_by))
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

    #[tool(description = "List documents for discovery, filtered by any combination of project, status (draft / in_review / approved / changes_requested), tag (a value in metadata.tags), task_id (docs attached to that task), and author. Filters AND together.")]
    async fn list_documents(
        &self,
        Parameters(a): Parameters<ListDocumentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_documents(
            &self.pool,
            a.project_id,
            s(&a.status),
            s(&a.tag),
            a.task_id,
            s(&a.author),
        )
        .await
        .map_err(err)
        .and_then(ok)
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
                "Task-board: a coordination board for agents. Call register_agent first — it \
                 binds this session to your agent id, so you can then omit created_by / \
                 assignee / author / agent_id on later calls and they default to you (pass one \
                 explicitly to act for another agent). If a call returns a 'no identity for \
                 this session' error, call register_agent again and retry. Create \
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

    // register_agent binds the session identity; identity params then default to it when
    // omitted, an explicit value still wins, and a session with no identity errors clearly.
    #[tokio::test]
    async fn session_identity_defaults_and_requires() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let board = Board::new(pool.clone(), None);
        let mkfail = |e: McpError| anyhow::anyhow!("{e:?}");

        let p = core::create_project(&pool, "P", None, None, None).await?;
        let pid = p["id"].as_i64().unwrap();

        // No identity + no explicit id -> a clear error, not acting as nobody.
        assert!(
            board
                .check_notifications(Parameters(serde_json::from_value(serde_json::json!({}))?))
                .await
                .is_err(),
            "check_notifications with neither session identity nor agent_id must error"
        );

        // register_agent binds this session.
        board
            .register_agent(Parameters(serde_json::from_value(
                serde_json::json!({"agent_id":"agent:x"}),
            )?))
            .await
            .map_err(mkfail)?;

        // create_task with no created_by defaults to the session identity.
        board
            .create_task(Parameters(serde_json::from_value(
                serde_json::json!({"project_id": pid, "title": "T"}),
            )?))
            .await
            .map_err(mkfail)?;
        let find = |tasks: &Value, title: &str| -> i64 {
            tasks
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["title"] == title)
                .unwrap()["id"]
                .as_i64()
                .unwrap()
        };
        let tasks = core::list_tasks(&pool, Some(pid), None, None, false, None, false, None).await?;
        let got = core::get_task(&pool, find(&tasks, "T")).await?;
        assert_eq!(got["created_by"], serde_json::json!("agent:x"));

        // An explicit created_by still wins over the session identity.
        board
            .create_task(Parameters(serde_json::from_value(
                serde_json::json!({"project_id": pid, "title": "T2", "created_by": "other"}),
            )?))
            .await
            .map_err(mkfail)?;
        let tasks = core::list_tasks(&pool, Some(pid), None, None, false, None, false, None).await?;
        let got2 = core::get_task(&pool, find(&tasks, "T2")).await?;
        assert_eq!(got2["created_by"], serde_json::json!("other"));

        // check_notifications now works with no agent_id (defaults to the bound identity).
        board
            .check_notifications(Parameters(serde_json::from_value(serde_json::json!({}))?))
            .await
            .map_err(mkfail)?;

        // set_status with no agent_id acts on the session identity.
        board
            .set_status(Parameters(serde_json::from_value(
                serde_json::json!({"status": "busy"}),
            )?))
            .await
            .map_err(mkfail)?;
        assert_eq!(core::get_agent(&pool, "agent:x").await?["status"], serde_json::json!("busy"));

        // send_message with no from_agent is attributed to the session identity. (Register the
        // recipient directly so it doesn't rebind this session's identity.)
        core::register_agent(&pool, "agent:y", None, None, None, None, None).await?;
        board
            .send_message(Parameters(serde_json::from_value(
                serde_json::json!({"to_agent": "agent:y", "body": "hi from session"}),
            )?))
            .await
            .map_err(mkfail)?;
        let msgs = core::get_messages(&pool, "agent:y", false, 50).await?;
        let notes = msgs["notifications"].as_array().expect("notifications array");
        assert!(
            notes.iter().any(|m| m.to_string().contains("agent:x")),
            "a DM sent with no from_agent shows the session identity as sender; got: {msgs}"
        );

        // A fresh session (new Board) starts with no identity again.
        let board2 = Board::new(pool.clone(), None);
        assert!(
            board2
                .check_notifications(Parameters(serde_json::from_value(serde_json::json!({}))?))
                .await
                .is_err(),
            "a new session has no identity until it registers"
        );
        Ok(())
    }

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
