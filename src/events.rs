//! Append-only event log + notification fan-out.
//!
//! Every mutation records an immutable event and delivers it to each recipient's
//! durable inbox (the primary, reliable channel — agents drain it with
//! `check_notifications`). If a recipient registered a `webhook_url`, we ALSO fire a
//! best-effort HTTP POST after the transaction commits. A faithful port of the Python
//! `board.events` module. (Live MCP server->client notifications are intentionally not
//! used: today's Claude clients don't wake an idle agent on them, so the inbox is the
//! real channel.)

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::Value;
use sqlx::{Row, Sqlite, Transaction};

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// A best-effort webhook POST to fire after the enclosing transaction commits.
#[derive(Clone, Debug)]
pub struct WebhookDelivery {
    pub agent_id: String,
    pub url: String,
    pub payload: Value,
}

/// Recipients that get an explicit set (possibly empty) rather than task-derived.
pub enum Recipients {
    /// Derive from the task (subscribers, project subscribers, assignee, creator).
    FromTask,
    /// Derive from the project (its subscribers). For project-level changes (rename,
    /// archive, ...) that aren't tied to a single task.
    FromProject(i64),
    /// An explicit set — e.g. a direct message, or a silent project.created.
    Explicit(BTreeSet<String>),
}

/// Who hears about a project change: its subscribers, minus whoever performed the action.
async fn recipients_for_project(
    tx: &mut Transaction<'_, Sqlite>,
    project_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<BTreeSet<String>> {
    let mut recips: BTreeSet<String> = BTreeSet::new();
    let subs = sqlx::query(
        "SELECT subscriber FROM subscriptions WHERE target_type='project' AND target_id=?",
    )
    .bind(project_id)
    .fetch_all(&mut **tx)
    .await?;
    for s in subs {
        recips.insert(s.try_get::<String, _>("subscriber")?);
    }
    if let Some(actor) = actor {
        recips.remove(actor);
    }
    Ok(recips)
}

/// Who hears about a task change: its subscribers, its project's subscribers, its
/// assignee and creator — minus whoever performed the action.
async fn recipients_for_task(
    tx: &mut Transaction<'_, Sqlite>,
    task_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<BTreeSet<String>> {
    let mut recips: BTreeSet<String> = BTreeSet::new();
    if let Some(row) = sqlx::query("SELECT project_id, assignee, created_by FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_optional(&mut **tx)
        .await?
    {
        let project_id: Option<i64> = row.try_get("project_id")?;
        if let Ok(Some(a)) = row.try_get::<Option<String>, _>("assignee") {
            recips.insert(a);
        }
        if let Ok(Some(c)) = row.try_get::<Option<String>, _>("created_by") {
            recips.insert(c);
        }
        let subs = sqlx::query(
            "SELECT subscriber FROM subscriptions WHERE target_type='task' AND target_id=?",
        )
        .bind(task_id)
        .fetch_all(&mut **tx)
        .await?;
        for s in subs {
            recips.insert(s.try_get::<String, _>("subscriber")?);
        }
        if let Some(pid) = project_id {
            let psubs = sqlx::query(
                "SELECT subscriber FROM subscriptions WHERE target_type='project' AND target_id=?",
            )
            .bind(pid)
            .fetch_all(&mut **tx)
            .await?;
            for s in psubs {
                recips.insert(s.try_get::<String, _>("subscriber")?);
            }
        }
    }
    if let Some(actor) = actor {
        recips.remove(actor);
    }
    Ok(recips)
}

/// Record an event and deliver it to each recipient's inbox. Returns the event seq.
/// Any webhook deliveries are appended to `hooks` to be fired after commit.
#[allow(clippy::too_many_arguments)]
pub async fn emit(
    tx: &mut Transaction<'_, Sqlite>,
    hooks: &mut Vec<WebhookDelivery>,
    r#type: &str,
    actor: Option<&str>,
    task_id: Option<i64>,
    project_id: Option<i64>,
    data: Value,
    recipients: Recipients,
) -> anyhow::Result<i64> {
    let ts = now_iso();
    let seq: i64 = sqlx::query(
        "INSERT INTO events(type, actor, project_id, task_id, data, created_at) \
         VALUES(?,?,?,?,?,?) RETURNING seq",
    )
    .bind(r#type)
    .bind(actor)
    .bind(project_id)
    .bind(task_id)
    .bind(data.to_string())
    .bind(&ts)
    .fetch_one(&mut **tx)
    .await?
    .try_get("seq")?;

    let recips = match recipients {
        Recipients::Explicit(set) => set,
        Recipients::FromTask => match task_id {
            Some(tid) => recipients_for_task(tx, tid, actor).await?,
            None => BTreeSet::new(),
        },
        Recipients::FromProject(pid) => recipients_for_project(tx, pid, actor).await?,
    };

    for r in &recips {
        sqlx::query("INSERT INTO inbox(recipient, event_seq, created_at) VALUES(?,?,?)")
            .bind(r)
            .bind(seq)
            .bind(&ts)
            .execute(&mut **tx)
            .await?;
    }

    // Collect webhook targets (agents in the recipient set with a webhook_url).
    if !recips.is_empty() {
        let payload = serde_json::json!({
            "event_seq": seq,
            "type": r#type,
            "actor": actor,
            "task_id": task_id,
            "project_id": project_id,
            "data": data,
            "created_at": ts,
        });
        for r in &recips {
            if let Some(row) = sqlx::query(
                "SELECT webhook_url FROM agents WHERE id=? AND webhook_url IS NOT NULL AND webhook_url != ''",
            )
            .bind(r)
            .fetch_optional(&mut **tx)
            .await?
            {
                let url: String = row.try_get("webhook_url")?;
                hooks.push(WebhookDelivery {
                    agent_id: r.clone(),
                    url,
                    payload: payload.clone(),
                });
            }
        }
    }

    Ok(seq)
}

/// Fire the accumulated best-effort webhooks in the background. The inbox already has
/// the event, so failures are logged and ignored.
pub fn fire_webhooks(hooks: Vec<WebhookDelivery>, timeout: Duration) {
    if hooks.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let client = match reqwest::Client::builder().timeout(timeout).build() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[task-board] webhook client build failed: {e}");
                return;
            }
        };
        for h in hooks {
            let mut body = h.payload.clone();
            if let Value::Object(ref mut m) = body {
                m.insert("recipient".into(), Value::String(h.agent_id.clone()));
            }
            if let Err(e) = client.post(&h.url).json(&body).send().await {
                tracing::warn!(
                    "[task-board] webhook to {} ({}) failed: {e}",
                    h.agent_id,
                    h.url
                );
            }
        }
    });
}
