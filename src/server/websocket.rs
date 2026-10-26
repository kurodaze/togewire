use axum::body::Bytes;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket};
use futures::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info};

use super::AppState;

const PING_INTERVAL: Duration = Duration::from_secs(54);
/// A live peer answers our ping (browsers auto-pong), so silence for two intervals means
/// the connection is half-open - drop it instead of leaking the slot until the OS notices.
const READ_IDLE_TIMEOUT: Duration = Duration::from_mins(2);

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

pub async fn handle_socket(state: AppState, socket: WebSocket, ip: String) {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Utf8Bytes>(256);
    state.ws_clients.write().insert(id, tx.clone());
    if let Some(current) = state.current().filter(|s| s.track.is_some()) {
        let _ = tx.try_send(super::workers::track_update_message(&state, &current));
    }
    drop(tx);

    let shutdown = state.shutdown.clone();
    let mut writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await; // first tick is immediate
        loop {
            let msg = tokio::select! {
                // `None`: our sender was dropped (player switched off), so close the socket
                // rather than idle on a client that will never see another update.
                msg = rx.recv() => match msg {
                    Some(msg) => Message::Text(msg),
                    None => break,
                },
                _ = ping.tick() => Message::Ping(Bytes::new()),
                () = shutdown.cancelled() => break,
            };
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Size limits are enforced by the upgrade (max_message_size/max_frame_size). The flag
    // keeps the per-IP tally balanced: a client repeating `listening: true` must not add a
    // count that only one disconnect ever takes back.
    let mut listening = false;
    loop {
        tokio::select! {
            _ = &mut writer => break,
            msg = tokio::time::timeout(READ_IDLE_TIMEOUT, ws_rx.next()) => match msg {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Some(on) = parse_listening_message(&text)
                        && on != listening
                    {
                        listening = on;
                        set_listening(&ip, on);
                    }
                }
                Ok(Some(Ok(Message::Close(_)) | Err(_)) | None) | Err(_) => break,
                Ok(Some(Ok(_))) => {}
            },
        }
    }
    writer.abort();

    if listening {
        set_listening(&ip, false);
    }
    state.ws_clients.write().remove(&id);
    super::with_peer(&ip, |p| p.sockets = p.sockets.saturating_sub(1));
    debug!("WebSocket client {id} disconnected");
}

/// `Some(on)` only for a `{"type":"listening"}` frame; anything else a client sends must
/// leave the tally alone rather than read as "stopped listening".
fn parse_listening_message(text: &str) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    (v["type"] == "listening").then(|| v["listening"].as_bool().unwrap_or(false))
}

/// Reported by the page. `/stream` cannot stand in for this: the browser only opens it
/// once audio exists, which is long after the listener pressed the button.
fn set_listening(ip: &str, on: bool) {
    super::with_peer(ip, |p| {
        if on {
            p.listening += 1;
            if p.listening == 1 {
                info!("Listener connected from {ip}");
            }
        } else if p.listening > 0 {
            p.listening -= 1;
            if p.listening == 0 {
                info!("Listener disconnected from {ip}");
            }
        }
    });
}
