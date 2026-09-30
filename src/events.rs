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
    /// Whether this recipient is a DIRECT subscriber of the event's target (a task/project/channel/
    /// document subscription, which includes an auto-subscribed assignee/creator/commenter and an
    /// @mentioned agent) vs present only via the whole-board firehose. The host notifier wakes the
    /// agent on a `subscribed` push for every event type (comments included) — subscription is the
    /// wake control (unsubscribe to opt out), per the operator's subscription-based wake model —
    /// while a firehose-only coordinator stays inbox/poll (not woken on every ticket).
    pub subscribed: bool,
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

/// Parse a subscription's stored `event_classes` (a JSON array of class-name strings, or NULL).
/// NULL / unparseable / empty => None, i.e. "no filter: deliver every event" (the legacy default).
fn parse_event_classes(raw: Option<String>) -> Option<Vec<String>> {
    let v: Vec<String> = serde_json::from_str(raw?.as_str()).ok()?;
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Whether an event (`event_type` + its `data`) falls into any subscriber-declared class (#462).
/// Classes are named bundles mapping to concrete emitted event types; `done` additionally refines
/// on the status transition value. An unknown class name matches nothing (forward-compatible).
fn event_in_classes(event_type: &str, data: &Value, classes: &[String]) -> bool {
    classes.iter().any(|c| match c.as_str() {
        "created" => event_type == "task.created",
        "comment" => event_type == "task.commented",
        "assigned" => event_type == "task.assigned",
        "blocked" => event_type == "task.blocked_on_you",
        "status" => event_type == "task.status_changed",
        // done = a task reaching the terminal "done" status; cancelled is excluded (not "ready").
        "done" => {
            event_type == "task.status_changed"
                && data.get("to").and_then(|v| v.as_str()) == Some("done")
        }
        "review" => event_type.starts_with("review."),
        "doc" => event_type.starts_with("document."),
        _ => false,
    })
}

/// Whether a subscription with stored `raw_classes` should deliver this event. A NULL/empty filter
/// delivers everything (legacy behavior); otherwise only events matching one of its classes.
fn subscription_delivers(event_type: &str, data: &Value, raw_classes: Option<String>) -> bool {
    match parse_event_classes(raw_classes) {
        None => true,
        Some(classes) => event_in_classes(event_type, data, &classes),
    }
}

/// Who hears about a project change: its subscribers, minus whoever performed the action.
async fn recipients_for_project(
    tx: &mut Transaction<'_, Sqlite>,
    project_id: i64,
    actor: Option<&str>,
    event_type: &str,
    data: &Value,
) -> anyhow::Result<BTreeSet<String>> {
    let mut recips: BTreeSet<String> = BTreeSet::new();
    let subs = sqlx::query(
        "SELECT subscriber, event_classes FROM subscriptions WHERE target_type='project' AND target_id=?",
    )
    .bind(project_id)
    .fetch_all(&mut **tx)
    .await?;
    for s in subs {
        if subscription_delivers(event_type, data, s.try_get("event_classes")?) {
            recips.insert(s.try_get::<String, _>("subscriber")?);
        }
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
    event_type: &str,
    data: &Value,
) -> anyhow::Result<BTreeSet<String>> {
    let mut recips: BTreeSet<String> = BTreeSet::new();
    let subs = sqlx::query(
        "SELECT subscriber, event_classes FROM subscriptions WHERE target_type='channel' AND target_id=?",
    )
    .bind(channel_id)
    .fetch_all(&mut **tx)
    .await?;
    for s in subs {
        if subscription_delivers(event_type, data, s.try_get("event_classes")?) {
            recips.insert(s.try_get::<String, _>("subscriber")?);
        }
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
    event_type: &str,
    data: &Value,
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
        "SELECT subscriber, event_classes FROM subscriptions WHERE target_type='document' AND target_id=?",
    )
    .bind(document_id)
    .fetch_all(&mut **tx)
    .await?;
    for s in subs {
        if subscription_delivers(event_type, data, s.try_get("event_classes")?) {
            recips.insert(s.try_get::<String, _>("subscriber")?);
        }
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
    event_type: &str,
    data: &Value,
) -> anyhow::Result<BTreeSet<String>> {
    let mut recips: BTreeSet<String> = BTreeSet::new();
    if let Some(row) = sqlx::query("SELECT project_id, assignee, created_by FROM tasks WHERE id=?")
        .bind(task_id)
        .fetch_optional(&mut **tx)
        .await?
    {
        let project_id: Option<i64> = row.try_get("project_id")?;
        // The assignee and creator are in the fan-out unconditionally (they always hear about their
        // own task); the per-subscription event-class filter (#462) applies only to subscription
        // rows, not to ownership.
        if let Ok(Some(a)) = row.try_get::<Option<String>, _>("assignee") {
            recips.insert(a);
        }
        if let Ok(Some(c)) = row.try_get::<Option<String>, _>("created_by") {
            recips.insert(c);
        }
        let subs = sqlx::query(
            "SELECT subscriber, event_classes FROM subscriptions WHERE target_type='task' AND target_id=?",
        )
        .bind(task_id)
        .fetch_all(&mut **tx)
        .await?;
        for s in subs {
            if subscription_delivers(event_type, data, s.try_get("event_classes")?) {
                recips.insert(s.try_get::<String, _>("subscriber")?);
            }
        }
        if let Some(pid) = project_id {
            let psubs = sqlx::query(
                "SELECT subscriber, event_classes FROM subscriptions WHERE target_type='project' AND target_id=?",
            )
            .bind(pid)
            .fetch_all(&mut **tx)
            .await?;
            for s in psubs {
                if subscription_delivers(event_type, data, s.try_get("event_classes")?) {
                    recips.insert(s.try_get::<String, _>("subscriber")?);
                }
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
            Some(tid) => recipients_for_task(tx, tid, actor, r#type, &data).await?,
            None => BTreeSet::new(),
        },
        Recipients::FromProject(pid) => recipients_for_project(tx, pid, actor, r#type, &data).await?,
        Recipients::FromChannel(cid) => recipients_for_channel(tx, cid, actor, r#type, &data).await?,
        Recipients::FromDocument(did) => recipients_for_document(tx, did, actor, r#type, &data).await?,
        Recipients::FromDocumentAndTask(did, tid) => {
            let mut set = recipients_for_document(tx, did, actor, r#type, &data).await?;
            set.extend(recipients_for_task(tx, tid, actor, r#type, &data).await?);
            set
        }
    };

    // The DIRECT subscribers of this event's target (before the firehose union below): the target's
    // task/project/channel/document subscribers plus an auto-subscribed assignee/creator/commenter/
    // @mention, or the Explicit set. These are the recipients the notifier push-wakes (subscription
    // is the wake control); a firehose-only recipient is added below but is NOT in this set.
    let mut direct: BTreeSet<String> = recips.clone();

    // A whole-board (firehose) subscriber is push-woken on task.created only — the new-task triage
    // signal a board-wide coordinator must react to promptly (#461). It joins the `direct`
    // (subscribed=true) set for THIS event type, so the notifier wakes it on creation instead of
    // leaving it to the next poll / heartbeat. For every other event type a firehose subscriber
    // stays firehose-tier (added to `recips` below but NOT `direct`, so subscribed=false — inbox/
    // poll), preserving the #384 intent of not waking a coordinator on high-volume per-ticket
    // chatter. (The durable, per-subscriber selective event-class filter is #462.)
    let wakes_firehose = r#type == "task.created";

    // Whole-board firehose: anyone subscribed with target_type='board'. An UNFILTERED board
    // subscription (event_classes NULL) receives EVERY event, regardless of the per-event recipient
    // set above (even otherwise-silent Explicit events), at the inbox/poll tier (subscribed=false)
    // — except the #461 task.created triage carve-out. A FILTERED board subscription (#462) receives
    // ONLY events matching its classes, and a match is a genuine wake (subscribed=true) since the
    // subscriber explicitly opted into those classes. Minus the actor either way.
    let board_subs =
        sqlx::query("SELECT subscriber, event_classes FROM subscriptions WHERE target_type='board'")
            .fetch_all(&mut **tx)
            .await?;
    for s in board_subs {
        let sub: String = s.try_get("subscriber")?;
        if Some(sub.as_str()) == actor {
            continue;
        }
        match parse_event_classes(s.try_get("event_classes")?) {
            Some(classes) => {
                if event_in_classes(r#type, &data, &classes) {
                    direct.insert(sub.clone());
                    recips.insert(sub);
                }
            }
            None => {
                if wakes_firehose {
                    direct.insert(sub.clone());
                }
                recips.insert(sub);
            }
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
            let is_subscribed = direct.contains(r);
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
                    subscribed: is_subscribed,
                });
            }

            // Best-effort live-tunnel wake: for a recipient reachable over a reverse tunnel,
            // push the same notification (with `recipient` + `subscribed` set, matching the webhook
            // body) as a `req` frame the daemon replays locally — so an idle agent wakes without
            // polling. A missing/failed tunnel is fine: the inbox row above + the poll deliver it.
            let mut wake = payload.clone();
            if let Value::Object(ref mut m) = wake {
                m.insert("recipient".into(), Value::String(r.clone()));
                m.insert("subscribed".into(), Value::Bool(is_subscribed));
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
                m.insert("subscribed".into(), Value::Bool(h.subscribed));
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

    /// emit stamps `subscribed=true` on a DIRECT target subscriber's wake (so the notifier wakes
    /// them on a comment — the operator's subscription-based model) and `subscribed=false` on a
    /// firehose-only recipient (inbox/poll, not woken on every ticket). Verified via the collected
    /// WebhookDelivery flags.
    #[tokio::test]
    async fn wake_marks_direct_subscribers_not_firehose() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        // Two agents with webhook_urls, so emit produces a WebhookDelivery (carrying `subscribed`)
        // for each.
        crate::core::register_agent(&pool, "alice", None, None, None, None, Some("http://x/wake")).await?;
        crate::core::register_agent(&pool, "coord", None, None, None, None, Some("http://x/wake")).await?;
        let p = crate::core::create_project(&pool, "P", None, Some("owner"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = crate::core::create_task(&pool, pid, "T", None, None, None, Some("owner"), None, None, None).await?;
        let tid = t["id"].as_i64().unwrap();
        // alice subscribes to the task directly; coord subscribes to the whole-board firehose.
        crate::core::subscribe(&pool, "alice", Some(tid), None, None, None, false).await?;
        crate::core::subscribe(&pool, "coord", None, None, None, None, true).await?;

        // A comment by someone else: both are recipients, but only alice is a DIRECT subscriber.
        let mut tx = pool.begin().await?;
        let mut hooks: Vec<WebhookDelivery> = Vec::new();
        emit(
            &mut tx,
            &mut hooks,
            "task.commented",
            Some("owner"),
            Some(tid),
            None,
            None,
            None,
            json!({ "body": "ping", "comment_id": 1 }),
            Recipients::FromTask,
        )
        .await?;
        tx.commit().await?;

        let alice = hooks.iter().find(|h| h.agent_id == "alice").expect("alice hook");
        let coord = hooks.iter().find(|h| h.agent_id == "coord").expect("coord hook");
        assert!(alice.subscribed, "direct task subscriber is woken on a comment");
        assert!(!coord.subscribed, "firehose-only recipient is not push-woken per ticket");
        Ok(())
    }

    /// #461: a whole-board firehose subscriber IS push-woken (subscribed=true) on task.created — the
    /// new-task triage signal — but stays firehose-tier (subscribed=false, inbox/poll) on a
    /// per-ticket event like task.commented, so the #384 "don't wake a coordinator on every ticket"
    /// intent still holds. Verified via the collected WebhookDelivery flags.
    #[tokio::test]
    async fn firehose_woken_on_task_created_only() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        crate::core::register_agent(&pool, "triage", None, None, None, None, Some("http://x/wake")).await?;
        // A whole-board (firehose) subscription — no task/project/etc. target.
        crate::core::subscribe(&pool, "triage", None, None, None, None, true).await?;

        // task.created: triage is woken even though it's only a firehose subscriber (not a direct
        // target subscriber of the new task). Explicit(empty) isolates the firehose path.
        let mut tx = pool.begin().await?;
        let mut hooks: Vec<WebhookDelivery> = Vec::new();
        emit(
            &mut tx,
            &mut hooks,
            "task.created",
            Some("owner"),
            Some(1),
            Some(1),
            None,
            None,
            json!({ "title": "T" }),
            Recipients::Explicit(BTreeSet::new()),
        )
        .await?;
        tx.commit().await?;
        let created = hooks.iter().find(|h| h.agent_id == "triage").expect("triage woken on task.created");
        assert!(created.subscribed, "firehose sub is push-woken (subscribed=true) on task.created");

        // task.commented: triage still RECEIVES it (firehose delivery) but is NOT push-woken.
        let mut tx = pool.begin().await?;
        let mut hooks: Vec<WebhookDelivery> = Vec::new();
        emit(
            &mut tx,
            &mut hooks,
            "task.commented",
            Some("owner"),
            Some(1),
            None,
            None,
            None,
            json!({ "body": "hi" }),
            Recipients::Explicit(BTreeSet::new()),
        )
        .await?;
        tx.commit().await?;
        let commented = hooks.iter().find(|h| h.agent_id == "triage").expect("triage still receives the comment");
        assert!(!commented.subscribed, "firehose sub is NOT push-woken on a per-ticket comment");
        Ok(())
    }

    /// #462: the event-class vocabulary maps to concrete emitted types, `done` refines on the
    /// status transition value (done matches, cancelled/other do not), and an empty/NULL filter is
    /// "no filter" (None).
    #[test]
    fn event_classes_match_expected_types() {
        let done = ["done".to_string()];
        assert!(event_in_classes("task.status_changed", &json!({ "to": "done" }), &done));
        assert!(!event_in_classes("task.status_changed", &json!({ "to": "cancelled" }), &done));
        assert!(!event_in_classes("task.status_changed", &json!({ "to": "in_progress" }), &done));
        assert!(event_in_classes("task.created", &json!({}), &["created".to_string()]));
        assert!(event_in_classes("task.blocked_on_you", &json!({}), &["blocked".to_string()]));
        assert!(event_in_classes("task.assigned", &json!({}), &["assigned".to_string()]));
        assert!(event_in_classes("task.commented", &json!({}), &["comment".to_string()]));
        assert!(event_in_classes("task.status_changed", &json!({ "to": "blocked" }), &["status".to_string()]));
        assert!(event_in_classes("review.status_changed", &json!({}), &["review".to_string()]));
        assert!(event_in_classes("document.approved", &json!({}), &["doc".to_string()]));
        // Non-matching type, and an unknown class name, match nothing.
        assert!(!event_in_classes("task.commented", &json!({}), &["created".to_string()]));
        assert!(!event_in_classes("task.created", &json!({}), &["bogus".to_string()]));
        // NULL / empty filter => None (deliver everything); a real list round-trips.
        assert!(parse_event_classes(None).is_none());
        assert!(parse_event_classes(Some("[]".to_string())).is_none());
        assert_eq!(
            parse_event_classes(Some(r#"["created","blocked"]"#.to_string())),
            Some(vec!["created".to_string(), "blocked".to_string()])
        );
    }

    /// #462: a filtered board subscription ([created]) is delivery-gated — it is delivered + woken
    /// (subscribed=true) on a matching event (task.created), but a non-matching event
    /// (task.commented) is not delivered to it at all: no inbox row, no wake. This is what removes
    /// the firehose inbox-noise, not just the wake.
    #[tokio::test]
    async fn event_class_filter_gates_delivery() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        crate::core::register_agent(&pool, "triage", None, None, None, None, Some("http://x/wake")).await?;
        // A filtered board subscription: only the `created` class.
        crate::core::subscribe_classed(&pool, "triage", None, None, None, None, true, &["created".to_string()])
            .await?;

        // task.created matches: delivered (one inbox row) and woken (subscribed=true).
        let mut tx = pool.begin().await?;
        let mut hooks: Vec<WebhookDelivery> = Vec::new();
        emit(&mut tx, &mut hooks, "task.created", Some("owner"), Some(1), Some(1), None, None,
            json!({ "title": "T" }), Recipients::Explicit(BTreeSet::new())).await?;
        tx.commit().await?;
        let created = hooks.iter().find(|h| h.agent_id == "triage").expect("filtered sub delivered task.created");
        assert!(created.subscribed, "a matching class wakes the filtered subscriber");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inbox WHERE recipient='triage'")
            .fetch_one(&pool).await?;
        assert_eq!(n, 1, "one inbox row for the matching event");

        // task.commented does NOT match: not delivered at all (no new inbox row, no wake).
        let mut tx = pool.begin().await?;
        let mut hooks: Vec<WebhookDelivery> = Vec::new();
        emit(&mut tx, &mut hooks, "task.commented", Some("owner"), Some(1), None, None, None,
            json!({ "body": "hi" }), Recipients::Explicit(BTreeSet::new())).await?;
        tx.commit().await?;
        assert!(hooks.iter().all(|h| h.agent_id != "triage"), "filtered-out event does not wake the subscriber");
        let n2: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inbox WHERE recipient='triage'")
            .fetch_one(&pool).await?;
        assert_eq!(n2, 1, "no new inbox row for the filtered-out event (delivery-gated)");
        Ok(())
    }
}
