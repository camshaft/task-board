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
    if has_fields && !status_changed && !reassigned && !unassigned {
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

        let comments = sqlx::query(
            "SELECT id, author, body, created_at, external_author FROM comments WHERE task_id=? ORDER BY id",
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
) -> anyhow::Result<Value> {
    let mut q = String::from(
        "SELECT id, project_id, title, status, assignee, priority, parent_id, updated_at FROM tasks",
    );
    let mut conds: Vec<&str> = Vec::new();
    if project_id.is_some() {
        conds.push("project_id=?");
    }
    if status.is_some() {
        conds.push("status=?");
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

    let ch = sqlx::query("SELECT dm_key FROM channels WHERE id=?")
        .bind(channel_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(ch) = ch else {
        anyhow::bail!("no channel {channel_id}");
    };
    let is_dm: Option<String> = ch.try_get("dm_key")?;
    let evtype = if is_dm.is_some() { "message.direct" } else { "channel.post" };

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
    sqlx::query("UPDATE channels SET updated_at=? WHERE id=?")
        .bind(now_iso())
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(json!({ "channel_id": channel_id, "seq": seq }))
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

pub async fn get_events(pool: &Pool, since_seq: i64, limit: i64) -> anyhow::Result<Value> {
    let rows = sqlx::query("SELECT * FROM events WHERE seq>? ORDER BY seq LIMIT ?")
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
    }
    Ok(Some(d))
}

/// Create a document with its first version. The `cid` is stored verbatim (the board does not
/// resolve or validate it). Returns the document JSON with its current version + version list.
pub async fn create_document(
    pool: &Pool,
    title: &str,
    project_id: Option<i64>,
    cid: &str,
    summary: Option<&str>,
    created_by: Option<&str>,
    metadata: Option<Value>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
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
        "INSERT INTO document_versions(document_id, version_no, cid, summary, created_by, created_at) \
         VALUES(?,1,?,?,?,?) RETURNING id",
    )
    .bind(did)
    .bind(cid)
    .bind(summary)
    .bind(created_by)
    .bind(&ts)
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
    emit(
        &mut tx,
        &mut hooks,
        "document.created",
        created_by,
        None,
        project_id,
        None,
        Some(did),
        json!({ "title": title, "slug": slug, "cid": cid, "version_no": 1 }),
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
pub async fn publish_version(
    pool: &Pool,
    document_id: i64,
    cid: &str,
    summary: Option<&str>,
    created_by: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
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
        "INSERT INTO document_versions(document_id, version_no, cid, summary, created_by, created_at) \
         VALUES(?,?,?,?,?,?) RETURNING id",
    )
    .bind(document_id)
    .bind(next_no)
    .bind(cid)
    .bind(summary)
    .bind(created_by)
    .bind(&ts)
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
    emit(
        &mut tx,
        &mut hooks,
        "document.version_published",
        created_by,
        None,
        project_id,
        None,
        Some(document_id),
        json!({ "version_no": next_no, "cid": cid, "summary": summary, "status": new_status }),
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
        "SELECT id, title, slug, project_id, status, current_version_id, approved_version_id, \
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
pub async fn comment_document(
    pool: &Pool,
    document_id: i64,
    version_id: Option<i64>,
    author: Option<&str>,
    body: &str,
    region: Option<Value>,
    reply_to: Option<i64>,
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
    let cid: i64 = sqlx::query(
        "INSERT INTO document_comments(document_id, version_id, author, body, region, reply_to, created_at) \
         VALUES(?,?,?,?,?,?,?) RETURNING id",
    )
    .bind(document_id)
    .bind(version_id)
    .bind(author)
    .bind(body)
    .bind(&region_str)
    .bind(reply_to)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    auto_subscribe_document(&mut tx, author, document_id).await?;
    emit(
        &mut tx,
        &mut hooks,
        "document.comment",
        author,
        None,
        None,
        None,
        Some(document_id),
        json!({ "comment_id": cid, "version_id": version_id, "body": body, "reply_to": reply_to }),
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
        update_task(&pool, tid, Some("in_progress"), None, None, None, None, Some("fixer"), None, None).await?;
        update_task(&pool, tid, Some("done"), None, None, None, None, Some("fixer"), None, None).await?;
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
        let d2 = publish_version(&pool, did, "bafyv2", Some("revise"), Some("alice")).await?;
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
        let d3 = publish_version(&pool, did, "bafyv3", None, Some("alice")).await?;
        assert_eq!(d3["status"], json!("in_review"), "new version supersedes approval");

        // Missing document -> error.
        assert!(get_document(&pool, 424242).await.is_err());
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
        let d = create_document(&pool, "Spec", None, "bafy1", None, Some("alice"), None).await?;
        let did = d["id"].as_i64().unwrap();
        // bob explicitly subscribes to the document.
        subscribe(&pool, "bob", None, None, None, Some(did), false).await?;

        // carol publishes v2 -> author (alice) + subscriber (bob) hear it; carol (actor) does not.
        publish_version(&pool, did, "bafy2", Some("second"), Some("carol")).await?;

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
        publish_version(&pool, did, "bafy3", None, Some("alice")).await?;
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
        let d = create_document(&pool, "Spec", None, "bafy1", None, Some("alice"), None).await?;
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
            comment_document(&pool, did, Some(vid), Some("carol"), "typo", Some(region.clone()), None)
                .await?;
        let cid = c["id"].as_i64().unwrap();
        assert_eq!(c["status"], json!("open"));
        assert_eq!(c["region"], region, "region round-trips as JSON");
        assert_eq!(c["version_id"], json!(vid));

        // A doc-level comment (no region), threaded under the first.
        let c2 = comment_document(&pool, did, None, Some("dave"), "agreed", None, Some(cid)).await?;
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

        // Errors: comment on a missing doc, resolve a missing comment.
        assert!(comment_document(&pool, 999, None, Some("x"), "hi", None, None).await.is_err());
        assert!(resolve_comment(&pool, 999, Some("x")).await.is_err());
        Ok(())
    }

    /// The review loop: submit -> request_changes -> publish (reopens) -> approve stamps the
    /// current version -> publishing again reopens review. Each transition notifies subscribers.
    #[tokio::test]
    async fn document_review_workflow() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let d = create_document(&pool, "Design", None, "bafy1", None, Some("alice"), None).await?;
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
        let v2 = publish_version(&pool, did, "bafy2", Some("addressed"), Some("alice")).await?;
        assert_eq!(v2["status"], json!("in_review"), "a new version reopens review");
        let v2id = v2["current_version"]["id"].as_i64().unwrap();

        // Operator approves -> stamps the current version.
        let ap = approve_document(&pool, did, Some("operator")).await?;
        assert_eq!(ap["status"], json!("approved"));
        assert_eq!(ap["approved_version_id"], json!(v2id), "approval stamps the current version");
        assert_eq!(ap["approved_by"], json!("operator"));

        // Approval is a stamp, not a lock: publishing again reopens review but keeps the stamp.
        let v3 = publish_version(&pool, did, "bafy3", None, Some("alice")).await?;
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
        let d = create_document(&pool, "Widget design", None, "bafy1", None, Some("bob"), None).await?;
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
            Some(json!({ "tags": ["design", "rfc"] })),
        ).await?;
        let aid = a["id"].as_i64().unwrap();
        let b = create_document(
            &pool, "B", None, "bafyB", None, Some("bob"), Some(json!({ "tags": ["ops"] })),
        ).await?;
        let bid = b["id"].as_i64().unwrap();
        let c = create_document(&pool, "C", Some(pid), "bafyC", None, Some("alice"), None).await?;
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
        update_task(&pool, c1id, Some("done"), None, None, None, None, Some("u"), None, None).await?;
        assert_eq!(get_task(&pool, eid).await?["child_rollup"], json!({ "done": 1, "total": 2 }));

        // Child surfaces parent_id + parent_title.
        let c = get_task(&pool, c1id).await?;
        assert_eq!(c["parent_id"], json!(eid));
        assert_eq!(c["parent_title"], json!("Epic"));

        // list_tasks top_level -> only the epic; parent_id -> the two children.
        assert_eq!(ids(&list_tasks(&pool, Some(pid), None, None, false, None, true, None).await?), vec![eid]);
        assert_eq!(
            ids(&list_tasks(&pool, Some(pid), None, None, false, Some(eid), false, None).await?),
            vec![c1id, c2id]
        );

        // Guards: self-parent + cycle rejected.
        assert!(update_task(&pool, eid, None, None, None, None, None, Some("u"), None, Some(eid)).await.is_err());
        assert!(update_task(&pool, eid, None, None, None, None, None, Some("u"), None, Some(c1id)).await.is_err());

        // Clear c2's parent (parent_id=0) -> top-level; emits task.reparented; roll-up shrinks.
        let r = update_task(&pool, c2id, None, None, None, None, None, Some("u"), None, Some(0)).await?;
        assert!(r["parent_id"].is_null());
        assert_eq!(get_task(&pool, eid).await?["child_rollup"], json!({ "done": 1, "total": 1 }));
        let evs = get_events(&pool, 0, 200).await?;
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
            titles(&list_tasks(&pool, None, None, None, false, None, false, Some("widget")).await?),
            vec!["Fix the widget pipeline".to_string(), "Widget docs".to_string()]
        );
        // Matches description too.
        assert_eq!(
            titles(&list_tasks(&pool, None, None, None, false, None, false, Some("reflow")).await?),
            vec!["Fix the widget pipeline".to_string()]
        );
        // Composable with assignee: widget + alice -> only the Beta doc task.
        assert_eq!(
            titles(&list_tasks(&pool, None, None, Some("alice"), false, None, false, Some("widget")).await?),
            vec!["Widget docs".to_string()]
        );
        // Composable with project scope: widget in Alpha -> only the pipeline task.
        assert_eq!(
            titles(&list_tasks(&pool, Some(pid1), None, None, false, None, false, Some("widget")).await?),
            vec!["Fix the widget pipeline".to_string()]
        );
        // No match -> empty.
        assert!(list_tasks(&pool, None, None, None, false, None, false, Some("zzznope"))
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
        update_task(&pool, tid, None, None, None, None, None, None, Some(json!({"c": 3})), None).await?;

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
        let tasks = list_tasks(&pool, Some(1), None, None, false, None, false, None).await?;
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
        assert_eq!(list_tasks(&pool, Some(aid), None, None, false, None, false, None).await?.as_array().unwrap().len(), 0);
        assert_eq!(list_tasks(&pool, Some(bid), None, None, false, None, false, None).await?.as_array().unwrap().len(), 1);

        // A task.moved event was recorded carrying both ends.
        let events = get_events(&pool, 0, 100).await?;
        let ev = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == json!("task.moved"))
            .expect("task.moved emitted");
        assert_eq!(ev["data"]["from_project_id"], json!(aid));
        assert_eq!(ev["data"]["to_project_id"], json!(bid));

        // Moving onto the current project is a no-op (no new event, still on B).
        let before = get_events(&pool, 0, 100).await?.as_array().unwrap().len();
        move_task(&pool, tid, bid, Some("u")).await?;
        let after = get_events(&pool, 0, 100).await?.as_array().unwrap().len();
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
        let un = list_tasks(&pool, Some(pid), None, None, true, None, false, None).await?;
        let un = un.as_array().unwrap();
        assert_eq!(un.len(), 1);
        assert_eq!(un[0]["title"], json!("free"));
        assert!(un[0]["assignee"].is_null());

        // assignee equality still works when unassigned is false.
        let mine = list_tasks(&pool, Some(pid), None, Some("alice"), false, None, false, None).await?;
        let mine = mine.as_array().unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["title"], json!("owned"));

        // unassigned=true wins over a contradictory assignee= filter (no owner beats owner=alice).
        let both = list_tasks(&pool, Some(pid), None, Some("alice"), true, None, false, None).await?;
        let both = both.as_array().unwrap();
        assert_eq!(both.len(), 1);
        assert_eq!(both[0]["title"], json!("free"));

        // No filter returns both.
        assert_eq!(
            list_tasks(&pool, Some(pid), None, None, false, None, false, None).await?.as_array().unwrap().len(),
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
            update_task(&pool, tid, None, Some(""), None, None, None, Some("u"), None, None).await?;
        assert!(cleared["assignee"].is_null(), "assignee should be NULL after unassign");

        let events = get_events(&pool, 0, 100).await?;
        let un = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == json!("task.unassigned"))
            .expect("task.unassigned emitted");
        assert_eq!(un["data"]["from"], json!("alice"), "carries the prior owner");

        // Clearing an already-unassigned task does NOT emit a second task.unassigned.
        update_task(&pool, tid, None, Some(""), None, None, None, Some("u"), None, None).await?;
        let after = get_events(&pool, 0, 200).await?;
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
        update_task(&pool, tid, None, Some("bob"), None, None, None, Some("u"), None, None).await?;
        let evs = get_events(&pool, 0, 200).await?;
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
}
