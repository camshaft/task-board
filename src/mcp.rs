//! MCP server exposing the task board to agents over streamable-HTTP.
//!
//! Identity is trust-on-first-use (LAN, no auth yet): tools that act on someone's behalf
//! take an explicit agent id (you pass your own handle). Reads return pretty JSON; writes
//! return the new/affected ids. A faithful port of the Python `board.server` tool surface.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ListResourcesResult, PaginatedRequestParams,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::{NotificationContext, RequestContext};
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
#[serde(deny_unknown_fields)]
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
    /// Arbitrary registry props (role, model, effort, interval, worktree, area,
    /// `repos: [{repo, branch}, ...]` — an agent may span several repos, each checked out
    /// in its own workspace, and `capabilities: ["content-sharing", ...]` — the capability
    /// mandate sets the fleet materializer selects for you, keyed on (role, repos, capabilities)).
    /// A hand-authored `capabilities` CSV/space/newline string or name list is normalized to a
    /// deduped string list. MERGED into any existing bag, not replaced.
    #[serde(default)]
    pub metadata: Option<JsonObject>,
    /// If set, every event delivered to your inbox is also POSTed here (best-effort).
    #[serde(default)]
    pub webhook_url: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetAgentArgs {
    pub agent_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListAgentsArgs {
    /// Filter by exact presence status (online / idle / busy / blocked / away / offline).
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
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub verbose: Option<bool>,
    /// Max rows (default 200, capped at 1000).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Rows to skip (pagination).
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub verbose: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetStatusArgs {
    /// Defaults to the agent this session registered as; pass to act for another.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Roster presence, one of: online, idle, busy, blocked, away, offline. Other/free-form text is
    /// coerced to the nearest presence (and the original salvaged into status_message) -- put your
    /// per-tick narrative in status_message, not here.
    pub status: String,
    #[serde(default)]
    pub status_message: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct MoveTaskArgs {
    pub task_id: i64,
    /// The project to move the task into.
    pub to_project_id: i64,
    /// Set to your agent id so you aren't notified of your own change.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListProjectsArgs {
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetProjectArgs {
    pub project_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTaskArgs {
    /// The project to create the task in. Optional when `parent_id` is given -- a child lives in its
    /// parent's project, so it is inherited. Required for a top-level task (no parent).
    #[serde(default, deserialize_with = "de_opt_i64_lenient")]
    pub project_id: Option<i64>,
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
    #[serde(default, deserialize_with = "de_opt_i64_lenient")]
    pub parent_id: Option<i64>,
    /// Optional external reference for idempotent ingest (bridge adapters). If a task is already
    /// linked on (source, external_id) it's returned with `created:false` instead of a duplicate;
    /// otherwise the task is created and the link recorded atomically (`created:true`).
    #[serde(default)]
    pub external_link: Option<core::ExternalRef>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
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
    #[serde(default, deserialize_with = "de_opt_i64_lenient")]
    pub parent_id: Option<i64>,
    /// What this task is blocked on. REQUIRED when setting status=blocked — a blocked task must
    /// record what it is waiting on. Omit to leave unchanged; pass kind="none" to clear.
    #[serde(default, deserialize_with = "de_opt_blocked_on_lenient")]
    pub blocked_on: Option<BlockedOnArgs>,
    /// Flat alternative to `blocked_on` for a client that cannot nest an object: pass
    /// `blocked_on_kind` (task | agent | team | operator | external, or "none" to clear) together
    /// with `blocked_on_ref` (the blocking task/agent/team id). Ignored when `blocked_on` is given.
    /// Example: blocked_on_kind="task", blocked_on_ref="611".
    #[serde(default)]
    pub blocked_on_kind: Option<String>,
    #[serde(default)]
    pub blocked_on_ref: Option<String>,
    /// Optional note recorded with the flat blocked_on form -- what/why it is waiting, free text.
    /// Paired with `blocked_on_kind`/`blocked_on_ref`; ignored when the nested `blocked_on` object
    /// is given (put the note in that object instead). Example: blocked_on_note="waiting on CAS".
    #[serde(default)]
    pub blocked_on_note: Option<String>,
    /// Return the full task (including the `description`) in the response. Default false — the
    /// response omits the description to keep a looping caller's context light; fetch it with get_task.
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub verbose: Option<bool>,
}

/// What a blocked task is waiting on: kind is task | agent | team | operator | external (or "none" to
/// clear), target is the blocking task id, agent id, or team id (ignored for operator/external). When
/// kind=agent that agent is notified they are blocking; when kind=team every person the team resolves
/// to is notified.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlockedOnArgs {
    pub kind: String,
    /// The blocking id (a task id for kind=task, agent id for kind=agent, team id for kind=team).
    /// A client may send it as a string OR a bare number (e.g. target:611) -- both are accepted.
    #[serde(default, deserialize_with = "de_opt_string_scalar")]
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
            other => Err(format!(
                "expected a boolean (or \"true\"/\"false\"), got string {other:?}"
            )),
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
            t.parse::<i64>().map(Some).map_err(|_| {
                serde::de::Error::custom(format!("expected an integer, got string {s:?}"))
            })
        }
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected an integer, got {other}"
        ))),
    }
}

/// An `Option<String>` where the client may have sent the value as a bare number instead of a
/// string (task 691): accepts null/absent -> None, a string (empty/whitespace -> None), or an
/// integer/number coerced to its decimal string. This lets `blocked_on.target: 611` (an int) work,
/// not just `"611"` -- an agent that writes the bare task id as a number no longer gets "expected a
/// string".
fn de_opt_string_scalar<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<String>, D::Error> {
    match Option::<serde_json::Value>::deserialize(d)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => {
            let t = s.trim();
            Ok(if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            })
        }
        Some(serde_json::Value::Number(n)) => Ok(Some(n.to_string())),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected a string id (or a bare number), got {other}"
        ))),
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
                target: if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                },
                note: None,
            }
        }
        None => BlockedOnArgs {
            kind: s.to_string(),
            target: None,
            note: None,
        },
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
                    .map_err(|e| {
                        serde::de::Error::custom(format!(
                            "blocked_on JSON string did not parse: {e}"
                        ))
                    })
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
#[serde(deny_unknown_fields)]
pub struct SetTaskPropsArgs {
    pub task_id: i64,
    /// Key/value properties to merge into the task's metadata.
    pub props: JsonObject,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetDocumentPropsArgs {
    /// The document id. Omit if you pass `path` instead.
    #[serde(default, deserialize_with = "de_opt_i64_lenient")]
    pub document_id: Option<i64>,
    /// The document's wiki path or slug, as an alternative to document_id (path wins if both given).
    #[serde(default)]
    pub path: Option<String>,
    /// Key/value properties to merge into the document's metadata (e.g. description, type, tags).
    pub props: JsonObject,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ArchiveTaskArgs {
    pub task_id: i64,
    /// The agent performing the archive/restore (for the event actor). Defaults to the
    /// agent this session registered as.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// "What is waiting on X" views: filter blocked tasks by blocked_on kind (task|agent|operator|external).
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
#[serde(deny_unknown_fields)]
pub struct CommentTaskArgs {
    pub task_id: i64,
    pub body: String,
    /// Who is commenting (your agent id). `agent_id`/`actor` are accepted as aliases, since those
    /// are the identity field names other board tools use — so a call that passes agent_id (a common
    /// habit) still records the author, and the self-notify "minus the actor" exclusion fires,
    /// instead of silently storing a null author and echoing your own comment back to you (task 531).
    #[serde(default, alias = "agent_id", alias = "actor")]
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
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub acknowledge_banned: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubscribeArgs {
    /// The agent to (un)subscribe. Defaults to the agent this session registered as.
    /// `agent_id`/`actor` are accepted as aliases, since those are the identity field names
    /// other board tools use — so a call that passes agent_id (a common habit) still names the
    /// subscriber, instead of being rejected with a misleading "no identity for this session"
    /// error (task 901).
    #[serde(default, alias = "agent_id", alias = "actor")]
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
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
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
#[serde(deny_unknown_fields)]
pub struct MuteTaskArgs {
    pub task_id: i64,
    /// The agent to mute/unmute the task for. Defaults to the agent this session registered as.
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ListChannelsArgs {
    /// If set, list channels this agent belongs to (incl. private/DM). Omit for public only.
    #[serde(default)]
    pub member: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetChannelArgs {
    pub channel_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    #[serde(default, deserialize_with = "de_bool_lenient")]
    pub desc: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ListExternalIdentitiesArgs {
    /// Filter by originating system (e.g. "slack"). Omit to list all.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct WorkspaceKindNameArgs {
    /// The workspace kind's name.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SetIdentityAliasArgs {
    /// The alias to map (stored lowercased; the lookup key), e.g. "operator".
    pub alias: String,
    /// The canonical identity it resolves to, e.g. "cameron".
    pub canonical: String,
    #[serde(default)]
    pub created_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BannedPhraseArgs {
    /// The phrase to remove from the banned list.
    pub phrase: String,
}

// --- People / teams (multi-operator model, task 542 Phase 1b) ---

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePersonArgs {
    /// Stable string handle for the person (e.g. "cameron"). Upserts if it already exists.
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTeamArgs {
    /// Stable string handle for the team (e.g. "operator"). Upserts if it already exists.
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PersonIdArgs {
    /// The person's stable string handle.
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TeamIdArgs {
    /// The team's stable string handle.
    pub team_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TeamMemberArgs {
    /// The team to add to / remove from.
    pub team_id: String,
    /// The member's handle: a person id, a team id, or an agent id (per member_kind).
    pub member_id: String,
    /// "person", "team", or "agent" (team-scoped agents, task 542).
    pub member_kind: String,
    #[serde(default)]
    pub created_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LintTextArgs {
    /// The text to dry-run against the live content gate (banned-phrase list + ASCII-only rule).
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GradeDocumentArgs {
    /// The document body (markdown) to grade against the mechanical doc_7 A8 conformance rubric.
    pub content: String,
    /// The document title, graded separately from the body (title/heading rules). Optional.
    #[serde(default)]
    pub title: Option<String>,
    /// Override the main-body prose-word budget. Defaults to the locked ~700-word basis.
    #[serde(default, deserialize_with = "de_opt_i64_lenient")]
    pub body_length_budget_words: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct FulfillSecretArgs {
    pub id: i64,
    /// The fulfiller capability token returned when the request was created.
    pub token: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetChannelPropsArgs {
    pub channel_id: i64,
    /// Key/value properties to merge into the channel's metadata — e.g. the outbound
    /// reflect-back policy `{"direction":"both","outbound_authors":["concierge"]}`.
    pub props: JsonObject,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetChannelAutoJoinArgs {
    pub channel_id: i64,
    /// true = every agent is a member (existing agents joined now + new agents auto-join on
    /// register); false = stop auto-joining (existing members stay).
    #[serde(deserialize_with = "de_bool_lenient")]
    pub auto_join: bool,
    /// The agent performing the change (event actor). Defaults to this session's identity.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ListExternalLinksArgs {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub board_kind: Option<String>,
    #[serde(default)]
    pub board_id: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct InviteToChannelArgs {
    pub channel_id: i64,
    /// The agent to invite (auto-joined).
    pub agent_id: String,
    /// Your agent id (the inviter).
    #[serde(default)]
    pub invited_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckNotificationsArgs {
    /// Whose inbox to drain. Defaults to the agent this session registered as.
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default = "default_true", deserialize_with = "de_bool_lenient")]
    pub mark_read: bool,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendMessageArgs {
    /// Sender. Defaults to the agent this session registered as.
    #[serde(default)]
    pub from_agent: Option<String>,
    pub to_agent: String,
    pub body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenDmArgs {
    /// The other agent in the 1:1 DM.
    pub with_agent: String,
    /// This side of the DM. Defaults to the agent this session registered as.
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetMessagesArgs {
    /// Whose messages. Defaults to the agent this session registered as.
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default = "default_true", deserialize_with = "de_bool_lenient")]
    pub mark_read: bool,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
    #[serde(default, deserialize_with = "de_bool_lenient")]
    pub desc: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// A one-line change-note for this version: what changed and what to look for. The operator
    /// reads it as the review caption (shown on the version list + diff), so a clear note makes
    /// approval review fast and accurate. For v1, a short note on what the document is.
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
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub acknowledge_banned: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// A one-line change-note for THIS revision: what changed since the previous version and what
    /// to look for. The operator reads it as the review caption (version list + diff), so always
    /// fill it on a publish -- a blank change-note makes the operator's review slower.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
    /// MIME type of this version's bytes (default text/markdown). The board records only the label.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Submit even if the content contains a banned phrase (the pre-submit lint otherwise rejects
    /// it). Text content is scanned; non-text content is not.
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub acknowledge_banned: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetDocumentPathArgs {
    pub document_id: i64,
    /// The wiki path to file this document under (e.g. architecture/board/events). An empty
    /// string clears the path (unfiles the doc). Must be unique among filed documents.
    pub path: String,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListWikiArgs {
    /// Only documents filed under this path prefix (e.g. architecture returns architecture and
    /// everything beneath it). Omit for the whole wiki tree. Results are ordered by path.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Include archived (retired) documents in the tree. Hidden by default.
    #[serde(default, deserialize_with = "de_bool_lenient")]
    pub include_archived: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetDocumentArgs {
    /// The document id. Omit if you pass `path` instead.
    #[serde(default, deserialize_with = "de_opt_i64_lenient")]
    pub document_id: Option<i64>,
    /// The document's wiki path (e.g. charters/v-nix) or slug, as an alternative to document_id, so
    /// a doc cited by path can be read without an id lookup first. If both are given, path wins.
    #[serde(default, deserialize_with = "de_opt_string_scalar")]
    pub path: Option<String>,
    /// When true, also fetch the current version's markdown from its pinned CID (server-side, via
    /// the board's IPFS backend) and inline it as `body` — so an agent building or reviewing from an
    /// approved doc gets the content in one call, whatever its own host can reach. Omit/false for
    /// metadata only. If the fetch fails (no backend, unreachable, non-text) the metadata still
    /// returns with `body: null` + a `body_error`. The doc stays CID-only at rest (fetched on read).
    #[serde(default, deserialize_with = "de_bool_lenient")]
    pub include_body: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadDocumentArgs {
    /// The document id. Omit if you pass `path` instead.
    #[serde(default, deserialize_with = "de_opt_i64_lenient")]
    pub document_id: Option<i64>,
    /// The document's wiki path (e.g. charters/v-nix) or slug, as an alternative to document_id, so
    /// a doc cited by path can be read without an id lookup first. If both are given, path wins.
    #[serde(default, deserialize_with = "de_opt_string_scalar")]
    pub path: Option<String>,
    /// Which version's body to read. Omit for the current version.
    #[serde(default)]
    pub version_no: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateDocumentArgs {
    pub document_id: i64,
    /// New title — a short, specific noun phrase; the viewer renders the title as the page header.
    pub title: String,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListDocumentsArgs {
    #[serde(default)]
    pub project_id: Option<i64>,
    /// Status filter. Accepts a single status OR a comma-separated set (match any), and the
    /// operator vocabulary (pending-review -> operator_review, published -> approved).
    #[serde(default)]
    pub status: Option<String>,
    /// A value in the document's metadata.tags array.
    #[serde(default)]
    pub tag: Option<String>,
    /// Exclude documents carrying this tag (e.g. exclude_tag="charter" to hide charters). The
    /// primitive the UI composes default-hide from.
    #[serde(default)]
    pub exclude_tag: Option<String>,
    /// Only documents attached to this task.
    #[serde(default)]
    pub task_id: Option<i64>,
    /// Only documents created by this author (created_by).
    #[serde(default)]
    pub author: Option<String>,
    /// Include archived (retired) documents. Hidden by default.
    #[serde(default, deserialize_with = "de_bool_lenient")]
    pub include_archived: bool,
    /// Include agent-memory documents -- those carrying the reserved `agent-memory` tag, and those
    /// filed under the reserved repos/ or agents/ path prefixes. Hidden from the default feed by
    /// default (task_826); browse memory via list_wiki with a prefix.
    #[serde(default, deserialize_with = "de_bool_lenient")]
    pub include_memory: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommentDocumentArgs {
    pub document_id: i64,
    pub body: String,
    /// The version this comment is written against (anchors the region to immutable content).
    #[serde(default)]
    pub version_id: Option<i64>,
    /// Who is commenting (your agent id). `agent_id`/`actor` are accepted as aliases (the identity
    /// field names other board tools use), so a call that passes agent_id still records the author
    /// instead of storing null and defeating the self-notify exclusion (task 531).
    #[serde(default, alias = "agent_id", alias = "actor")]
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
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub acknowledge_banned: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveCommentArgs {
    pub comment_id: i64,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetDocumentCommentsArgs {
    pub document_id: i64,
    #[serde(default)]
    pub version_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCommentArgs {
    pub comment_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PoseQuestionArgs {
    pub task_id: i64,
    /// Legacy kind -- one of: yes_no, multiple_choice, select_all, fill_in_the_blank, rank_list. OMIT it for a CID-keyed question that instead carries its own `response_schema` plus a `ui.element_schema_cid` (the element's content id, its canonical type identifier).
    #[serde(default)]
    pub kind: Option<String>,
    /// The question prompt.
    pub prompt: String,
    /// Options as [{id, label}] -- required for multiple_choice / select_all / rank_list.
    #[serde(default)]
    pub options: Option<serde_json::Value>,
    /// The principal (person, team, or agent id) the question routes to; "operator" is the seeded team.
    pub routed_to: String,
    /// Whether the question blocks its task while open (default true). A non-blocking question lets the asker proceed, optionally on a `default`.
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub blocking: Option<bool>,
    /// Non-blocking only: the presumed answer the asker proceeds on (in the kind's answer value shape).
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    /// Non-blocking only: wait this many seconds for an answer before proceeding on the default (requires `default`).
    #[serde(default)]
    pub wait_period_seconds: Option<i64>,
    /// Optional inline JSON Schema the framed answer must satisfy (the schema-driven model). When present, a submitted answer is validated against this schema generically rather than by `kind`.
    #[serde(default)]
    pub response_schema: Option<serde_json::Value>,
    /// Optional UI descriptor stored verbatim (element name, props, element-schema CID); resolved by the client, not the board.
    #[serde(default)]
    pub ui: Option<serde_json::Value>,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnswerQuestionArgs {
    /// The question comment id to answer.
    pub comment_id: i64,
    /// The answer shape: bool / choice / text / ranked. Use text for an out-of-frame answer to a non-text kind.
    pub shape: String,
    /// The answer value: a boolean (bool); an array of option ids (choice: 1 for multiple_choice, N for select_all); a string (text); or the option ids in order (ranked).
    pub value: serde_json::Value,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeclineQuestionArgs {
    pub comment_id: i64,
    /// Why the question is declined; delivered to the asker and recorded on the task.
    pub feedback: String,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelQuestionArgs {
    pub comment_id: i64,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SupersedeQuestionArgs {
    pub comment_id: i64,
    /// The prompt for the replacement question (the old one is kept immutable + linked).
    pub new_prompt: String,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListTasksBlockingMeArgs {
    /// The principal to view for (defaults to you). A team-routed question surfaces for its members.
    #[serde(default)]
    pub viewer: Option<String>,
    #[serde(default)]
    pub project_id: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub include_archived: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListAwaitingArgs {
    /// The principal whose awaiting-decision queue to return (defaults to you). "operator" is the
    /// seeded operator team; a team-targeted block or team-routed question surfaces for members.
    #[serde(default)]
    pub viewer: Option<String>,
    #[serde(default)]
    pub project_id: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub include_archived: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentActorArgs {
    pub document_id: i64,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeprecateDocumentArgs {
    pub document_id: i64,
    /// Mark deprecated (default) or, when false, clear the deprecation + supersede link.
    #[serde(default, deserialize_with = "de_opt_bool_lenient")]
    pub deprecated: Option<bool>,
    /// The document that supersedes this one (recorded only when deprecating). Must exist.
    #[serde(default, deserialize_with = "de_opt_i64_lenient")]
    pub superseded_by: Option<i64>,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestChangesArgs {
    pub document_id: i64,
    #[serde(default)]
    pub actor: Option<String>,
    /// Optional note explaining what needs to change.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitToOperatorReviewArgs {
    pub document_id: i64,
    #[serde(default)]
    pub actor: Option<String>,
    /// The doc template you read and followed (e.g. the design-doc template id/name). Required
    /// unless you give a `template_waiver_reason`.
    #[serde(default)]
    pub template_followed: Option<String>,
    /// If no template applies, a non-empty reason why (a weak reason becomes a conformance finding,
    /// so make it substantive). Required only when `template_followed` is absent.
    #[serde(default)]
    pub template_waiver_reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachDocumentArgs {
    pub document_id: i64,
    pub task_id: i64,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateReviewArgs {
    /// What is being reviewed: document | code | design | agent-session | task.
    pub kind: String,
    /// The source classifying where the artifact lives (e.g. board-document, github-pull-request,
    /// url, agent-session, task). Metadata — the board doesn't fetch it.
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
#[serde(deny_unknown_fields)]
pub struct GetReviewArgs {
    pub review_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListReviewsArgs {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewTrendArgs {
    /// Restrict the trend to one review kind (document / code / design / ...).
    #[serde(default)]
    pub kind: Option<String>,
    /// Restrict the trend to one producing area/agent.
    #[serde(default)]
    pub area: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SetReviewVettedArgs {
    pub review_id: i64,
    /// true = mark the review vetted (adversarial review run + addressed); false = clear it.
    #[serde(deserialize_with = "de_bool_lenient")]
    pub vetted: bool,
    /// The agent setting the flag (defaults to this session's identity). Recorded for audit.
    #[serde(default)]
    pub actor: Option<String>,
    /// An optional note recorded on the audit log entry.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
        description = "Register (or update) yourself and mark yourself online. `agent_id` is the stable handle others address you by (e.g. 'agent:fixer-3'). `charter` is your role/mission (free-form). `metadata` is an optional dict of registry props (role, model, effort, interval, worktree, area, `repos: [{repo, branch}, ...]` for the repos you work in — one workspace checkout each, and `capabilities: [\"content-sharing\", ...]` — the capability mandate sets the fleet materializer composes for you, keyed on (role, repos, capabilities); a CSV/list is normalized to a deduped string list), MERGED into any existing bag. If you set `webhook_url`, every event delivered to your inbox is also POSTed there (best-effort)."
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

    #[tool(
        description = "Set your presence: online, idle, busy, blocked, away, or offline (+ an optional status_message note). Other/free-form status text is coerced to the nearest presence and salvaged into status_message -- keep the presence field a clean enum, put per-tick narrative in status_message."
    )]
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

    #[tool(
        description = "Request that an agent gracefully wind down: records the request (who/why/when, visible on the agent's page) and drops an agent.stand_down_requested notification into the agent's inbox so it observes the request on its next loop tick and stands down on its own terms (sets status offline, ends its loop). This is a SIGNAL, not an action — it never changes the agent's status and never kills or interrupts a live agent mid-work. The request stays pending until the agent honors it by going offline (which clears it). Use it to stand an agent down cleanly rather than reaping it."
    )]
    async fn request_stand_down(
        &self,
        Parameters(a): Parameters<RequestStandDownArgs>,
    ) -> Result<CallToolResult, McpError> {
        let requested_by = self.me_opt(s(&a.requested_by));
        core::request_stand_down(
            &self.pool,
            &a.agent_id,
            requested_by.as_deref(),
            s(&a.reason),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "List agents as a lightweight roster: each entry is a compact {id, display_name, status, metadata} — the small metadata bag is kept so callers can filter (e.g. metadata.native); only the heavy charter is dropped to stay under the token cap. Use get_agent for one agent's full charter, or pass verbose:true for full objects. Filters: status (exact), q (substring over id + display_name), and meta_key+meta_value (match a scalar metadata field like area/host — e.g. to find the vertical that owns a repo/area). Bounded by limit (default 200, max 1000) + offset."
    )]
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

    #[tool(
        description = "Get one agent by id, including its charter and metadata bag. O(1) vs filtering list_agents."
    )]
    async fn get_agent(
        &self,
        Parameters(a): Parameters<GetAgentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_agent(&self.pool, &a.agent_id)
            .await
            .map_err(err)
            .and_then(ok)
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
        .map(|v| {
            if verbose {
                v
            } else {
                core::strip_field(v, "charter")
            }
        })
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

    #[tool(
        description = "List projects (optionally filtered by status) with per-status task counts."
    )]
    async fn list_projects(
        &self,
        Parameters(a): Parameters<ListProjectsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_projects(&self.pool, s(&a.status))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Get one project and its tasks.")]
    async fn get_project(
        &self,
        Parameters(a): Parameters<GetProjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_project(&self.pool, a.project_id)
            .await
            .map_err(err)
            .and_then(ok)
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
        description = "Create a task in a project. The creator and assignee are auto-subscribed, so they get notified of future changes. `metadata` is an optional dict of arbitrary properties (pipeline state, source, ipfs_cid, target collection, ...). Pass `parent_id` to nest it under an epic — a child lives in its parent's project, so `project_id` is OPTIONAL when `parent_id` is given (it is inherited); supply `project_id` for a top-level task. Returns the new task incl. its id."
    )]
    async fn create_task(
        &self,
        Parameters(a): Parameters<CreateTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let created_by = self.me_opt(s(&a.created_by));
        // project_id is optional when parent_id is given (task 691/708): a child inherits its
        // parent's project, so the natural epic-decomposition call need not repeat it.
        let project_id = core::resolve_create_project(&self.pool, a.project_id, a.parent_id)
            .await
            .map_err(err)?;
        core::create_task(
            &self.pool,
            project_id,
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
        description = "Update a task. Pass only the fields you're changing. Statuses: todo / in_progress / blocked / done / cancelled. To CLEAR the owner (unassign), set `unassign: true` — this emits task.unassigned. (Prefer `unassign: true` over an empty-string `assignee`: the server treats `assignee=\"\"` as unassign too, but some clients can't serialize an empty string and produce malformed JSON.) Setting a non-empty `assignee` reassigns and emits task.assigned. `metadata` is MERGED into the task's props. Set `actor` to your agent id so you aren't notified of your own change. Notifies subscribers on status/assignee changes (e.g. reassign to hand a ticket to the next pipeline stage). Pass `parent_id` to reparent under an epic (same project), or 0 to clear the parent. Blocking is symmetric (task_902): if you pass `blocked_on` the task is marked blocked AUTOMATICALLY -- you need not also pass status=\"blocked\", and a provided blocked_on is never silently dropped. Conversely status=blocked still requires a `blocked_on` (kind: task, agent, team, operator, or external) recording what it waits on. Passing blocked_on together with an EXPLICIT non-blocked status is a contradiction and is rejected (so 'marking blocked' is always a real state change, not a prose note). Accepted blocked_on forms: a bare kind string `blocked_on:\"operator\"`; the shorthand `blocked_on:\"task:611\"`; the object `blocked_on:{kind:\"task\",target:\"611\"}` (target may be a string OR a bare number, and a task target may be written `611`, `task_611`, or `#611`); or the flat pair `blocked_on_kind:\"task\"` + `blocked_on_ref:\"611\"` for clients that cannot nest. kind=agent notifies that agent they are blocking; kind=external is for an infra/no-owner dependency (put what it waits on in blocked_on.note, no target) and stays OFF the operator queue; blocked_on auto-clears when the task leaves the blocked state. The response omits the (potentially large) `description` unless you pass verbose:true; fetch the full task with get_task."
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
        // `blocked_on` (nested object / string) is preferred; the flat `blocked_on_kind` +
        // `blocked_on_ref` pair is the fallback for a client that cannot send a nested object
        // (task 691). A "none"/"clear"/empty kind maps to Value::Null (clear).
        let flat_blocked_on = a.blocked_on_kind.as_deref().map(|kind| {
            if kind.is_empty() || kind == "none" || kind == "clear" {
                Value::Null
            } else {
                serde_json::json!({ "kind": kind, "target": a.blocked_on_ref, "note": a.blocked_on_note })
            }
        });
        let blocked_on = a
            .blocked_on
            .map(|bo| {
                if bo.kind.is_empty() || bo.kind == "none" || bo.kind == "clear" {
                    Value::Null
                } else {
                    serde_json::json!({ "kind": bo.kind, "target": bo.target, "note": bo.note })
                }
            })
            .or(flat_blocked_on);
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
        .map(|v| {
            if verbose {
                v
            } else {
                core::strip_field(v, "description")
            }
        })
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

    #[tool(
        description = "Get one task with its subscribers and a bounded slice of its most-recent comments. By default only the most recent comments are inlined (with comment_count + comments_truncated so you know when there is more) to stay under the read/context cap on a long thread; set comments_limit to page in more, 0 for metadata-only, or a negative number for the whole thread."
    )]
    async fn get_task(
        &self,
        Parameters(a): Parameters<GetTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Agent-facing default: bound to the most-recent slice unless the caller asks otherwise.
        let limit = a.comments_limit.unwrap_or(core::DEFAULT_TASK_COMMENTS);
        core::get_task_limited(&self.pool, a.task_id, Some(limit))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Soft-archive a task: it's hidden from list_tasks by default (still visible with include_archived: true), but its comments, subscribers, links, and history are preserved and it still resolves by id. Archiving is orthogonal to status — an archived task keeps whatever status it had. Reversible with restore_task. Use to retire settled or superseded tasks from the active board. Notifies the task's subscribers."
    )]
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

    #[tool(
        description = "Restore a previously archived task (clears the archive stamp so it reappears in the default list_tasks view). Notifies the task's subscribers."
    )]
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

    #[tool(
        description = "List tasks, optionally filtered by project, status, and/or assignee. Pass `unassigned: true` to list only tasks with no assignee. Nesting: `parent_id` lists an epic's direct children; `top_level: true` lists only unparented tasks (epics + loose tasks — the default board view). `q` is a free-text search over title + description (across all projects when project_id is omitted). `blocked_on_kind` (task|agent|operator|external) and `blocked_on_ref` give the \"what is waiting on X\" views — e.g. blocked_on_kind=operator for everything awaiting the operator, blocked_on_kind=external for tasks waiting on infra, or blocked_on_ref=<agent> for what is blocked on that agent. Archived tasks are hidden by default; pass `include_archived: true` to list them too."
    )]
    async fn list_tasks(
        &self,
        Parameters(a): Parameters<ListTasksArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_tasks(
            &self.pool,
            a.project_id,
            s(&a.status),
            s(&a.assignee),
            a.unassigned.unwrap_or(false),
            a.parent_id,
            a.top_level.unwrap_or(false),
            s(&a.q),
            s(&a.blocked_on_kind),
            s(&a.blocked_on_ref),
            s(&a.meta_key),
            s(&a.meta_value),
            a.include_archived.unwrap_or(false),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Add a comment to a task. Notifies the task's subscribers/assignee (except you)."
    )]
    async fn comment_task(
        &self,
        Parameters(a): Parameters<CommentTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let author = self.me_opt(s(&a.author));
        core::check_content(&self.pool, &a.body, a.acknowledge_banned.unwrap_or(false))
            .await
            .map_err(err)?;
        core::comment_task(
            &self.pool,
            a.task_id,
            &a.body,
            author.as_deref(),
            s(&a.external_author),
            a.external_link,
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    // --- Subscriptions ---
    #[tool(
        description = "Subscribe an agent to a task, a project, a channel, a document, OR the whole board so it's notified of activity there. Give exactly one of task_id / project_id / channel_id / document_id, or set `board: true` for the whole-board firehose. Subscribing to a channel joins it. Pass `event_classes` (e.g. [\"created\"]) to make it a filtered, delivery-gated subscription that only delivers + wakes on those event classes — the low-noise alternative to the full firehose; omit for every event. Idempotent: re-subscribing the same target updates the class set. Pass `thread_root` (a channel post's event seq) to subscribe to a THREAD (#438): you are then delivered + woken on in-thread follow-ups (reply_to = that root) without a re-mention."
    )]
    async fn subscribe(
        &self,
        Parameters(a): Parameters<SubscribeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let sub = self.me_req(a.subscriber.as_deref())?;
        let board = a.board.unwrap_or(false);
        match (a.thread_root, a.event_classes.as_deref()) {
            (Some(root), _) => core::subscribe_thread(&self.pool, &sub, root).await,
            (None, Some(ec)) if !ec.is_empty() => {
                core::subscribe_classed(
                    &self.pool,
                    &sub,
                    a.task_id,
                    a.project_id,
                    a.channel_id,
                    a.document_id,
                    board,
                    ec,
                )
                .await
            }
            _ => {
                core::subscribe(
                    &self.pool,
                    &sub,
                    a.task_id,
                    a.project_id,
                    a.channel_id,
                    a.document_id,
                    board,
                )
                .await
            }
        }
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Stop notifying an agent about a task, project, channel (leaving a channel), document, the whole board (board: true), or a thread (thread_root = the root post seq, to leave a joined thread)."
    )]
    async fn unsubscribe(
        &self,
        Parameters(a): Parameters<SubscribeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let sub = self.me_req(a.subscriber.as_deref())?;
        match a.thread_root {
            Some(root) => core::unsubscribe_thread(&self.pool, &sub, root).await,
            None => {
                core::unsubscribe(
                    &self.pool,
                    &sub,
                    a.task_id,
                    a.project_id,
                    a.channel_id,
                    a.document_id,
                    a.board.unwrap_or(false),
                )
                .await
            }
        }
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Mute a task for yourself: detach from its event fan-out so comments / status changes on it stop notifying (and waking) you — even on a task you created or are assigned (unsubscribe can't do that, since the creator is always in the fan-out). Use it to stand down cleanly from a task you opened. Direct messages still reach you; restore with unmute_task."
    )]
    async fn mute_task(
        &self,
        Parameters(a): Parameters<MuteTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let who = self.me_req(a.agent.as_deref())?;
        core::mute_task(&self.pool, &who, a.task_id)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Unmute a task for yourself (reverses mute_task): rejoin its event fan-out."
    )]
    async fn unmute_task(
        &self,
        Parameters(a): Parameters<MuteTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let who = self.me_req(a.agent.as_deref())?;
        core::unmute_task(&self.pool, &who, a.task_id)
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

    #[tool(
        description = "List channels. Public channels are always shown; private channels (incl. DMs) only when `member` is set to an agent that belongs to them. Set `member` to your id to list just the channels you're in."
    )]
    async fn list_channels(
        &self,
        Parameters(a): Parameters<ListChannelsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_channels(&self.pool, s(&a.member))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Get one channel with its member list.")]
    async fn get_channel(
        &self,
        Parameters(a): Parameters<GetChannelArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_channel(&self.pool, a.channel_id)
            .await
            .map_err(err)
            .and_then(ok)
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

    #[tool(
        description = "Read a channel's post history. Default is oldest-first after `since_seq` (scrollback / catching up on a channel you just joined). To read the NEWEST posts — e.g. after a channel.post notification woke you and you want the post that woke you — pass desc=true (newest-first); desc=true with limit=1 returns just the latest post. Do NOT escalate `limit` on the default oldest-first order to reach recent posts — that pulls the whole history; use desc=true instead. Use before_seq to page earlier (pass the oldest seq you already have, with desc=true)."
    )]
    async fn get_channel_posts(
        &self,
        Parameters(a): Parameters<GetChannelPostsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_channel_posts(
            &self.pool,
            a.channel_id,
            a.since_seq,
            a.before_seq,
            a.limit,
            a.desc,
        )
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

    #[tool(
        description = "List external (bridged) identities, optionally filtered by `source` (e.g. 'slack'). Newest-updated first."
    )]
    async fn list_external_identities(
        &self,
        Parameters(a): Parameters<ListExternalIdentitiesArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_external_identities(&self.pool, s(&a.source))
            .await
            .map_err(err)
            .and_then(ok)
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

    #[tool(
        description = "Fetch one workspace kind (setup_script + config) by name — what fleet spin-up reads to materialize an agent's workspace."
    )]
    async fn get_workspace_kind(
        &self,
        Parameters(a): Parameters<WorkspaceKindNameArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_workspace_kind(&self.pool, &a.name)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "List all custom workspace kinds (named env setup definitions fleet spin-up materializes from board data)."
    )]
    async fn list_workspace_kinds(&self) -> Result<CallToolResult, McpError> {
        core::list_workspace_kinds(&self.pool)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(description = "Retire a workspace kind by name.")]
    async fn delete_workspace_kind(
        &self,
        Parameters(a): Parameters<WorkspaceKindNameArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::delete_workspace_kind(&self.pool, &a.name)
            .await
            .map_err(err)
            .and_then(ok)
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

    #[tool(
        description = "List the fleet banned-phrases list (the phrases the pre-submit content lint checks docs and comments against)."
    )]
    async fn list_banned_phrases(&self) -> Result<CallToolResult, McpError> {
        core::list_banned_phrases(&self.pool)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Dry-run the FULL pre-submit content lint on arbitrary text WITHOUT writing anything. Returns {clean, banned_phrases:[..], non_ascii:[{char, codepoint, line, column}, ..], bare_refs:[{ref, suggestions:[..]}, ..], soft_refs:[{ref, suggestion}, ..]} against the authoritative live banned-phrases list, the ASCII-only rule, AND the ambiguous bare-\"#N\" typed-ref rule the write path hard-rejects. Use this as a PRE-SEND lint on any composed body (comment/message/task/doc) before the write, so a bare \"#N\" or a banned phrase is caught and fixed with no rejected-write round-trip; each bare_refs hit carries the ready-to-paste typed forms (#task_N canonical / owner-repo#N / drop the # for a plain ordinal). soft_refs are ADVISORY only (they do not affect clean): a hashless typed ref (task_N) is tolerated but nudged toward the canonical #task_N (task_869). Especially useful before a publish_version by CID, which the write-path gate does not scan."
    )]
    async fn lint_text(
        &self,
        Parameters(a): Parameters<LintTextArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::lint_text(&self.pool, &a.text)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Grade a design document against the mechanical doc_7 A8 conformance rubric WITHOUT writing anything. Returns {clean, has_hard_fail, findings:[{check, severity (hard_fail|warn), line, message}]} with actionable-remedy messages, over 8 checks: ascii-only, required-sections-in-order, banned-phrases, title/heading rules, body-hygiene (no tables/images in the main body), status/provenance markers, caps-for-emphasis, and main-body prose length. Pass the document `content` (markdown) and its `title` (graded separately); optionally override `body_length_budget_words`. This is the single grading source of truth -- use it as a pre-submit self-check before publishing a design doc."
    )]
    async fn grade_document(
        &self,
        Parameters(a): Parameters<GradeDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::grade_document(
            &self.pool,
            &a.content,
            a.title.as_deref().unwrap_or(""),
            a.body_length_budget_words,
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(description = "Remove a phrase from the fleet banned-phrases list.")]
    async fn remove_banned_phrase(
        &self,
        Parameters(a): Parameters<BannedPhraseArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::remove_banned_phrase(&self.pool, &a.phrase)
            .await
            .map_err(err)
            .and_then(ok)
    }

    // --- Identity aliases (task 532) ---
    #[tool(
        description = "List the identity aliases (alias -> canonical identity, e.g. operator -> cameron). Use it to resolve or display a floating name like \"operator\" as the real identity across assignee, blocked_on, and @-mentions."
    )]
    async fn list_identity_aliases(&self) -> Result<CallToolResult, McpError> {
        core::list_identity_aliases(&self.pool)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Upsert an identity alias (alias -> canonical identity), e.g. operator -> cameron. Idempotent on the alias (repoints an existing one); the alias is stored lowercased."
    )]
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

    // --- People / teams (multi-operator model, task 542) ---
    #[tool(
        description = "List people (first-class human identities, multi-operator model). A separate registry from agents; a principal resolves across people/agents/teams at read time."
    )]
    async fn list_people(&self) -> Result<CallToolResult, McpError> {
        core::list_people(&self.pool)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Create or upsert a person by stable string handle (e.g. \"cameron\"). Idempotent on the id; display_name/metadata are updated on re-create."
    )]
    async fn create_person(
        &self,
        Parameters(a): Parameters<CreatePersonArgs>,
    ) -> Result<CallToolResult, McpError> {
        let created_by = self.me_opt(s(&a.created_by));
        core::create_person(
            &self.pool,
            &a.id,
            s(&a.display_name),
            created_by.as_deref(),
            a.metadata.map(Value::Object),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Delete a person and drop their team memberships. Errors if the person does not exist."
    )]
    async fn delete_person(
        &self,
        Parameters(a): Parameters<PersonIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::delete_person(&self.pool, &a.id)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "List teams (addressable groups whose members are people OR other teams)."
    )]
    async fn list_teams(&self) -> Result<CallToolResult, McpError> {
        core::list_teams(&self.pool).await.map_err(err).and_then(ok)
    }

    #[tool(
        description = "Create or upsert a team by stable string handle (e.g. \"operator\"). Idempotent on the id; display_name/metadata are updated on re-create."
    )]
    async fn create_team(
        &self,
        Parameters(a): Parameters<CreateTeamArgs>,
    ) -> Result<CallToolResult, McpError> {
        let created_by = self.me_opt(s(&a.created_by));
        core::create_team(
            &self.pool,
            &a.id,
            s(&a.display_name),
            created_by.as_deref(),
            a.metadata.map(Value::Object),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Get a team with its direct members and its fully-resolved principal sets (nested teams expanded, cycle-guarded): returns the team row + members:[{member_id, member_kind}] + resolved_people:[..] + resolved_agents:[..] (people and agents are kept separate)."
    )]
    async fn get_team(
        &self,
        Parameters(a): Parameters<TeamIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_team(&self.pool, &a.team_id)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Delete a team and drop its memberships (its members and its membership in parent teams). Errors if the team does not exist."
    )]
    async fn delete_team(
        &self,
        Parameters(a): Parameters<TeamIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::delete_team(&self.pool, &a.team_id)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Add a person, team, or agent as a member of a team (idempotent). member_kind is \"person\", \"team\", or \"agent\" (team-scoped agents). Rejects a sub-team add that would create a membership cycle, a self-add, and a member that does not exist in its registry. A team resolves to its people AND its agents."
    )]
    async fn add_team_member(
        &self,
        Parameters(a): Parameters<TeamMemberArgs>,
    ) -> Result<CallToolResult, McpError> {
        let created_by = self.me_opt(s(&a.created_by));
        core::add_team_member(
            &self.pool,
            &a.team_id,
            &a.member_id,
            &a.member_kind,
            created_by.as_deref(),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Remove a member (person, team, or agent) from a team (idempotent). member_kind is \"person\", \"team\", or \"agent\"."
    )]
    async fn remove_team_member(
        &self,
        Parameters(a): Parameters<TeamMemberArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::remove_team_member(&self.pool, &a.team_id, &a.member_id, &a.member_kind)
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

    #[tool(
        description = "List secret requests as metadata only (never the ciphertext or tokens) — for visibility into what's requested/submitted/awaiting fulfillment."
    )]
    async fn list_secret_requests(&self) -> Result<CallToolResult, McpError> {
        core::list_secret_requests(&self.pool)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Fulfill a secret request (fulfiller-token-gated): call this after you've pulled the ciphertext and relocated the secret into its durable home. The board then deletes the request row + its transient ciphertext. Idempotent."
    )]
    async fn fulfill_secret(
        &self,
        Parameters(a): Parameters<FulfillSecretArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::fulfill_secret(&self.pool, a.id, &a.token)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Map a board entity to an entity in a bridged external system (the generic link behind the Slack channel-map, GitHub issue↔task, thread↔task, and the chorus doc-attach). `board_kind` is channel|task|thread|comment|document, `board_id` the board-side id; `source`+`external_id` identify the external side. Idempotent on (source, external_id); metadata merges. This is how a bridge adapter resolves e.g. a board channel to its Slack channel, or attaches a chorus URL to a document (board_kind=document, metadata={url})."
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

    #[tool(
        description = "List external links (bridged mappings), filtered by any of `source`, `board_kind`, `board_id`. The read path a bridge adapter uses to resolve a board entity to its external counterpart (or vice-versa)."
    )]
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
        core::promote_thread(
            &self.pool,
            a.channel_id,
            a.root_post_seq,
            a.project_id,
            actor.as_deref(),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Invite another agent into a channel: they're auto-joined and get a channel.invite in their inbox (no accept step). They can unsubscribe to leave."
    )]
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

    #[tool(
        description = "Get (or create) the private 1:1 DM channel with another agent, returning the channel and its members. Idempotent and order-independent — the same pair always resolves to the same channel, created on first call. Lets you open/link a DM before any message is sent; send_message reuses this same channel."
    )]
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

    #[tool(
        description = "Read the raw append-only event log (the audit trail of everything). Default oldest-first after `since_seq` (tail the log incrementally); pass desc=true for the LATEST N events newest-first (a live activity feed)."
    )]
    async fn get_events(
        &self,
        Parameters(a): Parameters<GetEventsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_events(&self.pool, a.since_seq, a.limit, s(&a.actor), a.desc)
            .await
            .map_err(err)
            .and_then(ok)
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
        let cid = crate::ipfs::resolve_cid(
            a.cid.as_deref(),
            a.content.as_deref(),
            self.ipfs_api_url.as_deref(),
        )
        .await
        .map_err(err)?;
        // Published by CID (no inline content the check above could see): fetch + gate the bytes (task 564).
        if a.content.is_none() {
            core::check_cid_content(
                &self.pool,
                self.ipfs_api_url.as_deref(),
                &cid,
                a.content_type.as_deref().unwrap_or("text/markdown"),
                a.acknowledge_banned.unwrap_or(false),
            )
            .await
            .map_err(err)?;
        }
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
        description = "Publish a new immutable version of a document. Pass `cid` (bare content id; the board does not resolve it) or, with no local IPFS, raw `content` to content-address server-side (requires a configured IPFS backend). Always fill `summary` with a one-line change-note for THIS revision (what changed + what to look for): the operator reads it as the review caption on the version list + diff, so a blank summary slows their approval review. Appends the version, advances the current pointer, and returns the updated document. A new version drops an approved/changes_requested doc back to in_review. When you pass raw `content`, the board also re-indexes the document's [[wiki-link]] outbound links and ![[embed]] transclusions (a CID-only publish leaves prior edges untouched, since the board never fetches the bytes)."
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
        let cid = crate::ipfs::resolve_cid(
            a.cid.as_deref(),
            a.content.as_deref(),
            self.ipfs_api_url.as_deref(),
        )
        .await
        .map_err(err)?;
        // Published by CID (no inline content the check above could see): fetch + gate the bytes (task 564).
        if a.content.is_none() {
            core::check_cid_content(
                &self.pool,
                self.ipfs_api_url.as_deref(),
                &cid,
                a.content_type.as_deref().unwrap_or("text/markdown"),
                a.acknowledge_banned.unwrap_or(false),
            )
            .await
            .map_err(err)?;
        }
        core::publish_version(
            &self.pool,
            a.document_id,
            &cid,
            s(&a.summary),
            s(&a.created_by),
            s(&a.content_type),
            s(&a.content),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Get one document with its current version and full version list (each version is a bare CID + summary). Identify it by document_id OR by its wiki path/slug (e.g. charters/v-nix) -- so a doc cited by path can be read without an id lookup first. Pass include_body=true to also inline the current version's markdown, fetched server-side from its pinned CID."
    )]
    async fn get_document(
        &self,
        Parameters(a): Parameters<GetDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = core::resolve_document_ref(&self.pool, a.document_id, a.path.as_deref())
            .await
            .map_err(err)?;
        core::get_document_with_body(&self.pool, self.ipfs_api_url.as_deref(), id, a.include_body)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Rename a document — set its title (a short, specific noun phrase; the viewer renders the title as the page header). Metadata-only: versions, content, wiki path, and review status are untouched. Emits document.updated and notifies subscribers."
    )]
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
        description = "Read a document's body content from-session: resolves the version's CID and returns the text (the current version, or pass version_no). Identify the document by document_id OR by its wiki path/slug (e.g. charters/v-nix). The board fetches it through its own IPFS backend, so you don't need local IPFS or a gateway. Binary content (image/pdf/...) returns a null content + the CID to fetch via the REST gateway instead."
    )]
    async fn read_document(
        &self,
        Parameters(a): Parameters<ReadDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = core::resolve_document_ref(&self.pool, a.document_id, a.path.as_deref())
            .await
            .map_err(err)?;
        core::read_document_content(&self.pool, self.ipfs_api_url.as_deref(), id, a.version_no)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Merge key/value properties into a document's metadata (description, type, tags, provenance) WITHOUT cutting a content version -- the document analog of set_task_props. Identify the document by document_id OR by its wiki path/slug. Use this to refresh an evolving description or tags (the list + wiki index project metadata.description); publish_version is for content, this is for metadata. Emits document.updated."
    )]
    async fn set_document_props(
        &self,
        Parameters(a): Parameters<SetDocumentPropsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = core::resolve_document_ref(&self.pool, a.document_id, a.path.as_deref())
            .await
            .map_err(err)?;
        core::set_document_props(&self.pool, id, Value::Object(a.props), s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "List a document's versions (immutable), newest first. Identify it by document_id OR by its wiki path/slug (e.g. charters/v-nix)."
    )]
    async fn get_document_versions(
        &self,
        Parameters(a): Parameters<GetDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = core::resolve_document_ref(&self.pool, a.document_id, a.path.as_deref())
            .await
            .map_err(err)?;
        core::get_document_versions(&self.pool, id)
            .await
            .map_err(err)
            .and_then(ok)
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
        core::list_wiki(&self.pool, s(&a.prefix), a.include_archived)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "List documents for discovery, filtered by any combination of project, status, tag (a value in metadata.tags), exclude_tag (hide docs with that tag), task_id (docs attached to that task), and author. Filters AND together. `status` accepts a single value OR a comma-separated set (match any) and the operator vocabulary draft / pending-review / published (mapped to the stored draft / operator_review / approved). Archived (retired) documents are hidden unless include_archived=true. Agent-memory documents (those carrying the reserved agent-memory tag, or filed under the reserved repos/ and agents/ path prefixes) are hidden from this feed unless include_memory=true; browse memory via list_wiki with a prefix instead. For the default-hide pattern (e.g. hide charters unless pending-review), compose exclude_tag with a tag+status query."
    )]
    async fn list_documents(
        &self,
        Parameters(a): Parameters<ListDocumentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let statuses = a
            .status
            .as_deref()
            .map(core::parse_status_filter)
            .unwrap_or_default();
        core::list_documents_filtered(
            &self.pool,
            &core::DocListFilter {
                project_id: a.project_id,
                statuses,
                tag: s(&a.tag),
                exclude_tag: s(&a.exclude_tag),
                task_id: a.task_id,
                author: s(&a.author),
                include_archived: a.include_archived,
                include_memory: a.include_memory,
            },
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

    #[tool(
        description = "Mark a document comment resolved (open -> resolved). Notifies the document's subscribers."
    )]
    async fn resolve_comment(
        &self,
        Parameters(a): Parameters<ResolveCommentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::resolve_comment(&self.pool, a.comment_id, s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "List a document's comments (oldest first), optionally filtered by version_id and/or status (open / resolved)."
    )]
    async fn get_document_comments(
        &self,
        Parameters(a): Parameters<GetDocumentCommentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_document_comments(&self.pool, a.document_id, a.version_id, s(&a.status))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Read one comment by its id, with its type (plain/question/answer), parsed payload, lifecycle state, and reply_to/supersedes links. For a machine read of a question/answer comment outside its task thread."
    )]
    async fn get_comment(
        &self,
        Parameters(a): Parameters<GetCommentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_comment(&self.pool, a.comment_id)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Pose a structured question on a task (doc_33), routed to a person/team/agent. Give EITHER a legacy kind (yes_no / multiple_choice / select_all / fill_in_the_blank / rank_list) OR -- for a CID-keyed question (doc_33 v16) -- omit kind and carry an inline response_schema (the validation contract) plus a ui.element_schema_cid (the element's content id, its canonical type identifier the client branches on). When a response_schema is present, submitted answers are validated against it generically. Blocking by default (contributes to the task's question-block until resolved); pass blocking=false for a non-blocking question the asker proceeds on, optionally with a default + wait_period_seconds. Returns the question comment; notifies the routed-to principal."
    )]
    async fn pose_question(
        &self,
        Parameters(a): Parameters<PoseQuestionArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::pose_question_full(
            &self.pool,
            a.task_id,
            a.kind.as_deref(),
            &a.prompt,
            a.options,
            &a.routed_to,
            a.blocking.unwrap_or(true),
            a.default,
            a.wait_period_seconds,
            a.response_schema,
            a.ui,
            self.me_opt(s(&a.actor)).as_deref(),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Answer an open question (doc_33). For a kind-based question a framed answer (shape matching the kind: bool / choice / text / ranked) marks it answered; a text answer to a non-text kind is the universal out-of-frame escape and marks it answered-outside-frame. For a schema-driven question (one carrying a response_schema) the value is validated against that schema generically -- a non-text shape must satisfy it, and a text answer that satisfies it is framed while one that does not is the out-of-frame escape. Records a reply answer comment, clears the task's question-block if it was the last blocking one, and notifies the asker. Returns the answer comment."
    )]
    async fn answer_question(
        &self,
        Parameters(a): Parameters<AnswerQuestionArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::answer_question(
            &self.pool,
            a.comment_id,
            &a.shape,
            a.value,
            self.me_opt(s(&a.actor)).as_deref(),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Decline an open question (doc_33): an explicit refusal with feedback, distinct from an out-of-frame answer. Records the feedback as a reply comment, moves the question to declined, clears the task's question-block if it was the last blocking one, and notifies the asker."
    )]
    async fn decline_question(
        &self,
        Parameters(a): Parameters<DeclineQuestionArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::decline_question(
            &self.pool,
            a.comment_id,
            &a.feedback,
            self.me_opt(s(&a.actor)).as_deref(),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Cancel an open question you posed (doc_33): the asking agent withdraws a question it no longer needs. Only the asker may cancel. Moves it to cancelled, clears the task's question-block if it was the last blocking one, and notifies the routed-to principal."
    )]
    async fn cancel_question(
        &self,
        Parameters(a): Parameters<CancelQuestionArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::cancel_question(
            &self.pool,
            a.comment_id,
            self.me_opt(s(&a.actor)).as_deref(),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Supersede an open question you posed (doc_33 A6): correct or restate it with a replacement. The old question is kept immutable (state -> superseded, linked via superseded_by); the new one is a fresh open question copying the old payload (routing/kind/options/blocking/default) with the new prompt. Only the asker may supersede. A blocking question stays blocked across the swap. Returns the new question comment; notifies the routed-to principal."
    )]
    async fn supersede_question(
        &self,
        Parameters(a): Parameters<SupersedeQuestionArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::supersede_question(
            &self.pool,
            a.comment_id,
            &a.new_prompt,
            self.me_opt(s(&a.actor)).as_deref(),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "List tasks with an open BLOCKING question routed to you (or to `viewer`) -- the question-based 'waiting on me' view (doc_33 A5). A question routed to a team you belong to surfaces here too. Complements the scalar blocked_on 'waiting on me' from list_tasks; union the two for a full picture."
    )]
    async fn list_tasks_blocking_me(
        &self,
        Parameters(a): Parameters<ListTasksBlockingMeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let Some(viewer) = self.me_opt(a.viewer.as_deref()) else {
            return Err(err(anyhow::anyhow!(
                "no viewer: pass `viewer` or call with a session identity"
            )));
        };
        core::list_tasks_blocking_me(
            &self.pool,
            &viewer,
            a.project_id,
            a.include_archived.unwrap_or(false),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "The unified 'awaiting you' queue (task_860 + task_873): everything awaiting a decision from `viewer` (defaults to you), keyed INDEPENDENT of assignee (owner-held tasks are deliberately not assigned to the principal), team-expanded, deduped, as a FLAT array of discriminated items. kind='task' rows {task_id, task_title, project_id, status, updated_at, blocked_on_principal, blocked_on_note, questions:[full question comment objects]} cover tasks blocked_on the principal OR carrying an open blocking question routed to it (answer the questions inline). kind='document' rows {document_id, title, status, version_no, updated_at, path} cover docs awaiting the operator's approval (status operator_review) and appear only when the viewer resolves to the operator. Supersedes list_tasks_blocking_me (questions-only). Pass viewer=operator for the operator's queue."
    )]
    async fn list_awaiting(
        &self,
        Parameters(a): Parameters<ListAwaitingArgs>,
    ) -> Result<CallToolResult, McpError> {
        let Some(viewer) = self.me_opt(a.viewer.as_deref()) else {
            return Err(err(anyhow::anyhow!(
                "no viewer: pass `viewer` or call with a session identity"
            )));
        };
        core::list_awaiting(
            &self.pool,
            &viewer,
            a.project_id,
            a.include_archived.unwrap_or(false),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Submit a document for review (status -> in_review). Notifies the document's subscribers."
    )]
    async fn submit_for_review(
        &self,
        Parameters(a): Parameters<DocumentActorArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::submit_for_review(&self.pool, a.document_id, s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Submit a document into the operator's review queue (status -> operator_review) -- the single gated chokepoint before the operator first sees it. REJECTED unless BOTH hold: (1) you name the doc template you followed (template_followed) or give a non-empty template_waiver_reason, and (2) a design-conformance review has run against the CURRENT version (an adversarial_review summary entry recording reviewed_version == the current version) with zero open findings (every finding's child task done/cancelled). Fail-closed: a doc can never reach the operator un-reviewed. Notifies the document's subscribers."
    )]
    async fn submit_to_operator_review(
        &self,
        Parameters(a): Parameters<SubmitToOperatorReviewArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::submit_to_operator_review(
            &self.pool,
            a.document_id,
            s(&a.actor),
            s(&a.template_followed),
            s(&a.template_waiver_reason),
            self.ipfs_api_url.as_deref(),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Request changes on a document (status -> changes_requested), with an optional note. Notifies the author + subscribers; the author then publishes a new version."
    )]
    async fn request_changes(
        &self,
        Parameters(a): Parameters<RequestChangesArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::request_changes(&self.pool, a.document_id, s(&a.actor), s(&a.note))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Approve a document: stamps the current version as approved (approved_version_id + approved_by) and sets status=approved. Not a lock — publishing a new version reopens review. Notifies subscribers."
    )]
    async fn approve_document(
        &self,
        Parameters(a): Parameters<DocumentActorArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::approve_document(&self.pool, a.document_id, s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Soft-archive (retire) a document: it's hidden from list_documents and the wiki tree by default, but its versions, comments, links, and history are preserved and it still resolves by id. Reversible with restore_document. Use for throwaway or superseded docs. Notifies subscribers."
    )]
    async fn archive_document(
        &self,
        Parameters(a): Parameters<DocumentActorArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::set_document_archived(&self.pool, a.document_id, true, s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Restore a previously archived document (clears the archive stamp so it reappears in listings). Notifies subscribers."
    )]
    async fn restore_document(
        &self,
        Parameters(a): Parameters<DocumentActorArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::set_document_archived(&self.pool, a.document_id, false, s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Mark a document deprecated (optionally recording the document that supersedes it), or pass deprecated=false to clear it. ORTHOGONAL to archive: a deprecated doc stays VISIBLE in listings (a client shows a deprecated / superseded-by banner) rather than being hidden -- use archive_document to hide. deprecated defaults to true; superseded_by is recorded only when deprecating and must reference an existing document. Notifies subscribers."
    )]
    async fn deprecate_document(
        &self,
        Parameters(a): Parameters<DeprecateDocumentArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::set_document_deprecated(
            &self.pool,
            a.document_id,
            a.deprecated.unwrap_or(true),
            a.superseded_by,
            s(&a.actor),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "HARD-DELETE a document and all its dependent rows (versions, comments, attachments, links, embeds). IRREVERSIBLE -- unlike archive_document, this cannot be undone. Guarded: the document must be ARCHIVED first (archive_document), so deletion is always a deliberate two-step. Use only for true garbage (throwaway test docs); prefer archive for anything you might want back. Notifies subscribers (document.deleted)."
    )]
    async fn delete_document(
        &self,
        Parameters(a): Parameters<DocumentActorArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::delete_document(&self.pool, a.document_id, s(&a.actor))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Attach a document to a task (many-to-many). Notifies both the document's and the task's subscribers, so a task watcher learns a design doc landed. Idempotent."
    )]
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

    #[tool(
        description = "Get one review with its full append-only log (oldest-first). Findings are the log entries of type `finding`."
    )]
    async fn get_review(
        &self,
        Parameters(a): Parameters<GetReviewArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::get_review(&self.pool, a.review_id)
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "List reviews (newest-touched first), optionally filtered by status, kind, and/or assignee. Returns reviews without their logs — fetch one with get_review for the timeline."
    )]
    async fn list_reviews(
        &self,
        Parameters(a): Parameters<ListReviewsArgs>,
    ) -> Result<CallToolResult, McpError> {
        core::list_reviews(&self.pool, s(&a.status), s(&a.kind), s(&a.assignee))
            .await
            .map_err(err)
            .and_then(ok)
    }

    #[tool(
        description = "Improvement trend derived from review logs (no stored counter): findings-per-review with an earlier-vs-later trend, overall and sliced by review kind and by producing area/agent, counterbalanced by an escaped-defect signal (findings logged after approval, re-opens, and lineage follow-ups). A slice where findings fell while escaped defects rose is `flagged` rather than counted as improvement. Optionally filter to one kind and/or area."
    )]
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
        core::set_review_status(
            &self.pool,
            a.review_id,
            &a.status,
            actor.as_deref(),
            s(&a.note),
        )
        .await
        .map_err(err)
        .and_then(ok)
    }

    #[tool(
        description = "Set (or clear) a review's `vetted` flag - the adversarial-review gate (adversarial review was run AND addressed). Records who set it and durably logs the change (a decision entry, vetted from->to + actor), emits review.vetted_changed. Setting it to its current value is an idempotent no-op. Per D17 this is audit-only, not identity-gated: the board records the actor rather than blocking a caller. The concluding status transition stays with set_review_status."
    )]
    async fn set_review_vetted(
        &self,
        Parameters(a): Parameters<SetReviewVettedArgs>,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.me_opt(s(&a.actor));
        core::set_review_vetted(
            &self.pool,
            a.review_id,
            a.vetted,
            actor.as_deref(),
            s(&a.note),
        )
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

/// The board's canonical writing-guidance document: the Fleet Doc-Writing Style Guide (`doc_7`,
/// filed at the `guides/doc-writing-style-guide` wiki path). Exposed over MCP as a discoverable
/// resource (task_369) so a resources-aware client can find and load the guidance natively, without
/// a bespoke registration surface. A plain MCP resource is deliberately standard-agnostic: if the
/// MCP skills extension (`io.modelcontextprotocol/skills`, SEP-2640) later stabilizes with host
/// support, this is cheap to repoint to a `skill://` manifest. The board Document stays the
/// authoring + storage layer; this is only the discovery surface over it.
const WRITING_SKILL_DOC_ID: i64 = 7;

/// Stable resource URI for [`WRITING_SKILL_DOC_ID`]. Uses the `file://` scheme because that is the
/// scheme Claude Code's `@`-mention resource picker documents as referenceable (`@server:file://...`);
/// a custom scheme like `skill://` is not confirmed to resolve in the picker today, so a documented
/// scheme keeps the resource actually loadable. The path mirrors the doc's `guides/...` wiki path.
/// If the MCP skills extension later lands with host support, the URI repoints to its manifest then.
const WRITING_SKILL_URI: &str = "file://guides/doc-writing-style-guide";

/// The reserved wiki path of the ui-element catalog document (task_820): the board doc holding the
/// agent-facing name -> {cid, title, description, props_schema} records for every structured-question
/// UI element (built by `--build-ui-catalog`). Resolved by path, not a fixed doc id, so re-creating
/// the doc never breaks the resource.
const UI_ELEMENTS_DOC_PATH: &str = "system/ui-elements";

/// Stable resource URI for the ui-element catalog ([`UI_ELEMENTS_DOC_PATH`]). `file://` scheme to
/// match Claude Code's `@`-mention resource picker (see [`WRITING_SKILL_URI`]); path mirrors the
/// doc's wiki path. read_resource serves the catalog document's current-version content from its CID
/// (doc_728 Solution B) so an agent resolves the live element->CID mapping in one resources/read.
const UI_ELEMENTS_URI: &str = "file://system/ui-elements";

#[tool_handler]
impl ServerHandler for Board {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
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

    /// Advertise the board's discoverable resources. Currently one entry: the Fleet Doc-Writing
    /// Style Guide ([`WRITING_SKILL_DOC_ID`]), so a skills-aware MCP client can find the writing
    /// guidance without a bespoke registration. The set is static, so pagination and
    /// list_changed/subscribe are intentionally not wired (task_369).
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let writing_guide = Resource::new(WRITING_SKILL_URI, "fleet-doc-writing-style-guide")
            .with_title("Fleet Doc-Writing Style Guide")
            .with_description(
                "How to write a fleet document and get it reviewed: the required outline \
                 (Background / Problem Statement / Requirements-Goals-Non-Goals / Solutions with \
                 pros-cons that cite the goals / Recommendation), the ASCII-only and \
                 no-banned-phrases format rules, \
                 and the judgment-layer humanizing patterns a scanner cannot catch. The board \
                 Document doc_7 is the authoritative source; this resource serves its current text.",
            )
            .with_mime_type("text/markdown");
        let ui_elements = Resource::new(UI_ELEMENTS_URI, "ui-element-catalog")
            .with_title("Structured-question UI element catalog")
            .with_description(
                "Resolve the live UI-element -> CID mapping to author a structured (rich) \
                 question. One JSON record per element -- name, cid (stamp it as \
                 ui.element_schema_cid on a CID-keyed pose_question), title, description, and \
                 props_schema (the ui.props contract). Supply an inline response_schema per the \
                 element's description. Served from the system/ui-elements board document's current \
                 version, so it stays current as elements are added -- no local checkout or build.",
            )
            .with_mime_type("application/json");
        Ok(ListResourcesResult::with_all_items(vec![
            writing_guide,
            ui_elements,
        ]))
    }

    /// Read a discoverable resource's content, fetched server-side from the backing document's pinned
    /// CID (the same read path as the read_document tool), so a client gets it in one call regardless
    /// of its host. The writing-skill URI serves [`WRITING_SKILL_DOC_ID`]'s current markdown; the
    /// ui-elements URI serves the catalog document filed at [`UI_ELEMENTS_DOC_PATH`] as JSON
    /// (task_820). Any other URI is a resource-not-found.
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let (doc, uri, mime) = if request.uri == WRITING_SKILL_URI {
            let doc = core::read_document_content(
                &self.pool,
                self.ipfs_api_url.as_deref(),
                WRITING_SKILL_DOC_ID,
                None,
            )
            .await
            .map_err(err)?;
            (doc, WRITING_SKILL_URI, "text/markdown")
        } else if request.uri == UI_ELEMENTS_URI {
            let doc = core::read_document_content_at_path(
                &self.pool,
                self.ipfs_api_url.as_deref(),
                UI_ELEMENTS_DOC_PATH,
            )
            .await
            .map_err(err)?;
            (doc, UI_ELEMENTS_URI, "application/json")
        } else {
            return Err(McpError::resource_not_found(
                format!("no resource {}", request.uri),
                None,
            ));
        };
        let body = doc.get("content").and_then(Value::as_str).ok_or_else(|| {
            McpError::internal_error(format!("resource {uri} has no readable text content"), None)
        })?;
        Ok(
            ReadResourceResult::new(vec![ResourceContents::text(body, uri).with_mime_type(mime)])
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::schemars::schema_for;
    use serde_json::{from_value, json};

    /// Arg structs reject unknown/misnamed params instead of silently dropping them (task 759 /
    /// task_914): the historical bite was set_status(note=...) when the field is status_message, and
    /// update_task typos -- both returned success while the value vanished. deny_unknown_fields makes
    /// serde name the offending field. task_914 generalized it across the whole tool-arg family, so a
    /// misnamed param on ANY tool fails loud; this spot-checks a representative sample. Valid params
    /// still parse, and recognized aliases (agent_id -> author/subscriber) are NOT "unknown".
    #[test]
    fn write_tool_args_reject_unknown_fields() {
        // set_status: the real param is status_message; a stray `note` must fail loud, not drop.
        assert!(
            from_value::<SetStatusArgs>(json!({"status": "online", "note": "oops"})).is_err(),
            "unknown field on SetStatusArgs must be rejected"
        );
        assert!(
            from_value::<SetStatusArgs>(json!({"status": "online", "status_message": "ok"}))
                .is_ok(),
            "valid SetStatusArgs still parses"
        );
        // update_task: a misspelled field must fail loud rather than no-op.
        assert!(
            from_value::<UpdateTaskArgs>(json!({"task_id": 1, "statuss": "done"})).is_err(),
            "unknown field on UpdateTaskArgs must be rejected"
        );
        // task_914: a representative sample across the create / comment / post / message / document /
        // question / review write surface -- a stray key fails loud on each.
        assert!(from_value::<CreateTaskArgs>(json!({"project_id": 1, "titel": "x"})).is_err());
        assert!(from_value::<CommentTaskArgs>(json!({"task_id": 1, "txt": "x"})).is_err());
        assert!(from_value::<PostToChannelArgs>(json!({"channel_id": 1, "msg": "x"})).is_err());
        assert!(from_value::<SendMessageArgs>(json!({"to_agent": "a", "message": "x"})).is_err());
        assert!(from_value::<CreateDocumentArgs>(json!({"title": "t", "contents": "x"})).is_err());
        assert!(from_value::<PoseQuestionArgs>(json!({"prompt": "p", "route_to": "x"})).is_err());
        assert!(from_value::<CreateReviewArgs>(json!({"kind": "k", "targetref": "x"})).is_err());
        // A recognized alias is NOT an unknown field: comment_task still takes agent_id for author,
        // and subscribe still takes agent_id for subscriber (task_531 / task_901), under deny.
        assert!(
            from_value::<CommentTaskArgs>(json!({"task_id": 1, "body": "x", "agent_id": "a"}))
                .is_ok(),
            "a known alias (agent_id) is not rejected by deny_unknown_fields"
        );
        assert!(
            from_value::<SubscribeArgs>(json!({"task_id": 1, "agent_id": "a"})).is_ok(),
            "subscribe agent_id alias still parses under deny_unknown_fields"
        );
    }

    /// A note sent via the FLAT blocked_on form (blocked_on_note, paired with blocked_on_kind/ref)
    /// is now a recognized field, not silently dropped (task 802): the flat path previously had no
    /// note sibling and hardcoded note=null, so a blocked task filed via the flat form lost its
    /// stated reason.
    #[test]
    fn update_task_flat_blocked_on_note_parses() {
        let a = from_value::<UpdateTaskArgs>(json!({
            "task_id": 1,
            "status": "blocked",
            "blocked_on_kind": "agent",
            "blocked_on_ref": "v-board-ui",
            "blocked_on_note": "waiting on the CAS endpoint"
        }))
        .expect("flat blocked_on with a note parses");
        assert_eq!(
            a.blocked_on_note.as_deref(),
            Some("waiting on the CAS endpoint")
        );
    }

    /// get_channel_posts' tool description must surface the recency params, so an agent woken by a
    /// channel.post notification reaches for desc=true (newest-first) instead of escalating `limit`
    /// on the oldest-first default and pulling the whole history (task 817).
    #[test]
    fn get_channel_posts_description_surfaces_recency_params() {
        let tools = Board::tool_router().list_all();
        let t = tools
            .iter()
            .find(|t| t.name == "get_channel_posts")
            .expect("get_channel_posts tool is registered");
        let desc = t.description.as_deref().unwrap_or_default();
        assert!(
            desc.contains("desc=true"),
            "description names desc=true: {desc}"
        );
        assert!(
            desc.contains("NEWEST") || desc.contains("newest") || desc.contains("latest"),
            "description names the newest-posts use-case: {desc}"
        );
    }

    /// MCP arg fields tolerate a client that JSON-stringifies scalar/struct values (task 351):
    /// include_body (bool), comments_limit (i64), the list_tasks scalar filters, and blocked_on
    /// (struct) all accept either the native type or its stringified form, while the native form
    /// still works.
    #[test]
    fn mcp_args_tolerate_stringified_scalars() {
        // bool: "true"/"false" strings and the native bool.
        assert!(
            from_value::<GetDocumentArgs>(json!({"document_id": 1, "include_body": "true"}))
                .unwrap()
                .include_body
        );
        assert!(
            !from_value::<GetDocumentArgs>(json!({"document_id": 1, "include_body": "false"}))
                .unwrap()
                .include_body
        );
        assert!(
            from_value::<GetDocumentArgs>(json!({"document_id": 1, "include_body": true}))
                .unwrap()
                .include_body
        );
        assert!(
            !from_value::<GetDocumentArgs>(json!({"document_id": 1}))
                .unwrap()
                .include_body
        );

        // acknowledge_banned (Option<bool>) on the content-gate tools -- the task 603 break: the
        // harness sent "true" and the server rejected, forcing a raw-REST fallback.
        assert_eq!(
            from_value::<PublishVersionArgs>(
                json!({"document_id": 1, "acknowledge_banned": "true"})
            )
            .unwrap()
            .acknowledge_banned,
            Some(true)
        );
        assert_eq!(
            from_value::<CommentDocumentArgs>(
                json!({"document_id": 1, "body": "x", "acknowledge_banned": "false"})
            )
            .unwrap()
            .acknowledge_banned,
            Some(false)
        );
        assert_eq!(
            from_value::<CommentTaskArgs>(
                json!({"task_id": 1, "body": "x", "acknowledge_banned": true})
            )
            .unwrap()
            .acknowledge_banned,
            Some(true)
        );
        // Required bool args (no default) coerce too.
        assert!(
            from_value::<SetReviewVettedArgs>(json!({"review_id": 1, "vetted": "true"}))
                .unwrap()
                .vetted
        );
        assert!(
            !from_value::<SetChannelAutoJoinArgs>(json!({"channel_id": 1, "auto_join": "false"}))
                .unwrap()
                .auto_join
        );

        // i64: stringified and native, plus absent -> None.
        assert_eq!(
            from_value::<GetTaskArgs>(json!({"task_id": 1, "comments_limit": "5"}))
                .unwrap()
                .comments_limit,
            Some(5)
        );
        assert_eq!(
            from_value::<GetTaskArgs>(json!({"task_id": 1, "comments_limit": 5}))
                .unwrap()
                .comments_limit,
            Some(5)
        );
        assert_eq!(
            from_value::<GetTaskArgs>(json!({"task_id": 1}))
                .unwrap()
                .comments_limit,
            None
        );

        // task 715: create_task / update_task parent_id tolerates a stringified int (the shape the
        // MCP harness handed the server as "628", which previously failed "expected i64").
        assert_eq!(
            from_value::<CreateTaskArgs>(json!({"title": "t", "parent_id": "628"}))
                .unwrap()
                .parent_id,
            Some(628)
        );
        assert_eq!(
            from_value::<CreateTaskArgs>(json!({"title": "t", "parent_id": 628}))
                .unwrap()
                .parent_id,
            Some(628)
        );
        assert_eq!(
            from_value::<UpdateTaskArgs>(json!({"task_id": 1, "parent_id": "5"}))
                .unwrap()
                .parent_id,
            Some(5)
        );
        assert!(from_value::<CreateTaskArgs>(json!({"title": "t"}))
            .unwrap()
            .parent_id
            .is_none());

        // list_tasks scalar filters: stringified bool + i64.
        let lt = from_value::<ListTasksArgs>(
            json!({"project_id": "28", "unassigned": "true", "top_level": "false"}),
        )
        .unwrap();
        assert_eq!(lt.project_id, Some(28));
        assert_eq!(lt.unassigned, Some(true));
        assert_eq!(lt.top_level, Some(false));

        // blocked_on: bare kind string, JSON-string of the object, native object, "kind:target", null.
        let bare = from_value::<UpdateTaskArgs>(json!({"task_id": 1, "blocked_on": "operator"}))
            .unwrap()
            .blocked_on
            .unwrap();
        assert_eq!(bare.kind, "operator");
        assert!(bare.target.is_none());
        let jstr = from_value::<UpdateTaskArgs>(
            json!({"task_id": 1, "blocked_on": "{\"kind\":\"operator\"}"}),
        )
        .unwrap()
        .blocked_on
        .unwrap();
        assert_eq!(jstr.kind, "operator");
        let obj = from_value::<UpdateTaskArgs>(
            json!({"task_id": 1, "blocked_on": {"kind": "agent", "target": "foo"}}),
        )
        .unwrap()
        .blocked_on
        .unwrap();
        assert_eq!(
            (obj.kind.as_str(), obj.target.as_deref()),
            ("agent", Some("foo"))
        );
        let kt = from_value::<UpdateTaskArgs>(json!({"task_id": 1, "blocked_on": "task:123"}))
            .unwrap()
            .blocked_on
            .unwrap();
        assert_eq!(
            (kt.kind.as_str(), kt.target.as_deref()),
            ("task", Some("123"))
        );
        assert!(
            from_value::<UpdateTaskArgs>(json!({"task_id": 1, "blocked_on": null}))
                .unwrap()
                .blocked_on
                .is_none()
        );
        assert!(from_value::<UpdateTaskArgs>(json!({"task_id": 1}))
            .unwrap()
            .blocked_on
            .is_none());

        // task 691: an INTEGER target (the shape an agent naturally writes for a task id) is
        // coerced to its string form, both as a native object and as a JSON-string of the object --
        // previously this failed with "expected a string".
        let int_obj = from_value::<UpdateTaskArgs>(
            json!({"task_id": 1, "blocked_on": {"kind": "task", "target": 611}}),
        )
        .unwrap()
        .blocked_on
        .unwrap();
        assert_eq!(
            (int_obj.kind.as_str(), int_obj.target.as_deref()),
            ("task", Some("611"))
        );
        let int_jstr = from_value::<UpdateTaskArgs>(
            json!({"task_id": 1, "blocked_on": "{\"kind\":\"task\",\"target\":611}"}),
        )
        .unwrap()
        .blocked_on
        .unwrap();
        assert_eq!(int_jstr.target.as_deref(), Some("611"));

        // task 691: the flat blocked_on_kind + blocked_on_ref pair (for clients that cannot nest an
        // object) is carried on the args; the handler folds it into blocked_on when the nested form
        // is absent.
        let flat = from_value::<UpdateTaskArgs>(
            json!({"task_id": 1, "blocked_on_kind": "task", "blocked_on_ref": "611"}),
        )
        .unwrap();
        assert_eq!(flat.blocked_on_kind.as_deref(), Some("task"));
        assert_eq!(flat.blocked_on_ref.as_deref(), Some("611"));
    }

    /// comment_task / comment_document accept the fleet-habit identity field (agent_id / actor) as an
    /// alias for author, so a call that passes agent_id records the author instead of a null that
    /// would defeat the self-notify "minus the actor" exclusion (task 531). Native author still works;
    /// absent stays anonymous (None).
    #[test]
    fn comment_author_accepts_agent_id_and_actor_aliases() {
        let via_agent_id = from_value::<CommentTaskArgs>(
            json!({"task_id": 1, "body": "x", "agent_id": "v-task-board"}),
        )
        .unwrap();
        assert_eq!(via_agent_id.author.as_deref(), Some("v-task-board"));
        let via_actor = from_value::<CommentTaskArgs>(
            json!({"task_id": 1, "body": "x", "actor": "v-task-board"}),
        )
        .unwrap();
        assert_eq!(via_actor.author.as_deref(), Some("v-task-board"));
        let native =
            from_value::<CommentTaskArgs>(json!({"task_id": 1, "body": "x", "author": "alice"}))
                .unwrap();
        assert_eq!(native.author.as_deref(), Some("alice"));
        assert!(
            from_value::<CommentTaskArgs>(json!({"task_id": 1, "body": "x"}))
                .unwrap()
                .author
                .is_none()
        );

        let doc = from_value::<CommentDocumentArgs>(
            json!({"document_id": 1, "body": "x", "agent_id": "v-task-board"}),
        )
        .unwrap();
        assert_eq!(doc.author.as_deref(), Some("v-task-board"));
    }

    /// subscribe / unsubscribe (both Parameters<SubscribeArgs>) accept the fleet-habit identity
    /// field (agent_id / actor) as an alias for subscriber, so a call that passes agent_id names
    /// the subscriber instead of being rejected with a misleading "no identity for this session"
    /// error (task 901). Native subscriber still works; absent stays None (defaults to the session).
    #[test]
    fn subscribe_subscriber_accepts_agent_id_and_actor_aliases() {
        let via_agent_id =
            from_value::<SubscribeArgs>(json!({"task_id": 1, "agent_id": "v-foo"})).unwrap();
        assert_eq!(via_agent_id.subscriber.as_deref(), Some("v-foo"));
        let via_actor =
            from_value::<SubscribeArgs>(json!({"task_id": 1, "actor": "v-foo"})).unwrap();
        assert_eq!(via_actor.subscriber.as_deref(), Some("v-foo"));
        let native =
            from_value::<SubscribeArgs>(json!({"task_id": 1, "subscriber": "v-bar"})).unwrap();
        assert_eq!(native.subscriber.as_deref(), Some("v-bar"));
        assert!(from_value::<SubscribeArgs>(json!({"task_id": 1}))
            .unwrap()
            .subscriber
            .is_none());
    }

    // The board advertises tools/list_changed so a client refetches its tool list after a
    // redeploy adds/changes a tool (see on_initialized).
    #[tokio::test]
    async fn advertises_tool_list_changed_capability() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let board = Board::new(pool, None);
        let tools = board
            .get_info()
            .capabilities
            .tools
            .expect("tools capability present");
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
        let t = core::create_task(
            &pool,
            pid,
            "T",
            None,
            Some("alice"),
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        assert_eq!(
            core::get_task(&pool, tid).await?["assignee"],
            serde_json::json!("alice")
        );

        // unassign: true clears the owner (no empty-string assignee needed).
        board
            .update_task(Parameters(serde_json::from_value(
                serde_json::json!({"task_id": tid, "unassign": true, "actor": "u"}),
            )?))
            .await
            .map_err(mkfail)?;
        assert!(core::get_task(&pool, tid).await?["assignee"].is_null());

        // unassign wins over a concurrently-supplied assignee value.
        core::update_task(
            &pool,
            tid,
            None,
            Some("bob"),
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        board
            .update_task(Parameters(serde_json::from_value(
                serde_json::json!({"task_id": tid, "assignee": "carol", "unassign": true, "actor": "u"}),
            )?))
            .await
            .map_err(mkfail)?;
        assert!(
            core::get_task(&pool, tid).await?["assignee"].is_null(),
            "unassign takes precedence over assignee"
        );
        Ok(())
    }

    // task 691: the flat blocked_on_kind + blocked_on_ref pair blocks a task end-to-end over the
    // MCP handler (for a client that cannot nest an object), equivalent to the nested blocked_on.
    #[tokio::test]
    async fn update_task_flat_blocked_on_pair_blocks_the_task() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let board = Board::new(pool.clone(), None);
        let mkfail = |e: McpError| anyhow::anyhow!("{e:?}");

        let p = core::create_project(&pool, "P", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = core::create_task(
            &pool,
            pid,
            "T",
            None,
            Some("alice"),
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        let dep = core::create_task(
            &pool,
            pid,
            "Dep",
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        let dep_id = dep["id"].as_i64().unwrap();

        // Flat pair with a bare id ref -> the task is blocked on the dependency.
        board
            .update_task(Parameters(serde_json::from_value(serde_json::json!({
                "task_id": tid, "status": "blocked",
                "blocked_on_kind": "task", "blocked_on_ref": dep_id.to_string(), "actor": "u",
            }))?))
            .await
            .map_err(mkfail)?;
        let task = core::get_task(&pool, tid).await?;
        assert_eq!(task["status"], serde_json::json!("blocked"));
        assert_eq!(task["blocked_on"]["kind"], serde_json::json!("task"));
        assert_eq!(
            task["blocked_on"]["target"],
            serde_json::json!(dep_id.to_string())
        );
        Ok(())
    }

    // task 708: a child task (parent_id given) is created WITHOUT a project_id -- it inherits the
    // parent's project, so the natural epic-decomposition call no longer bounces on a missing
    // field. A top-level task with neither project_id nor parent_id still errors clearly.
    #[tokio::test]
    async fn create_task_child_inherits_parent_project() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let board = Board::new(pool.clone(), None);
        let mkfail = |e: McpError| anyhow::anyhow!("{e:?}");

        let p = core::create_project(&pool, "Epic project", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let epic = core::create_task(
            &pool,
            pid,
            "Epic",
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        let epic_id = epic["id"].as_i64().unwrap();

        // Child with parent_id and NO project_id -> created in the parent's project (the call that
        // previously bounced on "missing field project_id").
        board
            .create_task(Parameters(serde_json::from_value(serde_json::json!({
                "title": "Subtask", "parent_id": epic_id, "created_by": "u",
            }))?))
            .await
            .map_err(mkfail)?;
        let kids = core::list_tasks(
            &pool,
            Some(pid),
            None,
            None,
            false,
            Some(epic_id),
            false,
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .await?;
        let kids = kids.as_array().unwrap();
        assert_eq!(
            kids.len(),
            1,
            "the child was created in the parent's project"
        );
        assert_eq!(kids[0]["title"], serde_json::json!("Subtask"));
        assert_eq!(kids[0]["project_id"], serde_json::json!(pid));

        // Neither project_id nor parent_id -> a clear error (not acting on a guessed project).
        assert!(
            board
                .create_task(Parameters(serde_json::from_value(serde_json::json!({
                    "title": "Orphan", "created_by": "u",
                }))?))
                .await
                .is_err(),
            "a top-level task with no project_id must error"
        );
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
        let tasks = core::list_tasks(
            &pool,
            Some(pid),
            None,
            None,
            false,
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .await?;
        let got = core::get_task(&pool, find(&tasks, "T")).await?;
        assert_eq!(got["created_by"], serde_json::json!("agent:x"));

        // An explicit created_by still wins over the session identity.
        board
            .create_task(Parameters(serde_json::from_value(
                serde_json::json!({"project_id": pid, "title": "T2", "created_by": "other"}),
            )?))
            .await
            .map_err(mkfail)?;
        let tasks = core::list_tasks(
            &pool,
            Some(pid),
            None,
            None,
            false,
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .await?;
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
        assert_eq!(
            core::get_agent(&pool, "agent:x").await?["status"],
            serde_json::json!("busy")
        );

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
        let notes = msgs["notifications"]
            .as_array()
            .expect("notifications array");
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
            || ty
                .as_array()
                .is_some_and(|a| a.iter().any(|t| t == "object"));
        assert!(is_object, "{prop} should be an object schema, got {schema}");
    }

    #[test]
    fn free_form_json_args_have_object_schemas() {
        prop_type_is_object(
            serde_json::to_value(schema_for!(SetTaskPropsArgs)).unwrap(),
            "props",
        );
        prop_type_is_object(
            serde_json::to_value(schema_for!(SetDocumentPropsArgs)).unwrap(),
            "props",
        );
        prop_type_is_object(
            serde_json::to_value(schema_for!(SetChannelPropsArgs)).unwrap(),
            "props",
        );
        prop_type_is_object(
            serde_json::to_value(schema_for!(CreateTaskArgs)).unwrap(),
            "metadata",
        );
        prop_type_is_object(
            serde_json::to_value(schema_for!(UpdateTaskArgs)).unwrap(),
            "metadata",
        );
        prop_type_is_object(
            serde_json::to_value(schema_for!(CreateChannelArgs)).unwrap(),
            "metadata",
        );
        prop_type_is_object(
            serde_json::to_value(schema_for!(UpsertExternalIdentityArgs)).unwrap(),
            "metadata",
        );
        prop_type_is_object(
            serde_json::to_value(schema_for!(UpsertExternalLinkArgs)).unwrap(),
            "metadata",
        );
    }
}
