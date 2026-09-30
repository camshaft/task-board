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
    /// Whether this event is ACTIONABLE for this recipient (assignee / @mentioned / a direct
    /// message) vs a passive FYI (a broad subscriber or the firehose). The host notifier wakes
    /// the agent only on an actionable push, so collaboration (a comment to the assignee, an
    /// @mention) wakes instantly while FYI fan-out stays quiet (no wake drain). See
    /// [`actionable_recipients`].
    pub actionable: bool,
}

/// Recipients that get an explicit set (possibly empty) rather than task-derived.
pub enum Recipients {
    /// Derive from the task (subscribers, project subscribers, assignee, creator).
    FromTask,
    /// Derive from the project (its subscribers). For project-level changes (rename,
    /// archive, ...) that aren't tied to a single task.
    FromProject(i64),
    /// Derive from the channel (its subscribers = its members). For channel posts and DMs.
    FromChannel(i64),
    /// Derive from the document (its subscribers). For document publishes and review activity.
    FromDocument(i64),
    /// Union of a document's and a task's recipients — for a doc<->task attachment, so both a
    /// doc watcher and a task watcher learn about the link. (document_id, task_id)
    FromDocumentAndTask(i64, i64),
    /// An explicit set — e.g. a silent project.created.
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

/// Who hears about a channel post: the channel's subscribers (its members), minus the
/// poster. Membership IS subscription — joining a channel is a `channel` subscription row.
async fn recipients_for_channel(
    tx: &mut Transaction<'_, Sqlite>,
    channel_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<BTreeSet<String>> {
    let mut recips: BTreeSet<String> = BTreeSet::new();
    let subs = sqlx::query(
        "SELECT subscriber FROM subscriptions WHERE target_type='channel' AND target_id=?",
    )
    .bind(channel_id)
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

/// Who hears about a document change: its subscribers, minus whoever performed the action.
/// Document subscription is the same machinery as channels — a `document` subscription row.
/// Who hears about a document change (a comment, a new version, a review action): its owner
/// (creator) and its subscribers — minus whoever performed the action. The owner is included
/// explicitly, like a task's created_by, so they always hear about their own document even if a
/// subscription row is missing (e.g. a doc created before auto-subscribe existed).
async fn recipients_for_document(
    tx: &mut Transaction<'_, Sqlite>,
    document_id: i64,
    actor: Option<&str>,
) -> anyhow::Result<BTreeSet<String>> {
    let mut recips: BTreeSet<String> = BTreeSet::new();
    if let Some(row) = sqlx::query("SELECT created_by FROM documents WHERE id=?")
        .bind(document_id)
        .fetch_optional(&mut **tx)
        .await?
    {
        if let Ok(Some(owner)) = row.try_get::<Option<String>, _>("created_by") {
            recips.insert(owner);
        }
    }
    let subs = sqlx::query(
        "SELECT subscriber FROM subscriptions WHERE target_type='document' AND target_id=?",
    )
    .bind(document_id)
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
        // Muted agents detach from this task's fan-out even though they're the
        // creator/assignee/subscriber (a stood-down owner opting out of FYI wakes).
        let muted = sqlx::query("SELECT agent FROM task_mutes WHERE task_id=?")
            .bind(task_id)
            .fetch_all(&mut **tx)
            .await?;
        for m in muted {
            recips.remove(&m.try_get::<String, _>("agent")?);
        }
    }
    if let Some(actor) = actor {
        recips.remove(actor);
    }
    Ok(recips)
}

/// The subset of `recips` for whom this event is ACTIONABLE (they are expected to do something)
/// rather than a passive FYI. Actionable = a direct message to them, the task's assignee on task
/// activity, a freshly-assigned assignee, or an `@id` mention in the event's body/summary/note.
/// The host notifier wakes an idle agent only on an actionable push, so collaboration wakes
/// instantly without the broad-subscriber wake drain that gated FYI wakes (#215).
async fn actionable_recipients(
    tx: &mut Transaction<'_, Sqlite>,
    event_type: &str,
    task_id: Option<i64>,
    data: &Value,
    recips: &BTreeSet<String>,
) -> anyhow::Result<BTreeSet<String>> {
    // A direct message is inherently actionable for whoever receives it.
    if event_type.starts_with("message.") {
        return Ok(recips.clone());
    }
    let mut out: BTreeSet<String> = BTreeSet::new();
    // The task's current assignee is an actionable target for any activity on their task.
    if let Some(tid) = task_id {
        if let Some(row) = sqlx::query("SELECT assignee FROM tasks WHERE id=?")
            .bind(tid)
            .fetch_optional(&mut **tx)
            .await?
        {
            if let Some(a) = row.try_get::<Option<String>, _>("assignee")? {
                if recips.contains(&a) {
                    out.insert(a);
                }
            }
        }
    }
    // A freshly-assigned assignee (task.assigned carries it in data) is actionable even before a
    // re-read would see it.
    if let Some(a) = data.get("assignee").and_then(|v| v.as_str()) {
        if recips.contains(a) {
            out.insert(a.to_string());
        }
    }
    // `@id` mentions in any human-authored text on the event -> actionable for the mentioned id.
    let text: String = ["body", "summary", "note"]
        .iter()
        .filter_map(|k| data.get(*k).and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join(" ");
    if !text.is_empty() {
        for r in recips {
            if text.contains(&format!("@{r}")) {
                out.insert(r.clone());
            }
        }
    }
    Ok(out)
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
    channel_id: Option<i64>,
    document_id: Option<i64>,
    data: Value,
    recipients: Recipients,
) -> anyhow::Result<i64> {
    let ts = now_iso();
    let seq: i64 = sqlx::query(
        "INSERT INTO events(type, actor, project_id, task_id, channel_id, document_id, data, created_at) \
         VALUES(?,?,?,?,?,?,?,?) RETURNING seq",
    )
    .bind(r#type)
    .bind(actor)
    .bind(project_id)
    .bind(task_id)
    .bind(channel_id)
    .bind(document_id)
    .bind(data.to_string())
    .bind(&ts)
    .fetch_one(&mut **tx)
    .await?
    .try_get("seq")?;

    let mut recips = match recipients {
        Recipients::Explicit(set) => set,
        Recipients::FromTask => match task_id {
            Some(tid) => recipients_for_task(tx, tid, actor).await?,
            None => BTreeSet::new(),
        },
        Recipients::FromProject(pid) => recipients_for_project(tx, pid, actor).await?,
        Recipients::FromChannel(cid) => recipients_for_channel(tx, cid, actor).await?,
        Recipients::FromDocument(did) => recipients_for_document(tx, did, actor).await?,
        Recipients::FromDocumentAndTask(did, tid) => {
            let mut set = recipients_for_document(tx, did, actor).await?;
            set.extend(recipients_for_task(tx, tid, actor).await?);
            set
        }
    };

    // Whole-board firehose: anyone subscribed with target_type='board' receives EVERY event,
    // regardless of the per-event recipient set above (even otherwise-silent Explicit events).
    // Minus the actor, so an agent isn't notified of its own action.
    let board_subs =
        sqlx::query("SELECT subscriber FROM subscriptions WHERE target_type='board'")
            .fetch_all(&mut **tx)
            .await?;
    for s in board_subs {
        let sub: String = s.try_get("subscriber")?;
        if Some(sub.as_str()) != actor {
            recips.insert(sub);
        }
    }

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
        // Who is this event actionable for (wake now) vs a passive FYI (inbox only)? Computed once
        // over the final recipient set, then stamped per recipient on both push paths.
        let actionable = actionable_recipients(tx, r#type, task_id, &data, &recips).await?;
        let payload = serde_json::json!({
            "event_seq": seq,
            "type": r#type,
            "actor": actor,
            "task_id": task_id,
            "project_id": project_id,
            "channel_id": channel_id,
            "document_id": document_id,
            "data": data,
            "created_at": ts,
        });
        for r in &recips {
            let is_actionable = actionable.contains(r);
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
                    actionable: is_actionable,
                });
            }

            // Best-effort live-tunnel wake: for a recipient reachable over a reverse tunnel,
            // push the same notification (with `recipient` + `actionable` set, matching the webhook
            // body) as a `req` frame the daemon replays locally — so an idle agent wakes without
            // polling. A missing/failed tunnel is fine: the inbox row above + the poll deliver it.
            let mut wake = payload.clone();
            if let Value::Object(ref mut m) = wake {
                m.insert("recipient".into(), Value::String(r.clone()));
                m.insert("actionable".into(), Value::Bool(is_actionable));
            }
            crate::tunnel::try_wake(r, &wake);
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
                m.insert("actionable".into(), Value::Bool(h.actionable));
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// actionable_recipients marks the assignee + @mentioned ids on task activity, every recipient
    /// of a direct message, and a freshly-assigned assignee — while a plain subscriber is FYI.
    #[tokio::test]
    async fn actionable_targets_assignee_mention_and_dm() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = crate::core::create_project(&pool, "P", None, Some("owner"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        // Task assigned to alice, created by owner.
        let t = crate::core::create_task(&pool, pid, "T", None, Some("alice"), None, Some("owner"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();

        let recips: BTreeSet<String> =
            ["alice", "bob", "carol", "owner"].iter().map(|s| s.to_string()).collect();
        let mut tx = pool.begin().await?;

        // task.commented: assignee (alice) + an @mentioned id (carol) are actionable; a plain
        // subscriber (bob) and the creator without a mention (owner) are FYI.
        let data = serde_json::json!({ "body": "hey @carol take a look", "comment_id": 1 });
        let a = actionable_recipients(&mut tx, "task.commented", Some(tid), &data, &recips).await?;
        assert!(a.contains("alice"), "assignee actionable: {a:?}");
        assert!(a.contains("carol"), "mentioned actionable: {a:?}");
        assert!(!a.contains("bob"), "plain subscriber is FYI: {a:?}");
        assert!(!a.contains("owner"), "creator without mention is FYI: {a:?}");

        // A direct message is actionable for every recipient.
        let dm = actionable_recipients(&mut tx, "message.direct", None, &json!({ "body": "hi" }), &recips)
            .await?;
        assert_eq!(dm, recips, "every DM recipient is actionable");

        // task.assigned: the freshly-assigned assignee (from data) is actionable.
        let asg = actionable_recipients(&mut tx, "task.assigned", Some(tid), &json!({ "assignee": "bob" }), &recips)
            .await?;
        assert!(asg.contains("bob"), "new assignee actionable: {asg:?}");
        assert!(asg.contains("alice"), "current assignee also actionable: {asg:?}");
        tx.commit().await?;
        Ok(())
    }
}
