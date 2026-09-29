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
    Value::Object(base).to_string()
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
    let n = sqlx::query(
        "UPDATE agents SET status=?, status_message=COALESCE(?,status_message), last_seen=? WHERE id=?",
    )
    .bind(status)
    .bind(status_message)
    .bind(&ts)
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

pub async fn list_agents(pool: &Pool) -> anyhow::Result<Value> {
    let rows = sqlx::query("SELECT * FROM agents ORDER BY last_seen DESC")
        .fetch_all(pool)
        .await?;
    Ok(Value::Array(rows.iter().map(agent_json).collect()))
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
        // metadata: parse JSON string -> object (mirrors get_task).
        let meta: Value = m
            .get("metadata")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));
        m.insert("metadata".into(), meta);
        m.insert(
            "tasks".into(),
            Value::Array(tasks.iter().map(row_to_json).collect()),
        );
    }
    Ok(d)
}

// --- Tasks ---

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
) -> anyhow::Result<Value> {
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
    let out = fetch_one_json(&mut tx, "SELECT * FROM tasks WHERE id=?", tid)
        .await?
        .unwrap_or(Value::Null);
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
            other => anyhow::bail!("give a valid `blocked_on.kind` (task, agent, or operator), not '{other}'"),
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
            "give a `blocked_on` (kind: task, agent, or operator) — a blocked task must record what it is waiting on"
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
        let stored_ref = if kind == "operator" { None } else { target.clone() };
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

pub async fn get_task(pool: &Pool, task_id: i64) -> anyhow::Result<Value> {
    let t = sqlx::query("SELECT * FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_optional(pool)
        .await?;
    let Some(t) = t else { return Ok(Value::Null) };
    let mut d = row_to_json(&t);
    if let Value::Object(ref mut m) = d {
        // metadata: parse JSON string -> object
        let meta: Value = m
            .get("metadata")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));
        m.insert("metadata".into(), meta);

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

        let comments = sqlx::query(
            "SELECT id, author, body, created_at, external_author, origin_ref FROM comments WHERE task_id=? ORDER BY id",
        )
        .bind(task_id)
        .fetch_all(pool)
        .await?;
        m.insert(
            "comments".into(),
            Value::Array(comments.iter().map(row_to_json).collect()),
        );

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
            Value::Array(docs.iter().map(row_to_json).collect()),
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
        m.insert("children".into(), Value::Array(children.iter().map(row_to_json).collect()));
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
) -> anyhow::Result<Value> {
    let mut q = String::from(
        "SELECT id, project_id, title, status, assignee, priority, parent_id, updated_at, \
         blocked_on_kind, blocked_on_ref FROM tasks",
    );
    let mut conds: Vec<&str> = Vec::new();
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
    let rows = query.fetch_all(pool).await?;
    Ok(Value::Array(rows.iter().map(row_to_json).collect()))
}

pub async fn comment_task(
    pool: &Pool,
    task_id: i64,
    body: &str,
    author: Option<&str>,
    external_author: Option<&str>,
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
    // If this task mirrors a promoted channel thread, fan the comment back out as a thread reply.
    mirror_task_comment_to_thread(&mut tx, &mut hooks, task_id, cid, author, body, external_author).await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(json!({ "comment_id": cid, "task_id": task_id }))
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
pub async fn post_to_channel(
    pool: &Pool,
    channel_id: i64,
    sender: &str,
    body: &str,
    reply_to: Option<i64>,
    external_author: Option<&str>,
) -> anyhow::Result<Value> {
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
        Recipients::FromChannel(channel_id),
    )
    .await?;

    // Outbound reflect-back authz (design #141 §5): the board is authoritative on which posts
    // may leave the board for an external system. A `channel.outbound_reflect` event is emitted
    // ONLY when the channel's policy permits this author — a bridge adapter is a dumb executor
    // that acts solely on these authorized events. Other posts stay board-internal (no event).
    if channel_reflects_out(&ch_meta, sender) {
        let mut reflect = json!({
            "channel_id": channel_id,
            "post_seq": seq,
            "author": sender,
            "body": body,
        });
        if let Some(parent) = reply_to {
            reflect["reply_to"] = json!(parent);
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

/// Outbound reflect-back policy (design #141 §5), read from a channel's `metadata`:
/// `{ "outbound_authors": [..] (default ["concierge"]), "direction": "in"|"out"|"both" (default
/// "in") }`. A post reflects OUT to an external system iff the channel's `direction` allows
/// outbound (`out`/`both`) AND its `author` is in the `outbound_authors` allowlist. The safe
/// default is board-internal: an unconfigured channel (direction defaults to "in") reflects
/// nothing, so existing channels never start leaking to an external system.
fn channel_reflects_out(metadata: &Value, author: &str) -> bool {
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
    emit(tx, hooks, "channel.post", Some(from), None, None, Some(channel_id), None, data, Recipients::FromChannel(channel_id))
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
    limit: i64,
) -> anyhow::Result<Value> {
    // Only post events — a channel's event stream also carries channel.created / channel.invite
    // which aren't messages. DM channels post as message.direct, named ones as channel.post.
    let rows = sqlx::query(
        "SELECT seq, type, actor, channel_id, data, created_at FROM events \
         WHERE channel_id=? AND seq>? AND type IN ('channel.post','message.direct') \
         ORDER BY seq LIMIT ?",
    )
    .bind(channel_id)
    .bind(since_seq)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for r in &rows {
        let mut d = row_to_json(r);
        if let Value::Object(ref mut m) = d {
            let data: Value = m
                .get("data")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| json!({}));
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
) -> anyhow::Result<Value> {
    // Optional `actor` filter — a complete per-agent activity feed without over-fetching.
    let sql = if actor.is_some() {
        "SELECT * FROM events WHERE seq>? AND actor=? ORDER BY seq LIMIT ?"
    } else {
        "SELECT * FROM events WHERE seq>? ORDER BY seq LIMIT ?"
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
            let data: Value = m
                .get("data")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| json!({}));
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

// --- External links (bridged mappings: channel-map, issue↔task, thread↔task) ---

/// The board entity kinds an external link may target.
const EXTERNAL_LINK_KINDS: &[&str] = &["channel", "task", "thread"];

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
        anyhow::bail!("give a `board_kind` of one of: channel, task, thread");
    }
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    // A clean 404 for the two kinds backed by a real table (thread = a channel post seq, skipped).
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
    let mut seen: BTreeSet<String> = BTreeSet::new();
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
                if !path.is_empty() && seen.insert(path.clone()) {
                    out.push(WikiEdge {
                        path,
                        label: label.filter(|l| !l.is_empty()),
                        kind: if is_embed { "embed" } else { "link" },
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
    sqlx::query("DELETE FROM document_links WHERE source_document_id=?")
        .bind(source_document_id)
        .execute(&mut **tx)
        .await?;
    for e in extract_wiki_edges(content) {
        // Resolve an embed's version pin to the immutable version id, if that doc+version exists.
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
            "INSERT INTO document_links(source_document_id, target_path, label, kind, \
             target_version_id, region, created_at) VALUES(?,?,?,?,?,?,?)",
        )
        .bind(source_document_id)
        .bind(&e.path)
        .bind(e.label.as_deref())
        .bind(e.kind)
        .bind(target_version_id)
        .bind(e.region.as_deref())
        .bind(ts)
        .execute(&mut **tx)
        .await?;
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
        m.insert("attached_tasks".into(), Value::Array(tasks.iter().map(row_to_json).collect()));

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

        // Outbound embeds (![[target]] this doc transcludes, kind='embed'). Carries the pinned
        // target_version_id (null = floats to the target's current version) and an optional
        // region fragment for a partial embed, plus the resolved target doc.
        let embeds = sqlx::query(
            "SELECT l.target_path, l.label, l.target_version_id, l.region, \
             d.id AS target_document_id, d.title AS target_title, d.status AS target_status \
             FROM document_links l LEFT JOIN documents d ON d.path = l.target_path \
             WHERE l.source_document_id=? AND l.kind='embed' ORDER BY l.target_path",
        )
        .bind(document_id)
        .fetch_all(&mut **tx)
        .await?;
        m.insert("embeds".into(), Value::Array(embeds.iter().map(row_to_json).collect()));

        // Incoming edges to THIS doc's path: backlinks (things that LINK here) and embedded_by
        // (things that EMBED here -- the "what depends on me before I change it" payoff). Both
        // empty when this doc is unfiled (no path), since an edge can only target a path.
        let (backlinks, embedded_by) = match m.get("path").and_then(|v| v.as_str()) {
            Some(p) => {
                let incoming = sqlx::query(
                    "SELECT s.id, s.title, s.path, s.status, l.label, l.kind, l.region \
                     FROM document_links l JOIN documents s ON s.id = l.source_document_id \
                     WHERE l.target_path=? ORDER BY s.path, s.id",
                )
                .bind(p)
                .fetch_all(&mut **tx)
                .await?;
                let mut back = Vec::new();
                let mut emb = Vec::new();
                for r in &incoming {
                    let v = row_to_json(r);
                    if v.get("kind").and_then(|k| k.as_str()) == Some("embed") {
                        emb.push(v);
                    } else {
                        back.push(v);
                    }
                }
                (Value::Array(back), Value::Array(emb))
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
) -> anyhow::Result<Value> {
    let mut q = String::from(
        "SELECT id, title, slug, path, project_id, status, current_version_id, approved_version_id, \
         created_by, updated_at FROM documents",
    );
    // conds and the binds below MUST stay in the same order.
    let mut conds: Vec<&str> = Vec::new();
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
    Ok(Value::Array(rows.iter().map(row_to_json).collect()))
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
pub async fn list_wiki(pool: &Pool, prefix: Option<&str>) -> anyhow::Result<Value> {
    let rows = match prefix.map(|p| p.trim().trim_matches('/')).filter(|p| !p.is_empty()) {
        Some(p) => {
            sqlx::query(
                "SELECT id, title, slug, path, project_id, status, current_version_id, \
                 approved_version_id, created_by, updated_at FROM documents \
                 WHERE path IS NOT NULL AND (path=? OR path LIKE ? || '/%') ORDER BY path",
            )
            .bind(p)
            .bind(p)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query(
                "SELECT id, title, slug, path, project_id, status, current_version_id, \
                 approved_version_id, created_by, updated_at FROM documents \
                 WHERE path IS NOT NULL ORDER BY path",
            )
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
    // Validate the document exists for a clean 404 (version_id/reply_to are FK-enforced).
    if sqlx::query("SELECT 1 FROM documents WHERE id=?")
        .bind(document_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        anyhow::bail!("no document {document_id}");
    }
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
    let mut data = json!({ "comment_id": cid, "version_id": version_id, "body": body, "reply_to": reply_to });
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
    let out = sqlx::query("SELECT * FROM document_comments WHERE id=?")
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
    let out = sqlx::query("SELECT * FROM document_comments WHERE id=?")
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
    let mut q = String::from("SELECT * FROM document_comments WHERE document_id=?");
    if version_id.is_some() {
        q.push_str(" AND version_id=?");
    }
    if status.is_some() {
        q.push_str(" AND status=?");
    }
    q.push_str(" ORDER BY id");
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
        sqlx::query("SELECT project_id, current_version_id FROM documents WHERE id=?")
            .bind(document_id)
            .fetch_optional(&mut *tx)
            .await?
    else {
        anyhow::bail!("no document {document_id}");
    };
    let project_id: Option<i64> = row.try_get("project_id")?;
    let current_version_id: Option<i64> = row.try_get("current_version_id")?;
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
        let t = create_task(&pool, pid, "Calibrate pressure advance", None, Some("fixer"), None, Some("planner"), None, None).await?;
        let tid = t["id"].as_i64().unwrap();

        subscribe(&pool, "planner", Some(tid), None, None, None, false).await?; // (already auto-subscribed as creator)
        comment_task(&pool, tid, "Start from PA=0.03", Some("planner"), None).await?;
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
        let t = create_task(&pool, aid, "T", None, None, None, Some("alice"), None, None).await?;
        let tid = t["id"].as_i64().unwrap();
        comment_task(&pool, tid, "hi", Some("alice"), None).await?;
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
        assert_eq!(list_documents(&pool, Some(pid), None, None, None, None).await?.as_array().unwrap().len(), 1);
        assert_eq!(
            list_documents(&pool, Some(pid), Some("draft"), None, None, None).await?.as_array().unwrap().len(),
            1
        );
        assert_eq!(
            list_documents(&pool, Some(pid), Some("approved"), None, None, None).await?.as_array().unwrap().len(),
            0
        );
        assert_eq!(list_documents(&pool, Some(99999), None, None, None, None).await?.as_array().unwrap().len(), 0);

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
        let all = list_wiki(&pool, None).await?;
        let all = all.as_array().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0]["path"], json!("architecture/board/events"));
        assert_eq!(all[1]["path"], json!("architecture/board/schema"));
        assert_eq!(all[2]["path"], json!("runbooks/deploy"));

        // Prefix filter: only the architecture subtree.
        let arch = list_wiki(&pool, Some("architecture")).await?;
        assert_eq!(arch.as_array().unwrap().len(), 2);
        // A prefix must not match a sibling that merely shares a string head.
        assert_eq!(list_wiki(&pool, Some("runbooks")).await?.as_array().unwrap().len(), 1);

        // Rename frees the old path (b can now take it) and clearing unfiles a doc.
        set_document_path(&pool, a, "architecture/board/events-v2", None).await?;
        set_document_path(&pool, b, "architecture/board/events", None).await?; // no longer a collision
        set_document_path(&pool, c, "", None).await?; // clear -> unfiled
        assert!(get_document(&pool, c).await?["path"].is_null());
        assert_eq!(list_wiki(&pool, None).await?.as_array().unwrap().len(), 2);
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
        // Links and embeds are separated by kind. Note: [[lib/widget]] and ![[lib/widget]] share
        // a path, so the first-seen (the link) wins that path -> one link, and embeds are the
        // distinct-path ![[..]] targets (@v1 collapses to lib/widget too, already taken).
        let links = s_doc["outbound_links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["target_path"] == json!("lib/widget")), "the link is recorded");
        let embeds = s_doc["embeds"].as_array().unwrap();
        // lib/widget was claimed by the link (first-seen), so the surviving embed is lib/notes.
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

        // embedded_by: the Widget doc sees who embeds it (Page2's pinned embed), separate from links.
        let t_doc = get_document(&pool, tid).await?;
        let emb_by = t_doc["embedded_by"].as_array().unwrap();
        assert!(emb_by.iter().any(|e| e["id"] == json!(s2["id"].as_i64().unwrap())), "Page2 embeds Widget");
        // The plain link from the first Page shows up as a backlink, not an embed.
        assert!(t_doc["backlinks"].as_array().unwrap().iter().any(|b| b["id"] == json!(sid)), "Page links Widget");
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
        let t = create_task(&pool, pid, "Build widget", None, Some("alice"), None, Some("alice"), None, None)
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
        let t = create_task(&pool, pid, "T", None, None, None, Some("u"), None, None).await?;
        let tid = t["id"].as_i64().unwrap();
        attach_document(&pool, aid, tid, Some("u")).await?;

        let ids = |v: &Value| -> Vec<i64> {
            v.as_array().unwrap().iter().map(|d| d["id"].as_i64().unwrap()).collect()
        };

        // author
        assert_eq!(ids(&list_documents(&pool, None, None, None, None, Some("alice")).await?), vec![aid, cid]);
        assert_eq!(ids(&list_documents(&pool, None, None, None, None, Some("bob")).await?), vec![bid]);
        // tag
        assert_eq!(ids(&list_documents(&pool, None, None, Some("design"), None, None).await?), vec![aid]);
        assert_eq!(ids(&list_documents(&pool, None, None, Some("ops"), None, None).await?), vec![bid]);
        assert!(list_documents(&pool, None, None, Some("nope"), None, None).await?.as_array().unwrap().is_empty());
        // task attachment
        assert_eq!(ids(&list_documents(&pool, None, None, None, Some(tid), None).await?), vec![aid]);
        // project
        assert_eq!(ids(&list_documents(&pool, Some(pid), None, None, None, None).await?), vec![aid, cid]);
        // status
        assert_eq!(ids(&list_documents(&pool, None, Some("approved"), None, None, None).await?), vec![cid]);
        // combined AND: project + tag rfc + author alice -> only A
        assert_eq!(
            ids(&list_documents(&pool, Some(pid), None, Some("rfc"), None, Some("alice")).await?),
            vec![aid]
        );
        // contradictory combo -> empty
        assert!(list_documents(&pool, None, None, Some("ops"), None, Some("alice")).await?.as_array().unwrap().is_empty());
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
        let epic = create_task(&pool, pid, "Epic", None, None, None, Some("u"), None, None).await?;
        let eid = epic["id"].as_i64().unwrap();
        let c1 = create_task(&pool, pid, "c1", None, None, None, Some("u"), None, Some(eid)).await?;
        let c1id = c1["id"].as_i64().unwrap();
        let c2 = create_task(&pool, pid, "c2", None, None, None, Some("u"), None, Some(eid)).await?;
        let c2id = c2["id"].as_i64().unwrap();

        // Cross-project parent rejected at create.
        assert!(create_task(&pool, pid2, "x", None, None, None, Some("u"), None, Some(eid)).await.is_err());
        // Non-existent parent rejected.
        assert!(create_task(&pool, pid, "y", None, None, None, Some("u"), None, Some(99999)).await.is_err());

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
        assert_eq!(ids(&list_tasks(&pool, Some(pid), None, None, false, None, true, None, None, None).await?), vec![eid]);
        assert_eq!(
            ids(&list_tasks(&pool, Some(pid), None, None, false, Some(eid), false, None, None, None).await?),
            vec![c1id, c2id]
        );

        // Guards: self-parent + cycle rejected.
        assert!(update_task(&pool, eid, None, None, None, None, None, Some("u"), None, Some(eid), None).await.is_err());
        assert!(update_task(&pool, eid, None, None, None, None, None, Some("u"), None, Some(c1id), None).await.is_err());

        // Clear c2's parent (parent_id=0) -> top-level; emits task.reparented; roll-up shrinks.
        let r = update_task(&pool, c2id, None, None, None, None, None, Some("u"), None, Some(0), None).await?;
        assert!(r["parent_id"].is_null());
        assert_eq!(get_task(&pool, eid).await?["child_rollup"], json!({ "done": 1, "total": 1 }));
        let evs = get_events(&pool, 0, 200, None).await?;
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

        create_task(&pool, pid1, "Fix the widget pipeline", Some("handles reflow"), None, None, Some("u"), None, None).await?;
        create_task(&pool, pid1, "Unrelated chore", None, None, None, Some("u"), None, None).await?;
        create_task(&pool, pid2, "Widget docs", Some("describe the WIDGET api"), Some("alice"), None, Some("u"), None, None).await?;

        let titles = |v: &Value| -> Vec<String> {
            let mut t: Vec<String> =
                v.as_array().unwrap().iter().map(|x| x["title"].as_str().unwrap().to_string()).collect();
            t.sort();
            t
        };

        // "widget" across ALL projects (case-insensitive) -> the two widget tasks, not the chore.
        assert_eq!(
            titles(&list_tasks(&pool, None, None, None, false, None, false, Some("widget"), None, None).await?),
            vec!["Fix the widget pipeline".to_string(), "Widget docs".to_string()]
        );
        // Matches description too.
        assert_eq!(
            titles(&list_tasks(&pool, None, None, None, false, None, false, Some("reflow"), None, None).await?),
            vec!["Fix the widget pipeline".to_string()]
        );
        // Composable with assignee: widget + alice -> only the Beta doc task.
        assert_eq!(
            titles(&list_tasks(&pool, None, None, Some("alice"), false, None, false, Some("widget"), None, None).await?),
            vec!["Widget docs".to_string()]
        );
        // Composable with project scope: widget in Alpha -> only the pipeline task.
        assert_eq!(
            titles(&list_tasks(&pool, Some(pid1), None, None, false, None, false, Some("widget"), None, None).await?),
            vec!["Fix the widget pipeline".to_string()]
        );
        // No match -> empty.
        assert!(list_tasks(&pool, None, None, None, false, None, false, Some("zzznope"), None, None)
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
        let t = create_task(&pool, pid, "T", None, None, None, None, Some(json!({"a": 1})), None).await?;
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
        update_agent(&pool, "v-x", None, None, None, Some("away"), None, None, Some(json!({"branch": "main"}))).await?;
        let got = get_agent(&pool, "v-x").await?;
        assert_eq!(got["status"], "away");
        assert_eq!(
            got["metadata"],
            json!({"role": "vertical", "model": "opus", "effort": "high", "branch": "main"})
        );

        // update_agent on an unknown agent errors (it's a mutate, not an upsert).
        assert!(update_agent(&pool, "nope", None, None, None, None, None, None, None).await.is_err());

        // A fresh agent gets an empty bag by default, not null.
        register_agent(&pool, "v-y", None, None, None, None, None).await?;
        assert_eq!(get_agent(&pool, "v-y").await?["metadata"], json!({}));
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
        create_task(&pool, 2, "on dupe", None, None, None, Some("u"), None, None).await?;
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
        let tasks = list_tasks(&pool, Some(1), None, None, false, None, false, None, None, None).await?;
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
        let t = create_task(&pool, aid, "T", None, None, None, Some("u"), None, None).await?;
        let tid = t["id"].as_i64().unwrap();

        let moved = move_task(&pool, tid, bid, Some("u")).await?;
        assert_eq!(moved["project_id"], json!(bid));
        // It now lists under B, not A.
        assert_eq!(list_tasks(&pool, Some(aid), None, None, false, None, false, None, None, None).await?.as_array().unwrap().len(), 0);
        assert_eq!(list_tasks(&pool, Some(bid), None, None, false, None, false, None, None, None).await?.as_array().unwrap().len(), 1);

        // A task.moved event was recorded carrying both ends.
        let events = get_events(&pool, 0, 100, None).await?;
        let ev = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == json!("task.moved"))
            .expect("task.moved emitted");
        assert_eq!(ev["data"]["from_project_id"], json!(aid));
        assert_eq!(ev["data"]["to_project_id"], json!(bid));

        // Moving onto the current project is a no-op (no new event, still on B).
        let before = get_events(&pool, 0, 100, None).await?.as_array().unwrap().len();
        move_task(&pool, tid, bid, Some("u")).await?;
        let after = get_events(&pool, 0, 100, None).await?.as_array().unwrap().len();
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
        create_task(&pool, pid, "owned", None, Some("alice"), None, Some("u"), None, None).await?;
        create_task(&pool, pid, "free", None, None, None, Some("u"), None, None).await?;

        // unassigned=true -> only the ownerless task.
        let un = list_tasks(&pool, Some(pid), None, None, true, None, false, None, None, None).await?;
        let un = un.as_array().unwrap();
        assert_eq!(un.len(), 1);
        assert_eq!(un[0]["title"], json!("free"));
        assert!(un[0]["assignee"].is_null());

        // assignee equality still works when unassigned is false.
        let mine = list_tasks(&pool, Some(pid), None, Some("alice"), false, None, false, None, None, None).await?;
        let mine = mine.as_array().unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["title"], json!("owned"));

        // unassigned=true wins over a contradictory assignee= filter (no owner beats owner=alice).
        let both = list_tasks(&pool, Some(pid), None, Some("alice"), true, None, false, None, None, None).await?;
        let both = both.as_array().unwrap();
        assert_eq!(both.len(), 1);
        assert_eq!(both[0]["title"], json!("free"));

        // No filter returns both.
        assert_eq!(
            list_tasks(&pool, Some(pid), None, None, false, None, false, None, None, None).await?.as_array().unwrap().len(),
            2
        );
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
        let t = create_task(&pool, pid, "T", None, Some("alice"), None, Some("u"), None, None).await?;
        let tid = t["id"].as_i64().unwrap();

        // Clear the owner.
        let cleared =
            update_task(&pool, tid, None, Some(""), None, None, None, Some("u"), None, None, None).await?;
        assert!(cleared["assignee"].is_null(), "assignee should be NULL after unassign");

        let events = get_events(&pool, 0, 100, None).await?;
        let un = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == json!("task.unassigned"))
            .expect("task.unassigned emitted");
        assert_eq!(un["data"]["from"], json!("alice"), "carries the prior owner");

        // Clearing an already-unassigned task does NOT emit a second task.unassigned.
        update_task(&pool, tid, None, Some(""), None, None, None, Some("u"), None, None, None).await?;
        let after = get_events(&pool, 0, 200, None).await?;
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
        let evs = get_events(&pool, 0, 200, None).await?;
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
        let t = create_task(&pool, aid, "T", None, None, None, Some("u"), None, None).await?;
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
        let backlog = get_channel_posts(&pool, cid, 0, 100).await?;
        let posts = backlog.as_array().unwrap();
        assert_eq!(posts.len(), 1, "one post in history");
        assert_eq!(posts[0]["data"]["body"], json!("hello all"));

        // A threaded reply carries the parent seq.
        post_to_channel(&pool, cid, "bob", "hi alice", Some(post_seq), None).await?;
        let backlog = get_channel_posts(&pool, cid, 0, 100).await?;
        let posts = backlog.as_array().unwrap();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[1]["data"]["reply_to"].as_i64(), Some(post_seq));
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
        let posts = get_channel_posts(&pool, cid, 0, 100).await?;
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
        let posts = get_channel_posts(&pool, cid, 0, 100).await?;
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
        let t = create_task(&pool, pid, "T", None, None, None, Some("slack-bridge"), None, None).await?;
        let tid = t["id"].as_i64().unwrap();
        comment_task(&pool, tid, "hi from slack", Some("slack-bridge"), Some("slack:U123")).await?;
        let task = get_task(&pool, tid).await?;
        let c0 = &task["comments"][0];
        assert_eq!(c0["author"], json!("slack-bridge"), "author is the fleet ingester");
        assert_eq!(c0["external_author"], json!("slack:U123"), "attributed to the external human");

        // A channel post carries the same attribution on its event data.
        let ch = create_channel(&pool, "bridge", None, Some("slack-bridge"), None).await?;
        let cid = ch["id"].as_i64().unwrap();
        post_to_channel(&pool, cid, "slack-bridge", "hello", None, Some("slack:U123")).await?;
        let posts = get_channel_posts(&pool, cid, 0, 100).await?;
        assert_eq!(posts[0]["data"]["from"], json!("slack-bridge"));
        assert_eq!(posts[0]["data"]["external_author"], json!("slack:U123"));

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
        assert!(!channel_reflects_out(&json!({}), "concierge"));
        assert!(!channel_reflects_out(&json!({ "direction": "in" }), "concierge"));
        // direction out/both with no allowlist -> the documented ["concierge"] default.
        assert!(channel_reflects_out(&json!({ "direction": "out" }), "concierge"));
        assert!(channel_reflects_out(&json!({ "direction": "both" }), "concierge"));
        assert!(!channel_reflects_out(&json!({ "direction": "out" }), "worker"));
        // An explicit allowlist replaces the default (and thus can EXCLUDE concierge).
        let p = json!({ "direction": "both", "outbound_authors": ["worker"] });
        assert!(channel_reflects_out(&p, "worker"));
        assert!(!channel_reflects_out(&p, "concierge"));
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
        let ev = get_events(&pool, 0, 500, None).await?;
        let r = reflects(&ev, cid);
        assert_eq!(r.len(), 1, "allowed author reflects out");
        assert_eq!(r[0]["data"]["author"], json!("concierge"));
        assert_eq!(r[0]["data"]["body"], json!("to slack"));
        assert!(r[0]["data"]["post_seq"].as_i64().is_some());

        // Denied author -> no new reflect event.
        post_to_channel(&pool, cid, "worker", "internal only", None, None).await?;
        let ev = get_events(&pool, 0, 500, None).await?;
        assert_eq!(reflects(&ev, cid).len(), 1, "denied author stays board-internal");

        // An unconfigured channel never reflects, even for concierge.
        let plain = create_channel(&pool, "plain", None, Some("concierge"), None).await?;
        let pid = plain["id"].as_i64().unwrap();
        post_to_channel(&pool, pid, "concierge", "hi", None, None).await?;
        let ev = get_events(&pool, 0, 500, None).await?;
        assert_eq!(reflects(&ev, pid).len(), 0, "default policy is board-internal");

        // set_channel_props turns reflect-back ON for the plain channel.
        set_channel_props(&pool, pid, json!({ "direction": "out" })).await?;
        post_to_channel(&pool, pid, "concierge", "now out", None, None).await?;
        let ev = get_events(&pool, 0, 500, None).await?;
        assert_eq!(reflects(&ev, pid).len(), 1, "policy configurable after creation");
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
        let posts = get_channel_posts(&pool, cid, 0, 100).await?;
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
        let t = create_task(&pool, pid, "T", None, None, None, Some("concierge"), None, None).await?;
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
        assert_eq!(get_channel_posts(&pool, cid, 0, 100).await?.as_array().unwrap().len(), 2, "root + r1 only");

        // Direction 2: a new task comment -> a thread reply.
        comment_task(&pool, tid, "reply from board", Some("worker"), None).await?;
        let posts = get_channel_posts(&pool, cid, 0, 100).await?;
        let posts = posts.as_array().unwrap();
        assert_eq!(posts.len(), 3, "root + r1 + the mirrored comment");
        let mirrored = posts.iter().find(|p| p["data"]["origin_comment"].is_i64()).unwrap();
        assert_eq!(mirrored["data"]["reply_to"], json!(root_seq));
        assert_eq!(mirrored["data"]["from"], json!("worker"));
        assert_eq!(mirrored["data"]["body"], json!("reply from board"));
        // No echo: the mirrored post did NOT create another task comment (still r1-mirror + worker's).
        assert_eq!(get_task(&pool, tid).await?["comments"].as_array().unwrap().len(), 2, "no echo comment");

        // Safety: a comment on a NON-linked task posts nothing to the channel.
        let solo = create_task(&pool, pid, "solo", None, None, None, Some("worker"), None, None).await?["id"].as_i64().unwrap();
        comment_task(&pool, solo, "unrelated", Some("worker"), None).await?;
        assert_eq!(get_channel_posts(&pool, cid, 0, 100).await?.as_array().unwrap().len(), 3, "unlinked task doesn't post");
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
        let tid = create_task(&pool, pid, "T", None, None, None, Some("creator"), None, None).await?["id"].as_i64().unwrap();
        subscribe(&pool, "watcher", Some(tid), None, None, None, false).await?;

        // op1 comments -> the pure subscriber hears it; the author (op1) does not hear its own.
        comment_task(&pool, tid, "first", Some("op1"), None).await?;
        let w = check_notifications(&pool, "watcher", true, 50, None).await?;
        assert_eq!(w["count"].as_i64(), Some(1), "pure subscriber notified: {w}");
        assert_eq!(w["notifications"][0]["type"], json!("task.commented"));
        assert_eq!(w["notifications"][0]["task_id"], json!(tid), "wake payload carries task_id");
        assert_eq!(check_notifications(&pool, "op1", true, 50, None).await?["count"].as_i64(), Some(0), "author not notified of own comment");

        // Commenting auto-subscribed op1, so it hears a subsequent comment by someone else.
        comment_task(&pool, tid, "second", Some("watcher"), None).await?;
        let o = check_notifications(&pool, "op1", true, 50, None).await?;
        assert_eq!(o["count"].as_i64(), Some(1), "commenter auto-subscribed, hears later comments: {o}");
        assert_eq!(o["notifications"][0]["type"], json!("task.commented"));

        // Every task.commented event carries task_id (the wake/webhook payload's routing key).
        let events = get_events(&pool, 0, 500, None).await?;
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
        let tid = create_task(&pool, pid, "T", None, None, None, Some("a"), None, None).await?["id"].as_i64().unwrap();
        comment_task(&pool, tid, "hi", Some("b"), None).await?;

        let all = get_events(&pool, 0, 500, None).await?;
        assert!(all.as_array().unwrap().len() >= 3, "project.created + task.created + task.commented");

        let by_a = get_events(&pool, 0, 500, Some("a")).await?;
        let by_a = by_a.as_array().unwrap();
        assert!(!by_a.is_empty());
        assert!(by_a.iter().all(|e| e["actor"] == json!("a")), "only actor a: {by_a:?}");
        assert!(by_a.iter().any(|e| e["type"] == json!("task.created")));

        let by_b = get_events(&pool, 0, 500, Some("b")).await?;
        let by_b = by_b.as_array().unwrap();
        assert_eq!(by_b.len(), 1, "b only authored the comment");
        assert_eq!(by_b[0]["type"], json!("task.commented"));
        assert_eq!(by_b[0]["actor"], json!("b"));

        assert!(get_events(&pool, 0, 500, Some("nobody")).await?.as_array().unwrap().is_empty());
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
        let t = create_task(&pool, pid, "Ship it", None, None, None, Some("alice"), None, None).await?;
        let tid = t["id"].as_i64().unwrap();
        let dep = create_task(&pool, pid, "Dependency", None, None, None, Some("bob"), None, None).await?;
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
        let on_agent = list_tasks(&pool, Some(pid), None, None, false, None, false, None, Some("agent"), Some("agent:rev")).await?;
        assert_eq!(on_agent.as_array().unwrap().len(), 1);

        // Block on the OPERATOR -> ref is null, and the operator view lists it.
        update_task(&pool, tid, Some("blocked"), None, None, None, None, Some("alice"), None, None,
            Some(json!({"kind": "operator"}))).await?;
        let bo = get_task(&pool, tid).await?["blocked_on"].clone();
        assert_eq!(bo["kind"], json!("operator"));
        assert!(bo["target"].is_null());
        let on_op = list_tasks(&pool, None, None, None, false, None, false, None, Some("operator"), None).await?;
        assert!(on_op.as_array().unwrap().iter().any(|t| t["id"] == json!(tid)));

        // Leaving blocked clears blocked_on.
        update_task(&pool, tid, Some("in_progress"), None, None, None, None, Some("alice"), None, None, None).await?;
        assert!(get_task(&pool, tid).await?["blocked_on"].is_null(), "unblocking clears blocked_on");
        Ok(())
    }
}
