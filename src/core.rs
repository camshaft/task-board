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

pub async fn register_agent(
    pool: &Pool,
    agent_id: &str,
    display_name: Option<&str>,
    kind: Option<&str>,
    webhook_url: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let exists = sqlx::query("SELECT 1 FROM agents WHERE id=?")
        .bind(agent_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
    if exists {
        sqlx::query(
            "UPDATE agents SET display_name=COALESCE(?,display_name), kind=COALESCE(?,kind), \
             webhook_url=COALESCE(?,webhook_url), status='online', last_seen=? WHERE id=?",
        )
        .bind(display_name)
        .bind(kind)
        .bind(webhook_url)
        .bind(&ts)
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO agents(id, display_name, kind, status, webhook_url, created_at, last_seen) \
             VALUES(?,?,?,'online',?,?,?)",
        )
        .bind(agent_id)
        .bind(display_name)
        .bind(kind)
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
    let out = row_to_json(&row);
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
    let out = row_to_json(&row);
    tx.commit().await?;
    Ok(out)
}

pub async fn list_agents(pool: &Pool) -> anyhow::Result<Value> {
    let rows = sqlx::query("SELECT * FROM agents ORDER BY last_seen DESC")
        .fetch_all(pool)
        .await?;
    Ok(Value::Array(rows.iter().map(row_to_json).collect()))
}

// --- Projects ---

pub async fn create_project(
    pool: &Pool,
    name: &str,
    description: Option<&str>,
    created_by: Option<&str>,
) -> anyhow::Result<Value> {
    let ts = now_iso();
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let pid: i64 = sqlx::query(
        "INSERT INTO projects(name, description, created_by, created_at, updated_at) \
         VALUES(?,?,?,?,?) RETURNING id",
    )
    .bind(name)
    .bind(description)
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
        json!({ "name": name }),
        Recipients::Explicit(BTreeSet::new()),
    )
    .await?;
    let out = fetch_one_json(&mut tx, "SELECT * FROM projects WHERE id=?", pid)
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
    let meta_str = metadata.unwrap_or_else(|| json!({})).to_string();
    let tid: i64 = sqlx::query(
        "INSERT INTO tasks(project_id, title, description, assignee, priority, created_by, \
         metadata, status, created_at, updated_at) VALUES(?,?,?,?,?,?,?, 'todo', ?, ?) RETURNING id",
    )
    .bind(project_id)
    .bind(title)
    .bind(description)
    .bind(assignee)
    .bind(priority)
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
        for (_, val) in fields.iter() {
            if let Some(v) = val {
                q = q.bind(*v);
            }
        }
        if let Some(ref m) = merged_meta {
            q = q.bind(m);
        }
        q = q.bind(&ts).bind(task_id);
        q.execute(&mut *tx).await?;
    }

    if let Some(a) = assignee {
        auto_subscribe(&mut tx, Some(a), task_id).await?;
    }

    let status_changed = status.is_some() && status != Some(old_status.as_str());
    let assignee_changed = assignee.is_some() && assignee != old_assignee.as_deref();

    if status_changed {
        emit(
            &mut tx,
            &mut hooks,
            "task.status_changed",
            actor,
            Some(task_id),
            Some(old_project_id),
            json!({ "from": old_status, "to": status, "title": old_title }),
            Recipients::FromTask,
        )
        .await?;
    }
    if assignee_changed {
        emit(
            &mut tx,
            &mut hooks,
            "task.assigned",
            actor,
            Some(task_id),
            Some(old_project_id),
            json!({ "assignee": assignee, "title": old_title }),
            Recipients::FromTask,
        )
        .await?;
    }
    if has_fields && !status_changed && !assignee_changed {
        emit(
            &mut tx,
            &mut hooks,
            "task.updated",
            actor,
            Some(task_id),
            Some(old_project_id),
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
            "SELECT id, author, body, created_at FROM comments WHERE task_id=? ORDER BY id",
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
    }
    Ok(d)
}

pub async fn list_tasks(
    pool: &Pool,
    project_id: Option<i64>,
    status: Option<&str>,
    assignee: Option<&str>,
) -> anyhow::Result<Value> {
    let mut q =
        String::from("SELECT id, project_id, title, status, assignee, priority, updated_at FROM tasks");
    let mut conds: Vec<&str> = Vec::new();
    if project_id.is_some() {
        conds.push("project_id=?");
    }
    if status.is_some() {
        conds.push("status=?");
    }
    if assignee.is_some() {
        conds.push("assignee=?");
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
    if let Some(a) = assignee {
        query = query.bind(a);
    }
    let rows = query.fetch_all(pool).await?;
    Ok(Value::Array(rows.iter().map(row_to_json).collect()))
}

pub async fn comment_task(
    pool: &Pool,
    task_id: i64,
    body: &str,
    author: Option<&str>,
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
    let cid: i64 = sqlx::query(
        "INSERT INTO comments(task_id, author, body, created_at) VALUES(?,?,?,?) RETURNING id",
    )
    .bind(task_id)
    .bind(author)
    .bind(body)
    .bind(&ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;
    auto_subscribe(&mut tx, author, task_id).await?;
    emit(
        &mut tx,
        &mut hooks,
        "task.commented",
        author,
        Some(task_id),
        None,
        json!({ "comment_id": cid, "body": body }),
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
) -> anyhow::Result<Value> {
    let (tt, tid) = target(task_id, project_id)?;
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
) -> anyhow::Result<Value> {
    let (tt, tid) = target(task_id, project_id)?;
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

fn target(task_id: Option<i64>, project_id: Option<i64>) -> anyhow::Result<(&'static str, i64)> {
    match (task_id, project_id) {
        (Some(t), _) => Ok(("task", t)),
        (None, Some(p)) => Ok(("project", p)),
        (None, None) => anyhow::bail!("give task_id or project_id"),
    }
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

pub async fn send_message(
    pool: &Pool,
    from_agent: &str,
    to_agent: &str,
    body: &str,
) -> anyhow::Result<Value> {
    let mut tx = pool.begin().await?;
    let mut hooks: Vec<WebhookDelivery> = Vec::new();
    let mut recips = BTreeSet::new();
    recips.insert(to_agent.to_string());
    emit(
        &mut tx,
        &mut hooks,
        "message.direct",
        Some(from_agent),
        None,
        None,
        json!({ "to": to_agent, "body": body }),
        Recipients::Explicit(recips),
    )
    .await?;
    tx.commit().await?;
    fire_webhooks(hooks, webhook_timeout(pool));
    Ok(json!({ "to": to_agent, "delivered": true }))
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

// The webhook timeout is carried on the pool's app state; we thread it via a thread-local
// set at startup to avoid changing every signature. See `main.rs`.
fn webhook_timeout(_pool: &Pool) -> Duration {
    crate::WEBHOOK_TIMEOUT
        .get()
        .copied()
        .unwrap_or_else(|| Duration::from_secs(5))
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

        register_agent(&pool, "planner", Some("Planner"), None, None).await?;
        register_agent(&pool, "fixer", Some("Fixer"), None, None).await?;

        let p = create_project(&pool, "Voron tuning", Some("dial in the printer"), Some("planner")).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(&pool, pid, "Calibrate pressure advance", None, Some("fixer"), None, Some("planner"), None).await?;
        let tid = t["id"].as_i64().unwrap();

        subscribe(&pool, "planner", Some(tid), None).await?; // (already auto-subscribed as creator)
        comment_task(&pool, tid, "Start from PA=0.03", Some("planner")).await?;
        update_task(&pool, tid, Some("in_progress"), None, None, None, None, Some("fixer"), None).await?;
        update_task(&pool, tid, Some("done"), None, None, None, None, Some("fixer"), None).await?;
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

    /// metadata is merged (not overwritten) on update_task and set_task_props.
    #[tokio::test]
    async fn metadata_merges() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = create_project(&pool, "P", None, None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = create_task(&pool, pid, "T", None, None, None, None, Some(json!({"a": 1}))).await?;
        let tid = t["id"].as_i64().unwrap();

        set_task_props(&pool, tid, json!({"b": 2})).await?;
        update_task(&pool, tid, None, None, None, None, None, None, Some(json!({"c": 3}))).await?;

        let task = get_task(&pool, tid).await?;
        assert_eq!(task["metadata"], json!({"a": 1, "b": 2, "c": 3}));
        Ok(())
    }
}
