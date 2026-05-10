/// Socket server for wrapper connections.
///
/// Listens on /tmp/alor/daemon.sock and handles:
/// - wrapper.register — wrapper announces its agent_id
/// - task.accept/complete/blocked — task state updates
/// - status.response — heartbeat replies
/// - cli.* — CLI commands (Phase 9)
///
/// Outbound messages (task.assign, status.request) are sent via the
/// connection registry.
///
/// Implementation is split across submodules to keep this file
/// focused on server bootstrap + accept-loop wiring. Each submodule
/// extends `SocketServer` via additional `impl` blocks:
///
/// - `broadcast`          — event fanout to CLI event-stream subscribers.
/// - `connection`         — per-connection accept / IPC framing loop.
/// - `routing`            — wrapper-protocol + CLI message dispatch.
/// - `agent_lifecycle`    — spawn, runtime resolution, send_to, mark_agent_*.
///
/// All submodules live under `src/wrapper/server/`. Tests moved to
/// `tests.rs` in the same directory to keep `mod.rs` small.

mod agent_lifecycle;
mod broadcast;
mod connection;
mod routing;

// After the god-module split, mod.rs no longer needs the framed-send
// gate itself — the `cli.agent.send_message` handler that called it
// now lives in `routing.rs`, which reaches it via its own import.
// Kept here as a deliberate re-export point for future tests that
// want to drive the gate from outside routing.
#[allow(unused_imports)]
use agent_lifecycle::is_framed_send_allowed;

// Imports trimmed after the god-module split: mod.rs now only
// needs what's referenced by the struct definition, `with_configs`,
// `run`, the accept loop, the reaper task, and the `cli_error*`
// helpers. Everything else the former server.rs pulled in has
// migrated to its respective submodule.
use crate::daemon::config::AgentConfig;
use crate::daemon::state::AppState;
use crate::terminal::pane_manager::PaneManager;
use crate::wrapper::protocol::{Envelope, MSG_CLI_ERROR};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use uuid::Uuid;

pub const DAEMON_SOCKET: &str = "/tmp/alor/daemon.sock";

/// A connected wrapper's write half, keyed by agent_id.
type WriterMap = Arc<Mutex<HashMap<String, tokio::net::unix::OwnedWriteHalf>>>;
type ChildMap = Arc<Mutex<HashMap<String, std::process::Child>>>;
/// Each subscriber's writer lives behind its own mutex so broadcast_event
/// can snapshot handles under the outer lock, drop it, then write per-sub
/// without stalling every subscriber on one slow client.
type EventSubscribers =
    Arc<Mutex<HashMap<u64, Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>>>>;
/// Tracks tasks awaiting a `task.accept` ack from the worker after
/// the daemon dispatched `task.assign`. Key is task_id; value is a
/// oneshot sender the MSG_TASK_ACCEPT handler drops on ack arrival,
/// which wakes the accept-watchdog's select and cancels the timeout.
///
/// Pre-fix (before task 2026-04-20 follow-up) the daemon just
/// wrote the assign envelope and flipped state on subsequent
/// `task.accept`; if the worker never replied (frame dropped,
/// worker crashed mid-handshake, socket stalled during reconnect)
/// the task sat in ASSIGNED forever. The watchdog now bounds that
/// failure mode to `ACCEPT_ACK_TIMEOUT_SECS`.
type PendingAccepts = Arc<
    Mutex<HashMap<uuid::Uuid, tokio::sync::oneshot::Sender<()>>>,
>;

/// Shared state for the socket server.
#[derive(Clone)]
pub struct SocketServer {
    writers: WriterMap,
    app_state: AppState,
    pane_manager: PaneManager,
    agent_configs: Arc<Vec<(String, AgentConfig)>>,
    spawned: ChildMap,
    event_subscribers: EventSubscribers,
    next_sub_id: Arc<AtomicU64>,
    pub(super) pending_accepts: PendingAccepts,
}

impl SocketServer {
    pub fn with_configs(
        app_state: AppState,
        pane_manager: PaneManager,
        configs: Vec<(String, AgentConfig)>,
    ) -> Self {
        Self {
            writers: Arc::new(Mutex::new(HashMap::new())),
            app_state,
            pane_manager,
            agent_configs: Arc::new(configs),
            spawned: Arc::new(Mutex::new(HashMap::new())),
            event_subscribers: Arc::new(Mutex::new(HashMap::new())),
            next_sub_id: Arc::new(AtomicU64::new(0)),
            pending_accepts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Start listening. Call this in a spawned task.
    pub async fn run(&self) -> Result<()> {
        // Ensure parent directory exists
        let sock_path = Path::new(DAEMON_SOCKET);
        if let Some(parent) = sock_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }

        // Remove stale socket file
        if sock_path.exists() {
            tokio::fs::remove_file(sock_path).await.ok();
        }

        let listener = UnixListener::bind(sock_path)
            .context("bind daemon socket")?;

        // Restrict socket to owner only (0600) so unprivileged users can't
        // connect and issue CLI commands.
        std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o600))
            .context("set daemon socket permissions")?;

        info!(path = DAEMON_SOCKET, "socket server listening");

        // Periodically reap exited children so `spawned` doesn't grow
        // forever.  Wrappers/workers that exit on their own (crash, /quit,
        // normal shutdown) aren't removed through the kill/delete path, and
        // without try_wait the kernel keeps zombie entries and we keep a
        // stale Child handle indefinitely.
        {
            let reaper = self.clone();
            tokio::spawn(async move {
                let mut interval =
                    tokio::time::interval(std::time::Duration::from_secs(30));
                interval.tick().await; // skip the immediate first tick
                loop {
                    interval.tick().await;
                    let mut spawned = reaper.spawned.lock().await;
                    spawned.retain(|id, child| match child.try_wait() {
                        Ok(Some(status)) => {
                            info!(instance = %id, exit = ?status, "reaped exited child");
                            false
                        }
                        Ok(None) => true,
                        Err(e) => {
                            warn!(instance = %id, error = %e, "try_wait failed");
                            true
                        }
                    });
                }
            });
        }

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let server = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = server.handle_connection(stream).await {
                            warn!("connection handler error: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    error!("accept error: {e}");
                }
            }
        }
    }

    // handle_connection moved to submodule: see `server/connection.rs`.

    // unique_instance_id + handle_spawn moved to submodule:
    // see `server/agent_lifecycle.rs`.

    // broadcast_event moved to submodule: see `server/broadcast.rs`.

    // send_to / is_connected / mark_agent_zombie / mark_agent_killed
    // moved to submodule: see `server/agent_lifecycle.rs`.
}

// `cli_error` + `cli_error_coded` are used from multiple submodules
// (routing + agent_lifecycle), so they live here and are exposed
// `pub(super)` — visible to the `server` module family, invisible
// to the rest of the crate.
pub(super) fn cli_error(correlation_id: Uuid, message: &str) -> Envelope {
    // Direct struct literal so correlation_id from the request can
    // flow through; Envelope::new would assign a fresh one.
    // protocol_version stamped at the local version — every
    // sender-side envelope carries it, per the gate contract in
    // `wrapper/protocol.rs`.
    Envelope {
        kind: MSG_CLI_ERROR.to_string(),
        correlation_id,
        protocol_version: crate::wrapper::protocol::PROTOCOL_VERSION,
        payload: serde_json::json!({"error": message}),
    }
}

/// Variant of `cli_error` that stamps a stable `code` alongside the
/// human-readable `error`. Callers (orchestrator-py) pattern-match on
/// `code` to raise typed exceptions; the `error` prose stays the surface
/// that makes it into logs and LLM contexts. Use when the rejection is
/// something a caller might plausibly want to handle specifically — not
/// for every unexpected failure.
pub(super) fn cli_error_coded(correlation_id: Uuid, code: &str, message: &str) -> Envelope {
    Envelope {
        kind: MSG_CLI_ERROR.to_string(),
        correlation_id,
        protocol_version: crate::wrapper::protocol::PROTOCOL_VERSION,
        payload: serde_json::json!({"code": code, "error": message}),
    }
}


// SDK_FRAMED_RUNTIME + resolve_agent_runtime + is_framed_send_allowed
// moved to submodule: see `server/agent_lifecycle.rs`.


#[cfg(test)]
mod tests;
