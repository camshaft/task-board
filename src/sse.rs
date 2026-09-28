//! Server-Sent Events: a live push feed of board activity for the web UI.
//!
//! A single background *tailer* task follows the append-only `events` table by `seq` and
//! publishes each new row to an in-process `broadcast` bus; every SSE connection subscribes
//! to that bus. Driving the feed off the committed event log (rather than instrumenting each
//! mutation) means we (a) never broadcast an uncommitted change — a row only has a `seq`
//! after its transaction commits — and (b) pick up *every* writer for free: REST, MCP
//! agents, even the dedup CLI. The cost is up-to-`TAIL_INTERVAL` latency, fine for a board.
//!
//! Clients don't branch on event types; the payload carries just enough (`type`,
//! `project_id`, `task_id`, `seq`) for the UI's single invalidation choke point to refresh
//! the affected resources. `seq` doubles as the SSE event id, so a reconnecting client
//! replays what it missed via Last-Event-ID straight out of the events table.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use sqlx::Row;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tokio_stream::StreamExt;

use crate::db::Pool;

/// How many undelivered events the bus buffers per subscriber before a slow client is told
/// to resync. Generous: bursts (a script creating many tasks) shouldn't trip it.
const BUS_CAPACITY: usize = 512;
/// How often the tailer polls the events table for new rows.
const TAIL_INTERVAL: Duration = Duration::from_millis(500);
/// Max rows the tailer pulls per tick, and the ceiling on per-connection replay.
const BATCH: i64 = 512;

/// The compact event pushed to browsers. Not the full event row — just the routing info the
/// client needs to invalidate the right resources (it refetches the data itself).
#[derive(Clone, Debug, Serialize)]
pub struct StreamEvent {
    pub seq: i64,
    #[serde(rename = "type")]
    pub r#type: String,
    pub project_id: Option<i64>,
    pub task_id: Option<i64>,
}

/// Create the broadcast bus. The returned sender is cloned into app state (SSE handlers
/// subscribe to it) and handed to the tailer (which publishes to it).
pub fn channel() -> broadcast::Sender<StreamEvent> {
    broadcast::channel(BUS_CAPACITY).0
}

/// Start the background tailer: poll the events table for rows after the last seen `seq` and
/// publish each to the bus. Seeded at the current max seq so it streams only *new* activity
/// (history is available to clients via replay, not re-broadcast on boot).
pub fn spawn_tailer(pool: Pool, tx: broadcast::Sender<StreamEvent>) {
    tokio::spawn(async move {
        let mut last = max_seq(&pool).await;
        let mut ticker = tokio::time::interval(TAIL_INTERVAL);
        loop {
            ticker.tick().await;
            match fetch_since(&pool, last, BATCH).await {
                Ok(events) => {
                    for ev in events {
                        last = ev.seq;
                        // Err just means no one's connected right now — the event stays in
                        // the log and any future client replays it. Nothing to do.
                        let _ = tx.send(ev);
                    }
                }
                Err(e) => tracing::warn!("[task-board] sse tailer query failed: {e}"),
            }
        }
    });
}

/// Build the SSE response for one connection: replay whatever the client missed (if it
/// reconnected with a Last-Event-ID), then stream live events from the bus. `rx` must be
/// subscribed by the caller *before* this runs so no event slips through the gap between
/// the replay snapshot and going live.
pub async fn stream(
    pool: Pool,
    rx: broadcast::Receiver<StreamEvent>,
    last_event_id: Option<i64>,
) -> Response {
    // `cutoff` is the highest seq already accounted for by the replay portion; live events
    // are filtered to seq > cutoff so nothing is delivered twice.
    let mut cutoff = last_event_id.unwrap_or(0);
    let replay: Vec<Result<Event, Infallible>> = match last_event_id {
        // Reconnect: replay the gap. If it's too big to send row-by-row, tell the client to
        // resync (drop everything and refetch) instead of flooding it.
        Some(since) => match fetch_since(&pool, since, BATCH + 1).await {
            Ok(events) if events.len() as i64 > BATCH => vec![Ok(resync_event())],
            Ok(events) => {
                if let Some(last) = events.last() {
                    cutoff = last.seq;
                }
                events.iter().map(|e| Ok(sse_event(e))).collect()
            }
            Err(e) => {
                tracing::warn!("[task-board] sse replay failed: {e}");
                vec![Ok(resync_event())]
            }
        },
        // First connection: start live from now. The client's initial REST fetches are its
        // baseline; SSE only needs to deliver changes from here forward.
        None => {
            cutoff = max_seq(&pool).await;
            Vec::new()
        }
    };

    let live = BroadcastStream::new(rx).filter_map(move |r| match r {
        Ok(ev) if ev.seq > cutoff => Some(Ok::<_, Infallible>(sse_event(&ev))),
        Ok(_) => None, // already covered by replay
        // The client fell behind the buffer and lost events — have it resync from scratch.
        Err(BroadcastStreamRecvError::Lagged(n)) => {
            tracing::debug!("[task-board] sse client lagged by {n} events; asking it to resync");
            Some(Ok(resync_event()))
        }
    });

    let body = tokio_stream::iter(replay).chain(live);
    Sse::new(body)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("keep-alive"))
        .into_response()
}

/// Render a `StreamEvent` as an SSE message. `id` = seq so the browser reports it as
/// Last-Event-ID on reconnect; the JSON body carries the routing fields.
fn sse_event(ev: &StreamEvent) -> Event {
    Event::default()
        .id(ev.seq.to_string())
        .json_data(ev)
        .unwrap_or_else(|_| Event::default().comment("unserializable event"))
}

/// A "drop your caches and refetch everything" signal — no id, so the client keeps its last
/// real Last-Event-ID for the next reconnect.
fn resync_event() -> Event {
    Event::default()
        .json_data(serde_json::json!({ "type": "resync" }))
        .unwrap_or_else(|_| Event::default().comment("resync"))
}

/// Events with seq greater than `since`, oldest first, capped at `limit`.
async fn fetch_since(pool: &Pool, since: i64, limit: i64) -> anyhow::Result<Vec<StreamEvent>> {
    let rows = sqlx::query(
        "SELECT seq, type, project_id, task_id FROM events WHERE seq > ? ORDER BY seq LIMIT ?",
    )
    .bind(since)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(StreamEvent {
                seq: r.try_get("seq")?,
                r#type: r.try_get("type")?,
                project_id: r.try_get("project_id")?,
                task_id: r.try_get("task_id")?,
            })
        })
        .collect()
}

/// The current highest event seq (0 if the log is empty).
async fn max_seq(pool: &Pool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(seq), 0) FROM events")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core;

    /// The tailer publishes only events committed after it starts, and each carries the
    /// routing fields the UI invalidates on.
    #[tokio::test]
    async fn tailer_broadcasts_new_events() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;

        // A project created before the tailer starts must NOT be re-broadcast (it's history;
        // clients get it from their initial fetch / replay).
        core::create_project(&pool, "old", None, Some("u"), None).await?;

        let tx = channel();
        let mut rx = tx.subscribe();
        spawn_tailer(pool.clone(), tx);

        // A task created after the tailer starts should arrive on the bus.
        let p = core::create_project(&pool, "live", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        let t = core::create_task(&pool, pid, "T", None, None, None, Some("u"), None).await?;
        let tid = t["id"].as_i64().unwrap();

        // Poll the bus (tailer wakes every TAIL_INTERVAL) until we see the task event.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(600), rx.recv()).await {
                Ok(Ok(ev)) => {
                    let is_task = ev.task_id == Some(tid);
                    got.push(ev);
                    if is_task {
                        break;
                    }
                }
                _ => continue,
            }
        }

        let task_ev = got
            .iter()
            .find(|e| e.task_id == Some(tid))
            .expect("task.created should reach the bus");
        assert_eq!(task_ev.r#type, "task.created");
        assert_eq!(task_ev.project_id, Some(pid));
        // The pre-tailer "old" project event was seeded past, so it never shows up.
        assert!(
            got.iter().all(|e| e.r#type != "project.created" || e.project_id == Some(pid)),
            "should not replay events from before the tailer started"
        );
        Ok(())
    }

    /// fetch_since is the replay primitive: strictly-after `since`, oldest-first, capped.
    #[tokio::test]
    async fn fetch_since_returns_gap_in_order() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let pool = crate::db::init(tmp.path().join("b.db").to_str().unwrap()).await?;
        let p = core::create_project(&pool, "p", None, Some("u"), None).await?;
        let pid = p["id"].as_i64().unwrap();
        for i in 0..3 {
            core::create_task(&pool, pid, &format!("t{i}"), None, None, None, Some("u"), None).await?;
        }

        let all = fetch_since(&pool, 0, BATCH).await?;
        assert!(all.len() >= 4, "project.created + 3 task.created");
        // Strictly increasing seq.
        assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));

        // Replaying after the first seq drops it and returns the rest.
        let after_first = fetch_since(&pool, all[0].seq, BATCH).await?;
        assert_eq!(after_first.len(), all.len() - 1);
        assert_eq!(after_first[0].seq, all[1].seq);
        Ok(())
    }
}
