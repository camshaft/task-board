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
            "ambiguous bare reference \"#{n}\": write task_{n} for a board task, or <owner>/<repo>#{n} \
             (e.g. camshaft/task-board#{n}) for an external GitHub reference"
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
    let Some(id) = m.get("external_author").and_then(|v| v.as_str()).map(str::to_string) else {
        return;
    };
    let name: Option<String> =
        sqlx::query_scalar::<_, Option<String>>("SELECT display_name FROM external_identities WHERE id=?")
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
        let auto_channels =
            sqlx::query("SELECT id FROM channels WHERE auto_join=1").fetch_all(&mut *tx).await?;
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
    const CLEARABLE: [&str; 5] = ["display_name", "kind", "charter", "status_message", "webhook_url"];
    if let Some(clear) = clear {
        for name in clear {
            if !CLEARABLE.contains(&name.as_str()) {
                anyhow::bail!("cannot clear field '{name}'; clearable fields: {}", CLEARABLE.join(", "));
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

pub async fn set_status(
    pool: &Pool,
    agent_id: &str,
    status: &str,
    status_message: Option<&str>,
) -> anyhow::Result<Value> {
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
                for k in ["id", "display_name", "status", "metadata"] {
                    if let Some(v) = o.get(k) {
                        m.insert(k.to_string(), v.clone());
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
        let out = project_json(&mut tx, existing_id).await?.unwrap_or(Value::Null);
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
            let clash = sqlx::query("SELECT 1 FROM projects WHERE name = ? COLLATE NOCASE AND id != ?")
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
        let out = project_json(&mut tx, project_id).await?.unwrap_or(Value::Null);
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
    let out = project_json(&mut tx, project_id).await?.unwrap_or(Value::Null);
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
        None => sqlx::query("SELECT * FROM projects ORDER BY id")
            .fetch_all(pool)
            .await?,
    };
    let mut out = Vec::new();
    for r in &rows {
        let mut d = row_to_json(r);
        let pid: i64 = r.try_get("id")?;
        let counts = sqlx::query("SELECT status, COUNT(*) n FROM tasks WHERE project_id=? GROUP BY status")
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
    let rows = sqlx::query("SELECT alias, canonical, created_by, created_at FROM identity_aliases ORDER BY alias")
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
    if let Value::Object(ref mut m) = out {
        m.insert("created".into(), json!(true));
    }
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
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
        let target: Option<i64> = if new_parent == 0 { None } else { Some(new_parent) };
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
        Set { kind: String, target: Option<String>, note: Option<String> },
    }
    let change = match &blocked_on {
        None => BlockedChange::Leave,
        Some(Value::Null) => BlockedChange::Clear,
        Some(Value::Object(o)) => {
            let kind = o.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let target = o
                .get("target")
                .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_i64().map(|n| n.to_string())))
                .filter(|s| !s.is_empty());
            let note = o.get("note").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string());
            BlockedChange::Set { kind, target, note }
        }
        Some(_) => anyhow::bail!("give `blocked_on` as an object with a `kind`, or null to clear"),
    };
    if let BlockedChange::Set { kind, target, .. } = &change {
        match kind.as_str() {
            "task" => {
                let Some(t) = target.as_deref().and_then(|s| s.parse::<i64>().ok()) else {
                    anyhow::bail!("give a `blocked_on.target` task id when kind=task");
                };
                if sqlx::query("SELECT 1 FROM tasks WHERE id=?").bind(t).fetch_optional(&mut *tx).await?.is_none() {
                    anyhow::bail!("no task {t}");
                }
            }
            "agent" => {
                let Some(a) = target.as_deref() else {
                    anyhow::bail!("give a `blocked_on.target` agent id when kind=agent");
                };
                if sqlx::query("SELECT 1 FROM agents WHERE id=?").bind(a).fetch_optional(&mut *tx).await?.is_none() {
                    anyhow::bail!("no agent {a}");
                }
            }
            "operator" => {}
            // External/infra dependency with no board owner (task_112): the free-text blocked_on.note
            // says what it waits on; no target. A DISTINCT kind from operator, so it stays OFF the
            // operator queue (blocked_on_kind='operator') -- a task waiting on external infra is
            // truthfully blocked without wrongly pinging the operator dashboard.
            "external" => {}
            other => anyhow::bail!(
                "give a valid `blocked_on.kind` (task, agent, operator, or external), not '{other}'"
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
            "give a `blocked_on` (kind: task, agent, operator, or external) — a blocked task must record what it is waiting on"
        );
    }
    let mut blocked_changed = false;
    let mut notify_agent: Option<(String, Option<String>)> = None; // (agent id, note) to notify
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
        let stored_ref = if kind == "operator" || kind == "external" { None } else { target.clone() };
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
        // Notify a newly-blocking agent (skip if it's already blocked on the same agent).
        if kind == "agent" {
            if let Some(agent) = &stored_ref {
                let already = old_blocked_kind.as_deref() == Some("agent")
                    && old_blocked_ref.as_deref() == Some(agent.as_str());
                if !already {
                    notify_agent = Some((agent.clone(), note.clone()));
                }
            }
        }
    }
    if let Some((agent, note)) = notify_agent {
        let mut set = BTreeSet::new();
        set.insert(agent);
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
    if (has_fields || blocked_changed) && !status_changed && !reassigned && !unassigned && !reparented {
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
    let out = fetch_one_json(&mut tx, "SELECT * FROM tasks WHERE id=?", task_id)
        .await?
        .unwrap_or(Value::Null);
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
    let child_count: i64 =
        sqlx::query("SELECT COUNT(*) AS n FROM tasks WHERE parent_id=?")
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
    let event_type = if archived { "task.archived" } else { "task.restored" };
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
        let monitor_exempt = meta.get("monitor_exempt").and_then(|v| v.as_bool()).unwrap_or(false);
        m.insert("metadata".into(), meta);
        m.insert("monitor_exempt".into(), Value::Bool(monitor_exempt));

        // Collapse the raw blocked_on_* columns into one nested object (null when not blocked).
        let blocked_on = m.get("blocked_on_kind").and_then(|v| v.as_str()).map(|kind| {
            json!({
                "kind": kind,
                "target": m.get("blocked_on_ref").cloned().unwrap_or(Value::Null),
                "note": m.get("blocked_on_note").cloned().unwrap_or(Value::Null),
            })
        });
        m.remove("blocked_on_kind");
        m.remove("blocked_on_ref");
        m.remove("blocked_on_note");
        m.insert("blocked_on".into(), blocked_on.unwrap_or(Value::Null));

        // Comments, bounded by `comments_limit` (#511). Always report the total so a caller knows
        // whether there is more than what was inlined.
        let comment_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM comments WHERE task_id=?")
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
                     ei.display_name AS external_author_name \
                     FROM comments c LEFT JOIN external_identities ei ON ei.id = c.external_author \
                     WHERE c.task_id=? ORDER BY c.id DESC LIMIT ?",
                )
                .bind(task_id)
                .bind(n)
                .fetch_all(pool)
                .await?;
                rows.iter().rev().map(row_to_json).collect()
            }
            // None (or a non-positive n other than 0): the whole thread, chronological.
            _ => {
                let rows = sqlx::query(
                    "SELECT c.id, c.author, c.body, c.created_at, c.external_author, c.origin_ref, \
                     ei.display_name AS external_author_name \
                     FROM comments c LEFT JOIN external_identities ei ON ei.id = c.external_author \
                     WHERE c.task_id=? ORDER BY c.id",
                )
                .bind(task_id)
                .fetch_all(pool)
                .await?;
                rows.iter().map(row_to_json).collect()
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
        let children = sqlx::query(
            "SELECT id, title, status FROM tasks WHERE parent_id=? ORDER BY id",
        )
        .bind(task_id)
        .fetch_all(pool)
        .await?;
        let total = children.len() as i64;
        let done = children
            .iter()
            .filter(|r| r.try_get::<String, _>("status").map(|s| s == "done").unwrap_or(false))
            .count() as i64;
        m.insert("children".into(), Value::Array(children.iter().map(|r| row_to_json_ref(r, "task")).collect()));
        m.insert("child_rollup".into(), json!({ "done": done, "total": total }));

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
    let out: Vec<Value> = rows
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
    Ok(Value::Array(out))
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
    mirror_task_comment_to_thread(&mut tx, &mut hooks, task_id, cid, author, body, external_author).await?;
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
    Ok(json!({ "subscriber": subscriber, "target_type": tt, "target_id": tid, "event_classes": event_classes }))
}

/// Subscribe an agent to a channel THREAD (#438). The thread root is a channel post's event seq;
/// subsequent in-thread posts (reply_to = this root) are delivered to the subscriber AND wake them,
/// so a reactive agent that joined a thread answers later follow-ups without a re-mention. The root
/// seq is globally unique, so it alone keys the subscription. Idempotent (INSERT OR IGNORE) so a
/// bridge daemon can safely re-register the same root each tick.
pub async fn subscribe_thread(pool: &Pool, subscriber: &str, thread_root: i64) -> anyhow::Result<Value> {
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
pub async fn unsubscribe_thread(pool: &Pool, subscriber: &str, thread_root: i64) -> anyhow::Result<Value> {
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
    if let Some(row) = sqlx::query(
        "SELECT id FROM channels WHERE dm_key IS NULL AND name = ? COLLATE NOCASE",
    )
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?
    {
        let existing_id: i64 = row.try_get("id")?;
        if let Some(sub) = created_by {
            join_channel(&mut tx, existing_id, sub).await?;
        }
        let out = channel_row_json(&mut tx, existing_id).await?.unwrap_or(Value::Null);
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
    let out = channel_row_json(&mut tx, channel_id).await?.unwrap_or(Value::Null);
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
    let evtype = if is_dm.is_some() { "message.direct" } else { "channel.post" };
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
    mirror_thread_reply_to_task(&mut tx, &mut hooks, channel_id, seq, reply_to, sender, body, external_author)
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
    post_to_channel_meta(pool, channel_id, sender, body, reply_to, external_author, None).await
}

/// Outbound reflect-back policy (design #141 §5), read from a policy-bearing `metadata` bag —
/// a channel's `metadata` for channel posts, or an `external_links` row's `metadata` for task
/// comments (task 264): `{ "outbound_authors": [..] (default ["concierge"]), "direction":
/// "in"|"out"|"both" (default "in") }`. Content reflects OUT to an external system iff
/// `direction` allows outbound (`out`/`both`) AND its `author` is in the `outbound_authors`
/// allowlist. The safe default is board-internal: an unconfigured entity (direction defaults to
/// "in") reflects nothing, so existing channels/links never start leaking to an external system.
fn reflects_out(metadata: &Value, author: &str) -> bool {
    let direction = metadata.get("direction").and_then(|v| v.as_str()).unwrap_or("in");
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
pub async fn set_channel_props(pool: &Pool, channel_id: i64, props: Value) -> anyhow::Result<Value> {
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
        let agents = sqlx::query("SELECT id FROM agents").fetch_all(&mut *tx).await?;
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
    let out = channel_row_json(&mut tx, channel_id).await?.unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Derive a task title from a thread's root body: its first non-empty line, trimmed and
/// truncated, with a fallback when the body is empty.
fn thread_title(body: &str, channel_id: i64) -> String {
    let first = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
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
    if let Some(row) = sqlx::query("SELECT task_id FROM task_links WHERE source_kind=? AND source_id=?")
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
        s.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_else(|| json!({}))
    };

    // The root post must exist in this channel.
    let Some(root) = posts.iter().find(|r| r.try_get::<i64, _>("seq").ok() == Some(root_post_seq))
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
    let Some(root_seq) = reply_to else { return Ok(()) };
    let source_id = format!("channel:{channel_id}:{root_seq}");
    let task_id: Option<i64> = sqlx::query(
        "SELECT task_id FROM task_links WHERE source_kind='channel_thread' AND source_id=?",
    )
    .bind(&source_id)
    .fetch_optional(&mut **tx)
    .await?
    .map(|r| r.try_get("task_id"))
    .transpose()?;
    let Some(task_id) = task_id else { return Ok(()) };
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
    emit(tx, hooks, "task.commented", Some(from), Some(task_id), None, None, None, data, Recipients::FromTask)
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
    let (Some(channel_id), Some(root_seq)) = (meta["channel_id"].as_i64(), meta["root_post_seq"].as_i64())
    else {
        return Ok(());
    };
    let from = author.unwrap_or("anon");
    let mut data = json!({ "body": body, "from": from, "reply_to": root_seq, "origin_comment": comment_id });
    if let Some(ext) = external_author {
        data["external_author"] = json!(ext);
    }
    emit(tx, hooks, "channel.post", Some(from), None, None, Some(channel_id), None, data, Recipients::FromChannelThread(channel_id, Some(root_seq)))
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
    let before_clause = if before_seq.is_some() { "AND seq<?" } else { "" };
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
    let out = channel_row_json(&mut tx, channel_id).await?.unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Get-or-create the private 1:1 DM channel for an unordered pair of agents. The pair is
/// keyed by `dm_key` (both ids sorted, NUL-joined) so A→B and B→A resolve to one channel.
/// Both agents are auto-joined. This is what lets DMs reuse the channel data model.
async fn dm_channel(
    tx: &mut Transaction<'_, Sqlite>,
    a: &str,
    b: &str,
) -> anyhow::Result<i64> {
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
        anyhow::bail!("give an `id` for the external identity (namespaced source:handle, e.g. slack:U123)");
    }
    if source.trim().is_empty() {
        anyhow::bail!("give a `source` for the external identity (e.g. slack, github)");
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    // Merge metadata into any existing bag (mirrors register_agent / update_project).
    let existing: Option<String> = sqlx::query("SELECT metadata FROM external_identities WHERE id=?")
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
    Ok(Value::Array(rows.iter().map(hydrate_external_identity).collect()))
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
        .or_else(|| existing.as_ref().and_then(|r| r.try_get::<Option<String>, _>("setup_script").ok().flatten()))
        .unwrap_or_default();
    let final_desc = description
        .map(str::to_string)
        .or_else(|| existing.as_ref().and_then(|r| r.try_get::<Option<String>, _>("description").ok().flatten()));
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
    Ok(Value::Array(rows.iter().map(hydrate_workspace_kind).collect()))
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
    let (mut line, mut col) = (1usize, 1usize);
    for ch in text.chars() {
        if ch == '\n' {
            line += 1;
            col = 1;
            continue;
        }
        if !ch.is_ascii() {
            anyhow::bail!(
                "non-ASCII character {ch:?} (U+{:04X}) at line {line}, column {col}. Board content \
                 must be ASCII — replace it (an em dash with '-', curly quotes with straight quotes, \
                 an arrow with '<->', drop emoji), or pass acknowledge_banned=true to submit anyway.",
                ch as u32
            );
        }
        col += 1;
    }
    Ok(())
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

// --- External links (bridged mappings: channel-map, issue↔task, thread↔task) ---

/// The board entity kinds an external link may target.
const EXTERNAL_LINK_KINDS: &[&str] = &["channel", "task", "thread", "comment"];

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
        anyhow::bail!("give a `board_kind` of one of: channel, task, thread, comment");
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    // A clean 404 for the kinds backed by a real table (thread = a channel post seq, skipped).
    match board_kind {
        "channel" => {
            if sqlx::query("SELECT 1 FROM channels WHERE id=?").bind(board_id).fetch_optional(&mut *tx).await?.is_none() {
                anyhow::bail!("no channel {board_id}");
            }
        }
        "task" => {
            if sqlx::query("SELECT 1 FROM tasks WHERE id=?").bind(board_id).fetch_optional(&mut *tx).await?.is_none() {
                anyhow::bail!("no task {board_id}");
            }
        }
        "comment" => {
            if sqlx::query("SELECT 1 FROM comments WHERE id=?").bind(board_id).fetch_optional(&mut *tx).await?.is_none() {
                anyhow::bail!("no comment {board_id}");
            }
        }
        _ => {}
    }
    // Merge metadata into any existing bag (mirrors the other upserts).
    let existing: Option<String> = sqlx::query("SELECT metadata FROM external_links WHERE source=? AND external_id=?")
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
    Ok(Value::Array(rows.iter().map(hydrate_external_link).collect()))
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
                    Some((p, v)) => (p, v.trim().trim_start_matches(['v', 'V']).parse::<i64>().ok()),
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
                        region: if is_embed { region.filter(|r| !r.is_empty()) } else { None },
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
    let Some(mut d) =
        fetch_one_json(tx, "SELECT * FROM documents WHERE id=?", document_id).await?
    else {
        return Ok(None);
    };
    let vrows = sqlx::query(
        "SELECT * FROM document_versions WHERE document_id=? ORDER BY version_no DESC",
    )
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
        let current =
            cur_id.and_then(|cid| versions.iter().find(|v| v["id"].as_i64() == Some(cid)).cloned());
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
        m.insert("attached_tasks".into(), Value::Array(tasks.iter().map(|r| row_to_json_ref(r, "task")).collect()));

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
        m.insert("outbound_links".into(), Value::Array(out_links.iter().map(row_to_json).collect()));

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
        m.insert("embeds".into(), Value::Array(embeds.iter().map(row_to_json).collect()));

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
    let out = document_json(&mut tx, document_id).await?.unwrap_or(Value::Null);
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
                    m.insert("body".into(), content.get("content").cloned().unwrap_or(Value::Null));
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
    let out = document_json(&mut tx, document_id).await?.unwrap_or(Value::Null);
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(out)
}

/// Cap on a from-session document-body read, matching the REST IPFS gateway cap.
pub const DOCUMENT_READ_CAP_BYTES: usize = 25 * 1024 * 1024;

/// Whether a content_type is text-shaped, i.e. safe to return as a UTF-8 string from the read
/// path. Binary types (image/pdf/...) are not inlined; the caller fetches their bytes by CID.
pub fn is_text_content_type(ct: &str) -> bool {
    let t = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
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
        let text = String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("document {document_id} v{vn} content is not valid UTF-8"))?;
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
    let rows = sqlx::query(
        "SELECT * FROM document_versions WHERE document_id=? ORDER BY version_no DESC",
    )
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
    Ok(Value::Array(rows.iter().map(|r| row_to_json_ref(r, "doc")).collect()))
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
    let new_path: Option<&str> = if trimmed.is_empty() { None } else { Some(trimmed) };
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
    let out = document_json(&mut tx, document_id).await?.unwrap_or(Value::Null);
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
    let arch = if include_archived { "" } else { " AND archived_at IS NULL" };
    let cols = "id, title, slug, path, project_id, status, current_version_id, \
                approved_version_id, created_by, updated_at, archived_at";
    let rows = match prefix.map(|p| p.trim().trim_matches('/')).filter(|p| !p.is_empty()) {
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
        Recipients::FromDocument(document_id),
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
pub async fn resolve_comment(pool: &Pool, comment_id: i64, actor: Option<&str>) -> anyhow::Result<Value> {
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
    Ok(Value::Array(rows.iter().map(document_comment_json).collect()))
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
    let out = document_json(&mut tx, document_id).await?.unwrap_or(Value::Null);
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
    let event_type = if archived { "document.archived" } else { "document.restored" };
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
    let out = document_json(&mut tx, document_id).await?.unwrap_or(Value::Null);
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
        m.insert("submit_url".into(), json!(format!("/secret-requests/{id}?t={submit_token}")));
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
    let row = sqlx::query("SELECT submit_token, submit_used, status, name, fulfiller FROM secret_requests WHERE id=?")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else { anyhow::bail!("no secret request {id}") };
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
    let Some(row) = row else { anyhow::bail!("no secret request {id}") };
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
    let row = sqlx::query("SELECT fulfiller_token, name, fulfiller, requested_by FROM secret_requests WHERE id=?")
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
const REVIEW_STATUSES: [&str; 5] =
    ["open", "in_review", "changes_requested", "approved", "closed"];

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
    let mut out = review_json(&mut tx, rid, true).await?.unwrap_or(Value::Null);
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
        let out = review_json(&mut tx, review_id, true).await?.unwrap_or(Value::Null);
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
    let out = review_json(&mut tx, review_id, true).await?.unwrap_or(Value::Null);
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
        let out = review_json(&mut tx, review_id, true).await?.unwrap_or(Value::Null);
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
    let out = review_json(&mut tx, review_id, true).await?.unwrap_or(Value::Null);
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
    Ok(json!({ "review_id": review_id, "entry_id": eid, "appended": true, "entry_type": entry_type }))
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
    let fpr = if n > 0 { findings as f64 / n as f64 } else { 0.0 };

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
        ("insufficient_data", "insufficient_data", false, Value::Null, Value::Null)
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
            Some(at) => finding_times.iter().filter(|t| t.as_str() > at.as_str()).count() as u64,
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

        let p = create_project(&pool, "Voron tuning", Some("dial in the printer"), Some("planner"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(&pool, pid, "Calibrate pressure advance", None, Some("fixer"), None, Some("planner"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();

        subscribe(&pool, "planner", Some(tid), None, None, None, false).await?; // (already auto-subscribed as creator)
        comment_task(&pool, tid, "Start from PA=0.03", Some("planner"), None, None).await?;
        update_task(&pool, tid, Some("in_progress"), None, None, None, None, Some("fixer"), None, None, None).await?;
        update_task(&pool, tid, Some("done"), None, None, None, None, Some("fixer"), None, None, None).await?;
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
        let expected: BTreeSet<String> =
            ["task.status_changed", "message.direct"].iter().map(|s| s.to_string()).collect();
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
        let t = create_task(&pool, pid, "T", None, None, None, Some("a"), None, None, None).await?;
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

    /// detect_bare_task_refs finds bare "#N" refs using the linkifier boundary rule, de-duped in
    /// first-seen order, and rejects the non-refs: a word char before "#" (incl. a repo-qualified
    /// owner/repo#N), "##", "&#123;", "#12ab", and a bare "#" with no digits (task #517).
    #[test]
    fn detect_bare_task_refs_matches_only_ambiguous_bare_refs() {
        assert_eq!(detect_bare_task_refs("see #183 please"), vec![183]);
        assert_eq!(detect_bare_task_refs("#12 and #34 and #12 again"), vec![12, 34]);
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
        let t = create_task(&pool, pid, "T", None, None, None, Some("a"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();

        // Comment with a bare ref -> rejected with an actionable, typed-form-naming error.
        let err = comment_task(&pool, tid, "duplicate of #7", Some("a"), None, None).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("ambiguous bare reference"), "got: {msg}");
        assert!(msg.contains("task_7"), "error names the typed form: {msg}");
        // Nothing was stored (rejected pre-write).
        assert_eq!(get_task(&pool, tid).await?["comment_count"], json!(0));

        // A clean comment (typed form + repo-qualified external) succeeds.
        comment_task(&pool, tid, "use task_7 and camshaft/task-board#7", Some("a"), None, None).await?;
        // A bare "#N" inside inline code or a fenced block is NOT a reference -> allowed.
        comment_task(&pool, tid, "the literal `#9` token", Some("a"), None, None).await?;
        comment_task(&pool, tid, "```\nsee #9 in code\n```", Some("a"), None, None).await?;
        assert_eq!(get_task(&pool, tid).await?["comment_count"], json!(3));

        // create_task + publish-style paths reject a bare ref in title/description too.
        assert!(create_task(&pool, pid, "blocks #9", None, None, None, Some("a"), None, None, None).await.is_err());
        assert!(create_task(&pool, pid, "title", Some("see #9"), None, None, Some("a"), None, None, None).await.is_err());
        // A clean create succeeds.
        create_task(&pool, pid, "clean title", Some("see task_9"), None, None, Some("a"), None, None, None).await?;
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
        create_task(&pool, pid, "plain", None, None, None, Some("a"), None, None, None).await?;
        let exempt = create_task(&pool, pid, "exempt", None, None, None, Some("a"), None, None, None).await?;
        let eid = exempt["id"].as_i64().unwrap();

        // Default: not exempt.
        assert_eq!(get_task(&pool, eid).await?["monitor_exempt"], json!(false));

        // Set metadata.monitor_exempt via the update_task metadata merge.
        update_task(&pool, eid, None, None, None, None, None, Some("a"), Some(json!({ "monitor_exempt": true })), None, None)
            .await?;

        let got = get_task(&pool, eid).await?;
        assert_eq!(got["monitor_exempt"], json!(true), "get_task reflects the derived flag");
        assert_eq!(got["metadata"]["monitor_exempt"], json!(true), "metadata stays the source of truth");

        // list_tasks surfaces the derived bool per row.
        let list =
            list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, None, None, false).await?;
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
        let t = create_task(&pool, pid, "blocked on brazil merge", None, None, None, Some("a"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();

        // Block on external with a free-text note, no target.
        update_task(&pool, tid, Some("blocked"), None, None, None, None, Some("a"), None, None,
            Some(json!({ "kind": "external", "note": "daily Brazil TPCII->VS merge / stale CratesIoIndex" }))).await?;
        let got = get_task(&pool, tid).await?;
        assert_eq!(got["status"], json!("blocked"));
        assert_eq!(got["blocked_on"]["kind"], json!("external"));
        assert_eq!(got["blocked_on"]["target"], json!(null), "external has no target");
        assert!(got["blocked_on"]["note"].as_str().unwrap().contains("Brazil"));

        // OFF the operator queue; ON the external filter.
        let op = list_tasks(&pool, None, None, None, false, None, false, None, Some("operator"), None, None, None, false).await?;
        assert!(op.as_array().unwrap().iter().all(|x| x["id"] != json!(tid)), "external task must not be on the operator queue");
        let ext = list_tasks(&pool, None, None, None, false, None, false, None, Some("external"), None, None, None, false).await?;
        assert!(ext.as_array().unwrap().iter().any(|x| x["id"] == json!(tid)), "external task found via the external filter");

        // An unknown kind is still rejected.
        assert!(update_task(&pool, tid, Some("blocked"), None, None, None, None, Some("a"), None, None,
            Some(json!({ "kind": "bogus" }))).await.is_err());
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
        let t = create_task(&pool, aid, "T", None, None, None, Some("alice"), None, None, None).await?;
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
            assert!(types.contains(want), "firehose should carry {want}: {types:?}");
        }

        // The watcher's OWN action does not notify itself (actor excluded).
        create_project(&pool, "C", None, Some("watcher"), None).await?;
        let own = check_notifications(&pool, "watcher", true, 50, None).await?;
        assert_eq!(own["count"].as_i64(), Some(0), "actor excluded from its own events");

        // Unsubscribing stops the firehose.
        unsubscribe(&pool, "watcher", None, None, None, None, true).await?;
        create_project(&pool, "D", None, Some("alice"), None).await?;
        let after = check_notifications(&pool, "watcher", true, 50, None).await?;
        assert_eq!(after["count"].as_i64(), Some(0), "no events after unsubscribe");

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
        let d2 = publish_version(&pool, did, "bafyv2", Some("revise"), Some("alice"), None, None).await?;
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
        assert_eq!(list_documents(&pool, Some(pid), None, None, None, None, false).await?.as_array().unwrap().len(), 1);
        assert_eq!(
            list_documents(&pool, Some(pid), Some("draft"), None, None, None, false).await?.as_array().unwrap().len(),
            1
        );
        assert_eq!(
            list_documents(&pool, Some(pid), Some("approved"), None, None, None, false).await?.as_array().unwrap().len(),
            0
        );
        assert_eq!(list_documents(&pool, Some(99999), None, None, None, None, false).await?.as_array().unwrap().len(), 0);

        // A new version resets an approved doc back to in_review.
        sqlx::query("UPDATE documents SET status='approved' WHERE id=?")
            .bind(did)
            .execute(&pool)
            .await?;
        let d3 = publish_version(&pool, did, "bafyv3", None, Some("alice"), None, None).await?;
        assert_eq!(d3["status"], json!("in_review"), "new version supersedes approval");

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
            async move { create_document(&pool, title, None, "bafy", None, Some("alice"), None, None, None).await }
        };
        let a = mk("A").await?["id"].as_i64().unwrap();
        let b = mk("B").await?["id"].as_i64().unwrap();
        let c = mk("C").await?["id"].as_i64().unwrap();

        // File a doc; leading/trailing slashes are trimmed, and get_document reflects the path.
        let filed = set_document_path(&pool, a, "/architecture/board/events/", Some("alice")).await?;
        assert_eq!(filed["path"], json!("architecture/board/events"));
        assert_eq!(get_document(&pool, a).await?["path"], json!("architecture/board/events"));

        // Collision: filing another doc at the same path is rejected (400-mapped "give " error).
        let err = set_document_path(&pool, b, "architecture/board/events", None).await.unwrap_err();
        assert!(err.to_string().starts_with("give "), "collision error, got: {err}");

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
        assert_eq!(list_wiki(&pool, Some("runbooks"), false).await?.as_array().unwrap().len(), 1);

        // Rename frees the old path (b can now take it) and clearing unfiles a doc.
        set_document_path(&pool, a, "architecture/board/events-v2", None).await?;
        set_document_path(&pool, b, "architecture/board/events", None).await?; // no longer a collision
        set_document_path(&pool, c, "", None).await?; // clear -> unfiled
        assert!(get_document(&pool, c).await?["path"].is_null());
        assert_eq!(list_wiki(&pool, None, false).await?.as_array().unwrap().len(), 2);
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
            async move { create_document(&pool, title, None, "bafy", None, Some("alice"), None, None, None).await }
        };
        let keep = mk("Keep").await?["id"].as_i64().unwrap();
        let probe = mk("Probe").await?["id"].as_i64().unwrap();
        set_document_path(&pool, keep, "docs/keep", None).await?;
        set_document_path(&pool, probe, "docs/probe", None).await?;

        // Both visible before archiving.
        assert_eq!(list_documents(&pool, None, None, None, None, None, false).await?.as_array().unwrap().len(), 2);
        assert_eq!(list_wiki(&pool, None, false).await?.as_array().unwrap().len(), 2);

        // Archive the probe: hidden from list_documents and the wiki tree by default.
        let archived = set_document_archived(&pool, probe, true, Some("concierge")).await?;
        assert!(archived["archived_at"].is_string(), "archived_at is stamped");
        assert_eq!(list_documents(&pool, None, None, None, None, None, false).await?.as_array().unwrap().len(), 1);
        assert_eq!(list_wiki(&pool, None, false).await?.as_array().unwrap().len(), 1);

        // include_archived=true still surfaces it, and it always resolves by id (nothing destroyed).
        assert_eq!(list_documents(&pool, None, None, None, None, None, true).await?.as_array().unwrap().len(), 2);
        assert_eq!(list_wiki(&pool, None, true).await?.as_array().unwrap().len(), 2);
        assert!(get_document(&pool, probe).await?["archived_at"].is_string());

        // Restore: back in the listings, stamp cleared.
        let restored = set_document_archived(&pool, probe, false, Some("concierge")).await?;
        assert!(restored["archived_at"].is_null(), "restore clears the stamp");
        assert_eq!(list_documents(&pool, None, None, None, None, None, false).await?.as_array().unwrap().len(), 2);
        assert_eq!(list_wiki(&pool, None, false).await?.as_array().unwrap().len(), 2);

        // Archiving a missing document is an error.
        assert!(set_document_archived(&pool, 999_999, true, None).await.is_err());
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
        let t = create_task(&pool, pid, "T", None, None, None, Some("owner"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();

        // Baseline: another agent's comment reaches the creator (they're in the fan-out).
        comment_task(&pool, tid, "hello", Some("bob"), None, None).await?;
        let n = check_notifications(&pool, "owner", true, 50, None).await?;
        assert_eq!(n["count"], json!(1), "creator hears the comment before muting");

        // Mute for the owner → subsequent task events no longer reach them.
        mute_task(&pool, "owner", tid).await?;
        comment_task(&pool, tid, "hello again", Some("bob"), None, None).await?;
        let n = check_notifications(&pool, "owner", true, 50, None).await?;
        assert_eq!(n["count"], json!(0), "muted creator gets no fan-out for the task");

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
            &pool, "Intro", None, "bafyA", None, Some("alice"), None, None, Some(content),
        )
        .await?;
        let aid = a["id"].as_i64().unwrap();
        set_document_path(&pool, aid, "guide/intro", None).await?;

        // outbound_links: guide/setup + guide/advanced, de-duped, embed excluded, ordered by path.
        let a_doc = get_document(&pool, aid).await?;
        let links = a_doc["outbound_links"].as_array().unwrap();
        assert_eq!(links.len(), 2, "two distinct links, embed excluded, dup collapsed: {links:?}");
        assert_eq!(links[0]["target_path"], json!("guide/advanced"));
        assert_eq!(links[0]["label"], json!("Advanced Guide"));
        assert!(links[0]["target_document_id"].is_null(), "advanced dangles (nothing filed there)");
        assert_eq!(links[1]["target_path"], json!("guide/setup"));
        assert!(links[1]["label"].is_null());
        assert!(links[1]["target_document_id"].is_null(), "setup dangles until a doc is filed there");

        // File a doc at guide/setup: A's link to it now resolves, and that doc sees the backlink.
        let b = create_document(&pool, "Setup", None, "bafyB", None, Some("bob"), None, None, None).await?;
        let bid = b["id"].as_i64().unwrap();
        set_document_path(&pool, bid, "guide/setup", None).await?;

        let a_doc = get_document(&pool, aid).await?;
        let setup = a_doc["outbound_links"].as_array().unwrap().iter()
            .find(|l| l["target_path"] == json!("guide/setup")).unwrap().clone();
        assert_eq!(setup["target_document_id"], json!(bid), "link resolves to the doc filed at that path");
        assert_eq!(setup["target_title"], json!("Setup"));

        let b_doc = get_document(&pool, bid).await?;
        let backlinks = b_doc["backlinks"].as_array().unwrap();
        assert_eq!(backlinks.len(), 1, "A backlinks to Setup");
        assert_eq!(backlinks[0]["id"], json!(aid));
        assert_eq!(backlinks[0]["path"], json!("guide/intro"));

        // A new version WITH content re-indexes edges (now only guide/setup).
        publish_version(&pool, aid, "bafyA2", Some("trim"), Some("alice"), None, Some("only [[guide/setup]] now")).await?;
        let a_doc = get_document(&pool, aid).await?;
        assert_eq!(a_doc["outbound_links"].as_array().unwrap().len(), 1, "edges refreshed from new content");

        // A CID-only publish (no content) leaves the edges as-is (board can't rescan a bare CID).
        publish_version(&pool, aid, "bafyA3", None, Some("alice"), None, None).await?;
        let a_doc = get_document(&pool, aid).await?;
        assert_eq!(a_doc["outbound_links"].as_array().unwrap().len(), 1, "CID-only publish keeps prior edges");

        // An unfiled doc (no path) has no backlinks even if others link to some path.
        let c = create_document(&pool, "Orphan", None, "bafyC", None, Some("carol"), None, None, Some("x")).await?;
        assert_eq!(get_document(&pool, c["id"].as_i64().unwrap()).await?["backlinks"].as_array().unwrap().len(), 0);
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
        let t = create_document(&pool, "Widget", None, "bafyW1", None, Some("bob"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();
        set_document_path(&pool, tid, "lib/widget", None).await?;
        let v2 = publish_version(&pool, tid, "bafyW2", Some("v2"), Some("bob"), None, None).await?;
        let v1_id = v2["versions"].as_array().unwrap().iter()
            .find(|v| v["version_no"] == json!(1)).unwrap()["id"].as_i64().unwrap();

        // Source doc: one plain link, one floating embed, one pinned embed, one region embed.
        let content = "[[lib/widget]] jump\n![[lib/widget]] float\n![[lib/widget@v1]] pinned\n![[lib/notes#intro]] region";
        let s = create_document(&pool, "Page", None, "bafyS", None, Some("alice"), None, None, Some(content)).await?;
        let sid = s["id"].as_i64().unwrap();

        let s_doc = get_document(&pool, sid).await?;
        // Links and embeds are separate tables, so [[lib/widget]] (a link) and ![[lib/widget]]
        // (an embed) to the SAME path are BOTH recorded (task 108 fix — the bug was the embed
        // being dropped). Within embeds, the float ![[lib/widget]] is seen before ![[lib/widget@v1]]
        // so it wins that path (one embed edge per path).
        let links = s_doc["outbound_links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["target_path"] == json!("lib/widget")), "the link is recorded");
        let embeds = s_doc["embeds"].as_array().unwrap();
        let w_emb = embeds.iter().find(|e| e["target_path"] == json!("lib/widget")).unwrap();
        assert!(w_emb["target_version_id"].is_null(), "float embed (![[lib/widget]]) wins over the later @v1; floats");
        assert_eq!(w_emb["target_document_id"], json!(tid), "embed resolves to the filed doc");
        let notes = embeds.iter().find(|e| e["target_path"] == json!("lib/notes")).unwrap();
        assert_eq!(notes["region"], json!("intro"), "region fragment captured");
        assert!(notes["target_document_id"].is_null(), "lib/notes dangles (unfiled)");

        // Now a doc where the embed path is distinct so pinning resolves to a version id.
        let content2 = "![[lib/widget@v1]] pinned\n![[lib/widget-x]] float-dangling";
        let s2 = create_document(&pool, "Page2", None, "bafyS2", None, Some("alice"), None, None, Some(content2)).await?;
        let s2_doc = get_document(&pool, s2["id"].as_i64().unwrap()).await?;
        let emb = s2_doc["embeds"].as_array().unwrap();
        let pinned = emb.iter().find(|e| e["target_path"] == json!("lib/widget")).unwrap();
        assert_eq!(pinned["target_version_id"], json!(v1_id), "@v1 pins to version 1's id");
        assert_eq!(pinned["target_document_id"], json!(tid), "resolved to the target doc");
        let floating = emb.iter().find(|e| e["target_path"] == json!("lib/widget-x")).unwrap();
        assert!(floating["target_version_id"].is_null(), "unpinned embed floats (null version)");

        // embedded_by: the Widget doc sees who embeds it. Page2 pins it; Page floats it.
        let t_doc = get_document(&pool, tid).await?;
        let emb_by = t_doc["embedded_by"].as_array().unwrap();
        assert!(emb_by.iter().any(|e| e["id"] == json!(s2["id"].as_i64().unwrap())), "Page2 embeds Widget");
        // The task-108 bug case: Page BOTH links and embeds lib/widget, so it must appear in
        // BOTH backlinks AND embedded_by (previously the embed was silently dropped).
        assert!(emb_by.iter().any(|e| e["id"] == json!(sid)), "Page embeds Widget (same path it also links)");
        assert!(t_doc["backlinks"].as_array().unwrap().iter().any(|b| b["id"] == json!(sid)), "Page links Widget");
        Ok(())
    }

    /// The from-session content read path (task #303): resolve_document_version picks the current
    /// or a named version's (version_no, cid, content_type), read_document_content requires an IPFS
    /// backend (so REST maps to 503 without one), and is_text_content_type classifies text vs binary.
    #[tokio::test]
    async fn read_document_content_resolves_version_and_needs_backend() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = create_document(&pool, "Spec", None, "bafycurrent", None, Some("alice"), None, Some("text/markdown"), None).await?;
        let did = d["id"].as_i64().unwrap();
        publish_version(&pool, did, "bafyv2", Some("s"), Some("alice"), Some("application/pdf"), None).await?;

        // The current version resolves to v2 + its cid/content_type; a named older version resolves too.
        let (vn, cid, ct) = resolve_document_version(&pool, did, None).await?;
        assert_eq!((vn, cid.as_str(), ct.as_str()), (2, "bafyv2", "application/pdf"));
        let (vn1, cid1, ct1) = resolve_document_version(&pool, did, Some(1)).await?;
        assert_eq!((vn1, cid1.as_str(), ct1.as_str()), (1, "bafycurrent", "text/markdown"));
        // A missing version or document errors.
        assert!(resolve_document_version(&pool, did, Some(99)).await.is_err());
        assert!(resolve_document_version(&pool, 9999, None).await.is_err());

        // With no IPFS backend, the read path errors with the backend-required message (REST -> 503).
        let err = read_document_content(&pool, None, did, None).await.unwrap_err().to_string();
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
        let d = create_document(&pool, "Spec", None, "bafycurrent", None, Some("alice"), None, Some("text/markdown"), None).await?;
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
        assert!(full["body"].is_null(), "body is null when it can't be fetched");
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
        let d = create_document(&pool, "Design: a very long working title", None, "bafy1", None, Some("alice"), None, None, None).await?;
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
            alice["notifications"].as_array().unwrap().iter().any(|n| n["type"] == json!("document.updated")
                && n["data"]["title"] == json!("Review entity")),
            "owner notified of rename: {alice}"
        );
        let bob = check_notifications(&pool, "bob", true, 50, None).await?;
        assert_eq!(bob["count"].as_i64(), Some(0), "renamer excluded from own event");

        // An empty title and an unknown document are rejected.
        assert!(update_document(&pool, did, "   ", Some("bob")).await.is_err());
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
        let d = create_document(&pool, "Spec", None, "bafy1", None, Some("alice"), None, None, None).await?;
        let did = d["id"].as_i64().unwrap();
        // bob explicitly subscribes to the document.
        subscribe(&pool, "bob", None, None, None, Some(did), false).await?;

        // carol publishes v2 -> author (alice) + subscriber (bob) hear it; carol (actor) does not.
        publish_version(&pool, did, "bafy2", Some("second"), Some("carol"), None, None).await?;

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
        assert_eq!(bob["notifications"][0]["type"], json!("document.version_published"));
        assert_eq!(bob["notifications"][0]["data"]["version_no"], json!(2));
        // carol is the actor -> excluded from her own event.
        assert_eq!(carol["count"].as_i64(), Some(0), "actor excluded: {carol}");

        // Unsubscribing stops delivery.
        unsubscribe(&pool, "bob", None, None, None, Some(did), false).await?;
        publish_version(&pool, did, "bafy3", None, Some("alice"), None, None).await?;
        let bob2 = check_notifications(&pool, "bob", true, 50, None).await?;
        assert_eq!(bob2["count"].as_i64(), Some(0), "no events after unsubscribe");

        Ok(())
    }

    /// A document's owner (creator) is notified when someone comments on it, even if the owner
    /// holds no subscription row — the owner is included explicitly, like a task's created_by, so
    /// a comment always reaches the person who should respond. (operator #30 seq-2746 / task #300.)
    #[tokio::test]
    async fn document_owner_notified_on_comment_without_subscription() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = create_document(&pool, "Spec", None, "bafy1", None, Some("alice"), None, None, None).await?;
        let did = d["id"].as_i64().unwrap();

        // Remove the owner's auto-subscription, so the ONLY way alice can hear the comment is via
        // the explicit owner inclusion (this is the gap the fix closes).
        unsubscribe(&pool, "alice", None, None, None, Some(did), false).await?;

        // bob comments on alice's doc.
        comment_document(&pool, did, None, Some("bob"), "please clarify §2", None, None, None).await?;

        // alice (the owner) is still notified, despite having no subscription row.
        let alice = check_notifications(&pool, "alice", true, 50, None).await?;
        assert_eq!(alice["count"].as_i64(), Some(1), "owner notified without a subscription: {alice}");
        let n = &alice["notifications"][0];
        assert_eq!(n["type"], json!("document.comment"));
        // The payload is self-describing — which document, its title — and names the commenter as
        // the event actor, so the owner can act without a lookup (task #300 + #313).
        assert_eq!(n["data"]["document_id"], json!(did));
        assert_eq!(n["data"]["title"], json!("Spec"));
        assert_eq!(n["actor"], json!("bob"), "the commenter is surfaced as the event actor");

        // The commenter (actor) is not notified of their own comment.
        let bob = check_notifications(&pool, "bob", true, 50, None).await?;
        assert_eq!(bob["count"].as_i64(), Some(0), "actor excluded from own comment: {bob}");
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
        let d = create_document(&pool, "Design X", None, "bafy1", None, Some("alice"), None, None, None).await?;
        let did = d["id"].as_i64().unwrap();

        // bob (a reviewer) approves alice's document.
        approve_document(&pool, did, Some("bob")).await?;

        let alice = check_notifications(&pool, "alice", true, 50, None).await?;
        let n = &alice["notifications"][0];
        assert_eq!(n["type"], json!("document.approved"));
        assert_eq!(n["actor"], json!("bob"), "the approver is surfaced as the event actor");
        assert_eq!(n["data"]["document_id"], json!(did));
        assert_eq!(n["data"]["title"], json!("Design X"));
        assert_eq!(n["data"]["status"], json!("approved"));
        // The approver is not notified of their own action.
        let bob = check_notifications(&pool, "bob", true, 50, None).await?;
        assert_eq!(bob["count"].as_i64(), Some(0), "approver excluded from own event");

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
        let d = create_document(&pool, "Spec", None, "bafy1", None, Some("alice"), None, None, None).await?;
        let did = d["id"].as_i64().unwrap();
        let vid = d["current_version"]["id"].as_i64().unwrap();

        // bob subscribes so he hears comment activity.
        subscribe(&pool, "bob", None, None, None, Some(did), false).await?;

        // A region-anchored comment (carol). The region JSON is stored verbatim.
        let region = json!({
            "TextQuoteSelector": { "exact": "widgets", "prefix": "the ", "suffix": " are" },
            "TextPositionSelector": { "start": 10, "end": 17 }
        });
        let c =
            comment_document(&pool, did, Some(vid), Some("carol"), "typo", Some(region.clone()), None, None)
                .await?;
        let cid = c["id"].as_i64().unwrap();
        assert_eq!(c["status"], json!("open"));
        assert_eq!(c["region"], region, "region round-trips as JSON");
        assert_eq!(c["version_id"], json!(vid));

        // A doc-level comment (no region), threaded under the first.
        let c2 = comment_document(&pool, did, None, Some("dave"), "agreed", None, Some(cid), None).await?;
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
        assert_eq!(get_document_comments(&pool, did, None, None).await?.as_array().unwrap().len(), 2);
        assert_eq!(
            get_document_comments(&pool, did, Some(vid), None).await?.as_array().unwrap().len(),
            1,
            "only the region comment carries this version_id"
        );

        // Resolve flips status and is filterable.
        let r = resolve_comment(&pool, cid, Some("alice")).await?;
        assert_eq!(r["status"], json!("resolved"));
        assert_eq!(
            get_document_comments(&pool, did, None, Some("open")).await?.as_array().unwrap().len(),
            1
        );
        assert_eq!(
            get_document_comments(&pool, did, None, Some("resolved")).await?.as_array().unwrap().len(),
            1
        );

        // An ingested comment attributed to an external human (§6): author stays the ingester,
        // external_author carries the identity, and it round-trips through get_document_comments.
        let c3 = comment_document(&pool, did, None, Some("slack-bridge"), "from ada", None, None, Some("slack:U1")).await?;
        assert_eq!(c3["author"], json!("slack-bridge"));
        assert_eq!(c3["external_author"], json!("slack:U1"));
        let listed = get_document_comments(&pool, did, None, None).await?;
        let c3_listed = listed.as_array().unwrap().iter().find(|x| x["id"] == c3["id"]).unwrap();
        assert_eq!(c3_listed["external_author"], json!("slack:U1"), "attribution surfaces in the list");

        // Errors: comment on a missing doc, resolve a missing comment.
        assert!(comment_document(&pool, 999, None, Some("x"), "hi", None, None, None).await.is_err());
        assert!(resolve_comment(&pool, 999, Some("x")).await.is_err());
        Ok(())
    }

    /// The review loop: submit -> request_changes -> publish (reopens) -> approve stamps the
    /// current version -> publishing again reopens review. Each transition notifies subscribers.
    #[tokio::test]
    async fn document_review_workflow() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = create_document(&pool, "Design", None, "bafy1", None, Some("alice"), None, None, None).await?;
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
        let v2 = publish_version(&pool, did, "bafy2", Some("addressed"), Some("alice"), None, None).await?;
        assert_eq!(v2["status"], json!("in_review"), "a new version reopens review");
        let v2id = v2["current_version"]["id"].as_i64().unwrap();

        // Operator approves -> stamps the current version.
        let ap = approve_document(&pool, did, Some("operator")).await?;
        assert_eq!(ap["status"], json!("approved"));
        assert_eq!(ap["approved_version_id"], json!(v2id), "approval stamps the current version");
        assert_eq!(ap["approved_by"], json!("operator"));

        // Approval is a stamp, not a lock: publishing again reopens review but keeps the stamp.
        let v3 = publish_version(&pool, did, "bafy3", None, Some("alice"), None, None).await?;
        assert_eq!(v3["status"], json!("in_review"));
        assert_eq!(v3["approved_version_id"], json!(v2id), "stamp persists across a new version");

        // The operator (a subscriber) heard the author-driven transitions (submit, both publishes)
        // but not their own request_changes/approve (actor excluded).
        let ops = check_notifications(&pool, "operator", true, 50, None).await?;
        let types: Vec<String> = ops["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap().to_string())
            .collect();
        assert!(types.contains(&"document.submitted_for_review".to_string()), "{types:?}");
        assert!(types.contains(&"document.version_published".to_string()), "{types:?}");
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
        let t = create_task(&pool, pid, "Build widget", None, Some("alice"), None, Some("alice"), None, None, None)
            .await?;
        let tid = t["id"].as_i64().unwrap();
        let d = create_document(&pool, "Widget design", None, "bafy1", None, Some("bob"), None, None, None).await?;
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
        assert!(task["attached_documents"][0].as_object().unwrap().contains_key("project_id"));
        assert!(task["attached_documents"][0]["project_id"].is_null());

        // Idempotent: re-attaching doesn't duplicate.
        attach_document(&pool, did, tid, Some("carol")).await?;
        assert_eq!(
            get_task(&pool, tid).await?["attached_documents"].as_array().unwrap().len(),
            1
        );

        // Both the task owner (alice, subscribed as assignee/creator) and the doc author (bob,
        // subscribed on create) heard document.attached; carol (actor) did not.
        let alice = check_notifications(&pool, "alice", true, 50, None).await?;
        let bob = check_notifications(&pool, "bob", true, 50, None).await?;
        assert!(alice["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["type"] == json!("document.attached")), "task watcher heard it: {alice}");
        assert!(bob["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["type"] == json!("document.attached")), "doc watcher heard it: {bob}");

        // Detach removes the link and reports the removal.
        let rm = detach_document(&pool, did, tid, Some("carol")).await?;
        assert_eq!(rm["removed"], json!(1));
        assert_eq!(get_task(&pool, tid).await?["attached_documents"].as_array().unwrap().len(), 0);
        // Detaching again is a no-op (0 removed).
        assert_eq!(detach_document(&pool, did, tid, Some("carol")).await?["removed"], json!(0));

        // Attaching to a missing task or doc errors.
        assert!(attach_document(&pool, did, 9999, Some("carol")).await.is_err());
        assert!(attach_document(&pool, 9999, tid, Some("carol")).await.is_err());
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
            &pool, "A", Some(pid), "bafyA", None, Some("alice"),
            Some(json!({ "tags": ["design", "rfc"] })), None, None,
        ).await?;
        let aid = a["id"].as_i64().unwrap();
        let b = create_document(
            &pool, "B", None, "bafyB", None, Some("bob"), Some(json!({ "tags": ["ops"] })), None, None,
        ).await?;
        let bid = b["id"].as_i64().unwrap();
        let c = create_document(&pool, "C", Some(pid), "bafyC", None, Some("alice"), None, None, None).await?;
        let cid = c["id"].as_i64().unwrap();
        approve_document(&pool, cid, Some("op")).await?;

        // Attach A to a task.
        let t = create_task(&pool, pid, "T", None, None, None, Some("u"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();
        attach_document(&pool, aid, tid, Some("u")).await?;

        let ids = |v: &Value| -> Vec<i64> {
            v.as_array().unwrap().iter().map(|d| d["id"].as_i64().unwrap()).collect()
        };

        // author
        assert_eq!(ids(&list_documents(&pool, None, None, None, None, Some("alice"), false).await?), vec![aid, cid]);
        assert_eq!(ids(&list_documents(&pool, None, None, None, None, Some("bob"), false).await?), vec![bid]);
        // tag
        assert_eq!(ids(&list_documents(&pool, None, None, Some("design"), None, None, false).await?), vec![aid]);
        assert_eq!(ids(&list_documents(&pool, None, None, Some("ops"), None, None, false).await?), vec![bid]);
        assert!(list_documents(&pool, None, None, Some("nope"), None, None, false).await?.as_array().unwrap().is_empty());
        // task attachment
        assert_eq!(ids(&list_documents(&pool, None, None, None, Some(tid), None, false).await?), vec![aid]);
        // project
        assert_eq!(ids(&list_documents(&pool, Some(pid), None, None, None, None, false).await?), vec![aid, cid]);
        // status
        assert_eq!(ids(&list_documents(&pool, None, Some("approved"), None, None, None, false).await?), vec![cid]);
        // combined AND: project + tag rfc + author alice -> only A
        assert_eq!(
            ids(&list_documents(&pool, Some(pid), None, Some("rfc"), None, Some("alice"), false).await?),
            vec![aid]
        );
        // contradictory combo -> empty
        assert!(list_documents(&pool, None, None, Some("ops"), None, Some("alice"), false).await?.as_array().unwrap().is_empty());
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
            v.as_array().unwrap().iter().map(|t| t["id"].as_i64().unwrap()).collect()
        };

        // An epic with two children.
        let epic = create_task(&pool, pid, "Epic", None, None, None, Some("u"), None, None, None).await?;
        let eid = epic["id"].as_i64().unwrap();
        let c1 = create_task(&pool, pid, "c1", None, None, None, Some("u"), None, Some(eid), None).await?;
        let c1id = c1["id"].as_i64().unwrap();
        let c2 = create_task(&pool, pid, "c2", None, None, None, Some("u"), None, Some(eid), None).await?;
        let c2id = c2["id"].as_i64().unwrap();

        // Cross-project parent rejected at create.
        assert!(create_task(&pool, pid2, "x", None, None, None, Some("u"), None, Some(eid), None).await.is_err());
        // Non-existent parent rejected.
        assert!(create_task(&pool, pid, "y", None, None, None, Some("u"), None, Some(99999), None).await.is_err());

        // get_task: children + roll-up.
        let e = get_task(&pool, eid).await?;
        assert_eq!(ids(&e["children"]), vec![c1id, c2id]);
        assert_eq!(e["child_rollup"], json!({ "done": 0, "total": 2 }));

        // Mark c1 done -> roll-up 1/2.
        update_task(&pool, c1id, Some("done"), None, None, None, None, Some("u"), None, None, None).await?;
        assert_eq!(get_task(&pool, eid).await?["child_rollup"], json!({ "done": 1, "total": 2 }));

        // Child surfaces parent_id + parent_title.
        let c = get_task(&pool, c1id).await?;
        assert_eq!(c["parent_id"], json!(eid));
        assert_eq!(c["parent_title"], json!("Epic"));

        // list_tasks top_level -> only the epic; parent_id -> the two children.
        assert_eq!(ids(&list_tasks(&pool, Some(pid), None, None, false, None, true, None, None, None, None, None, false).await?), vec![eid]);
        assert_eq!(
            ids(&list_tasks(&pool, Some(pid), None, None, false, Some(eid), false, None, None, None, None, None, false).await?),
            vec![c1id, c2id]
        );

        // Guards: self-parent + cycle rejected.
        assert!(update_task(&pool, eid, None, None, None, None, None, Some("u"), None, Some(eid), None).await.is_err());
        assert!(update_task(&pool, eid, None, None, None, None, None, Some("u"), None, Some(c1id), None).await.is_err());

        // Clear c2's parent (parent_id=0) -> top-level; emits task.reparented; roll-up shrinks.
        let r = update_task(&pool, c2id, None, None, None, None, None, Some("u"), None, Some(0), None).await?;
        assert!(r["parent_id"].is_null());
        assert_eq!(get_task(&pool, eid).await?["child_rollup"], json!({ "done": 1, "total": 1 }));
        let evs = get_events(&pool, 0, 200, None, false).await?;
        assert!(evs.as_array().unwrap().iter().any(|e| e["type"] == json!("task.reparented")));

        // move_task guard: the epic still has a child -> cannot cross projects.
        assert!(move_task(&pool, eid, pid2, Some("u")).await.is_err());
        // c2 is now top-level with no children -> it can move.
        assert_eq!(move_task(&pool, c2id, pid2, Some("u")).await?["project_id"], json!(pid2));
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

        create_task(&pool, pid1, "Fix the widget pipeline", Some("handles reflow"), None, None, Some("u"), None, None, None).await?;
        create_task(&pool, pid1, "Unrelated chore", None, None, None, Some("u"), None, None, None).await?;
        create_task(&pool, pid2, "Widget docs", Some("describe the WIDGET api"), Some("alice"), None, Some("u"), None, None, None).await?;

        let titles = |v: &Value| -> Vec<String> {
            let mut t: Vec<String> =
                v.as_array().unwrap().iter().map(|x| x["title"].as_str().unwrap().to_string()).collect();
            t.sort();
            t
        };

        // "widget" across ALL projects (case-insensitive) -> the two widget tasks, not the chore.
        assert_eq!(
            titles(&list_tasks(&pool, None, None, None, false, None, false, Some("widget"), None, None, None, None, false).await?),
            vec!["Fix the widget pipeline".to_string(), "Widget docs".to_string()]
        );
        // Matches description too.
        assert_eq!(
            titles(&list_tasks(&pool, None, None, None, false, None, false, Some("reflow"), None, None, None, None, false).await?),
            vec!["Fix the widget pipeline".to_string()]
        );
        // Composable with assignee: widget + alice -> only the Beta doc task.
        assert_eq!(
            titles(&list_tasks(&pool, None, None, Some("alice"), false, None, false, Some("widget"), None, None, None, None, false).await?),
            vec!["Widget docs".to_string()]
        );
        // Composable with project scope: widget in Alpha -> only the pipeline task.
        assert_eq!(
            titles(&list_tasks(&pool, Some(pid1), None, None, false, None, false, Some("widget"), None, None, None, None, false).await?),
            vec!["Fix the widget pipeline".to_string()]
        );
        // No match -> empty.
        assert!(list_tasks(&pool, None, None, None, false, None, false, Some("zzznope"), None, None, None, None, false)
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
        let t = create_task(&pool, pid, "T", None, None, None, None, Some(json!({"a": 1})), None, None).await?;
        let tid = t["id"].as_i64().unwrap();

        set_task_props(&pool, tid, json!({"b": 2})).await?;
        update_task(&pool, tid, None, None, None, None, None, None, Some(json!({"c": 3})), None, None).await?;

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
        let a = register_agent(&pool, "v-x", None, None, None, Some(json!({"effort": "high"})), None).await?;
        assert_eq!(a["metadata"], json!({"role": "vertical", "model": "opus", "effort": "high"}));
        assert_eq!(a["charter"], "own X");

        // update_agent: away without a message keeps status_message; metadata merges again.
        update_agent(&pool, "v-x", None, None, None, Some("away"), None, None, Some(json!({"branch": "main"})), None).await?;
        let got = get_agent(&pool, "v-x").await?;
        assert_eq!(got["status"], "away");
        assert_eq!(
            got["metadata"],
            json!({"role": "vertical", "model": "opus", "effort": "high", "branch": "main"})
        );

        // clear affordance (task 489): set then CLEAR webhook_url to null in one call. A null/omitted
        // field would leave it unchanged, so clear is the only way to empty it.
        update_agent(&pool, "v-x", None, None, None, None, None, Some("http://x/wake"), None, None).await?;
        assert_eq!(get_agent(&pool, "v-x").await?["webhook_url"], json!("http://x/wake"));
        update_agent(&pool, "v-x", None, None, None, None, None, None, None, Some(&["webhook_url".to_string()])).await?;
        assert!(get_agent(&pool, "v-x").await?["webhook_url"].is_null(), "clear empties the field");
        // An explicit value wins over clearing the same field in one call.
        update_agent(&pool, "v-x", None, None, None, None, None, Some("http://y/wake"), None, Some(&["webhook_url".to_string()])).await?;
        assert_eq!(get_agent(&pool, "v-x").await?["webhook_url"], json!("http://y/wake"), "explicit value wins over clear");
        // A non-clearable field name is rejected.
        assert!(update_agent(&pool, "v-x", None, None, None, None, None, None, None, Some(&["status".to_string()])).await.is_err());

        // update_agent on an unknown agent errors (it's a mutate, not an upsert).
        assert!(update_agent(&pool, "nope", None, None, None, None, None, None, None, None).await.is_err());

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
        register_agent(&pool, "v-compiler", Some("Compiler"), Some("vertical"), Some(&big_charter), Some(json!({"area":"compiler"})), None).await?;
        register_agent(&pool, "v-runtime", Some("Runtime"), Some("vertical"), Some(&big_charter), Some(json!({"area":"runtime"})), None).await?;
        register_agent(&pool, "concierge", Some("Concierge"), Some("ops"), Some(&big_charter), Some(json!({"area":"ops"})), None).await?;
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
            assert!(a.get("charter").is_none(), "roster must omit the heavy charter: {a}");
            assert!(a.get("metadata").is_some(), "roster must include metadata for filtering: {a}");
        }
        // The metadata bag is the real object, so a consumer can filter on it (e.g. metadata.area).
        let compiler = arr.iter().find(|a| a["id"] == json!("v-compiler")).unwrap();
        assert_eq!(compiler["metadata"]["area"], json!("compiler"));

        // verbose -> full objects (charter present).
        let full = list_agents(&pool, None, None, None, None, true, None, None).await?;
        assert!(full.as_array().unwrap().iter().all(|a| a["charter"].is_string()));

        // status filter.
        let online = list_agents(&pool, Some("online"), None, None, None, false, None, None).await?;
        let online = online.as_array().unwrap();
        assert_eq!(online.len(), 1);
        assert_eq!(online[0]["id"], json!("v-compiler"));

        // q substring over id + display_name.
        let q = list_agents(&pool, None, Some("runtime"), None, None, false, None, None).await?;
        assert_eq!(q.as_array().unwrap().len(), 1);
        assert_eq!(q.as_array().unwrap()[0]["id"], json!("v-runtime"));

        // meta_key/meta_value routing filter (the v-cadenza-ci case: find the owning area).
        let by_area = list_agents(&pool, None, None, Some("area"), Some("compiler"), false, None, None).await?;
        assert_eq!(by_area.as_array().unwrap().len(), 1);
        assert_eq!(by_area.as_array().unwrap()[0]["id"], json!("v-compiler"));

        // limit + offset paginate.
        let page1 = list_agents(&pool, None, None, None, None, false, Some(2), Some(0)).await?;
        let page2 = list_agents(&pool, None, None, None, None, false, Some(2), Some(2)).await?;
        assert_eq!(page1.as_array().unwrap().len(), 2);
        assert_eq!(page2.as_array().unwrap().len(), 1);
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
        for (name, t) in [("Backend", "2026-01-01T00:00:00Z"), ("backend", "2026-02-01T00:00:00Z")] {
            sqlx::query("INSERT INTO projects(name, created_at, updated_at) VALUES(?,?,?)")
                .bind(name)
                .bind(t)
                .bind(&ts)
                .execute(&pool)
                .await?;
        }
        // ids: 1 = "Backend" (earlier), 2 = "backend" (later).
        create_task(&pool, 2, "on dupe", None, None, None, Some("u"), None, None, None).await?;
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
        let tasks = list_tasks(&pool, Some(1), None, None, false, None, false, None, None, None, None, None, false).await?;
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

        let p = create_project(&pool, "Alpha", None, Some("u"), Some(json!({"repo": "r1"}))).await?;
        let pid = p["id"].as_i64().unwrap();
        // create returns metadata parsed as an object, not a JSON string.
        assert_eq!(p["metadata"], json!({"repo": "r1"}));

        // Rename + add a metadata key (existing keys preserved = merge, not replace).
        let up = update_project(&pool, pid, Some("Alpha Prime"), None, None, Some(json!({"lang": "rust"})), Some("u")).await?;
        assert_eq!(up["name"], json!("Alpha Prime"));
        assert_eq!(up["metadata"], json!({"repo": "r1", "lang": "rust"}));

        // Archive it: it drops out of the active-filtered list but is still there.
        update_project(&pool, pid, None, None, Some("archived"), None, Some("u")).await?;
        let active = list_projects(&pool, Some("active")).await?;
        assert_eq!(active.as_array().unwrap().len(), 0, "archived project hidden from active list");
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
        let t = create_task(&pool, aid, "T", None, None, None, Some("u"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();

        let moved = move_task(&pool, tid, bid, Some("u")).await?;
        assert_eq!(moved["project_id"], json!(bid));
        // It now lists under B, not A.
        assert_eq!(list_tasks(&pool, Some(aid), None, None, false, None, false, None, None, None, None, None, false).await?.as_array().unwrap().len(), 0);
        assert_eq!(list_tasks(&pool, Some(bid), None, None, false, None, false, None, None, None, None, None, false).await?.as_array().unwrap().len(), 1);

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
        let before = get_events(&pool, 0, 100, None, false).await?.as_array().unwrap().len();
        move_task(&pool, tid, bid, Some("u")).await?;
        let after = get_events(&pool, 0, 100, None, false).await?.as_array().unwrap().len();
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
        create_task(&pool, pid, "owned", None, Some("alice"), None, Some("u"), None, None, None).await?;
        create_task(&pool, pid, "free", None, None, None, Some("u"), None, None, None).await?;

        // unassigned=true -> only the ownerless task.
        let un = list_tasks(&pool, Some(pid), None, None, true, None, false, None, None, None, None, None, false).await?;
        let un = un.as_array().unwrap();
        assert_eq!(un.len(), 1);
        assert_eq!(un[0]["title"], json!("free"));
        assert!(un[0]["assignee"].is_null());

        // assignee equality still works when unassigned is false.
        let mine = list_tasks(&pool, Some(pid), None, Some("alice"), false, None, false, None, None, None, None, None, false).await?;
        let mine = mine.as_array().unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["title"], json!("owned"));

        // unassigned=true wins over a contradictory assignee= filter (no owner beats owner=alice).
        let both = list_tasks(&pool, Some(pid), None, Some("alice"), true, None, false, None, None, None, None, None, false).await?;
        let both = both.as_array().unwrap();
        assert_eq!(both.len(), 1);
        assert_eq!(both[0]["title"], json!("free"));

        // No filter returns both.
        assert_eq!(
            list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, None, None, false).await?.as_array().unwrap().len(),
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
        create_task(&pool, pid, "obs-a1", None, None, None, Some("u"), Some(json!({"observes": "widget-a"})), None, None).await?;
        create_task(&pool, pid, "obs-a2", None, None, None, Some("u"), Some(json!({"observes": "widget-a"})), None, None).await?;
        create_task(&pool, pid, "obs-b", None, None, None, Some("u"), Some(json!({"observes": "widget-b"})), None, None).await?;
        create_task(&pool, pid, "plain", None, None, None, Some("u"), None, None, None).await?;

        let a = list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, Some("observes"), Some("widget-a"), false).await?;
        let titles: Vec<_> = a.as_array().unwrap().iter().map(|t| t["title"].as_str().unwrap().to_string()).collect();
        assert_eq!(titles, vec!["obs-a1", "obs-a2"]);

        // Composes with a status filter: no open task observes widget-b once it's marked done.
        let bid = list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, Some("observes"), Some("widget-b"), false)
            .await?[0]["id"]
            .as_i64()
            .unwrap();
        update_task(&pool, bid, Some("done"), None, None, None, None, Some("u"), None, None, None).await?;
        let open_b = list_tasks(&pool, Some(pid), Some("todo"), None, false, None, false, None, None, None, Some("observes"), Some("widget-b"), false).await?;
        assert_eq!(open_b.as_array().unwrap().len(), 0, "no OPEN task observes widget-b after it's done");

        // A key with no matching value returns nothing; only meta_key (no value) does not filter.
        let none = list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, Some("observes"), Some("nope"), false).await?;
        assert_eq!(none.as_array().unwrap().len(), 0);
        let unfiltered = list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, Some("observes"), None, false).await?;
        assert_eq!(unfiltered.as_array().unwrap().len(), 4, "meta_key without meta_value is inert");
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
        let keep = create_task(&pool, pid, "keep", None, None, None, Some("owner"), None, None, None).await?;
        let retire = create_task(&pool, pid, "retire", None, None, None, Some("owner"), None, None, None).await?;
        let retire_id = retire["id"].as_i64().unwrap();
        let _ = keep;

        // Both visible before archiving.
        let before = list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, None, None, false).await?;
        assert_eq!(before.as_array().unwrap().len(), 2);

        // Archive one: the default view drops it, but include_archived still lists it.
        let archived = set_task_archived(&pool, retire_id, true, Some("owner")).await?;
        assert!(archived["archived_at"].is_string(), "archived_at stamped: {archived}");
        let default_view = list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, None, None, false).await?;
        let titles: Vec<_> = default_view.as_array().unwrap().iter().map(|t| t["title"].as_str().unwrap().to_string()).collect();
        assert_eq!(titles, vec!["keep"], "archived task hidden by default");
        let with_archived = list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, None, None, true).await?;
        assert_eq!(with_archived.as_array().unwrap().len(), 2, "include_archived lists it");
        // Still fetchable by id.
        assert_eq!(get_task(&pool, retire_id).await?["title"].as_str(), Some("retire"));

        // Restore: reappears in the default view, stamp cleared.
        let restored = set_task_archived(&pool, retire_id, false, Some("owner")).await?;
        assert!(restored["archived_at"].is_null(), "archived_at cleared: {restored}");
        let after = list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, None, None, false).await?;
        assert_eq!(after.as_array().unwrap().len(), 2, "restored task back in default view");

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
        assert!(req["submit_url"].as_str().unwrap().contains(&format!("/secret-requests/{id}?t=")));
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
        assert!(submitted.get("ciphertext").is_none(), "submit metadata hides ciphertext");
        let notif = check_notifications(&pool, "green-machine-ops", true, 50, None).await?;
        let types: Vec<String> = notif["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap().to_string())
            .collect();
        assert!(types.contains(&"secret.submitted".to_string()), "fulfiller notified: {types:?}");

        // The submit link is single-use: a second submit is rejected.
        assert!(submit_secret(&pool, id, &submit_token, "AGAIN").await.is_err());

        // The fulfiller pulls the ciphertext (token-gated); a wrong token is rejected.
        assert!(get_secret_ciphertext(&pool, id, "wrong-token").await.is_err());
        let pulled = get_secret_ciphertext(&pool, id, &fulfiller_token).await?;
        assert_eq!(pulled["ciphertext"], "AGE-CIPHERTEXT-BLOB");

        // Fulfill deletes the row; a second fulfill (row gone) is idempotent.
        let done = fulfill_secret(&pool, id, &fulfiller_token).await?;
        assert_eq!(done["fulfilled"], true);
        assert!(get_secret_request(&pool, id).await.is_err(), "row deleted after fulfill");
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
        assert!(scan_banned_phrases(&pool, "anything at all").await?.is_empty());
        check_banned_phrases(&pool, "anything at all", false).await?;

        // Add two phrases (stored lowercased; idempotent + note-updating on re-add).
        add_banned_phrase(&pool, "The Floor", Some("jargon"), Some("librarian")).await?;
        add_banned_phrase(&pool, "floored", None, Some("librarian")).await?;
        add_banned_phrase(&pool, "the floor", Some("still jargon"), Some("librarian")).await?; // dup -> update
        let list = list_banned_phrases(&pool).await?;
        assert_eq!(list.as_array().unwrap().len(), 2, "dup add did not grow the list: {list}");

        // Case-insensitive, whole-phrase match; the term inside a larger word does NOT match.
        assert_eq!(scan_banned_phrases(&pool, "we hit THE FLOOR today").await?, vec!["the floor"]);
        assert_eq!(scan_banned_phrases(&pool, "I am floored.").await?, vec!["floored"]);
        assert!(scan_banned_phrases(&pool, "the floorboard creaks").await?.is_empty(), "whole-word only");
        assert!(scan_banned_phrases(&pool, "no jargon here").await?.is_empty());

        // check bails on a hit, unless acknowledged.
        let err = check_banned_phrases(&pool, "down to the floor", false).await.unwrap_err().to_string();
        assert!(err.starts_with("banned phrase"), "got: {err}");
        assert!(err.contains("the floor"), "names the phrase: {err}");
        check_banned_phrases(&pool, "down to the floor", true).await?; // acknowledged -> passes

        // Remove one; it stops matching and the list shrinks.
        let r = remove_banned_phrase(&pool, "THE FLOOR").await?;
        assert_eq!(r["deleted"], json!(true));
        assert!(scan_banned_phrases(&pool, "we hit the floor").await?.is_empty());
        assert_eq!(list_banned_phrases(&pool).await?.as_array().unwrap().len(), 1);
        assert_eq!(remove_banned_phrase(&pool, "the floor").await?["deleted"], json!(false), "already gone");
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
        let e = check_non_ascii("line one\nsecond \u{2014} dash", false).unwrap_err().to_string();
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
        let e = create_document(&pool, "Bad \u{2014} title", None, "Qmcid", None, Some("u"), None, None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("non-ASCII"), "create rejects non-ASCII title: {e}");
        // An ASCII create succeeds; renaming to a non-ASCII title is rejected; ASCII rename is fine.
        let d = create_document(&pool, "Good title", None, "Qmcid", None, Some("u"), None, None, None).await?;
        let id = d["id"].as_i64().unwrap();
        assert!(update_document(&pool, id, "Renamed \u{2194} bad", Some("u")).await.is_err());
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
        let t = create_task(&pool, pid, "T", None, None, None, Some("owner"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();
        // owner comments mentioning @alice (registered) and @nobody (not registered).
        comment_task(&pool, tid, "hey @alice and @nobody take a look", Some("owner"), None, None).await?;
        let task = get_task(&pool, tid).await?;
        let subs: Vec<&str> =
            task["subscribers"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert!(subs.contains(&"alice"), "mentioned registered agent subscribed: {subs:?}");
        assert!(!subs.contains(&"nobody"), "unregistered @token ignored: {subs:?}");
        Ok(())
    }

    #[test]
    fn extract_mentions_parses_at_tokens() {
        assert_eq!(
            extract_mentions("hi @v-task-board and @board-pm, cc @alice_1"),
            vec!["v-task-board", "board-pm", "alice_1"]
        );
        assert!(extract_mentions("no mentions here").is_empty());
        assert_eq!(extract_mentions("email a@b.com is not a mention start"), vec!["b"]);
    }

    /// #476: register_agent/update_agent coerce a hand-authored metadata.repos (a CSV/space/newline
    /// string, or a list of bare names) into the structured [{"repo": name}] form fleet spin-up
    /// expects; an already-structured list is left as-is; other metadata keys are untouched.
    #[test]
    fn coerce_repos_metadata_normalizes_unstructured_forms() {
        assert_eq!(
            coerce_repos_metadata(Some(&json!("Membrain, MembrainCDK\nElasticShuffleCDK"))),
            Some(json!([{ "repo": "Membrain" }, { "repo": "MembrainCDK" }, { "repo": "ElasticShuffleCDK" }]))
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
            v["members"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect()
        };

        // Before enabling, a and b (registered before the channel existed) are not members.
        assert!(!members(&get_channel(&pool, cid).await?).contains(&"a".to_string()));

        // Enable: backfill joins every registered agent.
        set_channel_auto_join(&pool, cid, true, Some("owner")).await?;
        let m = members(&get_channel(&pool, cid).await?);
        assert!(m.contains(&"a".to_string()) && m.contains(&"b".to_string()), "backfilled: {m:?}");

        // A newly-registered agent auto-joins.
        register_agent(&pool, "c", None, None, None, None, None).await?;
        assert!(members(&get_channel(&pool, cid).await?).contains(&"c".to_string()), "c auto-joined");

        // Disabling stops future auto-joins (existing members stay).
        set_channel_auto_join(&pool, cid, false, Some("owner")).await?;
        register_agent(&pool, "d", None, None, None, None, None).await?;
        let m2 = members(&get_channel(&pool, cid).await?);
        assert!(!m2.contains(&"d".to_string()), "d did not auto-join after disable: {m2:?}");
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
        let t = create_task(&pool, pid, "T", None, Some("alice"), None, Some("u"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();

        // Clear the owner.
        let cleared =
            update_task(&pool, tid, None, Some(""), None, None, None, Some("u"), None, None, None).await?;
        assert!(cleared["assignee"].is_null(), "assignee should be NULL after unassign");

        let events = get_events(&pool, 0, 100, None, false).await?;
        let un = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == json!("task.unassigned"))
            .expect("task.unassigned emitted");
        assert_eq!(un["data"]["from"], json!("alice"), "carries the prior owner");

        // Clearing an already-unassigned task does NOT emit a second task.unassigned.
        update_task(&pool, tid, None, Some(""), None, None, None, Some("u"), None, None, None).await?;
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
        update_task(&pool, tid, None, Some("bob"), None, None, None, Some("u"), None, None, None).await?;
        let evs = get_events(&pool, 0, 200, None, false).await?;
        assert!(
            evs.as_array().unwrap().iter().any(
                |e| e["type"] == json!("task.assigned") && e["data"]["assignee"] == json!("bob")
            ),
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
        let t = create_task(&pool, aid, "T", None, None, None, Some("u"), None, None, None).await?;
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
        update_project(&pool, 1, None, None, None, Some(json!({"repo": "r"})), Some("u")).await?;
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
            .as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert_eq!(members, ["alice", "bob"].iter().map(|s| s.to_string()).collect());

        // carol joins explicitly, then alice posts. bob + carol hear it; alice (poster) doesn't.
        subscribe(&pool, "carol", None, None, Some(cid), None, false).await?;
        let posted = post_to_channel(&pool, cid, "alice", "hello all", None, None).await?;
        let post_seq = posted["seq"].as_i64().unwrap();

        let bob = check_notifications(&pool, "bob", true, 50, None).await?;
        let carol = check_notifications(&pool, "carol", true, 50, None).await?;
        let alice = check_notifications(&pool, "alice", true, 50, None).await?;
        assert_eq!(bob["count"].as_i64(), Some(1), "bob hears the post: {bob}");
        assert_eq!(carol["count"].as_i64(), Some(1), "carol hears the post: {carol}");
        assert_eq!(alice["count"].as_i64(), Some(0), "poster isn't self-notified: {alice}");
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
        assert_eq!(m2["channel_id"].as_i64(), Some(cid), "A->B and B->A share a channel");

        // bob hears alice's message (1st DM), alice hears bob's (2nd) — each as message.direct,
        // never their own.
        let bob = check_notifications(&pool, "bob", true, 50, Some("message.direct")).await?;
        assert_eq!(bob["count"].as_i64(), Some(1), "bob: {bob}");
        assert_eq!(bob["notifications"][0]["data"]["body"], json!("hey bob"));
        let alice = get_messages(&pool, "alice", true, 50).await?;
        assert_eq!(alice["count"].as_i64(), Some(1), "alice: {alice}");
        assert_eq!(alice["notifications"][0]["data"]["body"], json!("hey alice"));

        // The DM channel is private: not in the public list, but visible to a member.
        let public = list_channels(&pool, None).await?;
        assert_eq!(public.as_array().unwrap().len(), 0, "DM hidden from public list");
        let alices = list_channels(&pool, Some("alice")).await?;
        assert_eq!(alices.as_array().unwrap().len(), 1, "member sees their DM channel");
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
            .as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert!(members.contains("bob"), "invitee auto-joined");

        let bob = check_notifications(&pool, "bob", true, 50, Some("channel.invite")).await?;
        assert_eq!(bob["count"].as_i64(), Some(1), "bob got the invite: {bob}");
        assert_eq!(bob["notifications"][0]["data"]["invited_by"], json!("alice"));

        // Leaving = unsubscribe from the channel.
        unsubscribe(&pool, "bob", None, None, Some(cid), None, false).await?;
        let after = get_channel(&pool, cid).await?;
        let members: Vec<&str> = after["members"]
            .as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
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
        assert!(has_channel_col, "migration should have added events.channel_id");

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
        register_agent(&pool, "slack-bridge", Some("Slack Bridge"), None, None, None, None).await?;

        // Upsert an external identity, then re-upsert to refresh + merge metadata (idempotent).
        let e = upsert_external_identity(&pool, " slack:U123 ", "slack", Some("Ada"), Some(json!({"tz":"UTC"}))).await?;
        assert_eq!(e["id"], json!("slack:U123"), "id is trimmed + stored");
        assert_eq!(e["source"], json!("slack"));
        assert_eq!(e["display_name"], json!("Ada"));
        assert_eq!(e["metadata"]["tz"], json!("UTC"), "metadata parsed to an object");
        let e2 = upsert_external_identity(&pool, "slack:U123", "slack", None, Some(json!({"avatar":"x"}))).await?;
        assert_eq!(e2["display_name"], json!("Ada"), "null display_name keeps the prior value");
        assert_eq!(e2["metadata"]["tz"], json!("UTC"), "metadata is merged, not replaced");
        assert_eq!(e2["metadata"]["avatar"], json!("x"));

        // list, filtered by source.
        assert_eq!(list_external_identities(&pool, Some("slack")).await?.as_array().unwrap().len(), 1);
        assert_eq!(list_external_identities(&pool, Some("github")).await?.as_array().unwrap().len(), 0);
        assert!(get_external_identity(&pool, "slack:unknown").await?.is_null());

        // A comment ingested by the bridge, attributed to the external human.
        let p = create_project(&pool, "P", None, Some("slack-bridge"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(&pool, pid, "T", None, None, None, Some("slack-bridge"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();
        comment_task(&pool, tid, "hi from slack", Some("slack-bridge"), Some("slack:U123"), None).await?;
        let task = get_task(&pool, tid).await?;
        let c0 = &task["comments"][0];
        assert_eq!(c0["author"], json!("slack-bridge"), "author is the fleet ingester");
        assert_eq!(c0["external_author"], json!("slack:U123"), "attributed to the external human");
        assert_eq!(
            c0["external_author_name"], json!("Ada"),
            "the identity's display_name is resolved on read (id stays the key)"
        );

        // A channel post carries the same attribution on its event data.
        let ch = create_channel(&pool, "bridge", None, Some("slack-bridge"), None).await?;
        let cid = ch["id"].as_i64().unwrap();
        post_to_channel(&pool, cid, "slack-bridge", "hello", None, Some("slack:U123")).await?;
        let posts = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        assert_eq!(posts[0]["data"]["from"], json!("slack-bridge"));
        assert_eq!(posts[0]["data"]["external_author"], json!("slack:U123"));
        assert_eq!(
            posts[0]["data"]["external_author_name"], json!("Ada"),
            "channel-post attribution resolves the display name on read too"
        );

        // An identity with no registered display_name: external_author stays, name is absent
        // (consumers fall back to the id — never a fabricated name).
        post_to_channel(&pool, cid, "slack-bridge", "who am i", None, Some("slack:UNKNOWN")).await?;
        let posts2 = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        let last = posts2.as_array().unwrap().last().unwrap();
        assert_eq!(last["data"]["external_author"], json!("slack:UNKNOWN"));
        assert!(
            last["data"].get("external_author_name").is_none(),
            "no display_name registered -> no external_author_name (fall back to the id)"
        );

        // Guardrails: empty id/source are client errors.
        assert!(upsert_external_identity(&pool, "  ", "slack", None, None).await.is_err());
        assert!(upsert_external_identity(&pool, "slack:U9", "  ", None, None).await.is_err());
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
                .filter(|e| e["type"] == json!("channel.outbound_reflect") && e["data"]["channel_id"] == json!(cid))
                .cloned()
                .collect()
        };

        // An out-enabled channel with the default allowlist (concierge).
        let ch = create_channel(&pool, "bridge-out", None, Some("concierge"), Some(json!({"direction":"both"}))).await?;
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
        assert_eq!(reflects(&ev, cid).len(), 1, "denied author stays board-internal");

        // An unconfigured channel never reflects, even for concierge.
        let plain = create_channel(&pool, "plain", None, Some("concierge"), None).await?;
        let pid = plain["id"].as_i64().unwrap();
        post_to_channel(&pool, pid, "concierge", "hi", None, None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        assert_eq!(reflects(&ev, pid).len(), 0, "default policy is board-internal");

        // set_channel_props turns reflect-back ON for the plain channel.
        set_channel_props(&pool, pid, json!({ "direction": "out" })).await?;
        post_to_channel(&pool, pid, "concierge", "now out", None, None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        assert_eq!(reflects(&ev, pid).len(), 1, "policy configurable after creation");
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
        let human_ev = ev.as_array().unwrap().iter().find(|e| e["seq"] == json!(human_seq)).unwrap();
        assert_eq!(human_ev["data"]["metadata"]["thread_ts"], json!("1727.001"));

        // Frank replies on the board -> reflects OUT, and the reflect carries parent_metadata (the
        // human post's thread metadata) so the daemon threads statelessly.
        post_to_channel(&pool, cid, "frank", "Frank's reply", Some(human_seq), None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        let reflect = ev
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == json!("channel.outbound_reflect")
                && e["data"]["channel_id"] == json!(cid)
                && e["data"]["author"] == json!("frank"))
            .expect("frank's reply reflects out");
        assert_eq!(reflect["data"]["reply_to"], json!(human_seq));
        assert_eq!(
            reflect["data"]["parent_metadata"]["thread_ts"],
            json!("1727.001"),
            "reflect carries the parent's thread_ts for stateless threading: {reflect}"
        );

        // A top-level reflected post carries its OWN metadata and no parent_metadata.
        post_to_channel_meta(&pool, cid, "frank", "top-level", None, None, Some(json!({ "k": "v" }))).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        let top = ev
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == json!("channel.outbound_reflect") && e["data"]["author"] == json!("frank"))
            .next_back()
            .unwrap();
        assert_eq!(top["data"]["metadata"]["k"], json!("v"));
        assert!(top["data"].get("parent_metadata").is_none(), "no parent_metadata without reply_to");
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
                .filter(|e| e["type"] == json!("task.outbound_reflect") && e["data"]["task_id"] == json!(tid))
                .cloned()
                .collect()
        };

        // A task linked to a GitHub issue, out-enabled with the default allowlist (concierge).
        let task = create_task(&pool, pid, "t", None, None, None, Some("concierge"), None, None, None).await?;
        let tid = task["id"].as_i64().unwrap();
        upsert_external_link(
            &pool, "github", "camshaft/task-board#42", Some("issue"), "task", tid,
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
        comment_task(&pool, tid, "ingested from github", Some("gh-bridge"), Some("github:U9"), None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        assert_eq!(reflects(&ev, tid).len(), 1, "ingested comment stays board-internal (loop-safe)");

        // A task with no external link never reflects, even for an allowed author.
        let plain = create_task(&pool, pid, "unlinked", None, None, None, Some("concierge"), None, None, None).await?;
        let plain_id = plain["id"].as_i64().unwrap();
        comment_task(&pool, plain_id, "hi", Some("concierge"), None, None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        assert_eq!(reflects(&ev, plain_id).len(), 0, "unlinked task is board-internal");

        // Per-link independence: add a SECOND link that is inbound-only; a comment fires only the
        // out-enabled github link, not the inbound one.
        upsert_external_link(
            &pool, "gitlab", "grp/proj#7", None, "task", tid,
            Some(json!({ "direction": "in" })),
        )
        .await?;
        comment_task(&pool, tid, "second reflect", Some("concierge"), None, None).await?;
        let ev = get_events(&pool, 0, 500, None, false).await?;
        let r = reflects(&ev, tid);
        assert_eq!(r.len(), 2, "only the out-enabled link fires; inbound link stays internal");
        assert!(r.iter().all(|e| e["data"]["source"] == json!("github")), "gitlab (in) never reflects");
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
        let pid = create_project(&pool, "P", None, Some("gh"), None).await?["id"].as_i64().unwrap();
        let link = ExternalRef {
            source: "github".into(),
            external_id: "camshaft/x#1".into(),
            external_parent_id: Some("issue".into()),
        };

        // First ingest: creates the task + link, created:true.
        let a = create_task(&pool, pid, "issue 1", None, None, None, Some("gh"), None, None, Some(link.clone())).await?;
        assert_eq!(a["created"], json!(true));
        let tid = a["id"].as_i64().unwrap();

        // Retry with the SAME (source, external_id): returns the SAME task, created:false, no dup —
        // even though the retry passed a different title.
        let b = create_task(&pool, pid, "issue 1 RETRY", None, None, None, Some("gh"), None, None, Some(link.clone())).await?;
        assert_eq!(b["created"], json!(false));
        assert_eq!(b["id"].as_i64().unwrap(), tid, "same task, not a duplicate");
        assert_eq!(b["title"], json!("issue 1"), "existing task returned unchanged");
        let tasks = list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None, None, None, false).await?;
        assert_eq!(tasks.as_array().unwrap().len(), 1, "exactly one task, no duplicate");

        // Comment idempotency on the new board_kind='comment'.
        let clink = ExternalRef {
            source: "github".into(),
            external_id: "camshaft/x#1-c9".into(),
            external_parent_id: Some("camshaft/x#1".into()),
        };
        let c1 = comment_task(&pool, tid, "hi from github", Some("gh"), Some("github:U1"), Some(clink.clone())).await?;
        assert_eq!(c1["created"], json!(true));
        let cid = c1["comment_id"].as_i64().unwrap();
        let c2 = comment_task(&pool, tid, "hi from github RETRY", Some("gh"), Some("github:U1"), Some(clink.clone())).await?;
        assert_eq!(c2["created"], json!(false));
        assert_eq!(c2["comment_id"].as_i64().unwrap(), cid, "same comment, not a duplicate");
        let got = get_task(&pool, tid).await?;
        assert_eq!(got["comments"].as_array().unwrap().len(), 1, "exactly one comment, no duplicate");

        // A create/comment WITHOUT an external_link is unaffected and reports created:true.
        let plain = create_task(&pool, pid, "manual", None, None, None, Some("gh"), None, None, None).await?;
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
        let u = set_workspace_kind(&pool, "custom-env", None, Some(json!({ "branch": "main" })), None, None).await?;
        assert_eq!(u["setup_script"], json!("checkout && build"), "omitted script kept");
        assert_eq!(u["description"], json!("a custom workspace"), "omitted description kept");
        assert_eq!(u["config"]["cwd"], json!("/w"), "prior config key kept");
        assert_eq!(u["config"]["branch"], json!("main"), "new config key merged in");
        assert_eq!(u["created_by"], json!("board-pm"), "creator preserved");

        // List, unknown, delete (idempotent).
        assert_eq!(list_workspace_kinds(&pool).await?.as_array().unwrap().len(), 1);
        assert!(get_workspace_kind(&pool, "nope").await?.is_null());
        assert_eq!(delete_workspace_kind(&pool, "custom-env").await?["deleted"], json!(true));
        assert!(get_workspace_kind(&pool, "custom-env").await?.is_null());
        assert_eq!(delete_workspace_kind(&pool, "custom-env").await?["deleted"], json!(false));
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
        let root = post_to_channel(&pool, cid, "concierge", "Root topic\nmore detail", None, None).await?;
        let root_seq = root["seq"].as_i64().unwrap();
        let r1 = post_to_channel(&pool, cid, "slack-bridge", "reply from ada", Some(root_seq), Some("slack:U1")).await?;
        let r1_seq = r1["seq"].as_i64().unwrap();
        post_to_channel(&pool, cid, "concierge", "second reply", Some(root_seq), None).await?;
        post_to_channel(&pool, cid, "concierge", "unrelated top-level", None, None).await?;

        let res = promote_thread(&pool, cid, root_seq, pid, Some("concierge")).await?;
        let tid = res["task_id"].as_i64().unwrap();
        assert_eq!(res["imported_comments"], json!(2), "only the two direct replies import");
        assert_eq!(res["already_promoted"], json!(false));

        let task = get_task(&pool, tid).await?;
        assert_eq!(task["title"], json!("Root topic"), "title = root's first line");
        assert_eq!(task["description"], json!("Root topic\nmore detail"), "root body -> description");
        let comments = task["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0]["body"], json!("reply from ada"));
        assert_eq!(comments[0]["author"], json!("slack-bridge"), "ingester preserved");
        assert_eq!(comments[0]["external_author"], json!("slack:U1"), "attribution preserved");
        assert_eq!(comments[0]["origin_ref"], json!(r1_seq.to_string()), "origin id recorded for sync/dedup");
        assert_eq!(comments[1]["body"], json!("second reply"));

        // Timestamp fidelity: the imported comment carries the original reply's created_at.
        let posts = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        let r1_created = posts.as_array().unwrap().iter()
            .find(|p| p["seq"].as_i64() == Some(r1_seq)).unwrap()["created_at"].clone();
        assert_eq!(comments[0]["created_at"], r1_created, "reply timestamp preserved");

        // Idempotent: re-promoting returns the same task, no re-import, no duplicate comments.
        let again = promote_thread(&pool, cid, root_seq, pid, Some("concierge")).await?;
        assert_eq!(again["task_id"], json!(tid));
        assert_eq!(again["already_promoted"], json!(true));
        assert_eq!(get_task(&pool, tid).await?["comments"].as_array().unwrap().len(), 2, "no duplicate import");

        // A missing root post is an error.
        assert!(promote_thread(&pool, cid, 999999, pid, Some("concierge")).await.is_err());
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
        let t = create_task(&pool, pid, "T", None, None, None, Some("concierge"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();

        // Channel-map link (board channel <-> Slack channel).
        let l = upsert_external_link(&pool, "slack", "C123", None, "channel", cid, Some(json!({"name":"#planning"}))).await?;
        assert_eq!(l["source"], json!("slack"));
        assert_eq!(l["external_id"], json!("C123"));
        assert_eq!(l["board_kind"], json!("channel"));
        assert_eq!(l["board_id"], json!(cid));
        assert_eq!(l["metadata"]["name"], json!("#planning"));

        // Idempotent on (source, external_id): re-link updates parent + MERGES metadata.
        let l2 = upsert_external_link(&pool, "slack", "C123", Some("workspaceA"), "channel", cid, Some(json!({"topic":"x"}))).await?;
        assert_eq!(l2["external_parent_id"], json!("workspaceA"));
        assert_eq!(l2["metadata"]["name"], json!("#planning"), "metadata merged, not replaced");
        assert_eq!(l2["metadata"]["topic"], json!("x"));
        assert_eq!(list_external_links(&pool, Some("slack"), None, None).await?.as_array().unwrap().len(), 1, "still one link");

        // The SAME table carries a task link from a different source.
        upsert_external_link(&pool, "github", "https://gh/issues/1", None, "task", tid, None).await?;
        assert_eq!(list_external_links(&pool, None, None, None).await?.as_array().unwrap().len(), 2);
        assert_eq!(list_external_links(&pool, None, Some("channel"), Some(cid)).await?.as_array().unwrap().len(), 1, "adapter resolves board->external");
        assert_eq!(list_external_links(&pool, None, Some("task"), None).await?.as_array().unwrap().len(), 1);
        assert_eq!(list_external_links(&pool, Some("github"), None, None).await?.as_array().unwrap().len(), 1);

        // Guards: bad kind, missing board entity, empty source/external_id.
        assert!(upsert_external_link(&pool, "slack", "C9", None, "widget", cid, None).await.is_err());
        assert!(upsert_external_link(&pool, "slack", "C9", None, "channel", 999999, None).await.is_err());
        assert!(upsert_external_link(&pool, "  ", "C9", None, "channel", cid, None).await.is_err());
        assert!(upsert_external_link(&pool, "slack", "  ", None, "channel", cid, None).await.is_err());
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
        let tid = promote_thread(&pool, cid, root_seq, pid, Some("concierge")).await?["task_id"].as_i64().unwrap();
        assert_eq!(get_task(&pool, tid).await?["comments"].as_array().unwrap().len(), 0);

        // Direction 1: a new thread reply -> a task comment (attribution + origin preserved).
        let r1 = post_to_channel(&pool, cid, "slack-bridge", "reply from ada", Some(root_seq), Some("slack:U1")).await?;
        let r1_seq = r1["seq"].as_i64().unwrap();
        let comments = get_task(&pool, tid).await?["comments"].as_array().unwrap().clone();
        assert_eq!(comments.len(), 1, "thread reply mirrored to a task comment");
        assert_eq!(comments[0]["body"], json!("reply from ada"));
        assert_eq!(comments[0]["author"], json!("slack-bridge"));
        assert_eq!(comments[0]["external_author"], json!("slack:U1"));
        assert_eq!(comments[0]["origin_ref"], json!(r1_seq.to_string()));
        // No echo: the mirrored comment did NOT create another thread post.
        assert_eq!(get_channel_posts(&pool, cid, 0, None, 100, false).await?.as_array().unwrap().len(), 2, "root + r1 only");

        // Direction 2: a new task comment -> a thread reply.
        comment_task(&pool, tid, "reply from board", Some("worker"), None, None).await?;
        let posts = get_channel_posts(&pool, cid, 0, None, 100, false).await?;
        let posts = posts.as_array().unwrap();
        assert_eq!(posts.len(), 3, "root + r1 + the mirrored comment");
        let mirrored = posts.iter().find(|p| p["data"]["origin_comment"].is_i64()).unwrap();
        assert_eq!(mirrored["data"]["reply_to"], json!(root_seq));
        assert_eq!(mirrored["data"]["from"], json!("worker"));
        assert_eq!(mirrored["data"]["body"], json!("reply from board"));
        // No echo: the mirrored post did NOT create another task comment (still r1-mirror + worker's).
        assert_eq!(get_task(&pool, tid).await?["comments"].as_array().unwrap().len(), 2, "no echo comment");

        // Safety: a comment on a NON-linked task posts nothing to the channel.
        let solo = create_task(&pool, pid, "solo", None, None, None, Some("worker"), None, None, None).await?["id"].as_i64().unwrap();
        comment_task(&pool, solo, "unrelated", Some("worker"), None, None).await?;
        assert_eq!(get_channel_posts(&pool, cid, 0, None, 100, false).await?.as_array().unwrap().len(), 3, "unlinked task doesn't post");
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
        let pid = create_project(&pool, "P", None, Some("creator"), None).await?["id"].as_i64().unwrap();
        // Task with NO assignee, so `watcher` is a PURE subscriber (not assignee/creator).
        let tid = create_task(&pool, pid, "T", None, None, None, Some("creator"), None, None, None).await?["id"].as_i64().unwrap();
        subscribe(&pool, "watcher", Some(tid), None, None, None, false).await?;

        // op1 comments -> the pure subscriber hears it; the author (op1) does not hear its own.
        comment_task(&pool, tid, "first", Some("op1"), None, None).await?;
        let w = check_notifications(&pool, "watcher", true, 50, None).await?;
        assert_eq!(w["count"].as_i64(), Some(1), "pure subscriber notified: {w}");
        assert_eq!(w["notifications"][0]["type"], json!("task.commented"));
        assert_eq!(w["notifications"][0]["task_id"], json!(tid), "wake payload carries task_id");
        assert_eq!(check_notifications(&pool, "op1", true, 50, None).await?["count"].as_i64(), Some(0), "author not notified of own comment");

        // Commenting auto-subscribed op1, so it hears a subsequent comment by someone else.
        comment_task(&pool, tid, "second", Some("watcher"), None, None).await?;
        let o = check_notifications(&pool, "op1", true, 50, None).await?;
        assert_eq!(o["count"].as_i64(), Some(1), "commenter auto-subscribed, hears later comments: {o}");
        assert_eq!(o["notifications"][0]["type"], json!("task.commented"));

        // Every task.commented event carries task_id (the wake/webhook payload's routing key).
        let events = get_events(&pool, 0, 500, None, false).await?;
        let commented: Vec<&Value> = events.as_array().unwrap().iter().filter(|e| e["type"] == json!("task.commented")).collect();
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
        let pid = create_project(&pool, "P", None, Some("a"), None).await?["id"].as_i64().unwrap();
        let tid = create_task(&pool, pid, "T", None, None, None, Some("a"), None, None, None).await?["id"].as_i64().unwrap();
        comment_task(&pool, tid, "hi", Some("b"), None, None).await?;

        let all = get_events(&pool, 0, 500, None, false).await?;
        assert!(all.as_array().unwrap().len() >= 3, "project.created + task.created + task.commented");

        let by_a = get_events(&pool, 0, 500, Some("a"), false).await?;
        let by_a = by_a.as_array().unwrap();
        assert!(!by_a.is_empty());
        assert!(by_a.iter().all(|e| e["actor"] == json!("a")), "only actor a: {by_a:?}");
        assert!(by_a.iter().any(|e| e["type"] == json!("task.created")));

        let by_b = get_events(&pool, 0, 500, Some("b"), false).await?;
        let by_b = by_b.as_array().unwrap();
        assert_eq!(by_b.len(), 1, "b only authored the comment");
        assert_eq!(by_b[0]["type"], json!("task.commented"));
        assert_eq!(by_b[0]["actor"], json!("b"));

        assert!(get_events(&pool, 0, 500, Some("nobody"), false).await?.as_array().unwrap().is_empty());
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
        let pid = create_project(&pool, "P", None, Some("a"), None).await?["id"].as_i64().unwrap();
        // Generate a run of events (project.created + one task.created per create_task).
        let mut tids = Vec::new();
        for i in 0..6 {
            tids.push(create_task(&pool, pid, &format!("T{i}"), None, None, None, Some("a"), None, None, None).await?["id"].as_i64().unwrap());
        }

        let seq_of = |v: &Value| v["seq"].as_i64().unwrap();

        // desc=true, limit 3 -> the 3 highest seqs, strictly newest-first.
        let latest = get_events(&pool, 0, 3, None, true).await?;
        let latest = latest.as_array().unwrap().clone();
        assert_eq!(latest.len(), 3, "limit caps the window");
        assert!(seq_of(&latest[0]) > seq_of(&latest[1]) && seq_of(&latest[1]) > seq_of(&latest[2]), "newest-first: {latest:?}");

        // It is the TAIL, not the head: the newest desc seq == the max seq in the full asc log,
        // and the oldest asc event (project.created) is NOT in the latest-3 window.
        let asc = get_events(&pool, 0, 500, None, false).await?;
        let asc = asc.as_array().unwrap();
        let max_seq = asc.iter().map(seq_of).max().unwrap();
        assert_eq!(seq_of(&latest[0]), max_seq, "desc head is the tail of history");
        assert!(!latest.iter().any(|e| e["type"] == json!("project.created")), "oldest event excluded from latest-N");

        // The feed advances: a new event becomes the new desc head.
        comment_task(&pool, tids[0], "newest", Some("a"), None, None).await?;
        let latest2 = get_events(&pool, 0, 3, None, true).await?;
        let latest2 = latest2.as_array().unwrap();
        assert_eq!(latest2[0]["type"], json!("task.commented"), "the just-added event leads");
        assert!(seq_of(&latest2[0]) > max_seq, "advanced past the prior tail");

        // `seq>since_seq` lower bound still applies in desc mode (latest N ABOVE a floor).
        let above = get_events(&pool, max_seq, 50, None, true).await?;
        let above = above.as_array().unwrap();
        assert!(above.iter().all(|e| seq_of(e) > max_seq), "floor honored in desc: {above:?}");

        // Actor filter composes with desc.
        register_agent(&pool, "z", None, None, None, None, None).await?;
        let by_z = get_events(&pool, 0, 10, Some("z"), true).await?;
        assert!(by_z.as_array().unwrap().is_empty(), "actor filter still applies");
        Ok(())
    }

    /// #107: a document version records its content_type (MIME); create/publish accept it,
    /// defaulting to text/markdown, and reads expose it per version.
    #[tokio::test]
    async fn document_version_content_type() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        // v1 with an explicit non-markdown type.
        let d = create_document(&pool, "Diagram", None, "bafypng", None, Some("alice"), None, Some("image/png"), None).await?;
        let did = d["id"].as_i64().unwrap();
        assert_eq!(d["current_version"]["content_type"], json!("image/png"));

        // A later version can change the type; the authoritative type is per-version.
        let d2 = publish_version(&pool, did, "bafypdf", Some("as pdf"), Some("alice"), Some("application/pdf"), None).await?;
        assert_eq!(d2["current_version"]["content_type"], json!("application/pdf"));

        // Omitting content_type defaults to text/markdown (back-compat).
        let d3 = publish_version(&pool, did, "bafymd", None, Some("alice"), None, None).await?;
        assert_eq!(d3["current_version"]["content_type"], json!("text/markdown"));

        // get_document_versions surfaces content_type per version.
        let vers = get_document_versions(&pool, did).await?;
        let types: Vec<&str> = vers.as_array().unwrap().iter().map(|v| v["content_type"].as_str().unwrap()).collect();
        assert_eq!(types, vec!["text/markdown", "application/pdf", "image/png"], "newest first");

        // A default create_document (no content_type) is text/markdown, matching the initial docs work.
        let plain = create_document(&pool, "Notes", None, "bafymd2", None, Some("bob"), None, None, None).await?;
        assert_eq!(plain["current_version"]["content_type"], json!("text/markdown"));
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
        let t = create_task(&pool, pid, "Ship it", None, None, None, Some("alice"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();
        let dep = create_task(&pool, pid, "Dependency", None, None, None, Some("bob"), None, None, None).await?;
        let dep_id = dep["id"].as_i64().unwrap();

        // Enforcement: blocked without a blocked_on is rejected (maps to 400 via the "give " prefix).
        let e = update_task(&pool, tid, Some("blocked"), None, None, None, None, Some("alice"), None, None, None)
            .await
            .unwrap_err();
        assert!(e.to_string().starts_with("give "), "blocked needs a blocked_on, got: {e}");

        // Block on another TASK.
        update_task(&pool, tid, Some("blocked"), None, None, None, None, Some("alice"), None, None,
            Some(json!({"kind": "task", "target": dep_id.to_string(), "note": "waiting on dep"}))).await?;
        let bo = get_task(&pool, tid).await?["blocked_on"].clone();
        assert_eq!(bo["kind"], json!("task"));
        assert_eq!(bo["target"], json!(dep_id.to_string()));
        assert_eq!(bo["note"], json!("waiting on dep"));

        // A nonexistent task target is rejected.
        assert!(update_task(&pool, tid, Some("blocked"), None, None, None, None, Some("alice"), None, None,
            Some(json!({"kind": "task", "target": "999999"}))).await.is_err());

        // Re-block on an AGENT -> that agent is notified they're blocking.
        update_task(&pool, tid, Some("blocked"), None, None, None, None, Some("alice"), None, None,
            Some(json!({"kind": "agent", "target": "agent:rev", "note": "need review"}))).await?;
        let notes = check_notifications(&pool, "agent:rev", true, 50, None).await?;
        let arr = notes["notifications"].as_array().unwrap();
        assert!(
            arr.iter().any(|n| n["type"] == json!("task.blocked_on_you") && n["task_id"] == json!(tid)),
            "the blocking agent is notified: {notes}"
        );

        // "What is blocked on agent:rev" view.
        let on_agent = list_tasks(&pool, Some(pid), None, None, false, None, false, None, Some("agent"), Some("agent:rev"), None, None, false).await?;
        assert_eq!(on_agent.as_array().unwrap().len(), 1);

        // Block on the OPERATOR -> ref is null, and the operator view lists it.
        update_task(&pool, tid, Some("blocked"), None, None, None, None, Some("alice"), None, None,
            Some(json!({"kind": "operator"}))).await?;
        let bo = get_task(&pool, tid).await?["blocked_on"].clone();
        assert_eq!(bo["kind"], json!("operator"));
        assert!(bo["target"].is_null());
        let on_op = list_tasks(&pool, None, None, None, false, None, false, None, Some("operator"), None, None, None, false).await?;
        assert!(on_op.as_array().unwrap().iter().any(|t| t["id"] == json!(tid)));

        // Leaving blocked clears blocked_on.
        update_task(&pool, tid, Some("in_progress"), None, None, None, None, Some("alice"), None, None, None).await?;
        assert!(get_task(&pool, tid).await?["blocked_on"].is_null(), "unblocking clears blocked_on");
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
                    let t = create_task(&pool, pid, &format!("t{w}-{r}"), None, None, None, Some(&agent), None, None, None).await?;
                    let tid = t["id"].as_i64().unwrap();
                    comment_task(&pool, tid, "working", Some(&agent), None, None).await?;
                    update_task(&pool, tid, Some("in_progress"), None, None, None, None, Some(&agent), None, None, None).await?;
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

        set_review_status(&pool, rid, "changes_requested", Some("reviewer"), Some("fix the deref")).await?;
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
        assert!(set_review_status(&pool, rid, "bogus", Some("x"), None).await.is_err());
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
            &pool, "code", Some("github-pull-request"), Some("owner/repo#7"), Some("PR 7"),
            Some("in_review"), Some("bridge"), None, None, Some(ext.clone()),
        )
        .await?;
        assert_eq!(first["created"], json!(true));
        assert_eq!(first["status"], json!("in_review"));
        let rid = first["id"].as_i64().unwrap();

        let second = create_review(
            &pool, "code", Some("github-pull-request"), Some("owner/repo#7"), Some("PR 7 again"),
            Some("open"), Some("bridge"), None, None, Some(ext),
        )
        .await?;
        assert_eq!(second["created"], json!(false));
        assert_eq!(second["id"].as_i64().unwrap(), rid, "same review, no duplicate");
        assert_eq!(second["status"], json!("in_review"), "existing state preserved");

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

        let a = request_stand_down(&pool, "worker", Some("concierge"), Some("rebalancing the fleet")).await?;
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
        assert!(off["stand_down_requested_at"].is_null(), "cleared on offline");
        assert!(off["stand_down_requested_by"].is_null());
        assert!(off["stand_down_reason"].is_null());

        // Unknown agent -> error (surfaces as a 404 at the API).
        assert!(request_stand_down(&pool, "ghost", Some("x"), None).await.is_err());
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
        assert_eq!(c2["id"].as_i64().unwrap(), cid1, "same DM channel either way");

        // Both are members, and it's private.
        let members: BTreeSet<String> = c1["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap().to_string())
            .collect();
        assert_eq!(members, ["alice", "bob"].iter().map(|s| s.to_string()).collect());
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
        let r1 = create_review(&pool, "code", None, None, Some("r1"), None, Some("alpha"), None, None, None).await?;
        let r1id = r1["id"].as_i64().unwrap();
        for _ in 0..3 {
            append_review_log(&pool, r1id, "finding", Some("f"), Some("alpha"), None, None).await?;
        }

        // r2 (code, alpha): 1 pre-approval finding + 1 finding logged AFTER approval (escaped).
        let r2 = create_review(&pool, "code", None, None, Some("r2"), None, Some("alpha"), None, None, None).await?;
        let r2id = r2["id"].as_i64().unwrap();
        append_review_log(&pool, r2id, "finding", Some("pre"), Some("alpha"), None, None).await?;
        set_review_status(&pool, r2id, "in_review", Some("alpha"), None).await?;
        set_review_status(&pool, r2id, "approved", Some("alpha"), None).await?;
        tokio::time::sleep(std::time::Duration::from_millis(3)).await; // ensure a strictly later ts
        append_review_log(&pool, r2id, "finding", Some("escaped after approval"), Some("qa"), None, None).await?;

        // r3 (design, beta): a lineage follow-up (declares a predecessor) that still found something.
        let r3 = create_review(&pool, "design", None, None, Some("r3"), None, Some("beta"), None,
            Some(json!({ "predecessor_review_id": r1id })), None).await?;
        let r3id = r3["id"].as_i64().unwrap();
        append_review_log(&pool, r3id, "finding", Some("missed by predecessor"), Some("beta"), None, None).await?;

        // r4 (design, beta): a re-open (approved -> back to in_review), no findings.
        let r4 = create_review(&pool, "design", None, None, Some("r4"), None, Some("beta"), None, None, None).await?;
        let r4id = r4["id"].as_i64().unwrap();
        set_review_status(&pool, r4id, "in_review", Some("beta"), None).await?;
        set_review_status(&pool, r4id, "approved", Some("beta"), None).await?;
        set_review_status(&pool, r4id, "in_review", Some("beta"), Some("reopened for a regression")).await?;

        let t = review_improvement_trend(&pool, None, None).await?;
        assert_eq!(t["overall"]["reviews"].as_u64(), Some(4));

        // code slice: findings fell (3 -> 2) while escaped defects rose (0 -> 1) => FLAGGED.
        let by_kind = t["by_kind"].as_array().unwrap();
        let code = by_kind.iter().find(|s| s["kind"] == json!("code")).unwrap();
        assert_eq!(code["findings_trend"], json!("improving"));
        assert_eq!(code["escaped_trend"], json!("rising"));
        assert_eq!(code["flagged"], json!(true));
        assert_eq!(code["escaped_defects"]["post_approval_findings"].as_u64(), Some(1));
        assert_eq!(code["escaped_defects"]["total"].as_u64(), Some(1));

        // design slice: a lineage follow-up and a re-open each register as an escaped defect.
        let design = by_kind.iter().find(|s| s["kind"] == json!("design")).unwrap();
        assert_eq!(design["escaped_defects"]["lineage_followups"].as_u64(), Some(1));
        assert_eq!(design["escaped_defects"]["reopens"].as_u64(), Some(1));

        // Sliced by producing area too.
        let areas: BTreeSet<String> = t["by_area"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["area"].as_str().unwrap().to_string())
            .collect();
        assert!(areas.contains("alpha") && areas.contains("beta"), "areas: {areas:?}");

        // Filters narrow the population.
        let code_only = review_improvement_trend(&pool, Some("code"), None).await?;
        assert_eq!(code_only["overall"]["reviews"].as_u64(), Some(2));
        assert_eq!(code_only["overall"]["flagged"], json!(true));
        let beta_only = review_improvement_trend(&pool, None, Some("beta")).await?;
        assert_eq!(beta_only["overall"]["reviews"].as_u64(), Some(2));
        assert_eq!(beta_only["overall"]["escaped_defects"]["reopens"].as_u64(), Some(1));
        assert_eq!(beta_only["overall"]["escaped_defects"]["lineage_followups"].as_u64(), Some(1));
        Ok(())
    }

    /// Mutation responses are trimmed so a looping caller doesn't re-ingest its own charter /
    /// a task's description on every tick (task #416): set_status returns presence fields only,
    /// and strip_field drops a heavy blob while leaving everything else intact.
    #[tokio::test]
    async fn mutation_responses_are_trimmed() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        register_agent(&pool, "worker", Some("Worker"), None, Some("a very long charter ".repeat(200).trim()), None, None).await?;

        // set_status's core still returns the full agent; presence_projection (what the boundary
        // applies) keeps only presence fields and drops the charter.
        let full = set_status(&pool, "worker", "online", Some("ping")).await?;
        assert!(full["charter"].is_string(), "core still has the full object");
        let presence = presence_projection(full);
        assert_eq!(presence["id"], json!("worker"));
        assert_eq!(presence["status"], json!("online"));
        assert_eq!(presence["status_message"], json!("ping"));
        assert!(presence.get("last_seen").is_some());
        assert!(presence.get("charter").is_none(), "presence response must omit the charter");
        assert!(presence.get("metadata").is_none(), "presence response is presence-only");

        // strip_field drops one blob, keeps the rest, and is a no-op on a non-object.
        let agent = get_agent(&pool, "worker").await?;
        let trimmed = strip_field(agent.clone(), "charter");
        assert!(trimmed.get("charter").is_none());
        assert_eq!(trimmed["display_name"], json!("Worker"), "other fields survive the strip");
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

        let r = create_review(&pool, "design", None, None, Some("d"), None, Some("author"), None, None, None).await?;
        let rid = r["id"].as_i64().unwrap();
        assert_eq!(r["vetted"], json!(false));
        let base_log = r["log"].as_array().unwrap().len();

        // Mark vetted: flag flips, and a decision entry records actor + from/to.
        let v = set_review_vetted(&pool, rid, true, Some("gatekeeper"), Some("adversarial pass clean")).await?;
        assert_eq!(v["vetted"], json!(true));
        let log = v["log"].as_array().unwrap();
        assert_eq!(log.len(), base_log + 1, "one audit entry added");
        let entry = log.last().unwrap();
        assert_eq!(entry["entry_type"], json!("decision"));
        assert_eq!(entry["author"], json!("gatekeeper"));
        assert!(entry["body"].as_str().unwrap().contains("vetted: false -> true"), "audit records from/to: {entry}");

        // The creator (author) hears review.vetted_changed; the actor (gatekeeper) does not self-notify.
        let inbox = check_notifications(&pool, "author", true, 50, None).await?;
        assert!(
            inbox["notifications"].as_array().unwrap().iter().any(|n| n["type"] == json!("review.vetted_changed")),
            "creator is notified of the vetted change: {inbox}"
        );

        // Idempotent no-op: setting true again adds no log entry.
        let again = set_review_vetted(&pool, rid, true, Some("gatekeeper"), None).await?;
        assert_eq!(again["log"].as_array().unwrap().len(), base_log + 1, "no-op adds no entry");

        // Clearing flips it back and logs another decision entry.
        let cleared = set_review_vetted(&pool, rid, false, Some("gatekeeper"), None).await?;
        assert_eq!(cleared["vetted"], json!(false));
        assert_eq!(cleared["log"].as_array().unwrap().len(), base_log + 2);

        // Unknown review errors (404 at the API).
        assert!(set_review_vetted(&pool, 99999, true, Some("x"), None).await.is_err());
        Ok(())
    }
}
