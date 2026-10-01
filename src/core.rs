//! Task-board operations. Async functions returning `serde_json::Value`, so they're
//! usable from both the MCP layer and the REST API. A faithful port of the Python
//! `board.core` module — same fields, same notification semantics.

use serde_json::{json, Map, Value};
use sqlx::sqlite::{SqliteColumn, SqliteRow};
use sqlx::{Column, Row, Sqlite, Transaction, TypeInfo, ValueRef};

use crate::db::Pool;
use crate::events::{emit, fire_webhooks, now_iso, Recipients, WebhookDelivery};

use std::collections::BTreeSet;
use std::time::Duration;

/// Convert a dynamically-typed SQLite row into a JSON object, matching how the Python
/// `sqlite3.Row` -> dict conversion behaves (ints, floats, text, null).
pub fn row_to_json(row: &SqliteRow) -> Value {
    let mut obj = Map::new();
    for col in row.columns() {
        let name = col.name().to_string();
        obj.insert(name.clone(), column_to_json(row, col));
    }
    Value::Object(obj)
}

/// Add the typed canonical id `ref` ("<kind>_<id>", e.g. "task_472") to a resource object that
/// already carries its integer `id` (task 504). `kind` is the resource prefix ("task" | "doc" |
/// "project" | "channel"). The integer `id` field is RETAINED unchanged as a back-compat numeric
/// alias; `ref` is the canonical form the API returns, links render, and agents pass around. A
/// no-op if the object has no integer `id`.
pub fn insert_ref(m: &mut Map<String, Value>, kind: &str) {
    if let Some(id) = m.get("id").and_then(|v| v.as_i64()) {
        m.insert("ref".into(), json!(format!("{kind}_{id}")));
    }
}

/// `row_to_json` plus the typed canonical `ref` (see [`insert_ref`]) — for id-bearing resource
/// rows (tasks, documents, ...) so nested summaries carry a deep-linkable ref too.
pub fn row_to_json_ref(row: &SqliteRow, kind: &str) -> Value {
    let mut d = row_to_json(row);
    if let Value::Object(ref mut m) = d {
        insert_ref(m, kind);
    }
    d
}

/// Detect bare board-task references ("#<N>") in submitted user text that are ambiguous under the
/// #504 typed-id convention (task #517). A bare "#N" still resolves to a board task, but is easy to
/// confuse with an external GitHub reference, so a write nudges the author toward the typed `task_N`
/// form (or a repo-qualified `owner/repo#N` for an external PR). The boundary rule mirrors the web
/// linkifier: "#" + digits where the char before "#" is not a word char / another "#" / "&" (so
/// "abc#1", "##", "&#123;", and a repo-qualified "owner/repo#1" -- whose "#" follows a word char --
/// do NOT match), and the digits end at a word boundary (so "#12ab" does not match). Returns the
/// referenced task numbers in first-seen order, de-duplicated. Pure text scan, no DB lookup.
pub fn detect_bare_task_refs(text: &str) -> Vec<i64> {
    let bytes = text.as_bytes();
    let mut out: Vec<i64> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'#' {
            i += 1;
            continue;
        }
        // Reject when the char immediately before '#' is a word char / '#' / '&' (this is what
        // excludes a repo-qualified "owner/repo#N", whose '#' follows a word char).
        if i > 0 {
            let p = bytes[i - 1];
            if p == b'_' || p == b'#' || p == b'&' || p.is_ascii_alphanumeric() {
                i += 1;
                continue;
            }
        }
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j == start {
            i += 1; // a bare '#' with no digits
            continue;
        }
        // Require a word boundary after the digits (so "#12ab" is not a ref).
        if j < bytes.len() {
            let n = bytes[j];
            if n == b'_' || n.is_ascii_alphanumeric() {
                i = j + 1;
                continue;
            }
        }
        if let Ok(num) = text[start..j].parse::<i64>() {
            if num > 0 && !out.contains(&num) {
                out.push(num);
            }
        }
        i = j;
    }
    out
}

/// Blank out fenced code blocks (```...```) and inline code spans (`...`) from `text`, replacing
/// each with a single space, so a bare "#N" inside code or an example is never seen as a reference
/// (bare-#N hard-fail). Non-code text is copied verbatim (backticks are ASCII, so the slice
/// boundaries are always valid UTF-8). Dependency-free.
fn strip_code_regions(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut seg_start = 0;
    while i < b.len() {
        if b[i..].starts_with(b"```") {
            out.push_str(&text[seg_start..i]);
            i += 3;
            while i < b.len() && !b[i..].starts_with(b"```") {
                i += 1;
            }
            i = (i + 3).min(b.len()); // consume the closing fence (or run to EOF)
            out.push(' ');
            seg_start = i;
        } else if b[i] == b'`' {
            out.push_str(&text[seg_start..i]);
            i += 1;
            while i < b.len() && b[i] != b'`' {
                i += 1;
            }
            if i < b.len() {
                i += 1; // consume the closing backtick
            }
            out.push(' ');
            seg_start = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&text[seg_start..]);
    out
}

/// Reject a submitted bare "#N" board-task reference (task #517 follow-on; operator decision
/// seq-7557: hard-fail over silent normalize, "so the agent learns"). A bare "#N" is ambiguous
/// under the #504 typed-id convention, so instead of guessing we block the write with an actionable
/// error naming the typed form. Scans OUTSIDE code (fenced blocks + inline spans are skipped, as are
/// owner/repo#N and &#123; via [`detect_bare_task_refs`]). No override: the only way past is to write
/// the typed form. A no-op when the text has no bare ref.
pub fn check_bare_refs(text: &str) -> anyhow::Result<()> {
    let stripped = strip_code_regions(text);
    if let Some(&n) = detect_bare_task_refs(&stripped).first() {
        anyhow::bail!(
            "ambiguous bare reference \"#{n}\": write task_{n} for a board task, camshaft/task-board#{n} \
             (or <owner>/<repo>#{n}) for a GitHub PR/issue, or -- if {n} is a plain number such as a \
             board message or sequence ordinal -- drop the # and write \"{n}\""
        );
    }
    Ok(())
}

/// If `data` carries an `external_author` (an external_identities id, e.g. an ingested Slack
/// user like `slack:U0…`), resolve that identity's registered `display_name` and add it as
/// `external_author_name`, so a consumer can show the human's name while `external_author`
/// stays the stable key. A no-op when there's no external_author, or the identity is
/// unregistered / has no display_name (the consumer then falls back to the id). Must NOT be
/// called while a transaction holds the single pooled connection — resolve after commit.
async fn add_external_author_name(pool: &Pool, data: &mut Value) {
    let Value::Object(m) = data else { return };
    let Some(id) = m
        .get("external_author")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return;
    };
    let name: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT display_name FROM external_identities WHERE id=?",
    )
    .bind(&id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten();
    if let Some(name) = name {
        m.insert("external_author_name".into(), json!(name));
    }
}

fn column_to_json(row: &SqliteRow, col: &SqliteColumn) -> Value {
    let raw = match row.try_get_raw(col.ordinal()) {
        Ok(r) => r,
        Err(_) => return Value::Null,
    };
    if raw.is_null() {
        return Value::Null;
    }
    match col.type_info().name() {
        "INTEGER" | "BIGINT" => row
            .try_get::<i64, _>(col.ordinal())
            .map(|v| json!(v))
            .unwrap_or(Value::Null),
        "REAL" | "FLOAT" | "DOUBLE" => row
            .try_get::<f64, _>(col.ordinal())
            .map(|v| json!(v))
            .unwrap_or(Value::Null),
        _ => row
            .try_get::<String, _>(col.ordinal())
            .map(Value::String)
            // Fall back to int/float for columns SQLite reports without a declared type.
            .or_else(|_| row.try_get::<i64, _>(col.ordinal()).map(|v| json!(v)))
            .or_else(|_| row.try_get::<f64, _>(col.ordinal()).map(|v| json!(v)))
            .unwrap_or(Value::Null),
    }
}

/// The `@id` tokens in `text` (id chars = alphanumeric / `-` / `_`), for @mention auto-subscribe.
/// Returns the bare ids (no `@`), in order, with duplicates possible — callers dedupe via the
/// idempotent subscribe.
fn extract_mentions(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-' || bytes[j] == b'_')
            {
                j += 1;
            }
            if j > start {
                out.push(text[start..j].to_string());
            }
            i = j.max(start);
        } else {
            i += 1;
        }
    }
    out
}

/// Auto-subscribe any @mentioned REGISTERED agent in `text` to the task (idempotent). Per the
/// operator's subscription-based wake model, an @mention adds the agent to the subscription list
/// so they are woken on this and future activity. Unregistered `@tokens` are ignored (no junk subs).
async fn subscribe_mentions(
    tx: &mut Transaction<'_, Sqlite>,
    text: &str,
    task_id: i64,
) -> anyhow::Result<()> {
    for id in extract_mentions(text) {
        let exists = sqlx::query("SELECT 1 FROM agents WHERE id=?")
            .bind(&id)
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
        if exists {
            auto_subscribe(tx, Some(&id), task_id).await?;
        }
    }
    Ok(())
}

async fn auto_subscribe(
    tx: &mut Transaction<'_, Sqlite>,
    subscriber: Option<&str>,
    task_id: i64,
) -> anyhow::Result<()> {
    if let Some(sub) = subscriber {
        sqlx::query(
            "INSERT OR IGNORE INTO subscriptions(subscriber, target_type, target_id, created_at) \
             VALUES(?,'task',?,?)",
        )
        .bind(sub)
        .bind(task_id)
        .bind(now_iso())
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Auto-subscribe an agent to a document (idempotent), so they hear about future versions and
/// review activity. Mirrors auto_subscribe for tasks and auto-join-on-post for channels.
async fn auto_subscribe_document(
    tx: &mut Transaction<'_, Sqlite>,
    subscriber: Option<&str>,
    document_id: i64,
) -> anyhow::Result<()> {
    if let Some(sub) = subscriber {
        sqlx::query(
            "INSERT OR IGNORE INTO subscriptions(subscriber, target_type, target_id, created_at) \
             VALUES(?,'document',?,?)",
        )
        .bind(sub)
        .bind(document_id)
        .bind(now_iso())
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Auto-subscribe any @mentioned REGISTERED agent in `text` to the DOCUMENT (idempotent) -- the
/// document analog of subscribe_mentions for tasks. An @mention in a doc comment adds the agent to
/// the doc's subscription list so the document.comment fan-out reaches them, i.e. a mention notifies
/// the mentioned agent regardless of their prior subscription, matching task-comment @mentions.
/// Without this, doc-comment @mentions were a silent black hole (operator-reported: an operator's
/// @mention in a doc comment went completely unanswered). Unregistered @tokens are ignored.
async fn subscribe_mentions_document(
    tx: &mut Transaction<'_, Sqlite>,
    text: &str,
    document_id: i64,
) -> anyhow::Result<()> {
    for id in extract_mentions(text) {
        let exists = sqlx::query("SELECT 1 FROM agents WHERE id=?")
            .bind(&id)
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
        if exists {
            auto_subscribe_document(tx, Some(&id), document_id).await?;
        }
    }
    Ok(())
}

async fn fetch_one_json(
    tx: &mut Transaction<'_, Sqlite>,
    sql: &str,
    id: i64,
) -> anyhow::Result<Option<Value>> {
    Ok(sqlx::query(sql)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .as_ref()
        .map(row_to_json))
}

// --- Agents / presence ---

/// Turn an agent row into JSON with its `metadata` TEXT column parsed from a JSON string
/// into an object (mirrors how get_project/get_task surface their metadata). `row_to_json`
/// leaves it as a raw string, so every agent-returning path funnels through this.
fn agent_json(row: &SqliteRow) -> Value {
    let mut obj = match row_to_json(row) {
        Value::Object(m) => m,
        other => return other,
    };
    let meta = obj
        .get("metadata")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| json!({}));
    obj.insert("metadata".into(), meta);
    Value::Object(obj)
}

/// A minimal presence projection of an agent object — the fields a presence ping actually needs
/// (id, status, status_message, last_seen). set_status returns this instead of the full agent so a
/// long-running /loop agent doesn't re-ingest its own multi-KB charter into context on every tick
/// (task #416); the full object stays available via get_agent.
pub fn presence_projection(agent: Value) -> Value {
    let Value::Object(o) = agent else {
        return agent;
    };
    let mut m = Map::new();
    for k in ["id", "status", "status_message", "last_seen"] {
        if let Some(v) = o.get(k) {
            m.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(m)
}

/// Drop one heavy long-text field (an agent `charter` or a task `description`) from a mutation
/// response unless the caller opted into the full object via a `verbose` flag. A no-op on a
/// non-object. Keeps update_agent/update_task responses light for a looping caller (task #416).
pub fn strip_field(mut v: Value, field: &str) -> Value {
    if let Value::Object(ref mut m) = v {
        m.remove(field);
    }
    v
}

/// Merge `incoming` (an object) into `base_str` (a JSON string, default '{}') and return the
/// merged JSON as a string. Shallow key-level merge — the same semantics tasks/projects use.
fn merge_metadata(base_str: Option<&str>, incoming: Value) -> String {
    let mut base: Map<String, Value> =
        serde_json::from_str(base_str.unwrap_or("{}")).unwrap_or_default();
    if let Value::Object(m) = incoming {
        for (k, v) in m {
            base.insert(k, v);
        }
    }
    // Coerce a hand-authored metadata.repos into the structured [{"repo": <name>}] list that fleet
    // spin-up expects (task 476): a CSV / space / newline string, or a list of bare name strings,
    // becomes the object-list form. A value already in the object-list form is left untouched, so
    // this is idempotent. Defense at the write source, so a raw register_agent/update_agent can't
    // store a shape that spin-up silently drops (the membrain-cdk incident).
    if let Some(coerced) = coerce_repos_metadata(base.get("repos")) {
        base.insert("repos".into(), coerced);
    }
    Value::Object(base).to_string()
}

/// If `repos` is a delimited string or a list containing bare name strings, return the structured
/// `[{"repo": <name>}]` form; otherwise `None` (already structured / absent / unrecognized — leave
/// as-is). See [`merge_metadata`] (task 476).
fn coerce_repos_metadata(repos: Option<&Value>) -> Option<Value> {
    match repos? {
        Value::String(s) => Some(Value::Array(
            s.split([',', '\n', '\t', ' '])
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| json!({ "repo": t }))
                .collect(),
        )),
        Value::Array(items) if items.iter().any(Value::is_string) => Some(Value::Array(
            items
                .iter()
                .map(|it| match it {
                    Value::String(name) => json!({ "repo": name }),
                    other => other.clone(),
                })
                .collect(),
        )),
        _ => None,
    }
}

pub async fn register_agent(
    pool: &Pool,
    agent_id: &str,
    display_name: Option<&str>,
    kind: Option<&str>,
    charter: Option<&str>,
    metadata: Option<Value>,
    webhook_url: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let existing = sqlx::query("SELECT metadata FROM agents WHERE id=?")
        .bind(agent_id)
        .fetch_optional(&mut *tx)
        .await?;
    // metadata is MERGED into any existing bag (like update_project), so re-registering with
    // a partial bag adds/overwrites keys without dropping the rest.
    let merged_meta: Option<String> = metadata.map(|m| {
        let base: Option<String> = existing
            .as_ref()
            .and_then(|r| r.try_get::<Option<String>, _>("metadata").ok().flatten());
        merge_metadata(base.as_deref(), m)
    });
    if existing.is_some() {
        // COALESCE so re-registering to refresh presence doesn't clobber a charter (or any
        // other field) set earlier — only fields explicitly supplied this call overwrite.
        sqlx::query(
            "UPDATE agents SET display_name=COALESCE(?,display_name), kind=COALESCE(?,kind), \
             charter=COALESCE(?,charter), metadata=COALESCE(?,metadata), \
             webhook_url=COALESCE(?,webhook_url), status='online', last_seen=? WHERE id=?",
        )
        .bind(display_name)
        .bind(kind)
        .bind(charter)
        .bind(merged_meta.as_deref())
        .bind(webhook_url)
        .bind(&ts)
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO agents(id, display_name, kind, charter, metadata, status, webhook_url, created_at, last_seen) \
             VALUES(?,?,?,?,COALESCE(?,'{}'),'online',?,?,?)",
        )
        .bind(agent_id)
        .bind(display_name)
        .bind(kind)
        .bind(charter)
        .bind(merged_meta.as_deref())
        .bind(webhook_url)
        .bind(&ts)
        .bind(&ts)
        .execute(&mut *tx)
        .await?;
        // A newly-registered agent auto-joins every fleet-wide broadcast channel (auto_join=1),
        // so an announcement reaches it without a manual invite. Only on first registration —
        // a re-register (presence refresh) does not re-add a channel the agent has left.
        let auto_channels = sqlx::query("SELECT id FROM channels WHERE auto_join=1")
            .fetch_all(&mut *tx)
            .await?;
        for c in &auto_channels {
            let cid: i64 = c.try_get("id")?;
            join_channel(&mut tx, cid, agent_id).await?;
        }
    }
    let row = sqlx::query("SELECT * FROM agents WHERE id=?")
        .bind(agent_id)
        .fetch_one(&mut *tx)
        .await?;
    let out = agent_json(&row);
    tx.commit().await?;
    Ok(out)
}

/// Mutate an existing agent's fields — any subset of display_name, kind, charter, status,
/// status_message, webhook_url, metadata (MERGED). Unlike register_agent this does NOT
/// create-or-touch-presence: it fails if the agent doesn't exist and does not force status
/// online. This is the registry-write path (the board agent list as the fleet registry).
#[allow(clippy::too_many_arguments)]
pub async fn update_agent(
    pool: &Pool,
    agent_id: &str,
    display_name: Option<&str>,
    kind: Option<&str>,
    charter: Option<&str>,
    status: Option<&str>,
    status_message: Option<&str>,
    webhook_url: Option<&str>,
    metadata: Option<Value>,
    clear: Option<&[String]>,
) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let old = sqlx::query("SELECT metadata FROM agents WHERE id=?")
        .bind(agent_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(old) = old else {
        anyhow::bail!("no agent {agent_id}");
    };

    let mut set_clauses: Vec<String> = Vec::new();
    let fields: [(&str, Option<&str>); 6] = [
        ("display_name", display_name),
        ("kind", kind),
        ("charter", charter),
        ("status", status),
        ("status_message", status_message),
        ("webhook_url", webhook_url),
    ];
    for (col, val) in fields.iter() {
        if val.is_some() {
            set_clauses.push(format!("{col}=?"));
        }
    }
    // Explicit clear affordance (task 489): a merge-PATCH treats a null/omitted field as "leave
    // unchanged", so there was no way to reset a nullable agent field (e.g. a stale webhook_url)
    // to NULL through the tool — callers thrashed on malformed JSON and empty-string workarounds.
    // `clear` names columns to set to NULL. An explicit value for the same field wins (its `field=?`
    // is already queued, so we skip the NULL to avoid a duplicate SET). status is not clearable
    // (presence is a keyword, not nullable); metadata is merged, not cleared, via update.
    const CLEARABLE: [&str; 5] = [
        "display_name",
        "kind",
        "charter",
        "status_message",
        "webhook_url",
    ];
    if let Some(clear) = clear {
        for name in clear {
            if !CLEARABLE.contains(&name.as_str()) {
                anyhow::bail!(
                    "cannot clear field '{name}'; clearable fields: {}",
                    CLEARABLE.join(", ")
                );
            }
            if !set_clauses.iter().any(|c| c == &format!("{name}=?")) {
                set_clauses.push(format!("{name}=NULL"));
            }
        }
    }
    let merged_meta: Option<String> = if let Some(meta) = metadata {
        let old_meta: Option<String> = old.try_get("metadata")?;
        set_clauses.push("metadata=?".to_string());
        Some(merge_metadata(old_meta.as_deref(), meta))
    } else {
        None
    };

    if !set_clauses.is_empty() {
        let sql = format!("UPDATE agents SET {} WHERE id=?", set_clauses.join(", "));
        let mut q = sqlx::query(&sql);
        for (_, val) in fields.iter() {
            if let Some(v) = val {
                q = q.bind(*v);
            }
        }
        if let Some(ref m) = merged_meta {
            q = q.bind(m);
        }
        q = q.bind(agent_id);
        q.execute(&mut *tx).await?;
    }

    let row = sqlx::query("SELECT * FROM agents WHERE id=?")
        .bind(agent_id)
        .fetch_one(&mut *tx)
        .await?;
    let out = agent_json(&row);
    tx.commit().await?;
    Ok(out)
}

/// The canonical roster presence states (task 496). `set_status` coerces any input to one of these
/// so the presence field stays a clean enum instead of accumulating free-form per-tick narrative.
pub const PRESENCE_STATES: &[&str] = &["online", "idle", "busy", "blocked", "away", "offline"];

/// Coerce a caller-supplied `status` to a canonical presence value, non-destructively (task 496).
/// The roster presence field is an enum, but agents were cramming per-tick narrative into it,
/// corrupting presence and leaving `status_message` stale. This maps the input (via its first token
/// and a small synonym set) to a canonical value; when the input carried narrative beyond the
/// presence word (or was unrecognized free-text) and no explicit `status_message` was given, the
/// original text is salvaged into `status_message` so nothing is lost. Never rejects.
///
/// NOT-RUNNING coupling: done/cancelled/stopped/etc. coerce to `offline`, because the watchdog's
/// `agent_expected_running` rule keys on {offline, done, cancelled} = not-running and this enum has
/// no separate done/cancelled value (confirmed with v-fleet-tooling, task 496). If this enum ever
/// gains a distinct not-running value, `agent_expected_running`'s match set is the SINGLE place that
/// must be updated to keep the not-running semantics.
fn normalize_presence(status: &str, status_message: Option<&str>) -> (String, Option<String>) {
    let raw = status.trim();
    // The presence intent is the first whitespace/':'-delimited token (agents write e.g.
    // "idle: inbox drained" or "blocked on review").
    let token = raw
        .split([':', ' '])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let canonical = match token.as_str() {
        "online" | "active" | "available" | "up" | "ready" | "live" | "healthy" => Some("online"),
        "idle" | "free" => Some("idle"),
        "busy" | "working" | "in_progress" | "running" | "processing" => Some("busy"),
        "blocked" | "waiting" | "stuck" => Some("blocked"),
        "away" | "afk" => Some("away"),
        "offline" | "done" | "cancelled" | "canceled" | "stopped" | "complete" | "completed"
        | "finished" | "shutdown" | "dead" | "exited" | "terminated" | "gone" | "down" => {
            Some("offline")
        }
        _ => None,
    };
    let recognized = canonical.is_some();
    // Unrecognized free-text is still a live agent that called in: treat presence as online.
    let presence = canonical.unwrap_or("online").to_string();
    // Salvage when the input carried narrative beyond the presence word, or was unrecognized text --
    // but only into an empty status_message (an explicit status_message always wins).
    let has_narrative = raw.contains(' ') || raw.contains(':');
    let salvage = !recognized || has_narrative;
    let effective_message = match status_message {
        Some(m) if !m.trim().is_empty() => Some(m.to_string()),
        _ if salvage && !raw.is_empty() => Some(raw.to_string()),
        _ => status_message.map(|s| s.to_string()),
    };
    debug_assert!(
        PRESENCE_STATES.contains(&presence.as_str()),
        "normalize_presence produced a non-canonical value: {presence}"
    );
    (presence, effective_message)
}

pub async fn set_status(
    pool: &Pool,
    agent_id: &str,
    status: &str,
    status_message: Option<&str>,
) -> anyhow::Result<Value> {
    // Coerce to a canonical presence + salvage any narrative into status_message (task 496).
    let (status_norm, message_norm) = normalize_presence(status, status_message);
    let status = status_norm.as_str();
    let status_message = message_norm.as_deref();
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    // Going offline honors any pending spin-down request, so clear it (the request's lifecycle end).
    // Any other status leaves a pending request in place — it stands, visibly, until honored.
    let clear_stand_down = status == "offline";
    let n = sqlx::query(
        "UPDATE agents SET status=?, status_message=COALESCE(?,status_message), last_seen=?, \
         stand_down_requested_at=CASE WHEN ? THEN NULL ELSE stand_down_requested_at END, \
         stand_down_requested_by=CASE WHEN ? THEN NULL ELSE stand_down_requested_by END, \
         stand_down_reason=CASE WHEN ? THEN NULL ELSE stand_down_reason END WHERE id=?",
    )
    .bind(status)
    .bind(status_message)
    .bind(&ts)
    .bind(clear_stand_down)
    .bind(clear_stand_down)
    .bind(clear_stand_down)
    .bind(agent_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        // auto-register so presence is friction-free
        sqlx::query(
            "INSERT INTO agents(id, status, status_message, created_at, last_seen) VALUES(?,?,?,?,?)",
        )
        .bind(agent_id)
        .bind(status)
        .bind(status_message)
        .bind(&ts)
        .bind(&ts)
        .execute(&mut *tx)
        .await?;
    }
    let row = sqlx::query("SELECT * FROM agents WHERE id=?")
        .bind(agent_id)
        .fetch_one(&mut *tx)
        .await?;
    let out = agent_json(&row);
    tx.commit().await?;
    Ok(out)
}

/// Roster fields that a CROSS-REPO fleet consumer depends on, which therefore MUST survive any
/// future compaction of the default `list_agents` projection (task 500). This is a consumer-field
/// CONTRACT, not just the current field list: the `fleet_consumed_roster_fields_contract` test
/// asserts the default (non-verbose) projection keeps every field named here, so a projection tweak
/// cannot silently drop one. History: the task 418 compaction dropped `metadata`, and the fleet
/// watchdog (separate repo) filters observe-candidates on `metadata.native`; with the list omitting
/// metadata every agent read native=false and the WHOLE self-improve observer cadence went dark
/// (task 477 fixed the acute drop). Add a field here only when a fleet consumer genuinely depends on
/// it in the compact roster -- it is a standing promise the gate enforces.
pub const FLEET_CONSUMED_ROSTER_FIELDS: &[&str] = &["metadata"];

/// The default compact `list_agents` projection (task 418): the human-facing id/name/status plus the
/// load-bearing [`FLEET_CONSUMED_ROSTER_FIELDS`], minus the heavy `charter` (the one field whose size
/// overflowed the caller token cap, so the one dropped). `get_agent` / verbose still return the full
/// object. Keep every [`FLEET_CONSUMED_ROSTER_FIELDS`] entry in this list or the contract test reds.
const COMPACT_ROSTER_FIELDS: &[&str] = &["id", "display_name", "status", "metadata"];

/// List agents as a lightweight ROSTER by default (task #418): each entry is a compact
/// {id, display_name, status, metadata} — name + id + tiny presence + the small metadata bag — so a
/// scoped read stays well under a caller's token cap (the full roster with every agent's charter
/// overflowed it; the charter is the only heavy field, so it is the one dropped). Metadata is kept
/// because callers filter on it (e.g. the fleet watchdog on metadata.native, task 477). Fetch a
/// single agent's full record (incl charter) with get_agent, or pass `verbose` for the full
/// objects. Optional filters: `status` (exact), `q` (substring over id + display_name),
/// and `meta_key`/`meta_value` (equality on a scalar metadata field, e.g. area/host — for routing
/// a task to an owning vertical). Always bounded by `limit` (default 200, max 1000) + `offset`.
#[allow(clippy::too_many_arguments)]
pub async fn list_agents(
    pool: &Pool,
    status: Option<&str>,
    q: Option<&str>,
    meta_key: Option<&str>,
    meta_value: Option<&str>,
    verbose: bool,
    limit: Option<i64>,
    offset: Option<i64>,
) -> anyhow::Result<Value> {
    // Consumer-field contract (task 500): the compact projection must carry every fleet-consumed
    // field. Enforced at compile-coverage by fleet_consumed_roster_fields_contract; this debug check
    // is the belt-and-suspenders runtime guard (and keeps the contract const referenced in the bin).
    debug_assert!(
        FLEET_CONSUMED_ROSTER_FIELDS
            .iter()
            .all(|f| COMPACT_ROSTER_FIELDS.contains(f)),
        "a FLEET_CONSUMED_ROSTER_FIELDS entry is missing from COMPACT_ROSTER_FIELDS"
    );
    let mut sql = String::from("SELECT * FROM agents");
    let mut conds: Vec<&str> = Vec::new();
    if status.is_some() {
        conds.push("status=?");
    }
    if q.is_some() {
        conds.push("(id LIKE ? OR display_name LIKE ?)");
    }
    let use_meta = meta_key.is_some() && meta_value.is_some();
    if use_meta {
        conds.push("json_extract(metadata, '$.' || ?) = ?");
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY last_seen DESC LIMIT ? OFFSET ?");
    let lim = limit.unwrap_or(200).clamp(1, 1000);
    let off = offset.unwrap_or(0).max(0);

    let mut query = sqlx::query(&sql);
    if let Some(s) = status {
        query = query.bind(s.to_string());
    }
    if let Some(qq) = q {
        let like = format!("%{qq}%");
        query = query.bind(like.clone()).bind(like);
    }
    if use_meta {
        query = query
            .bind(meta_key.unwrap().to_string())
            .bind(meta_value.unwrap().to_string());
    }
    query = query.bind(lim).bind(off);
    let rows = query.fetch_all(pool).await?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            if verbose {
                return agent_json(r);
            }
            // Compact roster projection: id + display_name + status + metadata. `metadata` is a
            // small structured bag consumers filter on (e.g. the fleet watchdog selects agents by
            // metadata.native, task 477); the large `charter` is what the compaction (#418) drops,
            // so this stays well under the token cap while keeping the roster useful for filtering.
            // get_agent (or verbose=true) still returns everything.
            let full = agent_json(r);
            let mut m = Map::new();
            if let Value::Object(o) = &full {
                for k in COMPACT_ROSTER_FIELDS {
                    if let Some(v) = o.get(*k) {
                        m.insert((*k).to_string(), v.clone());
                    }
                }
            }
            Value::Object(m)
        })
        .collect();
    Ok(Value::Array(items))
}

pub async fn get_agent(pool: &Pool, agent_id: &str) -> anyhow::Result<Value> {
    let row = sqlx::query("SELECT * FROM agents WHERE id=?")
        .bind(agent_id)
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => Ok(agent_json(&r)),
        None => anyhow::bail!("no agent {agent_id}"),
    }
}

/// File a graceful spin-down request for an agent: record who asked + why + when, and drop an
/// `agent.stand_down_requested` event into the target's inbox (plus a live-tunnel wake) so the
/// agent observes it on its next loop tick and winds down on its own terms (status->offline, end
/// its loop). This is a SIGNAL, never an action: it does NOT change the agent's status and NEVER
/// kills or reaps a live agent mid-work (the live-watchdog ban). The request stays visible until
/// the agent honors it by going offline (set_status clears it then). Re-requesting refreshes the
/// stamp/reason. Board-native equivalent of the concierge/operator graceful stand-down path.
pub async fn request_stand_down(
    pool: &Pool,
    agent_id: &str,
    requested_by: Option<&str>,
    reason: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let n = sqlx::query(
        "UPDATE agents SET stand_down_requested_at=?, stand_down_requested_by=?, stand_down_reason=? WHERE id=?",
    )
    .bind(&ts)
    .bind(requested_by)
    .bind(reason)
    .bind(agent_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        anyhow::bail!("no agent {agent_id}");
    }
    // Deliver to the target so it observes the request in its loop (check_notifications). The
    // firehose union in emit also lets a coordinator/UI-watcher see stand-down requests.
    let mut recips = BTreeSet::new();
    recips.insert(agent_id.to_string());
    emit(
        &mut tx,
        &mut hooks,
        "agent.stand_down_requested",
        requested_by,
        None,
        None,
        None,
        None,
        json!({ "agent_id": agent_id, "requested_by": requested_by, "reason": reason }),
        Recipients::Explicit(recips),
    )
    .await?;
    let row = sqlx::query("SELECT * FROM agents WHERE id=?")
        .bind(agent_id)
        .fetch_one(&mut *tx)
        .await?;
    let out = agent_json(&row);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

// --- Projects ---

pub async fn create_project(
    pool: &Pool,
    name: &str,
    description: Option<&str>,
    created_by: Option<&str>,
    metadata: Option<Value>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();

    // Get-or-create: project names are unique case-insensitively. If one already exists
    // (any casing), return it unchanged rather than creating a duplicate — this keeps the
    // operation idempotent for agents and prevents the mixed-case sprawl we clean up in
    // merge_duplicate_projects().
    if let Some(row) = sqlx::query("SELECT id FROM projects WHERE name = ? COLLATE NOCASE")
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?
    {
        let existing_id: i64 = row.try_get("id")?;
        let out = project_json(&mut tx, existing_id)
            .await?
            .unwrap_or(Value::Null);
        tx.commit().await?;
        return Ok(out);
    }

    let meta_str = metadata.unwrap_or_else(|| json!({})).to_string();
    let pid: i64 = sqlx::query(
        "INSERT INTO projects(name, description, metadata, created_by, created_at, updated_at) \
         VALUES(?,?,?,?,?,?) RETURNING id",
    )
    .bind(name)
    .bind(description)
    .bind(&meta_str)
    .bind(created_by)
    .bind(&ts)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    emit(
        &mut tx,
        &mut hooks,
        "project.created",
        created_by,
        None,
        Some(pid),
        None,
        None,
        json!({ "name": name }),
        Recipients::Explicit(BTreeSet::new()),
    )
    .await?;
    let out = project_json(&mut tx, pid).await?.unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Fetch one project row as JSON with its `metadata` string parsed into an object (mirrors
/// how get_task surfaces task metadata). Returns None if the project doesn't exist.
async fn project_json(
    tx: &mut Transaction<'_, Sqlite>,
    project_id: i64,
) -> anyhow::Result<Option<Value>> {
    let Some(mut d) = fetch_one_json(tx, "SELECT * FROM projects WHERE id=?", project_id).await?
    else {
        return Ok(None);
    };
    if let Value::Object(ref mut m) = d {
        let meta: Value = m
            .get("metadata")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));
        m.insert("metadata".into(), meta);
    }
    Ok(Some(d))
}

/// Update a project's mutable fields — any subset of name, description, status, metadata.
/// Name changes are case-insensitively unique (reusing the get-or-create invariant): renaming
/// onto an existing project's name (other than itself) is rejected. `metadata` is MERGED into
/// the existing props, not replaced (same semantics as tasks). Emits `project.updated`.
pub async fn update_project(
    pool: &Pool,
    project_id: i64,
    name: Option<&str>,
    description: Option<&str>,
    status: Option<&str>,
    metadata: Option<Value>,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();

    let old = sqlx::query("SELECT * FROM projects WHERE id=?")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(old) = old else {
        anyhow::bail!("no project {project_id}");
    };
    let old_name: String = old.try_get("name")?;
    let old_status: Option<String> = old.try_get("status")?;
    let old_metadata: Option<String> = old.try_get("metadata")?;

    // A rename must not collide with another project's (case-folded) name — that would
    // reintroduce the duplicate sprawl create_project's get-or-create prevents.
    if let Some(new_name) = name {
        if !new_name.eq_ignore_ascii_case(&old_name) {
            let clash =
                sqlx::query("SELECT 1 FROM projects WHERE name = ? COLLATE NOCASE AND id != ?")
                    .bind(new_name)
                    .bind(project_id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .is_some();
            if clash {
                anyhow::bail!("give a unique name: a project named '{new_name}' already exists");
            }
        }
    }

    let mut set_clauses: Vec<String> = Vec::new();
    let fields: [(&str, Option<&str>); 3] = [
        ("name", name),
        ("description", description),
        ("status", status),
    ];
    for (col, val) in fields.iter() {
        if val.is_some() {
            set_clauses.push(format!("{col}=?"));
        }
    }
    let merged_meta: Option<String> = if let Some(meta) = metadata {
        let mut base: Map<String, Value> =
            serde_json::from_str(old_metadata.as_deref().unwrap_or("{}")).unwrap_or_default();
        if let Value::Object(m) = meta {
            for (k, v) in m {
                base.insert(k, v);
            }
        }
        set_clauses.push("metadata=?".to_string());
        Some(Value::Object(base).to_string())
    } else {
        None
    };

    if set_clauses.is_empty() {
        // Nothing to change — return the current project unchanged.
        let out = project_json(&mut tx, project_id)
            .await?
            .unwrap_or(Value::Null);
        tx.commit().await?;
        return Ok(out);
    }

    let sql = format!(
        "UPDATE projects SET {}, updated_at=? WHERE id=?",
        set_clauses.join(", ")
    );
    let mut q = sqlx::query(&sql);
    for (_, val) in fields.iter() {
        if let Some(v) = val {
            q = q.bind(*v);
        }
    }
    if let Some(ref m) = merged_meta {
        q = q.bind(m);
    }
    q = q.bind(&ts).bind(project_id);
    q.execute(&mut *tx).await?;

    // A status flip to/from 'archived' is the notable case; surface it in the event data so
    // the feed can read "archived"/"unarchived" without diffing.
    let status_changed = status.is_some() && status != old_status.as_deref();
    emit(
        &mut tx,
        &mut hooks,
        "project.updated",
        actor,
        None,
        Some(project_id),
        None,
        None,
        json!({
            "name": name.unwrap_or(&old_name),
            "status": status,
            "status_changed": status_changed,
        }),
        Recipients::FromProject(project_id),
    )
    .await?;
    let out = project_json(&mut tx, project_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

pub async fn list_projects(pool: &Pool, status: Option<&str>) -> anyhow::Result<Value> {
    let rows = match status {
        Some(s) => {
            sqlx::query("SELECT * FROM projects WHERE status=? ORDER BY id")
                .bind(s)
                .fetch_all(pool)
                .await?
        }
        None => {
            sqlx::query("SELECT * FROM projects ORDER BY id")
                .fetch_all(pool)
                .await?
        }
    };
    let mut out = Vec::new();
    for r in &rows {
        let mut d = row_to_json(r);
        let pid: i64 = r.try_get("id")?;
        let counts =
            sqlx::query("SELECT status, COUNT(*) n FROM tasks WHERE project_id=? GROUP BY status")
                .bind(pid)
                .fetch_all(pool)
                .await?;
        let mut cmap = Map::new();
        for c in &counts {
            let s: String = c.try_get("status")?;
            let n: i64 = c.try_get("n")?;
            cmap.insert(s, json!(n));
        }
        if let Value::Object(ref mut m) = d {
            insert_ref(m, "project"); // typed canonical id (task 504)
                                      // metadata: parse JSON string -> object (mirrors get_project/get_task).
            let meta: Value = m
                .get("metadata")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| json!({}));
            m.insert("metadata".into(), meta);
            m.insert("task_counts".into(), Value::Object(cmap));
        }
        out.push(d);
    }
    Ok(Value::Array(out))
}

pub async fn get_project(pool: &Pool, project_id: i64) -> anyhow::Result<Value> {
    let p = sqlx::query("SELECT * FROM projects WHERE id=?")
        .bind(project_id)
        .fetch_optional(pool)
        .await?;
    let Some(p) = p else { return Ok(Value::Null) };
    let mut d = row_to_json(&p);
    let tasks = sqlx::query(
        "SELECT id, title, status, assignee, priority FROM tasks WHERE project_id=? ORDER BY id",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    if let Value::Object(ref mut m) = d {
        insert_ref(m, "project"); // typed canonical id (task 504)
                                  // metadata: parse JSON string -> object (mirrors get_task).
        let meta: Value = m
            .get("metadata")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));
        m.insert("metadata".into(), meta);
        m.insert(
            "tasks".into(),
            Value::Array(tasks.iter().map(|r| row_to_json_ref(r, "task")).collect()),
        );
    }
    Ok(d)
}

// --- Identity aliases (task 532) ---

/// List the identity aliases (alias -> canonical identity), ordered by alias. A small config table
/// (seeded with operator -> cameron) that consumers/UI use to resolve or display a floating name
/// like "operator" as the canonical identity across assignee, blocked_on, and @-mentions.
pub async fn list_identity_aliases(pool: &Pool) -> anyhow::Result<Value> {
    let rows = sqlx::query(
        "SELECT alias, canonical, created_by, created_at FROM identity_aliases ORDER BY alias",
    )
    .fetch_all(pool)
    .await?;
    Ok(Value::Array(rows.iter().map(row_to_json).collect()))
}

/// Upsert an identity alias (task 532): map `alias` (lowercased/trimmed key) to `canonical`. An
/// existing alias is repointed. Rejects empty input and a self-alias (alias == canonical).
pub async fn set_identity_alias(
    pool: &Pool,
    alias: &str,
    canonical: &str,
    created_by: Option<&str>,
) -> anyhow::Result<Value> {
    let alias = alias.trim().to_ascii_lowercase();
    let canonical = canonical.trim();
    if alias.is_empty() || canonical.is_empty() {
        anyhow::bail!("give a non-empty alias and canonical identity");
    }
    if alias == canonical.to_ascii_lowercase() {
        anyhow::bail!("an alias cannot point at itself");
    }
    sqlx::query(
        "INSERT INTO identity_aliases(alias, canonical, created_by, created_at) VALUES(?,?,?,?) \
         ON CONFLICT(alias) DO UPDATE SET canonical=excluded.canonical",
    )
    .bind(&alias)
    .bind(canonical)
    .bind(created_by)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(json!({ "alias": alias, "canonical": canonical }))
}

// --- People / teams: the multi-operator identity model (doc_26 / task 542, Phase 1) ---
// People are first-class human identities in their own registry (separate from `agents`); teams are
// addressable groups whose members are people OR other teams (recursive). Ids are stable string
// handles. Membership is kept acyclic at write time and the read-time expansion is cycle-guarded.
// ENFORCEMENT is deferred: this records identities + membership and resolves them at read time;
// nothing here gates access or requires login.

/// Create (or idempotently upsert) a person. `id` is a stable string handle (e.g. "cameron").
pub async fn create_person(
    pool: &Pool,
    id: &str,
    display_name: Option<&str>,
    created_by: Option<&str>,
    metadata: Option<Value>,
) -> anyhow::Result<Value> {
    let id = id.trim();
    if id.is_empty() {
        anyhow::bail!("give a non-empty person id");
    }
    let meta = metadata.unwrap_or_else(|| json!({})).to_string();
    sqlx::query(
        "INSERT INTO people(id, display_name, created_by, created_at, metadata) VALUES(?,?,?,?,?) \
         ON CONFLICT(id) DO UPDATE SET \
           display_name=COALESCE(excluded.display_name, people.display_name), \
           metadata=excluded.metadata",
    )
    .bind(id)
    .bind(display_name)
    .bind(created_by)
    .bind(now_iso())
    .bind(meta)
    .execute(pool)
    .await?;
    get_person(pool, id).await
}

pub async fn get_person(pool: &Pool, id: &str) -> anyhow::Result<Value> {
    let Some(row) = sqlx::query("SELECT * FROM people WHERE id=?")
        .bind(id)
        .fetch_optional(pool)
        .await?
    else {
        anyhow::bail!("no person {id}");
    };
    Ok(row_to_json(&row))
}

pub async fn list_people(pool: &Pool) -> anyhow::Result<Value> {
    let rows = sqlx::query("SELECT * FROM people ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(Value::Array(rows.iter().map(row_to_json).collect()))
}

/// Create (or idempotently upsert) a team. `id` is a stable string handle (e.g. "operator").
pub async fn create_team(
    pool: &Pool,
    id: &str,
    display_name: Option<&str>,
    created_by: Option<&str>,
    metadata: Option<Value>,
) -> anyhow::Result<Value> {
    let id = id.trim();
    if id.is_empty() {
        anyhow::bail!("give a non-empty team id");
    }
    let meta = metadata.unwrap_or_else(|| json!({})).to_string();
    sqlx::query(
        "INSERT INTO teams(id, display_name, created_by, created_at, metadata) VALUES(?,?,?,?,?) \
         ON CONFLICT(id) DO UPDATE SET \
           display_name=COALESCE(excluded.display_name, teams.display_name), \
           metadata=excluded.metadata",
    )
    .bind(id)
    .bind(display_name)
    .bind(created_by)
    .bind(now_iso())
    .bind(meta)
    .execute(pool)
    .await?;
    get_team(pool, id).await
}

pub async fn list_teams(pool: &Pool) -> anyhow::Result<Value> {
    let rows = sqlx::query("SELECT * FROM teams ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(Value::Array(rows.iter().map(row_to_json).collect()))
}

/// A team plus its direct members and its fully-resolved person AND agent sets (nested teams
/// expanded). resolved_people and resolved_agents are kept separate (not a unified principals blob)
/// so an existing consumer of resolved_people is unaffected (task 542 Phase 1c, doc_26 v9).
pub async fn get_team(pool: &Pool, team_id: &str) -> anyhow::Result<Value> {
    let Some(row) = sqlx::query("SELECT * FROM teams WHERE id=?")
        .bind(team_id)
        .fetch_optional(pool)
        .await?
    else {
        anyhow::bail!("no team {team_id}");
    };
    let mut out = row_to_json(&row);
    let members: Vec<Value> = sqlx::query(
        "SELECT member_id, member_kind FROM team_members WHERE team_id=? ORDER BY member_kind, member_id",
    )
    .bind(team_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(row_to_json)
    .collect();
    let (people_set, agents_set) = resolve_team_principals(pool, team_id).await?;
    let people: Vec<Value> = people_set.into_iter().map(Value::String).collect();
    let agents: Vec<Value> = agents_set.into_iter().map(Value::String).collect();
    if let Value::Object(ref mut m) = out {
        m.insert("members".into(), Value::Array(members));
        m.insert("resolved_people".into(), Value::Array(people));
        m.insert("resolved_agents".into(), Value::Array(agents));
    }
    Ok(out)
}

/// Expand a team to the PRINCIPAL ids it contains, following nested teams: the PERSON ids and the
/// AGENT ids (team-scoped agents, task 542 Phase 1c). Cycle-guarded by a visited-set so a cyclic
/// membership graph still terminates (doc_26 appendix A1). Returns (people, agents).
async fn resolve_team_principals(
    pool: &Pool,
    team_id: &str,
) -> anyhow::Result<(BTreeSet<String>, BTreeSet<String>)> {
    let mut tx = pool.begin().await?;
    let out = resolve_team_principals_tx(&mut tx, team_id).await;
    tx.commit().await?;
    out
}

/// `resolve_team_principals` against an open transaction, so a write path that already holds the
/// (single) connection can expand a team WITHOUT grabbing a second pool connection -- grabbing one
/// while the write tx is open deadlocks the pool.
async fn resolve_team_principals_tx(
    tx: &mut Transaction<'_, Sqlite>,
    team_id: &str,
) -> anyhow::Result<(BTreeSet<String>, BTreeSet<String>)> {
    let mut people: BTreeSet<String> = BTreeSet::new();
    let mut agents: BTreeSet<String> = BTreeSet::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut stack = vec![team_id.to_string()];
    while let Some(tid) = stack.pop() {
        if !visited.insert(tid.clone()) {
            continue; // already expanded this team -> cycle guard
        }
        for r in sqlx::query("SELECT member_id, member_kind FROM team_members WHERE team_id=?")
            .bind(&tid)
            .fetch_all(&mut **tx)
            .await?
        {
            let mid: String = r.try_get("member_id")?;
            let kind: String = r.try_get("member_kind")?;
            match kind.as_str() {
                "team" => stack.push(mid),
                "agent" => {
                    agents.insert(mid);
                }
                _ => {
                    people.insert(mid); // "person"
                }
            }
        }
    }
    Ok((people, agents))
}

/// Whether team `from` can reach team `target` through nested-team membership (team-only BFS,
/// cycle-guarded). Used to reject a sub-team add that would create a membership cycle.
async fn team_reaches_team(pool: &Pool, from: &str, target: &str) -> anyhow::Result<bool> {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut stack = vec![from.to_string()];
    while let Some(tid) = stack.pop() {
        if tid == target {
            return Ok(true);
        }
        if !visited.insert(tid.clone()) {
            continue;
        }
        for r in
            sqlx::query("SELECT member_id FROM team_members WHERE team_id=? AND member_kind='team'")
                .bind(&tid)
                .fetch_all(pool)
                .await?
        {
            stack.push(r.try_get("member_id")?);
        }
    }
    Ok(false)
}

/// Add a person, team, or agent as a member of a team (idempotent). Validates the member exists in
/// its registry; for a sub-team, rejects self-membership and any add that would create a cycle (so
/// read-time expansion always terminates). Returns the updated team. Team-scoped agents (task 542
/// Phase 1c) are leaf members like people -- a team resolves to its people AND its agents.
pub async fn add_team_member(
    pool: &Pool,
    team_id: &str,
    member_id: &str,
    member_kind: &str,
    created_by: Option<&str>,
) -> anyhow::Result<Value> {
    let member_kind = member_kind.trim();
    if member_kind != "person" && member_kind != "team" && member_kind != "agent" {
        anyhow::bail!("member_kind must be \"person\", \"team\", or \"agent\"");
    }
    if sqlx::query("SELECT 1 FROM teams WHERE id=?")
        .bind(team_id)
        .fetch_optional(pool)
        .await?
        .is_none()
    {
        anyhow::bail!("no team {team_id}");
    }
    let member_table = match member_kind {
        "person" => "people",
        "team" => "teams",
        _ => "agents",
    };
    if sqlx::query(&format!("SELECT 1 FROM {member_table} WHERE id=?"))
        .bind(member_id)
        .fetch_optional(pool)
        .await?
        .is_none()
    {
        anyhow::bail!("no {member_kind} {member_id} to add as a member");
    }
    if member_kind == "team" {
        if member_id == team_id {
            anyhow::bail!("a team cannot be a member of itself");
        }
        if team_reaches_team(pool, member_id, team_id).await? {
            anyhow::bail!(
                "adding team \"{member_id}\" to team \"{team_id}\" would create a membership cycle"
            );
        }
    }
    sqlx::query(
        "INSERT INTO team_members(team_id, member_id, member_kind, created_by, created_at) \
         VALUES(?,?,?,?,?) ON CONFLICT(team_id, member_id, member_kind) DO NOTHING",
    )
    .bind(team_id)
    .bind(member_id)
    .bind(member_kind)
    .bind(created_by)
    .bind(now_iso())
    .execute(pool)
    .await?;
    get_team(pool, team_id).await
}

/// Remove a member from a team (idempotent). Returns the updated team.
pub async fn remove_team_member(
    pool: &Pool,
    team_id: &str,
    member_id: &str,
    member_kind: &str,
) -> anyhow::Result<Value> {
    sqlx::query("DELETE FROM team_members WHERE team_id=? AND member_id=? AND member_kind=?")
        .bind(team_id)
        .bind(member_id)
        .bind(member_kind)
        .execute(pool)
        .await?;
    get_team(pool, team_id).await
}

/// Delete a person and cascade: drop any team memberships where this person is a member.
pub async fn delete_person(pool: &Pool, id: &str) -> anyhow::Result<Value> {
    let res = sqlx::query("DELETE FROM people WHERE id=?")
        .bind(id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        anyhow::bail!("no person {id}");
    }
    sqlx::query("DELETE FROM team_members WHERE member_id=? AND member_kind='person'")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(json!({ "deleted": id }))
}

/// Delete a team and cascade: drop its own memberships (its members) and any memberships where it
/// is a sub-team of another team.
pub async fn delete_team(pool: &Pool, id: &str) -> anyhow::Result<Value> {
    let res = sqlx::query("DELETE FROM teams WHERE id=?")
        .bind(id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        anyhow::bail!("no team {id}");
    }
    sqlx::query("DELETE FROM team_members WHERE team_id=? OR (member_id=? AND member_kind='team')")
        .bind(id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(json!({ "deleted": id }))
}

/// Resolve an addressable principal id to the set of principal ids who can act on it (task 542): a
/// team expands to its people AND its agents (cycle-guarded); anything else (a person id, or an
/// agent id) resolves to itself. The read-time primitive for team-aware addressing + visibility in
/// later phases.
// Exercised by tests now; the bin consumer lands in a later phase (team addressing / visibility).
#[allow(dead_code)]
pub async fn resolve_principal_ids(pool: &Pool, id: &str) -> anyhow::Result<BTreeSet<String>> {
    if sqlx::query("SELECT 1 FROM teams WHERE id=?")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .is_some()
    {
        let (mut principals, agents) = resolve_team_principals(pool, id).await?;
        principals.extend(agents);
        Ok(principals)
    } else {
        Ok(BTreeSet::from([id.to_string()]))
    }
}

// --- Tasks ---

/// An external-system reference for idempotent ingest (task 270). A bridge adapter passes this
/// when creating a task from an external item (or commenting from an external comment); the board
/// dedups on `(source, external_id)` so the create/comment is exactly-once even if the adapter
/// retries — collapsing the old create-then-link two-call race. `source` + `external_id` are the
/// dedup key; `external_parent_id` records the external parent (e.g. the issue an ingested comment
/// belongs to).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct ExternalRef {
    /// External system, e.g. "github" or "slack".
    pub source: String,
    /// The external system's stable id for this item (issue key, comment id, ...). The dedup key.
    pub external_id: String,
    /// Optional external parent id (e.g. the issue an ingested comment belongs to).
    #[serde(default)]
    pub external_parent_id: Option<String>,
}

/// If `assignee` names someone the board does not recognize as a principal, return a human-facing
/// warning (task 340). The assignment is still ACCEPTED (an agent may be assigned before its first
/// register, and we never reject) -- the warning just surfaces that a non-recognized owner will not
/// be notified or auto-pick-up, so an orchestrator catches a typo'd / repo-name / dead owner like
/// "dotfiles" (which silently dead-lettered a task) at assign time. A principal is a registered
/// agent, or a known identity alias / its canonical target (so humans like "cameron" and aliases
/// like "operator" do not false-warn). When teams land (task 542) this check extends to team names.
/// The empty-string unassign sentinel and a `None` (unchanged) assignee never warn.
async fn assignee_registration_warning(
    tx: &mut Transaction<'_, Sqlite>,
    assignee: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let Some(a) = assignee else { return Ok(None) };
    if a.is_empty() {
        return Ok(None); // unassign sentinel
    }
    let known = sqlx::query(
        "SELECT 1 FROM agents WHERE id=? \
         UNION SELECT 1 FROM identity_aliases WHERE alias=? OR canonical=? LIMIT 1",
    )
    .bind(a)
    .bind(a)
    .bind(a)
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    if known {
        Ok(None)
    } else {
        Ok(Some(format!(
            "assignee \"{a}\" is not a registered agent or known identity; the task is assigned but \
             this owner will not be notified or auto-pick-up until it registers. If \"{a}\" is a \
             repo/role name or a typo, reassign to a registered agent."
        )))
    }
}

/// Resolve the project a new task belongs to from an optional explicit `project_id` and an optional
/// `parent_id` (task 708). A child always lives in its parent's project, so `project_id` is
/// redundant when `parent_id` is given: omit it and the parent's project is inherited; give both and
/// `create_task` enforces they agree; give neither and it is an error. This lets the natural
/// epic-decomposition call (`parent_id` only, no `project_id`) succeed instead of bouncing a whole
/// batch on a missing field.
pub async fn resolve_create_project(
    pool: &Pool,
    project_id: Option<i64>,
    parent_id: Option<i64>,
) -> anyhow::Result<i64> {
    match (project_id, parent_id) {
        (Some(p), _) => Ok(p),
        (None, Some(pid)) => {
            let parent_proj: Option<i64> = sqlx::query("SELECT project_id FROM tasks WHERE id=?")
                .bind(pid)
                .fetch_optional(pool)
                .await?
                .map(|r| r.try_get("project_id"))
                .transpose()?;
            parent_proj.ok_or_else(|| anyhow::anyhow!("no parent task {pid}"))
        }
        (None, None) => {
            anyhow::bail!("give a `project_id` (or a `parent_id` to inherit the parent's project)")
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn create_task(
    pool: &Pool,
    project_id: i64,
    title: &str,
    description: Option<&str>,
    assignee: Option<&str>,
    priority: Option<&str>,
    created_by: Option<&str>,
    metadata: Option<Value>,
    parent_id: Option<i64>,
    external_link: Option<ExternalRef>,
) -> anyhow::Result<Value> {
    // Reject an ambiguous bare "#N" in the submitted title/description (task #517 hard-fail).
    check_bare_refs(title)?;
    if let Some(d) = description {
        check_bare_refs(d)?;
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    if sqlx::query("SELECT 1 FROM projects WHERE id=?")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        anyhow::bail!("no project {project_id}");
    }
    // Idempotent ingest (task 270): if an external_link is given and a task is ALREADY linked on
    // (source, external_id), return that existing task with `created:false` — no duplicate. The
    // SELECT-then-INSERT is atomic under one tx (the pool serializes writers), so a retrying
    // adapter can't race two tasks in. `created` is injected into the returned task object.
    if let Some(ext) = &external_link {
        if let Some(row) = sqlx::query(
            "SELECT board_id FROM external_links WHERE source=? AND external_id=? AND board_kind='task'",
        )
        .bind(&ext.source)
        .bind(&ext.external_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            let existing: i64 = row.try_get("board_id")?;
            let mut out = fetch_one_json(&mut tx, "SELECT * FROM tasks WHERE id=?", existing)
                .await?
                .unwrap_or(Value::Null);
            if let Value::Object(ref mut m) = out {
                m.insert("created".into(), json!(false));
            }
            tx.commit().await?;
            return Ok(out);
        }
    }
    // A parent must exist and live in the SAME project (cross-project nesting is disallowed).
    if let Some(pid) = parent_id {
        let parent_proj: Option<i64> = sqlx::query("SELECT project_id FROM tasks WHERE id=?")
            .bind(pid)
            .fetch_optional(&mut *tx)
            .await?
            .map(|r| r.try_get("project_id"))
            .transpose()?;
        match parent_proj {
            None => anyhow::bail!("no parent task {pid}"),
            Some(pp) if pp != project_id => {
                anyhow::bail!("parent task {pid} is in a different project")
            }
            _ => {}
        }
    }
    let meta_str = metadata.unwrap_or_else(|| json!({})).to_string();
    let tid: i64 = sqlx::query(
        "INSERT INTO tasks(project_id, title, description, assignee, priority, parent_id, created_by, \
         metadata, status, created_at, updated_at) VALUES(?,?,?,?,?,?,?,?, 'todo', ?, ?) RETURNING id",
    )
    .bind(project_id)
    .bind(title)
    .bind(description)
    .bind(assignee)
    .bind(priority)
    .bind(parent_id)
    .bind(created_by)
    .bind(&meta_str)
    .bind(&ts)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    auto_subscribe(&mut tx, created_by, tid).await?;
    auto_subscribe(&mut tx, assignee, tid).await?;
    emit(
        &mut tx,
        &mut hooks,
        "task.created",
        created_by,
        Some(tid),
        Some(project_id),
        None,
        None,
        json!({ "title": title, "assignee": assignee }),
        Recipients::FromTask,
    )
    .await?;
    // Record the external dedup link atomically with the create (task 270). Plain INSERT: within
    // this tx a matching row was already ruled out above, and the pool serializes writers.
    if let Some(ext) = &external_link {
        sqlx::query(
            "INSERT INTO external_links(source, external_id, external_parent_id, board_kind, board_id, metadata, created_at, updated_at) \
             VALUES(?,?,?,'task',?,'{}',?,?)",
        )
        .bind(&ext.source)
        .bind(&ext.external_id)
        .bind(&ext.external_parent_id)
        .bind(tid)
        .bind(&ts)
        .bind(&ts)
        .execute(&mut *tx)
        .await?;
    }
    let mut out = fetch_one_json(&mut tx, "SELECT * FROM tasks WHERE id=?", tid)
        .await?
        .unwrap_or(Value::Null);
    let assignee_warning = assignee_registration_warning(&mut tx, assignee).await?;
    if let Value::Object(ref mut m) = out {
        m.insert("created".into(), json!(true));
        if let Some(w) = assignee_warning {
            m.insert("assignee_warning".into(), json!(w));
        }
    }
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Extract a numeric task id from a `blocked_on.target` written in any of the forms agents reach
/// for (task 691): a bare `611`, `task_611`, `task:611`, or `#611`. Returns None if what remains
/// after stripping a recognized prefix is not a plain integer.
fn parse_task_target(s: &str) -> Option<i64> {
    let t = s.trim();
    let t = t.strip_prefix('#').unwrap_or(t);
    let t = t
        .strip_prefix("task_")
        .or_else(|| t.strip_prefix("task:"))
        .or_else(|| t.strip_prefix("task "))
        .unwrap_or(t);
    t.trim().parse::<i64>().ok()
}

#[allow(clippy::too_many_arguments)]
pub async fn update_task(
    pool: &Pool,
    task_id: i64,
    status: Option<&str>,
    assignee: Option<&str>,
    title: Option<&str>,
    description: Option<&str>,
    priority: Option<&str>,
    actor: Option<&str>,
    metadata: Option<Value>,
    parent_id: Option<i64>,
    blocked_on: Option<Value>,
) -> anyhow::Result<Value> {
    // Reject an ambiguous bare "#N" in a submitted title/description (task #517 hard-fail).
    if let Some(t) = title {
        check_bare_refs(t)?;
    }
    if let Some(d) = description {
        check_bare_refs(d)?;
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let old = sqlx::query("SELECT * FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(old) = old else {
        anyhow::bail!("no task {task_id}");
    };
    let old_status: String = old.try_get("status")?;
    let old_assignee: Option<String> = old.try_get("assignee")?;
    let old_title: Option<String> = old.try_get("title")?;
    let old_project_id: i64 = old.try_get("project_id")?;
    let old_parent_id: Option<i64> = old.try_get("parent_id")?;
    let old_metadata: Option<String> = old.try_get("metadata")?;
    let old_blocked_kind: Option<String> = old.try_get("blocked_on_kind").ok().flatten();
    let old_blocked_ref: Option<String> = old.try_get("blocked_on_ref").ok().flatten();

    // Build a dynamic UPDATE from the provided fields, preserving column order.
    let mut set_clauses: Vec<String> = Vec::new();
    let fields: [(&str, Option<&str>); 5] = [
        ("status", status),
        ("assignee", assignee),
        ("title", title),
        ("description", description),
        ("priority", priority),
    ];
    for (col, val) in fields.iter() {
        if val.is_some() {
            set_clauses.push(format!("{col}=?"));
        }
    }
    let merged_meta: Option<String> = if let Some(meta) = metadata {
        let mut base: Map<String, Value> =
            serde_json::from_str(old_metadata.as_deref().unwrap_or("{}")).unwrap_or_default();
        if let Value::Object(m) = meta {
            for (k, v) in m {
                base.insert(k, v);
            }
        }
        set_clauses.push("metadata=?".to_string());
        Some(Value::Object(base).to_string())
    } else {
        None
    };

    let has_fields = !set_clauses.is_empty();
    if has_fields {
        let sql = format!(
            "UPDATE tasks SET {}, updated_at=? WHERE id=?",
            set_clauses.join(", ")
        );
        let mut q = sqlx::query(&sql);
        for (col, val) in fields.iter() {
            if let Some(v) = val {
                // An empty-string assignee is the "unassign" sentinel: store NULL, not "".
                if *col == "assignee" && v.is_empty() {
                    q = q.bind(None::<&str>);
                } else {
                    q = q.bind(*v);
                }
            }
        }
        if let Some(ref m) = merged_meta {
            q = q.bind(m);
        }
        q = q.bind(&ts).bind(task_id);
        q.execute(&mut *tx).await?;
    }

    // Reparenting. parent_id semantics: None = leave unchanged; Some(0) = clear (make top-level);
    // Some(pid) = set a parent. A parent must exist, be in the SAME project, not be the task
    // itself, and not be a descendant (no cycles).
    let mut reparented = false;
    let mut reparent_to: Option<i64> = None;
    if let Some(new_parent) = parent_id {
        let target: Option<i64> = if new_parent == 0 {
            None
        } else {
            Some(new_parent)
        };
        if target != old_parent_id {
            if let Some(np) = target {
                if np == task_id {
                    anyhow::bail!("a task cannot be its own parent");
                }
                let pp: Option<i64> = sqlx::query("SELECT project_id FROM tasks WHERE id=?")
                    .bind(np)
                    .fetch_optional(&mut *tx)
                    .await?
                    .map(|r| r.try_get("project_id"))
                    .transpose()?;
                match pp {
                    None => anyhow::bail!("no parent task {np}"),
                    Some(proj) if proj != old_project_id => {
                        anyhow::bail!("parent task {np} is in a different project")
                    }
                    _ => {}
                }
                // Walk the prospective parent's ancestor chain; reaching task_id = a cycle.
                let mut cur = Some(np);
                while let Some(c) = cur {
                    if c == task_id {
                        anyhow::bail!("reparenting would create a cycle");
                    }
                    cur = sqlx::query("SELECT parent_id FROM tasks WHERE id=?")
                        .bind(c)
                        .fetch_optional(&mut *tx)
                        .await?
                        .and_then(|r| r.try_get::<Option<i64>, _>("parent_id").ok())
                        .flatten();
                }
            }
            sqlx::query("UPDATE tasks SET parent_id=?, updated_at=? WHERE id=?")
                .bind(target)
                .bind(&ts)
                .bind(task_id)
                .execute(&mut *tx)
                .await?;
            reparented = true;
            reparent_to = target;
        }
    }

    // blocked_on (operator seq-1361): a blocked task records what it is waiting on. The param is
    // None to leave it unchanged, Value::Null to clear it, or {kind, target, note} to set it. A
    // task that is (or becomes) blocked MUST carry a blocked_on; a task that is not blocked never
    // keeps one (it is auto-cleared when the task leaves the blocked state).
    let new_status = status.unwrap_or(old_status.as_str());
    enum BlockedChange {
        Leave,
        Clear,
        Set {
            kind: String,
            target: Option<String>,
            note: Option<String>,
        },
    }
    let mut change = match &blocked_on {
        None => BlockedChange::Leave,
        Some(Value::Null) => BlockedChange::Clear,
        Some(Value::Object(o)) => {
            let kind = o
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let target = o
                .get("target")
                .and_then(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .or_else(|| v.as_i64().map(|n| n.to_string()))
                })
                .filter(|s| !s.is_empty());
            let note = o
                .get("note")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            BlockedChange::Set { kind, target, note }
        }
        Some(_) => anyhow::bail!("give `blocked_on` as an object with a `kind`, or null to clear"),
    };
    if let BlockedChange::Set { kind, target, .. } = &mut change {
        match kind.as_str() {
            "task" => {
                let Some(t) = target.as_deref().and_then(parse_task_target) else {
                    anyhow::bail!(
                        "give a `blocked_on.target` task id when kind=task -- a numeric task id, e.g. blocked_on:\"task:611\" or {{\"kind\":\"task\",\"target\":611}} (a bare `611`, `task_611`, or `#611` are all accepted)"
                    );
                };
                if sqlx::query("SELECT 1 FROM tasks WHERE id=?").bind(t).fetch_optional(&mut *tx).await?.is_none() {
                    anyhow::bail!("no task {t}");
                }
                // Persist the canonical bare id, so the "what is blocked on task X" view
                // (list_tasks blocked_on_ref=<id>) matches regardless of how it was written
                // (task_611 / #611 / "611").
                *target = Some(t.to_string());
            }
            "agent" => {
                let Some(a) = target.as_deref() else {
                    anyhow::bail!(
                        "give a `blocked_on.target` agent id when kind=agent, e.g. blocked_on:\"agent:v-foo\" or {{\"kind\":\"agent\",\"target\":\"v-foo\"}}"
                    );
                };
                if sqlx::query("SELECT 1 FROM agents WHERE id=?").bind(a).fetch_optional(&mut *tx).await?.is_none() {
                    anyhow::bail!("no agent {a}");
                }
            }
            // A team (multi-operator model, task 542): the task is blocked waiting on a GROUP, and
            // every person the team resolves to is notified. "operator" is just the seeded team, so
            // this generalizes the operator-queue concept to any addressable team.
            "team" => {
                let Some(t) = target.as_deref() else {
                    anyhow::bail!(
                        "give a `blocked_on.target` team id when kind=team, e.g. blocked_on:\"team:operators\" or {{\"kind\":\"team\",\"target\":\"operators\"}}"
                    );
                };
                if sqlx::query("SELECT 1 FROM teams WHERE id=?").bind(t).fetch_optional(&mut *tx).await?.is_none() {
                    anyhow::bail!("no team {t}");
                }
            }
            "operator" => {}
            // External/infra dependency with no board owner (task_112): the free-text blocked_on.note
            // says what it waits on; no target. A DISTINCT kind from operator, so it stays OFF the
            // operator queue (blocked_on_kind='operator') -- a task waiting on external infra is
            // truthfully blocked without wrongly pinging the operator dashboard.
            "external" => {}
            other => anyhow::bail!(
                "give a valid `blocked_on.kind` (task, agent, team, operator, or external), not '{other}'"
            ),
        }
    }
    // A blocked task must end up with a blocked_on set.
    let will_have_blocked_on = match &change {
        BlockedChange::Set { .. } => true,
        BlockedChange::Clear => false,
        BlockedChange::Leave => old_blocked_kind.is_some(),
    };
    if new_status == "blocked" && !will_have_blocked_on {
        anyhow::bail!(
            "give a `blocked_on` recording what this blocked task is waiting on. kind is one of: task, agent, team, operator, external. Examples: blocked_on:\"operator\" | blocked_on:\"task:611\" | blocked_on:{{\"kind\":\"agent\",\"target\":\"<agent-id>\"}}. The flat form blocked_on_kind:\"task\", blocked_on_ref:\"611\" also works; pass kind=\"none\" to clear."
        );
    }
    let mut blocked_changed = false;
    // Recipients to ping with task.blocked_on_you when the task newly blocks on them: a single agent
    // (kind=agent), or every person a team resolves to (kind=team).
    let mut notify_blocked: Option<(BTreeSet<String>, Option<String>)> = None;
    if new_status != "blocked" {
        // Not blocked -> carry no blocked_on. Clear if there was one (or a set was attempted).
        if old_blocked_kind.is_some() || matches!(change, BlockedChange::Set { .. }) {
            sqlx::query(
                "UPDATE tasks SET blocked_on_kind=NULL, blocked_on_ref=NULL, blocked_on_note=NULL, updated_at=? WHERE id=?",
            )
            .bind(&ts)
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
            blocked_changed = true;
        }
    } else if let BlockedChange::Set { kind, target, note } = &change {
        let stored_ref = if kind == "operator" || kind == "external" {
            None
        } else {
            target.clone()
        };
        sqlx::query(
            "UPDATE tasks SET blocked_on_kind=?, blocked_on_ref=?, blocked_on_note=?, updated_at=? WHERE id=?",
        )
        .bind(kind)
        .bind(&stored_ref)
        .bind(note.as_deref())
        .bind(&ts)
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
        blocked_changed = true;
        // Notify the newly-blocking party (skip if the block already pointed at the same target).
        if let Some(target) = &stored_ref {
            let already = old_blocked_kind.as_deref() == Some(kind.as_str())
                && old_blocked_ref.as_deref() == Some(target.as_str());
            if !already {
                if kind == "agent" {
                    notify_blocked = Some((BTreeSet::from([target.clone()]), note.clone()));
                } else if kind == "team" {
                    // Fan out to every principal the team resolves to -- people AND agents (team-
                    // scoped agents, task 542 Phase 1c; agents are the ids that poll an inbox, so a
                    // team block reaches them too). Resolve on the open tx -- a second pool
                    // connection here would deadlock the pool.
                    let (mut recipients, agents) =
                        resolve_team_principals_tx(&mut tx, target).await?;
                    recipients.extend(agents);
                    if !recipients.is_empty() {
                        notify_blocked = Some((recipients, note.clone()));
                    }
                }
            }
        }
    }
    if let Some((set, note)) = notify_blocked {
        emit(
            &mut tx,
            &mut hooks,
            "task.blocked_on_you",
            actor,
            Some(task_id),
            Some(old_project_id),
            None,
            None,
            json!({ "title": old_title, "note": note }),
            Recipients::Explicit(set),
        )
        .await?;
    }

    // An empty-string assignee means "unassign" (clear to NULL) rather than a new owner to
    // subscribe. Only auto-subscribe a real, non-empty owner.
    let clearing = assignee == Some("");
    if !clearing {
        if let Some(a) = assignee {
            auto_subscribe(&mut tx, Some(a), task_id).await?;
        }
    }

    let status_changed = status.is_some() && status != Some(old_status.as_str());
    // A real (re)assignment to a non-empty owner that differs from the current one.
    let reassigned = assignee.is_some() && !clearing && assignee != old_assignee.as_deref();
    // Unassignment: the owner was cleared, and there was an owner to remove.
    let unassigned = clearing && old_assignee.is_some();

    if status_changed {
        emit(
            &mut tx,
            &mut hooks,
            "task.status_changed",
            actor,
            Some(task_id),
            Some(old_project_id),
            None,
            None,
            json!({ "from": old_status, "to": status, "title": old_title }),
            Recipients::FromTask,
        )
        .await?;
    }
    // Auto-unblock fan-out (task 614): when THIS task completes, notify the subscribers of every task
    // that was blocked_on it (kind=task, ref=this task) so they learn the blocker cleared and can
    // start -- the durable auto-unblock mechanism, not a manual sweep. Keyed on "done" per the
    // operator directive (a cancelled blocker is a re-plan, not an auto-start, so it is NOT fanned).
    if status_changed && new_status == "done" {
        let dependents = sqlx::query(
            "SELECT id, project_id, title FROM tasks WHERE blocked_on_kind='task' AND blocked_on_ref=?",
        )
        .bind(task_id.to_string())
        .fetch_all(&mut *tx)
        .await?;
        for d in &dependents {
            let dep_id: i64 = d.try_get("id")?;
            let dep_project: i64 = d.try_get("project_id")?;
            let dep_title: Option<String> = d.try_get("title").ok().flatten();
            emit(
                &mut tx,
                &mut hooks,
                "task.unblocked",
                actor,
                Some(dep_id),
                Some(dep_project),
                None,
                None,
                json!({ "blocker_task_id": task_id, "blocker_title": old_title, "title": dep_title }),
                Recipients::FromTask,
            )
            .await?;
        }
    }
    if reassigned {
        emit(
            &mut tx,
            &mut hooks,
            "task.assigned",
            actor,
            Some(task_id),
            Some(old_project_id),
            None,
            None,
            json!({ "assignee": assignee, "title": old_title }),
            Recipients::FromTask,
        )
        .await?;
    }
    if unassigned {
        // Carry the prior owner so a subscription-only auto-assigner knows who dropped it.
        emit(
            &mut tx,
            &mut hooks,
            "task.unassigned",
            actor,
            Some(task_id),
            Some(old_project_id),
            None,
            None,
            json!({ "from": old_assignee, "title": old_title }),
            Recipients::FromTask,
        )
        .await?;
    }
    if reparented {
        emit(
            &mut tx,
            &mut hooks,
            "task.reparented",
            actor,
            Some(task_id),
            Some(old_project_id),
            None,
            None,
            json!({ "from_parent_id": old_parent_id, "to_parent_id": reparent_to, "title": old_title }),
            Recipients::FromTask,
        )
        .await?;
    }
    if (has_fields || blocked_changed)
        && !status_changed
        && !reassigned
        && !unassigned
        && !reparented
    {
        emit(
            &mut tx,
            &mut hooks,
            "task.updated",
            actor,
            Some(task_id),
            Some(old_project_id),
            None,
            None,
            json!({ "title": old_title }),
            Recipients::FromTask,
        )
        .await?;
    }
    let assignee_warning = assignee_registration_warning(&mut tx, assignee).await?;
    let mut out = fetch_one_json(&mut tx, "SELECT * FROM tasks WHERE id=?", task_id)
        .await?
        .unwrap_or(Value::Null);
    if let (Value::Object(ref mut m), Some(w)) = (&mut out, assignee_warning) {
        m.insert("assignee_warning".into(), json!(w));
    }
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Reparent a task onto a different project. Emits `task.moved` (carrying both the old and
/// new project ids in its data) so subscribers on either project — and the live UI — learn
/// the task left one board column set and joined another. No-op-safe: moving a task onto its
/// current project just returns it unchanged.
pub async fn move_task(
    pool: &Pool,
    task_id: i64,
    to_project_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();

    let old = sqlx::query("SELECT project_id, title, parent_id FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(old) = old else {
        anyhow::bail!("no task {task_id}");
    };
    let from_project_id: i64 = old.try_get("project_id")?;
    let title: Option<String> = old.try_get("title")?;

    if sqlx::query("SELECT 1 FROM projects WHERE id=?")
        .bind(to_project_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        anyhow::bail!("no project {to_project_id}");
    }

    if from_project_id == to_project_id {
        let out = fetch_one_json(&mut tx, "SELECT * FROM tasks WHERE id=?", task_id)
            .await?
            .unwrap_or(Value::Null);
        tx.commit().await?;
        return Ok(out);
    }

    // Nesting is per-project, so a task tangled in a parent/child relationship can't cross
    // projects — reject with guidance to unlink first (clear the parent / move children).
    let parent: Option<i64> = old.try_get("parent_id")?;
    if parent.is_some() {
        anyhow::bail!(
            "cannot move task {task_id} to another project while it has a parent — clear its parent (parent_id=0) first"
        );
    }
    let child_count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM tasks WHERE parent_id=?")
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await?
        .try_get("n")?;
    if child_count > 0 {
        anyhow::bail!(
            "cannot move task {task_id} to another project while it has {child_count} child task(s) — reparent them first"
        );
    }

    sqlx::query("UPDATE tasks SET project_id=?, updated_at=? WHERE id=?")
        .bind(to_project_id)
        .bind(&ts)
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    emit(
        &mut tx,
        &mut hooks,
        "task.moved",
        actor,
        Some(task_id),
        Some(to_project_id),
        None,
        None,
        json!({ "from_project_id": from_project_id, "to_project_id": to_project_id, "title": title }),
        Recipients::FromTask,
    )
    .await?;
    let out = fetch_one_json(&mut tx, "SELECT * FROM tasks WHERE id=?", task_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Soft-archive (retire) a task, or restore it. Archiving stamps `archived_at` so the task drops
/// out of list_tasks by default (pass include_archived to see it), but keeps its comments, links,
/// and event history intact — reversible, never a destructive delete. Orthogonal to status (a done
/// task stays done AND archived). Emits task.archived / task.restored to the task's subscribers.
/// Idempotent (re-archiving refreshes the stamp). Returns the updated task.
pub async fn set_task_archived(
    pool: &Pool,
    task_id: i64,
    archived: bool,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    if sqlx::query("SELECT 1 FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        anyhow::bail!("no task {task_id}");
    }
    let stamp = archived.then(|| ts.clone());
    sqlx::query("UPDATE tasks SET archived_at=?, updated_at=? WHERE id=?")
        .bind(stamp.as_deref())
        .bind(&ts)
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    let event_type = if archived {
        "task.archived"
    } else {
        "task.restored"
    };
    emit(
        &mut tx,
        &mut hooks,
        event_type,
        actor,
        Some(task_id),
        None,
        None,
        None,
        json!({ "archived": archived }),
        Recipients::FromTask,
    )
    .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    get_task(pool, task_id).await
}

/// Default number of most-recent comments the agent-facing MCP `get_task` inlines when the caller
/// gives no explicit `comments_limit` (task #511). The REST/UI path stays unbounded (`None`).
pub const DEFAULT_TASK_COMMENTS: i64 = 20;

/// Fetch a task with ALL its comments inlined (chronological). Thin wrapper over
/// [`get_task_limited`] — the unbounded form kept for internal callers and the REST/UI path, which
/// renders the full thread. Agent-facing MCP reads pass a bound to stay under the context cap (#511).
pub async fn get_task(pool: &Pool, task_id: i64) -> anyhow::Result<Value> {
    get_task_limited(pool, task_id, None).await
}

/// Fetch a task as JSON. `comments_limit` bounds the inlined `comments` array (task #511, sibling of
/// #418): `None` inlines the whole thread (chronological); `Some(n)` with `n > 0` inlines only the
/// most-recent `n` comments (still chronological within the slice); `Some(0)` inlines none
/// (metadata-only). The response always carries `comment_count` (the total on the task) and
/// `comments_truncated` (true when fewer than the total were inlined), so a caller knows there is
/// more history to page in with a larger limit. Bounding the default keeps a long-lived, busy task
/// from overflowing an agent's read/context cap and forcing a manual extraction workaround.
pub async fn get_task_limited(
    pool: &Pool,
    task_id: i64,
    comments_limit: Option<i64>,
) -> anyhow::Result<Value> {
    let t = sqlx::query("SELECT * FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_optional(pool)
        .await?;
    let Some(t) = t else { return Ok(Value::Null) };
    let mut d = row_to_json(&t);
    if let Value::Object(ref mut m) = d {
        insert_ref(m, "task"); // typed canonical id (task 504)
                               // metadata: parse JSON string -> object
        let meta: Value = m
            .get("metadata")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));
        // Surface metadata.monitor_exempt as a derived top-level bool (default false) so the nudge
        // daemon + the #506 holding-work watchdog can read it from a list/get scan (task from
        // board-pm; consumed by v-fleet-tooling). metadata stays the source of truth.
        let monitor_exempt = meta
            .get("monitor_exempt")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        m.insert("metadata".into(), meta);
        m.insert("monitor_exempt".into(), Value::Bool(monitor_exempt));

        // Collapse the raw blocked_on_* columns into one nested object (null when not blocked).
        let blocked_on = m
            .get("blocked_on_kind")
            .and_then(|v| v.as_str())
            .map(|kind| {
                json!({
                    "kind": kind,
                    "target": m.get("blocked_on_ref").cloned().unwrap_or(Value::Null),
                    "note": m.get("blocked_on_note").cloned().unwrap_or(Value::Null),
                })
            });
        m.remove("blocked_on_kind");
        m.remove("blocked_on_ref");
        m.remove("blocked_on_note");
        let scalar_blocked = blocked_on.is_some();
        m.insert("blocked_on".into(), blocked_on.unwrap_or(Value::Null));

        // Derived question-block (task_628 slice 3, doc_33 A5): a task is effectively blocked when it
        // has a scalar blocked_on OR >=1 OPEN BLOCKING question. These fields are DERIVED (never
        // stored) so they stay consistent as questions are answered/declined/superseded. `routed_to`
        // is a general principal (agent / team / operator / external), so this covers agent-to-agent
        // questions as-is (cameron, task_628 comment 2628) -- no operator special-casing.
        let blocking_questions = open_blocking_questions_json(pool, task_id).await?;
        let question_blocked = !blocking_questions.is_empty();
        // question_blocked_on: the union of the open blocking questions' routed_to, as RAW principal
        // ids (NOT doc_26-expanded), mirroring the scalar blocked_on target. Sorted + deduped.
        let mut question_blocked_on: BTreeSet<String> = BTreeSet::new();
        for q in &blocking_questions {
            if let Some(rt) = q.get("routed_to").and_then(|v| v.as_str()) {
                question_blocked_on.insert(rt.to_string());
            }
        }
        m.insert(
            "blocking_questions".into(),
            Value::Array(blocking_questions),
        );
        m.insert("question_blocked".into(), Value::Bool(question_blocked));
        m.insert(
            "question_blocked_on".into(),
            json!(question_blocked_on.into_iter().collect::<Vec<_>>()),
        );
        m.insert(
            "effectively_blocked".into(),
            Value::Bool(scalar_blocked || question_blocked),
        );

        // Comments, bounded by `comments_limit` (#511). Always report the total so a caller knows
        // whether there is more than what was inlined.
        let comment_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM comments WHERE task_id=?")
                .bind(task_id)
                .fetch_one(pool)
                .await?;
        let comments: Vec<Value> = match comments_limit {
            // Metadata-only: inline no comments.
            Some(0) => Vec::new(),
            // Most-recent `n`, fetched newest-first then reversed so the slice stays chronological.
            Some(n) if n > 0 => {
                let rows = sqlx::query(
                    "SELECT c.id, c.author, c.body, c.created_at, c.external_author, c.origin_ref, \
                     c.type, c.payload, c.state, c.reply_to, c.supersedes, c.superseded_by, \
                     ei.display_name AS external_author_name \
                     FROM comments c LEFT JOIN external_identities ei ON ei.id = c.external_author \
                     WHERE c.task_id=? ORDER BY c.id DESC LIMIT ?",
                )
                .bind(task_id)
                .bind(n)
                .fetch_all(pool)
                .await?;
                rows.iter().rev().map(comment_row_json).collect()
            }
            // None (or a non-positive n other than 0): the whole thread, chronological.
            _ => {
                let rows = sqlx::query(
                    "SELECT c.id, c.author, c.body, c.created_at, c.external_author, c.origin_ref, \
                     c.type, c.payload, c.state, c.reply_to, c.supersedes, c.superseded_by, \
                     ei.display_name AS external_author_name \
                     FROM comments c LEFT JOIN external_identities ei ON ei.id = c.external_author \
                     WHERE c.task_id=? ORDER BY c.id",
                )
                .bind(task_id)
                .fetch_all(pool)
                .await?;
                rows.iter().map(comment_row_json).collect()
            }
        };
        let inlined = comments.len() as i64;
        m.insert("comments".into(), Value::Array(comments));
        m.insert("comment_count".into(), json!(comment_count));
        m.insert("comments_truncated".into(), json!(inlined < comment_count));

        let subs = sqlx::query(
            "SELECT subscriber FROM subscriptions WHERE target_type='task' AND target_id=?",
        )
        .bind(task_id)
        .fetch_all(pool)
        .await?;
        let sub_ids: Vec<Value> = subs
            .iter()
            .filter_map(|r| r.try_get::<String, _>("subscriber").ok().map(Value::String))
            .collect();
        m.insert("subscribers".into(), Value::Array(sub_ids));

        // Documents attached to this task (id/title/status/slug/project_id summaries;
        // project_id lets a client deep-link to the document's project view).
        let docs = sqlx::query(
            "SELECT d.id, d.title, d.status, d.slug, d.project_id FROM document_attachments a \
             JOIN documents d ON d.id = a.document_id WHERE a.task_id=? ORDER BY d.id",
        )
        .bind(task_id)
        .fetch_all(pool)
        .await?;
        m.insert(
            "attached_documents".into(),
            Value::Array(docs.iter().map(|r| row_to_json_ref(r, "doc")).collect()),
        );

        // Epic nesting: children (id/title/status), a done/total roll-up, and the parent title.
        let children =
            sqlx::query("SELECT id, title, status FROM tasks WHERE parent_id=? ORDER BY id")
                .bind(task_id)
                .fetch_all(pool)
                .await?;
        let total = children.len() as i64;
        let done = children
            .iter()
            .filter(|r| {
                r.try_get::<String, _>("status")
                    .map(|s| s == "done")
                    .unwrap_or(false)
            })
            .count() as i64;
        m.insert(
            "children".into(),
            Value::Array(
                children
                    .iter()
                    .map(|r| row_to_json_ref(r, "task"))
                    .collect(),
            ),
        );
        m.insert(
            "child_rollup".into(),
            json!({ "done": done, "total": total }),
        );

        let parent_id = m.get("parent_id").and_then(|v| v.as_i64());
        let parent_title = if let Some(pid) = parent_id {
            sqlx::query("SELECT title FROM tasks WHERE id=?")
                .bind(pid)
                .fetch_optional(pool)
                .await?
                .and_then(|r| r.try_get::<Option<String>, _>("title").ok().flatten())
                .map(Value::String)
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        m.insert("parent_title".into(), parent_title);
    }
    Ok(d)
}

#[allow(clippy::too_many_arguments)]
pub async fn list_tasks(
    pool: &Pool,
    project_id: Option<i64>,
    status: Option<&str>,
    assignee: Option<&str>,
    unassigned: bool,
    parent_id: Option<i64>,
    top_level: bool,
    search: Option<&str>,
    blocked_on_kind: Option<&str>,
    blocked_on_ref: Option<&str>,
    meta_key: Option<&str>,
    meta_value: Option<&str>,
    include_archived: bool,
) -> anyhow::Result<Value> {
    let mut q = String::from(
        // json_extract surfaces metadata.monitor_exempt as a lean derived column (1/0/null) so a
        // list scan can read it without pulling full metadata; normalized to a bool below (#517
        // sibling; consumed by v-fleet-tooling's nudge daemon + #506 watchdog).
        "SELECT id, project_id, title, status, assignee, priority, parent_id, updated_at, \
         blocked_on_kind, blocked_on_ref, json_extract(metadata, '$.monitor_exempt') AS monitor_exempt \
         FROM tasks",
    );
    let mut conds: Vec<&str> = Vec::new();
    // Soft-archived tasks are hidden by default (like list_documents); include_archived shows them.
    // Binds no value, so it can sit anywhere in conds without disturbing the bind order below.
    if !include_archived {
        conds.push("archived_at IS NULL");
    }
    if project_id.is_some() {
        conds.push("project_id=?");
    }
    if status.is_some() {
        conds.push("status=?");
    }
    // "What is waiting on me/the operator/agent X" views: filter by blocked_on kind and/or ref.
    if blocked_on_kind.is_some() {
        conds.push("blocked_on_kind=?");
    }
    if blocked_on_ref.is_some() {
        conds.push("blocked_on_ref=?");
    }
    // Free-text search over title + description (case-insensitive LIKE). Composable with the
    // other filters and, with no project_id, spans every project.
    if search.is_some() {
        conds.push("(title LIKE ? OR description LIKE ?)");
    }
    // `unassigned` selects rows with no owner (assignee IS NULL); it takes precedence over an
    // `assignee=` equality filter (asking for both a specific owner and no owner is a
    // contradiction, so we honor the more specific "no owner" intent).
    if unassigned {
        conds.push("assignee IS NULL");
    } else if assignee.is_some() {
        conds.push("assignee=?");
    }
    // Nesting filters: `parent_id` lists the direct children of an epic; `top_level` lists only
    // unparented tasks (the default board view = epics + loose tasks). parent_id wins if both.
    if parent_id.is_some() {
        conds.push("parent_id=?");
    } else if top_level {
        conds.push("parent_id IS NULL");
    }
    // Match a single metadata key/value (e.g. an idempotency tag): tasks whose metadata JSON has
    // `<meta_key>` equal to `<meta_value>`. Both must be given together to apply.
    if meta_key.is_some() && meta_value.is_some() {
        conds.push("json_extract(metadata, '$.' || ?) = ?");
    }
    if !conds.is_empty() {
        q.push_str(" WHERE ");
        q.push_str(&conds.join(" AND "));
    }
    q.push_str(" ORDER BY id");

    let mut query = sqlx::query(&q);
    if let Some(pid) = project_id {
        query = query.bind(pid);
    }
    if let Some(s) = status {
        query = query.bind(s);
    }
    if let Some(k) = blocked_on_kind {
        query = query.bind(k);
    }
    if let Some(r) = blocked_on_ref {
        query = query.bind(r);
    }
    if let Some(needle) = search {
        let like = format!("%{needle}%");
        query = query.bind(like.clone()).bind(like);
    }
    if !unassigned {
        if let Some(a) = assignee {
            query = query.bind(a);
        }
    }
    if let Some(pp) = parent_id {
        query = query.bind(pp);
    }
    if let (Some(k), Some(v)) = (meta_key, meta_value) {
        query = query.bind(k).bind(v);
    }
    let rows = query.fetch_all(pool).await?;
    // Normalize the json_extract result (1/0/null) into a real bool `monitor_exempt` (default false).
    let mut out: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v = row_to_json_ref(r, "task");
            if let Value::Object(ref mut m) = v {
                let exempt = matches!(m.get("monitor_exempt"), Some(x) if x.as_i64() == Some(1) || x.as_bool() == Some(true));
                m.insert("monitor_exempt".into(), Value::Bool(exempt));
            }
            v
        })
        .collect();
    // Reachability of each task's owner (task 340): annotate assignee_status + assignee_last_seen so
    // an orchestrator can pick a live owner / spot a stale one without a second list_agents call.
    annotate_assignee_reachability(pool, &mut out).await?;
    Ok(Value::Array(out))
}

/// Add `assignee_status` + `assignee_last_seen` to each task object, read from the assignee's agent
/// row (task 340 part 2). One batched query over the distinct assignees (not a JOIN -- the list
/// query's own `status` column would collide with `agents.status`). A task with no assignee, or an
/// assignee that is not a registered agent, gets nulls -- which is itself the signal (pairs with the
/// create/update `assignee_warning`): a null `assignee_status` on a non-null `assignee` means that
/// owner is not a live board agent.
async fn annotate_assignee_reachability(pool: &Pool, tasks: &mut [Value]) -> anyhow::Result<()> {
    let assignees: std::collections::BTreeSet<String> = tasks
        .iter()
        .filter_map(|t| {
            t.get("assignee")
                .and_then(|a| a.as_str())
                .map(str::to_string)
        })
        .collect();
    if assignees.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat_n("?", assignees.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT id, status, last_seen FROM agents WHERE id IN ({placeholders})");
    let mut q = sqlx::query(&sql);
    for a in &assignees {
        q = q.bind(a);
    }
    let mut map: std::collections::BTreeMap<String, (Option<String>, Option<String>)> =
        std::collections::BTreeMap::new();
    for r in q.fetch_all(pool).await? {
        let id: String = r.try_get("id")?;
        map.insert(id, (r.try_get("status")?, r.try_get("last_seen")?));
    }
    for t in tasks.iter_mut() {
        if let Value::Object(m) = t {
            let (status, last_seen) = m
                .get("assignee")
                .and_then(|a| a.as_str())
                .and_then(|a| map.get(a))
                .cloned()
                .unwrap_or((None, None));
            m.insert(
                "assignee_status".into(),
                status.map(Value::String).unwrap_or(Value::Null),
            );
            m.insert(
                "assignee_last_seen".into(),
                last_seen.map(Value::String).unwrap_or(Value::Null),
            );
        }
    }
    Ok(())
}

pub async fn comment_task(
    pool: &Pool,
    task_id: i64,
    body: &str,
    author: Option<&str>,
    external_author: Option<&str>,
    external_link: Option<ExternalRef>,
) -> anyhow::Result<Value> {
    // Reject an ambiguous bare "#N" in the submitted comment body (task #517 hard-fail).
    check_bare_refs(body)?;
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    if sqlx::query("SELECT 1 FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        anyhow::bail!("no task {task_id}");
    }
    // Idempotent ingest (task 270): if an external_link is given and a comment is ALREADY linked
    // on (source, external_id), return it with `created:false` — no duplicate comment, no repeat
    // fan-out/reflect. Same atomic SELECT-then-INSERT-under-one-tx guarantee as create_task.
    if let Some(ext) = &external_link {
        if let Some(row) = sqlx::query(
            "SELECT board_id FROM external_links WHERE source=? AND external_id=? AND board_kind='comment'",
        )
        .bind(&ext.source)
        .bind(&ext.external_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            let existing: i64 = row.try_get("board_id")?;
            tx.commit().await?;
            return Ok(json!({ "comment_id": existing, "task_id": task_id, "created": false }));
        }
    }
    // `author` is the fleet agent that wrote/ingested the comment (drives auto-subscribe +
    // notification actor); `external_author`, when set, is the external_identities id the
    // comment is attributed TO — an ingested human renders as that person, not the ingester.
    let cid: i64 = sqlx::query(
        "INSERT INTO comments(task_id, author, body, created_at, external_author) VALUES(?,?,?,?,?) RETURNING id",
    )
    .bind(task_id)
    .bind(author)
    .bind(body)
    .bind(&ts)
    .bind(external_author)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    auto_subscribe(&mut tx, author, task_id).await?;
    // An @mention subscribes that agent to the task, so they are woken on this comment + future
    // activity (operator subscription-based wake model). Idempotent; unregistered @tokens ignored.
    subscribe_mentions(&mut tx, body, task_id).await?;
    let mut data = json!({ "comment_id": cid, "body": body });
    if let Some(ext) = external_author {
        data["external_author"] = json!(ext);
    }
    emit(
        &mut tx,
        &mut hooks,
        "task.commented",
        author,
        Some(task_id),
        None,
        None,
        None,
        data,
        Recipients::FromTask,
    )
    .await?;
    // Outbound reflect-back for task comments (task 264): if this task is linked to one or more
    // external systems (external_links, board_kind='task') whose per-link policy authorizes this
    // comment's author OUT, emit a `task.outbound_reflect` per authorized link — the task-comment
    // analogue of `channel.outbound_reflect` (#150), symmetric so both bridge adapters decode the
    // same shape. Authz is keyed off the LINK's metadata (`direction` + `outbound_authors`, via
    // reflects_out), so a task linked to several systems governs each independently. An INGESTED
    // comment is authored by the bridge agent (not in outbound_authors), so it never echoes back
    // out — no special-casing needed. Infra event: no inbox fan-out (firehose/SSE still see it).
    if let Some(auth) = author {
        let links = sqlx::query(
            "SELECT source, external_id, external_parent_id, metadata FROM external_links WHERE board_kind='task' AND board_id=?",
        )
        .bind(task_id)
        .fetch_all(&mut *tx)
        .await?;
        for link in links {
            let meta: Value = link
                .try_get::<Option<String>, _>("metadata")?
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| json!({}));
            if !reflects_out(&meta, auth) {
                continue;
            }
            let mut reflect = json!({
                "task_id": task_id,
                "comment_id": cid,
                "author": auth,
                "body": body,
                "source": link.try_get::<String, _>("source")?,
                "external_id": link.try_get::<String, _>("external_id")?,
            });
            if let Some(parent) = link.try_get::<Option<String>, _>("external_parent_id")? {
                reflect["external_parent_id"] = json!(parent);
            }
            if let Some(ext) = external_author {
                reflect["external_author"] = json!(ext);
            }
            emit(
                &mut tx,
                &mut hooks,
                "task.outbound_reflect",
                author,
                Some(task_id),
                None,
                None,
                None,
                reflect,
                Recipients::Explicit(std::collections::BTreeSet::new()),
            )
            .await?;
        }
    }
    // If this task mirrors a promoted channel thread, fan the comment back out as a thread reply.
    mirror_task_comment_to_thread(
        &mut tx,
        &mut hooks,
        task_id,
        cid,
        author,
        body,
        external_author,
    )
    .await?;
    // Record the external dedup link (board_kind='comment', board_id=comment id) atomically with
    // the insert (task 270), so a retrying adapter re-hits the short-circuit above instead of
    // duplicating the comment.
    if let Some(ext) = &external_link {
        sqlx::query(
            "INSERT INTO external_links(source, external_id, external_parent_id, board_kind, board_id, metadata, created_at, updated_at) \
             VALUES(?,?,?,'comment',?,'{}',?,?)",
        )
        .bind(&ext.source)
        .bind(&ext.external_id)
        .bind(&ext.external_parent_id)
        .bind(cid)
        .bind(&ts)
        .bind(&ts)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(json!({ "comment_id": cid, "task_id": task_id, "created": true }))
}

/// Serialize a comment row to JSON with the rich-comment-type fields normalized (doc_33 / task_628):
/// the `payload` TEXT column is parsed from a JSON string into an object (mirroring how get_task /
/// get_project surface their metadata), the typed fields (type/state/reply_to/supersedes/
/// superseded_by) pass through, and a canonical `comment_<id>` ref is attached. A row selected
/// without the `payload` column (a narrow/legacy select) is returned unchanged.
pub fn comment_row_json(row: &SqliteRow) -> Value {
    let mut v = row_to_json(row);
    if let Value::Object(ref mut m) = v {
        if let Some(payload_str) = m.get("payload").and_then(|p| p.as_str()) {
            let parsed = serde_json::from_str::<Value>(payload_str).unwrap_or_else(|_| json!({}));
            m.insert("payload".into(), parsed);
        }
        insert_ref(m, "comment");
    }
    v
}

/// Read one comment by id as JSON (with its parsed `payload` and typed fields), for a machine read
/// of a question/answer comment outside its task thread (task_628). The external author's display
/// name is joined as in the task-comment list. Errors if the comment does not exist.
pub async fn get_comment(pool: &Pool, comment_id: i64) -> anyhow::Result<Value> {
    let Some(row) = sqlx::query(
        "SELECT c.id, c.task_id, c.author, c.body, c.created_at, c.external_author, c.origin_ref, \
         c.type, c.payload, c.state, c.reply_to, c.supersedes, c.superseded_by, \
         ei.display_name AS external_author_name \
         FROM comments c LEFT JOIN external_identities ei ON ei.id = c.external_author \
         WHERE c.id=?",
    )
    .bind(comment_id)
    .fetch_optional(pool)
    .await?
    else {
        anyhow::bail!("no comment {comment_id}");
    };
    Ok(comment_row_json(&row))
}

// --- task_628 slice 3: derived question-block (read-side) ---

/// The predicate for a comment that currently BLOCKS its task: an OPEN question whose payload marks
/// it blocking. Pinned by the slice-1 contract (doc_33 A5). SQLite `json_extract` yields the integer
/// 1 for a JSON `true` and the string `'true'` for a JSON `"true"`, so `IN (1,'true')` matches both
/// shapes (a bare `true` keyword is just 1 in SQLite). Shared by the get_task surfacing and the
/// "waiting on me" view so the two never drift.
const OPEN_BLOCKING_QUESTION: &str =
    "type='question' AND state='open' AND json_extract(payload,'$.blocking') IN (1,'true')";

/// The OPEN BLOCKING questions on a task, oldest-first (by comment id), each as
/// `{comment_id, kind, routed_to, blocking, prompt}` (task_628 slice 3). `prompt` is the question's
/// body so a reader sees what is asked without a second fetch; `routed_to` is the raw principal the
/// question is posed to (any agent / team / operator). Empty when the task has no open blocking
/// question. The ordering is stable so a client can diff the list across polls.
async fn open_blocking_questions_json(pool: &Pool, task_id: i64) -> anyhow::Result<Vec<Value>> {
    let rows = sqlx::query(&format!(
        "SELECT id, body, payload FROM comments \
         WHERE task_id=? AND {OPEN_BLOCKING_QUESTION} ORDER BY id"
    ))
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let id: i64 = r.try_get("id")?;
        let body: String = r.try_get("body").unwrap_or_default();
        let payload: Value = r
            .try_get::<String, _>("payload")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({}));
        out.push(json!({
            "comment_id": id,
            "kind": payload.get("kind").cloned().unwrap_or(Value::Null),
            "routed_to": payload.get("routed_to").cloned().unwrap_or(Value::Null),
            "blocking": true,
            "prompt": body,
        }));
    }
    Ok(out)
}

/// The set of principal ids whose doc_26 expansion INCLUDES `viewer`: the viewer itself plus every
/// team that (transitively) has the viewer as a member. The inverse of `resolve_principal_ids`
/// (which expands a principal DOWNWARD into its members) -- used to match a question's `routed_to`
/// (which may name a team) against a concrete viewer. Cycle-guarded upward walk over `team_members`.
async fn principals_routing_to(pool: &Pool, viewer: &str) -> anyhow::Result<BTreeSet<String>> {
    let mut out: BTreeSet<String> = BTreeSet::from([viewer.to_string()]);
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut stack = vec![viewer.to_string()];
    while let Some(member) = stack.pop() {
        if !visited.insert(member.clone()) {
            continue; // already expanded this member -> cycle guard
        }
        for r in sqlx::query("SELECT team_id FROM team_members WHERE member_id=?")
            .bind(&member)
            .fetch_all(pool)
            .await?
        {
            let team: String = r.try_get("team_id")?;
            if out.insert(team.clone()) {
                stack.push(team); // this team may itself be a member of parent teams
            }
        }
    }
    Ok(out)
}

/// The "waiting on me" view for derived question-blocks (task_628 slice 3, doc_33 A5): tasks with an
/// open blocking question routed (doc_26-expanded) to `viewer`. The analogue of the scalar
/// `list_tasks(blocked_on_kind='agent', blocked_on_ref=viewer)` view -- a question routed to a TEAM
/// surfaces for every (transitive) member, one routed directly to the viewer matches as-is, and it
/// works for any principal (agent-to-agent included), not just the operator. Honors `project_id` +
/// `include_archived` and returns the same row shape (+ monitor_exempt / assignee reachability) as
/// `list_tasks`, so a caller can union this array with a scalar-blocked `list_tasks` result directly.
pub async fn list_tasks_blocking_me(
    pool: &Pool,
    viewer: &str,
    project_id: Option<i64>,
    include_archived: bool,
) -> anyhow::Result<Value> {
    // Precompute (in Rust, since team expansion is not expressible in SQL) the principals whose
    // routed_to would surface for this viewer, then match them with a plain IN (...) in the EXISTS.
    let routed = principals_routing_to(pool, viewer).await?;
    let placeholders = std::iter::repeat_n("?", routed.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut conds: Vec<String> = Vec::new();
    if !include_archived {
        conds.push("archived_at IS NULL".into());
    }
    if project_id.is_some() {
        conds.push("project_id=?".into());
    }
    conds.push(format!(
        "EXISTS (SELECT 1 FROM comments c WHERE c.task_id = tasks.id AND c.{OPEN_BLOCKING_QUESTION} \
         AND json_extract(c.payload,'$.routed_to') IN ({placeholders}))"
    ));
    let q = format!(
        "SELECT id, project_id, title, status, assignee, priority, parent_id, updated_at, \
         blocked_on_kind, blocked_on_ref, json_extract(metadata, '$.monitor_exempt') AS monitor_exempt \
         FROM tasks WHERE {} ORDER BY id",
        conds.join(" AND "),
    );
    // Bind order mirrors the cond order: project_id (if any), then the routed principals.
    let mut query = sqlx::query(&q);
    if let Some(pid) = project_id {
        query = query.bind(pid);
    }
    for p in &routed {
        query = query.bind(p);
    }
    let rows = query.fetch_all(pool).await?;
    let mut out: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v = row_to_json_ref(r, "task");
            if let Value::Object(ref mut m) = v {
                let exempt = matches!(m.get("monitor_exempt"), Some(x) if x.as_i64() == Some(1) || x.as_bool() == Some(true));
                m.insert("monitor_exempt".into(), Value::Bool(exempt));
            }
            v
        })
        .collect();
    annotate_assignee_reachability(pool, &mut out).await?;
    Ok(Value::Array(out))
}

// --- Operator-question comment types: core operations (doc_33 / task_628, slice 2 + 5) ---

/// The launch question kinds (doc_33 A2); the set is extensible by adding a kind here + its shape.
pub const QUESTION_KINDS: &[&str] = &[
    "yes_no",
    "multiple_choice",
    "select_all",
    "fill_in_the_blank",
    "rank_list",
];
/// The answer-shape discriminators (doc_33 A2): choice covers single + multi select, so four shapes
/// cover the five kinds. The out-of-frame escape is NOT a shape -- it is a `text` answer whose
/// question state becomes `answered_outside_frame`.
pub const ANSWER_SHAPES: &[&str] = &["choice", "bool", "text", "ranked"];
/// Question lifecycle states (doc_33 A2); `open` is the only non-terminal one.
pub const QUESTION_STATES: &[&str] = &[
    "open",
    "answered",
    "answered_outside_frame",
    "declined",
    "cancelled",
    "superseded",
];

/// The framed answer shape a kind expects (doc_33 A2).
fn kind_expected_shape(kind: &str) -> &'static str {
    match kind {
        "yes_no" => "bool",
        "multiple_choice" | "select_all" => "choice",
        "fill_in_the_blank" => "text",
        "rank_list" => "ranked",
        _ => "text",
    }
}
/// Whether a kind carries an id/label options list.
fn kind_needs_options(kind: &str) -> bool {
    matches!(kind, "multiple_choice" | "select_all" | "rank_list")
}

/// Extract the option id strings from an options array ([{id,label}, ...]).
fn option_ids(options: &Value) -> Vec<String> {
    options
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|o| o.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Validate + normalize a question's options for its kind: kinds that need options require a
/// non-empty array of {id, label} with unique non-empty ids; other kinds carry no options.
fn normalize_question_options(kind: &str, options: Option<Value>) -> anyhow::Result<Value> {
    if !kind_needs_options(kind) {
        return Ok(json!([]));
    }
    let arr = match options {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => anyhow::bail!(
            "kind '{kind}' requires a non-empty `options` array of {{id, label}} pairs"
        ),
    };
    let mut seen = BTreeSet::new();
    for o in &arr {
        let id = o
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("every option needs a non-empty string `id`"))?;
        if !seen.insert(id.to_string()) {
            anyhow::bail!("duplicate option id '{id}'");
        }
        if o.get("label")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
        {
            anyhow::bail!("option '{id}' needs a non-empty string `label`");
        }
    }
    Ok(Value::Array(arr))
}

/// Validate a FRAMED answer `value` against the question `kind` + its `options`. The out-of-frame
/// text escape is validated separately by the caller.
fn validate_answer_value(kind: &str, value: &Value, options: &Value) -> anyhow::Result<()> {
    let ids: BTreeSet<String> = option_ids(options).into_iter().collect();
    match kind {
        "yes_no" => {
            if !value.is_boolean() {
                anyhow::bail!("a yes_no answer must be a boolean");
            }
        }
        "fill_in_the_blank" => {
            if value.as_str().map(|s| s.trim().is_empty()).unwrap_or(true) {
                anyhow::bail!("a fill_in_the_blank answer must be non-empty text");
            }
        }
        "multiple_choice" | "select_all" => {
            let arr = value
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("a {kind} answer must be an array of option ids"))?;
            if kind == "multiple_choice" && arr.len() != 1 {
                anyhow::bail!("a multiple_choice answer must be exactly one option id");
            }
            let mut chosen = BTreeSet::new();
            for v in arr {
                let id = v
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("each chosen option must be a string id"))?;
                if !ids.contains(id) {
                    anyhow::bail!("'{id}' is not one of the question's option ids");
                }
                if !chosen.insert(id.to_string()) {
                    anyhow::bail!("duplicate chosen option '{id}'");
                }
            }
        }
        "rank_list" => {
            let arr = value.as_array().ok_or_else(|| {
                anyhow::anyhow!("a rank_list answer must be the option ids in order")
            })?;
            let mut got = BTreeSet::new();
            for v in arr {
                let id = v
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("each ranked entry must be a string id"))?;
                if !ids.contains(id) {
                    anyhow::bail!("'{id}' is not one of the question's option ids");
                }
                if !got.insert(id.to_string()) {
                    anyhow::bail!("option '{id}' appears twice in the ranking");
                }
            }
            if got != ids {
                anyhow::bail!("a rank_list answer must rank every option exactly once");
            }
        }
        other => anyhow::bail!("unknown question kind '{other}'"),
    }
    Ok(())
}

/// Classify a principal id as a team / agent / person, erroring if it is none. "operator" is the
/// seeded team, so it classifies as "team".
async fn principal_kind(
    tx: &mut Transaction<'_, Sqlite>,
    id: &str,
) -> anyhow::Result<&'static str> {
    if sqlx::query("SELECT 1 FROM teams WHERE id=?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .is_some()
    {
        return Ok("team");
    }
    if sqlx::query("SELECT 1 FROM agents WHERE id=?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .is_some()
    {
        return Ok("agent");
    }
    if sqlx::query("SELECT 1 FROM people WHERE id=?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .is_some()
    {
        return Ok("person");
    }
    anyhow::bail!("no principal '{id}' (not a known team, agent, or person)")
}

/// The notification recipients for a question routed to `routed_to`: the agents that can act on it
/// (a team expands to its member agents; an agent is itself; a person has no board inbox and is
/// surfaced via the queue, not pinged) -- mirroring how a blocked_on=team fan-out notifies agents.
async fn resolve_routed_to_agents(
    tx: &mut Transaction<'_, Sqlite>,
    routed_to: &str,
    kind: &str,
) -> anyhow::Result<BTreeSet<String>> {
    Ok(match kind {
        "team" => resolve_team_principals_tx(tx, routed_to).await?.1,
        "agent" => BTreeSet::from([routed_to.to_string()]),
        _ => BTreeSet::new(),
    })
}

/// The task's OPEN BLOCKING question comments, as (comment_id, routed_to) rows. The derived
/// question-block (slice 3) and the terminal recompute both read through this. A question is
/// blocking iff payload.blocking is truthy.
pub async fn open_blocking_questions(
    tx: &mut Transaction<'_, Sqlite>,
    task_id: i64,
) -> anyhow::Result<Vec<(i64, String)>> {
    let rows = sqlx::query(&format!(
        "SELECT id, COALESCE(json_extract(payload,'$.routed_to'),'') AS routed_to FROM comments \
         WHERE task_id=? AND {OPEN_BLOCKING_QUESTION} ORDER BY id"
    ))
    .bind(task_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.try_get::<i64, _>("id").unwrap_or_default(),
                r.try_get::<String, _>("routed_to").unwrap_or_default(),
            )
        })
        .collect())
}

/// After a BLOCKING question on `task_id` reaches a terminal state, recompute the task's derived
/// question-block: if no open blocking question remains, the task is no longer question-blocked, so
/// emit task.unblocked to its watchers (reusing the task notification path). Callers invoke this
/// only when the just-resolved question was blocking, so a surviving open blocking question keeps
/// the task blocked and emits nothing.
pub async fn recompute_question_block(
    tx: &mut Transaction<'_, Sqlite>,
    hooks: &mut Vec<WebhookDelivery>,
    task_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<()> {
    if open_blocking_questions(tx, task_id).await?.is_empty() {
        emit(
            tx,
            hooks,
            "task.unblocked",
            actor,
            Some(task_id),
            None,
            None,
            None,
            json!({ "task_id": task_id, "reason": "all_blocking_questions_resolved" }),
            Recipients::FromTask,
        )
        .await?;
    }
    Ok(())
}

/// Pose a question as a type=question comment on a task (doc_33 A4). Carries the kind, options,
/// routed-to principal, blocking flag, and (non-blocking only) an optional default + wait period.
/// A blocking question contributes to the task's derived question-block until it resolves. Emits
/// question.posed to the routed-to principal's agents. Returns the question comment.
#[allow(clippy::too_many_arguments)]
pub async fn pose_question(
    pool: &Pool,
    task_id: i64,
    kind: &str,
    prompt: &str,
    options: Option<Value>,
    routed_to: &str,
    blocking: bool,
    default: Option<Value>,
    wait_period_seconds: Option<i64>,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    check_bare_refs(prompt)?;
    let prompt = prompt.trim();
    if prompt.is_empty() {
        anyhow::bail!("give a non-empty question prompt");
    }
    if !QUESTION_KINDS.contains(&kind) {
        anyhow::bail!(
            "unknown question kind '{kind}' (expected one of: {})",
            QUESTION_KINDS.join(", ")
        );
    }
    let routed_to = routed_to.trim();
    if routed_to.is_empty() {
        anyhow::bail!("give a `routed_to` principal (a person, team, or agent id)");
    }
    let options = normalize_question_options(kind, options)?;
    if blocking {
        if default.is_some() {
            anyhow::bail!("a blocking question cannot carry a `default` -- a default is for a non-blocking question the asker proceeds on");
        }
        if wait_period_seconds.is_some() {
            anyhow::bail!(
                "`wait_period_seconds` applies only to a non-blocking question with a default"
            );
        }
    }
    if let Some(w) = wait_period_seconds {
        if w < 0 {
            anyhow::bail!("`wait_period_seconds` must be >= 0");
        }
        if default.is_none() {
            anyhow::bail!("`wait_period_seconds` requires a `default` (the answer the asker proceeds on after waiting)");
        }
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let Some(task_row) = sqlx::query("SELECT project_id FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        anyhow::bail!("no task {task_id}");
    };
    let project_id: i64 = task_row.try_get("project_id")?;
    let routed_kind = principal_kind(&mut tx, routed_to).await?;
    if let Some(ref d) = default {
        validate_answer_value(kind, d, &options)
            .map_err(|e| anyhow::anyhow!("invalid `default`: {e}"))?;
    }
    let mut payload = Map::new();
    payload.insert("kind".into(), json!(kind));
    if kind_needs_options(kind) {
        payload.insert("options".into(), options);
    }
    payload.insert("routed_to".into(), json!(routed_to));
    payload.insert("blocking".into(), json!(blocking));
    if let Some(d) = default {
        payload.insert("default".into(), d);
    }
    if let Some(w) = wait_period_seconds {
        payload.insert("wait_period_seconds".into(), json!(w));
    }
    let payload_str = Value::Object(payload).to_string();
    let cid: i64 = sqlx::query(
        "INSERT INTO comments(task_id, author, body, type, payload, state, created_at) \
         VALUES(?,?,?,'question',?,'open',?) RETURNING id",
    )
    .bind(task_id)
    .bind(actor)
    .bind(prompt)
    .bind(&payload_str)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    let recips = resolve_routed_to_agents(&mut tx, routed_to, routed_kind).await?;
    emit(
        &mut tx,
        &mut hooks,
        "question.posed",
        actor,
        Some(task_id),
        Some(project_id),
        None,
        None,
        json!({ "comment_id": cid, "task_id": task_id, "kind": kind, "routed_to": routed_to, "blocking": blocking, "prompt": prompt }),
        Recipients::Explicit(recips),
    )
    .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    get_comment(pool, cid).await
}

/// Load an OPEN question comment within a tx, returning (task_id, author, payload). Errors if the
/// comment is missing, not a question, or not open -- the open-only guard the mutating ops share.
async fn load_open_question(
    tx: &mut Transaction<'_, Sqlite>,
    comment_id: i64,
) -> anyhow::Result<(i64, Option<String>, Value)> {
    let Some(q) =
        sqlx::query("SELECT task_id, author, type, state, payload FROM comments WHERE id=?")
            .bind(comment_id)
            .fetch_optional(&mut **tx)
            .await?
    else {
        anyhow::bail!("no comment {comment_id}");
    };
    let qtype: String = q.try_get("type")?;
    if qtype != "question" {
        anyhow::bail!("comment {comment_id} is not a question (type={qtype})");
    }
    let state: Option<String> = q.try_get("state")?;
    if state.as_deref() != Some("open") {
        anyhow::bail!(
            "question {comment_id} is not open (state={}); only an open question can be answered, declined, or cancelled",
            state.as_deref().unwrap_or("none")
        );
    }
    let task_id: i64 = q.try_get("task_id")?;
    let author: Option<String> = q.try_get("author")?;
    let payload: Value =
        serde_json::from_str(&q.try_get::<String, _>("payload")?).unwrap_or_else(|_| json!({}));
    Ok((task_id, author, payload))
}

/// A short human-readable body for an answer comment, from its shape + value.
fn answer_body_summary(shape: &str, value: &Value) -> String {
    match shape {
        "bool" => {
            if value.as_bool() == Some(true) {
                "yes".into()
            } else {
                "no".into()
            }
        }
        "text" => value.as_str().unwrap_or("").to_string(),
        "choice" => value
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        "ranked" => value
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(" > ")
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Answer an open question (doc_33 A4). A framed answer (shape matching the kind) moves the question
/// to `answered`; a `text` answer to a non-text kind is the universal out-of-frame escape and moves
/// it to `answered_outside_frame`. Records a separate type=answer comment that replies to the
/// question, clears the task's question-block if it was the last blocking one, and notifies the
/// asker. Returns the answer comment.
pub async fn answer_question(
    pool: &Pool,
    comment_id: i64,
    shape: &str,
    value: Value,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    if !ANSWER_SHAPES.contains(&shape) {
        anyhow::bail!(
            "unknown answer shape '{shape}' (expected one of: {})",
            ANSWER_SHAPES.join(", ")
        );
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let (task_id, author, payload) = load_open_question(&mut tx, comment_id).await?;
    let kind = payload.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let options = payload.get("options").cloned().unwrap_or_else(|| json!([]));
    let blocking = payload
        .get("blocking")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let expected = kind_expected_shape(kind);
    let (new_state, out_of_frame) = if shape == expected {
        validate_answer_value(kind, &value, &options)?;
        ("answered", false)
    } else if shape == "text" {
        if value.as_str().map(|s| s.trim().is_empty()).unwrap_or(true) {
            anyhow::bail!("an out-of-frame text answer must be non-empty text");
        }
        ("answered_outside_frame", true)
    } else {
        anyhow::bail!(
            "answer shape '{shape}' does not match question kind '{kind}' (expected '{expected}'); use shape=text for an out-of-frame answer"
        );
    };
    debug_assert!(
        QUESTION_STATES.contains(&new_state),
        "answer must leave the question in a known lifecycle state"
    );
    let ans_payload = json!({ "shape": shape, "value": value }).to_string();
    let body = answer_body_summary(shape, &value);
    let aid: i64 = sqlx::query(
        "INSERT INTO comments(task_id, author, body, type, payload, reply_to, created_at) \
         VALUES(?,?,?,'answer',?,?,?) RETURNING id",
    )
    .bind(task_id)
    .bind(actor)
    .bind(&body)
    .bind(&ans_payload)
    .bind(comment_id)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    sqlx::query("UPDATE comments SET state=? WHERE id=?")
        .bind(new_state)
        .bind(comment_id)
        .execute(&mut *tx)
        .await?;
    if blocking {
        recompute_question_block(&mut tx, &mut hooks, task_id, actor).await?;
    }
    let mut recips = BTreeSet::new();
    if let Some(a) = &author {
        recips.insert(a.clone());
    }
    if let Some(act) = actor {
        recips.remove(act);
    }
    emit(
        &mut tx,
        &mut hooks,
        "question.answered",
        actor,
        Some(task_id),
        None,
        None,
        None,
        json!({ "question_comment_id": comment_id, "answer_comment_id": aid, "task_id": task_id, "shape": shape, "out_of_frame": out_of_frame, "state": new_state }),
        Recipients::Explicit(recips),
    )
    .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    get_comment(pool, aid).await
}

/// Decline an open question (doc_33 A4): an explicit refusal with feedback, distinct from an
/// out-of-frame answer. Records the feedback as a type=answer comment, moves the question to
/// `declined`, clears the task's question-block if it was the last blocking one, and notifies the
/// asker. Returns the declining answer comment.
pub async fn decline_question(
    pool: &Pool,
    comment_id: i64,
    feedback: &str,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    check_bare_refs(feedback)?;
    let feedback = feedback.trim();
    if feedback.is_empty() {
        anyhow::bail!("give non-empty `feedback` when declining a question");
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let (task_id, author, payload) = load_open_question(&mut tx, comment_id).await?;
    let blocking = payload
        .get("blocking")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let ans_payload = json!({ "shape": "text", "value": feedback, "declined": true }).to_string();
    let aid: i64 = sqlx::query(
        "INSERT INTO comments(task_id, author, body, type, payload, reply_to, created_at) \
         VALUES(?,?,?,'answer',?,?,?) RETURNING id",
    )
    .bind(task_id)
    .bind(actor)
    .bind(feedback)
    .bind(&ans_payload)
    .bind(comment_id)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    sqlx::query("UPDATE comments SET state='declined' WHERE id=?")
        .bind(comment_id)
        .execute(&mut *tx)
        .await?;
    if blocking {
        recompute_question_block(&mut tx, &mut hooks, task_id, actor).await?;
    }
    let mut recips = BTreeSet::new();
    if let Some(a) = &author {
        recips.insert(a.clone());
    }
    if let Some(act) = actor {
        recips.remove(act);
    }
    emit(
        &mut tx,
        &mut hooks,
        "question.declined",
        actor,
        Some(task_id),
        None,
        None,
        None,
        json!({ "question_comment_id": comment_id, "answer_comment_id": aid, "task_id": task_id, "feedback": feedback }),
        Recipients::Explicit(recips),
    )
    .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    get_comment(pool, aid).await
}

/// Cancel an open question (doc_33 A4): the ASKER withdraws a question it no longer needs (e.g. it
/// found the answer itself). Only the asking author may cancel. Moves the question to `cancelled`,
/// clears the task's question-block if it was the last blocking one, and notifies the routed-to
/// principal that it is withdrawn. Returns the cancelled question comment.
pub async fn cancel_question(
    pool: &Pool,
    comment_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let (task_id, author, payload) = load_open_question(&mut tx, comment_id).await?;
    if author.as_deref() != actor || actor.is_none() {
        anyhow::bail!(
            "only the asking agent can cancel a question (it was posed by {})",
            author.as_deref().unwrap_or("someone else")
        );
    }
    let blocking = payload
        .get("blocking")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let routed_to = payload
        .get("routed_to")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    sqlx::query("UPDATE comments SET state='cancelled' WHERE id=?")
        .bind(comment_id)
        .execute(&mut *tx)
        .await?;
    if blocking {
        recompute_question_block(&mut tx, &mut hooks, task_id, actor).await?;
    }
    let recips = if routed_to.is_empty() {
        BTreeSet::new()
    } else {
        let kind = principal_kind(&mut tx, routed_to).await.unwrap_or("agent");
        resolve_routed_to_agents(&mut tx, routed_to, kind).await?
    };
    emit(
        &mut tx,
        &mut hooks,
        "question.cancelled",
        actor,
        Some(task_id),
        None,
        None,
        None,
        json!({ "question_comment_id": comment_id, "task_id": task_id }),
        Recipients::Explicit(recips),
    )
    .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    get_comment(pool, comment_id).await
}

/// Supersede an open question (doc_33 A6): the ASKER replaces a question it needs to correct or
/// restate with a fresh one, preserving the audit trail. The old question is kept IMMUTABLE -- it
/// moves to state `superseded` (payload + any answers untouched) and gains `superseded_by` pointing
/// at the replacement; the new question is a fresh OPEN question comment carrying a verbatim COPY of
/// the old payload (routing, blocking, kind/options, default, wait period -- all preserved) with the
/// new prompt, and `supersedes` pointing back. Copying the payload wholesale keeps supersede
/// agnostic to the answer-model shape (task_628: the pose payload may move to a JSON-schema + UI
/// descriptor; re-pose just carries whatever is there forward). Only the asking author may
/// supersede (mirrors cancel). If the old question was blocking, the task stays blocked on the
/// replacement -- the terminal recompute sees the fresh open blocking question and emits no spurious
/// unblock. Best-effort notifies the routed-to agents (one event carries both ids). Returns the NEW
/// question comment.
pub async fn supersede_question(
    pool: &Pool,
    comment_id: i64,
    new_prompt: &str,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    check_bare_refs(new_prompt)?;
    let new_prompt = new_prompt.trim();
    if new_prompt.is_empty() {
        anyhow::bail!("give a non-empty prompt for the superseding question");
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let (task_id, author, payload) = load_open_question(&mut tx, comment_id).await?;
    // Only the asker may supersede their own question (mirrors cancel_question).
    if actor.is_none() || author.as_deref() != actor {
        anyhow::bail!(
            "only the asking agent can supersede a question (it was posed by {})",
            author.as_deref().unwrap_or("someone else")
        );
    }
    let blocking = payload
        .get("blocking")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let routed_to = payload
        .get("routed_to")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let project_id: i64 = sqlx::query("SELECT project_id FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await?
        .try_get("project_id")?;
    // Re-pose: a fresh OPEN question comment carrying a copy of the old payload + the new prompt,
    // linked back to the old via `supersedes`.
    let payload_str = payload.to_string();
    let new_cid: i64 = sqlx::query(
        "INSERT INTO comments(task_id, author, body, type, payload, state, supersedes, created_at) \
         VALUES(?,?,?,'question',?,'open',?,?) RETURNING id",
    )
    .bind(task_id)
    .bind(actor)
    .bind(new_prompt)
    .bind(&payload_str)
    .bind(comment_id)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    // The old question becomes immutable + superseded, pointing forward at its replacement.
    sqlx::query("UPDATE comments SET state='superseded', superseded_by=? WHERE id=?")
        .bind(new_cid)
        .bind(comment_id)
        .execute(&mut *tx)
        .await?;
    // Terminal recompute: the old blocking question resolved, but the fresh one keeps the task
    // blocked, so this sees a surviving open blocking question and emits no task.unblocked.
    if blocking {
        recompute_question_block(&mut tx, &mut hooks, task_id, actor).await?;
    }
    // Best-effort notify the routed-to agents: the question is superseded by new_comment_id. routed_to
    // is unchanged (payload copied), so recipients mirror the original pose.
    let recips = if routed_to.is_empty() {
        BTreeSet::new()
    } else {
        let kind = principal_kind(&mut tx, &routed_to).await.unwrap_or("agent");
        resolve_routed_to_agents(&mut tx, &routed_to, kind).await?
    };
    emit(
        &mut tx,
        &mut hooks,
        "question.superseded",
        actor,
        Some(task_id),
        Some(project_id),
        None,
        None,
        json!({ "question_comment_id": comment_id, "new_comment_id": new_cid, "task_id": task_id, "routed_to": routed_to, "blocking": blocking, "prompt": new_prompt }),
        Recipients::Explicit(recips),
    )
    .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    get_comment(pool, new_cid).await
}

/// Merge arbitrary key/value properties into a task's metadata (JSON). Returns the
/// merged metadata. Used for pipeline state (ipfs_cid, collection, stage, ...).
pub async fn set_task_props(pool: &Pool, task_id: i64, props: Value) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT metadata FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else {
        anyhow::bail!("no task {task_id}");
    };
    let existing: Option<String> = row.try_get("metadata")?;
    let mut meta: Map<String, Value> =
        serde_json::from_str(existing.as_deref().unwrap_or("{}")).unwrap_or_default();
    if let Value::Object(m) = props {
        for (k, v) in m {
            meta.insert(k, v);
        }
    }
    let meta_val = Value::Object(meta);
    sqlx::query("UPDATE tasks SET metadata=?, updated_at=? WHERE id=?")
        .bind(meta_val.to_string())
        .bind(&ts)
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({ "task_id": task_id, "metadata": meta_val }))
}

// --- Subscriptions ---

pub async fn subscribe(
    pool: &Pool,
    subscriber: &str,
    task_id: Option<i64>,
    project_id: Option<i64>,
    channel_id: Option<i64>,
    document_id: Option<i64>,
    board: bool,
) -> anyhow::Result<Value> {
    let (tt, tid) = target(task_id, project_id, channel_id, document_id, board)?;
    sqlx::query(
        "INSERT OR IGNORE INTO subscriptions(subscriber, target_type, target_id, created_at) \
         VALUES(?,?,?,?)",
    )
    .bind(subscriber)
    .bind(tt)
    .bind(tid)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(json!({ "subscriber": subscriber, "target_type": tt, "target_id": tid }))
}

/// Subscribe with an event-class filter (#462): the subscription is delivery-gated to `event_classes`
/// — only events in one of those classes reach the inbox AND wake the subscriber, everything else is
/// dropped for this subscription. The low-noise alternative to the full firehose (board + ["created"]
/// for a triage agent) or to a creator-subscribed all-events gap ticket (task + ["done", "blocked"]).
/// Idempotent: UPSERTS the class set, so a caller can safely re-register the same (subscriber,
/// target) with an updated filter. For an unfiltered (every-event) subscription, use `subscribe`.
#[allow(clippy::too_many_arguments)]
pub async fn subscribe_classed(
    pool: &Pool,
    subscriber: &str,
    task_id: Option<i64>,
    project_id: Option<i64>,
    channel_id: Option<i64>,
    document_id: Option<i64>,
    board: bool,
    event_classes: &[String],
) -> anyhow::Result<Value> {
    let (tt, tid) = target(task_id, project_id, channel_id, document_id, board)?;
    let ec = json!(event_classes).to_string();
    sqlx::query(
        "INSERT INTO subscriptions(subscriber, target_type, target_id, event_classes, created_at) \
         VALUES(?,?,?,?,?) \
         ON CONFLICT(subscriber, target_type, target_id) DO UPDATE SET event_classes=excluded.event_classes",
    )
    .bind(subscriber)
    .bind(tt)
    .bind(tid)
    .bind(&ec)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(
        json!({ "subscriber": subscriber, "target_type": tt, "target_id": tid, "event_classes": event_classes }),
    )
}

/// Subscribe an agent to a channel THREAD (#438). The thread root is a channel post's event seq;
/// subsequent in-thread posts (reply_to = this root) are delivered to the subscriber AND wake them,
/// so a reactive agent that joined a thread answers later follow-ups without a re-mention. The root
/// seq is globally unique, so it alone keys the subscription. Idempotent (INSERT OR IGNORE) so a
/// bridge daemon can safely re-register the same root each tick.
pub async fn subscribe_thread(
    pool: &Pool,
    subscriber: &str,
    thread_root: i64,
) -> anyhow::Result<Value> {
    sqlx::query(
        "INSERT OR IGNORE INTO subscriptions(subscriber, target_type, target_id, created_at) \
         VALUES(?,'thread',?,?)",
    )
    .bind(subscriber)
    .bind(thread_root)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(json!({ "subscriber": subscriber, "target_type": "thread", "target_id": thread_root }))
}

/// Unsubscribe an agent from a channel thread (#438). Used when a bridge evicts a cold thread from
/// its active set, so a stale thread stops waking the agent.
pub async fn unsubscribe_thread(
    pool: &Pool,
    subscriber: &str,
    thread_root: i64,
) -> anyhow::Result<Value> {
    let n = sqlx::query(
        "DELETE FROM subscriptions WHERE subscriber=? AND target_type='thread' AND target_id=?",
    )
    .bind(subscriber)
    .bind(thread_root)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(json!({ "removed": n }))
}

pub async fn unsubscribe(
    pool: &Pool,
    subscriber: &str,
    task_id: Option<i64>,
    project_id: Option<i64>,
    channel_id: Option<i64>,
    document_id: Option<i64>,
    board: bool,
) -> anyhow::Result<Value> {
    let (tt, tid) = target(task_id, project_id, channel_id, document_id, board)?;
    let n = sqlx::query(
        "DELETE FROM subscriptions WHERE subscriber=? AND target_type=? AND target_id=?",
    )
    .bind(subscriber)
    .bind(tt)
    .bind(tid)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(json!({ "removed": n }))
}

/// Mute a task for an agent: suppress this task's event fan-out (comments, status changes, …)
/// to that agent, even though they're the creator/assignee/subscriber. Lets a stood-down owner
/// detach from FYI wakes on a task they opened — `unsubscribe` can't, because the creator is in
/// the fan-out independent of any subscription row. Idempotent. Only this task's fan-out is
/// affected: a direct message still reaches the agent. `unmute_task` reverses it.
pub async fn mute_task(pool: &Pool, agent: &str, task_id: i64) -> anyhow::Result<Value> {
    if sqlx::query("SELECT 1 FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_optional(pool)
        .await?
        .is_none()
    {
        anyhow::bail!("no task {task_id}");
    }
    sqlx::query("INSERT OR IGNORE INTO task_mutes(task_id, agent, created_at) VALUES(?,?,?)")
        .bind(task_id)
        .bind(agent)
        .bind(now_iso())
        .execute(pool)
        .await?;
    Ok(json!({ "task_id": task_id, "agent": agent, "muted": true }))
}

/// Unmute a task for an agent (reverses `mute_task`): the agent rejoins the task's fan-out.
pub async fn unmute_task(pool: &Pool, agent: &str, task_id: i64) -> anyhow::Result<Value> {
    let n = sqlx::query("DELETE FROM task_mutes WHERE task_id=? AND agent=?")
        .bind(task_id)
        .bind(agent)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(json!({ "task_id": task_id, "agent": agent, "muted": false, "removed": n }))
}

fn target(
    task_id: Option<i64>,
    project_id: Option<i64>,
    channel_id: Option<i64>,
    document_id: Option<i64>,
    board: bool,
) -> anyhow::Result<(&'static str, i64)> {
    // `board` is the whole-board firehose scope: a single subscription that receives every
    // emitted event. It uses a fixed sentinel target_id (0 — real task/project/channel/document
    // ids start at 1), so (subscriber, 'board', 0) stays unique for INSERT OR IGNORE dedup.
    match (task_id, project_id, channel_id, document_id, board) {
        (Some(t), ..) => Ok(("task", t)),
        (None, Some(p), ..) => Ok(("project", p)),
        (None, None, Some(c), ..) => Ok(("channel", c)),
        (None, None, None, Some(d), _) => Ok(("document", d)),
        (None, None, None, None, true) => Ok(("board", 0)),
        (None, None, None, None, false) => {
            anyhow::bail!("give task_id, project_id, channel_id, document_id, or board=true")
        }
    }
}

// --- Channels ---
//
// A channel is a named discussion container agents post to and subscribe to. It reuses the
// existing machinery wholesale: membership IS a `channel` subscription row, a post IS an
// event (carrying channel_id) fanned out via Recipients::FromChannel, and the inbox / SSE
// tailer / webhooks carry channel posts for free. Direct messages are just a *private 1:1*
// channel (keyed by dm_key), so there is a single data model — see dm_channel().

/// Turn a channel row into JSON with `metadata` parsed from its TEXT column and `private`
/// surfaced as a bool (SQLite stores it as 0/1). Mirrors project_json/agent_json.
fn channel_json(row: &SqliteRow) -> Value {
    let mut obj = match row_to_json(row) {
        Value::Object(m) => m,
        other => return other,
    };
    let meta = obj
        .get("metadata")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| json!({}));
    obj.insert("metadata".into(), meta);
    insert_ref(&mut obj, "channel"); // typed canonical id (task 504)
    if let Some(p) = obj.get("private").and_then(|v| v.as_i64()) {
        obj.insert("private".into(), Value::Bool(p != 0));
    }
    Value::Object(obj)
}

/// Create a named channel (get-or-create by case-insensitive name, like projects), auto-
/// subscribing the creator as its first member. Returns the channel. Named channels are
/// never private — DMs are created via send_message/dm_channel, not here.
pub async fn create_channel(
    pool: &Pool,
    name: &str,
    topic: Option<&str>,
    created_by: Option<&str>,
    metadata: Option<Value>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();

    // Get-or-create by case-insensitive name among non-DM channels (dm_key IS NULL), so
    // opening "#general" twice returns the same channel rather than a duplicate.
    if let Some(row) =
        sqlx::query("SELECT id FROM channels WHERE dm_key IS NULL AND name = ? COLLATE NOCASE")
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?
    {
        let existing_id: i64 = row.try_get("id")?;
        if let Some(sub) = created_by {
            join_channel(&mut tx, existing_id, sub).await?;
        }
        let out = channel_row_json(&mut tx, existing_id)
            .await?
            .unwrap_or(Value::Null);
        tx.commit().await?;
        return Ok(out);
    }

    let meta_str = metadata.unwrap_or_else(|| json!({})).to_string();
    let cid: i64 = sqlx::query(
        "INSERT INTO channels(name, topic, private, metadata, created_by, created_at, updated_at) \
         VALUES(?,?,0,?,?,?,?) RETURNING id",
    )
    .bind(name)
    .bind(topic)
    .bind(&meta_str)
    .bind(created_by)
    .bind(&ts)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    if let Some(sub) = created_by {
        join_channel(&mut tx, cid, sub).await?;
    }
    // A silent creation event (no recipients) so it lands in the audit log + SSE tail.
    emit(
        &mut tx,
        &mut hooks,
        "channel.created",
        created_by,
        None,
        None,
        Some(cid),
        None,
        json!({ "name": name }),
        Recipients::Explicit(BTreeSet::new()),
    )
    .await?;
    let out = channel_row_json(&mut tx, cid).await?.unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Fetch one channel row as JSON (with member ids), or None if it doesn't exist.
async fn channel_row_json(
    tx: &mut Transaction<'_, Sqlite>,
    channel_id: i64,
) -> anyhow::Result<Option<Value>> {
    let Some(row) = sqlx::query("SELECT * FROM channels WHERE id=?")
        .bind(channel_id)
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(None);
    };
    let mut d = channel_json(&row);
    let members = sqlx::query(
        "SELECT subscriber FROM subscriptions WHERE target_type='channel' AND target_id=? ORDER BY subscriber",
    )
    .bind(channel_id)
    .fetch_all(&mut **tx)
    .await?;
    if let Value::Object(ref mut m) = d {
        let ids: Vec<Value> = members
            .iter()
            .filter_map(|r| r.try_get::<String, _>("subscriber").ok().map(Value::String))
            .collect();
        m.insert("members".into(), Value::Array(ids));
    }
    Ok(Some(d))
}

/// Subscribe an agent to a channel (idempotent). Membership IS subscription.
async fn join_channel(
    tx: &mut Transaction<'_, Sqlite>,
    channel_id: i64,
    subscriber: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO subscriptions(subscriber, target_type, target_id, created_at) \
         VALUES(?,'channel',?,?)",
    )
    .bind(subscriber)
    .bind(channel_id)
    .bind(now_iso())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// List channels. Private channels (incl. DMs) are shown only to their members; named public
/// channels are always listed. `member` optionally scopes to channels a given agent belongs to.
pub async fn list_channels(pool: &Pool, member: Option<&str>) -> anyhow::Result<Value> {
    // A channel is visible if it's public (private=0) OR the viewer is a member. When `member`
    // is given we also restrict to that agent's channels regardless of visibility.
    let rows = match member {
        Some(m) => {
            sqlx::query(
                "SELECT c.* FROM channels c \
                 JOIN subscriptions s ON s.target_type='channel' AND s.target_id=c.id \
                 WHERE s.subscriber=? ORDER BY c.id",
            )
            .bind(m)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query("SELECT * FROM channels WHERE private=0 ORDER BY id")
                .fetch_all(pool)
                .await?
        }
    };
    let mut out = Vec::new();
    for r in &rows {
        let cid: i64 = r.try_get("id")?;
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM subscriptions WHERE target_type='channel' AND target_id=?",
        )
        .bind(cid)
        .fetch_one(pool)
        .await?;
        let mut d = channel_json(r);
        if let Value::Object(ref mut m) = d {
            m.insert("member_count".into(), json!(n));
        }
        out.push(d);
    }
    Ok(Value::Array(out))
}

/// Fetch one channel with its member list.
pub async fn get_channel(pool: &Pool, channel_id: i64) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let out = channel_row_json(&mut tx, channel_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    Ok(out)
}

/// Post a message to a channel. The poster is auto-joined (so posting implies membership),
/// then the post is emitted to every member's inbox. `reply_to` optionally threads under a
/// parent post's event seq (one level only — a reply carries the parent seq in its data).
/// The event type is `channel.post` for named channels and `message.direct` for DM channels,
/// so DMs keep flowing through get_messages/the inbox exactly as before.
pub async fn post_to_channel_meta(
    pool: &Pool,
    channel_id: i64,
    sender: &str,
    body: &str,
    reply_to: Option<i64>,
    external_author: Option<&str>,
    metadata: Option<Value>,
) -> anyhow::Result<Value> {
    // Reject an ambiguous bare "#N" in the submitted post body (task #517 hard-fail).
    check_bare_refs(body)?;
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();

    let ch = sqlx::query("SELECT dm_key, metadata FROM channels WHERE id=?")
        .bind(channel_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(ch) = ch else {
        anyhow::bail!("no channel {channel_id}");
    };
    let is_dm: Option<String> = ch.try_get("dm_key")?;
    let evtype = if is_dm.is_some() {
        "message.direct"
    } else {
        "channel.post"
    };
    let ch_meta: Value = ch
        .try_get::<Option<String>, _>("metadata")?
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| json!({}));

    join_channel(&mut tx, channel_id, sender).await?;

    // `from` is the fleet agent that posted/ingested; `external_author`, when set, is the
    // external_identities id the message is attributed to (an ingested human), carried on the
    // post event so readers render the external person, not the ingesting agent.
    let mut data = json!({ "body": body, "from": sender });
    if let Some(parent) = reply_to {
        data["reply_to"] = json!(parent);
    }
    if let Some(ext) = external_author {
        data["external_author"] = json!(ext);
    }
    // Per-post metadata (#429): an arbitrary bag stored on the post (e.g. a bridge stamps a
    // relayed message's {slack_ts, slack_channel, thread_ts}). Carried on the post event's data so
    // it survives on the durable log and can be surfaced on reads + the outbound reflect.
    if let Some(ref m) = metadata {
        data["metadata"] = m.clone();
    }
    let seq = emit(
        &mut tx,
        &mut hooks,
        evtype,
        Some(sender),
        None,
        None,
        Some(channel_id),
        None,
        data,
        // Deliver to channel members AND, when this post replies to a thread root, to that thread's
        // subscribers (#438) — so an agent that joined the thread is woken on in-thread follow-ups
        // without a re-mention. reply_to=None is identical to a plain channel fan-out.
        Recipients::FromChannelThread(channel_id, reply_to),
    )
    .await?;

    // Outbound reflect-back authz (design #141 §5): the board is authoritative on which posts
    // may leave the board for an external system. A `channel.outbound_reflect` event is emitted
    // ONLY when the channel's policy permits this author — a bridge adapter is a dumb executor
    // that acts solely on these authorized events. Other posts stay board-internal (no event).
    if reflects_out(&ch_meta, sender) {
        let mut reflect = json!({
            "channel_id": channel_id,
            "post_seq": seq,
            "author": sender,
            "body": body,
        });
        if let Some(ref m) = metadata {
            reflect["metadata"] = m.clone();
        }
        if let Some(parent) = reply_to {
            reflect["reply_to"] = json!(parent);
            // Stateless threading (#429): surface the reply parent's stored metadata (e.g. a Slack
            // thread_ts) so a bridge can thread the reflected message with no {post_seq -> ts} map
            // of its own. One-level parent lookup — a bridge stamps the thread ROOT id on every
            // relayed post's metadata, so the immediate parent already carries the root.
            if let Some(prow) = sqlx::query("SELECT data FROM events WHERE seq=?")
                .bind(parent)
                .fetch_optional(&mut *tx)
                .await?
            {
                let pdata: Value = prow
                    .try_get::<String, _>("data")
                    .ok()
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .unwrap_or(Value::Null);
                if let Some(pm) = pdata.get("metadata") {
                    reflect["parent_metadata"] = pm.clone();
                }
            }
        }
        if let Some(ext) = external_author {
            reflect["external_author"] = json!(ext);
        }
        // Infra event: no inbox fan-out (a whole-board firehose subscriber / the SSE feed still
        // sees it — that's how the adapter picks it up).
        emit(
            &mut tx,
            &mut hooks,
            "channel.outbound_reflect",
            Some(sender),
            None,
            None,
            Some(channel_id),
            None,
            reflect,
            Recipients::Explicit(std::collections::BTreeSet::new()),
        )
        .await?;
    }

    // If this post replies to a promoted thread's root, mirror it into the linked task as a
    // comment (live thread->task sync, #151 slice 2). Direct insert -> no echo back to the thread.
    mirror_thread_reply_to_task(
        &mut tx,
        &mut hooks,
        channel_id,
        seq,
        reply_to,
        sender,
        body,
        external_author,
    )
    .await?;

    sqlx::query("UPDATE channels SET updated_at=? WHERE id=?")
        .bind(now_iso())
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(json!({ "channel_id": channel_id, "seq": seq }))
}

/// Post to a channel with no per-post metadata (the common path). Thin wrapper over
/// `post_to_channel_meta`.
pub async fn post_to_channel(
    pool: &Pool,
    channel_id: i64,
    sender: &str,
    body: &str,
    reply_to: Option<i64>,
    external_author: Option<&str>,
) -> anyhow::Result<Value> {
    post_to_channel_meta(
        pool,
        channel_id,
        sender,
        body,
        reply_to,
        external_author,
        None,
    )
    .await
}

/// Outbound reflect-back policy (design #141 §5), read from a policy-bearing `metadata` bag —
/// a channel's `metadata` for channel posts, or an `external_links` row's `metadata` for task
/// comments (task 264): `{ "outbound_authors": [..] (default ["concierge"]), "direction":
/// "in"|"out"|"both" (default "in") }`. Content reflects OUT to an external system iff
/// `direction` allows outbound (`out`/`both`) AND its `author` is in the `outbound_authors`
/// allowlist. The safe default is board-internal: an unconfigured entity (direction defaults to
/// "in") reflects nothing, so existing channels/links never start leaking to an external system.
fn reflects_out(metadata: &Value, author: &str) -> bool {
    let direction = metadata
        .get("direction")
        .and_then(|v| v.as_str())
        .unwrap_or("in");
    if direction != "out" && direction != "both" {
        return false;
    }
    match metadata.get("outbound_authors").and_then(|v| v.as_array()) {
        Some(authors) => authors.iter().any(|a| a.as_str() == Some(author)),
        // direction allows out but no explicit allowlist -> the documented default.
        None => author == "concierge",
    }
}

/// Merge arbitrary key/value properties into a channel's metadata (JSON), returning the merged
/// bag — mirrors `set_task_props`. This is how a channel's outbound reflect-back policy
/// (`outbound_authors` / `direction`, see `channel_reflects_out`) is configured after creation.
pub async fn set_channel_props(
    pool: &Pool,
    channel_id: i64,
    props: Value,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT metadata FROM channels WHERE id=?")
        .bind(channel_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else {
        anyhow::bail!("no channel {channel_id}");
    };
    let existing: Option<String> = row.try_get("metadata")?;
    let mut meta: Map<String, Value> =
        serde_json::from_str(existing.as_deref().unwrap_or("{}")).unwrap_or_default();
    if let Value::Object(m) = props {
        for (k, v) in m {
            meta.insert(k, v);
        }
    }
    let meta_val = Value::Object(meta);
    sqlx::query("UPDATE channels SET metadata=?, updated_at=? WHERE id=?")
        .bind(meta_val.to_string())
        .bind(&ts)
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({ "channel_id": channel_id, "metadata": meta_val }))
}

/// Set (or clear) a channel's `auto_join` flag — a fleet-wide broadcast channel every agent
/// belongs to. Enabling BACKFILLS: it joins every currently-registered agent, so the channel
/// immediately has everyone as a member; each agent registered later auto-joins on register (see
/// register_agent). Idempotent (re-enabling just re-runs the idempotent backfill; disabling leaves
/// existing members in place, it only stops future auto-joins).
pub async fn set_channel_auto_join(
    pool: &Pool,
    channel_id: i64,
    auto_join: bool,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    if sqlx::query("SELECT 1 FROM channels WHERE id=?")
        .bind(channel_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        anyhow::bail!("no channel {channel_id}");
    }
    sqlx::query("UPDATE channels SET auto_join=?, updated_at=? WHERE id=?")
        .bind(auto_join as i64)
        .bind(&ts)
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    if auto_join {
        // Backfill every registered agent as a member (idempotent).
        let agents = sqlx::query("SELECT id FROM agents")
            .fetch_all(&mut *tx)
            .await?;
        for a in &agents {
            let id: String = a.try_get("id")?;
            join_channel(&mut tx, channel_id, &id).await?;
        }
    }
    // Silent audit event (no fan-out) so the change lands in the log + SSE tail.
    emit(
        &mut tx,
        &mut hooks,
        "channel.updated",
        actor,
        None,
        None,
        Some(channel_id),
        None,
        json!({ "auto_join": auto_join }),
        Recipients::Explicit(BTreeSet::new()),
    )
    .await?;
    let out = channel_row_json(&mut tx, channel_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Derive a task title from a thread's root body: its first non-empty line, trimmed and
/// truncated, with a fallback when the body is empty.
fn thread_title(body: &str, channel_id: i64) -> String {
    let first = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if first.is_empty() {
        return format!("Thread from channel {channel_id}");
    }
    if first.chars().count() > 120 {
        let truncated: String = first.chars().take(117).collect();
        format!("{truncated}…")
    } else {
        first.to_string()
    }
}

/// Promote a channel thread into a task (design #141 §7, implements #143). The thread's root
/// post becomes the task description; each direct reply becomes a task comment, preserving the
/// original author/external-author attribution and timestamp. A durable `task_links` row records
/// the thread↔task link so the promotion is idempotent — re-promoting the same thread returns
/// the existing task (never a duplicate, never re-imported). Imported comments carry the source
/// post seq in `origin_ref` so later bidirectional sync (slice 2) can dedup.
///
/// Adapter-agnostic: the same import+link core a GitHub-issue bridge (#136) reuses — a promoted
/// thread is just one internal source feeding it.
pub async fn promote_thread(
    pool: &Pool,
    channel_id: i64,
    root_post_seq: i64,
    project_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let source_kind = "channel_thread";
    let source_id = format!("channel:{channel_id}:{root_post_seq}");
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();

    // Idempotent: a thread maps to exactly one task. Re-promoting returns the existing task.
    if let Some(row) =
        sqlx::query("SELECT task_id FROM task_links WHERE source_kind=? AND source_id=?")
            .bind(source_kind)
            .bind(&source_id)
            .fetch_optional(&mut *tx)
            .await?
    {
        let tid: i64 = row.try_get("task_id")?;
        tx.commit().await?;
        return Ok(json!({
            "task_id": tid,
            "channel_id": channel_id,
            "root_post_seq": root_post_seq,
            "already_promoted": true,
        }));
    }

    if sqlx::query("SELECT 1 FROM projects WHERE id=?")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        anyhow::bail!("no project {project_id}");
    }

    // Load the channel's posts (root + candidate replies) in one pass.
    let posts = sqlx::query(
        "SELECT seq, data, created_at FROM events \
         WHERE channel_id=? AND type IN ('channel.post','message.direct') ORDER BY seq",
    )
    .bind(channel_id)
    .fetch_all(&mut *tx)
    .await?;
    let parse = |s: Option<String>| -> Value {
        s.as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}))
    };

    // The root post must exist in this channel.
    let Some(root) = posts
        .iter()
        .find(|r| r.try_get::<i64, _>("seq").ok() == Some(root_post_seq))
    else {
        anyhow::bail!("no post {root_post_seq} in channel {channel_id}");
    };
    let root_data = parse(root.try_get("data")?);
    let root_body = root_data["body"].as_str().unwrap_or("").to_string();
    let root_created: String = root.try_get("created_at")?;
    let title = thread_title(&root_body, channel_id);

    // Create the task: root body -> description, timestamped at the thread's root.
    let tid: i64 = sqlx::query(
        "INSERT INTO tasks(project_id, title, description, created_by, metadata, status, created_at, updated_at) \
         VALUES(?,?,?,?, '{}', 'todo', ?, ?) RETURNING id",
    )
    .bind(project_id)
    .bind(&title)
    .bind(&root_body)
    .bind(actor)
    .bind(&root_created)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    auto_subscribe(&mut tx, actor, tid).await?;

    // Record the link FIRST (idempotency anchor), atomic with the import below.
    sqlx::query(
        "INSERT INTO task_links(task_id, source_kind, source_id, metadata, created_at) VALUES(?,?,?,?,?)",
    )
    .bind(tid)
    .bind(source_kind)
    .bind(&source_id)
    .bind(json!({ "channel_id": channel_id, "root_post_seq": root_post_seq }).to_string())
    .bind(&ts)
    .execute(&mut *tx)
    .await?;

    // Import each DIRECT reply (one-level threading) as a comment, oldest first, preserving
    // author / external_author / timestamp, and stamping origin_ref with the source post seq.
    let mut imported = 0i64;
    for r in &posts {
        let seq: i64 = r.try_get("seq")?;
        if seq == root_post_seq {
            continue;
        }
        let data = parse(r.try_get("data")?);
        if data["reply_to"].as_i64() != Some(root_post_seq) {
            continue;
        }
        let created: String = r.try_get("created_at")?;
        sqlx::query(
            "INSERT INTO comments(task_id, author, body, created_at, external_author, origin_ref) \
             VALUES(?,?,?,?,?,?)",
        )
        .bind(tid)
        .bind(data["from"].as_str())
        .bind(data["body"].as_str().unwrap_or(""))
        .bind(&created)
        .bind(data["external_author"].as_str())
        .bind(seq.to_string())
        .execute(&mut *tx)
        .await?;
        imported += 1;
    }

    emit(
        &mut tx,
        &mut hooks,
        "task.created",
        actor,
        Some(tid),
        Some(project_id),
        None,
        None,
        json!({ "title": title, "promoted_from": source_id }),
        Recipients::FromTask,
    )
    .await?;
    emit(
        &mut tx,
        &mut hooks,
        "channel.thread_promoted",
        actor,
        Some(tid),
        Some(project_id),
        Some(channel_id),
        None,
        json!({ "channel_id": channel_id, "root_post_seq": root_post_seq, "task_id": tid, "imported_comments": imported }),
        Recipients::FromChannel(channel_id),
    )
    .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(json!({
        "task_id": tid,
        "channel_id": channel_id,
        "root_post_seq": root_post_seq,
        "imported_comments": imported,
        "already_promoted": false,
    }))
}

// --- Live thread<->task sync (design #141 §7, #151 slice 2) ---
//
// Loop prevention is structural: mirrors are created by DIRECT emit/insert here, never by calling
// the public post_to_channel / comment_task verbs, so a mirrored item never re-enters the mirror
// path. Genuine posts/comments flow through the public verbs (each mirrors exactly once); imported
// (promote_thread) and mirrored-in comments bypass comment_task, so they never echo back.

/// A new channel post that replies to a PROMOTED thread's root is mirrored into the linked task
/// as a comment (preserving from/external-author, timestamped now, `origin_ref` = the post seq).
/// Idempotent via comments.origin_ref. Called from post_to_channel for genuine posts only.
#[allow(clippy::too_many_arguments)]
async fn mirror_thread_reply_to_task(
    tx: &mut Transaction<'_, Sqlite>,
    hooks: &mut Vec<WebhookDelivery>,
    channel_id: i64,
    post_seq: i64,
    reply_to: Option<i64>,
    from: &str,
    body: &str,
    external_author: Option<&str>,
) -> anyhow::Result<()> {
    let Some(root_seq) = reply_to else {
        return Ok(());
    };
    let source_id = format!("channel:{channel_id}:{root_seq}");
    let task_id: Option<i64> = sqlx::query(
        "SELECT task_id FROM task_links WHERE source_kind='channel_thread' AND source_id=?",
    )
    .bind(&source_id)
    .fetch_optional(&mut **tx)
    .await?
    .map(|r| r.try_get("task_id"))
    .transpose()?;
    let Some(task_id) = task_id else {
        return Ok(());
    };
    let origin = post_seq.to_string();
    if sqlx::query("SELECT 1 FROM comments WHERE task_id=? AND origin_ref=?")
        .bind(task_id)
        .bind(&origin)
        .fetch_optional(&mut **tx)
        .await?
        .is_some()
    {
        return Ok(()); // already mirrored this post
    }
    let ts = now_iso();
    let cid: i64 = sqlx::query(
        "INSERT INTO comments(task_id, author, body, created_at, external_author, origin_ref) \
         VALUES(?,?,?,?,?,?) RETURNING id",
    )
    .bind(task_id)
    .bind(from)
    .bind(body)
    .bind(&ts)
    .bind(external_author)
    .bind(&origin)
    .fetch_one(&mut **tx)
    .await?
    .try_get("id")?;
    let mut data = json!({ "comment_id": cid, "body": body, "mirrored_from_post": post_seq });
    if let Some(ext) = external_author {
        data["external_author"] = json!(ext);
    }
    emit(
        tx,
        hooks,
        "task.commented",
        Some(from),
        Some(task_id),
        None,
        None,
        None,
        data,
        Recipients::FromTask,
    )
    .await?;
    Ok(())
}

/// A genuine task comment on a thread-linked task is mirrored back onto the thread as a channel
/// reply (reply_to = the thread root), carrying `origin_comment` for provenance. Emitted directly
/// as a channel.post (not via post_to_channel), so it does not re-trigger the reply→comment
/// mirror. Called from comment_task; imported/mirrored-in comments bypass comment_task entirely.
async fn mirror_task_comment_to_thread(
    tx: &mut Transaction<'_, Sqlite>,
    hooks: &mut Vec<WebhookDelivery>,
    task_id: i64,
    comment_id: i64,
    author: Option<&str>,
    body: &str,
    external_author: Option<&str>,
) -> anyhow::Result<()> {
    let row = sqlx::query(
        "SELECT metadata FROM task_links WHERE source_kind='channel_thread' AND task_id=? LIMIT 1",
    )
    .bind(task_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else { return Ok(()) };
    let meta: Value = row
        .try_get::<Option<String>, _>("metadata")?
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| json!({}));
    let (Some(channel_id), Some(root_seq)) =
        (meta["channel_id"].as_i64(), meta["root_post_seq"].as_i64())
    else {
        return Ok(());
    };
    let from = author.unwrap_or("anon");
    let mut data =
        json!({ "body": body, "from": from, "reply_to": root_seq, "origin_comment": comment_id });
    if let Some(ext) = external_author {
        data["external_author"] = json!(ext);
    }
    emit(
        tx,
        hooks,
        "channel.post",
        Some(from),
        None,
        None,
        Some(channel_id),
        None,
        data,
        Recipients::FromChannelThread(channel_id, Some(root_seq)),
    )
    .await?;
    sqlx::query("UPDATE channels SET updated_at=? WHERE id=?")
        .bind(now_iso())
        .bind(channel_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Read a channel's post backlog after `since_seq`, oldest first (a fresh joiner needs
/// history the inbox doesn't hold). Returns channel.post AND message.direct events for the
/// channel so DM channels read uniformly.
pub async fn get_channel_posts(
    pool: &Pool,
    channel_id: i64,
    since_seq: i64,
    before_seq: Option<i64>,
    limit: i64,
    desc: bool,
) -> anyhow::Result<Value> {
    // Only post events — a channel's event stream also carries channel.created / channel.invite
    // which aren't messages. DM channels post as message.direct, named ones as channel.post.
    //
    // Two read modes over the same window, mirroring get_events (#266):
    // - ascending (desc=false, default): oldest-first — scrollback and incremental pollers
    //   (advance `since_seq` to the max seq seen for the next page).
    // - descending (desc=true): newest-first, so `since_seq=0, limit=N` yields the LATEST N posts
    //   (a chat view; a plain ORDER BY seq LIMIT N returns the N OLDEST). For a "load earlier"
    //   button, pass `before_seq` = the oldest seq you already have to get the N posts just older.
    // `seq>since_seq` (lower bound) and `seq<before_seq` (optional upper bound) compose with either
    // order.
    let order = if desc { "DESC" } else { "ASC" };
    let before_clause = if before_seq.is_some() {
        "AND seq<?"
    } else {
        ""
    };
    let sql = format!(
        "SELECT seq, type, actor, channel_id, data, created_at FROM events \
         WHERE channel_id=? AND seq>? {before_clause} AND type IN ('channel.post','message.direct') \
         ORDER BY seq {order} LIMIT ?"
    );
    let mut q = sqlx::query(&sql).bind(channel_id).bind(since_seq);
    if let Some(b) = before_seq {
        q = q.bind(b);
    }
    let rows = q.bind(limit).fetch_all(pool).await?;
    let mut out = Vec::new();
    for r in &rows {
        let mut d = row_to_json(r);
        if let Value::Object(ref mut m) = d {
            let mut data: Value = m
                .get("data")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| json!({}));
            add_external_author_name(pool, &mut data).await;
            m.insert("data".into(), data);
        }
        out.push(d);
    }
    Ok(Value::Array(out))
}

/// Invite an agent into a channel: auto-join + notify (no accept/decline — LAN trust). The
/// invitee gets a `channel.invite` in their inbox and can unsubscribe to leave. Idempotent.
pub async fn invite_to_channel(
    pool: &Pool,
    channel_id: i64,
    agent_id: &str,
    invited_by: Option<&str>,
) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();

    let ch = sqlx::query("SELECT name FROM channels WHERE id=?")
        .bind(channel_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(ch) = ch else {
        anyhow::bail!("no channel {channel_id}");
    };
    let name: Option<String> = ch.try_get("name")?;

    join_channel(&mut tx, channel_id, agent_id).await?;
    // Deliver the invite explicitly to the invitee (they may not have been a member to hear
    // a FromChannel fan-out for their own join).
    let mut recips = BTreeSet::new();
    recips.insert(agent_id.to_string());
    emit(
        &mut tx,
        &mut hooks,
        "channel.invite",
        invited_by,
        None,
        None,
        Some(channel_id),
        None,
        json!({ "channel_id": channel_id, "name": name, "invited": agent_id, "invited_by": invited_by }),
        Recipients::Explicit(recips),
    )
    .await?;
    let out = channel_row_json(&mut tx, channel_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Get-or-create the private 1:1 DM channel for an unordered pair of agents. The pair is
/// keyed by `dm_key` (both ids sorted, NUL-joined) so A→B and B→A resolve to one channel.
/// Both agents are auto-joined. This is what lets DMs reuse the channel data model.
async fn dm_channel(tx: &mut Transaction<'_, Sqlite>, a: &str, b: &str) -> anyhow::Result<i64> {
    let mut pair = [a, b];
    pair.sort_unstable();
    let dm_key = format!("{}\u{0}{}", pair[0], pair[1]);

    if let Some(row) = sqlx::query("SELECT id FROM channels WHERE dm_key=?")
        .bind(&dm_key)
        .fetch_optional(&mut **tx)
        .await?
    {
        let id: i64 = row.try_get("id")?;
        // Ensure both are members (an id could have been removed; keep the invariant).
        join_channel(tx, id, a).await?;
        join_channel(tx, id, b).await?;
        return Ok(id);
    }

    let ts = now_iso();
    let name = format!("dm:{}\u{2194}{}", pair[0], pair[1]);
    let cid: i64 = sqlx::query(
        "INSERT INTO channels(name, topic, private, dm_key, metadata, created_by, created_at, updated_at) \
         VALUES(?,NULL,1,?,'{}',?,?,?) RETURNING id",
    )
    .bind(&name)
    .bind(&dm_key)
    .bind(a)
    .bind(&ts)
    .bind(&ts)
    .fetch_one(&mut **tx)
    .await?
    .try_get("id")?;
    join_channel(tx, cid, a).await?;
    join_channel(tx, cid, b).await?;
    Ok(cid)
}

// --- Notifications / direct messages ---

pub async fn check_notifications(
    pool: &Pool,
    agent_id: &str,
    mark_read: bool,
    limit: i64,
    kind: Option<&str>,
) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let mut q = String::from(
        "SELECT i.id inbox_id, e.seq, e.type, e.actor, e.project_id, e.task_id, e.data, e.created_at \
         FROM inbox i JOIN events e ON e.seq=i.event_seq \
         WHERE i.recipient=? AND i.read_at IS NULL",
    );
    if kind.is_some() {
        q.push_str(" AND e.type=?");
    }
    q.push_str(" ORDER BY e.seq LIMIT ?");

    let mut query = sqlx::query(&q).bind(agent_id);
    if let Some(k) = kind {
        query = query.bind(k);
    }
    query = query.bind(limit);
    let rows = query.fetch_all(&mut *tx).await?;

    let mut items = Vec::new();
    let mut ids: Vec<i64> = Vec::new();
    for r in &rows {
        let mut d = row_to_json(r);
        if let Value::Object(ref mut m) = d {
            let data: Value = m
                .get("data")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| json!({}));
            m.insert("data".into(), data);
            if let Some(inbox_id) = m.remove("inbox_id").and_then(|v| v.as_i64()) {
                ids.push(inbox_id);
            }
        }
        items.push(d);
    }

    if mark_read && !ids.is_empty() {
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!("UPDATE inbox SET read_at=? WHERE id IN ({placeholders})");
        let mut q = sqlx::query(&sql).bind(now_iso());
        for id in &ids {
            q = q.bind(id);
        }
        q.execute(&mut *tx).await?;
    }

    sqlx::query("UPDATE agents SET last_seen=? WHERE id=?")
        .bind(now_iso())
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    // Resolve external_author -> display name AFTER commit (the single-connection pool is free
    // again, so this can't deadlock), so a bridged post/comment in the inbox shows the human's
    // name while external_author stays the stable key.
    for item in &mut items {
        if let Some(data) = item.get_mut("data") {
            add_external_author_name(pool, data).await;
        }
    }

    Ok(json!({ "count": items.len(), "notifications": items }))
}

/// Send a direct message. A DM is just a post to the private 1:1 channel for the pair, so
/// there's one data model — but the wire behavior is unchanged: the post is emitted as a
/// `message.direct` event (post_to_channel derives that type for DM channels) delivered to
/// the recipient's inbox, and get_messages still reads it. The event now also carries the
/// pair's channel_id, so the conversation has a durable home a client can page through.
pub async fn send_message(
    pool: &Pool,
    from_agent: &str,
    to_agent: &str,
    body: &str,
) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let cid = dm_channel(&mut tx, from_agent, to_agent).await?;
    tx.commit().await?;
    // post_to_channel opens its own transaction; the DM channel is committed above so it's
    // visible. Emits message.direct to the recipient (FromChannel minus the sender).
    post_to_channel(pool, cid, from_agent, body, None, None).await?;
    Ok(json!({ "to": to_agent, "channel_id": cid, "delivered": true }))
}

/// Get (or create) the private 1:1 DM channel for a pair of agents, returning the channel with
/// its members. Idempotent and order-independent: the same pair always resolves to the same
/// channel (keyed on the sorted pair), created on first request. Lets a client open/link a DM
/// before any message is sent — send_message reuses this exact channel. Silent: resolving the
/// channel emits no event (a message.direct fires only when a post is actually sent).
pub async fn get_or_create_dm(pool: &Pool, a: &str, b: &str) -> anyhow::Result<Value> {
    if a == b {
        anyhow::bail!("give two distinct agents for a DM");
    }
    let mut tx = pool.begin().await?;
    let cid = dm_channel(&mut tx, a, b).await?;
    tx.commit().await?;
    get_channel(pool, cid).await
}

pub async fn get_messages(
    pool: &Pool,
    agent_id: &str,
    mark_read: bool,
    limit: i64,
) -> anyhow::Result<Value> {
    check_notifications(pool, agent_id, mark_read, limit, Some("message.direct")).await
}

pub async fn get_events(
    pool: &Pool,
    since_seq: i64,
    limit: i64,
    actor: Option<&str>,
    desc: bool,
) -> anyhow::Result<Value> {
    // Two read modes over the same `seq>since_seq [AND actor=?]` window:
    // - ascending (desc=false, the default): oldest-first — what an incremental poller wants
    //   (advance `since_seq` to the max seq it has seen and ask for the next page).
    // - descending (desc=true): newest-first, so `since_seq=0, limit=N` yields the LATEST N events
    //   overall — what a live activity feed wants (a plain `ORDER BY seq LIMIT N` returns the N
    //   OLDEST and never advances). The `seq>since_seq` lower bound still applies, so a feed can
    //   also ask for "the latest N above some floor".
    // Optional `actor` filter — a complete per-agent activity feed without over-fetching.
    let sql = match (actor.is_some(), desc) {
        (true, false) => "SELECT * FROM events WHERE seq>? AND actor=? ORDER BY seq ASC LIMIT ?",
        (true, true) => "SELECT * FROM events WHERE seq>? AND actor=? ORDER BY seq DESC LIMIT ?",
        (false, false) => "SELECT * FROM events WHERE seq>? ORDER BY seq ASC LIMIT ?",
        (false, true) => "SELECT * FROM events WHERE seq>? ORDER BY seq DESC LIMIT ?",
    };
    let mut q = sqlx::query(sql).bind(since_seq);
    if let Some(a) = actor {
        q = q.bind(a);
    }
    let rows = q.bind(limit).fetch_all(pool).await?;
    let mut out = Vec::new();
    for r in &rows {
        let mut d = row_to_json(r);
        if let Value::Object(ref mut m) = d {
            let mut data: Value = m
                .get("data")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| json!({}));
            add_external_author_name(pool, &mut data).await;
            m.insert("data".into(), data);
        }
        out.push(d);
    }
    Ok(Value::Array(out))
}

// --- External identities (bridged actors) ---

/// Register or update an external identity — a human/actor from a bridged system (Slack,
/// GitHub, ...), kept distinct from fleet `agents`. `id` is namespaced `source:handle` (e.g.
/// "slack:U123ABC"). Idempotent upsert: re-registering refreshes the display name / metadata
/// (metadata MERGED, like agents/projects) and bumps `updated_at`. Returns the stored record.
pub async fn upsert_external_identity(
    pool: &Pool,
    id: &str,
    source: &str,
    display_name: Option<&str>,
    metadata: Option<Value>,
) -> anyhow::Result<Value> {
    let id = id.trim();
    if id.is_empty() {
        anyhow::bail!(
            "give an `id` for the external identity (namespaced source:handle, e.g. slack:U123)"
        );
    }
    if source.trim().is_empty() {
        anyhow::bail!("give a `source` for the external identity (e.g. slack, github)");
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    // Merge metadata into any existing bag (mirrors register_agent / update_project).
    let existing: Option<String> =
        sqlx::query("SELECT metadata FROM external_identities WHERE id=?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .and_then(|r| r.try_get::<Option<String>, _>("metadata").ok().flatten());
    let mut meta: Map<String, Value> = existing
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    if let Some(Value::Object(incoming)) = metadata {
        meta.extend(incoming);
    }
    let meta_str = Value::Object(meta).to_string();
    sqlx::query(
        "INSERT INTO external_identities(id, source, display_name, metadata, created_at, updated_at) \
         VALUES(?,?,?,?,?,?) \
         ON CONFLICT(id) DO UPDATE SET \
            source=excluded.source, \
            display_name=COALESCE(excluded.display_name, external_identities.display_name), \
            metadata=excluded.metadata, \
            updated_at=excluded.updated_at",
    )
    .bind(id)
    .bind(source.trim())
    .bind(display_name)
    .bind(&meta_str)
    .bind(&ts)
    .bind(&ts)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    get_external_identity(pool, id).await
}

/// Fetch one external identity by id (with its metadata bag parsed). Null if unknown.
pub async fn get_external_identity(pool: &Pool, id: &str) -> anyhow::Result<Value> {
    let row = sqlx::query("SELECT * FROM external_identities WHERE id=?")
        .bind(id.trim())
        .fetch_optional(pool)
        .await?;
    Ok(match row {
        Some(r) => hydrate_external_identity(&r),
        None => Value::Null,
    })
}

/// List external identities, optionally filtered by `source`, newest-updated first.
pub async fn list_external_identities(pool: &Pool, source: Option<&str>) -> anyhow::Result<Value> {
    let rows = match source {
        Some(src) => {
            sqlx::query("SELECT * FROM external_identities WHERE source=? ORDER BY updated_at DESC")
                .bind(src)
                .fetch_all(pool)
                .await?
        }
        None => {
            sqlx::query("SELECT * FROM external_identities ORDER BY updated_at DESC")
                .fetch_all(pool)
                .await?
        }
    };
    Ok(Value::Array(
        rows.iter().map(hydrate_external_identity).collect(),
    ))
}

/// Row -> JSON with the `metadata` TEXT column parsed into an object (like other hydrators).
fn hydrate_external_identity(r: &SqliteRow) -> Value {
    let mut v = row_to_json(r);
    if let Value::Object(ref mut m) = v {
        let meta: Value = m
            .get("metadata")
            .and_then(|x| x.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));
        m.insert("metadata".into(), meta);
    }
    v
}

// --- Workspace kinds (named env setup stored as board data; fleet spin-up consumes them) ---

/// Create or update a workspace kind — a named workspace definition (a `setup_script` plus a
/// `config` bag an agent is configured with). Environment-specific setup lives here as board
/// data, so fleet spin-up supports custom environment kinds defined in board resources. Idempotent
/// on `name`: an omitted `setup_script`/`description` keeps the stored value, `config` is MERGED,
/// and `created_at`/`created_by` are preserved; `updated_at` is bumped. Returns the stored record.
pub async fn set_workspace_kind(
    pool: &Pool,
    name: &str,
    setup_script: Option<&str>,
    config: Option<Value>,
    description: Option<&str>,
    created_by: Option<&str>,
) -> anyhow::Result<Value> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("give a `name` for the workspace kind");
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let existing = sqlx::query(
        "SELECT setup_script, config, description, created_by, created_at FROM workspace_kinds WHERE name=?",
    )
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?;
    // Merge config into any existing bag (mirrors the other upserts).
    let mut cfg: Map<String, Value> = existing
        .as_ref()
        .and_then(|r| r.try_get::<Option<String>, _>("config").ok().flatten())
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    if let Some(Value::Object(incoming)) = config {
        cfg.extend(incoming);
    }
    let cfg_str = Value::Object(cfg).to_string();
    // Keep the stored script/description/creator/created_at when this call omits them.
    let final_script = setup_script
        .map(str::to_string)
        .or_else(|| {
            existing.as_ref().and_then(|r| {
                r.try_get::<Option<String>, _>("setup_script")
                    .ok()
                    .flatten()
            })
        })
        .unwrap_or_default();
    let final_desc = description.map(str::to_string).or_else(|| {
        existing
            .as_ref()
            .and_then(|r| r.try_get::<Option<String>, _>("description").ok().flatten())
    });
    let final_creator = existing
        .as_ref()
        .and_then(|r| r.try_get::<Option<String>, _>("created_by").ok().flatten())
        .or_else(|| created_by.map(str::to_string));
    let created_at = existing
        .as_ref()
        .and_then(|r| r.try_get::<Option<String>, _>("created_at").ok().flatten())
        .unwrap_or_else(|| ts.clone());
    sqlx::query(
        "INSERT INTO workspace_kinds(name, setup_script, config, description, created_by, created_at, updated_at) \
         VALUES(?,?,?,?,?,?,?) \
         ON CONFLICT(name) DO UPDATE SET \
            setup_script=excluded.setup_script, config=excluded.config, \
            description=excluded.description, updated_at=excluded.updated_at",
    )
    .bind(name)
    .bind(&final_script)
    .bind(&cfg_str)
    .bind(&final_desc)
    .bind(&final_creator)
    .bind(&created_at)
    .bind(&ts)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    get_workspace_kind(pool, name).await
}

/// Fetch one workspace kind by name (with `config` parsed). Null if unknown. This is what fleet
/// spin-up reads to materialize an agent's workspace.
pub async fn get_workspace_kind(pool: &Pool, name: &str) -> anyhow::Result<Value> {
    let row = sqlx::query("SELECT * FROM workspace_kinds WHERE name=?")
        .bind(name.trim())
        .fetch_optional(pool)
        .await?;
    Ok(match row {
        Some(r) => hydrate_workspace_kind(&r),
        None => Value::Null,
    })
}

/// List all workspace kinds, ordered by name.
pub async fn list_workspace_kinds(pool: &Pool) -> anyhow::Result<Value> {
    let rows = sqlx::query("SELECT * FROM workspace_kinds ORDER BY name")
        .fetch_all(pool)
        .await?;
    Ok(Value::Array(
        rows.iter().map(hydrate_workspace_kind).collect(),
    ))
}

/// Retire a workspace kind. Returns `{name, deleted}` (deleted=false if it didn't exist).
pub async fn delete_workspace_kind(pool: &Pool, name: &str) -> anyhow::Result<Value> {
    let name = name.trim();
    let res = sqlx::query("DELETE FROM workspace_kinds WHERE name=?")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(json!({ "name": name, "deleted": res.rows_affected() > 0 }))
}

/// Row -> JSON with the `config` TEXT column parsed into an object (like other hydrators).
fn hydrate_workspace_kind(r: &SqliteRow) -> Value {
    let mut v = row_to_json(r);
    if let Value::Object(ref mut m) = v {
        let cfg: Value = m
            .get("config")
            .and_then(|x| x.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));
        m.insert("config".into(), cfg);
    }
    v
}

// --- Banned phrases (data-driven pre-submit content lint for docs + comments) ---

/// Add (or update the note on) a banned phrase. Stored trimmed + lowercased so matching is
/// case-insensitive. Idempotent on the phrase. Returns the stored record.
pub async fn add_banned_phrase(
    pool: &Pool,
    phrase: &str,
    note: Option<&str>,
    created_by: Option<&str>,
) -> anyhow::Result<Value> {
    let p = phrase.trim().to_lowercase();
    if p.is_empty() {
        anyhow::bail!("give a non-empty phrase to ban");
    }
    sqlx::query(
        "INSERT INTO banned_phrases(phrase, note, created_by, created_at) VALUES(?,?,?,?) \
         ON CONFLICT(phrase) DO UPDATE SET note=excluded.note",
    )
    .bind(&p)
    .bind(note)
    .bind(created_by)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(json!({ "phrase": p, "note": note, "created_by": created_by }))
}

/// The maintained banned-phrases list, alphabetical.
pub async fn list_banned_phrases(pool: &Pool) -> anyhow::Result<Value> {
    let rows = sqlx::query("SELECT * FROM banned_phrases ORDER BY phrase")
        .fetch_all(pool)
        .await?;
    Ok(Value::Array(rows.iter().map(row_to_json).collect()))
}

/// Remove a banned phrase. Returns `{phrase, deleted}` (deleted=false if it wasn't listed).
pub async fn remove_banned_phrase(pool: &Pool, phrase: &str) -> anyhow::Result<Value> {
    let p = phrase.trim().to_lowercase();
    let res = sqlx::query("DELETE FROM banned_phrases WHERE phrase=?")
        .bind(&p)
        .execute(pool)
        .await?;
    Ok(json!({ "phrase": p, "deleted": res.rows_affected() > 0 }))
}

/// Whether `needle` occurs in `haystack` as a whole phrase — bounded by a non-alphanumeric
/// character (or the string ends) on each side, so "the floor" does not match inside "the
/// floorboard". Both arguments must already be lowercased by the caller.
fn contains_whole_phrase(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let idx = start + pos;
        let end = idx + needle.len();
        let before_ok = idx == 0
            || !haystack[..idx]
                .chars()
                .next_back()
                .map(|c| c.is_alphanumeric())
                .unwrap_or(false);
        let after_ok = end == haystack.len()
            || !haystack[end..]
                .chars()
                .next()
                .map(|c| c.is_alphanumeric())
                .unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

/// Scan `text` against the banned-phrases list. Returns the matched phrases (case-insensitive,
/// whole-phrase), alphabetical. Empty when the list is empty or nothing matches.
pub async fn scan_banned_phrases(pool: &Pool, text: &str) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query("SELECT phrase FROM banned_phrases ORDER BY phrase")
        .fetch_all(pool)
        .await?;
    let hay = text.to_lowercase();
    let mut hits = Vec::new();
    for r in rows {
        let phrase: String = r.try_get("phrase")?;
        if contains_whole_phrase(&hay, &phrase) {
            hits.push(phrase);
        }
    }
    Ok(hits)
}

/// Pre-submit lint: bail with a clear, author-facing message if `text` contains any banned phrase
/// and the author has not acknowledged. `acknowledge=true` is the soft-block escape hatch (submit
/// anyway) — for intentional uses, e.g. content that quotes a banned phrase to discuss it. The
/// error message starts with "banned phrase" so the REST layer maps it to 400.
pub async fn check_banned_phrases(
    pool: &Pool,
    text: &str,
    acknowledge: bool,
) -> anyhow::Result<()> {
    if acknowledge {
        return Ok(());
    }
    let hits = scan_banned_phrases(pool, text).await?;
    if hits.is_empty() {
        return Ok(());
    }
    let list = hits
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "banned phrase(s) found: {list}. These are on the fleet banned-phrases list — that is not \
         how we write here. Rewrite to remove them, or pass acknowledge_banned=true to submit anyway."
    );
}

/// Pre-submit FORMAT lint (task 368, the ASCII-only ruling): bail if `text` contains any non-ASCII
/// character (codepoint > U+007F) — em dashes, curly quotes, arrows, emoji, and the like — unless
/// the author acknowledged. Reports the FIRST offending character with its codepoint and 1-based
/// line:column so the author can find and fix it. This is a distinct pass from the banned-phrase
/// matcher (a format rule, not a wording rule). The message starts with "non-ASCII" so the REST
/// layer maps it to 400.
pub fn check_non_ascii(text: &str, acknowledge: bool) -> anyhow::Result<()> {
    if acknowledge {
        return Ok(());
    }
    if let Some(&(ch, line, col)) = scan_non_ascii(text).first() {
        anyhow::bail!(
            "non-ASCII character {ch:?} (U+{:04X}) at line {line}, column {col}. Board content \
             must be ASCII — replace it (an em dash with '-', curly quotes with straight quotes, \
             an arrow with '<->', drop emoji), or pass acknowledge_banned=true to submit anyway.",
            ch as u32
        );
    }
    Ok(())
}

/// Scan `text` for EVERY non-ASCII character, returning `(char, line, column)` (1-based) for each.
/// The report-all counterpart to `check_non_ascii` (which bails on the first) — a dry-run lint
/// wants the full list, not just the earliest offender. Empty when the text is all ASCII.
pub fn scan_non_ascii(text: &str) -> Vec<(char, usize, usize)> {
    let (mut line, mut col) = (1usize, 1usize);
    let mut out = Vec::new();
    for ch in text.chars() {
        if ch == '\n' {
            line += 1;
            col = 1;
            continue;
        }
        if !ch.is_ascii() {
            out.push((ch, line, col));
        }
        col += 1;
    }
    out
}

/// Combined pre-submit content lint for authored free text (task/document comments + document
/// versions): the ASCII-format check then the banned-phrase check, both honoring the same
/// `acknowledge` escape hatch. One funnel so every authored surface runs the same checks and a new
/// check only has to be added here.
pub async fn check_content(pool: &Pool, text: &str, acknowledge: bool) -> anyhow::Result<()> {
    check_non_ascii(text, acknowledge)?;
    check_banned_phrases(pool, text, acknowledge).await?;
    Ok(())
}

/// Content-gate a document version published BY CID (task 564). A publish-by-CID carries no inline
/// content, so the caller's inline `check_content` never sees the bytes -- this fetches them from
/// the IPFS backend and runs the SAME gate, closing that bypass so a CID publish cannot smuggle
/// banned phrases / non-ASCII past the lint. No-op when the author acknowledged, when there is no
/// backend to fetch with (a pointer-only board cannot gate bytes it never resolves), or when the
/// content is not text-shaped (the checks only apply to text). Fail-closed on a configured backend:
/// if the bytes cannot be fetched we reject with an actionable message rather than silently skipping
/// the gate -- the author can pin/reach the CID, or pass acknowledge_banned to publish without it.
pub async fn check_cid_content(
    pool: &Pool,
    ipfs_api_url: Option<&str>,
    cid: &str,
    content_type: &str,
    acknowledge: bool,
) -> anyhow::Result<()> {
    if acknowledge {
        return Ok(());
    }
    let Some(url) = ipfs_api_url else {
        return Ok(());
    };
    if !is_text_content_type(content_type) {
        return Ok(());
    }
    let bytes = crate::ipfs::cat(url, cid, DOCUMENT_READ_CAP_BYTES)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
            "could not fetch CID {cid} to content-scan it before publishing ({e}); ensure it is \
             pinned/reachable on the board's IPFS, or pass acknowledge_banned=true to publish \
             without the content scan"
        )
        })?;
    if let Ok(text) = String::from_utf8(bytes) {
        check_content(pool, &text, acknowledge).await?;
    }
    Ok(())
}

/// Non-bailing dry-run lint (task 558): run the SAME authoritative checks as `check_content`
/// (ASCII-format + banned-phrase) over `text` and return a structured report of EVERY finding
/// instead of failing on the first. This is the source of truth for pre-publish checks so authors
/// verify against the live banned-phrases list + ASCII rule rather than a hand-maintained local
/// copy that drifts (and that a CID publish would otherwise skip entirely). Returns
/// `{clean, banned_phrases:[..], non_ascii:[{char, codepoint, line, column}, ..]}`.
pub async fn lint_text(pool: &Pool, text: &str) -> anyhow::Result<Value> {
    let banned = scan_banned_phrases(pool, text).await?;
    let non_ascii: Vec<Value> = scan_non_ascii(text)
        .into_iter()
        .map(|(ch, line, column)| {
            json!({
                "char": ch.to_string(),
                "codepoint": format!("U+{:04X}", ch as u32),
                "line": line,
                "column": column,
            })
        })
        .collect();
    // The same ambiguous bare-"#N" check the write path hard-rejects (check_bare_refs), surfaced
    // here so lint_text is a complete PRE-SEND lint (task 616): an agent (or a client wrapping its
    // board MCP calls) can lint a composed body before the write and fix a bare ref with no server
    // round-trip + lost-body recompose. Each hit carries the ready-to-paste typed forms.
    let bare_refs: Vec<Value> = detect_bare_task_refs(&strip_code_regions(text))
        .into_iter()
        .map(|n| {
            json!({
                "ref": format!("#{n}"),
                "suggestions": [
                    format!("task_{n}"),
                    format!("camshaft/task-board#{n}"),
                    format!("{n} (drop the # if it is a plain ordinal, e.g. a board message or sequence number)"),
                ],
            })
        })
        .collect();
    Ok(json!({
        "clean": banned.is_empty() && non_ascii.is_empty() && bare_refs.is_empty(),
        "banned_phrases": banned,
        "non_ascii": non_ascii,
        "bare_refs": bare_refs,
    }))
}

// --- Design-doc conformance grading (doc_7 A8, task 625 / task 622) ------------------------------
//
// grade_document runs the mechanical half of the design-doc conformance rubric and returns
// structured findings. The board is the single source of truth (operator steer, task_625): the
// board's own submit path and any client (fleet check-doc, the task_374 reviewer) call this one
// implementation behind the HTTP boundary -- no parallel fleet-side checker that could disagree.
//
// The check SET derives from and cites doc_7 appendix A8 (reference spec: task_625 comment 2614);
// the tunable A8 parameters are the named consts below, so a later A8 revision is a one-line change.
// Severities: "hard_fail" (mature, low-false-positive) vs "warn" (promote once the FP rate is proven
// low on real docs). One fuzzy A8 sub-check -- the opening-paragraph-reads-as-a-whole-doc-summary
// heuristic -- is intentionally NOT implemented yet: it needs real-doc FP tuning and has no reliable
// deterministic form; every other A8 check is deterministic and implemented here.

/// The required design-doc H2 sections, in order (doc_7 A8). The main body must contain exactly
/// these before "## Appendix". Tunable: update here if A8 refines.
const REQUIRED_SECTIONS: &[&str] = &[
    "Background",
    "Problem Statement",
    "Requirements / Goals / Non-Goals",
    "Solutions",
    "Recommendation",
];
/// Title/heading length ceiling (doc_7 A8): a title or heading must be under this many characters.
const HEADING_MAX_LEN: usize = 60;
/// Default main-body prose budget in words (doc_7 A8; board-pm's locked basis, task_625 comment 2543).
const DEFAULT_BODY_BUDGET_WORDS: i64 = 700;
/// Line-leading status/provenance/placeholder markers that do not belong in a doc body (doc_7 A8 #6).
const PROVENANCE_PREFIXES: &[&str] = &[
    "draft",
    "status:",
    "written by",
    "fact-checked by",
    "fact checked by",
    "published:",
    "not yet published",
    "not yet fact-checked",
    "not yet fact checked",
    "todo",
    "tbd",
    "placeholder",
];

fn gd_finding(check: &str, severity: &str, line: Option<i64>, message: String) -> Value {
    json!({ "check": check, "severity": severity, "line": line, "message": message })
}

/// An ASCII replacement hint for a non-ASCII char (doc_7 A8 #1 names the fix, not just the fault).
fn ascii_replacement(ch: char) -> &'static str {
    match ch {
        '\u{2014}' | '\u{2013}' => "'-' (a hyphen)",
        '\u{2018}' | '\u{2019}' => "a straight apostrophe '",
        '\u{201C}' | '\u{201D}' => "a straight quote \"",
        '\u{2026}' => "'...' (three dots)",
        '\u{00A0}' => "a normal space",
        '\u{2192}' | '\u{2190}' | '\u{2194}' => "an ASCII arrow like -> / <- / <->",
        _ => "an ASCII equivalent (or drop it)",
    }
}

/// Collapse internal runs of whitespace to a single space and trim, for stable heading matching.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A markdown table separator row, e.g. `|---|---|` or `:--- | ---:` (the reliable table signal).
fn is_table_separator(l: &str) -> bool {
    let s: String = l.chars().filter(|c| !c.is_whitespace()).collect();
    s.contains('|') && s.contains('-') && s.chars().all(|c| c == '|' || c == '-' || c == ':')
}

/// True if the line contains a markdown image `![alt](url)`.
fn has_markdown_image(l: &str) -> bool {
    l.find("![").map(|i| l[i..].contains("](")).unwrap_or(false)
}

/// Strip a leading list marker (`-`, `*`, `+`, or `N.`) from an already-trimmed line.
fn strip_list_marker(s: &str) -> &str {
    let t = s.trim_start();
    for m in ["- ", "* ", "+ "] {
        if let Some(r) = t.strip_prefix(m) {
            return r.trim_start();
        }
    }
    // ordered list: leading digits then '.' or ')'
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let after = &t[digits.len()..];
        if let Some(r) = after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))
        {
            return r.trim_start();
        }
    }
    t
}

/// Remove markdown link URLs, keeping the link text: `[text](url)` -> `text` (doc_7 A8 #8 counts
/// link text, not the URL). A pragmatic pass -- good enough for the warn-level word budget.
fn remove_link_urls(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    loop {
        match rest.find("](") {
            Some(pos) => {
                out.push_str(&rest[..pos]); // keeps "[text" (brackets stripped below)
                let after = &rest[pos + 2..];
                match after.find(')') {
                    Some(close) => rest = &after[close + 1..],
                    None => {
                        break;
                    }
                }
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out.replace(['[', ']', '!'], " ")
}

/// Count prose words on one main-body line: strip heading/list markers and link URLs, then count
/// whitespace tokens that carry a letter/digit and are not a bare URL (doc_7 A8 #8).
fn count_prose_words(line: &str) -> i64 {
    let s = line.trim();
    let s = s.trim_start_matches('#').trim_start();
    let s = strip_list_marker(s);
    remove_link_urls(s)
        .split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_alphanumeric()) && !w.starts_with("http"))
        .count() as i64
}

/// Grade a design document against the mechanical doc_7 A8 rubric. `title` is graded separately from
/// `content` (the Document title is its own field, A1). Returns
/// `{clean, has_hard_fail, findings:[{check, severity, line, message}]}`.
pub async fn grade_document(
    pool: &Pool,
    content: &str,
    title: &str,
    body_length_budget_words: Option<i64>,
) -> anyhow::Result<Value> {
    let mut findings: Vec<Value> = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    // Fenced-code map: a line inside (or on the fence of) a ``` / ~~~ block is excluded from the
    // prose/heading/marker checks.
    let mut in_code = vec![false; lines.len()];
    let mut fenced = false;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_code[i] = true;
            fenced = !fenced;
        } else {
            in_code[i] = fenced;
        }
    }

    // Headings (level + text + 1-based line), skipping fenced code.
    struct H {
        line: usize,
        level: usize,
        text: String,
    }
    let mut headings: Vec<H> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if in_code[i] {
            continue;
        }
        let t = l.trim_start();
        if t.starts_with('#') {
            let level = t.chars().take_while(|&c| c == '#').count();
            let text = t[level..].trim().to_string();
            headings.push(H {
                line: i + 1,
                level,
                text,
            });
        }
    }
    // The main body ends at the first "Appendix" heading (any level), if present.
    let bound = headings
        .iter()
        .find(|h| h.text.eq_ignore_ascii_case("Appendix"))
        .map(|h| h.line)
        .unwrap_or(usize::MAX);

    // 1. ascii-only (hard-fail): every non-ASCII char, with the ASCII replacement.
    for (ch, line, _col) in scan_non_ascii(content) {
        findings.push(gd_finding(
            "ascii-only",
            "hard_fail",
            Some(line as i64),
            format!(
                "non-ASCII character {ch:?} (U+{:04X}); replace it with {}",
                ch as u32,
                ascii_replacement(ch)
            ),
        ));
    }

    // 2. required-sections (hard-fail): exactly the required H2s, in order, before "## Appendix".
    let required_norm: Vec<String> = REQUIRED_SECTIONS.iter().map(|s| normalize_ws(s)).collect();
    let body_h2: Vec<&H> = headings
        .iter()
        .filter(|h| h.level == 2 && h.line < bound)
        .collect();
    let body_h2_norm: Vec<String> = body_h2.iter().map(|h| normalize_ws(&h.text)).collect();
    for (req, req_disp) in required_norm.iter().zip(REQUIRED_SECTIONS.iter()) {
        if !body_h2_norm.iter().any(|t| t.eq_ignore_ascii_case(req)) {
            findings.push(gd_finding(
                "required-sections",
                "hard_fail",
                None,
                format!(
                    "missing required section '## {req_disp}'; the main body must contain these H2 sections in order: {}",
                    REQUIRED_SECTIONS.join(", ")
                ),
            ));
        }
    }
    // Order: the required sections that ARE present must appear in canonical order.
    let present_idxs: Vec<usize> = body_h2_norm
        .iter()
        .filter_map(|t| required_norm.iter().position(|r| r.eq_ignore_ascii_case(t)))
        .collect();
    if present_idxs.windows(2).any(|w| w[0] >= w[1]) {
        findings.push(gd_finding(
            "required-sections",
            "hard_fail",
            None,
            format!(
                "required sections are out of order; they must appear as: {}",
                REQUIRED_SECTIONS.join(", ")
            ),
        ));
    }
    // Extra (non-required) H2 sections before the Appendix.
    for h in &body_h2 {
        let nt = normalize_ws(&h.text);
        if !required_norm.iter().any(|r| r.eq_ignore_ascii_case(&nt)) {
            findings.push(gd_finding(
                "required-sections",
                "hard_fail",
                Some(h.line as i64),
                format!(
                    "unexpected section '## {}' before '## Appendix'; the main body may contain only the required sections ({})",
                    h.text,
                    REQUIRED_SECTIONS.join(", ")
                ),
            ));
        }
    }

    // 3. banned-phrases (hard-fail): reuse the authoritative live scanner.
    for p in scan_banned_phrases(pool, content).await? {
        findings.push(gd_finding(
            "banned-phrases",
            "hard_fail",
            None,
            format!("banned phrase \"{p}\" found; rewrite to remove it (it is on the fleet banned-phrases list)"),
        ));
    }

    // 4. title/heading rules.
    let title_chars = title.chars().count();
    if title_chars >= HEADING_MAX_LEN {
        findings.push(gd_finding(
            "title-heading-rules",
            "hard_fail",
            None,
            format!("the title is {title_chars} characters; keep it under {HEADING_MAX_LEN} -- shorten it"),
        ));
    }
    let title_norm = normalize_ws(title);
    for h in &headings {
        let hlen = h.text.chars().count();
        if hlen >= HEADING_MAX_LEN {
            findings.push(gd_finding(
                "title-heading-rules",
                "hard_fail",
                Some(h.line as i64),
                format!(
                    "heading '{}' is {hlen} characters; keep headings under {HEADING_MAX_LEN}",
                    h.text
                ),
            ));
        }
        if !title_norm.is_empty() && normalize_ws(&h.text).eq_ignore_ascii_case(&title_norm) {
            findings.push(gd_finding(
                "title-heading-rules",
                "hard_fail",
                Some(h.line as i64),
                "the document title is repeated as a heading in the body; the title lives in the Document title field -- remove the in-body repetition".to_string(),
            ));
        }
        if matches!(h.text.chars().last(), Some('.') | Some('!') | Some('?')) {
            findings.push(gd_finding(
                "title-heading-rules",
                "warn",
                Some(h.line as i64),
                format!("heading '{}' reads as a sentence (ends with punctuation); headings should be short labels", h.text),
            ));
        }
    }

    // 5. body-hygiene (hard-fail): no tables or images in the main body (before the Appendix).
    for (i, l) in lines.iter().enumerate() {
        let line1 = i + 1;
        if line1 >= bound || in_code[i] {
            continue;
        }
        if is_table_separator(l) {
            findings.push(gd_finding(
                "body-hygiene",
                "hard_fail",
                Some(line1 as i64),
                "a markdown table appears in the main body; move tabular detail to the Appendix"
                    .to_string(),
            ));
        }
        if has_markdown_image(l) {
            findings.push(gd_finding(
                "body-hygiene",
                "hard_fail",
                Some(line1 as i64),
                "an image appears in the main body; move images to the Appendix".to_string(),
            ));
        }
    }

    // 6. status-provenance (warn): a line LEADING with a status/provenance/placeholder marker.
    for (i, l) in lines.iter().enumerate() {
        if in_code[i] {
            continue;
        }
        let lower = l.trim_start().to_lowercase();
        if PROVENANCE_PREFIXES.iter().any(|p| lower.starts_with(p)) {
            findings.push(gd_finding(
                "status-provenance",
                "warn",
                Some((i + 1) as i64),
                format!("line {} leads with a status/provenance/placeholder marker; drop editorial lines from the doc body", i + 1),
            ));
        }
    }

    // 7. caps-emphasis (warn): an all-caps word (4+ letters) that ALSO appears lowercase elsewhere
    // (so a genuine acronym, which never appears lowercase, is not flagged).
    let mut lower_words: BTreeSet<String> = BTreeSet::new();
    let mut caps_tokens: Vec<(String, usize)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if in_code[i] {
            continue;
        }
        for tok in l
            .split(|c: char| !c.is_ascii_alphabetic())
            .filter(|t| t.len() >= 4)
        {
            if tok.chars().all(|c| c.is_ascii_uppercase()) {
                caps_tokens.push((tok.to_string(), i + 1));
            } else if tok.chars().any(|c| c.is_ascii_lowercase()) {
                lower_words.insert(tok.to_lowercase());
            }
        }
    }
    let mut flagged: BTreeSet<String> = BTreeSet::new();
    for (tok, line) in &caps_tokens {
        if lower_words.contains(&tok.to_lowercase()) && flagged.insert(tok.clone()) {
            findings.push(gd_finding(
                "caps-emphasis",
                "warn",
                Some(*line as i64),
                format!("'{tok}' looks capitalized for emphasis (it also appears in lowercase); use normal case or markdown emphasis"),
            ));
        }
    }

    // 8. body-length (warn): prose words from the top to the first Appendix heading, over budget.
    let budget = body_length_budget_words.unwrap_or(DEFAULT_BODY_BUDGET_WORDS);
    let mut words = 0i64;
    for (i, l) in lines.iter().enumerate() {
        if i + 1 >= bound {
            break;
        }
        if in_code[i] {
            continue;
        }
        words += count_prose_words(l);
    }
    if words > budget {
        findings.push(gd_finding(
            "body-length",
            "warn",
            None,
            format!("the main body is about {words} prose words, over the ~{budget}-word budget; tighten it or move detail to the Appendix"),
        ));
    }

    let has_hard_fail = findings.iter().any(|f| f["severity"] == "hard_fail");
    Ok(json!({
        "clean": findings.is_empty(),
        "has_hard_fail": has_hard_fail,
        "findings": findings,
    }))
}

// --- External links (bridged mappings: channel-map, issue↔task, thread↔task) ---

/// The board entity kinds an external link may target.
const EXTERNAL_LINK_KINDS: &[&str] = &["channel", "task", "thread", "comment", "document"];

/// Create or update a mapping between a board entity and an external one — the generic link
/// behind the Slack channel-map, the GitHub issue↔task bridge, and thread promotion. Idempotent
/// on (source, external_id): re-linking the same external entity updates its board target /
/// parent / metadata (metadata MERGED) and bumps `updated_at`. Returns the stored record.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_external_link(
    pool: &Pool,
    source: &str,
    external_id: &str,
    external_parent_id: Option<&str>,
    board_kind: &str,
    board_id: i64,
    metadata: Option<Value>,
) -> anyhow::Result<Value> {
    let source = source.trim();
    let external_id = external_id.trim();
    if source.is_empty() {
        anyhow::bail!("give a `source` for the external link (e.g. slack, github)");
    }
    if external_id.is_empty() {
        anyhow::bail!("give an `external_id` for the external link (the external system's key)");
    }
    if !EXTERNAL_LINK_KINDS.contains(&board_kind) {
        anyhow::bail!("give a `board_kind` of one of: channel, task, thread, comment, document");
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    // A clean 404 for the kinds backed by a real table (thread = a channel post seq, skipped).
    match board_kind {
        "channel" => {
            if sqlx::query("SELECT 1 FROM channels WHERE id=?")
                .bind(board_id)
                .fetch_optional(&mut *tx)
                .await?
                .is_none()
            {
                anyhow::bail!("no channel {board_id}");
            }
        }
        "task" => {
            if sqlx::query("SELECT 1 FROM tasks WHERE id=?")
                .bind(board_id)
                .fetch_optional(&mut *tx)
                .await?
                .is_none()
            {
                anyhow::bail!("no task {board_id}");
            }
        }
        "comment" => {
            if sqlx::query("SELECT 1 FROM comments WHERE id=?")
                .bind(board_id)
                .fetch_optional(&mut *tx)
                .await?
                .is_none()
            {
                anyhow::bail!("no comment {board_id}");
            }
        }
        "document" => {
            if sqlx::query("SELECT 1 FROM documents WHERE id=?")
                .bind(board_id)
                .fetch_optional(&mut *tx)
                .await?
                .is_none()
            {
                anyhow::bail!("no document {board_id}");
            }
        }
        _ => {}
    }
    // Merge metadata into any existing bag (mirrors the other upserts).
    let existing: Option<String> =
        sqlx::query("SELECT metadata FROM external_links WHERE source=? AND external_id=?")
            .bind(source)
            .bind(external_id)
            .fetch_optional(&mut *tx)
            .await?
            .and_then(|r| r.try_get::<Option<String>, _>("metadata").ok().flatten());
    let mut meta: Map<String, Value> = existing
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    if let Some(Value::Object(incoming)) = metadata {
        meta.extend(incoming);
    }
    let meta_str = Value::Object(meta).to_string();
    sqlx::query(
        "INSERT INTO external_links(source, external_id, external_parent_id, board_kind, board_id, metadata, created_at, updated_at) \
         VALUES(?,?,?,?,?,?,?,?) \
         ON CONFLICT(source, external_id) DO UPDATE SET \
            external_parent_id=excluded.external_parent_id, \
            board_kind=excluded.board_kind, \
            board_id=excluded.board_id, \
            metadata=excluded.metadata, \
            updated_at=excluded.updated_at",
    )
    .bind(source)
    .bind(external_id)
    .bind(external_parent_id)
    .bind(board_kind)
    .bind(board_id)
    .bind(&meta_str)
    .bind(&ts)
    .bind(&ts)
    .execute(&mut *tx)
    .await?;
    let out = sqlx::query("SELECT * FROM external_links WHERE source=? AND external_id=?")
        .bind(source)
        .bind(external_id)
        .fetch_one(&mut *tx)
        .await?;
    let out = hydrate_external_link(&out);
    tx.commit().await?;
    Ok(out)
}

/// List external links, filtered by any combination of `source`, `board_kind`, and `board_id`
/// (the read path a bridge adapter uses to resolve a board entity to its external counterpart).
pub async fn list_external_links(
    pool: &Pool,
    source: Option<&str>,
    board_kind: Option<&str>,
    board_id: Option<i64>,
) -> anyhow::Result<Value> {
    // Build a filtered query with only the provided predicates (all optional).
    let mut sql = String::from("SELECT * FROM external_links WHERE 1=1");
    if source.is_some() {
        sql.push_str(" AND source=?");
    }
    if board_kind.is_some() {
        sql.push_str(" AND board_kind=?");
    }
    if board_id.is_some() {
        sql.push_str(" AND board_id=?");
    }
    sql.push_str(" ORDER BY updated_at DESC");
    let mut q = sqlx::query(&sql);
    if let Some(s) = source {
        q = q.bind(s);
    }
    if let Some(k) = board_kind {
        q = q.bind(k);
    }
    if let Some(id) = board_id {
        q = q.bind(id);
    }
    let rows = q.fetch_all(pool).await?;
    Ok(Value::Array(
        rows.iter().map(hydrate_external_link).collect(),
    ))
}

/// Row -> JSON with `metadata` parsed into an object.
fn hydrate_external_link(r: &SqliteRow) -> Value {
    let mut v = row_to_json(r);
    if let Value::Object(ref mut m) = v {
        let meta: Value = m
            .get("metadata")
            .and_then(|x| x.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));
        m.insert("metadata".into(), meta);
    }
    v
}

/// Merge case-insensitive duplicate projects into one canonical row each. For every group
/// of projects whose names match ignoring case, the earliest-created (lowest id on a tie)
/// is kept; tasks, project subscriptions, and events pointing at the others are repointed
/// onto it, then the duplicates are deleted. Idempotent: a DB with no dupes is unchanged.
///
/// This is a deliberate maintenance action (exposed via the `dedup_projects` admin path),
/// not something that runs on startup — back up the DB before invoking it.
pub async fn merge_duplicate_projects(pool: &Pool) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;

    // Groups with more than one project sharing a case-folded name.
    let groups = sqlx::query(
        "SELECT GROUP_CONCAT(id) AS ids FROM projects \
         GROUP BY name COLLATE NOCASE HAVING COUNT(*) > 1",
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut merged_groups = 0i64;
    let mut removed = 0i64;
    let mut details = Vec::new();

    for g in &groups {
        let ids_csv: String = g.try_get("ids")?;
        let mut ids: Vec<i64> = ids_csv.split(',').filter_map(|s| s.parse().ok()).collect();
        // Canonical = earliest created_at, tie-broken by lowest id (stable + deterministic).
        // Sort by (created_at, id) using the stored rows.
        let mut with_ts: Vec<(String, i64)> = Vec::new();
        for id in &ids {
            let row = sqlx::query("SELECT created_at FROM projects WHERE id=?")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
            with_ts.push((row.try_get::<String, _>("created_at")?, *id));
        }
        with_ts.sort();
        let keep = with_ts.first().map(|(_, id)| *id).unwrap();
        ids.retain(|&id| id != keep);

        for &dup in &ids {
            sqlx::query("UPDATE tasks SET project_id=? WHERE project_id=?")
                .bind(keep)
                .bind(dup)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE events SET project_id=? WHERE project_id=?")
                .bind(keep)
                .bind(dup)
                .execute(&mut *tx)
                .await?;
            // Repoint project subscriptions, but drop any that would collide with an
            // existing subscription on the canonical project (UNIQUE constraint).
            sqlx::query(
                "DELETE FROM subscriptions WHERE target_type='project' AND target_id=? \
                 AND subscriber IN (SELECT subscriber FROM subscriptions \
                   WHERE target_type='project' AND target_id=?)",
            )
            .bind(dup)
            .bind(keep)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE subscriptions SET target_id=? WHERE target_type='project' AND target_id=?",
            )
            .bind(keep)
            .bind(dup)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM projects WHERE id=?")
                .bind(dup)
                .execute(&mut *tx)
                .await?;
            removed += 1;
        }
        merged_groups += 1;
        details.push(json!({ "kept": keep, "removed": ids }));
    }

    tx.commit().await?;
    Ok(json!({
        "merged_groups": merged_groups,
        "projects_removed": removed,
        "groups": details,
    }))
}

// The webhook timeout is carried on the pool's app state; we thread it via a thread-local
// set at startup to avoid changing every signature. See `main.rs`.
fn webhook_timeout(_pool: &Pool) -> Duration {
    crate::WEBHOOK_TIMEOUT
        .get()
        .copied()
        .unwrap_or_else(|| Duration::from_secs(5))
}

// --- Documents ---
//
// A document is publishable, versioned content addressed by a CID. The board stores only the
// identifier (a bare CID) + metadata; the bytes live on IPFS and the *client* resolves the CID
// — the board never runs `ipfs add` nor composes a gateway URL, so the identifier stays
// location-independent. Each publish appends an immutable `document_versions` row and advances
// `current_version_id`. Event wiring (document.created / document.version_published, a document
// subscription scope, and Recipients::FromDocument) lands in the follow-on "events" task.

/// Derive a URL-friendly slug from a title: lowercase, runs of non-alphanumerics collapsed to a
/// single '-', ends trimmed. Non-unique — documents may share a title.
fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Normalize a wiki path the same way `set_document_path` does, so a link target and a
/// document's stored `path` compare equal: trim whitespace and strip surrounding slashes.
fn normalize_wiki_path(p: &str) -> String {
    p.trim().trim_matches('/').to_string()
}

/// One outbound edge parsed from a document's content: a `[[wiki-link]]` (a jump) or a
/// `![[transclusion]]` (an inline embed). `version_no` is an embed's `@vN` pin (None floats to
/// the target's current version); `region` is an embed's `#fragment` for a partial embed.
struct WikiEdge {
    path: String,
    label: Option<String>,
    kind: &'static str,
    version_no: Option<i64>,
    region: Option<String>,
}

/// Extract `[[wiki-link]]` / `[[path|label]]` and `![[embed]]` / `![[path@vN#region]]` targets
/// from a document's raw content. Returns edges de-duplicated by target_path (first occurrence
/// wins -- so a doc that both links AND embeds the same path records the first-seen edge only;
/// lifting that needs a composite-key migration), each path normalized like a document path.
fn extract_wiki_edges(content: &str) -> Vec<WikiEdge> {
    let bytes = content.as_bytes();
    let mut out: Vec<WikiEdge> = Vec::new();
    // De-dup per (kind, path) so a doc that BOTH [[links]] and ![[embeds]] the same path keeps
    // both edges (links and embeds live in separate tables — task 108).
    let mut seen: BTreeSet<(&str, String)> = BTreeSet::new();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            // A leading '!' makes it an embed (![[...]]) rather than a plain link.
            let is_embed = i > 0 && bytes[i - 1] == b'!';
            if let Some(close) = content[i + 2..].find("]]") {
                let inner = &content[i + 2..i + 2 + close];
                // inner = path[@vN][#region][|label]. Peel from the right: label, then region,
                // then a version pin, leaving the bare path.
                let (left, label) = match inner.split_once('|') {
                    Some((l, r)) => (l, Some(r.trim().to_string())),
                    None => (inner, None),
                };
                let (left, region) = match left.split_once('#') {
                    Some((l, r)) => (l, Some(r.trim().to_string())),
                    None => (left, None),
                };
                let (raw_path, version_no) = match left.split_once('@') {
                    Some((p, v)) => (
                        p,
                        v.trim().trim_start_matches(['v', 'V']).parse::<i64>().ok(),
                    ),
                    None => (left, None),
                };
                let path = normalize_wiki_path(raw_path);
                let kind = if is_embed { "embed" } else { "link" };
                if !path.is_empty() && seen.insert((kind, path.clone())) {
                    out.push(WikiEdge {
                        path,
                        label: label.filter(|l| !l.is_empty()),
                        kind,
                        // A pin/region only makes sense for an embed; ignore them on a link.
                        version_no: if is_embed { version_no } else { None },
                        region: if is_embed {
                            region.filter(|r| !r.is_empty())
                        } else {
                            None
                        },
                    });
                }
                i += 2 + close + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Recompute a document's outbound edges (links + embeds) from its raw `content`, replacing any
/// prior edges. Only called when a caller supplies content (the board stores CIDs, not bytes, so
/// a CID-only publish can't be re-scanned and leaves existing edges untouched). An embed's `@vN`
/// pin is resolved to a target_version_id against whatever doc is currently filed at the target
/// path (NULL when unresolved -- a dangling or floating embed).
async fn refresh_document_links(
    tx: &mut Transaction<'_, Sqlite>,
    source_document_id: i64,
    content: &str,
    ts: &str,
) -> anyhow::Result<()> {
    // Links and embeds live in separate tables so a doc can both link and embed the same path.
    sqlx::query("DELETE FROM document_links WHERE source_document_id=?")
        .bind(source_document_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM document_embeds WHERE source_document_id=?")
        .bind(source_document_id)
        .execute(&mut **tx)
        .await?;
    for e in extract_wiki_edges(content) {
        if e.kind == "embed" {
            // Resolve an embed's @vN pin to the immutable version id, if that doc+version exists.
            let target_version_id: Option<i64> = match e.version_no {
                Some(no) => sqlx::query(
                    "SELECT dv.id FROM documents d JOIN document_versions dv ON dv.document_id = d.id \
                     WHERE d.path=? AND dv.version_no=?",
                )
                .bind(&e.path)
                .bind(no)
                .fetch_optional(&mut **tx)
                .await?
                .and_then(|r| r.try_get::<i64, _>("id").ok()),
                None => None,
            };
            sqlx::query(
                "INSERT INTO document_embeds(source_document_id, target_path, label, \
                 target_version_id, region, created_at) VALUES(?,?,?,?,?,?)",
            )
            .bind(source_document_id)
            .bind(&e.path)
            .bind(e.label.as_deref())
            .bind(target_version_id)
            .bind(e.region.as_deref())
            .bind(ts)
            .execute(&mut **tx)
            .await?;
        } else {
            sqlx::query(
                "INSERT INTO document_links(source_document_id, target_path, label, kind, created_at) \
                 VALUES(?,?,?,'link',?)",
            )
            .bind(source_document_id)
            .bind(&e.path)
            .bind(e.label.as_deref())
            .bind(ts)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

/// Fetch a document as JSON (metadata parsed) plus its resolved `current_version` and full
/// `versions` list (newest first). Returns None if the document doesn't exist.
async fn document_json(
    tx: &mut Transaction<'_, Sqlite>,
    document_id: i64,
) -> anyhow::Result<Option<Value>> {
    let Some(mut d) = fetch_one_json(tx, "SELECT * FROM documents WHERE id=?", document_id).await?
    else {
        return Ok(None);
    };
    let vrows =
        sqlx::query("SELECT * FROM document_versions WHERE document_id=? ORDER BY version_no DESC")
            .bind(document_id)
            .fetch_all(&mut **tx)
            .await?;
    let versions: Vec<Value> = vrows.iter().map(row_to_json).collect();
    if let Value::Object(ref mut m) = d {
        insert_ref(m, "doc"); // typed canonical id (task 504)
        let meta: Value = m
            .get("metadata")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));
        m.insert("metadata".into(), meta);
        let cur_id = m.get("current_version_id").and_then(|v| v.as_i64());
        let current = cur_id.and_then(|cid| {
            versions
                .iter()
                .find(|v| v["id"].as_i64() == Some(cid))
                .cloned()
        });
        m.insert("current_version".into(), current.unwrap_or(Value::Null));
        m.insert("versions".into(), Value::Array(versions));

        // Tasks this document is attached to (id/title/status/project_id summaries;
        // project_id lets a client deep-link into the task's board + drawer).
        let tasks = sqlx::query(
            "SELECT t.id, t.title, t.status, t.project_id FROM document_attachments a \
             JOIN tasks t ON t.id = a.task_id WHERE a.document_id=? ORDER BY t.id",
        )
        .bind(document_id)
        .fetch_all(&mut **tx)
        .await?;
        m.insert(
            "attached_tasks".into(),
            Value::Array(tasks.iter().map(|r| row_to_json_ref(r, "task")).collect()),
        );

        // Outbound wiki links ([[target]] this doc points at, kind='link'), each resolved to the
        // document currently filed at that path (target_* are null when the link dangles).
        let out_links = sqlx::query(
            "SELECT l.target_path, l.label, d.id AS target_document_id, d.title AS target_title, \
             d.status AS target_status FROM document_links l \
             LEFT JOIN documents d ON d.path = l.target_path \
             WHERE l.source_document_id=? AND l.kind='link' ORDER BY l.target_path",
        )
        .bind(document_id)
        .fetch_all(&mut **tx)
        .await?;
        m.insert(
            "outbound_links".into(),
            Value::Array(out_links.iter().map(row_to_json).collect()),
        );

        // Outbound embeds (![[target]] this doc transcludes) from the document_embeds table.
        // Carries the pinned target_version_id (null = floats to the target's current version)
        // and an optional region fragment for a partial embed, plus the resolved target doc.
        let embeds = sqlx::query(
            "SELECT e.target_path, e.label, e.target_version_id, e.region, \
             d.id AS target_document_id, d.title AS target_title, d.status AS target_status \
             FROM document_embeds e LEFT JOIN documents d ON d.path = e.target_path \
             WHERE e.source_document_id=? ORDER BY e.target_path",
        )
        .bind(document_id)
        .fetch_all(&mut **tx)
        .await?;
        m.insert(
            "embeds".into(),
            Value::Array(embeds.iter().map(row_to_json).collect()),
        );

        // Incoming edges to THIS doc's path: backlinks (docs that LINK here) and embedded_by
        // (docs that EMBED here -- the "what depends on me before I change it" payoff). Both
        // empty when this doc is unfiled (no path), since an edge can only target a path.
        let (backlinks, embedded_by) = match m.get("path").and_then(|v| v.as_str()) {
            Some(p) => {
                let back = sqlx::query(
                    "SELECT s.id, s.title, s.path, s.status, l.label \
                     FROM document_links l JOIN documents s ON s.id = l.source_document_id \
                     WHERE l.target_path=? AND l.kind='link' ORDER BY s.path, s.id",
                )
                .bind(p)
                .fetch_all(&mut **tx)
                .await?;
                let emb = sqlx::query(
                    "SELECT s.id, s.title, s.path, s.status, e.label, e.region \
                     FROM document_embeds e JOIN documents s ON s.id = e.source_document_id \
                     WHERE e.target_path=? ORDER BY s.path, s.id",
                )
                .bind(p)
                .fetch_all(&mut **tx)
                .await?;
                (
                    Value::Array(back.iter().map(row_to_json).collect()),
                    Value::Array(emb.iter().map(row_to_json).collect()),
                )
            }
            None => (Value::Array(Vec::new()), Value::Array(Vec::new())),
        };
        m.insert("backlinks".into(), backlinks);
        m.insert("embedded_by".into(), embedded_by);
    }
    Ok(Some(d))
}

/// Create a document with its first version. The `cid` is stored verbatim (the board does not
/// resolve or validate it). Returns the document JSON with its current version + version list.
#[allow(clippy::too_many_arguments)]
pub async fn create_document(
    pool: &Pool,
    title: &str,
    project_id: Option<i64>,
    cid: &str,
    summary: Option<&str>,
    created_by: Option<&str>,
    metadata: Option<Value>,
    content_type: Option<&str>,
    content: Option<&str>,
) -> anyhow::Result<Value> {
    // A document title renders as the page h1, so it is in scope for the ASCII-only ruling. Hard
    // rule (no acknowledge): a non-ASCII title is never legitimate.
    check_non_ascii(title, false)?;
    let ts = now_iso();
    let ct = content_type.unwrap_or("text/markdown");
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let meta_str = metadata.unwrap_or_else(|| json!({})).to_string();
    let slug = slugify(title);
    let did: i64 = sqlx::query(
        "INSERT INTO documents(title, slug, project_id, metadata, created_by, created_at, updated_at) \
         VALUES(?,?,?,?,?,?,?) RETURNING id",
    )
    .bind(title)
    .bind(&slug)
    .bind(project_id)
    .bind(&meta_str)
    .bind(created_by)
    .bind(&ts)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    let vid: i64 = sqlx::query(
        "INSERT INTO document_versions(document_id, version_no, cid, summary, created_by, created_at, content_type) \
         VALUES(?,1,?,?,?,?,?) RETURNING id",
    )
    .bind(did)
    .bind(cid)
    .bind(summary)
    .bind(created_by)
    .bind(&ts)
    .bind(ct)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    sqlx::query("UPDATE documents SET current_version_id=? WHERE id=?")
        .bind(vid)
        .bind(did)
        .execute(&mut *tx)
        .await?;
    // The author subscribes so they hear about future versions and review activity.
    auto_subscribe_document(&mut tx, created_by, did).await?;
    // Index outbound wiki links when the caller supplied raw content (a CID-only create can't
    // be scanned -- the board never fetches the bytes).
    if let Some(c) = content {
        refresh_document_links(&mut tx, did, c, &ts).await?;
    }
    emit(
        &mut tx,
        &mut hooks,
        "document.created",
        created_by,
        None,
        project_id,
        None,
        Some(did),
        json!({ "title": title, "slug": slug, "cid": cid, "version_no": 1, "content_type": ct }),
        Recipients::FromDocument(did),
    )
    .await?;
    let out = document_json(&mut tx, did).await?.unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Append a new immutable version and advance the document's current pointer. A new version
/// supersedes any prior approval: an `approved` / `changes_requested` document drops back to
/// `in_review` (the review workflow itself lands in a follow-on task).
#[allow(clippy::too_many_arguments)]
pub async fn publish_version(
    pool: &Pool,
    document_id: i64,
    cid: &str,
    summary: Option<&str>,
    created_by: Option<&str>,
    content_type: Option<&str>,
    content: Option<&str>,
) -> anyhow::Result<Value> {
    // Reject an ambiguous bare "#N" in the submitted version summary/content (task #517 hard-fail).
    if let Some(s) = summary {
        check_bare_refs(s)?;
    }
    if let Some(c) = content {
        check_bare_refs(c)?;
    }
    let ts = now_iso();
    let ct = content_type.unwrap_or("text/markdown");
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let Some(doc) = sqlx::query("SELECT status, project_id FROM documents WHERE id=?")
        .bind(document_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        anyhow::bail!("no document {document_id}");
    };
    let status: String = doc.try_get("status")?;
    let project_id: Option<i64> = doc.try_get("project_id")?;
    let next_no: i64 = sqlx::query(
        "SELECT COALESCE(MAX(version_no),0)+1 AS n FROM document_versions WHERE document_id=?",
    )
    .bind(document_id)
    .fetch_one(&mut *tx)
    .await?
    .try_get("n")?;
    let vid: i64 = sqlx::query(
        "INSERT INTO document_versions(document_id, version_no, cid, summary, created_by, created_at, content_type) \
         VALUES(?,?,?,?,?,?,?) RETURNING id",
    )
    .bind(document_id)
    .bind(next_no)
    .bind(cid)
    .bind(summary)
    .bind(created_by)
    .bind(&ts)
    .bind(ct)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    let new_status = if status == "approved" || status == "changes_requested" {
        "in_review"
    } else {
        status.as_str()
    };
    sqlx::query("UPDATE documents SET current_version_id=?, status=?, updated_at=? WHERE id=?")
        .bind(vid)
        .bind(new_status)
        .bind(&ts)
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
    // The publisher subscribes (auto-join-on-post, like channels) so they hear about follow-ups.
    auto_subscribe_document(&mut tx, created_by, document_id).await?;
    // Re-index outbound wiki links from the new content (CID-only publishes leave edges as-is).
    if let Some(c) = content {
        refresh_document_links(&mut tx, document_id, c, &ts).await?;
    }
    emit(
        &mut tx,
        &mut hooks,
        "document.version_published",
        created_by,
        None,
        project_id,
        None,
        Some(document_id),
        json!({ "version_no": next_no, "cid": cid, "summary": summary, "status": new_status, "content_type": ct }),
        Recipients::FromDocument(document_id),
    )
    .await?;
    let out = document_json(&mut tx, document_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Fetch one document with its current version + version list.
pub async fn get_document(pool: &Pool, document_id: i64) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let out = document_json(&mut tx, document_id).await?;
    tx.commit().await?;
    match out {
        Some(v) => Ok(v),
        None => anyhow::bail!("no document {document_id}"),
    }
}

/// get_document plus, when `include_body` is set, the current version's markdown fetched
/// server-side from the pinned CID and inlined as `body` (task #424: an agent building/reviewing
/// from an approved doc gets the body in one call, regardless of its host — the board's own host
/// reaches the IPFS gateway; the doc stays CID-only at rest per #126, the body is fetched on read,
/// not stored). A body-fetch failure (no backend, unreachable, non-text) does NOT fail the call —
/// the metadata still returns with `body: null` + a `body_error`/`body_note`, so the tool degrades
/// gracefully instead of dead-ending.
pub async fn get_document_with_body(
    pool: &Pool,
    ipfs_api_url: Option<&str>,
    document_id: i64,
    include_body: bool,
) -> anyhow::Result<Value> {
    let mut v = get_document(pool, document_id).await?;
    if include_body {
        match read_document_content(pool, ipfs_api_url, document_id, None).await {
            Ok(content) => {
                if let Value::Object(ref mut m) = v {
                    m.insert(
                        "body".into(),
                        content.get("content").cloned().unwrap_or(Value::Null),
                    );
                    if let Some(ct) = content.get("content_type") {
                        m.insert("body_content_type".into(), ct.clone());
                    }
                    if let Some(note) = content.get("note") {
                        m.insert("body_note".into(), note.clone());
                    }
                }
            }
            Err(e) => {
                if let Value::Object(ref mut m) = v {
                    m.insert("body".into(), Value::Null);
                    m.insert("body_error".into(), json!(e.to_string()));
                }
            }
        }
    }
    Ok(v)
}

/// Rename a document (metadata only — the title, and its derived slug). Versions, content, path,
/// and review status are untouched. Emits document.updated to the doc's subscribers + owner and
/// returns the updated document. The title is what the viewer renders as the page header.
pub async fn update_document(
    pool: &Pool,
    document_id: i64,
    title: &str,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let title = title.trim();
    if title.is_empty() {
        anyhow::bail!("give a non-empty title");
    }
    // Titles are in scope for the ASCII-only ruling (they render as the page h1). Hard rule.
    check_non_ascii(title, false)?;
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let Some(row) = sqlx::query("SELECT project_id FROM documents WHERE id=?")
        .bind(document_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        anyhow::bail!("no document {document_id}");
    };
    let project_id: Option<i64> = row.try_get("project_id")?;
    let slug = slugify(title);
    sqlx::query("UPDATE documents SET title=?, slug=?, updated_at=? WHERE id=?")
        .bind(title)
        .bind(&slug)
        .bind(&ts)
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
    emit(
        &mut tx,
        &mut hooks,
        "document.updated",
        actor,
        None,
        project_id,
        None,
        Some(document_id),
        json!({ "document_id": document_id, "title": title, "slug": slug }),
        Recipients::FromDocument(document_id),
    )
    .await?;
    let out = document_json(&mut tx, document_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Cap on a from-session document-body read, matching the REST IPFS gateway cap.
pub const DOCUMENT_READ_CAP_BYTES: usize = 25 * 1024 * 1024;

/// Whether a content_type is text-shaped, i.e. safe to return as a UTF-8 string from the read
/// path. Binary types (image/pdf/...) are not inlined; the caller fetches their bytes by CID.
pub fn is_text_content_type(ct: &str) -> bool {
    let t = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    t.is_empty()
        || t.starts_with("text/")
        || t == "application/json"
        || t == "application/xml"
        || t == "application/javascript"
        || t.ends_with("+json")
        || t.ends_with("+xml")
}

/// Resolve a document version's stored (version_no, cid, content_type) — the current version when
/// `version_no` is None, or the named version otherwise.
pub async fn resolve_document_version(
    pool: &Pool,
    document_id: i64,
    version_no: Option<i64>,
) -> anyhow::Result<(i64, String, String)> {
    let row = match version_no {
        Some(n) => {
            sqlx::query(
                "SELECT version_no, cid, content_type FROM document_versions \
                 WHERE document_id=? AND version_no=?",
            )
            .bind(document_id)
            .bind(n)
            .fetch_optional(pool)
            .await?
        }
        None => {
            sqlx::query(
                "SELECT dv.version_no, dv.cid, dv.content_type FROM documents d \
                 JOIN document_versions dv ON dv.id = d.current_version_id WHERE d.id=?",
            )
            .bind(document_id)
            .fetch_optional(pool)
            .await?
        }
    };
    let Some(row) = row else {
        match version_no {
            Some(n) => anyhow::bail!("no version {n} for document {document_id}"),
            None => anyhow::bail!("no document {document_id}"),
        }
    };
    let vn: i64 = row.try_get("version_no")?;
    let cid: String = row.try_get("cid")?;
    let ct: Option<String> = row.try_get("content_type")?;
    Ok((vn, cid, ct.unwrap_or_else(|| "text/markdown".to_string())))
}

/// Read a document's body content from-session: resolve the version's CID and return the text,
/// fetched through the board's own IPFS backend server-side. This lets an agent on the board
/// client read a document body without local IPFS or a separate gateway. Text-shaped content is
/// returned inline as `content`; binary content (image/pdf/...) returns a null `content` + the CID
/// so the caller can fetch the raw bytes via the REST gateway instead. Requires `ipfs_api_url`.
pub async fn read_document_content(
    pool: &Pool,
    ipfs_api_url: Option<&str>,
    document_id: i64,
    version_no: Option<i64>,
) -> anyhow::Result<Value> {
    let (vn, cid, ct) = resolve_document_version(pool, document_id, version_no).await?;
    let Some(url) = ipfs_api_url else {
        anyhow::bail!(
            "no IPFS backend configured (set ipfs_api_url); this board can't read content by CID"
        );
    };
    let mut out = json!({
        "document_id": document_id,
        "version_no": vn,
        "cid": cid,
        "content_type": ct,
    });
    if is_text_content_type(&ct) {
        let bytes = crate::ipfs::cat(url, &cid, DOCUMENT_READ_CAP_BYTES).await?;
        let text = String::from_utf8(bytes).map_err(|_| {
            anyhow::anyhow!("document {document_id} v{vn} content is not valid UTF-8")
        })?;
        out["content"] = json!(text);
    } else {
        out["content"] = Value::Null;
        out["note"] = json!(format!(
            "binary content ({ct}); fetch the raw bytes via GET /api/ipfs/{cid}"
        ));
    }
    Ok(out)
}

/// List a document's versions (immutable), newest first.
pub async fn get_document_versions(pool: &Pool, document_id: i64) -> anyhow::Result<Value> {
    let rows =
        sqlx::query("SELECT * FROM document_versions WHERE document_id=? ORDER BY version_no DESC")
            .bind(document_id)
            .fetch_all(pool)
            .await?;
    Ok(Value::Array(rows.iter().map(row_to_json).collect()))
}

/// List documents for discovery, filtered by any combination of: project, status, tag (a value
/// in the document's `metadata.tags` array), task_id (documents attached to that task), and
/// author (created_by). All filters AND together.
#[allow(clippy::too_many_arguments)]
pub async fn list_documents(
    pool: &Pool,
    project_id: Option<i64>,
    status: Option<&str>,
    tag: Option<&str>,
    task_id: Option<i64>,
    author: Option<&str>,
    include_archived: bool,
) -> anyhow::Result<Value> {
    let mut q = String::from(
        "SELECT id, title, slug, path, project_id, status, current_version_id, approved_version_id, \
         created_by, updated_at, archived_at FROM documents",
    );
    // conds and the binds below MUST stay in the same order. (A cond that binds no value — like
    // the archived filter — can go anywhere without disturbing that order.)
    let mut conds: Vec<&str> = Vec::new();
    if !include_archived {
        conds.push("archived_at IS NULL");
    }
    if project_id.is_some() {
        conds.push("project_id=?");
    }
    if status.is_some() {
        conds.push("status=?");
    }
    if author.is_some() {
        conds.push("created_by=?");
    }
    if task_id.is_some() {
        conds.push("id IN (SELECT document_id FROM document_attachments WHERE task_id=?)");
    }
    if tag.is_some() {
        // A value in the metadata.tags JSON array. json_each yields no rows when tags is absent.
        conds.push("EXISTS (SELECT 1 FROM json_each(documents.metadata, '$.tags') WHERE value=?)");
    }
    if !conds.is_empty() {
        q.push_str(" WHERE ");
        q.push_str(&conds.join(" AND "));
    }
    q.push_str(" ORDER BY id");
    let mut query = sqlx::query(&q);
    if let Some(p) = project_id {
        query = query.bind(p);
    }
    if let Some(s) = status {
        query = query.bind(s);
    }
    if let Some(a) = author {
        query = query.bind(a);
    }
    if let Some(t) = task_id {
        query = query.bind(t);
    }
    if let Some(tg) = tag {
        query = query.bind(tg);
    }
    let rows = query.fetch_all(pool).await?;
    Ok(Value::Array(
        rows.iter().map(|r| row_to_json_ref(r, "doc")).collect(),
    ))
}

// --- Wiki: hierarchical paths over documents (#105) ---

/// File a document at a wiki `path` (or unfile it). A wiki page IS a document; the path is just
/// where it lives in the tree. Passing an empty/whitespace path clears it (unfiles). A non-empty
/// path is trimmed and must be unique across documents — a collision is a client error. Emits
/// `document.updated` so a tree view live-updates. Returns the updated document.
pub async fn set_document_path(
    pool: &Pool,
    document_id: i64,
    path: &str,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let trimmed = path.trim().trim_matches('/');
    let new_path: Option<&str> = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    };
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let Some(doc) = sqlx::query("SELECT project_id FROM documents WHERE id=?")
        .bind(document_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        anyhow::bail!("no document {document_id}");
    };
    let project_id: Option<i64> = doc.try_get("project_id")?;
    // Collision: another document already filed at this path.
    if let Some(p) = new_path {
        if let Some(row) = sqlx::query("SELECT id FROM documents WHERE path=? AND id<>?")
            .bind(p)
            .bind(document_id)
            .fetch_optional(&mut *tx)
            .await?
        {
            let other: i64 = row.try_get("id")?;
            anyhow::bail!("give a different `path`: '{p}' is already used by document {other}");
        }
    }
    sqlx::query("UPDATE documents SET path=?, updated_at=? WHERE id=?")
        .bind(new_path)
        .bind(&ts)
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    emit(
        &mut tx,
        &mut hooks,
        "document.updated",
        actor,
        None,
        project_id,
        None,
        Some(document_id),
        json!({ "path": new_path }),
        Recipients::FromDocument(document_id),
    )
    .await?;
    let out = document_json(&mut tx, document_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// List filed (path-bearing) documents for the wiki tree, optionally restricted to a path
/// `prefix` — the prefix itself plus everything nested under `prefix/`. Ordered by path, so a
/// client can render the tree directly. Returns doc summaries (incl. `path`).
pub async fn list_wiki(
    pool: &Pool,
    prefix: Option<&str>,
    include_archived: bool,
) -> anyhow::Result<Value> {
    // Archived docs are hidden from the tree by default (reversible; the doc still resolves by id).
    let arch = if include_archived {
        ""
    } else {
        " AND archived_at IS NULL"
    };
    let cols = "id, title, slug, path, project_id, status, current_version_id, \
                approved_version_id, created_by, updated_at, archived_at";
    let rows = match prefix
        .map(|p| p.trim().trim_matches('/'))
        .filter(|p| !p.is_empty())
    {
        Some(p) => {
            sqlx::query(&format!(
                "SELECT {cols} FROM documents \
                 WHERE path IS NOT NULL AND (path=? OR path LIKE ? || '/%'){arch} ORDER BY path",
            ))
            .bind(p)
            .bind(p)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query(&format!(
                "SELECT {cols} FROM documents WHERE path IS NOT NULL{arch} ORDER BY path",
            ))
            .fetch_all(pool)
            .await?
        }
    };
    Ok(Value::Array(rows.iter().map(row_to_json).collect()))
}

/// Turn a document_comment row into JSON with its `region` TEXT parsed back into a JSON object
/// (null when the comment has no anchor).
fn document_comment_json(row: &SqliteRow) -> Value {
    let mut obj = match row_to_json(row) {
        Value::Object(m) => m,
        other => return other,
    };
    let region: Value = obj
        .get("region")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null);
    obj.insert("region".into(), region);
    Value::Object(obj)
}

/// Comment on a document, optionally anchored to a `region` of a specific (immutable) `version_id`.
/// `region` is stored verbatim as JSON (W3C/Hypothesis-style selectors) — the backend never
/// interprets it. Auto-subscribes the commenter and emits `document.comment` (FromDocument).
#[allow(clippy::too_many_arguments)]
pub async fn comment_document(
    pool: &Pool,
    document_id: i64,
    version_id: Option<i64>,
    author: Option<&str>,
    body: &str,
    region: Option<Value>,
    reply_to: Option<i64>,
    external_author: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    // Validate the document exists (for a clean 404; version_id/reply_to are FK-enforced) and grab
    // its title so the notification names the document.
    let Some(doc_row) = sqlx::query("SELECT title FROM documents WHERE id=?")
        .bind(document_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        anyhow::bail!("no document {document_id}");
    };
    let title: Option<String> = doc_row.try_get("title")?;
    let region_str = region.map(|r| r.to_string());
    // `author` = the fleet agent that wrote/ingested the comment; `external_author`, when set, is
    // the external_identities id it's attributed to (an ingested human) — same as task comments.
    let cid: i64 = sqlx::query(
        "INSERT INTO document_comments(document_id, version_id, author, body, region, reply_to, created_at, external_author) \
         VALUES(?,?,?,?,?,?,?,?) RETURNING id",
    )
    .bind(document_id)
    .bind(version_id)
    .bind(author)
    .bind(body)
    .bind(&region_str)
    .bind(reply_to)
    .bind(&ts)
    .bind(external_author)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    auto_subscribe_document(&mut tx, author, document_id).await?;
    // An @mention in the comment subscribes that agent to the document, so the document.comment
    // fan-out below reaches them -- a mention notifies the mentioned agent regardless of their prior
    // subscription, matching task-comment @mentions. Without this, doc-comment @mentions were a
    // silent black hole (operator-reported). Idempotent; unregistered @tokens ignored.
    subscribe_mentions_document(&mut tx, body, document_id).await?;
    // Carry document_id + title in the payload so a drained notification identifies which document
    // the comment is on (the inbox row itself doesn't surface document_id), letting the owner
    // navigate straight to it to respond. The commenter is the event actor.
    let mut data = json!({ "document_id": document_id, "title": title, "comment_id": cid, "version_id": version_id, "body": body, "reply_to": reply_to });
    if let Some(ext) = external_author {
        data["external_author"] = json!(ext);
    }
    emit(
        &mut tx,
        &mut hooks,
        "document.comment",
        author,
        None,
        None,
        None,
        Some(document_id),
        data,
        // A comment on a doc also reaches the people working any task the doc is attached to --
        // their assignee/creator/subscribers -- not only doc watchers (task 581).
        Recipients::FromDocumentAndAttachedTasks(document_id),
    )
    .await?;
    let out = sqlx::query(
        "SELECT dc.*, ei.display_name AS external_author_name \
         FROM document_comments dc LEFT JOIN external_identities ei ON ei.id = dc.external_author \
         WHERE dc.id=?",
    )
    .bind(cid)
    .fetch_optional(&mut *tx)
    .await?
    .as_ref()
    .map(document_comment_json)
    .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Mark a comment resolved and emit `document.comment_resolved` (FromDocument).
pub async fn resolve_comment(
    pool: &Pool,
    comment_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let Some(row) = sqlx::query("SELECT document_id FROM document_comments WHERE id=?")
        .bind(comment_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        anyhow::bail!("no comment {comment_id}");
    };
    let document_id: i64 = row.try_get("document_id")?;
    sqlx::query("UPDATE document_comments SET status='resolved' WHERE id=?")
        .bind(comment_id)
        .execute(&mut *tx)
        .await?;
    emit(
        &mut tx,
        &mut hooks,
        "document.comment_resolved",
        actor,
        None,
        None,
        None,
        Some(document_id),
        json!({ "comment_id": comment_id }),
        Recipients::FromDocument(document_id),
    )
    .await?;
    let out = sqlx::query(
        "SELECT dc.*, ei.display_name AS external_author_name \
         FROM document_comments dc LEFT JOIN external_identities ei ON ei.id = dc.external_author \
         WHERE dc.id=?",
    )
    .bind(comment_id)
    .fetch_optional(&mut *tx)
    .await?
    .as_ref()
    .map(document_comment_json)
    .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// List a document's comments (oldest first), optionally filtered by version and/or status.
pub async fn get_document_comments(
    pool: &Pool,
    document_id: i64,
    version_id: Option<i64>,
    status: Option<&str>,
) -> anyhow::Result<Value> {
    let mut q = String::from(
        "SELECT dc.*, ei.display_name AS external_author_name \
         FROM document_comments dc LEFT JOIN external_identities ei ON ei.id = dc.external_author \
         WHERE dc.document_id=?",
    );
    if version_id.is_some() {
        q.push_str(" AND dc.version_id=?");
    }
    if status.is_some() {
        q.push_str(" AND dc.status=?");
    }
    q.push_str(" ORDER BY dc.id");
    let mut query = sqlx::query(&q).bind(document_id);
    if let Some(v) = version_id {
        query = query.bind(v);
    }
    if let Some(s) = status {
        query = query.bind(s);
    }
    let rows = query.fetch_all(pool).await?;
    Ok(Value::Array(
        rows.iter().map(document_comment_json).collect(),
    ))
}

/// Shared driver for a document status transition: set the status (optionally stamping the
/// current version as approved), emit an event to the doc's subscribers, and return the updated
/// document. Approval is a stamp on a specific version, not a lock — publishing again reopens
/// review (see publish_version).
async fn document_transition(
    pool: &Pool,
    document_id: i64,
    new_status: &str,
    stamp_approval: bool,
    actor: Option<&str>,
    event_type: &str,
    mut data: Value,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let Some(row) =
        sqlx::query("SELECT project_id, current_version_id, title FROM documents WHERE id=?")
            .bind(document_id)
            .fetch_optional(&mut *tx)
            .await?
    else {
        anyhow::bail!("no document {document_id}");
    };
    let project_id: Option<i64> = row.try_get("project_id")?;
    let current_version_id: Option<i64> = row.try_get("current_version_id")?;
    let title: Option<String> = row.try_get("title")?;
    // Enrich the event payload so a drained notification is self-describing — which document, its
    // title, and the new status — letting the owner act (e.g. on approval) without a lookup. The
    // approver is the event `actor`, already surfaced on the notification. (task #310)
    if let Value::Object(ref mut m) = data {
        m.insert("document_id".into(), json!(document_id));
        if let Some(t) = &title {
            m.insert("title".into(), json!(t));
        }
        m.insert("status".into(), json!(new_status));
    }
    if stamp_approval {
        sqlx::query(
            "UPDATE documents SET status=?, approved_version_id=?, approved_by=?, updated_at=? WHERE id=?",
        )
        .bind(new_status)
        .bind(current_version_id)
        .bind(actor)
        .bind(&ts)
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
        if let Value::Object(ref mut m) = data {
            m.insert("approved_version_id".into(), json!(current_version_id));
        }
    } else {
        sqlx::query("UPDATE documents SET status=?, updated_at=? WHERE id=?")
            .bind(new_status)
            .bind(&ts)
            .bind(document_id)
            .execute(&mut *tx)
            .await?;
    }
    emit(
        &mut tx,
        &mut hooks,
        event_type,
        actor,
        None,
        project_id,
        None,
        Some(document_id),
        data,
        Recipients::FromDocument(document_id),
    )
    .await?;
    let out = document_json(&mut tx, document_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Move a document into review (typically from draft or changes_requested). Convenience — a
/// publish_version also reopens review. Emits document.submitted_for_review.
pub async fn submit_for_review(
    pool: &Pool,
    document_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    document_transition(
        pool,
        document_id,
        "in_review",
        false,
        actor,
        "document.submitted_for_review",
        json!({}),
    )
    .await
}

/// Request changes on a document (status -> changes_requested), with an optional note. Emits
/// document.changes_requested so the author (a subscriber) is notified.
pub async fn request_changes(
    pool: &Pool,
    document_id: i64,
    actor: Option<&str>,
    note: Option<&str>,
) -> anyhow::Result<Value> {
    document_transition(
        pool,
        document_id,
        "changes_requested",
        false,
        actor,
        "document.changes_requested",
        json!({ "note": note }),
    )
    .await
}

/// Approve a document: stamp the current version as approved_version_id, record approved_by,
/// set status=approved. Emits document.approved.
pub async fn approve_document(
    pool: &Pool,
    document_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    document_transition(
        pool,
        document_id,
        "approved",
        true,
        actor,
        "document.approved",
        json!({}),
    )
    .await
}

/// Submit a document into the operator's review queue (status -> `operator_review`). This is the
/// single gated chokepoint before the operator (cameron) first sees a doc (task_623, operator
/// directive doc_32 comment 97): the submit is REJECTED unless BOTH hold --
///   1. Template attestation (operator requirement, task_622): the author names the doc template
///      they read and followed (`template_followed`, e.g. the design-doc template) OR gives a
///      non-empty `template_waiver_reason`. The STRENGTH of a waiver is a judgment call left to the
///      conformance reviewer, not checked mechanically here.
///   2. Design-conformance pass: a conformance review over this doc (`source='board_doc'`,
///      `target_ref=<id>`) has (a) a terminal `adversarial_review` summary entry whose JSON body
///      records `reviewed_version` == the doc's CURRENT version_no -- version-pinning forces a fresh
///      review after any edit, so a stale pass can't satisfy the gate -- and (b) zero OPEN
///      actionable findings: no `finding` log entry whose linked child task is not done/cancelled
///      (board-pm: the child-task status is the source of truth, the conformance review stays
///      append-only and is never flipped).
///
/// Fail-closed: any missing or mismatched signal rejects the transition with an actionable message,
/// so a doc can never reach the operator un-reviewed. The template attestation is stamped into the
/// doc metadata (durable) and carried on the emitted event. Emits
/// `document.submitted_for_operator_review` to the doc's subscribers.
pub async fn submit_to_operator_review(
    pool: &Pool,
    document_id: i64,
    actor: Option<&str>,
    template_followed: Option<&str>,
    template_waiver_reason: Option<&str>,
) -> anyhow::Result<Value> {
    // 1. Template attestation: one of the two must be non-empty after trimming.
    let template_followed = template_followed.map(str::trim).filter(|s| !s.is_empty());
    let template_waiver_reason = template_waiver_reason
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if template_followed.is_none() && template_waiver_reason.is_none() {
        anyhow::bail!(
            "submit_to_operator_review requires `template_followed` (the doc template you read and followed, e.g. the design-doc template) or, when none applies, a non-empty `template_waiver_reason`"
        );
    }

    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();

    let Some(row) = sqlx::query(
        "SELECT project_id, title, archived_at, current_version_id, metadata FROM documents WHERE id=?",
    )
    .bind(document_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        anyhow::bail!("no document {document_id}");
    };
    let project_id: Option<i64> = row.try_get("project_id")?;
    let title: Option<String> = row.try_get("title")?;
    let archived_at: Option<String> = row.try_get("archived_at")?;
    let current_version_id: Option<i64> = row.try_get("current_version_id")?;
    let meta_str: String = row.try_get("metadata")?;
    if archived_at.is_some() {
        anyhow::bail!(
            "document {document_id} is archived; restore it before submitting for operator review"
        );
    }
    let Some(cvid) = current_version_id else {
        anyhow::bail!("document {document_id} has no published version to review");
    };
    let current_version_no: i64 =
        sqlx::query("SELECT version_no FROM document_versions WHERE id=?")
            .bind(cvid)
            .fetch_one(&mut *tx)
            .await?
            .try_get("version_no")?;

    // 2a. Conformance ran against the CURRENT version: a terminal `adversarial_review` summary entry
    // on a review over this doc whose JSON body records `reviewed_version` == current_version_no.
    let target_ref = document_id.to_string();
    let summaries = sqlx::query(
        "SELECT rl.body AS body FROM review_log rl JOIN reviews r ON r.id = rl.review_id \
         WHERE r.source='board_doc' AND r.target_ref=? AND rl.entry_type='adversarial_review'",
    )
    .bind(&target_ref)
    .fetch_all(&mut *tx)
    .await?;
    let ran_current = summaries.iter().any(|s| {
        s.try_get::<Option<String>, _>("body")
            .ok()
            .flatten()
            .and_then(|b| serde_json::from_str::<Value>(&b).ok())
            .and_then(|v| v.get("reviewed_version").and_then(|r| r.as_i64()))
            .is_some_and(|rev| rev == current_version_no)
    });
    if !ran_current {
        anyhow::bail!(
            "design-conformance review has not run against the current version (v{current_version_no}) of document {document_id}: no adversarial_review summary entry records reviewed_version={current_version_no}. A conformance review must run (or re-run) on the current version before the doc can reach the operator"
        );
    }

    // 2b. Zero OPEN actionable findings: a `finding` log entry links a child task; the finding is
    // OPEN while that task is not done/cancelled (the child-task status is the source of truth, so
    // the append-only conformance review is never flipped).
    let open_findings: i64 = sqlx::query(
        "SELECT COUNT(*) AS n FROM review_log rl \
         JOIN reviews r ON r.id = rl.review_id \
         JOIN tasks t ON t.id = rl.task_id \
         WHERE r.source='board_doc' AND r.target_ref=? AND rl.entry_type='finding' \
           AND rl.task_id IS NOT NULL AND t.status NOT IN ('done','cancelled','canceled')",
    )
    .bind(&target_ref)
    .fetch_one(&mut *tx)
    .await?
    .try_get("n")?;
    if open_findings > 0 {
        anyhow::bail!(
            "document {document_id} has {open_findings} open conformance finding(s): close every finding's child task (done/cancelled) before submitting for operator review"
        );
    }

    // Passed. Stamp the template attestation into the doc metadata (durable) and advance status.
    let mut meta: Value = serde_json::from_str(&meta_str).unwrap_or_else(|_| json!({}));
    if let Value::Object(ref mut m) = meta {
        m.insert("template_followed".into(), json!(template_followed));
        m.insert(
            "template_waiver_reason".into(),
            json!(template_waiver_reason),
        );
        m.insert("operator_review_submitted_at".into(), json!(ts));
    }
    sqlx::query(
        "UPDATE documents SET status='operator_review', metadata=?, updated_at=? WHERE id=?",
    )
    .bind(meta.to_string())
    .bind(&ts)
    .bind(document_id)
    .execute(&mut *tx)
    .await?;

    emit(
        &mut tx,
        &mut hooks,
        "document.submitted_for_operator_review",
        actor,
        None,
        project_id,
        None,
        Some(document_id),
        json!({
            "document_id": document_id,
            "title": title,
            "status": "operator_review",
            "template_followed": template_followed,
            "template_waiver_reason": template_waiver_reason,
        }),
        Recipients::FromDocument(document_id),
    )
    .await?;
    let out = document_json(&mut tx, document_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Soft-archive (retire) a document, or restore it. Archiving stamps `archived_at` so the doc is
/// hidden from list_documents / list_wiki by default, but keeps its versions, comments, links, and
/// the append-only event log intact — reversible, and consistent with the board's audit model
/// (nothing is destroyed). Restoring clears the stamp. `archived_at` is orthogonal to the review
/// status. Emits document.archived / document.restored to the doc's subscribers. Idempotent
/// (re-archiving refreshes the stamp). Returns the updated document.
pub async fn set_document_archived(
    pool: &Pool,
    document_id: i64,
    archived: bool,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let Some(row) = sqlx::query("SELECT project_id FROM documents WHERE id=?")
        .bind(document_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        anyhow::bail!("no document {document_id}");
    };
    let project_id: Option<i64> = row.try_get("project_id")?;
    let stamp = archived.then(|| ts.clone());
    sqlx::query("UPDATE documents SET archived_at=?, updated_at=? WHERE id=?")
        .bind(stamp.as_deref())
        .bind(&ts)
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
    let event_type = if archived {
        "document.archived"
    } else {
        "document.restored"
    };
    emit(
        &mut tx,
        &mut hooks,
        event_type,
        actor,
        None,
        project_id,
        None,
        Some(document_id),
        json!({ "archived": archived }),
        Recipients::FromDocument(document_id),
    )
    .await?;
    let out = document_json(&mut tx, document_id)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Attach a document to a task (many-to-many, idempotent). Emits document.attached to both the
/// document's and the task's subscribers, so either side learns of the link.
pub async fn attach_document(
    pool: &Pool,
    document_id: i64,
    task_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    if sqlx::query("SELECT 1 FROM documents WHERE id=?")
        .bind(document_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        anyhow::bail!("no document {document_id}");
    }
    if sqlx::query("SELECT 1 FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        anyhow::bail!("no task {task_id}");
    }
    sqlx::query(
        "INSERT OR IGNORE INTO document_attachments(document_id, task_id, created_at) VALUES(?,?,?)",
    )
    .bind(document_id)
    .bind(task_id)
    .bind(&ts)
    .execute(&mut *tx)
    .await?;
    emit(
        &mut tx,
        &mut hooks,
        "document.attached",
        actor,
        Some(task_id),
        None,
        None,
        Some(document_id),
        json!({ "document_id": document_id, "task_id": task_id }),
        Recipients::FromDocumentAndTask(document_id, task_id),
    )
    .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(json!({ "document_id": document_id, "task_id": task_id, "attached": true }))
}

/// Detach a document from a task. Emits document.detached (to both sides) only if a link existed.
pub async fn detach_document(
    pool: &Pool,
    document_id: i64,
    task_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let n = sqlx::query("DELETE FROM document_attachments WHERE document_id=? AND task_id=?")
        .bind(document_id)
        .bind(task_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n > 0 {
        emit(
            &mut tx,
            &mut hooks,
            "document.detached",
            actor,
            Some(task_id),
            None,
            None,
            Some(document_id),
            json!({ "document_id": document_id, "task_id": task_id }),
            Recipients::FromDocumentAndTask(document_id, task_id),
        )
        .await?;
    }
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(json!({ "removed": n }))
}

// ---------------------------------------------------------------------------
// Secret requests — an ephemeral secret-REQUEST broker (task 272).
//
// The board brokers a *request* and a one-time, browser-encrypted handoff; it is never a secret
// store. An agent files a named request carrying the (non-secret) age recipient pubkeys + human
// instructions and gets a single-use capability submit link. An operator opens the link, and the
// browser encrypts the pasted value to the recipients and posts CIPHERTEXT ONLY — so the board
// never sees plaintext. A fulfiller pulls the ciphertext once (a separate token gates it),
// relocates it to durable storage, then the row (and its transient ciphertext) is deleted. A stuck
// submitted request is purged after its TTL. Ciphertext is never returned in list/metadata reads,
// never carried in an event, and never logged.
// ---------------------------------------------------------------------------

/// How long a submitted-but-unfulfilled request keeps its transient ciphertext before it is
/// auto-purged — the one at-rest window, so it is bounded.
const SECRET_SUBMIT_TTL: chrono::Duration = chrono::Duration::hours(1);

/// A fresh high-entropy capability token, from SQLite's CSPRNG (`randomblob`) — no plaintext
/// secret is involved, this just gates the submit link and the fulfiller pull.
async fn gen_capability_token(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<String> {
    let row = sqlx::query("SELECT lower(hex(randomblob(32))) AS t")
        .fetch_one(&mut **tx)
        .await?;
    Ok(row.try_get::<String, _>("t")?)
}

/// Render one secret-request row as safe metadata: never the ciphertext, never the tokens.
/// `recipients` is parsed back from its JSON string into an array.
fn secret_request_meta(row: &SqliteRow) -> Value {
    let recipients: Value = row
        .try_get::<String, _>("recipients")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!([]));
    json!({
        "id": row.try_get::<i64, _>("id").unwrap_or_default(),
        "name": row.try_get::<String, _>("name").unwrap_or_default(),
        "requested_by": row.try_get::<Option<String>, _>("requested_by").unwrap_or(None),
        "fulfiller": row.try_get::<Option<String>, _>("fulfiller").unwrap_or(None),
        "status": row.try_get::<String, _>("status").unwrap_or_default(),
        "recipients": recipients,
        "instructions": row.try_get::<Option<String>, _>("instructions").unwrap_or(None),
        "target": row.try_get::<Option<String>, _>("target").unwrap_or(None),
        "created_at": row.try_get::<Option<String>, _>("created_at").unwrap_or(None),
        "submitted_at": row.try_get::<Option<String>, _>("submitted_at").unwrap_or(None),
        "expires_at": row.try_get::<Option<String>, _>("expires_at").unwrap_or(None),
    })
}

/// Purge submitted-but-unfulfilled requests whose TTL has passed (the transient-ciphertext window),
/// emitting a metadata-only `secret.expired` for each. Called before any read so a stale request
/// disappears rather than lingering with ciphertext at rest.
async fn purge_expired_secret_requests(pool: &Pool) -> anyhow::Result<()> {
    let now = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let expired = sqlx::query(
        "SELECT id, name, fulfiller FROM secret_requests \
         WHERE status='submitted' AND expires_at IS NOT NULL AND expires_at < ?",
    )
    .bind(&now)
    .fetch_all(&mut *tx)
    .await?;
    for row in &expired {
        let id: i64 = row.try_get("id")?;
        let name: String = row.try_get("name")?;
        let fulfiller: Option<String> = row.try_get("fulfiller")?;
        sqlx::query("DELETE FROM secret_requests WHERE id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let mut recips = BTreeSet::new();
        if let Some(f) = &fulfiller {
            recips.insert(f.clone());
        }
        emit(
            &mut tx,
            &mut hooks,
            "secret.expired",
            None,
            None,
            None,
            None,
            None,
            json!({ "id": id, "name": name }),
            Recipients::Explicit(recips),
        )
        .await?;
    }
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(())
}

/// File a secret request. Returns the request metadata plus the `submit_url` (a relative path
/// carrying the single-use capability token — the requester prefixes the board's public base and
/// hands the link to the operator) and the `fulfiller_token` the fulfiller uses to pull + fulfill.
pub async fn create_secret_request(
    pool: &Pool,
    name: &str,
    recipients: &[String],
    instructions: Option<&str>,
    target: Option<&str>,
    fulfiller: Option<&str>,
    requested_by: Option<&str>,
) -> anyhow::Result<Value> {
    if name.trim().is_empty() {
        anyhow::bail!("give a name for the secret request");
    }
    let ts = now_iso();
    let recipients_json = serde_json::to_string(recipients)?;
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let submit_token = gen_capability_token(&mut tx).await?;
    let fulfiller_token = gen_capability_token(&mut tx).await?;
    let id: i64 = sqlx::query(
        "INSERT INTO secret_requests(name, requested_by, fulfiller, status, recipients, \
         instructions, target, submit_token, fulfiller_token, submit_used, created_at) \
         VALUES(?,?,?,'requested',?,?,?,?,?,0,?) RETURNING id",
    )
    .bind(name)
    .bind(requested_by)
    .bind(fulfiller)
    .bind(&recipients_json)
    .bind(instructions)
    .bind(target)
    .bind(&submit_token)
    .bind(&fulfiller_token)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    // Audit only (no ciphertext exists yet); notify the fulfiller if one is named so they know a
    // request is pending, but nothing is actionable until it's submitted.
    let mut recips = BTreeSet::new();
    if let Some(f) = fulfiller {
        recips.insert(f.to_string());
    }
    emit(
        &mut tx,
        &mut hooks,
        "secret.requested",
        requested_by,
        None,
        None,
        None,
        None,
        json!({ "id": id, "name": name, "fulfiller": fulfiller }),
        Recipients::Explicit(recips),
    )
    .await?;
    let row = sqlx::query("SELECT * FROM secret_requests WHERE id=?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let mut out = secret_request_meta(&row);
    if let Value::Object(ref mut m) = out {
        m.insert(
            "submit_url".into(),
            json!(format!("/secret-requests/{id}?t={submit_token}")),
        );
        m.insert("submit_token".into(), json!(submit_token));
        m.insert("fulfiller_token".into(), json!(fulfiller_token));
    }
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// One secret request as safe metadata (drives the submit page: name, instructions, recipients,
/// status). Never returns ciphertext or tokens. Purges expired requests first.
pub async fn get_secret_request(pool: &Pool, id: i64) -> anyhow::Result<Value> {
    purge_expired_secret_requests(pool).await?;
    let row = sqlx::query("SELECT * FROM secret_requests WHERE id=?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => Ok(secret_request_meta(&r)),
        None => anyhow::bail!("no secret request {id}"),
    }
}

/// All secret requests as safe metadata (never ciphertext or tokens). Purges expired first.
pub async fn list_secret_requests(pool: &Pool) -> anyhow::Result<Value> {
    purge_expired_secret_requests(pool).await?;
    let rows = sqlx::query("SELECT * FROM secret_requests ORDER BY id DESC")
        .fetch_all(pool)
        .await?;
    Ok(Value::Array(rows.iter().map(secret_request_meta).collect()))
}

/// Submit the (browser-encrypted) ciphertext for a request. Gated by the single-use submit token:
/// the link works once, then is spent. Stores the ciphertext transiently, flips the request to
/// `submitted` with a TTL, and directly notifies the fulfiller (inbox + their webhook) so they can
/// pull + relocate. The event carries metadata only — never the ciphertext.
pub async fn submit_secret(
    pool: &Pool,
    id: i64,
    token: &str,
    ciphertext: &str,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let expires_at = (chrono::Utc::now() + SECRET_SUBMIT_TTL)
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let row = sqlx::query(
        "SELECT submit_token, submit_used, status, name, fulfiller FROM secret_requests WHERE id=?",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        anyhow::bail!("no secret request {id}")
    };
    let stored_token: String = row.try_get("submit_token")?;
    let submit_used: i64 = row.try_get("submit_used")?;
    let status: String = row.try_get("status")?;
    let name: String = row.try_get("name")?;
    let fulfiller: Option<String> = row.try_get("fulfiller")?;
    // Constant-length compare is unnecessary here (the token is high-entropy and single-use), but
    // reject a wrong or spent token, and reject a re-submit onto an already-submitted request.
    if token != stored_token {
        anyhow::bail!("invalid submit token");
    }
    if submit_used != 0 {
        anyhow::bail!("submit link already used");
    }
    if status != "requested" {
        anyhow::bail!("secret request is not awaiting submission (status: {status})");
    }
    sqlx::query(
        "UPDATE secret_requests SET ciphertext=?, status='submitted', submitted_at=?, \
         expires_at=?, submit_used=1 WHERE id=?",
    )
    .bind(ciphertext)
    .bind(&ts)
    .bind(&expires_at)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let mut recips = BTreeSet::new();
    if let Some(f) = &fulfiller {
        recips.insert(f.clone());
    }
    emit(
        &mut tx,
        &mut hooks,
        "secret.submitted",
        None,
        None,
        None,
        None,
        None,
        json!({ "id": id, "name": name, "fulfiller": fulfiller, "expires_at": expires_at }),
        Recipients::Explicit(recips),
    )
    .await?;
    let out = sqlx::query("SELECT * FROM secret_requests WHERE id=?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let meta = secret_request_meta(&out);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(meta)
}

/// The fulfiller pulls the ciphertext ONCE to relocate it into durable storage. Gated by the
/// fulfiller token; only valid while the request is `submitted`. Returns `{id, name, ciphertext}` —
/// the one place ciphertext leaves the board, to an authorized holder. Purges expired first.
pub async fn get_secret_ciphertext(pool: &Pool, id: i64, token: &str) -> anyhow::Result<Value> {
    purge_expired_secret_requests(pool).await?;
    let row = sqlx::query(
        "SELECT fulfiller_token, status, name, ciphertext FROM secret_requests WHERE id=?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        anyhow::bail!("no secret request {id}")
    };
    let stored_token: String = row.try_get("fulfiller_token")?;
    if token != stored_token {
        anyhow::bail!("invalid fulfiller token");
    }
    let status: String = row.try_get("status")?;
    let ciphertext: Option<String> = row.try_get("ciphertext")?;
    match (status.as_str(), ciphertext) {
        ("submitted", Some(ct)) => Ok(json!({
            "id": id,
            "name": row.try_get::<String, _>("name").unwrap_or_default(),
            "ciphertext": ct,
        })),
        _ => anyhow::bail!("secret request {id} has no ciphertext to pull (status: {status})"),
    }
}

/// Fulfill a request: the fulfiller has relocated the secret to durable storage, so the board
/// deletes the row (and its transient ciphertext) and emits a metadata-only `secret.fulfilled`.
/// Idempotent: fulfilling an already-deleted request succeeds. Gated by the fulfiller token when
/// the row still exists.
pub async fn fulfill_secret(pool: &Pool, id: i64, token: &str) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let row = sqlx::query(
        "SELECT fulfiller_token, name, fulfiller, requested_by FROM secret_requests WHERE id=?",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        // Already gone — idempotent success (a retried fulfill after the row was deleted).
        tx.commit().await?;
        return Ok(json!({ "fulfilled": true, "id": id, "already": true }));
    };
    let stored_token: String = row.try_get("fulfiller_token")?;
    if token != stored_token {
        anyhow::bail!("invalid fulfiller token");
    }
    let name: String = row.try_get("name")?;
    let fulfiller: Option<String> = row.try_get("fulfiller")?;
    let requested_by: Option<String> = row.try_get("requested_by")?;
    sqlx::query("DELETE FROM secret_requests WHERE id=?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let mut recips = BTreeSet::new();
    if let Some(r) = &requested_by {
        recips.insert(r.clone());
    }
    emit(
        &mut tx,
        &mut hooks,
        "secret.fulfilled",
        fulfiller.as_deref(),
        None,
        None,
        None,
        None,
        json!({ "id": id, "name": name }),
        Recipients::Explicit(recips),
    )
    .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(json!({ "fulfilled": true, "id": id }))
}

/// Cancel (delete) a pending secret request — before submission or to abandon one. Deletes the row
/// (and any transient ciphertext) and emits a metadata-only `secret.cancelled`. Idempotent.
pub async fn cancel_secret_request(
    pool: &Pool,
    id: i64,
    actor: Option<&str>,
) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let row = sqlx::query("SELECT name, fulfiller FROM secret_requests WHERE id=?")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(json!({ "cancelled": true, "id": id, "already": true }));
    };
    let name: String = row.try_get("name")?;
    let fulfiller: Option<String> = row.try_get("fulfiller")?;
    sqlx::query("DELETE FROM secret_requests WHERE id=?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let mut recips = BTreeSet::new();
    if let Some(f) = &fulfiller {
        recips.insert(f.clone());
    }
    emit(
        &mut tx,
        &mut hooks,
        "secret.cancelled",
        actor,
        None,
        None,
        None,
        None,
        json!({ "id": id, "name": name }),
        Recipients::Explicit(recips),
    )
    .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(json!({ "cancelled": true, "id": id }))
}

// --- Reviews (Document #5, increment 1): a typed review over an artifact, with an A2 lifecycle
// state machine and a single append-only log. See src/db.rs for the schema + rationale. ---

/// The five A2 lifecycle states. `open` (created/linked, not yet active), `in_review` (reviewers
/// active; for a document this is submit-for-review, which spawns adversarial reviewers),
/// `changes_requested` (issues raised, author revises, cycles back), `approved` (positive
/// concluding), `closed` (non-approval concluding). approved/closed are conventionally terminal,
/// but a reopen (e.g. a reopened GitHub PR) is permitted — the machine validates that a status is
/// a KNOWN A2 state rather than forbidding transitions between them. Deliberately permissive: the
/// github-bridge adapter maps whatever an upstream PR does onto set_review_status, and the board
/// must not reject a legitimate upstream transition. A same-status set is an idempotent no-op.
const REVIEW_STATUSES: [&str; 5] = [
    "open",
    "in_review",
    "changes_requested",
    "approved",
    "closed",
];

/// The A1 log entry types. A finding is an entry of type `finding` (NOT a separate collection);
/// an actionable finding links a child `task_id`. Any count/trend (open findings, etc.) is
/// derived by reading the log in order — the log is the single source of truth.
const REVIEW_LOG_TYPES: [&str; 8] = [
    "submitted",
    "revised",
    "finding",
    "finding_resolved",
    "comment",
    "state_change",
    "adversarial_review",
    "decision",
];

fn is_review_status(s: &str) -> bool {
    REVIEW_STATUSES.contains(&s)
}
fn is_review_log_type(s: &str) -> bool {
    REVIEW_LOG_TYPES.contains(&s)
}

/// Turn a review row's JSON into a normalized object: `metadata` TEXT parsed into an object
/// (mirrors get_task/get_project) and `vetted` INTEGER surfaced as a bool.
fn normalize_review_obj(mut obj: Map<String, Value>) -> Value {
    let meta = obj
        .get("metadata")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| json!({}));
    obj.insert("metadata".into(), meta);
    if let Some(v) = obj.get("vetted").and_then(|v| v.as_i64()) {
        obj.insert("vetted".into(), json!(v != 0));
    }
    Value::Object(obj)
}

/// Fetch one review as JSON: its columns (normalized via `normalize_review_obj`) and, when
/// `with_log`, its `log` array ordered oldest-first. Returns None if the review doesn't exist.
async fn review_json(
    tx: &mut Transaction<'_, Sqlite>,
    review_id: i64,
    with_log: bool,
) -> anyhow::Result<Option<Value>> {
    let Some(row) = sqlx::query("SELECT * FROM reviews WHERE id=?")
        .bind(review_id)
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(None);
    };
    let obj = match row_to_json(&row) {
        Value::Object(m) => m,
        other => return Ok(Some(other)),
    };
    let mut out = normalize_review_obj(obj);
    if with_log {
        let rows = sqlx::query("SELECT * FROM review_log WHERE review_id=? ORDER BY id ASC")
            .bind(review_id)
            .fetch_all(&mut **tx)
            .await?;
        let log: Vec<Value> = rows.iter().map(row_to_json).collect();
        if let Value::Object(ref mut m) = out {
            m.insert("log".into(), Value::Array(log));
        }
    }
    Ok(Some(out))
}

/// Who hears about a review event: its creator and its assignee (reviewers), minus the actor.
/// The whole-board firehose union in `emit` adds board subscribers on top. Increment 1 has no
/// per-review subscription table; a richer links/watchers model arrives in a later increment.
fn review_recipients(
    created_by: Option<&str>,
    assignee: Option<&str>,
    actor: Option<&str>,
) -> BTreeSet<String> {
    let mut recips = BTreeSet::new();
    if let Some(c) = created_by {
        recips.insert(c.to_string());
    }
    if let Some(a) = assignee {
        recips.insert(a.to_string());
    }
    if let Some(actor) = actor {
        recips.remove(actor);
    }
    recips
}

/// Create a review over an artifact. Idempotent external ingest (task #270 pattern): if
/// `external_link` is given and a review is ALREADY linked on (source, external_id), return that
/// existing review with `created:false` — so a bridge replaying the same upstream PR never
/// duplicates. `status` defaults to `open`; a caller may seed another A2 state (validated). Emits
/// `review.created` and records the initial `submitted` log entry. Returns the review object
/// (including its `log`) with a `created` flag.
#[allow(clippy::too_many_arguments)]
pub async fn create_review(
    pool: &Pool,
    kind: &str,
    source: Option<&str>,
    target_ref: Option<&str>,
    title: Option<&str>,
    status: Option<&str>,
    created_by: Option<&str>,
    assignee: Option<&str>,
    metadata: Option<Value>,
    external_link: Option<ExternalRef>,
) -> anyhow::Result<Value> {
    let status = status.unwrap_or("open");
    if !is_review_status(status) {
        anyhow::bail!(
            "unknown review status '{status}' (expected one of: {})",
            REVIEW_STATUSES.join(", ")
        );
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    // Idempotent ingest: an existing link -> return that review, created:false. The
    // SELECT-then-INSERT is atomic under one tx (the pool serializes writers), so a retrying
    // adapter can't race two reviews in.
    if let Some(ext) = &external_link {
        if let Some(row) = sqlx::query(
            "SELECT board_id FROM external_links WHERE source=? AND external_id=? AND board_kind='review'",
        )
        .bind(&ext.source)
        .bind(&ext.external_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            let existing: i64 = row.try_get("board_id")?;
            let mut out = review_json(&mut tx, existing, true).await?.unwrap_or(Value::Null);
            if let Value::Object(ref mut m) = out {
                m.insert("created".into(), json!(false));
            }
            tx.commit().await?;
            return Ok(out);
        }
    }
    let meta_str = metadata.unwrap_or_else(|| json!({})).to_string();
    let rid: i64 = sqlx::query(
        "INSERT INTO reviews(kind, source, target_ref, status, title, vetted, created_by, assignee, metadata, created_at, updated_at) \
         VALUES(?,?,?,?,?,0,?,?,?,?,?) RETURNING id",
    )
    .bind(kind)
    .bind(source)
    .bind(target_ref)
    .bind(status)
    .bind(title)
    .bind(created_by)
    .bind(assignee)
    .bind(&meta_str)
    .bind(&ts)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    // Initial log entry: the review was submitted/opened (A2 — every lifecycle event is a log row).
    sqlx::query(
        "INSERT INTO review_log(review_id, entry_type, body, author, external_id, task_id, created_at) \
         VALUES(?,?,?,?,NULL,NULL,?)",
    )
    .bind(rid)
    .bind("submitted")
    .bind(title)
    .bind(created_by)
    .bind(&ts)
    .execute(&mut *tx)
    .await?;
    let recips = review_recipients(created_by, assignee, created_by);
    emit(
        &mut tx,
        &mut hooks,
        "review.created",
        created_by,
        None,
        None,
        None,
        None,
        json!({ "review_id": rid, "kind": kind, "title": title, "status": status }),
        Recipients::Explicit(recips),
    )
    .await?;
    // Record the external dedup link atomically with the create (a matching row was ruled out
    // above, and the pool serializes writers).
    if let Some(ext) = &external_link {
        sqlx::query(
            "INSERT INTO external_links(source, external_id, external_parent_id, board_kind, board_id, metadata, created_at, updated_at) \
             VALUES(?,?,?,'review',?,'{}',?,?)",
        )
        .bind(&ext.source)
        .bind(&ext.external_id)
        .bind(&ext.external_parent_id)
        .bind(rid)
        .bind(&ts)
        .bind(&ts)
        .execute(&mut *tx)
        .await?;
    }
    let mut out = review_json(&mut tx, rid, true)
        .await?
        .unwrap_or(Value::Null);
    if let Value::Object(ref mut m) = out {
        m.insert("created".into(), json!(true));
    }
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Fetch one review with its full log. Bails if it doesn't exist.
pub async fn get_review(pool: &Pool, review_id: i64) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let out = review_json(&mut tx, review_id, true).await?;
    tx.commit().await?;
    match out {
        Some(v) => Ok(v),
        None => anyhow::bail!("no review {review_id}"),
    }
}

/// List reviews (newest-touched first), optionally filtered by status, kind, and/or assignee.
/// Returns `{ "reviews": [...] }`; each review is normalized but WITHOUT its log (fetch one with
/// get_review for the timeline).
pub async fn list_reviews(
    pool: &Pool,
    status: Option<&str>,
    kind: Option<&str>,
    assignee: Option<&str>,
) -> anyhow::Result<Value> {
    let mut sql = String::from("SELECT * FROM reviews");
    let mut conds: Vec<&str> = Vec::new();
    if status.is_some() {
        conds.push("status=?");
    }
    if kind.is_some() {
        conds.push("kind=?");
    }
    if assignee.is_some() {
        conds.push("assignee=?");
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY updated_at DESC, id DESC");
    let mut q = sqlx::query(&sql);
    if let Some(s) = status {
        q = q.bind(s.to_string());
    }
    if let Some(k) = kind {
        q = q.bind(k.to_string());
    }
    if let Some(a) = assignee {
        q = q.bind(a.to_string());
    }
    let rows = q.fetch_all(pool).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| match row_to_json(row) {
            Value::Object(m) => normalize_review_obj(m),
            other => other,
        })
        .collect();
    Ok(json!({ "reviews": items }))
}

/// Transition a review to a new A2 status. A no-op if the review is already in `new_status`
/// (returns it unchanged, emits nothing, writes no log entry) — so a bridge re-applying the same
/// upstream state is idempotent. Rejects an unknown status. On a real transition it updates the
/// status, appends a `state_change` log entry, and emits `review.status_changed`, plus
/// `review.opened_for_review` when entering `in_review` and `review.terminal` when entering
/// `approved`/`closed`.
pub async fn set_review_status(
    pool: &Pool,
    review_id: i64,
    new_status: &str,
    actor: Option<&str>,
    note: Option<&str>,
) -> anyhow::Result<Value> {
    if !is_review_status(new_status) {
        anyhow::bail!(
            "unknown review status '{new_status}' (expected one of: {})",
            REVIEW_STATUSES.join(", ")
        );
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let Some(row) = sqlx::query("SELECT status, created_by, assignee FROM reviews WHERE id=?")
        .bind(review_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        anyhow::bail!("no review {review_id}");
    };
    let old_status: String = row.try_get("status")?;
    let created_by: Option<String> = row.try_get("created_by")?;
    let assignee: Option<String> = row.try_get("assignee")?;
    // Same status -> idempotent no-op (no log entry, no event).
    if old_status == new_status {
        let out = review_json(&mut tx, review_id, true)
            .await?
            .unwrap_or(Value::Null);
        tx.commit().await?;
        return Ok(out);
    }
    sqlx::query("UPDATE reviews SET status=?, updated_at=? WHERE id=?")
        .bind(new_status)
        .bind(&ts)
        .bind(review_id)
        .execute(&mut *tx)
        .await?;
    // Canonical, parseable transition prefix ("{old} -> {new}") so the improvement-trend reader
    // can reconstruct each review's status history from the log alone; an optional note follows.
    let body = match note {
        Some(n) => format!("{old_status} -> {new_status}: {n}"),
        None => format!("{old_status} -> {new_status}"),
    };
    sqlx::query(
        "INSERT INTO review_log(review_id, entry_type, body, author, external_id, task_id, created_at) \
         VALUES(?,?,?,?,NULL,NULL,?)",
    )
    .bind(review_id)
    .bind("state_change")
    .bind(&body)
    .bind(actor)
    .bind(&ts)
    .execute(&mut *tx)
    .await?;
    let recips = review_recipients(created_by.as_deref(), assignee.as_deref(), actor);
    emit(
        &mut tx,
        &mut hooks,
        "review.status_changed",
        actor,
        None,
        None,
        None,
        None,
        json!({ "review_id": review_id, "from": old_status, "to": new_status, "note": note }),
        Recipients::Explicit(recips.clone()),
    )
    .await?;
    if new_status == "in_review" {
        emit(
            &mut tx,
            &mut hooks,
            "review.opened_for_review",
            actor,
            None,
            None,
            None,
            None,
            json!({ "review_id": review_id }),
            Recipients::Explicit(recips.clone()),
        )
        .await?;
    }
    if new_status == "approved" || new_status == "closed" {
        emit(
            &mut tx,
            &mut hooks,
            "review.terminal",
            actor,
            None,
            None,
            None,
            None,
            json!({ "review_id": review_id, "status": new_status }),
            Recipients::Explicit(recips),
        )
        .await?;
    }
    let out = review_json(&mut tx, review_id, true)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Set (or clear) a review's `vetted` flag — the adversarial-review gate (A1): "adversarial review
/// was run AND its findings were addressed." Per the D17 decision this is an AUDIT-CONTRACT, not an
/// identity gate: the board does NOT check whether the caller is a person (actor is free-text), it
/// records WHO set it. Every real change durably logs a `decision` entry (body "vetted: {old} ->
/// {new}", author = actor) so any vetted flip is auditable after the fact, and emits
/// `review.vetted_changed`. Setting vetted to its current value is an idempotent no-op (no log, no
/// event). The concluding lifecycle transition stays with set_review_status; this only moves the
/// gate flag.
pub async fn set_review_vetted(
    pool: &Pool,
    review_id: i64,
    vetted: bool,
    actor: Option<&str>,
    note: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let Some(row) = sqlx::query("SELECT vetted, created_by, assignee FROM reviews WHERE id=?")
        .bind(review_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        anyhow::bail!("no review {review_id}");
    };
    let old_vetted: bool = row.try_get::<i64, _>("vetted")? != 0;
    let created_by: Option<String> = row.try_get("created_by")?;
    let assignee: Option<String> = row.try_get("assignee")?;
    // Same value -> idempotent no-op (no audit entry, no event).
    if old_vetted == vetted {
        let out = review_json(&mut tx, review_id, true)
            .await?
            .unwrap_or(Value::Null);
        tx.commit().await?;
        return Ok(out);
    }
    sqlx::query("UPDATE reviews SET vetted=?, updated_at=? WHERE id=?")
        .bind(i64::from(vetted))
        .bind(&ts)
        .bind(review_id)
        .execute(&mut *tx)
        .await?;
    // Durable audit entry: WHO changed the gate and from/to. entry_type=decision (not state_change,
    // so the improvement-trend's transition parser never mistakes it for a status transition).
    let body = match note {
        Some(n) => format!("vetted: {old_vetted} -> {vetted}: {n}"),
        None => format!("vetted: {old_vetted} -> {vetted}"),
    };
    sqlx::query(
        "INSERT INTO review_log(review_id, entry_type, body, author, external_id, task_id, created_at) \
         VALUES(?,?,?,?,NULL,NULL,?)",
    )
    .bind(review_id)
    .bind("decision")
    .bind(&body)
    .bind(actor)
    .bind(&ts)
    .execute(&mut *tx)
    .await?;
    let recips = review_recipients(created_by.as_deref(), assignee.as_deref(), actor);
    emit(
        &mut tx,
        &mut hooks,
        "review.vetted_changed",
        actor,
        None,
        None,
        None,
        None,
        json!({ "review_id": review_id, "vetted": vetted }),
        Recipients::Explicit(recips),
    )
    .await?;
    let out = review_json(&mut tx, review_id, true)
        .await?
        .unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Append an entry to a review's log. Idempotent on `external_id` when given: if an entry with
/// the same external_id already exists on this review, return it with `appended:false` — so a
/// bridge replaying the same upstream comment/finding never double-logs. `entry_type` is one of
/// the A1 log types (validated); a `finding` may carry a `task_id` linking the child task it
/// spawned. Emits `review.log_appended`.
#[allow(clippy::too_many_arguments)]
pub async fn append_review_log(
    pool: &Pool,
    review_id: i64,
    entry_type: &str,
    body: Option<&str>,
    author: Option<&str>,
    task_id: Option<i64>,
    external_id: Option<&str>,
) -> anyhow::Result<Value> {
    if !is_review_log_type(entry_type) {
        anyhow::bail!(
            "unknown review log type '{entry_type}' (expected one of: {})",
            REVIEW_LOG_TYPES.join(", ")
        );
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let Some(row) = sqlx::query("SELECT created_by, assignee FROM reviews WHERE id=?")
        .bind(review_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        anyhow::bail!("no review {review_id}");
    };
    let created_by: Option<String> = row.try_get("created_by")?;
    let assignee: Option<String> = row.try_get("assignee")?;
    // Idempotent on external_id (scoped to this review): a replayed upstream item returns the
    // existing entry rather than logging a duplicate.
    if let Some(ext) = external_id {
        if let Some(existing) =
            sqlx::query("SELECT id FROM review_log WHERE review_id=? AND external_id=?")
                .bind(review_id)
                .bind(ext)
                .fetch_optional(&mut *tx)
                .await?
        {
            let eid: i64 = existing.try_get("id")?;
            tx.commit().await?;
            return Ok(json!({ "review_id": review_id, "entry_id": eid, "appended": false }));
        }
    }
    let eid: i64 = sqlx::query(
        "INSERT INTO review_log(review_id, entry_type, body, author, external_id, task_id, created_at) \
         VALUES(?,?,?,?,?,?,?) RETURNING id",
    )
    .bind(review_id)
    .bind(entry_type)
    .bind(body)
    .bind(author)
    .bind(external_id)
    .bind(task_id)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    let recips = review_recipients(created_by.as_deref(), assignee.as_deref(), author);
    emit(
        &mut tx,
        &mut hooks,
        "review.log_appended",
        author,
        None,
        None,
        None,
        None,
        json!({ "review_id": review_id, "entry_id": eid, "entry_type": entry_type, "task_id": task_id }),
        Recipients::Explicit(recips),
    )
    .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(
        json!({ "review_id": review_id, "entry_id": eid, "appended": true, "entry_type": entry_type }),
    )
}

// --- Reviews: improvement trend (Document #5, increment 5 / BUILD 5) ---

/// Parse a state_change log body of the canonical form "{from} -> {to}" (optionally followed by
/// ": {note}") into (from, to). Returns None for a legacy/free-form body that isn't a transition.
fn parse_transition(body: &str) -> Option<(&str, &str)> {
    let (from, rest) = body.split_once(" -> ")?;
    let to = rest.split(':').next().unwrap_or(rest).trim();
    Some((from.trim(), to))
}

fn is_terminal_status(s: &str) -> bool {
    s == "approved" || s == "closed"
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Per-review derived numbers the trend aggregates over — all read from the generic log + metadata,
/// never a stored counter.
struct ReviewAgg {
    kind: String,
    area: String,
    findings: u64,
    /// Escaped-defect score: post-approval findings + (reopened as 1) + (lineage follow-up as 1).
    escaped: u64,
    post_approval_findings: u64,
    reopened: bool,
    lineage_followup: bool,
}

/// Summarize a set of reviews (already in created_at-ascending order) into a trend slice: total
/// findings + findings-per-review, the escaped-defect breakdown, and an earlier-vs-later split so a
/// FALL in findings BESIDE a RISE in escaped defects is `flagged` (looks like improvement but isn't)
/// rather than counted as improvement.
fn trend_slice(aggs: &[&ReviewAgg]) -> Value {
    let n = aggs.len();
    let findings: u64 = aggs.iter().map(|a| a.findings).sum();
    let escaped: u64 = aggs.iter().map(|a| a.escaped).sum();
    let post: u64 = aggs.iter().map(|a| a.post_approval_findings).sum();
    let reopens: u64 = aggs.iter().filter(|a| a.reopened).count() as u64;
    let lineage: u64 = aggs.iter().filter(|a| a.lineage_followup).count() as u64;
    let fpr = if n > 0 {
        findings as f64 / n as f64
    } else {
        0.0
    };

    let eps = 1e-9;
    let (findings_trend, escaped_trend, flagged, earlier_v, later_v) = if n >= 2 {
        let mid = n / 2;
        let (e, l) = (&aggs[..mid], &aggs[mid..]);
        let mean = |s: &[&ReviewAgg], f: fn(&ReviewAgg) -> u64| {
            s.iter().map(|a| f(a)).sum::<u64>() as f64 / s.len() as f64
        };
        let (efpr, lfpr) = (mean(e, |a| a.findings), mean(l, |a| a.findings));
        let (eepr, lepr) = (mean(e, |a| a.escaped), mean(l, |a| a.escaped));
        // Fewer findings later = improving; more = worsening.
        let ft = if lfpr + eps < efpr {
            "improving"
        } else if lfpr > efpr + eps {
            "worsening"
        } else {
            "flat"
        };
        // More escaped defects later = rising (bad); fewer = falling.
        let et = if lepr > eepr + eps {
            "rising"
        } else if lepr + eps < eepr {
            "falling"
        } else {
            "flat"
        };
        let fl = ft == "improving" && et == "rising";
        (
            ft,
            et,
            fl,
            json!({ "reviews": e.len(), "findings_per_review": round2(efpr), "escaped_per_review": round2(eepr) }),
            json!({ "reviews": l.len(), "findings_per_review": round2(lfpr), "escaped_per_review": round2(lepr) }),
        )
    } else {
        (
            "insufficient_data",
            "insufficient_data",
            false,
            Value::Null,
            Value::Null,
        )
    };

    json!({
        "reviews": n,
        "findings": findings,
        "findings_per_review": round2(fpr),
        "escaped_defects": {
            "total": escaped,
            "post_approval_findings": post,
            "reopens": reopens,
            "lineage_followups": lineage,
        },
        "earlier": earlier_v,
        "later": later_v,
        "findings_trend": findings_trend,
        "escaped_trend": escaped_trend,
        "flagged": flagged,
    })
}

/// The improvement reading (Document #5, A5/A6), derived entirely from each review's generic log +
/// metadata — no stored counter, no separate reporting store. For each review it counts `finding`
/// entries and three escaped-defect signals (a finding logged AFTER approval; a re-open, i.e. a
/// transition back out of a terminal state; and a lineage follow-up, i.e. a review that declares a
/// predecessor and still surfaced findings). It then reports findings-per-review and an
/// earlier-vs-later trend — overall and sliced by review `kind` and by producing `area`/agent — and
/// `flagged`s any slice where findings fell while escaped defects rose. Optionally filter to one
/// `kind` and/or one `area`.
pub async fn review_improvement_trend(
    pool: &Pool,
    kind: Option<&str>,
    area: Option<&str>,
) -> anyhow::Result<Value> {
    let mut sql = String::from("SELECT id, kind, created_by, metadata FROM reviews");
    if kind.is_some() {
        sql.push_str(" WHERE kind=?");
    }
    // created_at-ascending so a slice's earlier/later split is chronological without re-sorting.
    sql.push_str(" ORDER BY created_at ASC, id ASC");
    let mut q = sqlx::query(&sql);
    if let Some(k) = kind {
        q = q.bind(k.to_string());
    }
    let rows = q.fetch_all(pool).await?;

    let mut aggs: Vec<ReviewAgg> = Vec::new();
    for row in &rows {
        let rid: i64 = row.try_get("id")?;
        let rkind: String = row.try_get("kind")?;
        let created_by: Option<String> = row.try_get("created_by")?;
        let meta_str: String = row.try_get("metadata")?;
        let meta: Value = serde_json::from_str(&meta_str).unwrap_or_else(|_| json!({}));
        // Producing area: metadata.area, else metadata.produced_by, else the creator, else unknown.
        let r_area = meta
            .get("area")
            .and_then(|v| v.as_str())
            .or_else(|| meta.get("produced_by").and_then(|v| v.as_str()))
            .map(str::to_string)
            .or_else(|| created_by.clone())
            .unwrap_or_else(|| "unknown".into());
        if let Some(a) = area {
            if r_area != a {
                continue;
            }
        }
        let has_predecessor = ["predecessor_review_id", "predecessor", "predecessor_id"]
            .iter()
            .any(|k| meta.get(*k).map(|v| !v.is_null()).unwrap_or(false));

        let logs = sqlx::query(
            "SELECT entry_type, body, created_at FROM review_log WHERE review_id=? ORDER BY id ASC",
        )
        .bind(rid)
        .fetch_all(pool)
        .await?;
        let mut findings: u64 = 0;
        let mut finding_times: Vec<String> = Vec::new();
        let mut approved_at: Option<String> = None;
        let mut seen_terminal = false;
        let mut reopened = false;
        for lg in &logs {
            let et: String = lg.try_get("entry_type")?;
            let lts: String = lg.try_get("created_at")?;
            match et.as_str() {
                "finding" => {
                    findings += 1;
                    finding_times.push(lts);
                }
                "state_change" => {
                    let body: String = lg.try_get::<Option<String>, _>("body")?.unwrap_or_default();
                    if let Some((_from, to)) = parse_transition(&body) {
                        if to == "approved" && approved_at.is_none() {
                            approved_at = Some(lts);
                        }
                        if is_terminal_status(to) {
                            seen_terminal = true;
                        } else if seen_terminal {
                            reopened = true;
                        }
                    }
                }
                _ => {}
            }
        }
        // Timestamps share now_iso()'s fixed RFC3339 (micros + 'Z') shape, so lexicographic > is
        // chronologically after.
        let post_approval_findings = match &approved_at {
            Some(at) => finding_times
                .iter()
                .filter(|t| t.as_str() > at.as_str())
                .count() as u64,
            None => 0,
        };
        let lineage_followup = has_predecessor && findings > 0;
        let escaped = post_approval_findings + u64::from(reopened) + u64::from(lineage_followup);
        aggs.push(ReviewAgg {
            kind: rkind,
            area: r_area,
            findings,
            escaped,
            post_approval_findings,
            reopened,
            lineage_followup,
        });
    }

    let overall = trend_slice(&aggs.iter().collect::<Vec<_>>());
    let mut by_kind_map: std::collections::BTreeMap<String, Vec<&ReviewAgg>> = Default::default();
    let mut by_area_map: std::collections::BTreeMap<String, Vec<&ReviewAgg>> = Default::default();
    for a in &aggs {
        by_kind_map.entry(a.kind.clone()).or_default().push(a);
        by_area_map.entry(a.area.clone()).or_default().push(a);
    }
    let by_kind: Vec<Value> = by_kind_map
        .iter()
        .map(|(k, v)| {
            let mut s = trend_slice(v);
            if let Value::Object(ref mut m) = s {
                m.insert("kind".into(), json!(k));
            }
            s
        })
        .collect();
    let by_area: Vec<Value> = by_area_map
        .iter()
        .map(|(k, v)| {
            let mut s = trend_slice(v);
            if let Value::Object(ref mut m) = s {
                m.insert("area".into(), json!(k));
            }
            s
        })
        .collect();

    Ok(json!({
        "filters": { "kind": kind, "area": area },
        "overall": overall,
        "by_kind": by_kind,
        "by_area": by_area,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Port of scripts/smoke.py: exercises the core flow and asserts the exact
    /// notification semantics (planner hears 3, fixer hears 2, no self-notifications).
    #[tokio::test]
    async fn smoke() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("board.db");
        let pool = crate::db::init(db_path.to_str().unwrap()).await?;

        register_agent(&pool, "planner", Some("Planner"), None, None, None, None).await?;
        register_agent(&pool, "fixer", Some("Fixer"), None, None, None, None).await?;

        let p = create_project(
            &pool,
            "Voron tuning",
            Some("dial in the printer"),
            Some("planner"),
            None,
        )
        .await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            pid,
            "Calibrate pressure advance",
            None,
            Some("fixer"),
            None,
            Some("planner"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();

        subscribe(&pool, "planner", Some(tid), None, None, None, false).await?; // (already auto-subscribed as creator)
        comment_task(
            &pool,
            tid,
            "Start from PA=0.03",
            Some("planner"),
            None,
            None,
        )
        .await?;
        update_task(
            &pool,
            tid,
            Some("in_progress"),
            None,
            None,
            None,
            None,
            Some("fixer"),
            None,
            None,
            None,
        )
        .await?;
        update_task(
            &pool,
            tid,
            Some("done"),
            None,
            None,
            None,
            None,
            Some("fixer"),
            None,
            None,
            None,
        )
        .await?;
        send_message(&pool, "fixer", "planner", "PA done, landed at 0.032").await?;

        // planner should hear: 2 status changes (by fixer) + 1 DM = 3; NOT its own comment.
        // fixer should hear: task.created + the comment (by planner) = 2; NOT its own changes.
        let planner = check_notifications(&pool, "planner", true, 50, None).await?;
        let fixer = check_notifications(&pool, "fixer", true, 50, None).await?;
        assert_eq!(planner["count"].as_i64(), Some(3), "planner: {planner}");
        assert_eq!(fixer["count"].as_i64(), Some(2), "fixer: {fixer}");

        let types: BTreeSet<String> = planner["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap().to_string())
            .collect();
        let expected: BTreeSet<String> = ["task.status_changed", "message.direct"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(types, expected);

        // Draining a second time yields nothing (marked read).
        let again = check_notifications(&pool, "planner", true, 50, None).await?;
        assert_eq!(again["count"].as_i64(), Some(0), "should be drained");

        Ok(())
    }

    /// get_task_limited bounds the inlined comments (#511): None = the whole thread; Some(n) = the
    /// most-recent n (chronological within the slice); Some(0) = metadata-only. `comment_count` is
    /// always the true total and `comments_truncated` flags when fewer than all were inlined. The
    /// bare get_task wrapper stays unbounded (the REST/UI full-thread path).
    #[tokio::test]
    async fn get_task_comment_limit_bounds_inlined_slice() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("a"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
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
        for i in 0..5 {
            comment_task(&pool, tid, &format!("c{i}"), Some("a"), None, None).await?;
        }

        // None => all five, chronological, not truncated.
        let all = get_task_limited(&pool, tid, None).await?;
        let cs = all["comments"].as_array().unwrap();
        assert_eq!(cs.len(), 5);
        assert_eq!(cs[0]["body"], json!("c0"));
        assert_eq!(cs[4]["body"], json!("c4"));
        assert_eq!(all["comment_count"], json!(5));
        assert_eq!(all["comments_truncated"], json!(false));

        // Some(2) => the two MOST-RECENT, still chronological (c3, c4), truncated.
        let recent = get_task_limited(&pool, tid, Some(2)).await?;
        let cs = recent["comments"].as_array().unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0]["body"], json!("c3"));
        assert_eq!(cs[1]["body"], json!("c4"));
        assert_eq!(recent["comment_count"], json!(5));
        assert_eq!(recent["comments_truncated"], json!(true));

        // Some(0) => metadata-only, still reporting the true count + truncation.
        let meta_only = get_task_limited(&pool, tid, Some(0)).await?;
        assert_eq!(meta_only["comments"].as_array().unwrap().len(), 0);
        assert_eq!(meta_only["comment_count"], json!(5));
        assert_eq!(meta_only["comments_truncated"], json!(true));

        // The bare wrapper is unbounded (the REST/UI full-thread path).
        let wrapped = get_task(&pool, tid).await?;
        assert_eq!(wrapped["comments"].as_array().unwrap().len(), 5);
        assert_eq!(wrapped["comments_truncated"], json!(false));
        Ok(())
    }

    /// Slice 1 (task_628): the additive comment-type columns default a plain comment to type=plain
    /// with an empty parsed payload, get_comment reads one comment with a canonical comment_<id>
    /// ref, and a comment carrying a question-shaped type/payload/state surfaces parsed in both
    /// get_comment and the task's comment list.
    #[tokio::test]
    async fn comment_type_columns_and_get_comment() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("a"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
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

        let c = comment_task(&pool, tid, "plain one", Some("a"), None, None).await?;
        let cid = c["comment_id"].as_i64().unwrap();

        // A plain comment: type defaults to plain, payload parses to an empty object, ref present.
        let got = get_comment(&pool, cid).await?;
        assert_eq!(got["type"], json!("plain"));
        assert_eq!(got["payload"], json!({}));
        assert_eq!(got["body"], json!("plain one"));
        assert_eq!(got["ref"], json!(format!("comment_{cid}")));
        assert!(got["state"].is_null());

        // Simulate a question-typed comment (the ops slice will write these): the payload JSON is
        // surfaced parsed, and the lifecycle state passes through.
        sqlx::query(
            "UPDATE comments SET type='question', state='open', \
             payload='{\"kind\":\"yes_no\",\"routed_to\":\"operator\",\"blocking\":true}' WHERE id=?",
        )
        .bind(cid)
        .execute(&pool)
        .await?;
        let q = get_comment(&pool, cid).await?;
        assert_eq!(q["type"], json!("question"));
        assert_eq!(q["state"], json!("open"));
        assert_eq!(q["payload"]["kind"], json!("yes_no"));
        assert_eq!(q["payload"]["blocking"], json!(true));

        // The task's comment list surfaces the same typed/parsed fields.
        let task = get_task(&pool, tid).await?;
        let listed = &task["comments"].as_array().unwrap()[0];
        assert_eq!(listed["type"], json!("question"));
        assert_eq!(listed["payload"]["routed_to"], json!("operator"));

        // Unknown comment id errors.
        assert!(get_comment(&pool, 99999).await.is_err());
        Ok(())
    }

    /// Slice 2 (task_628): the question lifecycle ops -- pose/answer/decline/cancel with validation,
    /// the open-only guard, the out-of-frame text escape, and the blocking-question terminal path.
    #[tokio::test]
    async fn question_ops_lifecycle() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "rev", None, None, None, None, None).await?;
        let p = create_project(&pool, "P", None, Some("a"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            pid,
            "T",
            None,
            None,
            None,
            Some("asker"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();

        // Routed-to must be a known principal.
        assert!(pose_question(
            &pool,
            tid,
            "yes_no",
            "ship it?",
            None,
            "ghost",
            true,
            None,
            None,
            Some("asker")
        )
        .await
        .is_err());
        // multiple_choice requires options.
        assert!(pose_question(
            &pool,
            tid,
            "multiple_choice",
            "which?",
            None,
            "rev",
            true,
            None,
            None,
            Some("asker")
        )
        .await
        .is_err());
        // A blocking question cannot carry a default.
        assert!(pose_question(
            &pool,
            tid,
            "yes_no",
            "x?",
            None,
            "rev",
            true,
            Some(json!(true)),
            None,
            Some("asker")
        )
        .await
        .is_err());

        // Pose a blocking yes_no question routed to an agent.
        let q = pose_question(
            &pool,
            tid,
            "yes_no",
            "ship it?",
            None,
            "rev",
            true,
            None,
            None,
            Some("asker"),
        )
        .await?;
        let qid = q["id"].as_i64().unwrap();
        assert_eq!(q["type"], json!("question"));
        assert_eq!(q["state"], json!("open"));
        assert_eq!(q["payload"]["kind"], json!("yes_no"));
        assert_eq!(q["payload"]["blocking"], json!(true));

        // Wrong-shape framed answer is rejected; a text answer is accepted as the out-of-frame escape.
        assert!(
            answer_question(&pool, qid, "choice", json!(["x"]), Some("rev"))
                .await
                .is_err()
        );
        let a = answer_question(
            &pool,
            qid,
            "text",
            json!("neither -- hold for Q3"),
            Some("rev"),
        )
        .await?;
        assert_eq!(a["type"], json!("answer"));
        assert_eq!(a["reply_to"], json!(qid));
        assert_eq!(a["payload"]["shape"], json!("text"));
        // The question moved to answered-outside-frame (the escape), not plain answered.
        assert_eq!(
            get_comment(&pool, qid).await?["state"],
            json!("answered_outside_frame")
        );
        // Open-only guard: answering again is rejected.
        assert!(
            answer_question(&pool, qid, "bool", json!(true), Some("rev"))
                .await
                .is_err()
        );

        // multiple_choice: options validated; a framed single-choice answer resolves it.
        let opts = json!([{"id":"a","label":"A"},{"id":"b","label":"B"}]);
        let q2 = pose_question(
            &pool,
            tid,
            "multiple_choice",
            "a or b?",
            Some(opts),
            "rev",
            true,
            None,
            None,
            Some("asker"),
        )
        .await?;
        let q2id = q2["id"].as_i64().unwrap();
        assert!(
            answer_question(&pool, q2id, "choice", json!(["zzz"]), Some("rev"))
                .await
                .is_err(),
            "unknown option id rejected"
        );
        assert!(
            answer_question(&pool, q2id, "choice", json!(["a", "b"]), Some("rev"))
                .await
                .is_err(),
            "multiple_choice takes exactly one"
        );
        let a2 = answer_question(&pool, q2id, "choice", json!(["a"]), Some("rev")).await?;
        assert_eq!(a2["payload"]["value"], json!(["a"]));
        assert_eq!(get_comment(&pool, q2id).await?["state"], json!("answered"));

        // decline: an explicit refusal with feedback moves it to declined.
        let q3 = pose_question(
            &pool,
            tid,
            "yes_no",
            "do X?",
            None,
            "rev",
            true,
            None,
            None,
            Some("asker"),
        )
        .await?;
        let q3id = q3["id"].as_i64().unwrap();
        let d = decline_question(&pool, q3id, "not my call", Some("rev")).await?;
        assert_eq!(d["payload"]["declined"], json!(true));
        assert_eq!(get_comment(&pool, q3id).await?["state"], json!("declined"));

        // cancel: only the asking author may withdraw it.
        let q4 = pose_question(
            &pool,
            tid,
            "yes_no",
            "still need?",
            None,
            "rev",
            true,
            None,
            None,
            Some("asker"),
        )
        .await?;
        let q4id = q4["id"].as_i64().unwrap();
        assert!(
            cancel_question(&pool, q4id, Some("rev")).await.is_err(),
            "non-author cannot cancel"
        );
        cancel_question(&pool, q4id, Some("asker")).await?;
        assert_eq!(get_comment(&pool, q4id).await?["state"], json!("cancelled"));

        // Non-blocking with a default + wait period is accepted.
        let q5 = pose_question(
            &pool,
            tid,
            "yes_no",
            "proceed?",
            None,
            "rev",
            false,
            Some(json!(true)),
            Some(3600),
            Some("asker"),
        )
        .await?;
        assert_eq!(q5["payload"]["blocking"], json!(false));
        assert_eq!(q5["payload"]["default"], json!(true));
        assert_eq!(q5["payload"]["wait_period_seconds"], json!(3600));
        Ok(())
    }

    /// task_628 slice 3: get_task's DERIVED question-block (effectively_blocked / question_blocked /
    /// blocking_questions sorted by comment id / question_blocked_on raw ids) and the principal-
    /// general "waiting on me" view (list_tasks_blocking_me), including team-routed questions
    /// surfacing for members and answered questions dropping out of the block.
    #[tokio::test]
    async fn derived_question_block_and_waiting_on_me() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "asker", None, None, None, None, None).await?;
        register_agent(&pool, "bob", None, None, None, None, None).await?;
        register_agent(&pool, "carol", None, None, None, None, None).await?;
        let p = create_project(&pool, "P", None, Some("asker"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            pid,
            "T",
            None,
            Some("asker"),
            None,
            Some("asker"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();

        // Writes a plain comment then promotes it to a question (the ops slice writes these directly;
        // the read-side under test only cares about the stored columns).
        async fn pose(pool: &Pool, tid: i64, body: &str, payload: &str) -> anyhow::Result<i64> {
            let cid = comment_task(pool, tid, body, Some("asker"), None, None).await?["comment_id"]
                .as_i64()
                .unwrap();
            sqlx::query("UPDATE comments SET type='question', state='open', payload=? WHERE id=?")
                .bind(payload)
                .bind(cid)
                .execute(pool)
                .await?;
            Ok(cid)
        }
        fn ids(v: &Value) -> Vec<i64> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|t| t["id"].as_i64().unwrap())
                .collect()
        }

        // No questions yet -> not effectively blocked, empty derived fields.
        let before = get_task(&pool, tid).await?;
        assert_eq!(before["effectively_blocked"], json!(false));
        assert_eq!(before["question_blocked"], json!(false));
        assert_eq!(before["blocking_questions"].as_array().unwrap().len(), 0);
        assert_eq!(before["question_blocked_on"], json!([]));

        // A NON-blocking question does not block, nor does an ANSWERED one. A blocking open question
        // routed to agent "bob" does.
        pose(
            &pool,
            tid,
            "fyi?",
            r#"{"kind":"yes_no","routed_to":"bob","blocking":false}"#,
        )
        .await?;
        let c_bob = pose(
            &pool,
            tid,
            "ship it?",
            r#"{"kind":"yes_no","routed_to":"bob","blocking":true}"#,
        )
        .await?;

        let got = get_task(&pool, tid).await?;
        assert_eq!(got["effectively_blocked"], json!(true));
        assert_eq!(got["question_blocked"], json!(true));
        let bq = got["blocking_questions"].as_array().unwrap();
        assert_eq!(bq.len(), 1, "only the open blocking question counts");
        assert_eq!(bq[0]["comment_id"], json!(c_bob));
        assert_eq!(bq[0]["kind"], json!("yes_no"));
        assert_eq!(bq[0]["routed_to"], json!("bob"));
        assert_eq!(bq[0]["blocking"], json!(true));
        assert_eq!(bq[0]["prompt"], json!("ship it?"));
        assert_eq!(got["question_blocked_on"], json!(["bob"]));

        // "waiting on me": bob sees it; carol (uninvolved) does not.
        assert_eq!(
            ids(&list_tasks_blocking_me(&pool, "bob", None, false).await?),
            vec![tid]
        );
        assert!(list_tasks_blocking_me(&pool, "carol", None, false)
            .await?
            .as_array()
            .unwrap()
            .is_empty());

        // A second blocking question routed to a TEAM carol belongs to: union sorted by raw id,
        // ordering by comment id holds, and carol now surfaces (team expansion).
        create_team(&pool, "qa", Some("QA"), Some("asker"), None).await?;
        add_team_member(&pool, "qa", "carol", "agent", Some("asker")).await?;
        let c_team = pose(
            &pool,
            tid,
            "qa sign-off?",
            r#"{"kind":"yes_no","routed_to":"qa","blocking":true}"#,
        )
        .await?;

        let got2 = get_task(&pool, tid).await?;
        let bq2 = got2["blocking_questions"].as_array().unwrap();
        assert_eq!(bq2.len(), 2);
        assert_eq!(bq2[0]["comment_id"], json!(c_bob), "sorted by comment id");
        assert_eq!(bq2[1]["comment_id"], json!(c_team));
        assert_eq!(got2["question_blocked_on"], json!(["bob", "qa"]));
        assert_eq!(
            ids(&list_tasks_blocking_me(&pool, "carol", None, false).await?),
            vec![tid]
        );
        assert_eq!(
            ids(&list_tasks_blocking_me(&pool, "bob", None, false).await?),
            vec![tid]
        );
        // An unrelated principal sees nothing.
        assert!(list_tasks_blocking_me(&pool, "dave", None, false)
            .await?
            .as_array()
            .unwrap()
            .is_empty());

        // Answering bob's question drops it from the derived block + bob's view; the task stays
        // effectively blocked on the still-open qa question.
        sqlx::query("UPDATE comments SET state='answered' WHERE id=?")
            .bind(c_bob)
            .execute(&pool)
            .await?;
        let got3 = get_task(&pool, tid).await?;
        assert_eq!(got3["blocking_questions"].as_array().unwrap().len(), 1);
        assert_eq!(got3["question_blocked_on"], json!(["qa"]));
        assert_eq!(got3["effectively_blocked"], json!(true));
        assert!(list_tasks_blocking_me(&pool, "bob", None, false)
            .await?
            .as_array()
            .unwrap()
            .is_empty());
        Ok(())
    }

    /// task_628 slice 4: supersede_question re-poses a fresh open question carrying a verbatim copy
    /// of the old payload, keeps the old one immutable + linked (state=superseded, superseded_by),
    /// is author-only, keeps a blocking task blocked across the swap (no spurious unblock), notifies
    /// the routed-to principal, and refuses to supersede a non-open question.
    #[tokio::test]
    async fn supersede_question_re_poses_and_links() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "rev", None, None, None, None, None).await?;
        let p = create_project(&pool, "P", None, Some("asker"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            pid,
            "T",
            None,
            None,
            None,
            Some("asker"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();

        // Pose a blocking yes_no question routed to agent "rev".
        let q = pose_question(
            &pool,
            tid,
            "yes_no",
            "ship v1?",
            None,
            "rev",
            true,
            None,
            None,
            Some("asker"),
        )
        .await?;
        let qid = q["id"].as_i64().unwrap();

        // Only the asking author may supersede.
        assert!(supersede_question(&pool, qid, "ship v2?", Some("rev"))
            .await
            .is_err());
        // A non-empty prompt is required.
        assert!(supersede_question(&pool, qid, "   ", Some("asker"))
            .await
            .is_err());

        // Author supersedes with a corrected prompt.
        let new = supersede_question(&pool, qid, "ship v2 (clarified)?", Some("asker")).await?;
        let new_cid = new["id"].as_i64().unwrap();
        assert_eq!(new["type"], json!("question"));
        assert_eq!(new["state"], json!("open"));
        assert_eq!(new["body"], json!("ship v2 (clarified)?"));
        assert_eq!(new["supersedes"], json!(qid));
        // Payload copied verbatim (routing + blocking + kind preserved).
        assert_eq!(new["payload"]["kind"], json!("yes_no"));
        assert_eq!(new["payload"]["blocking"], json!(true));
        assert_eq!(new["payload"]["routed_to"], json!("rev"));

        // The old question is immutable + superseded, pointing forward at its replacement.
        let old = get_comment(&pool, qid).await?;
        assert_eq!(old["state"], json!("superseded"));
        assert_eq!(old["superseded_by"], json!(new_cid));
        assert_eq!(old["body"], json!("ship v1?"), "old prompt untouched");
        assert_eq!(
            old["payload"]["blocking"],
            json!(true),
            "old payload untouched"
        );

        // The task stays blocked across the swap: the only open blocking question is now the new one
        // (so the terminal recompute saw a surviving block and emitted no task.unblocked).
        let mut tx = pool.begin().await?;
        let obq = open_blocking_questions(&mut tx, tid).await?;
        tx.rollback().await?;
        assert_eq!(obq, vec![(new_cid, "rev".to_string())]);

        // The routed-to principal is notified of the supersede, with the replacement id.
        let notes = check_notifications(&pool, "rev", true, 50, None).await?;
        assert!(
            notes["notifications"].as_array().unwrap().iter().any(|n| {
                n["type"] == json!("question.superseded")
                    && n["data"]["new_comment_id"] == json!(new_cid)
                    && n["data"]["question_comment_id"] == json!(qid)
            }),
            "routed-to agent is notified of the supersede: {notes}"
        );

        // A superseded (non-open) question cannot be superseded again.
        assert!(supersede_question(&pool, qid, "ship v3?", Some("asker"))
            .await
            .is_err());

        // Non-blocking supersede carries the default + wait period forward and needs no recompute.
        let nb = pose_question(
            &pool,
            tid,
            "yes_no",
            "nice to have?",
            None,
            "rev",
            false,
            Some(json!(true)),
            Some(3600),
            Some("asker"),
        )
        .await?;
        let nb_new = supersede_question(
            &pool,
            nb["id"].as_i64().unwrap(),
            "still nice to have?",
            Some("asker"),
        )
        .await?;
        assert_eq!(nb_new["state"], json!("open"));
        assert_eq!(nb_new["payload"]["blocking"], json!(false));
        assert_eq!(nb_new["payload"]["default"], json!(true));
        assert_eq!(nb_new["payload"]["wait_period_seconds"], json!(3600));
        Ok(())
    }

    /// detect_bare_task_refs finds bare "#N" refs using the linkifier boundary rule, de-duped in
    /// first-seen order, and rejects the non-refs: a word char before "#" (incl. a repo-qualified
    /// owner/repo#N), "##", "&#123;", "#12ab", and a bare "#" with no digits (task #517).
    #[test]
    fn detect_bare_task_refs_matches_only_ambiguous_bare_refs() {
        assert_eq!(detect_bare_task_refs("see #183 please"), vec![183]);
        assert_eq!(
            detect_bare_task_refs("#12 and #34 and #12 again"),
            vec![12, 34]
        );
        assert_eq!(detect_bare_task_refs("(#7)"), vec![7]);
        assert!(detect_bare_task_refs("abc#1").is_empty());
        assert!(detect_bare_task_refs("camshaft/fleet#183").is_empty());
        assert!(detect_bare_task_refs("##5").is_empty());
        assert!(detect_bare_task_refs("&#123;").is_empty());
        assert!(detect_bare_task_refs("#12ab").is_empty());
        assert!(detect_bare_task_refs("a # b").is_empty());
        assert!(detect_bare_task_refs("task_5 is fine").is_empty());
    }

    /// check_bare_refs hard-fails on a bare "#N" OUTSIDE code, but allows it inside inline code /
    /// fenced blocks and allows the unambiguous forms (typed task_N, repo-qualified owner/repo#N).
    #[test]
    fn check_bare_refs_hard_fails_outside_code_only() {
        assert!(check_bare_refs("see #9 please").is_err());
        assert!(check_bare_refs("blocked on #12 and #34").is_err());
        // Inside code -> allowed (not a reference).
        assert!(check_bare_refs("the `#9` token").is_ok());
        assert!(check_bare_refs("```\n#9 in a fence\n```").is_ok());
        // Unambiguous forms -> allowed.
        assert!(check_bare_refs("task_9 and camshaft/task-board#9").is_ok());
        assert!(check_bare_refs("no refs here at all").is_ok());
        // A real bare ref outside code still fails even if another is fenced in code.
        assert!(check_bare_refs("real #9 and `#12` in code").is_err());
    }

    /// A write whose submitted text has a bare "#N" is HARD-REJECTED with an actionable error
    /// (operator decision: block over silent normalize); a clean write (typed form + repo-qualified
    /// external) succeeds, and a bare "#N" INSIDE code (inline or fenced) is allowed (task #517).
    #[tokio::test]
    async fn writes_hard_fail_on_ambiguous_bare_ref() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("a"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
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

        // Comment with a bare ref -> rejected with an actionable, typed-form-naming error.
        let err = comment_task(&pool, tid, "duplicate of #7", Some("a"), None, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("ambiguous bare reference"), "got: {msg}");
        assert!(msg.contains("task_7"), "error names the typed form: {msg}");
        assert!(
            msg.contains("camshaft/task-board#7"),
            "error names the GitHub form: {msg}"
        );
        // task 616: an agent writing a bare "#N" for a plain ordinal (e.g. a board message number)
        // gets told to drop the #, since neither typed form fits that case.
        assert!(
            msg.contains("drop the #") && msg.contains("ordinal"),
            "error covers the plain-ordinal case: {msg}"
        );
        // Nothing was stored (rejected pre-write).
        assert_eq!(get_task(&pool, tid).await?["comment_count"], json!(0));

        // A clean comment (typed form + repo-qualified external) succeeds.
        comment_task(
            &pool,
            tid,
            "use task_7 and camshaft/task-board#7",
            Some("a"),
            None,
            None,
        )
        .await?;
        // A bare "#N" inside inline code or a fenced block is NOT a reference -> allowed.
        comment_task(&pool, tid, "the literal `#9` token", Some("a"), None, None).await?;
        comment_task(
            &pool,
            tid,
            "```\nsee #9 in code\n```",
            Some("a"),
            None,
            None,
        )
        .await?;
        assert_eq!(get_task(&pool, tid).await?["comment_count"], json!(3));

        // create_task + publish-style paths reject a bare ref in title/description too.
        assert!(create_task(
            &pool,
            pid,
            "blocks #9",
            None,
            None,
            None,
            Some("a"),
            None,
            None,
            None
        )
        .await
        .is_err());
        assert!(create_task(
            &pool,
            pid,
            "title",
            Some("see #9"),
            None,
            None,
            Some("a"),
            None,
            None,
            None
        )
        .await
        .is_err());
        // A clean create succeeds.
        create_task(
            &pool,
            pid,
            "clean title",
            Some("see task_9"),
            None,
            None,
            Some("a"),
            None,
            None,
            None,
        )
        .await?;
        Ok(())
    }

    /// metadata.monitor_exempt surfaces as a derived top-level bool on both get_task and list_tasks
    /// (default false), so the nudge daemon + #506 watchdog can read it from a list scan without
    /// pulling full metadata; metadata stays the source of truth (board-pm; v-fleet-tooling).
    #[tokio::test]
    async fn monitor_exempt_surfaces_as_derived_bool() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("a"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        create_task(
            &pool,
            pid,
            "plain",
            None,
            None,
            None,
            Some("a"),
            None,
            None,
            None,
        )
        .await?;
        let exempt = create_task(
            &pool,
            pid,
            "exempt",
            None,
            None,
            None,
            Some("a"),
            None,
            None,
            None,
        )
        .await?;
        let eid = exempt["id"].as_i64().unwrap();

        // Default: not exempt.
        assert_eq!(get_task(&pool, eid).await?["monitor_exempt"], json!(false));

        // Set metadata.monitor_exempt via the update_task metadata merge.
        update_task(
            &pool,
            eid,
            None,
            None,
            None,
            None,
            None,
            Some("a"),
            Some(json!({ "monitor_exempt": true })),
            None,
            None,
        )
        .await?;

        let got = get_task(&pool, eid).await?;
        assert_eq!(
            got["monitor_exempt"],
            json!(true),
            "get_task reflects the derived flag"
        );
        assert_eq!(
            got["metadata"]["monitor_exempt"],
            json!(true),
            "metadata stays the source of truth"
        );

        // list_tasks surfaces the derived bool per row.
        let list = list_tasks(
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
        let arr = list.as_array().unwrap();
        let by_title = |t: &str| arr.iter().find(|x| x["title"] == json!(t)).unwrap().clone();
        assert_eq!(by_title("exempt")["monitor_exempt"], json!(true));
        assert_eq!(by_title("plain")["monitor_exempt"], json!(false));
        Ok(())
    }

    /// Identity aliases (task 532): the operator -> cameron seed is present; set_identity_alias
    /// upserts (repoints an existing alias, lowercasing the key); empty + self-alias are rejected.
    /// The alias map is what consumers/UI use to resolve a floating name to its canonical identity.
    #[tokio::test]
    async fn identity_aliases_seed_and_upsert() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        let canonical_of = |list: &Value, alias: &str| -> Option<String> {
            list.as_array()?
                .iter()
                .find(|a| a["alias"] == json!(alias))
                .and_then(|a| a["canonical"].as_str().map(str::to_string))
        };

        // Seeded operator -> cameron.
        let list = list_identity_aliases(&pool).await?;
        assert_eq!(canonical_of(&list, "operator").as_deref(), Some("cameron"));

        // Upsert a new alias (key lowercased), then repoint it.
        set_identity_alias(&pool, "Boss", "cameron", Some("tester")).await?;
        let list = list_identity_aliases(&pool).await?;
        assert_eq!(canonical_of(&list, "boss").as_deref(), Some("cameron"));
        set_identity_alias(&pool, "boss", "dana", None).await?;
        let list = list_identity_aliases(&pool).await?;
        assert_eq!(canonical_of(&list, "boss").as_deref(), Some("dana"));

        // Rejects empty + self-alias.
        assert!(set_identity_alias(&pool, "", "x", None).await.is_err());
        assert!(set_identity_alias(&pool, "x", "x", None).await.is_err());
        Ok(())
    }

    /// A task can be blocked on kind=external (an infra dependency with no board owner, task_112):
    /// no target required, a free-text note, and it stays OFF the operator queue
    /// (blocked_on_kind=operator) while being findable via blocked_on_kind=external. An unknown kind
    /// is still rejected.
    #[tokio::test]
    async fn blocked_on_external_is_off_the_operator_queue() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("a"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            pid,
            "blocked on upstream merge",
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

        // Block on external with a free-text note, no target.
        update_task(&pool, tid, Some("blocked"), None, None, None, None, Some("a"), None, None,
            Some(json!({ "kind": "external", "note": "daily upstream dependency sync / stale package index" }))).await?;
        let got = get_task(&pool, tid).await?;
        assert_eq!(got["status"], json!("blocked"));
        assert_eq!(got["blocked_on"]["kind"], json!("external"));
        assert_eq!(
            got["blocked_on"]["target"],
            json!(null),
            "external has no target"
        );
        assert!(got["blocked_on"]["note"]
            .as_str()
            .unwrap()
            .contains("upstream"));

        // OFF the operator queue; ON the external filter.
        let op = list_tasks(
            &pool,
            None,
            None,
            None,
            false,
            None,
            false,
            None,
            Some("operator"),
            None,
            None,
            None,
            false,
        )
        .await?;
        assert!(
            op.as_array().unwrap().iter().all(|x| x["id"] != json!(tid)),
            "external task must not be on the operator queue"
        );
        let ext = list_tasks(
            &pool,
            None,
            None,
            None,
            false,
            None,
            false,
            None,
            Some("external"),
            None,
            None,
            None,
            false,
        )
        .await?;
        assert!(
            ext.as_array()
                .unwrap()
                .iter()
                .any(|x| x["id"] == json!(tid)),
            "external task found via the external filter"
        );

        // An unknown kind is still rejected.
        assert!(update_task(
            &pool,
            tid,
            Some("blocked"),
            None,
            None,
            None,
            None,
            Some("a"),
            None,
            None,
            Some(json!({ "kind": "bogus" }))
        )
        .await
        .is_err());
        Ok(())
    }

    /// Auto-unblock (task 614): completing a blocker task fans a task.unblocked notification out to
    /// the subscribers of every task that was blocked_on it, so dependents learn the blocker cleared
    /// without a manual sweep.
    #[tokio::test]
    async fn completing_a_blocker_notifies_dependents() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        // Blocker B (owned by b-owner) and two dependents D1/D2 (owned by d1/d2) blocked on it.
        let b = create_task(
            &pool,
            pid,
            "blocker",
            None,
            None,
            None,
            Some("b-owner"),
            None,
            None,
            None,
        )
        .await?;
        let b_id = b["id"].as_i64().unwrap();
        let d1 = create_task(
            &pool,
            pid,
            "dep1",
            None,
            None,
            None,
            Some("d1"),
            None,
            None,
            None,
        )
        .await?;
        let d1_id = d1["id"].as_i64().unwrap();
        let d2 = create_task(
            &pool,
            pid,
            "dep2",
            None,
            None,
            None,
            Some("d2"),
            None,
            None,
            None,
        )
        .await?;
        let d2_id = d2["id"].as_i64().unwrap();
        // `other` is blocked on D1 (a DIFFERENT task), so completing B must NOT notify it.
        let other = create_task(
            &pool,
            pid,
            "other",
            None,
            None,
            None,
            Some("o"),
            None,
            None,
            None,
        )
        .await?;
        let other_id = other["id"].as_i64().unwrap();
        for (dep, who, target) in [
            (d1_id, "d1", b_id),
            (d2_id, "d2", b_id),
            (other_id, "o", d1_id),
        ] {
            update_task(
                &pool,
                dep,
                Some("blocked"),
                None,
                None,
                None,
                None,
                Some(who),
                None,
                None,
                Some(json!({"kind": "task", "target": target.to_string(), "note": "waiting"})),
            )
            .await?;
        }
        // Drain pre-existing notifications so the only task.unblocked we see comes from completing B.
        for who in ["d1", "d2", "o"] {
            check_notifications(&pool, who, true, 100, None).await?;
        }

        // Complete the blocker (a real status change).
        update_task(
            &pool,
            b_id,
            Some("done"),
            None,
            None,
            None,
            None,
            Some("b-owner"),
            None,
            None,
            None,
        )
        .await?;

        // Each dependent-on-B owner is notified task.unblocked for THEIR task, naming the blocker.
        for (dep, who) in [(d1_id, "d1"), (d2_id, "d2")] {
            let notes = check_notifications(&pool, who, true, 100, None).await?;
            let hit = notes["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["type"] == json!("task.unblocked") && n["task_id"] == json!(dep));
            assert!(
                hit.is_some(),
                "{who} notified task.unblocked for dep {dep}: {notes}"
            );
            assert_eq!(
                hit.unwrap()["data"]["blocker_task_id"],
                json!(b_id),
                "names the blocker"
            );
        }

        // `other` (blocked on D1, not B) is NOT spuriously unblocked by B completing.
        let o_notes = check_notifications(&pool, "o", true, 100, None).await?;
        assert!(
            !o_notes["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["type"] == json!("task.unblocked")),
            "a task blocked on a different blocker is not spuriously unblocked: {o_notes}"
        );
        Ok(())
    }

    /// A board-scope subscription is a firehose: it receives every event — including events in
    /// projects the subscriber never joined and otherwise-silent ones — minus its own actions,
    /// and stops when unsubscribed.
    #[tokio::test]
    async fn board_subscription_is_a_firehose() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        subscribe(&pool, "watcher", None, None, None, None, true).await?;

        // Activity by another actor, in projects the watcher never joined.
        let a = create_project(&pool, "A", None, Some("alice"), None).await?;
        let aid = a["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            aid,
            "T",
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        comment_task(&pool, tid, "hi", Some("alice"), None, None).await?;
        create_project(&pool, "B", None, Some("alice"), None).await?;

        // The watcher hears all four: project.created (A, silent to others), task.created,
        // task.commented, project.created (B) — despite subscribing to nothing specific.
        let watcher = check_notifications(&pool, "watcher", true, 50, None).await?;
        assert_eq!(watcher["count"].as_i64(), Some(4), "firehose: {watcher}");
        let types: BTreeSet<String> = watcher["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap().to_string())
            .collect();
        for want in ["project.created", "task.created", "task.commented"] {
            assert!(
                types.contains(want),
                "firehose should carry {want}: {types:?}"
            );
        }

        // The watcher's OWN action does not notify itself (actor excluded).
        create_project(&pool, "C", None, Some("watcher"), None).await?;
        let own = check_notifications(&pool, "watcher", true, 50, None).await?;
        assert_eq!(
            own["count"].as_i64(),
            Some(0),
            "actor excluded from its own events"
        );

        // Unsubscribing stops the firehose.
        unsubscribe(&pool, "watcher", None, None, None, None, true).await?;
        create_project(&pool, "D", None, Some("alice"), None).await?;
        let after = check_notifications(&pool, "watcher", true, 50, None).await?;
        assert_eq!(
            after["count"].as_i64(),
            Some(0),
            "no events after unsubscribe"
        );

        Ok(())
    }

    /// Documents: create makes version 1 and points current at it; publish appends immutable
    /// versions and advances the pointer; get/list/versions read back; a new version supersedes
    /// a prior approval.
    #[tokio::test]
    async fn documents_versioning_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "Docs", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();

        let d = create_document(
            &pool,
            "Design: Widgets",
            Some(pid),
            "bafyv1",
            Some("first draft"),
            Some("alice"),
            Some(json!({ "tags": ["design"] })),
            None,
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();
        assert_eq!(d["title"], json!("Design: Widgets"));
        assert_eq!(d["slug"], json!("design-widgets"));
        assert_eq!(d["status"], json!("draft"));
        assert_eq!(d["metadata"]["tags"][0], json!("design"));
        assert_eq!(d["current_version"]["version_no"], json!(1));
        assert_eq!(d["current_version"]["cid"], json!("bafyv1"));
        assert_eq!(d["versions"].as_array().unwrap().len(), 1);

        // Publish version 2 -> current advances, immutable history grows.
        let d2 = publish_version(
            &pool,
            did,
            "bafyv2",
            Some("revise"),
            Some("alice"),
            None,
            None,
        )
        .await?;
        assert_eq!(d2["current_version"]["version_no"], json!(2));
        assert_eq!(d2["current_version"]["cid"], json!("bafyv2"));
        assert_eq!(d2["versions"].as_array().unwrap().len(), 2);

        // get_document_versions -> newest first.
        let vers = get_document_versions(&pool, did).await?;
        let vers = vers.as_array().unwrap();
        assert_eq!(vers.len(), 2);
        assert_eq!(vers[0]["version_no"], json!(2));
        assert_eq!(vers[1]["version_no"], json!(1));

        // list_documents by project + status.
        assert_eq!(
            list_documents(&pool, Some(pid), None, None, None, None, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_documents(&pool, Some(pid), Some("draft"), None, None, None, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_documents(&pool, Some(pid), Some("approved"), None, None, None, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            list_documents(&pool, Some(99999), None, None, None, None, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            0
        );

        // A new version resets an approved doc back to in_review.
        sqlx::query("UPDATE documents SET status='approved' WHERE id=?")
            .bind(did)
            .execute(&pool)
            .await?;
        let d3 = publish_version(&pool, did, "bafyv3", None, Some("alice"), None, None).await?;
        assert_eq!(
            d3["status"],
            json!("in_review"),
            "new version supersedes approval"
        );

        // Missing document -> error.
        assert!(get_document(&pool, 424242).await.is_err());
        Ok(())
    }

    /// The wiki layer: a document can be filed at a slash-separated path (unique among filed
    /// docs), renamed, and cleared; list_wiki returns filed docs ordered by path and honors a
    /// prefix filter (the prefix node plus everything beneath it).
    #[tokio::test]
    async fn wiki_path_filing_and_listing() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let mk = |title: &'static str| {
            let pool = pool.clone();
            async move {
                create_document(
                    &pool,
                    title,
                    None,
                    "bafy",
                    None,
                    Some("alice"),
                    None,
                    None,
                    None,
                )
                .await
            }
        };
        let a = mk("A").await?["id"].as_i64().unwrap();
        let b = mk("B").await?["id"].as_i64().unwrap();
        let c = mk("C").await?["id"].as_i64().unwrap();

        // File a doc; leading/trailing slashes are trimmed, and get_document reflects the path.
        let filed =
            set_document_path(&pool, a, "/architecture/board/events/", Some("alice")).await?;
        assert_eq!(filed["path"], json!("architecture/board/events"));
        assert_eq!(
            get_document(&pool, a).await?["path"],
            json!("architecture/board/events")
        );

        // Collision: filing another doc at the same path is rejected (400-mapped "give " error).
        let err = set_document_path(&pool, b, "architecture/board/events", None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().starts_with("give "),
            "collision error, got: {err}"
        );

        // File the rest of the tree, plus one doc outside it.
        set_document_path(&pool, b, "architecture/board/schema", None).await?;
        set_document_path(&pool, c, "runbooks/deploy", None).await?;

        // Whole wiki: all three, ordered by path (architecture/* before runbooks/*).
        let all = list_wiki(&pool, None, false).await?;
        let all = all.as_array().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0]["path"], json!("architecture/board/events"));
        assert_eq!(all[1]["path"], json!("architecture/board/schema"));
        assert_eq!(all[2]["path"], json!("runbooks/deploy"));

        // Prefix filter: only the architecture subtree.
        let arch = list_wiki(&pool, Some("architecture"), false).await?;
        assert_eq!(arch.as_array().unwrap().len(), 2);
        // A prefix must not match a sibling that merely shares a string head.
        assert_eq!(
            list_wiki(&pool, Some("runbooks"), false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // Rename frees the old path (b can now take it) and clearing unfiles a doc.
        set_document_path(&pool, a, "architecture/board/events-v2", None).await?;
        set_document_path(&pool, b, "architecture/board/events", None).await?; // no longer a collision
        set_document_path(&pool, c, "", None).await?; // clear -> unfiled
        assert!(get_document(&pool, c).await?["path"].is_null());
        assert_eq!(
            list_wiki(&pool, None, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            2
        );
        Ok(())
    }

    /// Soft-archive: archiving a doc hides it from list_documents and the wiki tree by default
    /// (but include_archived=true still shows it), the doc still resolves by id with archived_at
    /// set, and restore brings it back. Reversible; nothing is destroyed.
    #[tokio::test]
    async fn document_archive_hides_and_restores() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let mk = |title: &'static str| {
            let pool = pool.clone();
            async move {
                create_document(
                    &pool,
                    title,
                    None,
                    "bafy",
                    None,
                    Some("alice"),
                    None,
                    None,
                    None,
                )
                .await
            }
        };
        let keep = mk("Keep").await?["id"].as_i64().unwrap();
        let probe = mk("Probe").await?["id"].as_i64().unwrap();
        set_document_path(&pool, keep, "docs/keep", None).await?;
        set_document_path(&pool, probe, "docs/probe", None).await?;

        // Both visible before archiving.
        assert_eq!(
            list_documents(&pool, None, None, None, None, None, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            list_wiki(&pool, None, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // Archive the probe: hidden from list_documents and the wiki tree by default.
        let archived = set_document_archived(&pool, probe, true, Some("concierge")).await?;
        assert!(
            archived["archived_at"].is_string(),
            "archived_at is stamped"
        );
        assert_eq!(
            list_documents(&pool, None, None, None, None, None, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_wiki(&pool, None, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // include_archived=true still surfaces it, and it always resolves by id (nothing destroyed).
        assert_eq!(
            list_documents(&pool, None, None, None, None, None, true)
                .await?
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            list_wiki(&pool, None, true)
                .await?
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(get_document(&pool, probe).await?["archived_at"].is_string());

        // Restore: back in the listings, stamp cleared.
        let restored = set_document_archived(&pool, probe, false, Some("concierge")).await?;
        assert!(
            restored["archived_at"].is_null(),
            "restore clears the stamp"
        );
        assert_eq!(
            list_documents(&pool, None, None, None, None, None, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            list_wiki(&pool, None, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // Archiving a missing document is an error.
        assert!(set_document_archived(&pool, 999_999, true, None)
            .await
            .is_err());
        Ok(())
    }

    /// Muting a task detaches an agent from its fan-out: a stood-down creator stops getting the
    /// task's event notifications (which `unsubscribe` can't stop, since the creator is in the
    /// fan-out independent of a subscription). Unmute restores delivery.
    #[tokio::test]
    async fn mute_task_detaches_from_fan_out() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("owner"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            pid,
            "T",
            None,
            None,
            None,
            Some("owner"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();

        // Baseline: another agent's comment reaches the creator (they're in the fan-out).
        comment_task(&pool, tid, "hello", Some("bob"), None, None).await?;
        let n = check_notifications(&pool, "owner", true, 50, None).await?;
        assert_eq!(
            n["count"],
            json!(1),
            "creator hears the comment before muting"
        );

        // Mute for the owner → subsequent task events no longer reach them.
        mute_task(&pool, "owner", tid).await?;
        comment_task(&pool, tid, "hello again", Some("bob"), None, None).await?;
        let n = check_notifications(&pool, "owner", true, 50, None).await?;
        assert_eq!(
            n["count"],
            json!(0),
            "muted creator gets no fan-out for the task"
        );

        // Unmute → back in the fan-out.
        unmute_task(&pool, "owner", tid).await?;
        comment_task(&pool, tid, "third", Some("bob"), None, None).await?;
        let n = check_notifications(&pool, "owner", true, 50, None).await?;
        assert_eq!(n["count"], json!(1), "unmuted creator hears comments again");

        // Muting a missing task is an error.
        assert!(mute_task(&pool, "owner", 999_999).await.is_err());
        Ok(())
    }

    /// Wiki links: [[target]] / [[path|label]] in a document's content become document_links
    /// edges (embeds ![[..]] excluded), outbound_links resolve to whatever doc is filed at the
    /// target path (dangling = null target), backlinks appear on the target, a new version WITH
    /// content re-indexes the edges, and a CID-only publish leaves them untouched.
    #[tokio::test]
    async fn wiki_links_and_backlinks() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        // Source doc A links to two paths and embeds a third (the embed must NOT become a link).
        let content = "See [[guide/setup]] and [[guide/advanced|Advanced Guide]].\n![[guide/diagram]]\nDup [[guide/setup]] again.";
        let a = create_document(
            &pool,
            "Intro",
            None,
            "bafyA",
            None,
            Some("alice"),
            None,
            None,
            Some(content),
        )
        .await?;
        let aid = a["id"].as_i64().unwrap();
        set_document_path(&pool, aid, "guide/intro", None).await?;

        // outbound_links: guide/setup + guide/advanced, de-duped, embed excluded, ordered by path.
        let a_doc = get_document(&pool, aid).await?;
        let links = a_doc["outbound_links"].as_array().unwrap();
        assert_eq!(
            links.len(),
            2,
            "two distinct links, embed excluded, dup collapsed: {links:?}"
        );
        assert_eq!(links[0]["target_path"], json!("guide/advanced"));
        assert_eq!(links[0]["label"], json!("Advanced Guide"));
        assert!(
            links[0]["target_document_id"].is_null(),
            "advanced dangles (nothing filed there)"
        );
        assert_eq!(links[1]["target_path"], json!("guide/setup"));
        assert!(links[1]["label"].is_null());
        assert!(
            links[1]["target_document_id"].is_null(),
            "setup dangles until a doc is filed there"
        );

        // File a doc at guide/setup: A's link to it now resolves, and that doc sees the backlink.
        let b = create_document(
            &pool,
            "Setup",
            None,
            "bafyB",
            None,
            Some("bob"),
            None,
            None,
            None,
        )
        .await?;
        let bid = b["id"].as_i64().unwrap();
        set_document_path(&pool, bid, "guide/setup", None).await?;

        let a_doc = get_document(&pool, aid).await?;
        let setup = a_doc["outbound_links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["target_path"] == json!("guide/setup"))
            .unwrap()
            .clone();
        assert_eq!(
            setup["target_document_id"],
            json!(bid),
            "link resolves to the doc filed at that path"
        );
        assert_eq!(setup["target_title"], json!("Setup"));

        let b_doc = get_document(&pool, bid).await?;
        let backlinks = b_doc["backlinks"].as_array().unwrap();
        assert_eq!(backlinks.len(), 1, "A backlinks to Setup");
        assert_eq!(backlinks[0]["id"], json!(aid));
        assert_eq!(backlinks[0]["path"], json!("guide/intro"));

        // A new version WITH content re-indexes edges (now only guide/setup).
        publish_version(
            &pool,
            aid,
            "bafyA2",
            Some("trim"),
            Some("alice"),
            None,
            Some("only [[guide/setup]] now"),
        )
        .await?;
        let a_doc = get_document(&pool, aid).await?;
        assert_eq!(
            a_doc["outbound_links"].as_array().unwrap().len(),
            1,
            "edges refreshed from new content"
        );

        // A CID-only publish (no content) leaves the edges as-is (board can't rescan a bare CID).
        publish_version(&pool, aid, "bafyA3", None, Some("alice"), None, None).await?;
        let a_doc = get_document(&pool, aid).await?;
        assert_eq!(
            a_doc["outbound_links"].as_array().unwrap().len(),
            1,
            "CID-only publish keeps prior edges"
        );

        // An unfiled doc (no path) has no backlinks even if others link to some path.
        let c = create_document(
            &pool,
            "Orphan",
            None,
            "bafyC",
            None,
            Some("carol"),
            None,
            None,
            Some("x"),
        )
        .await?;
        assert_eq!(
            get_document(&pool, c["id"].as_i64().unwrap()).await?["backlinks"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        Ok(())
    }

    /// Embeds (transclusion): ![[path]] floats to the target's current version, ![[path@vN]]
    /// pins to an immutable version, ![[path#region]] carries a region fragment; get_document
    /// separates embeds from links and surfaces embedded_by ("what embeds this").
    #[tokio::test]
    async fn wiki_embeds_transclusion() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        // Target doc "lib/widget" with two versions (so a @v1 pin has something to resolve to).
        let t = create_document(
            &pool,
            "Widget",
            None,
            "bafyW1",
            None,
            Some("bob"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        set_document_path(&pool, tid, "lib/widget", None).await?;
        let v2 = publish_version(&pool, tid, "bafyW2", Some("v2"), Some("bob"), None, None).await?;
        let v1_id = v2["versions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["version_no"] == json!(1))
            .unwrap()["id"]
            .as_i64()
            .unwrap();

        // Source doc: one plain link, one floating embed, one pinned embed, one region embed.
        let content = "[[lib/widget]] jump\n![[lib/widget]] float\n![[lib/widget@v1]] pinned\n![[lib/notes#intro]] region";
        let s = create_document(
            &pool,
            "Page",
            None,
            "bafyS",
            None,
            Some("alice"),
            None,
            None,
            Some(content),
        )
        .await?;
        let sid = s["id"].as_i64().unwrap();

        let s_doc = get_document(&pool, sid).await?;
        // Links and embeds are separate tables, so [[lib/widget]] (a link) and ![[lib/widget]]
        // (an embed) to the SAME path are BOTH recorded (task 108 fix — the bug was the embed
        // being dropped). Within embeds, the float ![[lib/widget]] is seen before ![[lib/widget@v1]]
        // so it wins that path (one embed edge per path).
        let links = s_doc["outbound_links"].as_array().unwrap();
        assert!(
            links
                .iter()
                .any(|l| l["target_path"] == json!("lib/widget")),
            "the link is recorded"
        );
        let embeds = s_doc["embeds"].as_array().unwrap();
        let w_emb = embeds
            .iter()
            .find(|e| e["target_path"] == json!("lib/widget"))
            .unwrap();
        assert!(
            w_emb["target_version_id"].is_null(),
            "float embed (![[lib/widget]]) wins over the later @v1; floats"
        );
        assert_eq!(
            w_emb["target_document_id"],
            json!(tid),
            "embed resolves to the filed doc"
        );
        let notes = embeds
            .iter()
            .find(|e| e["target_path"] == json!("lib/notes"))
            .unwrap();
        assert_eq!(notes["region"], json!("intro"), "region fragment captured");
        assert!(
            notes["target_document_id"].is_null(),
            "lib/notes dangles (unfiled)"
        );

        // Now a doc where the embed path is distinct so pinning resolves to a version id.
        let content2 = "![[lib/widget@v1]] pinned\n![[lib/widget-x]] float-dangling";
        let s2 = create_document(
            &pool,
            "Page2",
            None,
            "bafyS2",
            None,
            Some("alice"),
            None,
            None,
            Some(content2),
        )
        .await?;
        let s2_doc = get_document(&pool, s2["id"].as_i64().unwrap()).await?;
        let emb = s2_doc["embeds"].as_array().unwrap();
        let pinned = emb
            .iter()
            .find(|e| e["target_path"] == json!("lib/widget"))
            .unwrap();
        assert_eq!(
            pinned["target_version_id"],
            json!(v1_id),
            "@v1 pins to version 1's id"
        );
        assert_eq!(
            pinned["target_document_id"],
            json!(tid),
            "resolved to the target doc"
        );
        let floating = emb
            .iter()
            .find(|e| e["target_path"] == json!("lib/widget-x"))
            .unwrap();
        assert!(
            floating["target_version_id"].is_null(),
            "unpinned embed floats (null version)"
        );

        // embedded_by: the Widget doc sees who embeds it. Page2 pins it; Page floats it.
        let t_doc = get_document(&pool, tid).await?;
        let emb_by = t_doc["embedded_by"].as_array().unwrap();
        assert!(
            emb_by
                .iter()
                .any(|e| e["id"] == json!(s2["id"].as_i64().unwrap())),
            "Page2 embeds Widget"
        );
        // The task-108 bug case: Page BOTH links and embeds lib/widget, so it must appear in
        // BOTH backlinks AND embedded_by (previously the embed was silently dropped).
        assert!(
            emb_by.iter().any(|e| e["id"] == json!(sid)),
            "Page embeds Widget (same path it also links)"
        );
        assert!(
            t_doc["backlinks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["id"] == json!(sid)),
            "Page links Widget"
        );
        Ok(())
    }

    /// The from-session content read path (task #303): resolve_document_version picks the current
    /// or a named version's (version_no, cid, content_type), read_document_content requires an IPFS
    /// backend (so REST maps to 503 without one), and is_text_content_type classifies text vs binary.
    #[tokio::test]
    async fn read_document_content_resolves_version_and_needs_backend() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = create_document(
            &pool,
            "Spec",
            None,
            "bafycurrent",
            None,
            Some("alice"),
            None,
            Some("text/markdown"),
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();
        publish_version(
            &pool,
            did,
            "bafyv2",
            Some("s"),
            Some("alice"),
            Some("application/pdf"),
            None,
        )
        .await?;

        // The current version resolves to v2 + its cid/content_type; a named older version resolves too.
        let (vn, cid, ct) = resolve_document_version(&pool, did, None).await?;
        assert_eq!(
            (vn, cid.as_str(), ct.as_str()),
            (2, "bafyv2", "application/pdf")
        );
        let (vn1, cid1, ct1) = resolve_document_version(&pool, did, Some(1)).await?;
        assert_eq!(
            (vn1, cid1.as_str(), ct1.as_str()),
            (1, "bafycurrent", "text/markdown")
        );
        // A missing version or document errors.
        assert!(resolve_document_version(&pool, did, Some(99))
            .await
            .is_err());
        assert!(resolve_document_version(&pool, 9999, None).await.is_err());

        // With no IPFS backend, the read path errors with the backend-required message (REST -> 503).
        let err = read_document_content(&pool, None, did, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no IPFS backend"), "got: {err}");

        assert!(is_text_content_type("text/markdown"));
        assert!(is_text_content_type("application/json"));
        assert!(is_text_content_type("application/vnd.foo+json"));
        assert!(is_text_content_type(""));
        assert!(!is_text_content_type("image/png"));
        assert!(!is_text_content_type("application/pdf"));
        Ok(())
    }

    /// get_document_with_body (task #424): include_body=false returns metadata only; include_body=true
    /// tries to inline the current version's markdown from its CID, and when no IPFS backend is wired
    /// it degrades gracefully - the metadata still returns with body:null + a body_error, never a
    /// failed call - so a builder/reviewer tool keeps working even when content can't be resolved.
    #[tokio::test]
    async fn get_document_with_body_inlines_or_degrades() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = create_document(
            &pool,
            "Spec",
            None,
            "bafycurrent",
            None,
            Some("alice"),
            None,
            Some("text/markdown"),
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();

        // Metadata-only: identical to get_document, no body fields.
        let meta = get_document_with_body(&pool, None, did, false).await?;
        assert_eq!(meta, get_document(&pool, did).await?);
        assert!(meta.get("body").is_none());
        assert!(meta.get("body_error").is_none());

        // include_body with no backend: metadata still present, body null + a body_error note.
        let full = get_document_with_body(&pool, None, did, true).await?;
        assert_eq!(full["id"].as_i64(), Some(did));
        assert!(full["title"].as_str().is_some(), "metadata is preserved");
        assert!(
            full["body"].is_null(),
            "body is null when it can't be fetched"
        );
        let be = full["body_error"].as_str().expect("body_error note");
        assert!(be.contains("no IPFS backend"), "got: {be}");
        Ok(())
    }

    /// update_document renames a doc: it sets the title + derived slug, persists, emits
    /// document.updated to the owner/subscribers (actor excluded), and rejects an empty title or a
    /// missing document. Versions/content are untouched. (design-review-entity rename request.)
    #[tokio::test]
    async fn update_document_renames_title_and_slug() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = create_document(
            &pool,
            "Design: a very long working title",
            None,
            "bafy1",
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();

        // bob renames it to a short noun phrase.
        let updated = update_document(&pool, did, "Review entity", Some("bob")).await?;
        assert_eq!(updated["title"], json!("Review entity"));
        assert_eq!(updated["slug"], json!("review-entity"));
        // Persisted, and the current version (content) is unchanged.
        let got = get_document(&pool, did).await?;
        assert_eq!(got["title"], json!("Review entity"));
        assert_eq!(got["current_version"]["cid"], json!("bafy1"));

        // The owner (alice) hears document.updated with the new title; the actor (bob) does not.
        let alice = check_notifications(&pool, "alice", true, 50, None).await?;
        assert!(
            alice["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["type"] == json!("document.updated")
                    && n["data"]["title"] == json!("Review entity")),
            "owner notified of rename: {alice}"
        );
        let bob = check_notifications(&pool, "bob", true, 50, None).await?;
        assert_eq!(
            bob["count"].as_i64(),
            Some(0),
            "renamer excluded from own event"
        );

        // An empty title and an unknown document are rejected.
        assert!(update_document(&pool, did, "   ", Some("bob"))
            .await
            .is_err());
        assert!(update_document(&pool, 9999, "x", None).await.is_err());
        Ok(())
    }

    /// Documents are subscribable: the author is auto-subscribed on create, an explicit
    /// subscriber hears version publishes, the publishing actor is excluded from its own event,
    /// and unsubscribing stops delivery. Rides the existing inbox machinery (Recipients::FromDocument).
    #[tokio::test]
    async fn documents_subscription_and_events() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        // alice creates a doc (auto-subscribed as author).
        let d = create_document(
            &pool,
            "Spec",
            None,
            "bafy1",
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();
        // bob explicitly subscribes to the document.
        subscribe(&pool, "bob", None, None, None, Some(did), false).await?;

        // carol publishes v2 -> author (alice) + subscriber (bob) hear it; carol (actor) does not.
        publish_version(
            &pool,
            did,
            "bafy2",
            Some("second"),
            Some("carol"),
            None,
            None,
        )
        .await?;

        let alice = check_notifications(&pool, "alice", true, 50, None).await?;
        let bob = check_notifications(&pool, "bob", true, 50, None).await?;
        let carol = check_notifications(&pool, "carol", true, 50, None).await?;

        // alice was subscribed as author (not notified of her own create), so she hears the publish.
        let alice_types: BTreeSet<String> = alice["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap().to_string())
            .collect();
        assert!(
            alice_types.contains("document.version_published"),
            "author hears publishes: {alice}"
        );
        // bob (subscriber) hears exactly the publish, carrying the new version.
        assert_eq!(bob["count"].as_i64(), Some(1), "bob: {bob}");
        assert_eq!(
            bob["notifications"][0]["type"],
            json!("document.version_published")
        );
        assert_eq!(bob["notifications"][0]["data"]["version_no"], json!(2));
        // carol is the actor -> excluded from her own event.
        assert_eq!(carol["count"].as_i64(), Some(0), "actor excluded: {carol}");

        // Unsubscribing stops delivery.
        unsubscribe(&pool, "bob", None, None, None, Some(did), false).await?;
        publish_version(&pool, did, "bafy3", None, Some("alice"), None, None).await?;
        let bob2 = check_notifications(&pool, "bob", true, 50, None).await?;
        assert_eq!(
            bob2["count"].as_i64(),
            Some(0),
            "no events after unsubscribe"
        );

        Ok(())
    }

    /// A document's owner (creator) is notified when someone comments on it, even if the owner
    /// holds no subscription row — the owner is included explicitly, like a task's created_by, so
    /// a comment always reaches the person who should respond. (operator #30 seq-2746 / task #300.)
    #[tokio::test]
    async fn document_owner_notified_on_comment_without_subscription() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = create_document(
            &pool,
            "Spec",
            None,
            "bafy1",
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();

        // Remove the owner's auto-subscription, so the ONLY way alice can hear the comment is via
        // the explicit owner inclusion (this is the gap the fix closes).
        unsubscribe(&pool, "alice", None, None, None, Some(did), false).await?;

        // bob comments on alice's doc.
        comment_document(
            &pool,
            did,
            None,
            Some("bob"),
            "please clarify §2",
            None,
            None,
            None,
        )
        .await?;

        // alice (the owner) is still notified, despite having no subscription row.
        let alice = check_notifications(&pool, "alice", true, 50, None).await?;
        assert_eq!(
            alice["count"].as_i64(),
            Some(1),
            "owner notified without a subscription: {alice}"
        );
        let n = &alice["notifications"][0];
        assert_eq!(n["type"], json!("document.comment"));
        // The payload is self-describing — which document, its title — and names the commenter as
        // the event actor, so the owner can act without a lookup (task #300 + #313).
        assert_eq!(n["data"]["document_id"], json!(did));
        assert_eq!(n["data"]["title"], json!("Spec"));
        assert_eq!(
            n["actor"],
            json!("bob"),
            "the commenter is surfaced as the event actor"
        );

        // The commenter (actor) is not notified of their own comment.
        let bob = check_notifications(&pool, "bob", true, 50, None).await?;
        assert_eq!(
            bob["count"].as_i64(),
            Some(0),
            "actor excluded from own comment: {bob}"
        );
        Ok(())
    }

    /// An @mention in a DOCUMENT comment notifies the mentioned REGISTERED agent even with no prior
    /// subscription -- the operator-reported silent black hole (doc-comment @mentions did not fire,
    /// while task-comment ones did). Mirrors subscribe_mentions for tasks; unregistered @tokens are
    /// ignored (no junk subscription).
    #[tokio::test]
    async fn document_comment_mention_notifies_mentioned_agent() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "librarian", None, None, None, None, None).await?;
        let d = create_document(
            &pool,
            "Spec",
            None,
            "bafy1",
            None,
            Some("cameron"),
            None,
            None,
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();

        // cameron (owner) comments, @mentioning librarian (not the owner, no prior subscription) and
        // an UNREGISTERED @token that must be ignored (no junk sub, no error).
        comment_document(
            &pool,
            did,
            None,
            Some("cameron"),
            "@librarian please review section 2, cc @ghost-nobody",
            None,
            None,
            None,
        )
        .await?;

        // librarian is notified of the doc comment despite never subscribing.
        let lib = check_notifications(&pool, "librarian", true, 50, None).await?;
        let hit = lib["notifications"].as_array().unwrap().iter().find(|n| {
            n["type"] == json!("document.comment") && n["data"]["document_id"] == json!(did)
        });
        assert!(
            hit.is_some(),
            "mentioned agent notified of the doc comment: {lib}"
        );

        // The unregistered @token was ignored: no subscription row was created for it.
        let ghost_subbed = sqlx::query(
            "SELECT 1 FROM subscriptions WHERE subscriber='ghost-nobody' AND target_type='document' AND target_id=?",
        )
        .bind(did)
        .fetch_optional(&pool)
        .await?
        .is_some();
        assert!(
            !ghost_subbed,
            "unregistered @token must not create a subscription"
        );
        Ok(())
    }

    /// A document's owner is notified when it's approved (and on the other review transitions),
    /// with a self-describing payload — document_id, title, new status — plus the approver as the
    /// event actor, so they can act without a lookup. The approver is excluded from their own
    /// event. (operator #310.)
    #[tokio::test]
    async fn document_approval_notifies_owner_with_context() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = create_document(
            &pool,
            "Design X",
            None,
            "bafy1",
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();

        // bob (a reviewer) approves alice's document.
        approve_document(&pool, did, Some("bob")).await?;

        let alice = check_notifications(&pool, "alice", true, 50, None).await?;
        let n = &alice["notifications"][0];
        assert_eq!(n["type"], json!("document.approved"));
        assert_eq!(
            n["actor"],
            json!("bob"),
            "the approver is surfaced as the event actor"
        );
        assert_eq!(n["data"]["document_id"], json!(did));
        assert_eq!(n["data"]["title"], json!("Design X"));
        assert_eq!(n["data"]["status"], json!("approved"));
        // The approver is not notified of their own action.
        let bob = check_notifications(&pool, "bob", true, 50, None).await?;
        assert_eq!(
            bob["count"].as_i64(),
            Some(0),
            "approver excluded from own event"
        );

        // request_changes carries the same self-describing context.
        request_changes(&pool, did, Some("bob"), Some("tighten §2")).await?;
        let alice2 = check_notifications(&pool, "alice", true, 50, None).await?;
        let m = &alice2["notifications"][0];
        assert_eq!(m["type"], json!("document.changes_requested"));
        assert_eq!(m["data"]["document_id"], json!(did));
        assert_eq!(m["data"]["title"], json!("Design X"));
        assert_eq!(m["data"]["status"], json!("changes_requested"));
        assert_eq!(m["data"]["note"], json!("tighten §2"));
        Ok(())
    }

    /// Document comments: region-anchored + doc-level + threaded, region JSON round-trips,
    /// comments fan out to document subscribers, filter by version/status, and resolve flips
    /// status.
    #[tokio::test]
    async fn document_comments_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = create_document(
            &pool,
            "Spec",
            None,
            "bafy1",
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();
        let vid = d["current_version"]["id"].as_i64().unwrap();

        // bob subscribes so he hears comment activity.
        subscribe(&pool, "bob", None, None, None, Some(did), false).await?;

        // A region-anchored comment (carol). The region JSON is stored verbatim.
        let region = json!({
            "TextQuoteSelector": { "exact": "widgets", "prefix": "the ", "suffix": " are" },
            "TextPositionSelector": { "start": 10, "end": 17 }
        });
        let c = comment_document(
            &pool,
            did,
            Some(vid),
            Some("carol"),
            "typo",
            Some(region.clone()),
            None,
            None,
        )
        .await?;
        let cid = c["id"].as_i64().unwrap();
        assert_eq!(c["status"], json!("open"));
        assert_eq!(c["region"], region, "region round-trips as JSON");
        assert_eq!(c["version_id"], json!(vid));

        // A doc-level comment (no region), threaded under the first.
        let c2 = comment_document(
            &pool,
            did,
            None,
            Some("dave"),
            "agreed",
            None,
            Some(cid),
            None,
        )
        .await?;
        assert!(c2["region"].is_null(), "doc-level comment has null region");
        assert_eq!(c2["reply_to"], json!(cid));

        // bob (subscriber) heard both comments.
        let bob = check_notifications(&pool, "bob", true, 50, None).await?;
        assert_eq!(bob["count"].as_i64(), Some(2), "bob: {bob}");
        assert!(bob["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["type"] == json!("document.comment")));

        // List + filters.
        assert_eq!(
            get_document_comments(&pool, did, None, None)
                .await?
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            get_document_comments(&pool, did, Some(vid), None)
                .await?
                .as_array()
                .unwrap()
                .len(),
            1,
            "only the region comment carries this version_id"
        );

        // Resolve flips status and is filterable.
        let r = resolve_comment(&pool, cid, Some("alice")).await?;
        assert_eq!(r["status"], json!("resolved"));
        assert_eq!(
            get_document_comments(&pool, did, None, Some("open"))
                .await?
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            get_document_comments(&pool, did, None, Some("resolved"))
                .await?
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // An ingested comment attributed to an external human (§6): author stays the ingester,
        // external_author carries the identity, and it round-trips through get_document_comments.
        let c3 = comment_document(
            &pool,
            did,
            None,
            Some("slack-bridge"),
            "from ada",
            None,
            None,
            Some("slack:U1"),
        )
        .await?;
        assert_eq!(c3["author"], json!("slack-bridge"));
        assert_eq!(c3["external_author"], json!("slack:U1"));
        let listed = get_document_comments(&pool, did, None, None).await?;
        let c3_listed = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == c3["id"])
            .unwrap();
        assert_eq!(
            c3_listed["external_author"],
            json!("slack:U1"),
            "attribution surfaces in the list"
        );

        // Errors: comment on a missing doc, resolve a missing comment.
        assert!(
            comment_document(&pool, 999, None, Some("x"), "hi", None, None, None)
                .await
                .is_err()
        );
        assert!(resolve_comment(&pool, 999, Some("x")).await.is_err());
        Ok(())
    }

    /// The review loop: submit -> request_changes -> publish (reopens) -> approve stamps the
    /// current version -> publishing again reopens review. Each transition notifies subscribers.
    #[tokio::test]
    async fn document_review_workflow() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = create_document(
            &pool,
            "Design",
            None,
            "bafy1",
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();
        assert_eq!(d["status"], json!("draft"));

        // The operator subscribes and drives the review.
        subscribe(&pool, "operator", None, None, None, Some(did), false).await?;

        // Author submits for review.
        let r = submit_for_review(&pool, did, Some("alice")).await?;
        assert_eq!(r["status"], json!("in_review"));

        // Operator requests changes.
        let rc = request_changes(&pool, did, Some("operator"), Some("tighten section 2")).await?;
        assert_eq!(rc["status"], json!("changes_requested"));

        // Author publishes a new version -> reopens review (per publish_version).
        let v2 = publish_version(
            &pool,
            did,
            "bafy2",
            Some("addressed"),
            Some("alice"),
            None,
            None,
        )
        .await?;
        assert_eq!(
            v2["status"],
            json!("in_review"),
            "a new version reopens review"
        );
        let v2id = v2["current_version"]["id"].as_i64().unwrap();

        // Operator approves -> stamps the current version.
        let ap = approve_document(&pool, did, Some("operator")).await?;
        assert_eq!(ap["status"], json!("approved"));
        assert_eq!(
            ap["approved_version_id"],
            json!(v2id),
            "approval stamps the current version"
        );
        assert_eq!(ap["approved_by"], json!("operator"));

        // Approval is a stamp, not a lock: publishing again reopens review but keeps the stamp.
        let v3 = publish_version(&pool, did, "bafy3", None, Some("alice"), None, None).await?;
        assert_eq!(v3["status"], json!("in_review"));
        assert_eq!(
            v3["approved_version_id"],
            json!(v2id),
            "stamp persists across a new version"
        );

        // The operator (a subscriber) heard the author-driven transitions (submit, both publishes)
        // but not their own request_changes/approve (actor excluded).
        let ops = check_notifications(&pool, "operator", true, 50, None).await?;
        let types: Vec<String> = ops["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap().to_string())
            .collect();
        assert!(
            types.contains(&"document.submitted_for_review".to_string()),
            "{types:?}"
        );
        assert!(
            types.contains(&"document.version_published".to_string()),
            "{types:?}"
        );
        assert!(
            !types.contains(&"document.approved".to_string()),
            "actor excluded from own approve: {types:?}"
        );

        // Missing doc errors.
        assert!(approve_document(&pool, 999, Some("x")).await.is_err());
        Ok(())
    }

    /// Attaching a document to a task links both sides (surfaced in get_task + get_document),
    /// is idempotent, notifies both the doc's and the task's subscribers, and detaches cleanly.
    #[tokio::test]
    async fn document_attachment_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        // A task owned by alice, and a doc authored by bob.
        let t = create_task(
            &pool,
            pid,
            "Build widget",
            None,
            Some("alice"),
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        let d = create_document(
            &pool,
            "Widget design",
            None,
            "bafy1",
            None,
            Some("bob"),
            None,
            None,
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();

        // Drain the create notifications so the attach fan-out is isolated.
        let _ = check_notifications(&pool, "alice", true, 50, None).await?;
        let _ = check_notifications(&pool, "bob", true, 50, None).await?;

        // Attach (carol acts) -> both sides see the link.
        let r = attach_document(&pool, did, tid, Some("carol")).await?;
        assert_eq!(r["attached"], json!(true));
        let task = get_task(&pool, tid).await?;
        assert_eq!(task["attached_documents"].as_array().unwrap().len(), 1);
        assert_eq!(task["attached_documents"][0]["id"], json!(did));
        let doc = get_document(&pool, did).await?;
        assert_eq!(doc["attached_tasks"].as_array().unwrap().len(), 1);
        assert_eq!(doc["attached_tasks"][0]["id"], json!(tid));
        // Both summaries carry project_id so a client can deep-link. The task belongs to pid;
        // the doc was created without a project, so its summary's project_id is null (present).
        assert_eq!(doc["attached_tasks"][0]["project_id"], json!(pid));
        assert!(task["attached_documents"][0]
            .as_object()
            .unwrap()
            .contains_key("project_id"));
        assert!(task["attached_documents"][0]["project_id"].is_null());

        // Idempotent: re-attaching doesn't duplicate.
        attach_document(&pool, did, tid, Some("carol")).await?;
        assert_eq!(
            get_task(&pool, tid).await?["attached_documents"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // Both the task owner (alice, subscribed as assignee/creator) and the doc author (bob,
        // subscribed on create) heard document.attached; carol (actor) did not.
        let alice = check_notifications(&pool, "alice", true, 50, None).await?;
        let bob = check_notifications(&pool, "bob", true, 50, None).await?;
        assert!(
            alice["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["type"] == json!("document.attached")),
            "task watcher heard it: {alice}"
        );
        assert!(
            bob["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["type"] == json!("document.attached")),
            "doc watcher heard it: {bob}"
        );

        // Detach removes the link and reports the removal.
        let rm = detach_document(&pool, did, tid, Some("carol")).await?;
        assert_eq!(rm["removed"], json!(1));
        assert_eq!(
            get_task(&pool, tid).await?["attached_documents"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        // Detaching again is a no-op (0 removed).
        assert_eq!(
            detach_document(&pool, did, tid, Some("carol")).await?["removed"],
            json!(0)
        );

        // Attaching to a missing task or doc errors.
        assert!(attach_document(&pool, did, 9999, Some("carol"))
            .await
            .is_err());
        assert!(attach_document(&pool, 9999, tid, Some("carol"))
            .await
            .is_err());
        Ok(())
    }

    /// list_documents discovery filters: project, status, tag (metadata.tags), task_id
    /// (attachment), author — each alone and combined (AND).
    #[tokio::test]
    async fn list_documents_discovery_filters() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();

        let a = create_document(
            &pool,
            "A",
            Some(pid),
            "bafyA",
            None,
            Some("alice"),
            Some(json!({ "tags": ["design", "rfc"] })),
            None,
            None,
        )
        .await?;
        let aid = a["id"].as_i64().unwrap();
        let b = create_document(
            &pool,
            "B",
            None,
            "bafyB",
            None,
            Some("bob"),
            Some(json!({ "tags": ["ops"] })),
            None,
            None,
        )
        .await?;
        let bid = b["id"].as_i64().unwrap();
        let c = create_document(
            &pool,
            "C",
            Some(pid),
            "bafyC",
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let cid = c["id"].as_i64().unwrap();
        approve_document(&pool, cid, Some("op")).await?;

        // Attach A to a task.
        let t = create_task(
            &pool,
            pid,
            "T",
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        attach_document(&pool, aid, tid, Some("u")).await?;

        let ids = |v: &Value| -> Vec<i64> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|d| d["id"].as_i64().unwrap())
                .collect()
        };

        // author
        assert_eq!(
            ids(&list_documents(&pool, None, None, None, None, Some("alice"), false).await?),
            vec![aid, cid]
        );
        assert_eq!(
            ids(&list_documents(&pool, None, None, None, None, Some("bob"), false).await?),
            vec![bid]
        );
        // tag
        assert_eq!(
            ids(&list_documents(&pool, None, None, Some("design"), None, None, false).await?),
            vec![aid]
        );
        assert_eq!(
            ids(&list_documents(&pool, None, None, Some("ops"), None, None, false).await?),
            vec![bid]
        );
        assert!(
            list_documents(&pool, None, None, Some("nope"), None, None, false)
                .await?
                .as_array()
                .unwrap()
                .is_empty()
        );
        // task attachment
        assert_eq!(
            ids(&list_documents(&pool, None, None, None, Some(tid), None, false).await?),
            vec![aid]
        );
        // project
        assert_eq!(
            ids(&list_documents(&pool, Some(pid), None, None, None, None, false).await?),
            vec![aid, cid]
        );
        // status
        assert_eq!(
            ids(&list_documents(&pool, None, Some("approved"), None, None, None, false).await?),
            vec![cid]
        );
        // combined AND: project + tag rfc + author alice -> only A
        assert_eq!(
            ids(&list_documents(
                &pool,
                Some(pid),
                None,
                Some("rfc"),
                None,
                Some("alice"),
                false
            )
            .await?),
            vec![aid]
        );
        // contradictory combo -> empty
        assert!(
            list_documents(&pool, None, None, Some("ops"), None, Some("alice"), false)
                .await?
                .as_array()
                .unwrap()
                .is_empty()
        );
        Ok(())
    }

    /// Task nesting/epics: parent_id on create + update, same-project + self + cycle guards,
    /// get_task children/roll-up/parent, list_tasks top_level + parent_id filters, clear-parent,
    /// and the move_task cross-project guard for entangled tasks.
    #[tokio::test]
    async fn task_parent_epic_nesting() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let p2 = create_project(&pool, "P2", None, Some("u"), None).await?;
        let pid2 = p2["id"].as_i64().unwrap();

        let ids = |v: &Value| -> Vec<i64> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|t| t["id"].as_i64().unwrap())
                .collect()
        };

        // An epic with two children.
        let epic = create_task(
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
        let eid = epic["id"].as_i64().unwrap();
        let c1 = create_task(
            &pool,
            pid,
            "c1",
            None,
            None,
            None,
            Some("u"),
            None,
            Some(eid),
            None,
        )
        .await?;
        let c1id = c1["id"].as_i64().unwrap();
        let c2 = create_task(
            &pool,
            pid,
            "c2",
            None,
            None,
            None,
            Some("u"),
            None,
            Some(eid),
            None,
        )
        .await?;
        let c2id = c2["id"].as_i64().unwrap();

        // Cross-project parent rejected at create.
        assert!(create_task(
            &pool,
            pid2,
            "x",
            None,
            None,
            None,
            Some("u"),
            None,
            Some(eid),
            None
        )
        .await
        .is_err());
        // Non-existent parent rejected.
        assert!(create_task(
            &pool,
            pid,
            "y",
            None,
            None,
            None,
            Some("u"),
            None,
            Some(99999),
            None
        )
        .await
        .is_err());

        // get_task: children + roll-up.
        let e = get_task(&pool, eid).await?;
        assert_eq!(ids(&e["children"]), vec![c1id, c2id]);
        assert_eq!(e["child_rollup"], json!({ "done": 0, "total": 2 }));

        // Mark c1 done -> roll-up 1/2.
        update_task(
            &pool,
            c1id,
            Some("done"),
            None,
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        assert_eq!(
            get_task(&pool, eid).await?["child_rollup"],
            json!({ "done": 1, "total": 2 })
        );

        // Child surfaces parent_id + parent_title.
        let c = get_task(&pool, c1id).await?;
        assert_eq!(c["parent_id"], json!(eid));
        assert_eq!(c["parent_title"], json!("Epic"));

        // list_tasks top_level -> only the epic; parent_id -> the two children.
        assert_eq!(
            ids(&list_tasks(
                &pool,
                Some(pid),
                None,
                None,
                false,
                None,
                true,
                None,
                None,
                None,
                None,
                None,
                false
            )
            .await?),
            vec![eid]
        );
        assert_eq!(
            ids(&list_tasks(
                &pool,
                Some(pid),
                None,
                None,
                false,
                Some(eid),
                false,
                None,
                None,
                None,
                None,
                None,
                false
            )
            .await?),
            vec![c1id, c2id]
        );

        // Guards: self-parent + cycle rejected.
        assert!(update_task(
            &pool,
            eid,
            None,
            None,
            None,
            None,
            None,
            Some("u"),
            None,
            Some(eid),
            None
        )
        .await
        .is_err());
        assert!(update_task(
            &pool,
            eid,
            None,
            None,
            None,
            None,
            None,
            Some("u"),
            None,
            Some(c1id),
            None
        )
        .await
        .is_err());

        // Clear c2's parent (parent_id=0) -> top-level; emits task.reparented; roll-up shrinks.
        let r = update_task(
            &pool,
            c2id,
            None,
            None,
            None,
            None,
            None,
            Some("u"),
            None,
            Some(0),
            None,
        )
        .await?;
        assert!(r["parent_id"].is_null());
        assert_eq!(
            get_task(&pool, eid).await?["child_rollup"],
            json!({ "done": 1, "total": 1 })
        );
        let evs = get_events(&pool, 0, 200, None, false).await?;
        assert!(evs
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == json!("task.reparented")));

        // move_task guard: the epic still has a child -> cannot cross projects.
        assert!(move_task(&pool, eid, pid2, Some("u")).await.is_err());
        // c2 is now top-level with no children -> it can move.
        assert_eq!(
            move_task(&pool, c2id, pid2, Some("u")).await?["project_id"],
            json!(pid2)
        );
        Ok(())
    }

    /// list_tasks free-text search spans projects (no project_id), matches title OR description
    /// case-insensitively, and composes with assignee/project filters.
    #[tokio::test]
    async fn list_tasks_text_search() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p1 = create_project(&pool, "Alpha", None, Some("u"), None).await?;
        let pid1 = p1["id"].as_i64().unwrap();
        let p2 = create_project(&pool, "Beta", None, Some("u"), None).await?;
        let pid2 = p2["id"].as_i64().unwrap();

        create_task(
            &pool,
            pid1,
            "Fix the widget pipeline",
            Some("handles reflow"),
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        create_task(
            &pool,
            pid1,
            "Unrelated chore",
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        create_task(
            &pool,
            pid2,
            "Widget docs",
            Some("describe the WIDGET api"),
            Some("alice"),
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;

        let titles = |v: &Value| -> Vec<String> {
            let mut t: Vec<String> = v
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["title"].as_str().unwrap().to_string())
                .collect();
            t.sort();
            t
        };

        // "widget" across ALL projects (case-insensitive) -> the two widget tasks, not the chore.
        assert_eq!(
            titles(
                &list_tasks(
                    &pool,
                    None,
                    None,
                    None,
                    false,
                    None,
                    false,
                    Some("widget"),
                    None,
                    None,
                    None,
                    None,
                    false
                )
                .await?
            ),
            vec![
                "Fix the widget pipeline".to_string(),
                "Widget docs".to_string()
            ]
        );
        // Matches description too.
        assert_eq!(
            titles(
                &list_tasks(
                    &pool,
                    None,
                    None,
                    None,
                    false,
                    None,
                    false,
                    Some("reflow"),
                    None,
                    None,
                    None,
                    None,
                    false
                )
                .await?
            ),
            vec!["Fix the widget pipeline".to_string()]
        );
        // Composable with assignee: widget + alice -> only the Beta doc task.
        assert_eq!(
            titles(
                &list_tasks(
                    &pool,
                    None,
                    None,
                    Some("alice"),
                    false,
                    None,
                    false,
                    Some("widget"),
                    None,
                    None,
                    None,
                    None,
                    false
                )
                .await?
            ),
            vec!["Widget docs".to_string()]
        );
        // Composable with project scope: widget in Alpha -> only the pipeline task.
        assert_eq!(
            titles(
                &list_tasks(
                    &pool,
                    Some(pid1),
                    None,
                    None,
                    false,
                    None,
                    false,
                    Some("widget"),
                    None,
                    None,
                    None,
                    None,
                    false
                )
                .await?
            ),
            vec!["Fix the widget pipeline".to_string()]
        );
        // No match -> empty.
        assert!(list_tasks(
            &pool,
            None,
            None,
            None,
            false,
            None,
            false,
            Some("zzznope"),
            None,
            None,
            None,
            None,
            false
        )
        .await?
        .as_array()
        .unwrap()
        .is_empty());
        Ok(())
    }

    /// metadata is merged (not overwritten) on update_task and set_task_props.
    #[tokio::test]
    async fn metadata_merges() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, None, None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            pid,
            "T",
            None,
            None,
            None,
            None,
            Some(json!({"a": 1})),
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();

        set_task_props(&pool, tid, json!({"b": 2})).await?;
        update_task(
            &pool,
            tid,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(json!({"c": 3})),
            None,
            None,
        )
        .await?;

        let task = get_task(&pool, tid).await?;
        assert_eq!(task["metadata"], json!({"a": 1, "b": 2, "c": 3}));
        Ok(())
    }

    /// The agent list can serve as the fleet registry: agents carry a merged metadata bag,
    /// and update_agent mutates fields + merges metadata without re-registering (and without
    /// forcing status back to online, unlike register_agent).
    #[tokio::test]
    async fn agent_registry_metadata() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        // Register with an initial bag + charter.
        let a = register_agent(
            &pool,
            "v-x",
            Some("V X"),
            Some("vertical"),
            Some("own X"),
            Some(json!({"role": "vertical", "model": "opus"})),
            None,
        )
        .await?;
        assert_eq!(a["metadata"], json!({"role": "vertical", "model": "opus"}));
        assert_eq!(a["status"], "online");
        assert_eq!(a["charter"], "own X");

        // Re-register with a partial bag: merges (model kept), doesn't clobber charter.
        let a = register_agent(
            &pool,
            "v-x",
            None,
            None,
            None,
            Some(json!({"effort": "high"})),
            None,
        )
        .await?;
        assert_eq!(
            a["metadata"],
            json!({"role": "vertical", "model": "opus", "effort": "high"})
        );
        assert_eq!(a["charter"], "own X");

        // update_agent: away without a message keeps status_message; metadata merges again.
        update_agent(
            &pool,
            "v-x",
            None,
            None,
            None,
            Some("away"),
            None,
            None,
            Some(json!({"branch": "main"})),
            None,
        )
        .await?;
        let got = get_agent(&pool, "v-x").await?;
        assert_eq!(got["status"], "away");
        assert_eq!(
            got["metadata"],
            json!({"role": "vertical", "model": "opus", "effort": "high", "branch": "main"})
        );

        // clear affordance (task 489): set then CLEAR webhook_url to null in one call. A null/omitted
        // field would leave it unchanged, so clear is the only way to empty it.
        update_agent(
            &pool,
            "v-x",
            None,
            None,
            None,
            None,
            None,
            Some("http://x/wake"),
            None,
            None,
        )
        .await?;
        assert_eq!(
            get_agent(&pool, "v-x").await?["webhook_url"],
            json!("http://x/wake")
        );
        update_agent(
            &pool,
            "v-x",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&["webhook_url".to_string()]),
        )
        .await?;
        assert!(
            get_agent(&pool, "v-x").await?["webhook_url"].is_null(),
            "clear empties the field"
        );
        // An explicit value wins over clearing the same field in one call.
        update_agent(
            &pool,
            "v-x",
            None,
            None,
            None,
            None,
            None,
            Some("http://y/wake"),
            None,
            Some(&["webhook_url".to_string()]),
        )
        .await?;
        assert_eq!(
            get_agent(&pool, "v-x").await?["webhook_url"],
            json!("http://y/wake"),
            "explicit value wins over clear"
        );
        // A non-clearable field name is rejected.
        assert!(update_agent(
            &pool,
            "v-x",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&["status".to_string()])
        )
        .await
        .is_err());

        // update_agent on an unknown agent errors (it's a mutate, not an upsert).
        assert!(
            update_agent(&pool, "nope", None, None, None, None, None, None, None, None)
                .await
                .is_err()
        );

        // A fresh agent gets an empty bag by default, not null.
        register_agent(&pool, "v-y", None, None, None, None, None).await?;
        assert_eq!(get_agent(&pool, "v-y").await?["metadata"], json!({}));
        Ok(())
    }

    /// list_agents is a lightweight roster by default (task #418): compact {id, display_name,
    /// status}, no charter/metadata blob; verbose returns the full objects; status/q/meta filters
    /// and limit/offset narrow it.
    #[tokio::test]
    async fn list_agents_roster_projection_and_filters() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let big_charter = "x".repeat(3000);
        register_agent(
            &pool,
            "v-compiler",
            Some("Compiler"),
            Some("vertical"),
            Some(&big_charter),
            Some(json!({"area":"compiler"})),
            None,
        )
        .await?;
        register_agent(
            &pool,
            "v-runtime",
            Some("Runtime"),
            Some("vertical"),
            Some(&big_charter),
            Some(json!({"area":"runtime"})),
            None,
        )
        .await?;
        register_agent(
            &pool,
            "concierge",
            Some("Concierge"),
            Some("ops"),
            Some(&big_charter),
            Some(json!({"area":"ops"})),
            None,
        )
        .await?;
        set_status(&pool, "v-compiler", "online", None).await?;
        set_status(&pool, "v-runtime", "offline", None).await?;
        set_status(&pool, "concierge", "offline", None).await?; // register defaults to online

        // Default: compact projection — id/display_name/status + the small metadata bag (consumers
        // filter on it, e.g. the fleet watchdog on metadata.native, task 477), but NOT the large
        // charter (the compaction that keeps the roster under the token cap, #418).
        let roster = list_agents(&pool, None, None, None, None, false, None, None).await?;
        let arr = roster.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        for a in arr {
            assert!(a.get("id").is_some() && a.get("status").is_some());
            assert!(
                a.get("charter").is_none(),
                "roster must omit the heavy charter: {a}"
            );
            assert!(
                a.get("metadata").is_some(),
                "roster must include metadata for filtering: {a}"
            );
        }
        // The metadata bag is the real object, so a consumer can filter on it (e.g. metadata.area).
        let compiler = arr.iter().find(|a| a["id"] == json!("v-compiler")).unwrap();
        assert_eq!(compiler["metadata"]["area"], json!("compiler"));

        // verbose -> full objects (charter present).
        let full = list_agents(&pool, None, None, None, None, true, None, None).await?;
        assert!(full
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["charter"].is_string()));

        // status filter.
        let online =
            list_agents(&pool, Some("online"), None, None, None, false, None, None).await?;
        let online = online.as_array().unwrap();
        assert_eq!(online.len(), 1);
        assert_eq!(online[0]["id"], json!("v-compiler"));

        // q substring over id + display_name.
        let q = list_agents(&pool, None, Some("runtime"), None, None, false, None, None).await?;
        assert_eq!(q.as_array().unwrap().len(), 1);
        assert_eq!(q.as_array().unwrap()[0]["id"], json!("v-runtime"));

        // meta_key/meta_value routing filter (the v-cadenza-ci case: find the owning area).
        let by_area = list_agents(
            &pool,
            None,
            None,
            Some("area"),
            Some("compiler"),
            false,
            None,
            None,
        )
        .await?;
        assert_eq!(by_area.as_array().unwrap().len(), 1);
        assert_eq!(by_area.as_array().unwrap()[0]["id"], json!("v-compiler"));

        // limit + offset paginate.
        let page1 = list_agents(&pool, None, None, None, None, false, Some(2), Some(0)).await?;
        let page2 = list_agents(&pool, None, None, None, None, false, Some(2), Some(2)).await?;
        assert_eq!(page1.as_array().unwrap().len(), 2);
        assert_eq!(page2.as_array().unwrap().len(), 1);
        Ok(())
    }

    /// Consumer-field contract (task 500): the default (non-verbose) list_agents projection MUST keep
    /// every FLEET_CONSUMED_ROSTER_FIELDS entry, so a future compaction cannot silently drop a field a
    /// cross-repo fleet consumer depends on (the task 418 regression that dropped `metadata` and dark-
    /// started the whole observer cadence). This generalizes task 477's one-off metadata-present assert
    /// into the named contract: dropping a contracted field from COMPACT_ROSTER_FIELDS reds the gate.
    #[tokio::test]
    async fn fleet_consumed_roster_fields_contract() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        // An agent carrying the exact key the fleet watchdog filters on (metadata.native).
        register_agent(
            &pool,
            "v-x",
            None,
            None,
            None,
            Some(json!({"native": true, "area": "x"})),
            None,
        )
        .await?;

        let roster = list_agents(&pool, None, None, None, None, false, None, None).await?;
        let entry = roster
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == json!("v-x"))
            .unwrap();
        for field in FLEET_CONSUMED_ROSTER_FIELDS {
            assert!(
                entry.get(*field).is_some(),
                "contract: default roster projection must keep fleet-consumed field '{field}': {entry}"
            );
        }
        // The specific consumer key must survive, not just the metadata object shell.
        assert_eq!(
            entry["metadata"]["native"],
            json!(true),
            "metadata.native must round-trip in the roster"
        );
        // Every contracted field is actually part of the projection list (guards a typo'd contract
        // that names a field the projection never emits).
        for field in FLEET_CONSUMED_ROSTER_FIELDS {
            assert!(
                COMPACT_ROSTER_FIELDS.contains(field),
                "contract field '{field}' is not in COMPACT_ROSTER_FIELDS -- the projection cannot emit it"
            );
        }
        Ok(())
    }

    /// The Rust impl can open a legacy board.db created before tasks.metadata existed:
    /// init() back-fills the column, and existing rows keep working.
    #[tokio::test]
    async fn opens_legacy_db_without_metadata() -> anyhow::Result<()> {
        use sqlx::Row;
        use std::str::FromStr;
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("legacy.db");
        let url = format!("sqlite://{}", path.to_str().unwrap());

        // Build a pre-metadata DB: tasks table WITHOUT the metadata column, plus a row.
        {
            let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)?.create_if_missing(true);
            let legacy = sqlx::SqlitePool::connect_with(opts).await?;
            sqlx::query(
                "CREATE TABLE projects (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, \
                 description TEXT, status TEXT NOT NULL DEFAULT 'active', created_by TEXT, \
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
            )
            .execute(&legacy)
            .await?;
            sqlx::query(
                "CREATE TABLE tasks (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 project_id INTEGER NOT NULL, title TEXT NOT NULL, description TEXT, \
                 status TEXT NOT NULL DEFAULT 'todo', priority TEXT, assignee TEXT, \
                 created_by TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
            )
            .execute(&legacy)
            .await?;
            sqlx::query("INSERT INTO projects(name, created_at, updated_at) VALUES('old','t','t')")
                .execute(&legacy)
                .await?;
            sqlx::query(
                "INSERT INTO tasks(project_id, title, created_at, updated_at) \
                 VALUES(1,'old task','t','t')",
            )
            .execute(&legacy)
            .await?;
            legacy.close().await;
        }

        // Open it with the real init: the migration adds tasks.metadata (default '{}').
        let pool = crate::db::init(path.to_str().unwrap()).await?;
        let has_meta = sqlx::query("PRAGMA table_info(tasks)")
            .fetch_all(&pool)
            .await?
            .iter()
            .any(|r| r.get::<String, _>("name") == "metadata");
        assert!(has_meta, "migration should have added tasks.metadata");

        // The pre-existing row reads back with an empty metadata object, and new writes work.
        let old = get_task(&pool, 1).await?;
        assert_eq!(old["title"], json!("old task"));
        assert_eq!(old["metadata"], json!({}));
        set_task_props(&pool, 1, json!({"stage": "resumed"})).await?;
        let after = get_task(&pool, 1).await?;
        assert_eq!(after["metadata"], json!({"stage": "resumed"}));
        Ok(())
    }

    /// create_project is case-insensitively idempotent: a differently-cased name returns
    /// the existing project instead of making a duplicate.
    #[tokio::test]
    async fn create_project_is_case_insensitive_get_or_create() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        let a = create_project(&pool, "Backend", Some("first"), Some("u"), None).await?;
        let b = create_project(&pool, "backend", Some("dupe"), Some("u"), None).await?;
        let c = create_project(&pool, "BACKEND", None, None, None).await?;

        assert_eq!(a["id"], b["id"]);
        assert_eq!(a["id"], c["id"]);
        // Original row is untouched (name/description preserved, not overwritten).
        assert_eq!(b["name"], json!("Backend"));
        assert_eq!(b["description"], json!("first"));

        let projects = list_projects(&pool, None).await?;
        assert_eq!(projects.as_array().unwrap().len(), 1);
        Ok(())
    }

    /// merge_duplicate_projects folds case-variant projects into the earliest one,
    /// repointing tasks, events, and subscriptions (deduping subscription collisions).
    #[tokio::test]
    async fn merge_duplicate_projects_folds_and_repoints() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        // Two case-variant projects created directly (bypass get-or-create) to simulate
        // the pre-existing sprawl.
        let ts = now_iso();
        for (name, t) in [
            ("Backend", "2026-01-01T00:00:00Z"),
            ("backend", "2026-02-01T00:00:00Z"),
        ] {
            sqlx::query("INSERT INTO projects(name, created_at, updated_at) VALUES(?,?,?)")
                .bind(name)
                .bind(t)
                .bind(&ts)
                .execute(&pool)
                .await?;
        }
        // ids: 1 = "Backend" (earlier), 2 = "backend" (later).
        create_task(
            &pool,
            2,
            "on dupe",
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        // Same subscriber on both projects -> collision on repoint; both on dupe only too.
        subscribe(&pool, "alice", None, Some(1), None, None, false).await?;
        subscribe(&pool, "alice", None, Some(2), None, None, false).await?; // will collide with keep=1
        subscribe(&pool, "bob", None, Some(2), None, None, false).await?; // repoints cleanly onto 1

        let report = merge_duplicate_projects(&pool).await?;
        assert_eq!(report["merged_groups"], json!(1));
        assert_eq!(report["projects_removed"], json!(1));

        // Only the earliest survives.
        let projects = list_projects(&pool, None).await?;
        let arr = projects.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], json!(1));

        // The task moved onto the surviving project.
        let tasks = list_tasks(
            &pool,
            Some(1),
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
        assert_eq!(tasks.as_array().unwrap().len(), 1);

        // Subscriptions: alice (deduped to one), bob (repointed) both on project 1.
        let subs: Vec<String> = sqlx::query(
            "SELECT subscriber FROM subscriptions WHERE target_type='project' AND target_id=1 ORDER BY subscriber",
        )
        .fetch_all(&pool)
        .await?
        .iter()
        .map(|r| r.get::<String, _>("subscriber"))
        .collect();
        assert_eq!(subs, vec!["alice".to_string(), "bob".to_string()]);

        // Running again is a no-op.
        let again = merge_duplicate_projects(&pool).await?;
        assert_eq!(again["merged_groups"], json!(0));
        Ok(())
    }

    /// update_project renames, archives (status), and MERGES metadata; the reads surface
    /// metadata as a parsed object.
    #[tokio::test]
    async fn update_project_renames_archives_and_merges_metadata() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        let p =
            create_project(&pool, "Alpha", None, Some("u"), Some(json!({"repo": "r1"}))).await?;
        let pid = p["id"].as_i64().unwrap();
        // create returns metadata parsed as an object, not a JSON string.
        assert_eq!(p["metadata"], json!({"repo": "r1"}));

        // Rename + add a metadata key (existing keys preserved = merge, not replace).
        let up = update_project(
            &pool,
            pid,
            Some("Alpha Prime"),
            None,
            None,
            Some(json!({"lang": "rust"})),
            Some("u"),
        )
        .await?;
        assert_eq!(up["name"], json!("Alpha Prime"));
        assert_eq!(up["metadata"], json!({"repo": "r1", "lang": "rust"}));

        // Archive it: it drops out of the active-filtered list but is still there.
        update_project(&pool, pid, None, None, Some("archived"), None, Some("u")).await?;
        let active = list_projects(&pool, Some("active")).await?;
        assert_eq!(
            active.as_array().unwrap().len(),
            0,
            "archived project hidden from active list"
        );
        let archived = list_projects(&pool, Some("archived")).await?;
        assert_eq!(archived.as_array().unwrap().len(), 1);

        // Restore.
        update_project(&pool, pid, None, None, Some("active"), None, Some("u")).await?;
        let active = list_projects(&pool, Some("active")).await?;
        assert_eq!(active.as_array().unwrap().len(), 1);
        Ok(())
    }

    /// A rename that collides (case-insensitively) with another project is rejected.
    #[tokio::test]
    async fn update_project_rename_collision_is_rejected() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let a = create_project(&pool, "Alpha", None, Some("u"), None).await?;
        create_project(&pool, "Beta", None, Some("u"), None).await?;
        let aid = a["id"].as_i64().unwrap();

        // Rename Alpha -> "beta" (different case) collides with the existing Beta.
        let err = update_project(&pool, aid, Some("beta"), None, None, None, Some("u")).await;
        assert!(err.is_err(), "rename onto an existing name should fail");

        // Renaming to a different case of its OWN name is allowed (no real collision).
        let ok = update_project(&pool, aid, Some("ALPHA"), None, None, None, Some("u")).await?;
        assert_eq!(ok["name"], json!("ALPHA"));
        Ok(())
    }

    /// move_task reparents a task and emits task.moved with both project ids; a no-op move
    /// (same project) leaves it unchanged.
    #[tokio::test]
    async fn move_task_reparents_and_emits() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let a = create_project(&pool, "A", None, Some("u"), None).await?;
        let b = create_project(&pool, "B", None, Some("u"), None).await?;
        let aid = a["id"].as_i64().unwrap();
        let bid = b["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            aid,
            "T",
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();

        let moved = move_task(&pool, tid, bid, Some("u")).await?;
        assert_eq!(moved["project_id"], json!(bid));
        // It now lists under B, not A.
        assert_eq!(
            list_tasks(
                &pool,
                Some(aid),
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
                false
            )
            .await?
            .as_array()
            .unwrap()
            .len(),
            0
        );
        assert_eq!(
            list_tasks(
                &pool,
                Some(bid),
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
                false
            )
            .await?
            .as_array()
            .unwrap()
            .len(),
            1
        );

        // A task.moved event was recorded carrying both ends.
        let events = get_events(&pool, 0, 100, None, false).await?;
        let ev = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == json!("task.moved"))
            .expect("task.moved emitted");
        assert_eq!(ev["data"]["from_project_id"], json!(aid));
        assert_eq!(ev["data"]["to_project_id"], json!(bid));

        // Moving onto the current project is a no-op (no new event, still on B).
        let before = get_events(&pool, 0, 100, None, false)
            .await?
            .as_array()
            .unwrap()
            .len();
        move_task(&pool, tid, bid, Some("u")).await?;
        let after = get_events(&pool, 0, 100, None, false)
            .await?
            .as_array()
            .unwrap()
            .len();
        assert_eq!(before, after, "no-op move should not emit an event");
        Ok(())
    }

    /// list_tasks(unassigned=true) returns only tasks with no assignee, and that intent takes
    /// precedence over a contradictory assignee= equality filter.
    #[tokio::test]
    async fn list_tasks_unassigned_filter() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        create_task(
            &pool,
            pid,
            "owned",
            None,
            Some("alice"),
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        create_task(
            &pool,
            pid,
            "free",
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;

        // unassigned=true -> only the ownerless task.
        let un = list_tasks(
            &pool,
            Some(pid),
            None,
            None,
            true,
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
        let un = un.as_array().unwrap();
        assert_eq!(un.len(), 1);
        assert_eq!(un[0]["title"], json!("free"));
        assert!(un[0]["assignee"].is_null());

        // assignee equality still works when unassigned is false.
        let mine = list_tasks(
            &pool,
            Some(pid),
            None,
            Some("alice"),
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
        let mine = mine.as_array().unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["title"], json!("owned"));

        // unassigned=true wins over a contradictory assignee= filter (no owner beats owner=alice).
        let both = list_tasks(
            &pool,
            Some(pid),
            None,
            Some("alice"),
            true,
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
        let both = both.as_array().unwrap();
        assert_eq!(both.len(), 1);
        assert_eq!(both[0]["title"], json!("free"));

        // No filter returns both.
        assert_eq!(
            list_tasks(
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
                false
            )
            .await?
            .as_array()
            .unwrap()
            .len(),
            2
        );
        Ok(())
    }

    /// (meta_key, meta_value) filters to tasks whose metadata JSON has that key equal to that value
    /// — the idempotency query "is there already an open task observing target X". Both must be set
    /// to apply, and it composes with the other filters (project_id + status).
    #[tokio::test]
    async fn list_tasks_metadata_filter() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        // Two tasks observing "widget-a", one observing "widget-b", one with no metadata.
        create_task(
            &pool,
            pid,
            "obs-a1",
            None,
            None,
            None,
            Some("u"),
            Some(json!({"observes": "widget-a"})),
            None,
            None,
        )
        .await?;
        create_task(
            &pool,
            pid,
            "obs-a2",
            None,
            None,
            None,
            Some("u"),
            Some(json!({"observes": "widget-a"})),
            None,
            None,
        )
        .await?;
        create_task(
            &pool,
            pid,
            "obs-b",
            None,
            None,
            None,
            Some("u"),
            Some(json!({"observes": "widget-b"})),
            None,
            None,
        )
        .await?;
        create_task(
            &pool,
            pid,
            "plain",
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;

        let a = list_tasks(
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
            Some("observes"),
            Some("widget-a"),
            false,
        )
        .await?;
        let titles: Vec<_> = a
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["title"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(titles, vec!["obs-a1", "obs-a2"]);

        // Composes with a status filter: no open task observes widget-b once it's marked done.
        let bid = list_tasks(
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
            Some("observes"),
            Some("widget-b"),
            false,
        )
        .await?[0]["id"]
            .as_i64()
            .unwrap();
        update_task(
            &pool,
            bid,
            Some("done"),
            None,
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        let open_b = list_tasks(
            &pool,
            Some(pid),
            Some("todo"),
            None,
            false,
            None,
            false,
            None,
            None,
            None,
            Some("observes"),
            Some("widget-b"),
            false,
        )
        .await?;
        assert_eq!(
            open_b.as_array().unwrap().len(),
            0,
            "no OPEN task observes widget-b after it's done"
        );

        // A key with no matching value returns nothing; only meta_key (no value) does not filter.
        let none = list_tasks(
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
            Some("observes"),
            Some("nope"),
            false,
        )
        .await?;
        assert_eq!(none.as_array().unwrap().len(), 0);
        let unfiltered = list_tasks(
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
            Some("observes"),
            None,
            false,
        )
        .await?;
        assert_eq!(
            unfiltered.as_array().unwrap().len(),
            4,
            "meta_key without meta_value is inert"
        );
        Ok(())
    }

    /// Archiving hides a task from the default list_tasks view but keeps it fetchable by id and
    /// listable with include_archived; archiving is orthogonal to status; restore reverses it.
    #[tokio::test]
    async fn set_task_archived_hides_by_default_and_restores() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("owner"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let keep = create_task(
            &pool,
            pid,
            "keep",
            None,
            None,
            None,
            Some("owner"),
            None,
            None,
            None,
        )
        .await?;
        let retire = create_task(
            &pool,
            pid,
            "retire",
            None,
            None,
            None,
            Some("owner"),
            None,
            None,
            None,
        )
        .await?;
        let retire_id = retire["id"].as_i64().unwrap();
        let _ = keep;

        // Both visible before archiving.
        let before = list_tasks(
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
        assert_eq!(before.as_array().unwrap().len(), 2);

        // Archive one: the default view drops it, but include_archived still lists it.
        let archived = set_task_archived(&pool, retire_id, true, Some("owner")).await?;
        assert!(
            archived["archived_at"].is_string(),
            "archived_at stamped: {archived}"
        );
        let default_view = list_tasks(
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
        let titles: Vec<_> = default_view
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["title"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(titles, vec!["keep"], "archived task hidden by default");
        let with_archived = list_tasks(
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
            true,
        )
        .await?;
        assert_eq!(
            with_archived.as_array().unwrap().len(),
            2,
            "include_archived lists it"
        );
        // Still fetchable by id.
        assert_eq!(
            get_task(&pool, retire_id).await?["title"].as_str(),
            Some("retire")
        );

        // Restore: reappears in the default view, stamp cleared.
        let restored = set_task_archived(&pool, retire_id, false, Some("owner")).await?;
        assert!(
            restored["archived_at"].is_null(),
            "archived_at cleared: {restored}"
        );
        let after = list_tasks(
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
        assert_eq!(
            after.as_array().unwrap().len(),
            2,
            "restored task back in default view"
        );

        // Archiving an unknown task errors.
        assert!(set_task_archived(&pool, 999_999, true, None).await.is_err());
        Ok(())
    }

    /// The secret-request broker lifecycle (task 272): request → submit (single-use token) →
    /// fulfiller pulls ciphertext (token-gated) → fulfill deletes the row. Metadata reads never
    /// expose the ciphertext or the tokens; a spent or wrong token is rejected; the fulfiller is
    /// notified on submit; fulfilling a gone row is idempotent.
    #[tokio::test]
    async fn secret_request_broker_lifecycle() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        let recips = vec![
            "ssh-ed25519 AAAA operator".to_string(),
            "ssh-ed25519 BBBB green-machine".to_string(),
        ];
        let req = create_secret_request(
            &pool,
            "github-bridge.token.age",
            &recips,
            Some("GitHub PAT, repo+read:org scope"),
            Some("secrets/github-bridge.token.age"),
            Some("green-machine-ops"),
            Some("v-github-bridge"),
        )
        .await?;
        let id = req["id"].as_i64().unwrap();
        let submit_token = req["submit_token"].as_str().unwrap().to_string();
        let fulfiller_token = req["fulfiller_token"].as_str().unwrap().to_string();
        assert!(req["submit_url"]
            .as_str()
            .unwrap()
            .contains(&format!("/secret-requests/{id}?t=")));
        assert_eq!(req["status"], "requested");
        assert_eq!(req["recipients"].as_array().unwrap().len(), 2);

        // A metadata read never carries the ciphertext or the tokens.
        let meta = get_secret_request(&pool, id).await?;
        assert!(meta.get("ciphertext").is_none());
        assert!(meta.get("submit_token").is_none());
        assert!(meta.get("fulfiller_token").is_none());

        // A wrong submit token is rejected.
        assert!(submit_secret(&pool, id, "wrong-token", "CT").await.is_err());

        // Submit with the right token flips to submitted, hides the ciphertext, notifies the fulfiller.
        let submitted = submit_secret(&pool, id, &submit_token, "AGE-CIPHERTEXT-BLOB").await?;
        assert_eq!(submitted["status"], "submitted");
        assert!(
            submitted.get("ciphertext").is_none(),
            "submit metadata hides ciphertext"
        );
        let notif = check_notifications(&pool, "green-machine-ops", true, 50, None).await?;
        let types: Vec<String> = notif["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap().to_string())
            .collect();
        assert!(
            types.contains(&"secret.submitted".to_string()),
            "fulfiller notified: {types:?}"
        );

        // The submit link is single-use: a second submit is rejected.
        assert!(submit_secret(&pool, id, &submit_token, "AGAIN")
            .await
            .is_err());

        // The fulfiller pulls the ciphertext (token-gated); a wrong token is rejected.
        assert!(get_secret_ciphertext(&pool, id, "wrong-token")
            .await
            .is_err());
        let pulled = get_secret_ciphertext(&pool, id, &fulfiller_token).await?;
        assert_eq!(pulled["ciphertext"], "AGE-CIPHERTEXT-BLOB");

        // Fulfill deletes the row; a second fulfill (row gone) is idempotent.
        let done = fulfill_secret(&pool, id, &fulfiller_token).await?;
        assert_eq!(done["fulfilled"], true);
        assert!(
            get_secret_request(&pool, id).await.is_err(),
            "row deleted after fulfill"
        );
        let again = fulfill_secret(&pool, id, "any").await?;
        assert_eq!(again["already"], true, "idempotent fulfill on a gone row");

        Ok(())
    }

    /// The banned-phrases list + scanner (task #308): add/list/remove round-trips, the scan is
    /// case-insensitive and whole-phrase (not a substring of a larger word), and check_banned_phrases
    /// bails on a hit unless acknowledged.
    #[tokio::test]
    async fn banned_phrases_list_and_scan() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        // Empty list: nothing matches, and check passes.
        assert!(scan_banned_phrases(&pool, "anything at all")
            .await?
            .is_empty());
        check_banned_phrases(&pool, "anything at all", false).await?;

        // Add two phrases (stored lowercased; idempotent + note-updating on re-add).
        add_banned_phrase(&pool, "The Floor", Some("jargon"), Some("librarian")).await?;
        add_banned_phrase(&pool, "floored", None, Some("librarian")).await?;
        add_banned_phrase(&pool, "the floor", Some("still jargon"), Some("librarian")).await?; // dup -> update
        let list = list_banned_phrases(&pool).await?;
        assert_eq!(
            list.as_array().unwrap().len(),
            2,
            "dup add did not grow the list: {list}"
        );

        // Case-insensitive, whole-phrase match; the term inside a larger word does NOT match.
        assert_eq!(
            scan_banned_phrases(&pool, "we hit THE FLOOR today").await?,
            vec!["the floor"]
        );
        assert_eq!(
            scan_banned_phrases(&pool, "I am floored.").await?,
            vec!["floored"]
        );
        assert!(
            scan_banned_phrases(&pool, "the floorboard creaks")
                .await?
                .is_empty(),
            "whole-word only"
        );
        assert!(scan_banned_phrases(&pool, "no jargon here")
            .await?
            .is_empty());

        // check bails on a hit, unless acknowledged.
        let err = check_banned_phrases(&pool, "down to the floor", false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("banned phrase"), "got: {err}");
        assert!(err.contains("the floor"), "names the phrase: {err}");
        check_banned_phrases(&pool, "down to the floor", true).await?; // acknowledged -> passes

        // Remove one; it stops matching and the list shrinks.
        let r = remove_banned_phrase(&pool, "THE FLOOR").await?;
        assert_eq!(r["deleted"], json!(true));
        assert!(scan_banned_phrases(&pool, "we hit the floor")
            .await?
            .is_empty());
        assert_eq!(
            list_banned_phrases(&pool).await?.as_array().unwrap().len(),
            1
        );
        assert_eq!(
            remove_banned_phrase(&pool, "the floor").await?["deleted"],
            json!(false),
            "already gone"
        );
        Ok(())
    }

    /// The non-ASCII FORMAT check (task 368): ASCII passes; an em dash / curly quote / arrow /
    /// emoji is rejected with a located "non-ASCII" message; line:column tracks newlines; and
    /// acknowledge is the escape hatch.
    #[test]
    fn non_ascii_format_check() {
        assert!(check_non_ascii("plain ascii - straight \"quotes\" ok", false).is_ok());
        for bad in [
            "an em dash \u{2014} here",
            "curly \u{201c}quote\u{201d}",
            "arrow \u{2194} x",
            "emoji \u{1F600}",
        ] {
            let e = check_non_ascii(bad, false).unwrap_err().to_string();
            assert!(e.starts_with("non-ASCII"), "starts with non-ASCII: {e}");
            assert!(e.contains("line 1, column"), "reports a location: {e}");
        }
        // Location counts newlines.
        let e = check_non_ascii("line one\nsecond \u{2014} dash", false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("line 2"), "counts newlines: {e}");
        // Acknowledge overrides.
        assert!(check_non_ascii("em dash \u{2014} acked", true).is_ok());
    }

    /// A document title is in scope for the ASCII-only ruling: create_document and update_document
    /// reject a non-ASCII title (hard rule, no acknowledge), so new non-ASCII titles can't enter.
    #[tokio::test]
    async fn document_title_must_be_ascii() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        // Create with a non-ASCII (em dash) title is rejected before any DB write.
        let e = create_document(
            &pool,
            "Bad \u{2014} title",
            None,
            "Qmcid",
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            e.starts_with("non-ASCII"),
            "create rejects non-ASCII title: {e}"
        );
        // An ASCII create succeeds; renaming to a non-ASCII title is rejected; ASCII rename is fine.
        let d = create_document(
            &pool,
            "Good title",
            None,
            "Qmcid",
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        let id = d["id"].as_i64().unwrap();
        assert!(
            update_document(&pool, id, "Renamed \u{2194} bad", Some("u"))
                .await
                .is_err()
        );
        update_document(&pool, id, "Renamed good", Some("u")).await?;
        Ok(())
    }

    /// An @mention in a task comment auto-subscribes that REGISTERED agent to the task (so the
    /// wake model notifies them), while an unregistered @token is ignored.
    #[tokio::test]
    async fn comment_at_mention_subscribes_registered_agent() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "alice", None, None, None, None, None).await?;
        let p = create_project(&pool, "P", None, Some("owner"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            pid,
            "T",
            None,
            None,
            None,
            Some("owner"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        // owner comments mentioning @alice (registered) and @nobody (not registered).
        comment_task(
            &pool,
            tid,
            "hey @alice and @nobody take a look",
            Some("owner"),
            None,
            None,
        )
        .await?;
        let task = get_task(&pool, tid).await?;
        let subs: Vec<&str> = task["subscribers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            subs.contains(&"alice"),
            "mentioned registered agent subscribed: {subs:?}"
        );
        assert!(
            !subs.contains(&"nobody"),
            "unregistered @token ignored: {subs:?}"
        );
        Ok(())
    }

    #[test]
    fn extract_mentions_parses_at_tokens() {
        assert_eq!(
            extract_mentions("hi @v-task-board and @board-pm, cc @alice_1"),
            vec!["v-task-board", "board-pm", "alice_1"]
        );
        assert!(extract_mentions("no mentions here").is_empty());
        assert_eq!(
            extract_mentions("email a@b.com is not a mention start"),
            vec!["b"]
        );
    }

    /// #476: register_agent/update_agent coerce a hand-authored metadata.repos (a CSV/space/newline
    /// string, or a list of bare names) into the structured [{"repo": name}] form fleet spin-up
    /// expects; an already-structured list is left as-is; other metadata keys are untouched.
    #[test]
    fn coerce_repos_metadata_normalizes_unstructured_forms() {
        assert_eq!(
            coerce_repos_metadata(Some(&json!("Membrain, MembrainCDK\nElasticShuffleCDK"))),
            Some(
                json!([{ "repo": "Membrain" }, { "repo": "MembrainCDK" }, { "repo": "ElasticShuffleCDK" }])
            )
        );
        assert_eq!(
            coerce_repos_metadata(Some(&json!(["a", "b"]))),
            Some(json!([{ "repo": "a" }, { "repo": "b" }]))
        );
        // Already structured, absent, or an unrelated type => no change (None).
        assert_eq!(coerce_repos_metadata(Some(&json!([{ "repo": "a" }]))), None);
        assert_eq!(coerce_repos_metadata(None), None);
        assert_eq!(coerce_repos_metadata(Some(&json!(42))), None);
        // merge_metadata applies the coercion on write and leaves other keys intact.
        let out = merge_metadata(Some(r#"{"role":"x"}"#), json!({ "repos": "a b" }));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["repos"], json!([{ "repo": "a" }, { "repo": "b" }]));
        assert_eq!(v["role"], json!("x"));
    }

    /// Enabling a channel's auto_join backfills every registered agent as a member, a later-
    /// registered agent auto-joins on register, and disabling stops future auto-joins.
    #[tokio::test]
    async fn channel_auto_join_backfills_and_new_agents_join() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "a", None, None, None, None, None).await?;
        register_agent(&pool, "b", None, None, None, None, None).await?;
        let ch = create_channel(&pool, "announcements", None, Some("owner"), None).await?;
        let cid = ch["id"].as_i64().unwrap();
        let members = |v: &Value| -> Vec<String> {
            v["members"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap().to_string())
                .collect()
        };

        // Before enabling, a and b (registered before the channel existed) are not members.
        assert!(!members(&get_channel(&pool, cid).await?).contains(&"a".to_string()));

        // Enable: backfill joins every registered agent.
        set_channel_auto_join(&pool, cid, true, Some("owner")).await?;
        let m = members(&get_channel(&pool, cid).await?);
        assert!(
            m.contains(&"a".to_string()) && m.contains(&"b".to_string()),
            "backfilled: {m:?}"
        );

        // A newly-registered agent auto-joins.
        register_agent(&pool, "c", None, None, None, None, None).await?;
        assert!(
            members(&get_channel(&pool, cid).await?).contains(&"c".to_string()),
            "c auto-joined"
        );

        // Disabling stops future auto-joins (existing members stay).
        set_channel_auto_join(&pool, cid, false, Some("owner")).await?;
        register_agent(&pool, "d", None, None, None, None, None).await?;
        let m2 = members(&get_channel(&pool, cid).await?);
        assert!(
            !m2.contains(&"d".to_string()),
            "d did not auto-join after disable: {m2:?}"
        );
        assert!(m2.contains(&"c".to_string()), "existing member c retained");
        Ok(())
    }

    /// Clearing an assignee (assignee="") unsets the owner and emits task.unassigned carrying the
    /// prior owner; setting an owner emits task.assigned; clearing an already-unowned task is a
    /// no-op for the event.
    #[tokio::test]
    async fn update_task_unassign_emits_task_unassigned() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
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

        // Clear the owner.
        let cleared = update_task(
            &pool,
            tid,
            None,
            Some(""),
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        assert!(
            cleared["assignee"].is_null(),
            "assignee should be NULL after unassign"
        );

        let events = get_events(&pool, 0, 100, None, false).await?;
        let un = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == json!("task.unassigned"))
            .expect("task.unassigned emitted");
        assert_eq!(
            un["data"]["from"],
            json!("alice"),
            "carries the prior owner"
        );

        // Clearing an already-unassigned task does NOT emit a second task.unassigned.
        update_task(
            &pool,
            tid,
            None,
            Some(""),
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        let after = get_events(&pool, 0, 200, None, false).await?;
        assert_eq!(
            after
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["type"] == json!("task.unassigned"))
                .count(),
            1,
            "no second task.unassigned for a no-op clear"
        );

        // Re-assigning to a real owner emits task.assigned.
        update_task(
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
        let evs = get_events(&pool, 0, 200, None, false).await?;
        assert!(
            evs.as_array()
                .unwrap()
                .iter()
                .any(|e| e["type"] == json!("task.assigned")
                    && e["data"]["assignee"] == json!("bob")),
            "task.assigned emitted for the new owner"
        );
        Ok(())
    }

    /// move_task onto a non-existent project is rejected.
    #[tokio::test]
    async fn move_task_to_missing_project_fails() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let a = create_project(&pool, "A", None, Some("u"), None).await?;
        let aid = a["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            aid,
            "T",
            None,
            None,
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        assert!(move_task(&pool, tid, 9999, Some("u")).await.is_err());
        Ok(())
    }

    /// A legacy board.db predating projects.metadata opens cleanly: init() back-fills the
    /// column and project reads default it to {}.
    #[tokio::test]
    async fn opens_legacy_db_without_projects_metadata() -> anyhow::Result<()> {
        use sqlx::Row;
        use std::str::FromStr;
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("legacy.db");
        let url = format!("sqlite://{}", path.to_str().unwrap());

        // Pre-metadata projects table (no metadata column), plus a row.
        {
            let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)?.create_if_missing(true);
            let legacy = sqlx::SqlitePool::connect_with(opts).await?;
            sqlx::query(
                "CREATE TABLE projects (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, \
                 description TEXT, status TEXT NOT NULL DEFAULT 'active', created_by TEXT, \
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
            )
            .execute(&legacy)
            .await?;
            sqlx::query("INSERT INTO projects(name, created_at, updated_at) VALUES('old','t','t')")
                .execute(&legacy)
                .await?;
            legacy.close().await;
        }

        let pool = crate::db::init(path.to_str().unwrap()).await?;
        let has_meta = sqlx::query("PRAGMA table_info(projects)")
            .fetch_all(&pool)
            .await?
            .iter()
            .any(|r| r.get::<String, _>("name") == "metadata");
        assert!(has_meta, "migration should have added projects.metadata");

        let got = get_project(&pool, 1).await?;
        assert_eq!(got["name"], json!("old"));
        assert_eq!(got["metadata"], json!({}));
        // And a merge write works on the back-filled column.
        update_project(
            &pool,
            1,
            None,
            None,
            None,
            Some(json!({"repo": "r"})),
            Some("u"),
        )
        .await?;
        let after = get_project(&pool, 1).await?;
        assert_eq!(after["metadata"], json!({"repo": "r"}));
        Ok(())
    }

    /// A channel post fans out to every member's inbox except the poster, a fresh joiner can
    /// read the backlog via get_channel_posts, and create_channel is a case-insensitive
    /// get-or-create that auto-joins the creator.
    #[tokio::test]
    async fn channel_post_fans_out_to_members() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        // Creator auto-joins; a case-variant name returns the same channel (no duplicate).
        let c = create_channel(&pool, "General", Some("chat"), Some("alice"), None).await?;
        let cid = c["id"].as_i64().unwrap();
        assert_eq!(c["members"], json!(["alice"]));
        let again = create_channel(&pool, "general", None, Some("bob"), None).await?;
        assert_eq!(again["id"].as_i64(), Some(cid), "get-or-create by name");
        // bob joined via the get-or-create call.
        let members: BTreeSet<String> = again["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            members,
            ["alice", "bob"].iter().map(|s| s.to_string()).collect()
        );

        // carol joins explicitly, then alice posts. bob + carol hear it; alice (poster) doesn't.
        subscribe(&pool, "carol", None, None, Some(cid), None, false).await?;
        let posted = post_to_channel(&pool, cid, "alice", "hello all", None, None).await?;
        let post_seq = posted["seq"].as_i64().unwrap();

        let bob = check_notifications(&pool, "bob", true, 50, None).await?;
        let carol = check_notifications(&pool, "carol", true, 50, None).await?;
        let alice = check_notifications(&pool, "alice", true, 50, None).await?;
        assert_eq!(bob["count"].as_i64(), Some(1), "bob hears the post: {bob}");
        assert_eq!(
            carol["count"].as_i64(),
            Some(1),
            "carol hears the post: {carol}"
        );
        assert_eq!(
            alice["count"].as_i64(),
            Some(0),
            "poster isn't self-notified: {alice}"
        );
        assert_eq!(bob["notifications"][0]["type"], json!("channel.post"));

        // A fresh joiner reads history from the backlog (inbox only holds post-join events).
        let backlog = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        let posts = backlog.as_array().unwrap();
        assert_eq!(posts.len(), 1, "one post in history");
        assert_eq!(posts[0]["data"]["body"], json!("hello all"));

        // A threaded reply carries the parent seq.
        post_to_channel(&pool, cid, "bob", "hi alice", Some(post_seq), None).await?;
        let backlog = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        let posts = backlog.as_array().unwrap();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[1]["data"]["reply_to"].as_i64(), Some(post_seq));
        Ok(())
    }

    /// get_channel_posts read modes (task #315, mirroring get_events #266): ascending is oldest-
    /// first (scrollback); desc is newest-first so since_seq=0 + a limit yields the LATEST N (a chat
    /// view); before_seq pages earlier; and the seq bounds compose with either order.
    #[tokio::test]
    async fn get_channel_posts_desc_and_paging() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let c = create_channel(&pool, "log", None, Some("alice"), None).await?;
        let cid = c["id"].as_i64().unwrap();
        // Five posts, in order.
        let mut seqs = Vec::new();
        for i in 1..=5 {
            let p = post_to_channel(&pool, cid, "alice", &format!("m{i}"), None, None).await?;
            seqs.push(p["seq"].as_i64().unwrap());
        }
        let bodies = |v: &Value| {
            v.as_array()
                .unwrap()
                .iter()
                .map(|p| p["data"]["body"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };

        // Ascending (default): oldest-first.
        let asc = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        assert_eq!(bodies(&asc), vec!["m1", "m2", "m3", "m4", "m5"]);

        // Descending latest-N: the 2 most recent, newest-first (a plain ASC LIMIT 2 would give m1,m2).
        let latest2 = get_channel_posts(&pool, cid, 0, None, 2, true).await?;
        assert_eq!(bodies(&latest2), vec!["m5", "m4"]);

        // Load earlier: before_seq = the oldest seq shown (m4's), desc, limit 2 -> m3, m2.
        let earlier = get_channel_posts(&pool, cid, 0, Some(seqs[3]), 2, true).await?;
        assert_eq!(bodies(&earlier), vec!["m3", "m2"]);

        // The since_seq lower bound still composes (asc): posts strictly after m2.
        let after = get_channel_posts(&pool, cid, seqs[1], None, 100, false).await?;
        assert_eq!(bodies(&after), vec!["m3", "m4", "m5"]);
        Ok(())
    }

    /// A DM is a private 1:1 channel: send_message resolves the pair channel (same channel
    /// A→B and B→A), the recipient gets a message.direct in their inbox (unchanged wire
    /// behavior), the sender doesn't, and the private channel is hidden from the public list.
    #[tokio::test]
    async fn dm_is_a_private_channel() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        let m1 = send_message(&pool, "alice", "bob", "hey bob").await?;
        let cid = m1["channel_id"].as_i64().unwrap();
        // Reverse direction resolves to the SAME channel (canonical dm_key).
        let m2 = send_message(&pool, "bob", "alice", "hey alice").await?;
        assert_eq!(
            m2["channel_id"].as_i64(),
            Some(cid),
            "A->B and B->A share a channel"
        );

        // bob hears alice's message (1st DM), alice hears bob's (2nd) — each as message.direct,
        // never their own.
        let bob = check_notifications(&pool, "bob", true, 50, Some("message.direct")).await?;
        assert_eq!(bob["count"].as_i64(), Some(1), "bob: {bob}");
        assert_eq!(bob["notifications"][0]["data"]["body"], json!("hey bob"));
        let alice = get_messages(&pool, "alice", true, 50).await?;
        assert_eq!(alice["count"].as_i64(), Some(1), "alice: {alice}");
        assert_eq!(
            alice["notifications"][0]["data"]["body"],
            json!("hey alice")
        );

        // The DM channel is private: not in the public list, but visible to a member.
        let public = list_channels(&pool, None).await?;
        assert_eq!(
            public.as_array().unwrap().len(),
            0,
            "DM hidden from public list"
        );
        let alices = list_channels(&pool, Some("alice")).await?;
        assert_eq!(
            alices.as_array().unwrap().len(),
            1,
            "member sees their DM channel"
        );
        assert_eq!(alices[0]["private"], json!(true));

        // Full conversation is readable as a backlog on the shared channel.
        let posts = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        assert_eq!(posts.as_array().unwrap().len(), 2);
        Ok(())
    }

    /// invite_to_channel auto-joins the invitee and drops a channel.invite in their inbox
    /// (no accept step); they can unsubscribe to leave.
    #[tokio::test]
    async fn invite_auto_joins_and_notifies() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        let c = create_channel(&pool, "planning", None, Some("alice"), None).await?;
        let cid = c["id"].as_i64().unwrap();

        let after = invite_to_channel(&pool, cid, "bob", Some("alice")).await?;
        let members: BTreeSet<String> = after["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(members.contains("bob"), "invitee auto-joined");

        let bob = check_notifications(&pool, "bob", true, 50, Some("channel.invite")).await?;
        assert_eq!(bob["count"].as_i64(), Some(1), "bob got the invite: {bob}");
        assert_eq!(
            bob["notifications"][0]["data"]["invited_by"],
            json!("alice")
        );

        // Leaving = unsubscribe from the channel.
        unsubscribe(&pool, "bob", None, None, Some(cid), None, false).await?;
        let after = get_channel(&pool, cid).await?;
        let members: Vec<&str> = after["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(!members.contains(&"bob"), "unsubscribe leaves the channel");
        Ok(())
    }

    /// The channels feature opens a pre-channels DB in place: the events table gains a
    /// channel_id column and the channels table is created, so posting works on a legacy DB.
    #[tokio::test]
    async fn opens_legacy_db_without_channels() -> anyhow::Result<()> {
        use sqlx::Row;
        use std::str::FromStr;
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("legacy.db");
        let url = format!("sqlite://{}", path.to_str().unwrap());

        // A pre-channels events table (no channel_id column) with an existing row.
        {
            let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)?.create_if_missing(true);
            let legacy = sqlx::SqlitePool::connect_with(opts).await?;
            sqlx::query(
                "CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, type TEXT NOT NULL, \
                 actor TEXT, project_id INTEGER, task_id INTEGER, data TEXT, created_at TEXT NOT NULL)",
            )
            .execute(&legacy)
            .await?;
            sqlx::query("INSERT INTO events(type, created_at) VALUES('legacy.event','t')")
                .execute(&legacy)
                .await?;
            legacy.close().await;
        }

        // Real init adds events.channel_id and creates the channels table.
        let pool = crate::db::init(path.to_str().unwrap()).await?;
        let has_channel_col = sqlx::query("PRAGMA table_info(events)")
            .fetch_all(&pool)
            .await?
            .iter()
            .any(|r| r.get::<String, _>("name") == "channel_id");
        assert!(
            has_channel_col,
            "migration should have added events.channel_id"
        );

        // Channels work on the migrated DB: create, post, read back.
        let c = create_channel(&pool, "general", None, Some("alice"), None).await?;
        let cid = c["id"].as_i64().unwrap();
        post_to_channel(&pool, cid, "alice", "first post", None, None).await?;
        let posts = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        assert_eq!(posts.as_array().unwrap().len(), 1);
        assert_eq!(posts[0]["data"]["body"], json!("first post"));
        Ok(())
    }

    /// External-bridge core (#149 §4/§6): an external identity is upsertable + readable, and an
    /// ingested comment/post attributes to it while `author`/`from` stays the fleet ingester.
    #[tokio::test]
    async fn external_identity_and_attribution_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(
            &pool,
            "slack-bridge",
            Some("Slack Bridge"),
            None,
            None,
            None,
            None,
        )
        .await?;

        // Upsert an external identity, then re-upsert to refresh + merge metadata (idempotent).
        let e = upsert_external_identity(
            &pool,
            " slack:U123 ",
            "slack",
            Some("Ada"),
            Some(json!({"tz":"UTC"})),
        )
        .await?;
        assert_eq!(e["id"], json!("slack:U123"), "id is trimmed + stored");
        assert_eq!(e["source"], json!("slack"));
        assert_eq!(e["display_name"], json!("Ada"));
        assert_eq!(
            e["metadata"]["tz"],
            json!("UTC"),
            "metadata parsed to an object"
        );
        let e2 = upsert_external_identity(
            &pool,
            "slack:U123",
            "slack",
            None,
            Some(json!({"avatar":"x"})),
        )
        .await?;
        assert_eq!(
            e2["display_name"],
            json!("Ada"),
            "null display_name keeps the prior value"
        );
        assert_eq!(
            e2["metadata"]["tz"],
            json!("UTC"),
            "metadata is merged, not replaced"
        );
        assert_eq!(e2["metadata"]["avatar"], json!("x"));

        // list, filtered by source.
        assert_eq!(
            list_external_identities(&pool, Some("slack"))
                .await?
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_external_identities(&pool, Some("github"))
                .await?
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert!(get_external_identity(&pool, "slack:unknown")
            .await?
            .is_null());

        // A comment ingested by the bridge, attributed to the external human.
        let p = create_project(&pool, "P", None, Some("slack-bridge"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            pid,
            "T",
            None,
            None,
            None,
            Some("slack-bridge"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        comment_task(
            &pool,
            tid,
            "hi from slack",
            Some("slack-bridge"),
            Some("slack:U123"),
            None,
        )
        .await?;
        let task = get_task(&pool, tid).await?;
        let c0 = &task["comments"][0];
        assert_eq!(
            c0["author"],
            json!("slack-bridge"),
            "author is the fleet ingester"
        );
        assert_eq!(
            c0["external_author"],
            json!("slack:U123"),
            "attributed to the external human"
        );
        assert_eq!(
            c0["external_author_name"],
            json!("Ada"),
            "the identity's display_name is resolved on read (id stays the key)"
        );

        // A channel post carries the same attribution on its event data.
        let ch = create_channel(&pool, "bridge", None, Some("slack-bridge"), None).await?;
        let cid = ch["id"].as_i64().unwrap();
        post_to_channel(
            &pool,
            cid,
            "slack-bridge",
            "hello",
            None,
            Some("slack:U123"),
        )
        .await?;
        let posts = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        assert_eq!(posts[0]["data"]["from"], json!("slack-bridge"));
        assert_eq!(posts[0]["data"]["external_author"], json!("slack:U123"));
        assert_eq!(
            posts[0]["data"]["external_author_name"],
            json!("Ada"),
            "channel-post attribution resolves the display name on read too"
        );

        // An identity with no registered display_name: external_author stays, name is absent
        // (consumers fall back to the id — never a fabricated name).
        post_to_channel(
            &pool,
            cid,
            "slack-bridge",
            "who am i",
            None,
            Some("slack:UNKNOWN"),
        )
        .await?;
        let posts2 = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        let last = posts2.as_array().unwrap().last().unwrap();
        assert_eq!(last["data"]["external_author"], json!("slack:UNKNOWN"));
        assert!(
            last["data"].get("external_author_name").is_none(),
            "no display_name registered -> no external_author_name (fall back to the id)"
        );

        // Guardrails: empty id/source are client errors.
        assert!(upsert_external_identity(&pool, "  ", "slack", None, None)
            .await
            .is_err());
        assert!(
            upsert_external_identity(&pool, "slack:U9", "  ", None, None)
                .await
                .is_err()
        );
        Ok(())
    }

    /// The pure outbound reflect-back policy (#150 §5): default is board-internal; only
    /// direction out/both + an allowed author reflects out.
    #[test]
    fn outbound_reflect_policy() {
        // Default (unconfigured) + explicit "in" never reflect out.
        assert!(!reflects_out(&json!({}), "concierge"));
        assert!(!reflects_out(&json!({ "direction": "in" }), "concierge"));
        // direction out/both with no allowlist -> the documented ["concierge"] default.
        assert!(reflects_out(&json!({ "direction": "out" }), "concierge"));
        assert!(reflects_out(&json!({ "direction": "both" }), "concierge"));
        assert!(!reflects_out(&json!({ "direction": "out" }), "worker"));
        // An explicit allowlist replaces the default (and thus can EXCLUDE concierge).
        let p = json!({ "direction": "both", "outbound_authors": ["worker"] });
        assert!(reflects_out(&p, "worker"));
        assert!(!reflects_out(&p, "concierge"));
    }

    /// End-to-end (#150 gate): a post by an allowed author on an out-enabled channel emits a
    /// `channel.outbound_reflect` event; a denied author's post emits none. set_channel_props
    /// configures the policy on an existing channel.
    #[tokio::test]
    async fn outbound_reflect_event_emission() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "concierge", None, None, None, None, None).await?;
        register_agent(&pool, "worker", None, None, None, None, None).await?;

        let reflects = |events: &Value, cid: i64| -> Vec<Value> {
            events
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| {
                    e["type"] == json!("channel.outbound_reflect")
                        && e["data"]["channel_id"] == json!(cid)
                })
                .cloned()
                .collect()
        };

        // An out-enabled channel with the default allowlist (concierge).
        let ch = create_channel(
            &pool,
            "bridge-out",
            None,
            Some("concierge"),
            Some(json!({"direction":"both"})),
        )
        .await?;
        let cid = ch["id"].as_i64().unwrap();

        // Allowed author -> reflect event carrying the post details.
        post_to_channel(&pool, cid, "concierge", "to slack", None, None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        let r = reflects(&ev, cid);
        assert_eq!(r.len(), 1, "allowed author reflects out");
        assert_eq!(r[0]["data"]["author"], json!("concierge"));
        assert_eq!(r[0]["data"]["body"], json!("to slack"));
        assert!(r[0]["data"]["post_seq"].as_i64().is_some());

        // Denied author -> no new reflect event.
        post_to_channel(&pool, cid, "worker", "internal only", None, None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        assert_eq!(
            reflects(&ev, cid).len(),
            1,
            "denied author stays board-internal"
        );

        // An unconfigured channel never reflects, even for concierge.
        let plain = create_channel(&pool, "plain", None, Some("concierge"), None).await?;
        let pid = plain["id"].as_i64().unwrap();
        post_to_channel(&pool, pid, "concierge", "hi", None, None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        assert_eq!(
            reflects(&ev, pid).len(),
            0,
            "default policy is board-internal"
        );

        // set_channel_props turns reflect-back ON for the plain channel.
        set_channel_props(&pool, pid, json!({ "direction": "out" })).await?;
        post_to_channel(&pool, pid, "concierge", "now out", None, None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        assert_eq!(
            reflects(&ev, pid).len(),
            1,
            "policy configurable after creation"
        );
        Ok(())
    }

    /// Stateless bridge threading (#429): per-post metadata is stored on the post and surfaced on
    /// channel.outbound_reflect; a reply's reflect also carries the reply-parent's metadata as
    /// `parent_metadata`, so a bridge threads without keeping its own {post_seq -> ts} map.
    #[tokio::test]
    async fn post_metadata_and_parent_metadata_on_reflect() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "frank", None, None, None, None, None).await?;
        register_agent(&pool, "membrane-bridge", None, None, None, None, None).await?;

        // A bridged channel where frank reflects OUT.
        let ch = create_channel(
            &pool,
            "membrain-sync",
            None,
            Some("membrane-bridge"),
            Some(json!({ "direction": "both", "outbound_authors": ["frank"] })),
        )
        .await?;
        let cid = ch["id"].as_i64().unwrap();

        // Inbound relay: the bridge posts a human's Slack message with its thread metadata. The
        // bridge is not in outbound_authors, so this post does NOT reflect out.
        let human = post_to_channel_meta(
            &pool,
            cid,
            "membrane-bridge",
            "hello from a human",
            None,
            Some("slack:U1"),
            Some(json!({ "slack_ts": "1727.001", "slack_channel": "C0", "thread_ts": "1727.001" })),
        )
        .await?;
        let human_seq = human["seq"].as_i64().unwrap();

        // The metadata is stored on the post event (surfaced on reads).
        let ev = get_events(&pool, 0, 500, None, false).await?;
        let human_ev = ev
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["seq"] == json!(human_seq))
            .unwrap();
        assert_eq!(human_ev["data"]["metadata"]["thread_ts"], json!("1727.001"));

        // Frank replies on the board -> reflects OUT, and the reflect carries parent_metadata (the
        // human post's thread metadata) so the daemon threads statelessly.
        post_to_channel(&pool, cid, "frank", "Frank's reply", Some(human_seq), None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        let reflect = ev
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e["type"] == json!("channel.outbound_reflect")
                    && e["data"]["channel_id"] == json!(cid)
                    && e["data"]["author"] == json!("frank")
            })
            .expect("frank's reply reflects out");
        assert_eq!(reflect["data"]["reply_to"], json!(human_seq));
        assert_eq!(
            reflect["data"]["parent_metadata"]["thread_ts"],
            json!("1727.001"),
            "reflect carries the parent's thread_ts for stateless threading: {reflect}"
        );

        // A top-level reflected post carries its OWN metadata and no parent_metadata.
        post_to_channel_meta(
            &pool,
            cid,
            "frank",
            "top-level",
            None,
            None,
            Some(json!({ "k": "v" })),
        )
        .await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        let top = ev
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| {
                e["type"] == json!("channel.outbound_reflect")
                    && e["data"]["author"] == json!("frank")
            })
            .next_back()
            .unwrap();
        assert_eq!(top["data"]["metadata"]["k"], json!("v"));
        assert!(
            top["data"].get("parent_metadata").is_none(),
            "no parent_metadata without reply_to"
        );
        Ok(())
    }

    /// End-to-end (task 264 gate): a comment by an allowed author on a task linked OUT emits a
    /// `task.outbound_reflect` carrying the link's external target; a denied author (e.g. the
    /// ingesting bridge) emits none; an unlinked/unconfigured task never reflects; per-link authz
    /// is independent (two links on one task, only the out-enabled one fires).
    #[tokio::test]
    async fn task_outbound_reflect_event_emission() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "concierge", None, None, None, None, None).await?;
        register_agent(&pool, "gh-bridge", None, None, None, None, None).await?;
        let proj = create_project(&pool, "p", None, Some("concierge"), None).await?;
        let pid = proj["id"].as_i64().unwrap();

        let reflects = |events: &Value, tid: i64| -> Vec<Value> {
            events
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| {
                    e["type"] == json!("task.outbound_reflect")
                        && e["data"]["task_id"] == json!(tid)
                })
                .cloned()
                .collect()
        };

        // A task linked to a GitHub issue, out-enabled with the default allowlist (concierge).
        let task = create_task(
            &pool,
            pid,
            "t",
            None,
            None,
            None,
            Some("concierge"),
            None,
            None,
            None,
        )
        .await?;
        let tid = task["id"].as_i64().unwrap();
        upsert_external_link(
            &pool,
            "github",
            "camshaft/task-board#42",
            Some("issue"),
            "task",
            tid,
            Some(json!({ "direction": "both" })),
        )
        .await?;

        // Allowed author -> one reflect event carrying the comment + external target.
        comment_task(&pool, tid, "reflect me", Some("concierge"), None, None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        let r = reflects(&ev, tid);
        assert_eq!(r.len(), 1, "allowed author reflects out");
        assert_eq!(r[0]["data"]["author"], json!("concierge"));
        assert_eq!(r[0]["data"]["body"], json!("reflect me"));
        assert_eq!(r[0]["data"]["source"], json!("github"));
        assert_eq!(r[0]["data"]["external_id"], json!("camshaft/task-board#42"));
        assert_eq!(r[0]["data"]["external_parent_id"], json!("issue"));
        assert!(r[0]["data"]["comment_id"].as_i64().is_some());

        // Denied author (the ingesting bridge, not in outbound_authors) -> no echo back out.
        comment_task(
            &pool,
            tid,
            "ingested from github",
            Some("gh-bridge"),
            Some("github:U9"),
            None,
        )
        .await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        assert_eq!(
            reflects(&ev, tid).len(),
            1,
            "ingested comment stays board-internal (loop-safe)"
        );

        // A task with no external link never reflects, even for an allowed author.
        let plain = create_task(
            &pool,
            pid,
            "unlinked",
            None,
            None,
            None,
            Some("concierge"),
            None,
            None,
            None,
        )
        .await?;
        let plain_id = plain["id"].as_i64().unwrap();
        comment_task(&pool, plain_id, "hi", Some("concierge"), None, None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        assert_eq!(
            reflects(&ev, plain_id).len(),
            0,
            "unlinked task is board-internal"
        );

        // Per-link independence: add a SECOND link that is inbound-only; a comment fires only the
        // out-enabled github link, not the inbound one.
        upsert_external_link(
            &pool,
            "gitlab",
            "grp/proj#7",
            None,
            "task",
            tid,
            Some(json!({ "direction": "in" })),
        )
        .await?;
        comment_task(&pool, tid, "second reflect", Some("concierge"), None, None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        let r = reflects(&ev, tid);
        assert_eq!(
            r.len(),
            2,
            "only the out-enabled link fires; inbound link stays internal"
        );
        assert!(
            r.iter().all(|e| e["data"]["source"] == json!("github")),
            "gitlab (in) never reflects"
        );
        Ok(())
    }

    /// Idempotent external ingest (task 270): create_task / comment_task with an `external_link`
    /// dedup on (source, external_id), so a retrying bridge adapter is exactly-once — no duplicate
    /// task or comment, and the existing entity is returned with `created:false`.
    #[tokio::test]
    async fn idempotent_ingest_dedups_on_external_link() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "gh", None, None, None, None, None).await?;
        let pid = create_project(&pool, "P", None, Some("gh"), None).await?["id"]
            .as_i64()
            .unwrap();
        let link = ExternalRef {
            source: "github".into(),
            external_id: "camshaft/x#1".into(),
            external_parent_id: Some("issue".into()),
        };

        // First ingest: creates the task + link, created:true.
        let a = create_task(
            &pool,
            pid,
            "issue 1",
            None,
            None,
            None,
            Some("gh"),
            None,
            None,
            Some(link.clone()),
        )
        .await?;
        assert_eq!(a["created"], json!(true));
        let tid = a["id"].as_i64().unwrap();

        // Retry with the SAME (source, external_id): returns the SAME task, created:false, no dup —
        // even though the retry passed a different title.
        let b = create_task(
            &pool,
            pid,
            "issue 1 RETRY",
            None,
            None,
            None,
            Some("gh"),
            None,
            None,
            Some(link.clone()),
        )
        .await?;
        assert_eq!(b["created"], json!(false));
        assert_eq!(b["id"].as_i64().unwrap(), tid, "same task, not a duplicate");
        assert_eq!(
            b["title"],
            json!("issue 1"),
            "existing task returned unchanged"
        );
        let tasks = list_tasks(
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
        assert_eq!(
            tasks.as_array().unwrap().len(),
            1,
            "exactly one task, no duplicate"
        );

        // Comment idempotency on the new board_kind='comment'.
        let clink = ExternalRef {
            source: "github".into(),
            external_id: "camshaft/x#1-c9".into(),
            external_parent_id: Some("camshaft/x#1".into()),
        };
        let c1 = comment_task(
            &pool,
            tid,
            "hi from github",
            Some("gh"),
            Some("github:U1"),
            Some(clink.clone()),
        )
        .await?;
        assert_eq!(c1["created"], json!(true));
        let cid = c1["comment_id"].as_i64().unwrap();
        let c2 = comment_task(
            &pool,
            tid,
            "hi from github RETRY",
            Some("gh"),
            Some("github:U1"),
            Some(clink.clone()),
        )
        .await?;
        assert_eq!(c2["created"], json!(false));
        assert_eq!(
            c2["comment_id"].as_i64().unwrap(),
            cid,
            "same comment, not a duplicate"
        );
        let got = get_task(&pool, tid).await?;
        assert_eq!(
            got["comments"].as_array().unwrap().len(),
            1,
            "exactly one comment, no duplicate"
        );

        // A create/comment WITHOUT an external_link is unaffected and reports created:true.
        let plain = create_task(
            &pool,
            pid,
            "manual",
            None,
            None,
            None,
            Some("gh"),
            None,
            None,
            None,
        )
        .await?;
        assert_eq!(plain["created"], json!(true));
        let pc = comment_task(&pool, tid, "manual comment", Some("gh"), None, None).await?;
        assert_eq!(pc["created"], json!(true));
        Ok(())
    }

    /// Workspace kinds (task 287): define/get/list/delete a named env-setup resource, with
    /// omitted setup_script/description preserved on update and `config` merged.
    #[tokio::test]
    async fn workspace_kind_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        // Define.
        let k = set_workspace_kind(
            &pool,
            "custom-env",
            Some("checkout && build"),
            Some(json!({ "cwd": "/w", "repo": "r" })),
            Some("a custom workspace"),
            Some("board-pm"),
        )
        .await?;
        assert_eq!(k["name"], json!("custom-env"));
        assert_eq!(k["setup_script"], json!("checkout && build"));
        assert_eq!(k["config"]["cwd"], json!("/w"));
        assert_eq!(k["description"], json!("a custom workspace"));
        assert_eq!(k["created_by"], json!("board-pm"));

        // Get (config parsed).
        let g = get_workspace_kind(&pool, "custom-env").await?;
        assert_eq!(g["config"]["repo"], json!("r"));

        // Update: omit script + description (kept), merge a new config key (cwd/repo kept).
        let u = set_workspace_kind(
            &pool,
            "custom-env",
            None,
            Some(json!({ "branch": "main" })),
            None,
            None,
        )
        .await?;
        assert_eq!(
            u["setup_script"],
            json!("checkout && build"),
            "omitted script kept"
        );
        assert_eq!(
            u["description"],
            json!("a custom workspace"),
            "omitted description kept"
        );
        assert_eq!(u["config"]["cwd"], json!("/w"), "prior config key kept");
        assert_eq!(
            u["config"]["branch"],
            json!("main"),
            "new config key merged in"
        );
        assert_eq!(u["created_by"], json!("board-pm"), "creator preserved");

        // List, unknown, delete (idempotent).
        assert_eq!(
            list_workspace_kinds(&pool).await?.as_array().unwrap().len(),
            1
        );
        assert!(get_workspace_kind(&pool, "nope").await?.is_null());
        assert_eq!(
            delete_workspace_kind(&pool, "custom-env").await?["deleted"],
            json!(true)
        );
        assert!(get_workspace_kind(&pool, "custom-env").await?.is_null());
        assert_eq!(
            delete_workspace_kind(&pool, "custom-env").await?["deleted"],
            json!(false)
        );
        Ok(())
    }

    /// promote_thread (#151 §7): a channel thread imports into a task — root→description,
    /// direct replies→comments preserving author/external-author + timestamps — and the
    /// thread↔task link makes it idempotent.
    #[tokio::test]
    async fn promote_thread_imports_and_is_idempotent() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "concierge", None, None, None, None, None).await?;
        register_agent(&pool, "slack-bridge", None, None, None, None, None).await?;
        let p = create_project(&pool, "P", None, Some("concierge"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let ch = create_channel(&pool, "planning", None, Some("concierge"), None).await?;
        let cid = ch["id"].as_i64().unwrap();

        // A thread: root + two direct replies (one attributed to an external human) + an
        // unrelated top-level post that must NOT be imported.
        let root = post_to_channel(
            &pool,
            cid,
            "concierge",
            "Root topic\nmore detail",
            None,
            None,
        )
        .await?;
        let root_seq = root["seq"].as_i64().unwrap();
        let r1 = post_to_channel(
            &pool,
            cid,
            "slack-bridge",
            "reply from ada",
            Some(root_seq),
            Some("slack:U1"),
        )
        .await?;
        let r1_seq = r1["seq"].as_i64().unwrap();
        post_to_channel(
            &pool,
            cid,
            "concierge",
            "second reply",
            Some(root_seq),
            None,
        )
        .await?;
        post_to_channel(&pool, cid, "concierge", "unrelated top-level", None, None).await?;

        let res = promote_thread(&pool, cid, root_seq, pid, Some("concierge")).await?;
        let tid = res["task_id"].as_i64().unwrap();
        assert_eq!(
            res["imported_comments"],
            json!(2),
            "only the two direct replies import"
        );
        assert_eq!(res["already_promoted"], json!(false));

        let task = get_task(&pool, tid).await?;
        assert_eq!(
            task["title"],
            json!("Root topic"),
            "title = root's first line"
        );
        assert_eq!(
            task["description"],
            json!("Root topic\nmore detail"),
            "root body -> description"
        );
        let comments = task["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0]["body"], json!("reply from ada"));
        assert_eq!(
            comments[0]["author"],
            json!("slack-bridge"),
            "ingester preserved"
        );
        assert_eq!(
            comments[0]["external_author"],
            json!("slack:U1"),
            "attribution preserved"
        );
        assert_eq!(
            comments[0]["origin_ref"],
            json!(r1_seq.to_string()),
            "origin id recorded for sync/dedup"
        );
        assert_eq!(comments[1]["body"], json!("second reply"));

        // Timestamp fidelity: the imported comment carries the original reply's created_at.
        let posts = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        let r1_created = posts
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["seq"].as_i64() == Some(r1_seq))
            .unwrap()["created_at"]
            .clone();
        assert_eq!(
            comments[0]["created_at"], r1_created,
            "reply timestamp preserved"
        );

        // Idempotent: re-promoting returns the same task, no re-import, no duplicate comments.
        let again = promote_thread(&pool, cid, root_seq, pid, Some("concierge")).await?;
        assert_eq!(again["task_id"], json!(tid));
        assert_eq!(again["already_promoted"], json!(true));
        assert_eq!(
            get_task(&pool, tid).await?["comments"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "no duplicate import"
        );

        // A missing root post is an error.
        assert!(promote_thread(&pool, cid, 999999, pid, Some("concierge"))
            .await
            .is_err());
        Ok(())
    }

    /// external_links (#149 slice 2): the generic bridged mapping — channel-map + a task link in
    /// ONE table, idempotent on (source, external_id), filterable by the adapter's read path.
    #[tokio::test]
    async fn external_link_channel_map_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let ch = create_channel(&pool, "planning", None, Some("concierge"), None).await?;
        let cid = ch["id"].as_i64().unwrap();
        let p = create_project(&pool, "P", None, Some("concierge"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            pid,
            "T",
            None,
            None,
            None,
            Some("concierge"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();

        // Channel-map link (board channel <-> Slack channel).
        let l = upsert_external_link(
            &pool,
            "slack",
            "C123",
            None,
            "channel",
            cid,
            Some(json!({"name":"#planning"})),
        )
        .await?;
        assert_eq!(l["source"], json!("slack"));
        assert_eq!(l["external_id"], json!("C123"));
        assert_eq!(l["board_kind"], json!("channel"));
        assert_eq!(l["board_id"], json!(cid));
        assert_eq!(l["metadata"]["name"], json!("#planning"));

        // Idempotent on (source, external_id): re-link updates parent + MERGES metadata.
        let l2 = upsert_external_link(
            &pool,
            "slack",
            "C123",
            Some("workspaceA"),
            "channel",
            cid,
            Some(json!({"topic":"x"})),
        )
        .await?;
        assert_eq!(l2["external_parent_id"], json!("workspaceA"));
        assert_eq!(
            l2["metadata"]["name"],
            json!("#planning"),
            "metadata merged, not replaced"
        );
        assert_eq!(l2["metadata"]["topic"], json!("x"));
        assert_eq!(
            list_external_links(&pool, Some("slack"), None, None)
                .await?
                .as_array()
                .unwrap()
                .len(),
            1,
            "still one link"
        );

        // The SAME table carries a task link from a different source.
        upsert_external_link(
            &pool,
            "github",
            "https://gh/issues/1",
            None,
            "task",
            tid,
            None,
        )
        .await?;
        assert_eq!(
            list_external_links(&pool, None, None, None)
                .await?
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            list_external_links(&pool, None, Some("channel"), Some(cid))
                .await?
                .as_array()
                .unwrap()
                .len(),
            1,
            "adapter resolves board->external"
        );
        assert_eq!(
            list_external_links(&pool, None, Some("task"), None)
                .await?
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_external_links(&pool, Some("github"), None, None)
                .await?
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // Guards: bad kind, missing board entity, empty source/external_id.
        assert!(
            upsert_external_link(&pool, "slack", "C9", None, "widget", cid, None)
                .await
                .is_err()
        );
        assert!(
            upsert_external_link(&pool, "slack", "C9", None, "channel", 999999, None)
                .await
                .is_err()
        );
        assert!(
            upsert_external_link(&pool, "  ", "C9", None, "channel", cid, None)
                .await
                .is_err()
        );
        assert!(
            upsert_external_link(&pool, "slack", "  ", None, "channel", cid, None)
                .await
                .is_err()
        );

        // "document" kind (task 578 chorus board-side): attach a chorus URL to a doc and read the
        // whole sync set inline via list_external_links(source, board_kind), no get_document per doc.
        let doc = create_document(
            &pool,
            "Spec",
            None,
            "bafycid",
            None,
            Some("u"),
            None,
            None,
            None,
        )
        .await?;
        let doc_id = doc["id"].as_i64().unwrap();
        upsert_external_link(
            &pool,
            "chorus",
            "chorus-doc-7",
            None,
            "document",
            doc_id,
            Some(json!({ "url": "https://chorus.example/d/7" })),
        )
        .await?;
        let synced = list_external_links(&pool, Some("chorus"), Some("document"), None).await?;
        let arr = synced.as_array().unwrap();
        assert_eq!(arr.len(), 1, "the chorus sync set is one call: {synced}");
        assert_eq!(arr[0]["board_id"], json!(doc_id));
        assert_eq!(arr[0]["external_id"], json!("chorus-doc-7"));
        assert_eq!(
            arr[0]["metadata"]["url"],
            json!("https://chorus.example/d/7"),
            "url inline"
        );
        // A nonexistent document target is rejected (the existence-check arm).
        assert!(
            upsert_external_link(
                &pool,
                "chorus",
                "chorus-doc-x",
                None,
                "document",
                999999,
                None
            )
            .await
            .is_err(),
            "unknown document rejected"
        );
        Ok(())
    }

    /// #151 slice 2: live bidirectional thread<->task sync. A new thread reply mirrors to a task
    /// comment and a new task comment mirrors to a thread reply — each exactly once, no echo.
    #[tokio::test]
    async fn thread_task_live_sync() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "concierge", None, None, None, None, None).await?;
        register_agent(&pool, "worker", None, None, None, None, None).await?;
        let p = create_project(&pool, "P", None, Some("concierge"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let ch = create_channel(&pool, "planning", None, Some("concierge"), None).await?;
        let cid = ch["id"].as_i64().unwrap();

        // Promote a (reply-less) thread -> task, then wire lives.
        let root = post_to_channel(&pool, cid, "concierge", "root topic", None, None).await?;
        let root_seq = root["seq"].as_i64().unwrap();
        let tid = promote_thread(&pool, cid, root_seq, pid, Some("concierge")).await?["task_id"]
            .as_i64()
            .unwrap();
        assert_eq!(
            get_task(&pool, tid).await?["comments"]
                .as_array()
                .unwrap()
                .len(),
            0
        );

        // Direction 1: a new thread reply -> a task comment (attribution + origin preserved).
        let r1 = post_to_channel(
            &pool,
            cid,
            "slack-bridge",
            "reply from ada",
            Some(root_seq),
            Some("slack:U1"),
        )
        .await?;
        let r1_seq = r1["seq"].as_i64().unwrap();
        let comments = get_task(&pool, tid).await?["comments"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(comments.len(), 1, "thread reply mirrored to a task comment");
        assert_eq!(comments[0]["body"], json!("reply from ada"));
        assert_eq!(comments[0]["author"], json!("slack-bridge"));
        assert_eq!(comments[0]["external_author"], json!("slack:U1"));
        assert_eq!(comments[0]["origin_ref"], json!(r1_seq.to_string()));
        // No echo: the mirrored comment did NOT create another thread post.
        assert_eq!(
            get_channel_posts(&pool, cid, 0, None, 100, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            2,
            "root + r1 only"
        );

        // Direction 2: a new task comment -> a thread reply.
        comment_task(&pool, tid, "reply from board", Some("worker"), None, None).await?;
        let posts = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        let posts = posts.as_array().unwrap();
        assert_eq!(posts.len(), 3, "root + r1 + the mirrored comment");
        let mirrored = posts
            .iter()
            .find(|p| p["data"]["origin_comment"].is_i64())
            .unwrap();
        assert_eq!(mirrored["data"]["reply_to"], json!(root_seq));
        assert_eq!(mirrored["data"]["from"], json!("worker"));
        assert_eq!(mirrored["data"]["body"], json!("reply from board"));
        // No echo: the mirrored post did NOT create another task comment (still r1-mirror + worker's).
        assert_eq!(
            get_task(&pool, tid).await?["comments"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "no echo comment"
        );

        // Safety: a comment on a NON-linked task posts nothing to the channel.
        let solo = create_task(
            &pool,
            pid,
            "solo",
            None,
            None,
            None,
            Some("worker"),
            None,
            None,
            None,
        )
        .await?["id"]
            .as_i64()
            .unwrap();
        comment_task(&pool, solo, "unrelated", Some("worker"), None, None).await?;
        assert_eq!(
            get_channel_posts(&pool, cid, 0, None, 100, false)
                .await?
                .as_array()
                .unwrap()
                .len(),
            3,
            "unlinked task doesn't post"
        );
        Ok(())
    }

    /// #145: commenting on a task notifies every SUBSCRIBER (not just assignee/creator), excludes
    /// the comment's author, and auto-subscribes the commenter — and each task.commented carries
    /// `task_id`, which is what the webhook/tunnel wake payload uses to drive the agent loop's
    /// `[notification] task #<id>` wake (the reactive path; the wake fan-out itself lives in
    /// events::emit -> try_wake / fire_webhooks and is exercised by the tunnel integration test).
    #[tokio::test]
    async fn comment_notifies_subscribers_for_wake() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "creator", None, None, None, None, None).await?;
        register_agent(&pool, "watcher", None, None, None, None, None).await?;
        register_agent(&pool, "op1", None, None, None, None, None).await?;
        let pid = create_project(&pool, "P", None, Some("creator"), None).await?["id"]
            .as_i64()
            .unwrap();
        // Task with NO assignee, so `watcher` is a PURE subscriber (not assignee/creator).
        let tid = create_task(
            &pool,
            pid,
            "T",
            None,
            None,
            None,
            Some("creator"),
            None,
            None,
            None,
        )
        .await?["id"]
            .as_i64()
            .unwrap();
        subscribe(&pool, "watcher", Some(tid), None, None, None, false).await?;

        // op1 comments -> the pure subscriber hears it; the author (op1) does not hear its own.
        comment_task(&pool, tid, "first", Some("op1"), None, None).await?;
        let w = check_notifications(&pool, "watcher", true, 50, None).await?;
        assert_eq!(
            w["count"].as_i64(),
            Some(1),
            "pure subscriber notified: {w}"
        );
        assert_eq!(w["notifications"][0]["type"], json!("task.commented"));
        assert_eq!(
            w["notifications"][0]["task_id"],
            json!(tid),
            "wake payload carries task_id"
        );
        assert_eq!(
            check_notifications(&pool, "op1", true, 50, None).await?["count"].as_i64(),
            Some(0),
            "author not notified of own comment"
        );

        // Commenting auto-subscribed op1, so it hears a subsequent comment by someone else.
        comment_task(&pool, tid, "second", Some("watcher"), None, None).await?;
        let o = check_notifications(&pool, "op1", true, 50, None).await?;
        assert_eq!(
            o["count"].as_i64(),
            Some(1),
            "commenter auto-subscribed, hears later comments: {o}"
        );
        assert_eq!(o["notifications"][0]["type"], json!("task.commented"));

        // Every task.commented event carries task_id (the wake/webhook payload's routing key).
        let events = get_events(&pool, 0, 500, None, false).await?;
        let commented: Vec<&Value> = events
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == json!("task.commented"))
            .collect();
        assert_eq!(commented.len(), 2);
        assert!(commented.iter().all(|e| e["task_id"] == json!(tid)));
        Ok(())
    }

    /// get_events(actor=…) returns only that actor's events — a complete per-agent activity feed
    /// (requested by the UI, task #124) without over-fetching + client-side filtering.
    #[tokio::test]
    async fn get_events_actor_filter() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "a", None, None, None, None, None).await?;
        register_agent(&pool, "b", None, None, None, None, None).await?;
        let pid = create_project(&pool, "P", None, Some("a"), None).await?["id"]
            .as_i64()
            .unwrap();
        let tid = create_task(
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
        .await?["id"]
            .as_i64()
            .unwrap();
        comment_task(&pool, tid, "hi", Some("b"), None, None).await?;

        let all = get_events(&pool, 0, 500, None, false).await?;
        assert!(
            all.as_array().unwrap().len() >= 3,
            "project.created + task.created + task.commented"
        );

        let by_a = get_events(&pool, 0, 500, Some("a"), false).await?;
        let by_a = by_a.as_array().unwrap();
        assert!(!by_a.is_empty());
        assert!(
            by_a.iter().all(|e| e["actor"] == json!("a")),
            "only actor a: {by_a:?}"
        );
        assert!(by_a.iter().any(|e| e["type"] == json!("task.created")));

        let by_b = get_events(&pool, 0, 500, Some("b"), false).await?;
        let by_b = by_b.as_array().unwrap();
        assert_eq!(by_b.len(), 1, "b only authored the comment");
        assert_eq!(by_b[0]["type"], json!("task.commented"));
        assert_eq!(by_b[0]["actor"], json!("b"));

        assert!(get_events(&pool, 0, 500, Some("nobody"), false)
            .await?
            .as_array()
            .unwrap()
            .is_empty());
        Ok(())
    }

    /// get_events(desc=true) returns the LATEST N events newest-first — the live-activity-feed
    /// mode (task 266). A plain ORDER BY seq LIMIT N returns the N OLDEST and never advances; the
    /// desc window must track the tail as new events arrive, and still honor the actor filter and
    /// the `seq>since_seq` lower bound.
    #[tokio::test]
    async fn get_events_desc_returns_latest_newest_first() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "a", None, None, None, None, None).await?;
        let pid = create_project(&pool, "P", None, Some("a"), None).await?["id"]
            .as_i64()
            .unwrap();
        // Generate a run of events (project.created + one task.created per create_task).
        let mut tids = Vec::new();
        for i in 0..6 {
            tids.push(
                create_task(
                    &pool,
                    pid,
                    &format!("T{i}"),
                    None,
                    None,
                    None,
                    Some("a"),
                    None,
                    None,
                    None,
                )
                .await?["id"]
                    .as_i64()
                    .unwrap(),
            );
        }

        let seq_of = |v: &Value| v["seq"].as_i64().unwrap();

        // desc=true, limit 3 -> the 3 highest seqs, strictly newest-first.
        let latest = get_events(&pool, 0, 3, None, true).await?;
        let latest = latest.as_array().unwrap().clone();
        assert_eq!(latest.len(), 3, "limit caps the window");
        assert!(
            seq_of(&latest[0]) > seq_of(&latest[1]) && seq_of(&latest[1]) > seq_of(&latest[2]),
            "newest-first: {latest:?}"
        );

        // It is the TAIL, not the head: the newest desc seq == the max seq in the full asc log,
        // and the oldest asc event (project.created) is NOT in the latest-3 window.
        let asc = get_events(&pool, 0, 500, None, false).await?;
        let asc = asc.as_array().unwrap();
        let max_seq = asc.iter().map(seq_of).max().unwrap();
        assert_eq!(
            seq_of(&latest[0]),
            max_seq,
            "desc head is the tail of history"
        );
        assert!(
            !latest.iter().any(|e| e["type"] == json!("project.created")),
            "oldest event excluded from latest-N"
        );

        // The feed advances: a new event becomes the new desc head.
        comment_task(&pool, tids[0], "newest", Some("a"), None, None).await?;
        let latest2 = get_events(&pool, 0, 3, None, true).await?;
        let latest2 = latest2.as_array().unwrap();
        assert_eq!(
            latest2[0]["type"],
            json!("task.commented"),
            "the just-added event leads"
        );
        assert!(
            seq_of(&latest2[0]) > max_seq,
            "advanced past the prior tail"
        );

        // `seq>since_seq` lower bound still applies in desc mode (latest N ABOVE a floor).
        let above = get_events(&pool, max_seq, 50, None, true).await?;
        let above = above.as_array().unwrap();
        assert!(
            above.iter().all(|e| seq_of(e) > max_seq),
            "floor honored in desc: {above:?}"
        );

        // Actor filter composes with desc.
        register_agent(&pool, "z", None, None, None, None, None).await?;
        let by_z = get_events(&pool, 0, 10, Some("z"), true).await?;
        assert!(
            by_z.as_array().unwrap().is_empty(),
            "actor filter still applies"
        );
        Ok(())
    }

    /// #107: a document version records its content_type (MIME); create/publish accept it,
    /// defaulting to text/markdown, and reads expose it per version.
    #[tokio::test]
    async fn document_version_content_type() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        // v1 with an explicit non-markdown type.
        let d = create_document(
            &pool,
            "Diagram",
            None,
            "bafypng",
            None,
            Some("alice"),
            None,
            Some("image/png"),
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();
        assert_eq!(d["current_version"]["content_type"], json!("image/png"));

        // A later version can change the type; the authoritative type is per-version.
        let d2 = publish_version(
            &pool,
            did,
            "bafypdf",
            Some("as pdf"),
            Some("alice"),
            Some("application/pdf"),
            None,
        )
        .await?;
        assert_eq!(
            d2["current_version"]["content_type"],
            json!("application/pdf")
        );

        // Omitting content_type defaults to text/markdown (back-compat).
        let d3 = publish_version(&pool, did, "bafymd", None, Some("alice"), None, None).await?;
        assert_eq!(
            d3["current_version"]["content_type"],
            json!("text/markdown")
        );

        // get_document_versions surfaces content_type per version.
        let vers = get_document_versions(&pool, did).await?;
        let types: Vec<&str> = vers
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["content_type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            vec!["text/markdown", "application/pdf", "image/png"],
            "newest first"
        );

        // A default create_document (no content_type) is text/markdown, matching the initial docs work.
        let plain = create_document(
            &pool,
            "Notes",
            None,
            "bafymd2",
            None,
            Some("bob"),
            None,
            None,
            None,
        )
        .await?;
        assert_eq!(
            plain["current_version"]["content_type"],
            json!("text/markdown")
        );
        Ok(())
    }

    /// blocked_on (operator seq-1361): a blocked task must record what it waits on; kind=agent
    /// notifies that agent; the operator/agent views filter by kind/ref; leaving blocked clears it.
    #[tokio::test]
    async fn blocked_on_records_notifies_and_clears() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        register_agent(&pool, "agent:rev", None, None, None, None, None).await?;
        let t = create_task(
            &pool,
            pid,
            "Ship it",
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        let dep = create_task(
            &pool,
            pid,
            "Dependency",
            None,
            None,
            None,
            Some("bob"),
            None,
            None,
            None,
        )
        .await?;
        let dep_id = dep["id"].as_i64().unwrap();

        // Enforcement: blocked without a blocked_on is rejected (maps to 400 via the "give " prefix).
        let e = update_task(
            &pool,
            tid,
            Some("blocked"),
            None,
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            e.to_string().starts_with("give "),
            "blocked needs a blocked_on, got: {e}"
        );

        // Block on another TASK.
        update_task(
            &pool,
            tid,
            Some("blocked"),
            None,
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            Some(json!({"kind": "task", "target": dep_id.to_string(), "note": "waiting on dep"})),
        )
        .await?;
        let bo = get_task(&pool, tid).await?["blocked_on"].clone();
        assert_eq!(bo["kind"], json!("task"));
        assert_eq!(bo["target"], json!(dep_id.to_string()));
        assert_eq!(bo["note"], json!("waiting on dep"));

        // A nonexistent task target is rejected.
        assert!(update_task(
            &pool,
            tid,
            Some("blocked"),
            None,
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            Some(json!({"kind": "task", "target": "999999"}))
        )
        .await
        .is_err());

        // Re-block on an AGENT -> that agent is notified they're blocking.
        update_task(
            &pool,
            tid,
            Some("blocked"),
            None,
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            Some(json!({"kind": "agent", "target": "agent:rev", "note": "need review"})),
        )
        .await?;
        let notes = check_notifications(&pool, "agent:rev", true, 50, None).await?;
        let arr = notes["notifications"].as_array().unwrap();
        assert!(
            arr.iter()
                .any(|n| n["type"] == json!("task.blocked_on_you") && n["task_id"] == json!(tid)),
            "the blocking agent is notified: {notes}"
        );

        // "What is blocked on agent:rev" view.
        let on_agent = list_tasks(
            &pool,
            Some(pid),
            None,
            None,
            false,
            None,
            false,
            None,
            Some("agent"),
            Some("agent:rev"),
            None,
            None,
            false,
        )
        .await?;
        assert_eq!(on_agent.as_array().unwrap().len(), 1);

        // Block on the OPERATOR -> ref is null, and the operator view lists it.
        update_task(
            &pool,
            tid,
            Some("blocked"),
            None,
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            Some(json!({"kind": "operator"})),
        )
        .await?;
        let bo = get_task(&pool, tid).await?["blocked_on"].clone();
        assert_eq!(bo["kind"], json!("operator"));
        assert!(bo["target"].is_null());
        let on_op = list_tasks(
            &pool,
            None,
            None,
            None,
            false,
            None,
            false,
            None,
            Some("operator"),
            None,
            None,
            None,
            false,
        )
        .await?;
        assert!(on_op
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == json!(tid)));

        // Block on a TEAM -> every PRINCIPAL the team resolves to is notified: people AND agents
        // (team-scoped agents, task 542 Phase 1c). agent:qa is fresh (never individually blocked), so
        // its only blocked_on_you for this task can come from the team fan-out.
        create_person(&pool, "pat", Some("Pat"), Some("u"), None).await?;
        create_person(&pool, "sam", Some("Sam"), Some("u"), None).await?;
        register_agent(&pool, "agent:qa", None, None, None, None, None).await?;
        create_team(&pool, "reviewers", Some("Reviewers"), Some("u"), None).await?;
        add_team_member(&pool, "reviewers", "pat", "person", Some("u")).await?;
        add_team_member(&pool, "reviewers", "sam", "person", Some("u")).await?;
        add_team_member(&pool, "reviewers", "agent:qa", "agent", Some("u")).await?;
        update_task(
            &pool,
            tid,
            Some("blocked"),
            None,
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            Some(json!({"kind": "team", "target": "reviewers", "note": "need a review"})),
        )
        .await?;
        let bo = get_task(&pool, tid).await?["blocked_on"].clone();
        assert_eq!(bo["kind"], json!("team"));
        assert_eq!(bo["target"], json!("reviewers"));
        for who in ["pat", "sam", "agent:qa"] {
            let notes = check_notifications(&pool, who, true, 50, None).await?;
            assert!(
                notes["notifications"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(
                        |n| n["type"] == json!("task.blocked_on_you") && n["task_id"] == json!(tid)
                    ),
                "team member {who} (person or agent) is notified of the block: {notes}"
            );
        }
        // A nonexistent team target is rejected.
        assert!(
            update_task(
                &pool,
                tid,
                Some("blocked"),
                None,
                None,
                None,
                None,
                Some("alice"),
                None,
                None,
                Some(json!({"kind": "team", "target": "ghosts"}))
            )
            .await
            .is_err(),
            "unknown team rejected"
        );

        // Leaving blocked clears blocked_on.
        update_task(
            &pool,
            tid,
            Some("in_progress"),
            None,
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        assert!(
            get_task(&pool, tid).await?["blocked_on"].is_null(),
            "unblocking clears blocked_on"
        );
        Ok(())
    }

    // The parse_task_target normalizer (task 691): a blocked_on task target written as a bare id,
    // task_-prefixed, task:-prefixed, or #-prefixed all resolve to the numeric id; junk does not.
    #[test]
    fn parse_task_target_accepts_the_forms_agents_write() {
        assert_eq!(parse_task_target("611"), Some(611));
        assert_eq!(parse_task_target("task_611"), Some(611));
        assert_eq!(parse_task_target("task:611"), Some(611));
        assert_eq!(parse_task_target("task 611"), Some(611));
        assert_eq!(parse_task_target("#611"), Some(611));
        assert_eq!(parse_task_target("  611  "), Some(611));
        assert_eq!(parse_task_target("foo"), None);
        assert_eq!(parse_task_target(""), None);
    }

    // task 691: the blocked_on friction fixes end-to-end. (1) the missing-blocked_on error is
    // self-documenting -- it carries a copy-pasteable example; (2) a task target written as
    // `task_<id>` or `#<id>` (not just the bare id) is accepted.
    #[tokio::test]
    async fn blocked_on_error_is_self_documenting_and_target_forms_are_accepted(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            pid,
            "T",
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        let dep = create_task(
            &pool,
            pid,
            "Dep",
            None,
            None,
            None,
            Some("bob"),
            None,
            None,
            None,
        )
        .await?;
        let dep_id = dep["id"].as_i64().unwrap();

        // (1) The missing-blocked_on error advertises a working, copy-pasteable form.
        let e = update_task(
            &pool,
            tid,
            Some("blocked"),
            None,
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains("blocked_on"), "names the field: {e}");
        assert!(
            e.contains("task:611"),
            "shows a copy-pasteable example: {e}"
        );
        assert!(e.contains("none"), "says how to clear it: {e}");

        // (2) A task_-prefixed target resolves to the bare id.
        update_task(
            &pool,
            tid,
            Some("blocked"),
            None,
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            Some(json!({"kind": "task", "target": format!("task_{dep_id}")})),
        )
        .await?;
        assert_eq!(
            get_task(&pool, tid).await?["blocked_on"]["target"],
            json!(dep_id.to_string())
        );

        // ... and a #-prefixed target works too.
        update_task(
            &pool,
            tid,
            Some("blocked"),
            None,
            None,
            None,
            None,
            Some("alice"),
            None,
            None,
            Some(json!({"kind": "task", "target": format!("#{dep_id}")})),
        )
        .await?;
        assert_eq!(
            get_task(&pool, tid).await?["blocked_on"]["target"],
            json!(dep_id.to_string())
        );
        Ok(())
    }

    /// Concurrency guard for task 191 ("database is locked"). Many agents hammer the board's
    /// write paths at once — notably check_notifications, which reads then writes (mark-read +
    /// last_seen) in one transaction, the classic read->write upgrade that can deadlock under
    /// WAL. This spins up several workers sharing one pool (as the live server does) and asserts
    /// NONE of their writes surface a lock error. It's both the regression guard for the busy_
    /// timeout/synchronous mitigation and the measurement harness the operator asked for: if this
    /// ever fails with "database is locked", that's the signal to add BEGIN IMMEDIATE / retries.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_do_not_lock() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "Load", None, Some("op"), None).await?;
        let pid = p["id"].as_i64().unwrap();

        const WORKERS: usize = 12;
        const ROUNDS: usize = 6;
        let mut handles = Vec::new();
        for w in 0..WORKERS {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                let agent = format!("agent:w{w}");
                register_agent(&pool, &agent, None, None, None, None, None).await?;
                for r in 0..ROUNDS {
                    // A burst mixing the common write paths, incl. the read->write upgrade in
                    // check_notifications (subscribed to the task it just created).
                    let t = create_task(
                        &pool,
                        pid,
                        &format!("t{w}-{r}"),
                        None,
                        None,
                        None,
                        Some(&agent),
                        None,
                        None,
                        None,
                    )
                    .await?;
                    let tid = t["id"].as_i64().unwrap();
                    comment_task(&pool, tid, "working", Some(&agent), None, None).await?;
                    update_task(
                        &pool,
                        tid,
                        Some("in_progress"),
                        None,
                        None,
                        None,
                        None,
                        Some(&agent),
                        None,
                        None,
                        None,
                    )
                    .await?;
                    check_notifications(&pool, &agent, true, 50, None).await?;
                }
                Ok::<_, anyhow::Error>(())
            }));
        }
        for h in handles {
            // A panic or an Err (e.g. "database is locked") fails the test with the message.
            h.await.expect("worker task did not panic")?;
        }
        Ok(())
    }

    /// The review lifecycle: create -> in_review -> changes_requested -> in_review -> approved,
    /// each transition landing a state_change log entry and the right terminal/opened events, and
    /// findings/comments appended to the single log. Also covers the same-status no-op.
    #[tokio::test]
    async fn review_lifecycle_and_log() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "author", None, None, None, None, None).await?;
        register_agent(&pool, "reviewer", None, None, None, None, None).await?;

        let r = create_review(
            &pool,
            "design",
            Some("board-document"),
            Some("42"),
            Some("A design"),
            None,
            Some("author"),
            Some("reviewer"),
            None,
            None,
        )
        .await?;
        let rid = r["id"].as_i64().unwrap();
        assert_eq!(r["created"], json!(true));
        assert_eq!(r["status"], json!("open"));
        assert_eq!(r["vetted"], json!(false));
        // Initial 'submitted' log entry is present.
        assert_eq!(r["log"].as_array().unwrap().len(), 1);
        assert_eq!(r["log"][0]["entry_type"], json!("submitted"));

        // open -> in_review emits status_changed + opened_for_review (reviewer hears them).
        set_review_status(&pool, rid, "in_review", Some("author"), None).await?;
        // Same status again = idempotent no-op: no new log entry.
        let noop = set_review_status(&pool, rid, "in_review", Some("author"), None).await?;
        assert_eq!(noop["status"], json!("in_review"));
        assert_eq!(noop["log"].as_array().unwrap().len(), 2); // submitted + one state_change

        // A finding with a linked child task, plus a comment.
        let finding = append_review_log(
            &pool,
            rid,
            "finding",
            Some("null deref on empty input"),
            Some("reviewer"),
            None,
            Some("gh:owner/repo#c1"),
        )
        .await?;
        assert_eq!(finding["appended"], json!(true));
        // Replaying the same external_id is idempotent.
        let dup = append_review_log(
            &pool,
            rid,
            "finding",
            Some("null deref on empty input"),
            Some("reviewer"),
            None,
            Some("gh:owner/repo#c1"),
        )
        .await?;
        assert_eq!(dup["appended"], json!(false));
        assert_eq!(dup["entry_id"], finding["entry_id"]);

        set_review_status(
            &pool,
            rid,
            "changes_requested",
            Some("reviewer"),
            Some("fix the deref"),
        )
        .await?;
        set_review_status(&pool, rid, "in_review", Some("author"), None).await?;
        let approved = set_review_status(&pool, rid, "approved", Some("reviewer"), None).await?;
        assert_eq!(approved["status"], json!("approved"));

        // Full timeline: submitted + 4 real state_changes + 1 finding = 6 entries.
        let got = get_review(&pool, rid).await?;
        assert_eq!(got["log"].as_array().unwrap().len(), 6, "log: {got}");
        let finding_count = got["log"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["entry_type"] == json!("finding"))
            .count();
        assert_eq!(finding_count, 1);

        // An unknown status is rejected.
        assert!(set_review_status(&pool, rid, "bogus", Some("x"), None)
            .await
            .is_err());
        Ok(())
    }

    /// Idempotent external ingest: creating twice on the same external_link returns the SAME
    /// review with created:false, never a duplicate — the contract the github-bridge relies on.
    #[tokio::test]
    async fn review_external_ingest_is_idempotent() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let ext = ExternalRef {
            source: "github_pr".into(),
            external_id: "owner/repo#7".into(),
            external_parent_id: None,
        };
        let first = create_review(
            &pool,
            "code",
            Some("github-pull-request"),
            Some("owner/repo#7"),
            Some("PR 7"),
            Some("in_review"),
            Some("bridge"),
            None,
            None,
            Some(ext.clone()),
        )
        .await?;
        assert_eq!(first["created"], json!(true));
        assert_eq!(first["status"], json!("in_review"));
        let rid = first["id"].as_i64().unwrap();

        let second = create_review(
            &pool,
            "code",
            Some("github-pull-request"),
            Some("owner/repo#7"),
            Some("PR 7 again"),
            Some("open"),
            Some("bridge"),
            None,
            None,
            Some(ext),
        )
        .await?;
        assert_eq!(second["created"], json!(false));
        assert_eq!(
            second["id"].as_i64().unwrap(),
            rid,
            "same review, no duplicate"
        );
        assert_eq!(
            second["status"],
            json!("in_review"),
            "existing state preserved"
        );

        let all = list_reviews(&pool, None, None, None).await?;
        assert_eq!(all["reviews"].as_array().unwrap().len(), 1);
        Ok(())
    }

    /// A spin-down request is a signal, not an action: it records who/why, drops an
    /// agent.stand_down_requested into the target's inbox, does NOT change the agent's status, and
    /// clears when the agent honors it by going offline. Requesting for an unknown agent errors.
    #[tokio::test]
    async fn request_stand_down_signals_without_killing() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "worker", None, None, None, None, None).await?;
        set_status(&pool, "worker", "online", None).await?;

        let a = request_stand_down(
            &pool,
            "worker",
            Some("concierge"),
            Some("rebalancing the fleet"),
        )
        .await?;
        // Recorded, but status is untouched (never a kill / forced offline).
        assert_eq!(a["status"], json!("online"), "status must not change");
        assert_eq!(a["stand_down_requested_by"], json!("concierge"));
        assert_eq!(a["stand_down_reason"], json!("rebalancing the fleet"));
        assert!(a["stand_down_requested_at"].is_string());

        // The target observes it in its inbox.
        let inbox = check_notifications(&pool, "worker", true, 50, None).await?;
        let has_signal = inbox["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["type"] == json!("agent.stand_down_requested"));
        assert!(has_signal, "target must receive the signal: {inbox}");

        // Honoring it by going offline clears the request.
        let off = set_status(&pool, "worker", "offline", None).await?;
        assert_eq!(off["status"], json!("offline"));
        assert!(
            off["stand_down_requested_at"].is_null(),
            "cleared on offline"
        );
        assert!(off["stand_down_requested_by"].is_null());
        assert!(off["stand_down_reason"].is_null());

        // Unknown agent -> error (surfaces as a 404 at the API).
        assert!(request_stand_down(&pool, "ghost", Some("x"), None)
            .await
            .is_err());
        Ok(())
    }

    /// submit_to_operator_review is the single gated chokepoint before the operator sees a doc. It
    /// is fail-closed: rejected without a template attestation, without a conformance summary on the
    /// CURRENT version, or with an open actionable finding; it passes only when all three clear, and
    /// re-publishing a version re-closes the gate until a fresh summary pins the new version.
    #[tokio::test]
    async fn submit_to_operator_review_gate() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "Docs", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let d = create_document(
            &pool,
            "Design: Thing",
            Some(pid),
            "bafyv1",
            Some("v1"),
            Some("author"),
            None,
            None,
            None,
        )
        .await?;
        let did = d["id"].as_i64().unwrap();
        let dref = did.to_string();

        // No template attestation and no waiver -> rejected before any state is touched.
        assert!(
            submit_to_operator_review(&pool, did, Some("author"), None, None)
                .await
                .is_err()
        );
        assert!(
            submit_to_operator_review(&pool, did, Some("author"), Some("  "), Some(""))
                .await
                .is_err(),
            "whitespace-only attestations don't count"
        );

        // A conformance review exists over the doc, but no summary has run against the current
        // version yet -> rejected even with a template.
        let r = create_review(
            &pool,
            "design_conformance",
            Some("board_doc"),
            Some(dref.as_str()),
            Some("conformance"),
            None,
            Some("reviewer"),
            None,
            None,
            None,
        )
        .await?;
        let rid = r["id"].as_i64().unwrap();
        assert!(submit_to_operator_review(
            &pool,
            did,
            Some("author"),
            Some("design-doc-template"),
            None
        )
        .await
        .is_err());

        // Summary pins reviewed_version=1 (the current version), but there is an OPEN actionable
        // finding (its child task is not done) -> still rejected.
        append_review_log(
            &pool,
            rid,
            "adversarial_review",
            Some(&json!({ "reviewed_version": 1, "conformance": "pass" }).to_string()),
            Some("reviewer"),
            None,
            None,
        )
        .await?;
        let child = create_task(
            &pool,
            pid,
            "fix the thing",
            None,
            None,
            None,
            Some("reviewer"),
            None,
            None,
            None,
        )
        .await?;
        let child_id = child["id"].as_i64().unwrap();
        append_review_log(
            &pool,
            rid,
            "finding",
            Some("the thing is wrong"),
            Some("reviewer"),
            Some(child_id),
            None,
        )
        .await?;
        assert!(submit_to_operator_review(
            &pool,
            did,
            Some("author"),
            Some("design-doc-template"),
            None
        )
        .await
        .is_err());

        // Close the finding's child task -> all three conditions clear -> passes, status advances,
        // and the template attestation is stamped on the doc.
        update_task(
            &pool,
            child_id,
            Some("done"),
            None,
            None,
            None,
            None,
            Some("reviewer"),
            None,
            None,
            None,
        )
        .await?;
        let out = submit_to_operator_review(
            &pool,
            did,
            Some("author"),
            Some("design-doc-template"),
            None,
        )
        .await?;
        assert_eq!(out["status"], json!("operator_review"));
        assert_eq!(
            out["metadata"]["template_followed"],
            json!("design-doc-template")
        );

        // Version-pinning: publishing a new version re-closes the gate (the summary pinned v1, not
        // the current v2), even on the waiver path. A fresh summary on v2 reopens it.
        publish_version(&pool, did, "bafyv2", Some("v2"), Some("author"), None, None).await?;
        assert!(submit_to_operator_review(
            &pool,
            did,
            Some("author"),
            None,
            Some("no template fits an experiment note")
        )
        .await
        .is_err());
        append_review_log(
            &pool,
            rid,
            "adversarial_review",
            Some(&json!({ "reviewed_version": 2, "conformance": "pass" }).to_string()),
            Some("reviewer"),
            None,
            None,
        )
        .await?;
        let out2 = submit_to_operator_review(
            &pool,
            did,
            Some("author"),
            None,
            Some("no template fits an experiment note"),
        )
        .await?;
        assert_eq!(out2["status"], json!("operator_review"));
        assert!(out2["metadata"]["template_followed"].is_null());
        assert_eq!(
            out2["metadata"]["template_waiver_reason"],
            json!("no template fits an experiment note")
        );
        Ok(())
    }

    /// get_or_create_dm resolves the same private 1:1 channel for a pair regardless of order,
    /// creates it once (idempotent), lists both as members, and stays consistent with the channel
    /// send_message uses. A self-DM is rejected.
    #[tokio::test]
    async fn get_or_create_dm_is_idempotent_and_order_independent() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "alice", None, None, None, None, None).await?;
        register_agent(&pool, "bob", None, None, None, None, None).await?;

        let c1 = get_or_create_dm(&pool, "alice", "bob").await?;
        let cid1 = c1["id"].as_i64().unwrap();
        // Order-independent + idempotent: (bob, alice) resolves to the same channel.
        let c2 = get_or_create_dm(&pool, "bob", "alice").await?;
        assert_eq!(
            c2["id"].as_i64().unwrap(),
            cid1,
            "same DM channel either way"
        );

        // Both are members, and it's private.
        let members: BTreeSet<String> = c1["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            members,
            ["alice", "bob"].iter().map(|s| s.to_string()).collect()
        );
        assert_eq!(c1["private"], json!(true));

        // send_message reuses the exact same channel (no duplicate DM).
        let sent = send_message(&pool, "alice", "bob", "hi").await?;
        assert_eq!(sent["channel_id"].as_i64().unwrap(), cid1);

        // A self-DM is rejected (400 at the API).
        assert!(get_or_create_dm(&pool, "alice", "alice").await.is_err());
        Ok(())
    }

    /// The improvement trend is derived purely from the review logs on a seeded dataset: a fall in
    /// findings that hides a RISE in escaped defects is flagged, not counted as improvement; and
    /// post-approval findings, re-opens, and lineage follow-ups each register as escaped defects.
    #[tokio::test]
    async fn review_improvement_trend_from_log() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        // r1 (code, alpha): 3 findings, no escaped defects. Created first => earlier window.
        let r1 = create_review(
            &pool,
            "code",
            None,
            None,
            Some("r1"),
            None,
            Some("alpha"),
            None,
            None,
            None,
        )
        .await?;
        let r1id = r1["id"].as_i64().unwrap();
        for _ in 0..3 {
            append_review_log(&pool, r1id, "finding", Some("f"), Some("alpha"), None, None).await?;
        }

        // r2 (code, alpha): 1 pre-approval finding + 1 finding logged AFTER approval (escaped).
        let r2 = create_review(
            &pool,
            "code",
            None,
            None,
            Some("r2"),
            None,
            Some("alpha"),
            None,
            None,
            None,
        )
        .await?;
        let r2id = r2["id"].as_i64().unwrap();
        append_review_log(
            &pool,
            r2id,
            "finding",
            Some("pre"),
            Some("alpha"),
            None,
            None,
        )
        .await?;
        set_review_status(&pool, r2id, "in_review", Some("alpha"), None).await?;
        set_review_status(&pool, r2id, "approved", Some("alpha"), None).await?;
        tokio::time::sleep(std::time::Duration::from_millis(3)).await; // ensure a strictly later ts
        append_review_log(
            &pool,
            r2id,
            "finding",
            Some("escaped after approval"),
            Some("qa"),
            None,
            None,
        )
        .await?;

        // r3 (design, beta): a lineage follow-up (declares a predecessor) that still found something.
        let r3 = create_review(
            &pool,
            "design",
            None,
            None,
            Some("r3"),
            None,
            Some("beta"),
            None,
            Some(json!({ "predecessor_review_id": r1id })),
            None,
        )
        .await?;
        let r3id = r3["id"].as_i64().unwrap();
        append_review_log(
            &pool,
            r3id,
            "finding",
            Some("missed by predecessor"),
            Some("beta"),
            None,
            None,
        )
        .await?;

        // r4 (design, beta): a re-open (approved -> back to in_review), no findings.
        let r4 = create_review(
            &pool,
            "design",
            None,
            None,
            Some("r4"),
            None,
            Some("beta"),
            None,
            None,
            None,
        )
        .await?;
        let r4id = r4["id"].as_i64().unwrap();
        set_review_status(&pool, r4id, "in_review", Some("beta"), None).await?;
        set_review_status(&pool, r4id, "approved", Some("beta"), None).await?;
        set_review_status(
            &pool,
            r4id,
            "in_review",
            Some("beta"),
            Some("reopened for a regression"),
        )
        .await?;

        let t = review_improvement_trend(&pool, None, None).await?;
        assert_eq!(t["overall"]["reviews"].as_u64(), Some(4));

        // code slice: findings fell (3 -> 2) while escaped defects rose (0 -> 1) => FLAGGED.
        let by_kind = t["by_kind"].as_array().unwrap();
        let code = by_kind.iter().find(|s| s["kind"] == json!("code")).unwrap();
        assert_eq!(code["findings_trend"], json!("improving"));
        assert_eq!(code["escaped_trend"], json!("rising"));
        assert_eq!(code["flagged"], json!(true));
        assert_eq!(
            code["escaped_defects"]["post_approval_findings"].as_u64(),
            Some(1)
        );
        assert_eq!(code["escaped_defects"]["total"].as_u64(), Some(1));

        // design slice: a lineage follow-up and a re-open each register as an escaped defect.
        let design = by_kind
            .iter()
            .find(|s| s["kind"] == json!("design"))
            .unwrap();
        assert_eq!(
            design["escaped_defects"]["lineage_followups"].as_u64(),
            Some(1)
        );
        assert_eq!(design["escaped_defects"]["reopens"].as_u64(), Some(1));

        // Sliced by producing area too.
        let areas: BTreeSet<String> = t["by_area"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["area"].as_str().unwrap().to_string())
            .collect();
        assert!(
            areas.contains("alpha") && areas.contains("beta"),
            "areas: {areas:?}"
        );

        // Filters narrow the population.
        let code_only = review_improvement_trend(&pool, Some("code"), None).await?;
        assert_eq!(code_only["overall"]["reviews"].as_u64(), Some(2));
        assert_eq!(code_only["overall"]["flagged"], json!(true));
        let beta_only = review_improvement_trend(&pool, None, Some("beta")).await?;
        assert_eq!(beta_only["overall"]["reviews"].as_u64(), Some(2));
        assert_eq!(
            beta_only["overall"]["escaped_defects"]["reopens"].as_u64(),
            Some(1)
        );
        assert_eq!(
            beta_only["overall"]["escaped_defects"]["lineage_followups"].as_u64(),
            Some(1)
        );
        Ok(())
    }

    /// Mutation responses are trimmed so a looping caller doesn't re-ingest its own charter /
    /// a task's description on every tick (task #416): set_status returns presence fields only,
    /// and strip_field drops a heavy blob while leaving everything else intact.
    #[tokio::test]
    async fn mutation_responses_are_trimmed() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(
            &pool,
            "worker",
            Some("Worker"),
            None,
            Some("a very long charter ".repeat(200).trim()),
            None,
            None,
        )
        .await?;

        // set_status's core still returns the full agent; presence_projection (what the boundary
        // applies) keeps only presence fields and drops the charter.
        let full = set_status(&pool, "worker", "online", Some("ping")).await?;
        assert!(
            full["charter"].is_string(),
            "core still has the full object"
        );
        let presence = presence_projection(full);
        assert_eq!(presence["id"], json!("worker"));
        assert_eq!(presence["status"], json!("online"));
        assert_eq!(presence["status_message"], json!("ping"));
        assert!(presence.get("last_seen").is_some());
        assert!(
            presence.get("charter").is_none(),
            "presence response must omit the charter"
        );
        assert!(
            presence.get("metadata").is_none(),
            "presence response is presence-only"
        );

        // strip_field drops one blob, keeps the rest, and is a no-op on a non-object.
        let agent = get_agent(&pool, "worker").await?;
        let trimmed = strip_field(agent.clone(), "charter");
        assert!(trimmed.get("charter").is_none());
        assert_eq!(
            trimmed["display_name"],
            json!("Worker"),
            "other fields survive the strip"
        );
        assert_eq!(strip_field(json!("scalar"), "charter"), json!("scalar"));
        Ok(())
    }

    /// set_review_vetted flips the gate, durably logs WHO + from/to (a decision entry, per the D17
    /// audit-contract), emits review.vetted_changed, and is an idempotent no-op on the same value.
    #[tokio::test]
    async fn set_review_vetted_audits_and_is_idempotent() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "author", None, None, None, None, None).await?;
        register_agent(&pool, "gatekeeper", None, None, None, None, None).await?;

        let r = create_review(
            &pool,
            "design",
            None,
            None,
            Some("d"),
            None,
            Some("author"),
            None,
            None,
            None,
        )
        .await?;
        let rid = r["id"].as_i64().unwrap();
        assert_eq!(r["vetted"], json!(false));
        let base_log = r["log"].as_array().unwrap().len();

        // Mark vetted: flag flips, and a decision entry records actor + from/to.
        let v = set_review_vetted(
            &pool,
            rid,
            true,
            Some("gatekeeper"),
            Some("adversarial pass clean"),
        )
        .await?;
        assert_eq!(v["vetted"], json!(true));
        let log = v["log"].as_array().unwrap();
        assert_eq!(log.len(), base_log + 1, "one audit entry added");
        let entry = log.last().unwrap();
        assert_eq!(entry["entry_type"], json!("decision"));
        assert_eq!(entry["author"], json!("gatekeeper"));
        assert!(
            entry["body"]
                .as_str()
                .unwrap()
                .contains("vetted: false -> true"),
            "audit records from/to: {entry}"
        );

        // The creator (author) hears review.vetted_changed; the actor (gatekeeper) does not self-notify.
        let inbox = check_notifications(&pool, "author", true, 50, None).await?;
        assert!(
            inbox["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["type"] == json!("review.vetted_changed")),
            "creator is notified of the vetted change: {inbox}"
        );

        // Idempotent no-op: setting true again adds no log entry.
        let again = set_review_vetted(&pool, rid, true, Some("gatekeeper"), None).await?;
        assert_eq!(
            again["log"].as_array().unwrap().len(),
            base_log + 1,
            "no-op adds no entry"
        );

        // Clearing flips it back and logs another decision entry.
        let cleared = set_review_vetted(&pool, rid, false, Some("gatekeeper"), None).await?;
        assert_eq!(cleared["vetted"], json!(false));
        assert_eq!(cleared["log"].as_array().unwrap().len(), base_log + 2);

        // Unknown review errors (404 at the API).
        assert!(set_review_vetted(&pool, 99999, true, Some("x"), None)
            .await
            .is_err());
        Ok(())
    }

    /// lint_text (task 558) is the non-bailing dry-run: it reports EVERY finding against the live
    /// list (both banned phrases and all non-ASCII chars with position) instead of failing on the
    /// first, and reflects clean text as clean=true. Same source of truth as check_content.
    #[tokio::test]
    async fn lint_text_reports_all_findings_without_bailing() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        add_banned_phrase(&pool, "robust", None, Some("tester")).await?;

        // Clean ASCII text with no banned phrase: clean=true, all lists empty.
        let clean = lint_text(&pool, "a perfectly fine sentence").await?;
        assert_eq!(clean["clean"], json!(true), "clean text: {clean}");
        assert_eq!(clean["banned_phrases"].as_array().unwrap().len(), 0);
        assert_eq!(clean["non_ascii"].as_array().unwrap().len(), 0);
        assert_eq!(clean["bare_refs"].as_array().unwrap().len(), 0);

        // task 616: lint_text surfaces the same bare-"#N" the write path hard-rejects, so it is a
        // complete pre-send lint. A bare ref -> clean=false with ready-to-paste typed forms; a
        // typed form or a #N inside code is clean (matching check_bare_refs).
        let bare = lint_text(&pool, "duplicate of #190, see also task_7").await?;
        assert_eq!(bare["clean"], json!(false), "bare ref: {bare}");
        let refs = bare["bare_refs"].as_array().unwrap();
        assert_eq!(
            refs.len(),
            1,
            "only the bare #190 flagged, not task_7: {bare}"
        );
        assert_eq!(refs[0]["ref"], json!("#190"));
        let sugg = refs[0]["suggestions"].as_array().unwrap();
        assert!(
            sugg.iter().any(|s| s == &json!("task_190"))
                && sugg.iter().any(|s| s == &json!("camshaft/task-board#190")),
            "suggestions carry the typed forms: {bare}"
        );
        assert!(
            lint_text(&pool, "the `#190` token and task_7").await?["bare_refs"]
                .as_array()
                .unwrap()
                .is_empty(),
            "a #N inside code is not flagged"
        );

        // A banned phrase plus two non-ASCII chars: all reported, clean=false, and no early bail
        // means the em dash on line 2 is caught even though the banned phrase came first.
        let dirty = lint_text(&pool, "this is robust\nand uses \u{2014} plus \u{2764}").await?;
        assert_eq!(dirty["clean"], json!(false), "dirty text: {dirty}");
        assert_eq!(dirty["banned_phrases"], json!(["robust"]));
        let na = dirty["non_ascii"].as_array().unwrap();
        assert_eq!(na.len(), 2, "both non-ASCII chars reported: {dirty}");
        assert_eq!(na[0]["codepoint"], json!("U+2014"));
        assert_eq!(na[0]["line"], json!(2));
        assert_eq!(na[1]["codepoint"], json!("U+2764"));

        // check_non_ascii (the bailing path) agrees on the first offender: same source of truth.
        assert!(check_non_ascii("ok \u{2014} no", false).is_err());
        assert!(check_non_ascii("all ascii here", false).is_ok());
        Ok(())
    }

    // task 625: grade_document scores the mechanical doc_7 A8 rubric. A conformant doc is clean; a
    // doc that trips multiple checks reports each with the right severity and has_hard_fail=true.
    #[tokio::test]
    async fn grade_document_scores_the_a8_rubric() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        add_banned_phrase(&pool, "robust", None, Some("t")).await?;

        // Conformant: required sections in order, ascii, short headings, no body table/image, under
        // budget. The table in the Appendix is allowed (it is past the main-body bound).
        let clean_doc = "Intro paragraph with a little context.\n\
            \n## Background\nSome context.\n\
            \n## Problem Statement\nThe problem.\n\
            \n## Requirements / Goals / Non-Goals\nThe requirements.\n\
            \n## Solutions\nThe options.\n\
            \n## Recommendation\nThe pick.\n\
            \n## Appendix\ncol | col\n--- | ---\na | b\n";
        let g = grade_document(&pool, clean_doc, "A Concise Design Title", None).await?;
        assert_eq!(g["clean"], json!(true), "clean doc should pass: {g}");
        assert_eq!(g["has_hard_fail"], json!(false));
        assert_eq!(g["findings"].as_array().unwrap().len(), 0);

        // Trips: ascii (em dash), banned-phrase, missing required section, a table in the body,
        // a status/provenance line, and a caps-for-emphasis word.
        let dirty = "Status: draft\n\
            \n## Background\n\
            This uses an em dash \u{2014} and the word robust. It is REALLY so, and it really matters.\n\
            \n## Problem Statement\nA table in the body:\ncol | col\n--- | ---\nx | y\n\
            \n## Solutions\nOptions; the Requirements section is missing.\n\
            \n## Recommendation\nPick.\n\
            \n## Appendix\nend\n";
        let d = grade_document(&pool, dirty, "T", None).await?;
        assert_eq!(d["clean"], json!(false), "dirty doc should fail: {d}");
        assert_eq!(d["has_hard_fail"], json!(true));
        let checks: std::collections::BTreeSet<String> = d["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["check"].as_str().unwrap().to_string())
            .collect();
        for expect in [
            "ascii-only",
            "banned-phrases",
            "required-sections",
            "body-hygiene",
            "status-provenance",
            "caps-emphasis",
        ] {
            assert!(checks.contains(expect), "expected a {expect} finding: {d}");
        }
        // Every finding carries the required fields.
        for f in d["findings"].as_array().unwrap() {
            assert!(
                f["check"].is_string() && f["severity"].is_string() && f["message"].is_string()
            );
        }
        Ok(())
    }

    /// check_cid_content (task 564) gates a publish-by-CID by fetching + scanning the bytes, but
    /// short-circuits (never touches IPFS) on the skip conditions: no backend configured, the
    /// author acknowledged, or non-text content. Those are the branches testable without a live
    /// backend; the fetch+scan itself is verified live (deployed board has an IPFS backend).
    #[tokio::test]
    async fn check_cid_content_skips_without_backend_or_ack_or_nontext() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        add_banned_phrase(&pool, "robust", None, Some("t")).await?;

        // No IPFS backend -> cannot fetch, so it cannot gate: Ok without touching the network.
        assert!(
            check_cid_content(&pool, None, "Qm-whatever", "text/markdown", false)
                .await
                .is_ok()
        );
        // A configured backend URL that we never reach, because acknowledge short-circuits first.
        assert!(check_cid_content(
            &pool,
            Some("http://127.0.0.1:1"),
            "Qm-x",
            "text/markdown",
            true
        )
        .await
        .is_ok());
        // Non-text content is out of scope for the banned-phrase/ASCII gate: skipped before any fetch.
        assert!(check_cid_content(
            &pool,
            Some("http://127.0.0.1:1"),
            "Qm-x",
            "image/png",
            false
        )
        .await
        .is_ok());
        Ok(())
    }

    /// A comment on a document notifies the people working any task the doc is attached to -- the
    /// task's assignee/creator -- not only doc watchers (task 581), so design-doc feedback reaches
    /// the owner of the related work even if they never subscribed to the doc itself.
    #[tokio::test]
    async fn doc_comment_notifies_attached_task_owner() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "owner", None, None, None, None, None).await?;
        register_agent(&pool, "assignee", None, None, None, None, None).await?;
        register_agent(&pool, "commenter", None, None, None, None, None).await?;
        let doc = create_document(
            &pool,
            "Design",
            None,
            "Qm-cid",
            None,
            Some("owner"),
            None,
            None,
            None,
        )
        .await?;
        let did = doc["id"].as_i64().unwrap();
        let p = create_project(&pool, "P", None, Some("owner"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(
            &pool,
            pid,
            "T",
            None,
            Some("assignee"),
            None,
            Some("owner"),
            None,
            None,
            None,
        )
        .await?;
        let tid = t["id"].as_i64().unwrap();
        attach_document(&pool, did, tid, Some("owner")).await?;
        // Drain prior notifications so we isolate the comment's fan-out.
        check_notifications(&pool, "owner", true, 50, None).await?;
        check_notifications(&pool, "assignee", true, 50, None).await?;

        comment_document(
            &pool,
            did,
            None,
            Some("commenter"),
            "please revise section 2",
            None,
            None,
            None,
        )
        .await?;

        // The attached task's assignee hears the doc comment even though they never subscribed to
        // the doc; the doc owner hears it too (doc recipient).
        let assignee_inbox = check_notifications(&pool, "assignee", true, 50, None).await?;
        assert!(
            assignee_inbox["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["type"] == json!("document.comment")),
            "attached-task assignee hears the doc comment: {assignee_inbox}"
        );
        let owner_inbox = check_notifications(&pool, "owner", true, 50, None).await?;
        assert!(
            owner_inbox["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["type"] == json!("document.comment")),
            "doc owner hears the doc comment: {owner_inbox}"
        );
        Ok(())
    }

    /// normalize_presence (task 496) coerces free-form status into the canonical presence enum,
    /// salvaging narrative into status_message, never rejecting. Not-running narratives coerce to
    /// offline (the agent_expected_running coupling); an explicit status_message always wins.
    #[test]
    fn normalize_presence_coerces_and_salvages() {
        // Canonical value passes through untouched; an explicit message is preserved.
        assert_eq!(
            normalize_presence("online", None),
            ("online".to_string(), None)
        );
        assert_eq!(
            normalize_presence("busy", Some("compiling")),
            ("busy".to_string(), Some("compiling".to_string()))
        );
        // Synonyms map to canonical presence.
        assert_eq!(normalize_presence("working", None).0, "busy");
        assert_eq!(normalize_presence("afk", None).0, "away");
        assert_eq!(normalize_presence("available", None).0, "online");
        // Not-running narratives coerce to offline (keeps agent_expected_running's semantics).
        assert_eq!(normalize_presence("done", None).0, "offline");
        assert_eq!(normalize_presence("stopped", None).0, "offline");
        assert_eq!(normalize_presence("cancelled", None).0, "offline");
        // Narrative after the presence word: first token gives presence, full text is salvaged.
        assert_eq!(
            normalize_presence("idle: inbox drained", None),
            ("idle".to_string(), Some("idle: inbox drained".to_string()))
        );
        // Pure unrecognized narrative: presence defaults to online (a live caller), text salvaged.
        assert_eq!(
            normalize_presence("fixing the gate", None),
            ("online".to_string(), Some("fixing the gate".to_string()))
        );
        // An explicit status_message always wins over salvage.
        assert_eq!(
            normalize_presence("idle: drained", Some("real note")),
            ("idle".to_string(), Some("real note".to_string()))
        );
    }

    /// create_task/update_task annotate (never reject) when the assignee is not a recognized
    /// principal -- a registered agent or a known identity alias / canonical (task 340), so an
    /// orchestrator catches a dead-letter owner like "dotfiles" at assign time.
    #[tokio::test]
    async fn assignee_warning_flags_unregistered_non_principal() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "real-agent", None, None, None, None, None).await?;
        let p = create_project(&pool, "P", None, Some("real-agent"), None).await?;
        let pid = p["id"].as_i64().unwrap();

        // Unregistered, non-alias assignee -> warning present (the "dotfiles" dead-letter case).
        let t1 = create_task(
            &pool,
            pid,
            "T1",
            None,
            Some("dotfiles"),
            None,
            Some("real-agent"),
            None,
            None,
            None,
        )
        .await?;
        assert!(
            t1.get("assignee_warning").is_some(),
            "unregistered assignee warns: {t1}"
        );

        // Registered agent -> no warning.
        let t2 = create_task(
            &pool,
            pid,
            "T2",
            None,
            Some("real-agent"),
            None,
            Some("real-agent"),
            None,
            None,
            None,
        )
        .await?;
        assert!(
            t2.get("assignee_warning").is_none(),
            "registered assignee: no warning: {t2}"
        );

        // Seeded alias "operator" (-> cameron) and its canonical "cameron" are known identities.
        let t3 = create_task(
            &pool,
            pid,
            "T3",
            None,
            Some("operator"),
            None,
            Some("real-agent"),
            None,
            None,
            None,
        )
        .await?;
        assert!(
            t3.get("assignee_warning").is_none(),
            "alias assignee: no warning: {t3}"
        );
        let t4 = create_task(
            &pool,
            pid,
            "T4",
            None,
            Some("cameron"),
            None,
            Some("real-agent"),
            None,
            None,
            None,
        )
        .await?;
        assert!(
            t4.get("assignee_warning").is_none(),
            "canonical identity: no warning: {t4}"
        );

        // update_task: setting an unregistered assignee warns; unassigning (empty sentinel) does not.
        let tid = t2["id"].as_i64().unwrap();
        let u = update_task(
            &pool,
            tid,
            None,
            Some("ghost-xyz"),
            None,
            None,
            None,
            Some("real-agent"),
            None,
            None,
            None,
        )
        .await?;
        assert!(
            u.get("assignee_warning").is_some(),
            "update to unregistered warns: {u}"
        );
        let un = update_task(
            &pool,
            tid,
            None,
            Some(""),
            None,
            None,
            None,
            Some("real-agent"),
            None,
            None,
            None,
        )
        .await?;
        assert!(
            un.get("assignee_warning").is_none(),
            "unassign: no warning: {un}"
        );
        Ok(())
    }

    /// list_tasks annotates each task with its assignee's reachability (task 340 part 2): a live
    /// registered assignee gets its presence status + last_seen; an unregistered assignee or no
    /// assignee gets nulls (the signal that the owner is not a live board agent).
    #[tokio::test]
    async fn assignee_reachability_annotation() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "live-agent", None, None, None, None, None).await?;
        set_status(&pool, "live-agent", "busy", None).await?;
        let mut tasks = vec![
            json!({ "id": 1, "assignee": "live-agent" }),
            json!({ "id": 2, "assignee": "ghost-xyz" }),
            json!({ "id": 3, "assignee": Value::Null }),
        ];
        annotate_assignee_reachability(&pool, &mut tasks).await?;
        assert_eq!(
            tasks[0]["assignee_status"],
            json!("busy"),
            "live assignee status: {:?}",
            tasks[0]
        );
        assert!(
            tasks[0]["assignee_last_seen"].is_string(),
            "live assignee has last_seen"
        );
        assert_eq!(
            tasks[1]["assignee_status"],
            Value::Null,
            "unregistered assignee -> null status"
        );
        assert_eq!(tasks[1]["assignee_last_seen"], Value::Null);
        assert_eq!(
            tasks[2]["assignee_status"],
            Value::Null,
            "no assignee -> null status"
        );
        Ok(())
    }

    /// The multi-operator model (task 542 Phase 1): recursive team membership resolves to people
    /// through nested teams, write-time cycle/self/missing-member guards reject bad adds, the
    /// read-time expansion is cycle-safe even against a directly-inserted cycle, and
    /// resolve_principal_ids expands a team to people AND agents but passes a person/agent through.
    #[tokio::test]
    async fn teams_recursive_membership_and_cycle_guard() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        // Seeded by db::init: person cameron, team operator (member cameron).
        assert_eq!(
            get_team(&pool, "operator").await?["resolved_people"],
            json!(["cameron"])
        );

        create_person(&pool, "zach", Some("Zach"), Some("system"), None).await?;
        create_team(&pool, "eng", Some("Engineering"), Some("system"), None).await?;
        add_team_member(&pool, "eng", "zach", "person", Some("system")).await?;
        add_team_member(&pool, "eng", "operator", "team", Some("system")).await?; // nested team

        // eng expands to zach + cameron (cameron via the nested operator team).
        let eng_people: BTreeSet<String> = get_team(&pool, "eng").await?["resolved_people"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            eng_people,
            BTreeSet::from(["zach".to_string(), "cameron".to_string()])
        );

        // Team-scoped agents (task 542 Phase 1c): an agent can be a team member; the team resolves
        // to its people AND its agents, kept separate (resolved_people unchanged).
        register_agent(&pool, "v-rev", None, None, None, None, None).await?;
        add_team_member(&pool, "eng", "v-rev", "agent", Some("system")).await?;
        let eng = get_team(&pool, "eng").await?;
        assert_eq!(
            eng["resolved_agents"],
            json!(["v-rev"]),
            "agent member surfaces in resolved_agents"
        );
        let eng_people2: BTreeSet<String> = eng["resolved_people"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            eng_people2,
            BTreeSet::from(["zach".to_string(), "cameron".to_string()]),
            "resolved_people is NOT polluted by the agent member"
        );
        assert!(
            add_team_member(&pool, "eng", "v-ghost", "agent", Some("system"))
                .await
                .is_err(),
            "missing agent rejected"
        );

        // Write-time guards: a cycle (operator already nested under eng), self-membership, and a
        // missing member are all rejected.
        assert!(
            add_team_member(&pool, "operator", "eng", "team", Some("system"))
                .await
                .is_err(),
            "cycle add rejected"
        );
        assert!(
            add_team_member(&pool, "eng", "eng", "team", Some("system"))
                .await
                .is_err(),
            "self rejected"
        );
        assert!(
            add_team_member(&pool, "eng", "ghost", "person", Some("system"))
                .await
                .is_err(),
            "missing member rejected"
        );

        // resolve_principal_ids: a team expands to people AND agents; a person/agent passes through.
        assert_eq!(
            resolve_principal_ids(&pool, "operator").await?,
            BTreeSet::from(["cameron".to_string()])
        );
        assert_eq!(
            resolve_principal_ids(&pool, "eng").await?,
            BTreeSet::from([
                "zach".to_string(),
                "cameron".to_string(),
                "v-rev".to_string()
            ]),
            "eng principals = people (zach, cameron) + agent (v-rev)"
        );
        assert_eq!(
            resolve_principal_ids(&pool, "zach").await?,
            BTreeSet::from(["zach".to_string()])
        );
        assert_eq!(
            resolve_principal_ids(&pool, "v-some-agent").await?,
            BTreeSet::from(["v-some-agent".to_string()])
        );

        // Defensive: a cycle inserted DIRECTLY (bypassing the write-time guard) still terminates on
        // read -- the visited-set guard in resolve_team_principals stops the loop.
        create_team(&pool, "leads", None, Some("system"), None).await?;
        add_team_member(&pool, "leads", "eng", "team", Some("system")).await?;
        sqlx::query("INSERT INTO team_members(team_id, member_id, member_kind, created_at) VALUES('eng','leads','team',?)")
            .bind(now_iso())
            .execute(&pool)
            .await?;
        let _ = resolve_team_principals(&pool, "eng").await?; // must terminate despite the eng<->leads cycle

        // Deleting a team cascades its membership edges: 'leads' is dropped both as a member of eng
        // and as a team that had members. After it, eng no longer lists leads.
        delete_team(&pool, "leads").await?;
        assert!(
            get_team(&pool, "leads").await.is_err(),
            "deleted team is gone"
        );
        let eng_members: Vec<String> = get_team(&pool, "eng").await?["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["member_id"].as_str().unwrap().to_string())
            .collect();
        assert!(
            !eng_members.contains(&"leads".to_string()),
            "leads edge cascaded out of eng"
        );
        assert!(
            delete_team(&pool, "leads").await.is_err(),
            "deleting a missing team errors"
        );

        // Deleting a person cascades their memberships: zach drops out of eng's resolution.
        delete_person(&pool, "zach").await?;
        assert!(
            get_person(&pool, "zach").await.is_err(),
            "deleted person is gone"
        );
        let (eng_people_final, eng_agents_final) = resolve_team_principals(&pool, "eng").await?;
        assert_eq!(
            eng_people_final,
            BTreeSet::from(["cameron".to_string()]),
            "eng now resolves to cameron only (via operator) after zach deleted"
        );
        assert_eq!(
            eng_agents_final,
            BTreeSet::from(["v-rev".to_string()]),
            "the team-scoped agent member remains after the person deletions"
        );
        Ok(())
    }
}
