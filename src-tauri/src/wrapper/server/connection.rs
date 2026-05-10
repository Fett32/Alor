//! Per-connection IPC accept + framing loop.
//!
//! Split out of `server.rs` during the god-module decomposition.
//! Owns `SocketServer::handle_connection` — the entry point for each
//! accepted Unix-socket connection. Dispatches by first-envelope kind:
//!
//!   * `cli.event_stream` → park as a long-lived subscriber and
//!     keep the connection open until the subscriber disconnects.
//!   * Any other `cli.*` kind → one-shot CLI command; route to
//!     `handle_cli_message` and write a single response back.
//!   * `wrapper.register` → wrapper-lifecycle path: validate id,
//!     check collisions via `set_agent_connected`, stash the write
//!     half, add a pane, broadcast `agent.connected`, then loop
//!     reading wrapper-protocol messages into `handle_message`.
//!
//! On read-loop exit (EOF or read error) the writers map is cleaned
//! up, `connected` is flipped back, and an `agent.disconnected`
//! event is broadcast.
//!
//! Both routing dispatch methods live in `mod.rs` still (handle_message
//! + handle_cli_message). The next split step will move them into a
//! `routing.rs` submodule; until then `self.handle_*` resolves to the
//! parent impl block.

use std::sync::Arc;
use anyhow::{Context, Result};
use serde_json::json;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::wrapper::protocol::{
    Envelope, WrapperError, WrapperRegister, MSG_CLI_EVENT_STREAM, MSG_ERROR, MSG_REGISTER,
};

use super::SocketServer;

impl SocketServer {
    /// Handle one wrapper connection.
    pub(super) async fn handle_connection(&self, stream: UnixStream) -> Result<()> {
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // First message determines connection type
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // EOF before any message
        }

        let env: Envelope = serde_json::from_str(line.trim())
            .context("parse first envelope")?;

        // CLI messages: handle and return
        if env.kind.starts_with("cli.") {
            if env.kind == MSG_CLI_EVENT_STREAM {
                // Event stream subscriber: hold connection open
                let sub_id = self.next_sub_id.fetch_add(1, Ordering::Relaxed);
                {
                    let mut subs = self.event_subscribers.lock().await;
                    subs.insert(sub_id, Arc::new(Mutex::new(write_half)));
                }
                info!(sub_id, "cli event stream subscriber connected");

                // Keep reading until disconnect. We swallow read errors
                // locally so the subscriber is always removed from the map —
                // previously `?` would propagate out and skip the cleanup.
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(e) => {
                            warn!(sub_id, "event stream read error: {e}");
                            break;
                        }
                    }
                }

                {
                    let mut subs = self.event_subscribers.lock().await;
                    subs.remove(&sub_id);
                }
                info!(sub_id, "cli event stream subscriber disconnected");
                return Ok(());
            }

            // One-shot CLI command
            let response = self.handle_cli_message(env).await;
            let mut resp_line = serde_json::to_string(&response)?;
            resp_line.push('\n');

            // write_half is not yet consumed — we still own it
            let mut writer = write_half;
            writer.write_all(resp_line.as_bytes()).await?;
            writer.flush().await?;
            return Ok(());
        }

        // Wrapper registration flow
        if env.kind != MSG_REGISTER {
            anyhow::bail!("first message must be wrapper.register, got {}", env.kind);
        }

        let reg: WrapperRegister = env.decode_payload()
            .context("decode WrapperRegister")?;
        let agent_id = reg.agent_id.clone();

        // Validate agent ID: alphanumeric, dashes, underscores only (max 64 chars).
        if agent_id.is_empty()
            || agent_id.len() > 64
            || !agent_id.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            anyhow::bail!("invalid agent_id: {agent_id:?}");
        }

        info!(agent_id = %agent_id, "wrapper registered");

        // Update agent status in app state FIRST to check for collisions.
        // This is the source of truth for "connected".
        if let Err(e) = self.app_state.set_agent_connected(&agent_id, true) {
            warn!(agent_id = %agent_id, "registration rejected: {e:#}");
            let err_env = Envelope::new(
                MSG_ERROR,
                WrapperError {
                    agent_id: agent_id.clone(),
                    message: format!("Collision: {e:#}")
                }
            )?;
            let mut line = serde_json::to_string(&err_env)?;
            line.push('\n');
            let mut writer = write_half;
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await?;
            return Ok(());
        }

        // Store the write half
        {
            let mut writers = self.writers.lock().await;
            writers.insert(agent_id.clone(), write_half);
        }

        // Add agent pane to alor-main. Failure here used to be invisible
        // — a silent `warn!` — which masked the root cause of task
        // 1ed52762 (agents connected without a pane in alor-main). The
        // stale-entry bug that caused that is now fixed inside
        // `add_agent_pane`; escalating the log level to error! and
        // naming the agent + tmux session makes any remaining failure
        // class loud enough to catch on the next occurrence.
        // PaneManager::reconcile_panes still runs on boot + via the UI
        // as defense-in-depth if a novel failure shape slips through.
        if let Err(e) = self.pane_manager.add_agent_pane(&agent_id).await {
            let agent_session = format!("alor-{agent_id}");
            error!(
                agent_id = %agent_id,
                tmux_session = %agent_session,
                "add_agent_pane failed during wrapper.register: {e:#}"
            );
            // Emit a UI-visible event so the frontend can surface the
            // problem (currently logged; future: a toast/banner).
            let pane_fail = json!({
                "agent_id": &agent_id,
                "tmux_session": &agent_session,
                "error": format!("{e:#}"),
            });
            self.broadcast_event("agent.pane_add_failed", pane_fail.clone())
                .await;
            self.app_state
                .emit_event_with("agent.pane_add_failed", pane_fail);
        }

        // Broadcast connection event
        self.broadcast_event(
            "agent.connected",
            json!({"agent_id": &agent_id}),
        )
        .await;

        // Read loop
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                info!(agent_id = %agent_id, "wrapper disconnected");
                break;
            }

            let env: Envelope = match serde_json::from_str(line.trim()) {
                Ok(e) => e,
                Err(e) => {
                    warn!(agent_id = %agent_id, "malformed message: {e}");
                    continue;
                }
            };

            self.handle_message(&agent_id, env).await;
        }

        // Cleanup on disconnect
        {
            let mut writers = self.writers.lock().await;
            writers.remove(&agent_id);
        }
        let _ = self.app_state.set_agent_connected(&agent_id, false);

        // Broadcast disconnection event
        self.broadcast_event(
            "agent.disconnected",
            json!({"agent_id": &agent_id}),
        )
        .await;

        Ok(())
    }
}
