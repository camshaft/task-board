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
use rmcp::service::NotificationContext;
use rmcp::{
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler,
};
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
pub struct ListAgentsArgs {
    /// Filter by exact presence status (online / busy / away / offline).
    #[serde(default)]
    pub status: Option<String>,
    /// Substring filter over id + display_name (case-insensitive).
    #[serde(default)]
    pub q: Option<String>,
    /// With meta_value, match a scalar metadata field (e.g. meta_key="area", meta_value="compiler").
    #[serde(default)]
    pub meta_key: Option<String>,
    #[serde(default)]
    pub meta_value: Option<String>,
    /// Return full agent objects (incl the heavy charter) instead of the compact {id, display_name,
    /// status, metadata} roster projection. Default false.
    #[serde(default)]
    pub verbose: Option<bool>,
    /// Max rows (default 200, capped at 1000).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Rows to skip (pagination).
    #[serde(default)]
    pub offset: Option<i64>,
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
    /// Field names to CLEAR to null (a merge-PATCH leaves an omitted/null field unchanged, so this
    /// is the only way to reset a nullable field — e.g. clear: ["webhook_url"] to drop a stale wake
    /// URL). Clearable: display_name, kind, charter, status_message, webhook_url. An explicit value
    /// for the same field wins over clearing it.
    #[serde(default)]
    pub clear: Option<Vec<String>>,
    /// Return the full agent (including the `charter`) in the response. Default false — the response
    /// omits the charter to keep a looping caller's context light; fetch it with get_agent.
    #[serde(default)]
    pub verbose: Option<bool>,
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
pub struct RequestStandDownArgs {
    /// The agent asked to wind down.
    pub agent_id: String,
    /// Who is asking (defaults to this session's identity).
    #[serde(default)]
    pub requested_by: Option<String>,
    /// Optional reason shown to the agent + on its page.
    #[serde(default)]
    pub reason: Option<String>,
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
    /// Optional external reference for idempotent ingest (bridge adapters). If a task is already
    /// linked on (source, external_id) it's returned with `created:false` instead of a duplicate;
    /// otherwise the task is created and the link recorded atomically (`created:true`).
    #[serde(default)]
    pub external_link: Option<core::ExternalRef>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateTaskArgs {
    pub task_id: i64,
    /// todo / in_progress / blocked / done / cancelled
    #[serde(default)]
    pub status: Option<String>,
    /// New owner's agent id. To clear the owner (unassign), set `unassign: true` rather than
    /// sending an empty string here — some clients can't serialize "".
    #[serde(default)]
    pub assignee: Option<String>,
    /// Clear the task's owner (set it to no assignee). Takes precedence over `assignee`. This is
    /// the reliable, client-safe way to unassign (an empty-string `assignee` is not portable).
    #[serde(default)]
    pub unassign: Option<bool>,
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
    /// What this task is blocked on. REQUIRED when setting status=blocked — a blocked task must
    /// record what it is waiting on. Omit to leave unchanged; pass kind="none" to clear.
    #[serde(default, deserialize_with = "de_opt_blocked_on_lenient")]
    pub blocked_on: Option<BlockedOnArgs>,
    /// Return the full task (including the `description`) in the response. Default false — the
    /// response omits the description to keep a looping caller's context light; fetch it with get_task.
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub verbose: Option<bool>,
}

/// What a blocked task is waiting on: kind is task | agent | operator (or "none" to clear),
/// target is the blocking task id or agent id (ignored for operator). When kind=agent, that
/// agent is notified they are blocking.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockedOnArgs {
    pub kind: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

// String-tolerant `deserialize_with` helpers for MCP arg fields (task 351). Some MCP clients
// JSON-stringify scalar/struct argument values, so a strict serde type rejects them ("expected a
// boolean", "invalid type: string, expected i64", "expected struct BlockedOnArgs") and the agent
// abandons the call. These accept EITHER the native JSON type OR its stringified form, so one helper
// hardens a whole class of args (include_body: bool, comments_limit: i64, blocked_on: struct, ...)
// against the stringification. The advertised JsonSchema is unchanged (still the native type), so a
// well-behaved client is unaffected; this only widens what is accepted.

/// Coerce a JSON value into a bool, tolerating a stringified form: a real bool, "true"/"false"/
/// "yes"/"no"/"1"/"0" (case-insensitive), or a number (0 = false, else true). Returns a message on
/// anything else, which each deserializer maps to its own error type.
fn coerce_bool(v: &serde_json::Value) -> Result<bool, String> {
    match v {
        serde_json::Value::Bool(b) => Ok(*b),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Ok(true),
            "false" | "no" | "0" | "" => Ok(false),
            other => Err(format!("expected a boolean (or \"true\"/\"false\"), got string {other:?}")),
        },
        serde_json::Value::Number(n) => Ok(n.as_i64().map(|x| x != 0).unwrap_or(true)),
        other => Err(format!("expected a boolean, got {other}")),
    }
}

/// A bool that a client may have stringified (see [`coerce_bool`]).
fn de_bool_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    coerce_bool(&v).map_err(serde::de::Error::custom)
}

/// An `Option<bool>` variant of [`de_bool_lenient`]: null/absent -> None.
fn de_opt_bool_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    match Option::<serde_json::Value>::deserialize(d)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => coerce_bool(&v).map(Some).map_err(serde::de::Error::custom),
    }
}

/// An `Option<i64>` where the client may have stringified the number: accepts null, an integer, or a
/// string that parses to an i64 (empty string -> None).
fn de_opt_i64_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    match Option::<serde_json::Value>::deserialize(d)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("expected an integer")),
        Some(serde_json::Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                return Ok(None);
            }
            t.parse::<i64>()
                .map(Some)
                .map_err(|_| serde::de::Error::custom(format!("expected an integer, got string {s:?}")))
        }
        Some(other) => Err(serde::de::Error::custom(format!("expected an integer, got {other}"))),
    }
}

/// Parse a bare-string blocked_on into a [`BlockedOnArgs`] (task 351): "operator" -> {kind:operator};
/// "task:123" / "agent:foo" (or space-separated) -> {kind, target}. The convenience form that lets
/// an agent write blocked_on:"operator" instead of the nested object.
fn parse_bare_blocked_on(s: &str) -> BlockedOnArgs {
    let s = s.trim();
    match s.split_once([':', ' ']) {
        Some((k, t)) => {
            let t = t.trim();
            BlockedOnArgs {
                kind: k.trim().to_string(),
                target: if t.is_empty() { None } else { Some(t.to_string()) },
                note: None,
            }
        }
        None => BlockedOnArgs { kind: s.to_string(), target: None, note: None },
    }
}

/// An `Option<BlockedOnArgs>` the client may have sent as: null, the object, a JSON-string that
/// parses to the object, or a bare string kind ("operator", "task:123"). This is the task 351 fix:
/// a board-native MCP client that stringifies blocked_on can now set it, so status=blocked is
/// reachable and the task 506 park-as-blocked contract is satisfiable.
fn de_opt_blocked_on_lenient<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<BlockedOnArgs>, D::Error> {
    match Option::<serde_json::Value>::deserialize(d)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                return Ok(None);
            }
            if t.starts_with('{') {
                serde_json::from_str::<BlockedOnArgs>(t)
                    .map(Some)
                    .map_err(|e| serde::de::Error::custom(format!("blocked_on JSON string did not parse: {e}")))
            } else {
                Ok(Some(parse_bare_blocked_on(t)))
            }
        }
        Some(v @ serde_json::Value::Object(_)) => serde_json::from_value::<BlockedOnArgs>(v)
            .map(Some)
            .map_err(|e| serde::de::Error::custom(format!("invalid blocked_on object: {e}"))),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected blocked_on as an object or a kind string (e.g. \"operator\"), got {other}"
        ))),
    }
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
    /// How many of the most-recent comments to inline (chronological within the slice). Omit for
    /// the default recent slice; pass 0 for metadata-only (no comments); pass a larger number to
    /// page in more history, or a negative number for the whole thread. The response always carries
    /// `comment_count` (total) and `comments_truncated`, so you know when there is more to fetch.
    /// Bounding this keeps a long, busy task from overflowing the read/context cap (#511).
    #[serde(default, deserialize_with = "de_opt_i64_lenient")]
    pub comments_limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArchiveTaskArgs {
    pub task_id: i64,
    /// The agent performing the archive/restore (for the event actor). Defaults to the
    /// agent this session registered as.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListTasksArgs {
    #[serde(default, deserialize_with = "de_opt_i64_lenient")]
    pub project_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    /// Only tasks with no assignee (assignee IS NULL). Takes precedence over `assignee`.
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub unassigned: Option<bool>,
    /// Only the direct children of this task (an epic's subtasks). Takes precedence over `top_level`.
    #[serde(default, deserialize_with = "de_opt_i64_lenient")]
    pub parent_id: Option<i64>,
    /// Only top-level tasks (no parent) — epics + loose tasks, the default board view.
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub top_level: Option<bool>,
    /// Free-text search over task title + description (case-insensitive substring). With no
    /// project_id it searches across every project.
    #[serde(default)]
    pub q: Option<String>,
    /// "What is waiting on X" views: filter blocked tasks by blocked_on kind (task|agent|operator).
    #[serde(default)]
    pub blocked_on_kind: Option<String>,
    /// Filter by blocked_on ref (a blocking task id or agent id) — e.g. what is blocked on you.
    #[serde(default)]
    pub blocked_on_ref: Option<String>,
    /// Filter to tasks whose metadata has this key (a JSON path under `$.`, e.g. "observes").
    /// Pair with `meta_value`; both must be set for the filter to apply.
    #[serde(default)]
    pub meta_key: Option<String>,
    /// The value `meta_key` must equal (matched against `json_extract(metadata, '$.'||key)`).
    #[serde(default)]
    pub meta_value: Option<String>,
    /// Include archived tasks. Archived tasks are hidden by default; set true to list them too.
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub include_archived: Option<bool>,
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
    /// Optional external reference for idempotent ingest (bridge adapters). If a comment is
    /// already linked on (source, external_id) it's returned with `created:false` instead of a
    /// duplicate; otherwise the comment is created and the link recorded atomically.
    #[serde(default)]
    pub external_link: Option<core::ExternalRef>,
    /// Submit even if the body contains a banned phrase (the pre-submit lint otherwise rejects it).
    /// Use only for an intentional occurrence, e.g. quoting a banned phrase to discuss it.
    #[serde(default)]
    pub acknowledge_banned: Option<bool>,
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
    /// Optional event-class filter (#462): a subset of ["created", "done", "blocked", "status",
    /// "comment", "assigned", "review", "doc"]. When given, this subscription is delivery-gated to
    /// just those classes - only matching events reach your inbox AND wake you, everything else is
    /// dropped for this subscription. Omit for every event (the default). Applies to ANY target
    /// (board/project/channel/task): e.g. board + ["created"] wakes a triage agent only on new
    /// tasks; a gap-ticket owner uses task/project + ["done", "blocked"]. (Ignored by unsubscribe.)
    #[serde(default)]
    pub event_classes: Option<Vec<String>>,
    /// Subscribe to a channel THREAD (#438): the thread ROOT is a channel post's event seq. Delivers
    /// subsequent in-thread posts (reply_to = this root) to you AND wakes you, so a reactive agent
    /// that joined a thread answers later in-thread follow-ups without a re-mention. When set, this
    /// takes precedence over the other targets. (unsubscribe(thread_root) leaves the thread.)
    #[serde(default)]
    pub thread_root: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MuteTaskArgs {
    pub task_id: i64,
    /// The agent to mute/unmute the task for. Defaults to the agent this session registered as.
    #[serde(default)]
    pub agent: Option<String>,
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
    /// Optional per-post metadata bag stored on the post (e.g. a bridge stamps a relayed message's
    /// {slack_ts, slack_channel, thread_ts}). Surfaced on reads and on channel.outbound_reflect —
    /// and a reply's reflect also carries the parent post's metadata as `parent_metadata`, so a
    /// bridge can thread statelessly.
    #[serde(default)]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetChannelPostsArgs {
    pub channel_id: i64,
    #[serde(default)]
    pub since_seq: i64,
    #[serde(default = "default_events_limit")]
    pub limit: i64,
    /// Upper bound: only posts with seq < before_seq. For a "load earlier" page, pass the oldest
    /// seq you already have (with desc=true) to get the N posts just before it.
    #[serde(default)]
    pub before_seq: Option<i64>,
    /// false (default) = oldest-first (scrollback); true = newest-first, so since_seq=0 + limit=N
    /// returns the LATEST N posts (a chat view).
    #[serde(default)]
    pub desc: bool,
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
pub struct SetWorkspaceKindArgs {
    /// The kind key an agent's `metadata.workspace_kind` references.
    pub name: String,
    /// The script fleet spin-up runs to materialize the workspace. Omit to keep the stored one.
    #[serde(default)]
    pub setup_script: Option<String>,
    /// Hints the consumer reads. Canonical keys: `cwd` (dir to launch in after setup — absolute
    /// as-is, relative resolved under the consumer's root, else the agent's own dir), `pre_trust`
    /// (array of extra trusted paths), `env` (string→string env map for the launched agent). Any
    /// other keys are free-form for the kind's own use. MERGED into any existing bag.
    #[serde(default)]
    pub config: Option<JsonObject>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceKindNameArgs {
    /// The workspace kind's name.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddBannedPhraseArgs {
    /// The phrase to ban. Stored lowercased; matched case-insensitively and whole-phrase, so
    /// "the floor" does not match inside "the floorboard".
    pub phrase: String,
    /// Optional note: why it's banned, or what to write instead.
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetIdentityAliasArgs {
    /// The alias to map (stored lowercased; the lookup key), e.g. "operator".
    pub alias: String,
    /// The canonical identity it resolves to, e.g. "cameron".
    pub canonical: String,
    #[serde(default)]
    pub created_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BannedPhraseArgs {
    /// The phrase to remove from the banned list.
    pub phrase: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RequestSecretArgs {
    /// The secret's name (e.g. the durable filename it will land as).
    pub name: String,
    /// Age recipient public keys (non-secret) the browser encrypts the value to. For a
    /// host-bound secret include the recovery/user keys too, not just the host key.
    #[serde(default)]
    pub recipients: Vec<String>,
    /// Human instructions shown on the submit page — what the value is and where to obtain it.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Advisory placement hint for the fulfiller (the durable path + any wiring note).
    #[serde(default)]
    pub target: Option<String>,
    /// The agent to directly notify on submit + whose token gates the ciphertext pull.
    #[serde(default)]
    pub fulfiller: Option<String>,
    /// The requesting agent (defaults to this session's identity).
    #[serde(default)]
    pub requested_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FulfillSecretArgs {
    pub id: i64,
    /// The fulfiller capability token returned when the request was created.
    pub token: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetChannelPropsArgs {
    pub channel_id: i64,
    /// Key/value properties to merge into the channel's metadata — e.g. the outbound
    /// reflect-back policy `{"direction":"both","outbound_authors":["concierge"]}`.
    pub props: JsonObject,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetChannelAutoJoinArgs {
    pub channel_id: i64,
    /// true = every agent is a member (existing agents joined now + new agents auto-join on
    /// register); false = stop auto-joining (existing members stay).
    pub auto_join: bool,
    /// The agent performing the change (event actor). Defaults to this session's identity.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpsertExternalLinkArgs {
    /// Originating system, e.g. "slack" or "github".
    pub source: String,
    /// The external system's canonical key (e.g. a Slack channel id, a thread ts, an issue url).
    pub external_id: String,
    /// Optional external container (e.g. the Slack channel of a thread).
    #[serde(default)]
    pub external_parent_id: Option<String>,
    /// Board entity kind: "channel", "task", or "thread".
    pub board_kind: String,
    /// Board-side id (channel id / task id / thread root post seq).
    pub board_id: i64,
    /// Arbitrary props (external names, urls, ...). MERGED into any existing bag.
    #[serde(default)]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListExternalLinksArgs {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub board_kind: Option<String>,
    #[serde(default)]
    pub board_id: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PromoteThreadArgs {
    pub channel_id: i64,
    /// The seq of the thread's root post (its replies — posts with reply_to == this — are
    /// imported as task comments).
    pub root_post_seq: i64,
    /// Project the new task is created in.
    pub project_id: i64,
    /// Who is promoting (task creator + subscriber). Defaults to your session identity.
    #[serde(default)]
    pub actor: Option<String>,
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
pub struct OpenDmArgs {
    /// The other agent in the 1:1 DM.
    pub with_agent: String,
    /// This side of the DM. Defaults to the agent this session registered as.
    #[serde(default)]
    pub agent_id: Option<String>,
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
    /// Only events whose `actor` matches — a complete per-agent activity feed.
    #[serde(default)]
    pub actor: Option<String>,
    /// `true` returns the LATEST `limit` events (newest-first) — a live activity feed. Default
    /// `false` is oldest-first after `since_seq` — for incrementally tailing the log.
    #[serde(default)]
    pub desc: bool,
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
    /// MIME type of v1's bytes (default text/markdown), e.g. image/png, application/pdf,
    /// text/vnd.mermaid. The board records only the label; rendering is the client's job.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Submit even if the content contains a banned phrase (the pre-submit lint otherwise rejects
    /// it). Text content is scanned; non-text content is not.
    #[serde(default)]
    pub acknowledge_banned: Option<bool>,
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
    /// MIME type of this version's bytes (default text/markdown). The board records only the label.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Submit even if the content contains a banned phrase (the pre-submit lint otherwise rejects
    /// it). Text content is scanned; non-text content is not.
    #[serde(default)]
    pub acknowledge_banned: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetDocumentPathArgs {
    pub document_id: i64,
    /// The wiki path to file this document under (e.g. architecture/board/events). An empty
    /// string clears the path (unfiles the doc). Must be unique among filed documents.
    pub path: String,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListWikiArgs {
    /// Only documents filed under this path prefix (e.g. architecture returns architecture and
    /// everything beneath it). Omit for the whole wiki tree. Results are ordered by path.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Include archived (retired) documents in the tree. Hidden by default.
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetDocumentArgs {
    pub document_id: i64,
    /// When true, also fetch the current version's markdown from its pinned CID (server-side, via
    /// the board's IPFS backend) and inline it as `body` — so an agent building or reviewing from an
    /// approved doc gets the content in one call, whatever its own host can reach. Omit/false for
    /// metadata only. If the fetch fails (no backend, unreachable, non-text) the metadata still
    /// returns with `body: null` + a `body_error`. The doc stays CID-only at rest (fetched on read).
    #[serde(default, deserialize_with = "de_bool_lenient")]
    pub include_body: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadDocumentArgs {
    pub document_id: i64,
    /// Which version's body to read. Omit for the current version.
    #[serde(default)]
    pub version_no: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateDocumentArgs {
    pub document_id: i64,
    /// New title — a short, specific noun phrase; the viewer renders the title as the page header.
    pub title: String,
    #[serde(default)]
    pub actor: Option<String>,
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
    /// Include archived (retired) documents. Hidden by default.
    #[serde(default)]
    pub include_archived: bool,
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
    /// Optional external identity id (e.g. "slack:U123") to attribute this comment to — for an
    /// ingested human. `author` stays the fleet agent (you) that performed the write.
    #[serde(default)]
    pub external_author: Option<String>,
    /// Submit even if the body contains a banned phrase (the pre-submit lint otherwise rejects it).
    #[serde(default)]
    pub acknowledge_banned: Option<bool>,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateReviewArgs {
    /// What is being reviewed: document | code | design | agent-session | task.
    pub kind: String,
    /// The source classifying where the artifact lives (e.g. board-document, github-pull-request,
    /// code-amazon-change-request, url, agent-session, task). Metadata — the board doesn't fetch it.
    #[serde(default)]
    pub source: Option<String>,
    /// A pointer to the artifact within its source (a doc id, a PR url, a change-request id, ...).
    #[serde(default)]
    pub target_ref: Option<String>,
    /// A short title for the review (usually the artifact's title).
    #[serde(default)]
    pub title: Option<String>,
    /// Initial A2 lifecycle status; defaults to `open`. One of open / in_review /
    /// changes_requested / approved / closed.
    #[serde(default)]
    pub status: Option<String>,
    /// The agent that created/produced the review (defaults to this session's identity).
    #[serde(default)]
    pub created_by: Option<String>,
    /// The reviewer(s) assigned. A single agent id in increment 1.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Arbitrary properties: producing agent id, a predecessor review id, tags, ...
    #[serde(default)]
    pub metadata: Option<JsonObject>,
    /// Optional external reference for idempotent ingest (a bridge). If a review is already linked
    /// on (source, external_id) it's returned with `created:false` instead of a duplicate;
    /// otherwise the review is created and the link recorded atomically (`created:true`).
    #[serde(default)]
    pub external_link: Option<core::ExternalRef>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetReviewArgs {
    pub review_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListReviewsArgs {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReviewTrendArgs {
    /// Restrict the trend to one review kind (document / code / design / ...).
    #[serde(default)]
    pub kind: Option<String>,
    /// Restrict the trend to one producing area/agent.
    #[serde(default)]
    pub area: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetReviewStatusArgs {
    pub review_id: i64,
    /// The new A2 status: open / in_review / changes_requested / approved / closed. Re-applying
    /// the current status is an idempotent no-op.
    pub status: String,
    /// The agent making the transition (defaults to this session's identity).
    #[serde(default)]
    pub actor: Option<String>,
    /// An optional note recorded on the state-change log entry (e.g. why changes were requested).
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetReviewVettedArgs {
    pub review_id: i64,
    /// true = mark the review vetted (adversarial review run + addressed); false = clear it.
    pub vetted: bool,
    /// The agent setting the flag (defaults to this session's identity). Recorded for audit.
    #[serde(default)]
    pub actor: Option<String>,
    /// An optional note recorded on the audit log entry.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AppendReviewLogArgs {
    pub review_id: i64,
    /// The entry type: submitted / revised / finding / finding_resolved / comment / state_change /
    /// adversarial_review / decision. A `finding` is just an entry of this type (not a separate
    /// collection); an actionable finding links the child task it spawned via `task_id`.
    pub entry_type: String,
    /// The entry text (the comment, the finding description, the decision rationale, ...).
    #[serde(default)]
    pub body: Option<String>,
    /// The author of this entry (defaults to this session's identity).
    #[serde(default)]
    pub author: Option<String>,
    /// For an actionable `finding`: the id of the child task tracking the fix.
    #[serde(default)]
    pub task_id: Option<i64>,
    /// Optional external id for idempotent ingest (a bridge replaying an upstream comment/finding).
    /// If an entry with this external_id already exists on the review it's returned with
    /// `appended:false` instead of a duplicate.
    #[serde(default)]
    pub external_id: Option<String>,
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
        // Return only presence fields, not the full agent — a looping caller re-ingests this on
        // every tick and never needs its own charter echoed back (task #416).
        core::set_status(&self.pool, &me, &a.status, s(&a.status_message))
            .await
            .map(core::presence_projection)
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Request that an agent gracefully wind down: records the request (who/why/when, visible on the agent's page) and drops an agent.stand_down_requested notification into the agent's inbox so it observes the request on its next loop tick and stands down on its own terms (sets status offline, ends its loop). This is a SIGNAL, not an action — it never changes the agent's status and never kills or interrupts a live agent mid-work. The request stays pending until the agent honors it by going offline (which clears it). Use it to stand an agent down cleanly rather than reaping it.")]
    async fn request_stand_down(
        &self,
        Parameters(a): Parameters<RequestStandDownArgs>,
    ) -> Result<CallToolResult, McpError> {
        let requested_by = self.me_opt(s(&a.requested_by));
        core::request_stand_down(&self.pool, &a.agent_id, requested_by.as_deref(), s(&a.reason))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "List agents as a lightweight roster: each entry is a compact {id, display_name, status, metadata} — the small metadata bag is kept so callers can filter (e.g. metadata.native); only the heavy charter is dropped to stay under the token cap. Use get_agent for one agent's full charter, or pass verbose:true for full objects. Filters: status (exact), q (substring over id + display_name), and meta_key+meta_value (match a scalar metadata field like area/host — e.g. to find the vertical that owns a repo/area). Bounded by limit (default 200, max 1000) + offset.")]
    async fn list_agents(
        &self,
        Parameters(a): Parameters<ListAgentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_agents(
            &self.pool,
            s(&a.status),
            s(&a.q),
            s(&a.meta_key),
            s(&a.meta_value),
            a.verbose.unwrap_or(false),
            a.limit,
            a.offset,
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "Get one agent by id, including its charter and metadata bag. O(1) vs filtering list_agents.")]
    async fn get_agent(
        &self,
        Parameters(a): Parameters<GetAgentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_agent(&self.pool, &a.agent_id).await.map_err(err).and_then(ok)
    }

    #[tool(
        description = "Update an existing agent's fields + metadata WITHOUT re-registering (this is the registry-write path: the board agent list serves as the fleet registry). Pass only the fields you're changing. `metadata` is MERGED into the existing bag, not replaced. To reset a nullable field to null, name it in `clear` (e.g. clear: [\"webhook_url\"]) — an omitted/null field is left unchanged, so `clear` is the only way to empty one. Unlike register_agent this does not force status online and fails if the agent doesn't exist. The response omits the (potentially large) `charter` unless you pass verbose:true; fetch the full agent with get_agent."
    )]
    async fn update_agent(
        &self,
        Parameters(a): Parameters<UpdateAgentArgs>,
    ) -> Result<CallToolResult, McpError> {
        let verbose = a.verbose.unwrap_or(false);
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
            a.clear.as_deref(),
        )
        .await
        .map(|v| if verbose { v } else { core::strip_field(v, "charter") })
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
            a.external_link,
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Update a task. Pass only the fields you're changing. Statuses: todo / in_progress / blocked / done / cancelled. To CLEAR the owner (unassign), set `unassign: true` — this emits task.unassigned. (Prefer `unassign: true` over an empty-string `assignee`: the server treats `assignee=\"\"` as unassign too, but some clients can't serialize an empty string and produce malformed JSON.) Setting a non-empty `assignee` reassigns and emits task.assigned. `metadata` is MERGED into the task's props. Set `actor` to your agent id so you aren't notified of your own change. Notifies subscribers on status/assignee changes (e.g. reassign to hand a ticket to the next pipeline stage). Pass `parent_id` to reparent under an epic (same project), or 0 to clear the parent. When you set status=blocked you MUST pass `blocked_on` (kind: task, agent, or operator) recording what it waits on — kind=agent notifies that agent they are blocking; blocked_on auto-clears when the task leaves the blocked state. The response omits the (potentially large) `description` unless you pass verbose:true; fetch the full task with get_task."
    )]
    async fn update_task(
        &self,
        Parameters(a): Parameters<UpdateTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.me_opt(s(&a.actor));
        // `unassign: true` clears the owner; it maps onto the core empty-string sentinel and wins
        // over any `assignee` value (a client that can't send "" uses this instead).
        let assignee = if a.unassign.unwrap_or(false) {
            Some("")
        } else {
            s(&a.assignee)
        };
        let blocked_on = a.blocked_on.map(|bo| {
            if bo.kind.is_empty() || bo.kind == "none" || bo.kind == "clear" {
                Value::Null
            } else {
                serde_json::json!({ "kind": bo.kind, "target": bo.target, "note": bo.note })
            }
        });
        let verbose = a.verbose.unwrap_or(false);
        core::update_task(
            &self.pool,
            a.task_id,
            s(&a.status),
            assignee,
            s(&a.title),
            s(&a.description),
            s(&a.priority),
            actor.as_deref(),
            a.metadata.map(Value::Object),
            a.parent_id,
            blocked_on,
        )
        .await
        // The response omits the (potentially large) `description` unless verbose:true (task #416).
        .map(|v| if verbose { v } else { core::strip_field(v, "description") })
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

    #[tool(description = "Get one task with its subscribers and a bounded slice of its most-recent comments. By default only the most recent comments are inlined (with comment_count + comments_truncated so you know when there is more) to stay under the read/context cap on a long thread; set comments_limit to page in more, 0 for metadata-only, or a negative number for the whole thread.")]
    async fn get_task(
        &self,
        Parameters(a): Parameters<GetTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Agent-facing default: bound to the most-recent slice unless the caller asks otherwise.
        let limit = a.comments_limit.unwrap_or(core::DEFAULT_TASK_COMMENTS);
        core::get_task_limited(&self.pool, a.task_id, Some(limit)).await.map_err(err).and_then(ok)
    }

    #[tool(description = "Soft-archive a task: it's hidden from list_tasks by default (still visible with include_archived: true), but its comments, subscribers, links, and history are preserved and it still resolves by id. Archiving is orthogonal to status — an archived task keeps whatever status it had. Reversible with restore_task. Use to retire settled or superseded tasks from the active board. Notifies the task's subscribers.")]
    async fn archive_task(
        &self,
        Parameters(a): Parameters<ArchiveTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.me_opt(s(&a.actor));
        core::set_task_archived(&self.pool, a.task_id, true, actor.as_deref())
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Restore a previously archived task (clears the archive stamp so it reappears in the default list_tasks view). Notifies the task's subscribers.")]
    async fn restore_task(
        &self,
        Parameters(a): Parameters<ArchiveTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.me_opt(s(&a.actor));
        core::set_task_archived(&self.pool, a.task_id, false, actor.as_deref())
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "List tasks, optionally filtered by project, status, and/or assignee. Pass `unassigned: true` to list only tasks with no assignee. Nesting: `parent_id` lists an epic's direct children; `top_level: true` lists only unparented tasks (epics + loose tasks — the default board view). `q` is a free-text search over title + description (across all projects when project_id is omitted). `blocked_on_kind` (task|agent|operator) and `blocked_on_ref` give the \"what is waiting on X\" views — e.g. blocked_on_kind=operator for everything awaiting the operator, or blocked_on_ref=<agent> for what is blocked on that agent. Archived tasks are hidden by default; pass `include_archived: true` to list them too.")]
    async fn list_tasks(
        &self,
        Parameters(a): Parameters<ListTasksArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_tasks(&self.pool, a.project_id, s(&a.status), s(&a.assignee), a.unassigned.unwrap_or(false), a.parent_id, a.top_level.unwrap_or(false), s(&a.q), s(&a.blocked_on_kind), s(&a.blocked_on_ref), s(&a.meta_key), s(&a.meta_value), a.include_archived.unwrap_or(false))
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
        core::check_content(&self.pool, &a.body, a.acknowledge_banned.unwrap_or(false))
            .await
            .map_err(err)?;
        core::comment_task(&self.pool, a.task_id, &a.body, author.as_deref(), s(&a.external_author), a.external_link)
            .await
            .map_err(err)
            .and_then(ok)
    }

    // --- Subscriptions ---
    #[tool(description = "Subscribe an agent to a task, a project, a channel, a document, OR the whole board so it's notified of activity there. Give exactly one of task_id / project_id / channel_id / document_id, or set `board: true` for the whole-board firehose. Subscribing to a channel joins it. Pass `event_classes` (e.g. [\"created\"]) to make it a filtered, delivery-gated subscription that only delivers + wakes on those event classes — the low-noise alternative to the full firehose; omit for every event. Idempotent: re-subscribing the same target updates the class set. Pass `thread_root` (a channel post's event seq) to subscribe to a THREAD (#438): you are then delivered + woken on in-thread follow-ups (reply_to = that root) without a re-mention.")]
    async fn subscribe(
        &self,
        Parameters(a): Parameters<SubscribeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let sub = self.me_req(a.subscriber.as_deref())?;
        let board = a.board.unwrap_or(false);
        match (a.thread_root, a.event_classes.as_deref()) {
            (Some(root), _) => core::subscribe_thread(&self.pool, &sub, root).await,
            (None, Some(ec)) if !ec.is_empty() => {
                core::subscribe_classed(&self.pool, &sub, a.task_id, a.project_id, a.channel_id, a.document_id, board, ec).await
            }
            _ => core::subscribe(&self.pool, &sub, a.task_id, a.project_id, a.channel_id, a.document_id, board).await,
        }
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "Stop notifying an agent about a task, project, channel (leaving a channel), document, the whole board (board: true), or a thread (thread_root = the root post seq, to leave a joined thread).")]
    async fn unsubscribe(
        &self,
        Parameters(a): Parameters<SubscribeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let sub = self.me_req(a.subscriber.as_deref())?;
        match a.thread_root {
            Some(root) => core::unsubscribe_thread(&self.pool, &sub, root).await,
            None => core::unsubscribe(&self.pool, &sub, a.task_id, a.project_id, a.channel_id, a.document_id, a.board.unwrap_or(false)).await,
        }
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "Mute a task for yourself: detach from its event fan-out so comments / status changes on it stop notifying (and waking) you — even on a task you created or are assigned (unsubscribe can't do that, since the creator is always in the fan-out). Use it to stand down cleanly from a task you opened. Direct messages still reach you; restore with unmute_task.")]
    async fn mute_task(&self, Parameters(a): Parameters<MuteTaskArgs>) -> Result<CallToolResult, McpError> {
        let who = self.me_req(a.agent.as_deref())?;
        core::mute_task(&self.pool, &who, a.task_id).await.map_err(err).and_then(ok)
    }

    #[tool(description = "Unmute a task for yourself (reverses mute_task): rejoin its event fan-out.")]
    async fn unmute_task(&self, Parameters(a): Parameters<MuteTaskArgs>) -> Result<CallToolResult, McpError> {
        let who = self.me_req(a.agent.as_deref())?;
        core::unmute_task(&self.pool, &who, a.task_id).await.map_err(err).and_then(ok)
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
        description = "Post a message to a channel. You're auto-joined on posting. Every member's inbox gets it (drain with check_notifications). `reply_to` optionally threads under a parent post's seq (one level). `metadata` optionally stamps an arbitrary bag on the post (e.g. a bridge's {slack_ts, thread_ts}); it's surfaced on the post + on channel.outbound_reflect, and a reply's reflect also carries the parent's metadata as parent_metadata."
    )]
    async fn post_to_channel(
        &self,
        Parameters(a): Parameters<PostToChannelArgs>,
    ) -> Result<CallToolResult, McpError> {
        let sender = self.me_req(a.sender.as_deref())?;
        core::post_to_channel_meta(
            &self.pool,
            a.channel_id,
            &sender,
            &a.body,
            a.reply_to,
            s(&a.external_author),
            a.metadata.map(Value::Object),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "Read a channel's post history. Default is oldest-first after `since_seq` (scrollback / catching up on a channel you just joined). Pass desc=true for the LATEST N posts (newest-first — a chat view), and before_seq to page earlier.")]
    async fn get_channel_posts(
        &self,
        Parameters(a): Parameters<GetChannelPostsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_channel_posts(&self.pool, a.channel_id, a.since_seq, a.before_seq, a.limit, a.desc)
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

    #[tool(
        description = "Define or update a custom workspace kind — a named setup_script + config an agent is configured with, so a workspace-materializing tool (e.g. fleet spin-up) supports custom environment kinds defined in board resources. Environment-specific setup lives here as board data. Canonical config keys the consumer reads: cwd (launch dir after setup), pre_trust (extra trusted paths), env (env map); other keys are free-form. Idempotent on `name`: an omitted setup_script/description keeps the stored value, config MERGES. An agent selects it via metadata.workspace_kind = the name."
    )]
    async fn set_workspace_kind(
        &self,
        Parameters(a): Parameters<SetWorkspaceKindArgs>,
    ) -> Result<CallToolResult, McpError> {
        let creator = self.me_opt(None);
        core::set_workspace_kind(
            &self.pool,
            &a.name,
            s(&a.setup_script),
            a.config.map(Value::Object),
            s(&a.description),
            creator.as_deref(),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "Fetch one workspace kind (setup_script + config) by name — what fleet spin-up reads to materialize an agent's workspace.")]
    async fn get_workspace_kind(
        &self,
        Parameters(a): Parameters<WorkspaceKindNameArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_workspace_kind(&self.pool, &a.name).await.map_err(err).and_then(ok)
    }

    #[tool(description = "List all custom workspace kinds (named env setup definitions fleet spin-up materializes from board data).")]
    async fn list_workspace_kinds(&self) -> Result<CallToolResult, McpError> {
        core::list_workspace_kinds(&self.pool).await.map_err(err).and_then(ok)
    }

    #[tool(description = "Retire a workspace kind by name.")]
    async fn delete_workspace_kind(
        &self,
        Parameters(a): Parameters<WorkspaceKindNameArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::delete_workspace_kind(&self.pool, &a.name).await.map_err(err).and_then(ok)
    }

    // --- Banned phrases (pre-submit content lint for docs + comments) ---
    #[tool(
        description = "Add a phrase to the fleet banned-phrases list (jargon/idioms we've agreed not to use in docs and comments). The pre-submit lint on create_document / publish_version / comment_task / comment_document then rejects authored content containing it (case-insensitive, whole-phrase), unless the author passes acknowledge_banned. Idempotent on the phrase; pass an optional note for why or what to write instead."
    )]
    async fn add_banned_phrase(
        &self,
        Parameters(a): Parameters<AddBannedPhraseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let creator = self.me_opt(s(&a.created_by));
        core::add_banned_phrase(&self.pool, &a.phrase, s(&a.note), creator.as_deref())
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "List the fleet banned-phrases list (the phrases the pre-submit content lint checks docs and comments against).")]
    async fn list_banned_phrases(&self) -> Result<CallToolResult, McpError> {
        core::list_banned_phrases(&self.pool).await.map_err(err).and_then(ok)
    }

    #[tool(description = "Remove a phrase from the fleet banned-phrases list.")]
    async fn remove_banned_phrase(
        &self,
        Parameters(a): Parameters<BannedPhraseArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::remove_banned_phrase(&self.pool, &a.phrase).await.map_err(err).and_then(ok)
    }

    // --- Identity aliases (task 532) ---
    #[tool(description = "List the identity aliases (alias -> canonical identity, e.g. operator -> cameron). Use it to resolve or display a floating name like \"operator\" as the real identity across assignee, blocked_on, and @-mentions.")]
    async fn list_identity_aliases(&self) -> Result<CallToolResult, McpError> {
        core::list_identity_aliases(&self.pool).await.map_err(err).and_then(ok)
    }

    #[tool(description = "Upsert an identity alias (alias -> canonical identity), e.g. operator -> cameron. Idempotent on the alias (repoints an existing one); the alias is stored lowercased.")]
    async fn set_identity_alias(
        &self,
        Parameters(a): Parameters<SetIdentityAliasArgs>,
    ) -> Result<CallToolResult, McpError> {
        let creator = self.me_opt(s(&a.created_by));
        core::set_identity_alias(&self.pool, &a.alias, &a.canonical, creator.as_deref())
            .await
            .map_err(err)
            .and_then(ok)
    }

    // --- Secret requests (ephemeral secret-request broker) ---
    #[tool(
        description = "Request a named secret. The board is an ephemeral request broker, never a secret store: this files a request carrying the (non-secret) age recipient pubkeys + instructions and returns a single-use `submit_url` (relative to the board's base) you hand to an operator. The operator opens it, and the value is encrypted IN THE BROWSER to the recipients and posted as ciphertext — the board never sees plaintext. The named `fulfiller` is notified on submit and pulls the ciphertext once (with the returned `fulfiller_token`) to relocate it into durable storage, after which the request is deleted. Use for a token/credential a service needs, without handling the value yourself."
    )]
    async fn request_secret(
        &self,
        Parameters(a): Parameters<RequestSecretArgs>,
    ) -> Result<CallToolResult, McpError> {
        let requested_by = self.me_opt(s(&a.requested_by));
        core::create_secret_request(
            &self.pool,
            &a.name,
            &a.recipients,
            s(&a.instructions),
            s(&a.target),
            s(&a.fulfiller),
            requested_by.as_deref(),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "List secret requests as metadata only (never the ciphertext or tokens) — for visibility into what's requested/submitted/awaiting fulfillment.")]
    async fn list_secret_requests(&self) -> Result<CallToolResult, McpError> {
        core::list_secret_requests(&self.pool).await.map_err(err).and_then(ok)
    }

    #[tool(description = "Fulfill a secret request (fulfiller-token-gated): call this after you've pulled the ciphertext and relocated the secret into its durable home. The board then deletes the request row + its transient ciphertext. Idempotent.")]
    async fn fulfill_secret(
        &self,
        Parameters(a): Parameters<FulfillSecretArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::fulfill_secret(&self.pool, a.id, &a.token).await.map_err(err).and_then(ok)
    }

    #[tool(
        description = "Map a board entity to an entity in a bridged external system (the generic link behind the Slack channel-map, GitHub issue↔task, and thread↔task). `board_kind` is channel|task|thread, `board_id` the board-side id; `source`+`external_id` identify the external side. Idempotent on (source, external_id). This is how a bridge adapter resolves e.g. a board channel to its Slack channel."
    )]
    async fn upsert_external_link(
        &self,
        Parameters(a): Parameters<UpsertExternalLinkArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::upsert_external_link(
            &self.pool,
            &a.source,
            &a.external_id,
            s(&a.external_parent_id),
            &a.board_kind,
            a.board_id,
            a.metadata.map(Value::Object),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "List external links (bridged mappings), filtered by any of `source`, `board_kind`, `board_id`. The read path a bridge adapter uses to resolve a board entity to its external counterpart (or vice-versa).")]
    async fn list_external_links(
        &self,
        Parameters(a): Parameters<ListExternalLinksArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_external_links(&self.pool, s(&a.source), s(&a.board_kind), a.board_id)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Merge key/value props into a channel's metadata. Used to set the outbound reflect-back policy: `direction` ('in' | 'out' | 'both', default 'in') and `outbound_authors` (allowlist, default ['concierge']). A channel.outbound_reflect event fires for a post only when direction allows out AND its author is allowed — how 'only the concierge posts OUT to an external system' is enforced."
    )]
    async fn set_channel_props(
        &self,
        Parameters(a): Parameters<SetChannelPropsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::set_channel_props(&self.pool, a.channel_id, Value::Object(a.props))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Set (or clear) a channel's auto_join flag — a fleet-wide broadcast channel every agent belongs to. Enabling joins every currently-registered agent immediately AND auto-joins each agent registered later, so posts reach everyone without hand-inviting. Disabling only stops future auto-joins (existing members stay; they can unsubscribe). Idempotent."
    )]
    async fn set_channel_auto_join(
        &self,
        Parameters(a): Parameters<SetChannelAutoJoinArgs>,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.me_opt(s(&a.actor));
        core::set_channel_auto_join(&self.pool, a.channel_id, a.auto_join, actor.as_deref())
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Promote a channel thread into a task: the root post becomes the task description and each direct reply becomes a comment (preserving author / external-author attribution + timestamps). Installs a durable thread↔task link; idempotent — re-promoting the same thread returns the existing task, never a duplicate. Pass the root post's seq as root_post_seq."
    )]
    async fn promote_thread(
        &self,
        Parameters(a): Parameters<PromoteThreadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.me_opt(s(&a.actor));
        core::promote_thread(&self.pool, a.channel_id, a.root_post_seq, a.project_id, actor.as_deref())
            .await
            .map_err(err)
            .and_then(ok)
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

    #[tool(description = "Get (or create) the private 1:1 DM channel with another agent, returning the channel and its members. Idempotent and order-independent — the same pair always resolves to the same channel, created on first call. Lets you open/link a DM before any message is sent; send_message reuses this same channel.")]
    async fn open_dm(
        &self,
        Parameters(a): Parameters<OpenDmArgs>,
    ) -> Result<CallToolResult, McpError> {
        let me = self.me_req(a.agent_id.as_deref())?;
        core::get_or_create_dm(&self.pool, &me, &a.with_agent)
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

    #[tool(description = "Read the raw append-only event log (the audit trail of everything). Default oldest-first after `since_seq` (tail the log incrementally); pass desc=true for the LATEST N events newest-first (a live activity feed).")]
    async fn get_events(
        &self,
        Parameters(a): Parameters<GetEventsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_events(&self.pool, a.since_seq, a.limit, s(&a.actor), a.desc).await.map_err(err).and_then(ok)
    }

    // --- Documents ---
    #[tool(
        description = "Create a versioned document. Content lives on IPFS. Normally pass `cid` (a bare content id) — the board stores the identifier only and NEVER resolves it or composes a URL. If you have no local IPFS, pass raw `content` instead and the board content-addresses it server-side (requires the deployment to configure an IPFS backend; otherwise you get a clear error asking for a `cid`). Optionally attach it to a `project_id` and set `metadata` (tags, etc). Creates version 1. Returns the document with its current version + version list. When you pass raw `content`, the board indexes any [[wiki-link]] / [[path|label]] references (outbound links) and ![[path]] / ![[path@vN#region]] references (embeds/transclusions) in it, resolved against document paths, so edges can dangle until a target is filed."
    )]
    async fn create_document(
        &self,
        Parameters(a): Parameters<CreateDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(c) = a.content.as_deref() {
            if core::is_text_content_type(a.content_type.as_deref().unwrap_or("text/markdown")) {
                core::check_content(&self.pool, c, a.acknowledge_banned.unwrap_or(false))
                    .await
                    .map_err(err)?;
            }
        }
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
            s(&a.content_type),
            s(&a.content),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Publish a new immutable version of a document. Pass `cid` (bare content id; the board does not resolve it) or, with no local IPFS, raw `content` to content-address server-side (requires a configured IPFS backend). Appends the version, advances the current pointer, and returns the updated document. A new version drops an approved/changes_requested doc back to in_review. When you pass raw `content`, the board also re-indexes the document's [[wiki-link]] outbound links and ![[embed]] transclusions (a CID-only publish leaves prior edges untouched, since the board never fetches the bytes)."
    )]
    async fn publish_version(
        &self,
        Parameters(a): Parameters<PublishVersionArgs>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(c) = a.content.as_deref() {
            if core::is_text_content_type(a.content_type.as_deref().unwrap_or("text/markdown")) {
                core::check_content(&self.pool, c, a.acknowledge_banned.unwrap_or(false))
                    .await
                    .map_err(err)?;
            }
        }
        let cid = crate::ipfs::resolve_cid(a.cid.as_deref(), a.content.as_deref(), self.ipfs_api_url.as_deref())
            .await
            .map_err(err)?;
        core::publish_version(&self.pool, a.document_id, &cid, s(&a.summary), s(&a.created_by), s(&a.content_type), s(&a.content))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Get one document with its current version and full version list (each version is a bare CID + summary). Pass include_body=true to also inline the current version's markdown, fetched server-side from its pinned CID.")]
    async fn get_document(
        &self,
        Parameters(a): Parameters<GetDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_document_with_body(&self.pool, self.ipfs_api_url.as_deref(), a.document_id, a.include_body)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Rename a document — set its title (a short, specific noun phrase; the viewer renders the title as the page header). Metadata-only: versions, content, wiki path, and review status are untouched. Emits document.updated and notifies subscribers.")]
    async fn update_document(
        &self,
        Parameters(a): Parameters<UpdateDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.me_opt(s(&a.actor));
        core::update_document(&self.pool, a.document_id, &a.title, actor.as_deref())
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Read a document's body content from-session: resolves the version's CID and returns the text (the current version, or pass version_no). The board fetches it through its own IPFS backend, so you don't need local IPFS or a gateway. Binary content (image/pdf/...) returns a null content + the CID to fetch via the REST gateway instead."
    )]
    async fn read_document(
        &self,
        Parameters(a): Parameters<ReadDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::read_document_content(&self.pool, self.ipfs_api_url.as_deref(), a.document_id, a.version_no)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "List a document's versions (immutable), newest first.")]
    async fn get_document_versions(
        &self,
        Parameters(a): Parameters<GetDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_document_versions(&self.pool, a.document_id).await.map_err(err).and_then(ok)
    }

    #[tool(
        description = "Set (or clear) a document's wiki path — its slash-separated place in the wiki tree (e.g. architecture/board/events). Pass an empty string to unfile the document. Paths are unique among filed documents; a collision is rejected. Emits document.updated and notifies subscribers."
    )]
    async fn set_document_path(
        &self,
        Parameters(a): Parameters<SetDocumentPathArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::set_document_path(&self.pool, a.document_id, &a.path, s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "List path-filed documents as a wiki tree, ordered by path. Pass `prefix` to list only what's filed under that path (the prefix itself and everything beneath it); omit it for the whole wiki. Unfiled documents (no path) are excluded — use list_documents for those."
    )]
    async fn list_wiki(
        &self,
        Parameters(a): Parameters<ListWikiArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_wiki(&self.pool, s(&a.prefix), a.include_archived).await.map_err(err).and_then(ok)
    }

    #[tool(description = "List documents for discovery, filtered by any combination of project, status (draft / in_review / approved / changes_requested), tag (a value in metadata.tags), task_id (docs attached to that task), and author. Filters AND together. Archived (retired) documents are hidden unless include_archived=true.")]
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
            a.include_archived,
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
        core::check_content(&self.pool, &a.body, a.acknowledge_banned.unwrap_or(false))
            .await
            .map_err(err)?;
        core::comment_document(
            &self.pool,
            a.document_id,
            a.version_id,
            s(&a.author),
            &a.body,
            a.region.map(Value::Object),
            a.reply_to,
            s(&a.external_author),
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

    #[tool(description = "Soft-archive (retire) a document: it's hidden from list_documents and the wiki tree by default, but its versions, comments, links, and history are preserved and it still resolves by id. Reversible with restore_document. Use for throwaway or superseded docs. Notifies subscribers.")]
    async fn archive_document(
        &self,
        Parameters(a): Parameters<DocumentActorArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::set_document_archived(&self.pool, a.document_id, true, s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Restore a previously archived document (clears the archive stamp so it reappears in listings). Notifies subscribers.")]
    async fn restore_document(
        &self,
        Parameters(a): Parameters<DocumentActorArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::set_document_archived(&self.pool, a.document_id, false, s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
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

    // --- Reviews (a typed review over an artifact, with an A2 lifecycle + an append-only log) ---
    #[tool(
        description = "Create a review over an artifact (a board document, a GitHub pull request, a design, an agent-session, or a task). `kind` classifies the artifact; `source`+`target_ref` point at it (metadata — the board never dereferences target_ref). Starts in the `open` lifecycle state unless you seed another (open / in_review / changes_requested / approved / closed). Records an initial `submitted` log entry and emits review.created. For a bridge, pass `external_link` for idempotent ingest: an artifact already linked on (source, external_id) is returned with `created:false` instead of a duplicate. Returns the review incl. its log and a `created` flag."
    )]
    async fn create_review(
        &self,
        Parameters(a): Parameters<CreateReviewArgs>,
    ) -> Result<CallToolResult, McpError> {
        let created_by = self.me_opt(s(&a.created_by));
        core::create_review(
            &self.pool,
            &a.kind,
            s(&a.source),
            s(&a.target_ref),
            s(&a.title),
            s(&a.status),
            created_by.as_deref(),
            s(&a.assignee),
            a.metadata.map(Value::Object),
            a.external_link,
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "Get one review with its full append-only log (oldest-first). Findings are the log entries of type `finding`.")]
    async fn get_review(
        &self,
        Parameters(a): Parameters<GetReviewArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_review(&self.pool, a.review_id).await.map_err(err).and_then(ok)
    }

    #[tool(description = "List reviews (newest-touched first), optionally filtered by status, kind, and/or assignee. Returns reviews without their logs — fetch one with get_review for the timeline.")]
    async fn list_reviews(
        &self,
        Parameters(a): Parameters<ListReviewsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_reviews(&self.pool, s(&a.status), s(&a.kind), s(&a.assignee))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Improvement trend derived from review logs (no stored counter): findings-per-review with an earlier-vs-later trend, overall and sliced by review kind and by producing area/agent, counterbalanced by an escaped-defect signal (findings logged after approval, re-opens, and lineage follow-ups). A slice where findings fell while escaped defects rose is `flagged` rather than counted as improvement. Optionally filter to one kind and/or area.")]
    async fn review_improvement_trend(
        &self,
        Parameters(a): Parameters<ReviewTrendArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::review_improvement_trend(&self.pool, s(&a.kind), s(&a.area))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Transition a review's lifecycle status: open / in_review / changes_requested / approved / closed. Re-applying the current status is an idempotent no-op. Records a state_change log entry and emits review.status_changed, plus review.opened_for_review when entering in_review and review.terminal when entering approved/closed. approved and closed are the concluding states, but a reopen back to in_review/open is permitted."
    )]
    async fn set_review_status(
        &self,
        Parameters(a): Parameters<SetReviewStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.me_opt(s(&a.actor));
        core::set_review_status(&self.pool, a.review_id, &a.status, actor.as_deref(), s(&a.note))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Set (or clear) a review's `vetted` flag - the adversarial-review gate (adversarial review was run AND addressed). Records who set it and durably logs the change (a decision entry, vetted from->to + actor), emits review.vetted_changed. Setting it to its current value is an idempotent no-op. Per D17 this is audit-only, not identity-gated: the board records the actor rather than blocking a caller. The concluding status transition stays with set_review_status.")]
    async fn set_review_vetted(
        &self,
        Parameters(a): Parameters<SetReviewVettedArgs>,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.me_opt(s(&a.actor));
        core::set_review_vetted(&self.pool, a.review_id, a.vetted, actor.as_deref(), s(&a.note))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Append an entry to a review's log — a comment, a finding (with an optional child `task_id` tracking the fix), a finding_resolved, an adversarial_review note, a revision, or a decision. `entry_type` is one of submitted / revised / finding / finding_resolved / comment / state_change / adversarial_review / decision. Emits review.log_appended. For a bridge, pass `external_id` for idempotent ingest: an entry already logged under that external_id is returned with `appended:false` instead of a duplicate."
    )]
    async fn append_review_log(
        &self,
        Parameters(a): Parameters<AppendReviewLogArgs>,
    ) -> Result<CallToolResult, McpError> {
        let author = self.me_opt(s(&a.author));
        core::append_review_log(
            &self.pool,
            a.review_id,
            &a.entry_type,
            s(&a.body),
            author.as_deref(),
            a.task_id,
            s(&a.external_id),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }
}

#[tool_handler]
impl ServerHandler for Board {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Task-board: a coordination board for agents. The reliable way to identify \
                 yourself is to pass your agent id explicitly in the relevant field on every \
                 call — agent_id (check_notifications / set_status / get_messages), from_agent \
                 (send_message), created_by (create_task/project), or author (comments). That \
                 never fails and works regardless of your client's session handling. As a \
                 convenience, register_agent binds THIS session to your agent id so you can then \
                 omit those fields — but that only holds if your MCP client reuses the \
                 Mcp-Session-Id across calls; some clients (including fleet-native agents) open a \
                 fresh session per call, so the binding won't stick and a 'no identity for this \
                 session' error means exactly that — pass your id explicitly rather than only \
                 re-calling register_agent. Register once (to appear in the agent list + set \
                 presence), then pass ids explicitly if in doubt. Create projects/tasks, comment, \
                 subscribe, and drain your inbox with check_notifications."
                    .to_string(),
            )
    }

    /// Nudge a freshly-connected client to refetch `tools/list`. An MCP client fetches the tool
    /// list once at connect and caches it; when this board redeploys with a new tool/param, a
    /// reconnecting session would otherwise keep the stale schema — so a self-hosting owner
    /// couldn't call surface it just shipped without a full respawn. Emitting
    /// `notifications/tools/list_changed` right after `initialized` prompts a compliant client to
    /// refetch, so the current tool set is always in effect. Best-effort: log and move on if the
    /// peer is already gone.
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        if let Err(e) = context.peer.notify_tool_list_changed().await {
            tracing::warn!("failed to send tools/list_changed on initialize: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::schemars::schema_for;
    use serde_json::{from_value, json};

    /// MCP arg fields tolerate a client that JSON-stringifies scalar/struct values (task 351):
    /// include_body (bool), comments_limit (i64), the list_tasks scalar filters, and blocked_on
    /// (struct) all accept either the native type or its stringified form, while the native form
    /// still works.
    #[test]
    fn mcp_args_tolerate_stringified_scalars() {
        // bool: "true"/"false" strings and the native bool.
        assert!(from_value::<GetDocumentArgs>(json!({"document_id": 1, "include_body": "true"})).unwrap().include_body);
        assert!(!from_value::<GetDocumentArgs>(json!({"document_id": 1, "include_body": "false"})).unwrap().include_body);
        assert!(from_value::<GetDocumentArgs>(json!({"document_id": 1, "include_body": true})).unwrap().include_body);
        assert!(!from_value::<GetDocumentArgs>(json!({"document_id": 1})).unwrap().include_body);

        // i64: stringified and native, plus absent -> None.
        assert_eq!(from_value::<GetTaskArgs>(json!({"task_id": 1, "comments_limit": "5"})).unwrap().comments_limit, Some(5));
        assert_eq!(from_value::<GetTaskArgs>(json!({"task_id": 1, "comments_limit": 5})).unwrap().comments_limit, Some(5));
        assert_eq!(from_value::<GetTaskArgs>(json!({"task_id": 1})).unwrap().comments_limit, None);

        // list_tasks scalar filters: stringified bool + i64.
        let lt = from_value::<ListTasksArgs>(json!({"project_id": "28", "unassigned": "true", "top_level": "false"})).unwrap();
        assert_eq!(lt.project_id, Some(28));
        assert_eq!(lt.unassigned, Some(true));
        assert_eq!(lt.top_level, Some(false));

        // blocked_on: bare kind string, JSON-string of the object, native object, "kind:target", null.
        let bare = from_value::<UpdateTaskArgs>(json!({"task_id": 1, "blocked_on": "operator"})).unwrap().blocked_on.unwrap();
        assert_eq!(bare.kind, "operator");
        assert!(bare.target.is_none());
        let jstr = from_value::<UpdateTaskArgs>(json!({"task_id": 1, "blocked_on": "{\"kind\":\"operator\"}"})).unwrap().blocked_on.unwrap();
        assert_eq!(jstr.kind, "operator");
        let obj = from_value::<UpdateTaskArgs>(json!({"task_id": 1, "blocked_on": {"kind": "agent", "target": "foo"}})).unwrap().blocked_on.unwrap();
        assert_eq!((obj.kind.as_str(), obj.target.as_deref()), ("agent", Some("foo")));
        let kt = from_value::<UpdateTaskArgs>(json!({"task_id": 1, "blocked_on": "task:123"})).unwrap().blocked_on.unwrap();
        assert_eq!((kt.kind.as_str(), kt.target.as_deref()), ("task", Some("123")));
        assert!(from_value::<UpdateTaskArgs>(json!({"task_id": 1, "blocked_on": null})).unwrap().blocked_on.is_none());
        assert!(from_value::<UpdateTaskArgs>(json!({"task_id": 1})).unwrap().blocked_on.is_none());
    }

    // The board advertises tools/list_changed so a client refetches its tool list after a
    // redeploy adds/changes a tool (see on_initialized).
    #[tokio::test]
    async fn advertises_tool_list_changed_capability() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let board = Board::new(pool, None);
        let tools = board.get_info().capabilities.tools.expect("tools capability present");
        assert_eq!(tools.list_changed, Some(true));
        Ok(())
    }

    // `unassign: true` clears a task's owner over MCP, without needing an empty-string assignee
    // (which some clients can't serialize). It maps onto the core unassign sentinel.
    #[tokio::test]
    async fn update_task_unassign_flag_clears_owner() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let board = Board::new(pool.clone(), None);
        let mkfail = |e: McpError| anyhow::anyhow!("{e:?}");

        let p = core::create_project(&pool, "P", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = core::create_task(&pool, pid, "T", None, Some("alice"), None, Some("u"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();
        assert_eq!(core::get_task(&pool, tid).await?["assignee"], serde_json::json!("alice"));

        // unassign: true clears the owner (no empty-string assignee needed).
        board
            .update_task(Parameters(serde_json::from_value(
                serde_json::json!({"task_id": tid, "unassign": true, "actor": "u"}),
            )?))
            .await
            .map_err(mkfail)?;
        assert!(core::get_task(&pool, tid).await?["assignee"].is_null());

        // unassign wins over a concurrently-supplied assignee value.
        core::update_task(&pool, tid, None, Some("bob"), None, None, None, Some("u"), None, None, None).await?;
        board
            .update_task(Parameters(serde_json::from_value(
                serde_json::json!({"task_id": tid, "assignee": "carol", "unassign": true, "actor": "u"}),
            )?))
            .await
            .map_err(mkfail)?;
        assert!(core::get_task(&pool, tid).await?["assignee"].is_null(), "unassign takes precedence over assignee");
        Ok(())
    }

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
        let tasks = core::list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, None, None, false).await?;
        let got = core::get_task(&pool, find(&tasks, "T")).await?;
        assert_eq!(got["created_by"], serde_json::json!("agent:x"));

        // An explicit created_by still wins over the session identity.
        board
            .create_task(Parameters(serde_json::from_value(
                serde_json::json!({"project_id": pid, "title": "T2", "created_by": "other"}),
            )?))
            .await
            .map_err(mkfail)?;
        let tasks = core::list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, None, None, false).await?;
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
        prop_type_is_object(serde_json::to_value(schema_for!(SetChannelPropsArgs)).unwrap(), "props");
        prop_type_is_object(serde_json::to_value(schema_for!(CreateTaskArgs)).unwrap(), "metadata");
        prop_type_is_object(serde_json::to_value(schema_for!(UpdateTaskArgs)).unwrap(), "metadata");
        prop_type_is_object(serde_json::to_value(schema_for!(CreateChannelArgs)).unwrap(), "metadata");
        prop_type_is_object(serde_json::to_value(schema_for!(UpsertExternalIdentityArgs)).unwrap(), "metadata");
        prop_type_is_object(serde_json::to_value(schema_for!(UpsertExternalLinkArgs)).unwrap(), "metadata");
    }
}
