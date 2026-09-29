//! Reverse HTTP-over-WebSocket tunnel — board (server) side.
//!
//! A fleet-host daemon with no inbound network path dials in to `/tunnel/ws` and declares the
//! agents it serves. The board keeps a live-tunnel registry keyed by agent id so it can later
//! push a wake down the socket (send an HTTP `req` frame the daemon replays locally) instead of
//! the agent having to poll. This module is SLICE 1: accept the connection, parse `hello`, keep
//! the registry, reply `hello_ok`, and run keepalive. Wake-delivery (emitting `req` frames from
//! the inbox/event path and correlating `resp`/`err`) is slice 2.
//!
//! Wire contract (locked with fleet-tunnel; all JSON text frames, request/response bodies
//! base64): client→server `{"t":"hello","v":1,"host","agents":[...],"token"?}`; server→client
//! `{"t":"hello_ok","keepalive":<secs>}`; heartbeat `{"t":"ping"}`/`{"t":"pong"}`; (slice 2)
//! server→client `{"t":"req","id","method","path","headers","body"}` and client→server
//! `{"t":"resp",...}` / `{"t":"err",...}`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

/// How often the board sends a WS-level ping, and the `keepalive` it advertises to the daemon.
const KEEPALIVE_SECS: u64 = 30;

/// A live tunnel: the host it serves and a channel to push frames onto its socket. `id`
/// disambiguates reconnects so deregistering an old socket can't evict a newer one that
/// re-claimed the same agent id.
#[derive(Clone)]
pub struct TunnelEntry {
    pub id: u64,
    // `host` and `tx` are the wake-delivery handles: slice 2 looks up a target agent's entry
    // and sends a `req` frame on `tx`. Read by the tests already; keep them until slice 2 wires
    // the emit path (allow avoids a dead-code error under clippy -D warnings in the interim).
    #[allow(dead_code)]
    pub host: String,
    #[allow(dead_code)]
    pub tx: mpsc::UnboundedSender<Message>,
}

/// agent id → the live tunnel currently serving it. Last `hello` wins (an agent that moved
/// hosts re-claims itself). Shared (Arc) between the WS route and, in slice 2, the wake path.
pub type TunnelRegistry = Arc<Mutex<HashMap<String, TunnelEntry>>>;

pub fn registry() -> TunnelRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The `/tunnel/ws` route as a self-contained router carrying the registry as its state, so it
/// can be merged into the top-level app router (it is NOT under `/api` — the daemon dials
/// `/tunnel/ws` directly, and it's a WS upgrade, not part of the REST catalog).
pub fn ws_router(reg: TunnelRegistry) -> Router {
    Router::new().route("/tunnel/ws", get(ws_upgrade)).with_state(reg)
}

async fn ws_upgrade(State(reg): State<TunnelRegistry>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_tunnel(reg, socket))
}

#[derive(Deserialize)]
struct Hello {
    host: String,
    #[serde(default)]
    agents: Vec<String>,
    // `v` (protocol version) and `token` are part of the contract but unused in slice 1.
    #[serde(default)]
    #[allow(dead_code)]
    v: Option<i64>,
    #[serde(default)]
    #[allow(dead_code)]
    token: Option<String>,
}

async fn handle_tunnel(reg: TunnelRegistry, mut socket: WebSocket) {
    // 1) The first frame must be `hello`; answer WS-level pings while we wait, reject anything else.
    let hello: Hello = loop {
        match socket.recv().await {
            Some(Ok(Message::Text(t))) => {
                match serde_json::from_str::<serde_json::Value>(t.as_str()) {
                    Ok(v) if v.get("t").and_then(|x| x.as_str()) == Some("hello") => {
                        match serde_json::from_value::<Hello>(v) {
                            Ok(h) => break h,
                            Err(_) => {
                                let _ = socket.send(err_frame("bad hello frame")).await;
                                return;
                            }
                        }
                    }
                    _ => {
                        let _ = socket.send(err_frame("expected a hello frame first")).await;
                        return;
                    }
                }
            }
            Some(Ok(Message::Ping(p))) => {
                if socket.send(Message::Pong(p)).await.is_err() {
                    return;
                }
            }
            Some(Ok(_)) => {} // ignore other frames until we get hello
            Some(Err(_)) | None => return,
        }
    };

    // 2) Register this tunnel under each declared agent id.
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    {
        let mut map = reg.lock().unwrap();
        for a in &hello.agents {
            map.insert(a.clone(), TunnelEntry { id, host: hello.host.clone(), tx: tx.clone() });
        }
    }
    tracing::info!("[tunnel] host {:?} up, serving {} agent(s)", hello.host, hello.agents.len());

    // 3) hello_ok.
    let ok = json!({ "t": "hello_ok", "keepalive": KEEPALIVE_SECS }).to_string();
    if socket.send(Message::Text(ok.into())).await.is_err() {
        deregister(&reg, id, &hello.agents);
        return;
    }

    // 4) Pump until the socket drops: forward queued outbound frames (slice-2 wakes), answer
    //    heartbeats, and send periodic WS pings so a dead peer is detected. Inbound resp/err
    //    frames are accepted but not yet correlated (slice 2).
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(KEEPALIVE_SECS));
    keepalive.tick().await; // the first tick fires immediately; skip it
    loop {
        tokio::select! {
            outbound = rx.recv() => match outbound {
                Some(msg) => {
                    if socket.send(msg).await.is_err() {
                        break;
                    }
                }
                None => break, // all senders dropped (registry cleared) — shouldn't happen while live
            },
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Text(t))) => {
                    // App-level heartbeat; resp/err correlation is slice 2.
                    if serde_json::from_str::<serde_json::Value>(t.as_str())
                        .ok()
                        .and_then(|v| v.get("t").and_then(|x| x.as_str()).map(str::to_owned))
                        .as_deref()
                        == Some("ping")
                    {
                        let pong = json!({ "t": "pong" }).to_string();
                        if socket.send(Message::Text(pong.into())).await.is_err() {
                            break;
                        }
                    }
                }
                Some(Ok(Message::Ping(p))) => {
                    if socket.send(Message::Pong(p)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            _ = keepalive.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }

    deregister(&reg, id, &hello.agents);
    tracing::info!("[tunnel] host {:?} down", hello.host);
}

/// A one-off error frame (used before a rejected connection drops).
fn err_frame(msg: &str) -> Message {
    Message::Text(json!({ "t": "err", "id": 0, "code": "hello", "msg": msg }).to_string().into())
}

/// Remove this tunnel's agent claims — but only entries still pointing at THIS socket (`id`),
/// so a reconnect that re-registered the same agent id isn't clobbered.
fn deregister(reg: &TunnelRegistry, id: u64, agents: &[String]) {
    let mut map = reg.lock().unwrap();
    for a in agents {
        if map.get(a).map(|e| e.id) == Some(id) {
            map.remove(a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as Cm;

    // Read frames until the next text frame, returning its string (skips pings/pongs).
    async fn next_text<S>(ws: &mut S) -> String
    where
        S: StreamExt<Item = Result<Cm, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        loop {
            match ws.next().await {
                Some(Ok(Cm::Text(t))) => return t.to_string(),
                Some(Ok(_)) => continue,
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn hello_handshake_and_heartbeat_and_registry() -> anyhow::Result<()> {
        let reg = registry();
        let app = ws_router(reg.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        // Give the server a moment to start accepting.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/tunnel/ws")).await?;

        // hello -> hello_ok
        ws.send(Cm::Text(
            json!({"t":"hello","v":1,"host":"desk-1","agents":["agent:a","agent:b"]}).to_string(),
        ))
        .await?;
        let ok: serde_json::Value = serde_json::from_str(&next_text(&mut ws).await)?;
        assert_eq!(ok["t"], "hello_ok");
        assert!(ok["keepalive"].as_u64().is_some(), "hello_ok carries a keepalive");

        // Both declared agents are now live on this tunnel, mapped to the right host.
        {
            let map = reg.lock().unwrap();
            assert_eq!(map.get("agent:a").map(|e| e.host.as_str()), Some("desk-1"));
            assert!(map.contains_key("agent:b"));
        }

        // app-level ping -> pong
        ws.send(Cm::Text(json!({"t":"ping"}).to_string())).await?;
        let pong: serde_json::Value = serde_json::from_str(&next_text(&mut ws).await)?;
        assert_eq!(pong["t"], "pong");

        // On disconnect the tunnel is deregistered.
        ws.close(None).await?;
        // Let the server observe the close and run deregister.
        for _ in 0..50 {
            if reg.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(reg.lock().unwrap().is_empty(), "agents deregistered on disconnect");
        Ok(())
    }

    #[tokio::test]
    async fn non_hello_first_frame_is_rejected() -> anyhow::Result<()> {
        let reg = registry();
        let app = ws_router(reg.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/tunnel/ws")).await?;
        ws.send(Cm::Text(json!({"t":"ping"}).to_string())).await?;
        // Server sends an err frame then drops; registry stays empty.
        let err: serde_json::Value = serde_json::from_str(&next_text(&mut ws).await)?;
        assert_eq!(err["t"], "err");
        assert!(reg.lock().unwrap().is_empty());
        Ok(())
    }
}
