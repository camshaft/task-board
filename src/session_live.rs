//! Live session attach: the ephemeral per-agent frame fan-out behind doc_3426 ask 7 (task_1462).
//!
//! An attacher reads a running agent's live transcript + thought-process frames; the headless
//! harness pushes those frames to the board while at least one attacher is present. The frames are
//! EPHEMERAL and best-effort: they fan out over an in-process per-agent `broadcast` channel and are
//! never persisted. The durable recovery substrate is the `transcript_chunks` log (task_1463), a
//! separate concern -- an attacher that misses frames re-pulls that log to fill the gap.
//!
//! Backpressure invariant: a slow or disconnected attacher never applies backpressure to the
//! agent's turn loop or to other attachers. The broadcast ring drops the oldest buffered frames for
//! a lagging receiver and surfaces the drop as a `RecvError::Lagged(n)`, which the attach stream
//! turns into a gap marker so the attacher knows the dropped `frame_seq` range to re-pull.
//!
//! Durability split: only the attach-state transitions (first-attach / last-detach) and the
//! steer/abort control items are durable, delivered to the harness as events on the existing
//! emit + inbox + push path (see `core::{attach_session, detach_session, steer_session,
//! abort_session}`). This module holds just the in-memory frame bus; the authoritative
//! reference count that decides push-on / push-off lives in the durable `session_attachments`
//! table, not in this bus's live receiver count.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

/// Per-agent ring depth: how many undelivered frames the bus buffers before the oldest are dropped
/// for a lagging attacher (which then sees a gap and re-pulls the chunk log). Bounded so a slow
/// attacher can never grow memory without limit or stall the fan-out.
const FRAME_RING: usize = 256;

/// The kind of live frame, matching the two-channel shape agreed with v-fleet-tooling: an appended
/// transcript delta, or a thought-process / tool-call trace frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FrameKind {
    TranscriptDelta,
    ThoughtProcess,
}

/// One live frame the harness pushes and attachers receive. `frame_seq` is monotonic per agent
/// session, so an attacher that was dropped detects the gap (a jump in `frame_seq`) and re-pulls
/// the durable chunk log for the missed range. `session_id` + `turn_id` are the harness-internal
/// coordinates; the board treats them as opaque routing / labelling.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub frame_seq: i64,
    pub kind: FrameKind,
    pub payload: Value,
}

/// The in-process per-agent frame fan-out registry: one `broadcast` channel per agent that
/// currently has frames flowing. The harness push endpoint sends into the agent's channel; every
/// attached reader holds a `broadcast::Receiver`. A channel is created lazily on first use and
/// reclaimed once it has no receivers, so an unattached agent costs nothing.
#[derive(Default)]
pub struct FrameHub {
    agents: Mutex<HashMap<String, broadcast::Sender<Frame>>>,
}

impl FrameHub {
    /// Subscribe an attacher to an agent's live frames, creating the agent's channel if needed.
    /// The returned receiver drops the oldest frames (reported as `Lagged(n)`) if the attacher
    /// falls behind, so it can never backpressure the publisher or other attachers.
    pub fn subscribe(&self, agent_id: &str) -> broadcast::Receiver<Frame> {
        let mut map = self.agents.lock().unwrap();
        let tx = map
            .entry(agent_id.to_string())
            .or_insert_with(|| broadcast::channel(FRAME_RING).0);
        tx.subscribe()
    }

    /// Publish a frame to an agent's attachers. Non-blocking and best-effort: with no attacher
    /// listening the frame is dropped and the now-idle channel reclaimed, so an unattached agent
    /// costs nothing and a send never blocks the caller's turn loop. Returns the number of
    /// attachers the frame reached (0 when none).
    pub fn publish(&self, agent_id: &str, frame: Frame) -> usize {
        let mut map = self.agents.lock().unwrap();
        if let Some(tx) = map.get(agent_id) {
            // `send` errs only when there are no receivers; reclaim the idle channel then.
            match tx.send(frame) {
                Ok(n) => n,
                Err(_) => {
                    map.remove(agent_id);
                    0
                }
            }
        } else {
            // No channel => no attacher => nothing to do (ephemeral, best-effort).
            0
        }
    }

    /// Current attacher (receiver) count for an agent on the live bus. This is the LIVE fan-out
    /// count for diagnostics; the authoritative attach reference count that gates push-on / off is
    /// the durable `session_attachments` row count, not this.
    pub fn receiver_count(&self, agent_id: &str) -> usize {
        self.agents
            .lock()
            .unwrap()
            .get(agent_id)
            .map(|tx| tx.receiver_count())
            .unwrap_or(0)
    }
}

/// The process-global live frame hub. The frame fan-out is in-memory, ephemeral, per-process state
/// with no DB tailer, so a single shared instance backs every attach stream and frame push -- a
/// singleton bus, distinct from the events-tailer broadcast in `AppState` that mirrors the durable
/// event log. A static avoids threading the hub through `AppState` and its many construction sites.
pub fn hub() -> &'static FrameHub {
    static HUB: LazyLock<FrameHub> = LazyLock::new(FrameHub::default);
    &HUB
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(seq: i64) -> Frame {
        Frame {
            session_id: "sess-1".into(),
            turn_id: Some("t1".into()),
            frame_seq: seq,
            kind: FrameKind::TranscriptDelta,
            payload: json!({ "text": "hi" }),
        }
    }

    #[test]
    fn publish_with_no_attacher_is_a_noop() {
        let hub = FrameHub::default();
        assert_eq!(hub.receiver_count("a1"), 0);
        // Best-effort: a push to an unattached agent reaches nobody and does not panic.
        assert_eq!(hub.publish("a1", frame(1)), 0);
    }

    #[tokio::test]
    async fn attacher_receives_published_frames_in_order() {
        let hub = FrameHub::default();
        let mut rx = hub.subscribe("a1");
        assert_eq!(hub.receiver_count("a1"), 1);
        assert_eq!(hub.publish("a1", frame(1)), 1);
        assert_eq!(hub.publish("a1", frame(2)), 1);
        assert_eq!(rx.recv().await.unwrap().frame_seq, 1);
        assert_eq!(rx.recv().await.unwrap().frame_seq, 2);
    }

    #[tokio::test]
    async fn a_lagging_attacher_sees_a_gap_not_backpressure() {
        let hub = FrameHub::default();
        let mut rx = hub.subscribe("a1");
        // Overrun the ring without the receiver draining: the publisher never blocks.
        for seq in 0..(FRAME_RING as i64 + 10) {
            hub.publish("a1", frame(seq));
        }
        // The slow receiver is told it lagged (dropped-oldest), the signal the attach stream turns
        // into a gap marker, rather than ever stalling the publisher.
        match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => assert!(n >= 1),
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[test]
    fn channel_is_reclaimed_once_all_attachers_drop() {
        let hub = FrameHub::default();
        let rx = hub.subscribe("a1");
        assert_eq!(hub.receiver_count("a1"), 1);
        drop(rx);
        // With the last receiver gone, the next publish finds no receivers and reclaims the channel.
        assert_eq!(hub.publish("a1", frame(1)), 0);
        assert_eq!(hub.receiver_count("a1"), 0);
    }
}
