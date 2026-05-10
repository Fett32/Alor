//! Event broadcasting to CLI event-stream subscribers.
//!
//! Split out of `server.rs` during the god-module decomposition.
//! Owns `SocketServer::broadcast_event` — the one-to-many fanout path
//! used by every state-transition event (`task.accepted`,
//! `task.completed`, `agent.connected`, `agent.disconnected`,
//! `user.intervention`, `worker.*`, etc.). The subscriber map itself
//! (`event_subscribers`) is still a field on `SocketServer` in
//! `mod.rs` because connection-handler code owns the insert/remove
//! lifecycle.
//!
//! Design note — per-subscriber locking: each subscriber's writer
//! lives behind its own `Arc<Mutex<_>>` so `broadcast_event` can
//! snapshot handles under the outer lock, release it, then write
//! per-subscriber without stalling every other subscriber on one
//! slow client. That invariant is why the `EventSubscribers` type
//! alias is shaped the way it is (see `mod.rs`).

use std::sync::Arc;
use tokio::io::AsyncWriteExt;

use crate::wrapper::protocol::{Envelope, MSG_EVENT};

use super::SocketServer;

impl SocketServer {
    /// Broadcast an event to all event stream subscribers.
    pub(super) async fn broadcast_event(
        &self,
        event_type: &str,
        data: serde_json::Value,
    ) {
        let event = match Envelope::new(
            MSG_EVENT,
            serde_json::json!({
                "event": event_type,
                "data": data,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
        ) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut line = match serde_json::to_string(&event) {
            Ok(l) => l,
            Err(_) => return,
        };
        line.push('\n');
        let bytes: Arc<[u8]> = Arc::from(line.into_bytes());

        // Snapshot subscriber handles under the outer lock, then release it
        // so a single stuck subscriber can't stall other broadcasts.
        let handles: Vec<(u64, Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>)> = {
            let subs = self.event_subscribers.lock().await;
            subs.iter().map(|(id, w)| (*id, w.clone())).collect()
        };

        let mut dead = Vec::new();
        for (id, writer) in handles {
            let mut w = writer.lock().await;
            if w.write_all(&bytes).await.is_err() || w.flush().await.is_err() {
                dead.push(id);
            }
        }
        if !dead.is_empty() {
            let mut subs = self.event_subscribers.lock().await;
            for id in dead {
                subs.remove(&id);
            }
        }
    }
}
