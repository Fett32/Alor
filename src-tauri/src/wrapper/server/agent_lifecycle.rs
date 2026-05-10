//! Agent lifecycle operations owned by `SocketServer`.
//!
//! Split out of `server.rs` during the god-module decomposition.
//! Everything in this file is "what happens to an agent once it
//! exists" — spawn (template instantiation + child process launch),
//! send-to (write an envelope into the agent's wrapper socket),
//! and the mark-disconnected paths (zombie / killed auto-clear).
//!
//! Also hosts the SDK-runtime resolver + `is_framed_send_allowed`
//! gate. Those are used by the `cli.agent.send_message` handler in
//! `routing.rs` to decide whether a suppress_echo framed send is
//! safe for the target agent — only the `claude-sdk` runtime
//! understands the BEGIN/END sentinels, so splatting them into a
//! raw-pty wrapper's stdin would be visible garbage.
//!
//! Free-fn shape for the resolver/gate (rather than methods) is
//! deliberate: they take `&[(String, AgentConfig)]` + `&AppState`
//! rather than `&self`, which lets the inline unit tests feed them
//! synthetic configs without standing up a full `SocketServer`.

use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use uuid::Uuid;

use crate::daemon::config::AgentConfig;
use crate::daemon::state::AppState;
use crate::wrapper::protocol::{CliSpawn, Envelope, MSG_CLI_RESPONSE};

use super::{cli_error, SocketServer};

/// Runtime layer that hosts `claude-sdk` workers and speaks the
/// BEGIN/END framing state machine in its stdin loop (orchestrator-py's
/// worker.py). Any other runtime (wrapper, future codex/gemini) has a
/// raw pty with no framing awareness — injecting sentinel-wrapped
/// payloads would splat literal `__ALOR_ORCH_ECHO_BEGIN__<uuid>` lines
/// into the CLI's prompt.
pub(super) const SDK_FRAMED_RUNTIME: &str = "claude-sdk";

/// Resolve an agent_id to the runtime string declared in its yaml
/// config.
///
/// Resolution order:
///   1. Direct match against a loaded yaml slot (`agent_configs`).
///   2. For template-spawned instances: follow `Agent.template` from
///      runtime state back to the template's yaml config.
///
/// Returns `None` only when the agent is neither a known yaml slot nor
/// a state-persisted instance of a known template. In that unresolved
/// case the caller MUST treat the agent as non-SDK (fail-closed) —
/// allowing a framed send to an unidentifiable agent would defeat the
/// whole point of the gate.
pub(super) fn resolve_agent_runtime(
    agent_configs: &[(String, AgentConfig)],
    app_state: &AppState,
    agent_id: &str,
) -> Option<String> {
    if let Some((_, cfg)) = agent_configs.iter().find(|(id, _)| id == agent_id) {
        return Some(cfg.runtime.clone());
    }
    let agent = app_state.get_agent(agent_id)?;
    let template_id = agent.template.as_deref()?;
    agent_configs
        .iter()
        .find(|(id, _)| id == template_id)
        .map(|(_, cfg)| cfg.runtime.clone())
}

/// Gate for `cli.agent.send_message` with `suppress_echo=true`. Only
/// agents whose resolved runtime is `claude-sdk` may receive framed
/// sends; everything else (wrapper, unknown) is rejected so sentinel
/// bytes never land in a non-framing pty.
pub(super) fn is_framed_send_allowed(
    agent_configs: &[(String, AgentConfig)],
    app_state: &AppState,
    agent_id: &str,
) -> bool {
    matches!(
        resolve_agent_runtime(agent_configs, app_state, agent_id).as_deref(),
        Some(SDK_FRAMED_RUNTIME)
    )
}

impl SocketServer {
    /// Generate a unique instance ID for an agent kind.
    /// If `base` is not already taken, returns it as-is.
    /// Otherwise appends `-2`, `-3`, etc. until a free ID is found.
    pub(super) fn unique_instance_id(&self, base: &str) -> String {
        let agents = self.app_state.all_agents();
        let taken: std::collections::HashSet<&str> =
            agents.iter().map(|a| a.id.as_str()).collect();

        if !taken.contains(base) {
            return base.to_string();
        }

        for n in 2u32.. {
            let candidate = format!("{base}-{n}");
            if !taken.contains(candidate.as_str()) {
                return candidate;
            }
        }
        unreachable!()
    }

    /// Handle a cli.spawn request.
    pub(super) async fn handle_spawn(
        &self,
        correlation_id: Uuid,
        payload: CliSpawn,
    ) -> Envelope {
        // Find config for this agent
        let config = self
            .agent_configs
            .iter()
            .find(|(id, _)| id == &payload.agent)
            .map(|(_, cfg)| cfg.clone());

        let config = match config {
            Some(c) => c,
            None => {
                return cli_error(
                    correlation_id,
                    &format!("no config found for agent '{}'", payload.agent),
                )
            }
        };

        // Effective project + working_dir: payload override beats yaml config.
        let effective_project = payload.project.clone().or_else(|| config.project.clone());
        let effective_working_dir = payload
            .working_dir
            .clone()
            .or_else(|| config.working_dir.clone());

        // Templates must be parameterized at spawn time — refuse bare template
        // spawns that didn't pass either a project or a working_dir override.
        if config.template
            && payload.project.is_none()
            && payload.working_dir.is_none()
        {
            return cli_error(
                correlation_id,
                &format!(
                    "'{}' is a template; agent_spawn needs a project and/or working_dir override",
                    payload.agent
                ),
            );
        }

        // If spawning from a template, record the template id so the instance
        // can be respawned after daemon restart using the template's command.
        let template_ref = if config.template {
            Some(payload.agent.clone())
        } else {
            None
        };

        // Determine instance ID: explicit --name, auto-derived from project for
        // template spawns, or a unique numbered id as a last resort.
        let instance_id = match payload.name.clone() {
            Some(name) => name,
            None => {
                if config.template {
                    match effective_project.as_deref() {
                        Some(proj) if !proj.is_empty() => format!("{}-{}", payload.agent, proj),
                        _ => self.unique_instance_id(&payload.agent),
                    }
                } else {
                    self.unique_instance_id(&payload.agent)
                }
            }
        };

        // Collision guard: a template-derived id could accidentally collide
        // with an existing yaml slot (e.g. template 'claude' + project 'alor'
        // derives 'claude-alor', which is also a fixed yaml slot). Refuse so
        // we don't overwrite state metadata or spawn an orphan session.
        if config.template
            && instance_id != payload.agent
            && self
                .agent_configs
                .iter()
                .any(|(id, _)| id == &instance_id)
        {
            return cli_error(
                correlation_id,
                &format!(
                    "instance id '{}' conflicts with an existing yaml slot; \
                     use agent_ensure_running('{}') instead, or spawn with an explicit --name",
                    instance_id, instance_id
                ),
            );
        }

        // Register the instance in state with the base config's metadata plus
        // any runtime overrides.
        {
            let existing = self.app_state.get_agent(&instance_id);
            if existing.is_none() {
                let display_name = config
                    .identity
                    .as_ref()
                    .map(|id| format!("{} {}", id, instance_id))
                    .unwrap_or_else(|| instance_id.clone());
                let mut agent = crate::daemon::state::Agent::new(&instance_id, &display_name);
                agent.tmux_session = Some(format!("alor-{instance_id}"));
                agent.project = effective_project.clone();
                agent.tier = config.tier.clone();
                agent.max_concurrent = config.max_concurrent;
                agent.working_dir = effective_working_dir.clone();
                agent.template = template_ref.clone();
                self.app_state.register_agent(agent);
            } else {
                self.app_state.set_agent_metadata(
                    &instance_id,
                    effective_project.clone(),
                    Some(config.tier.clone()),
                    Some(config.max_concurrent),
                    effective_working_dir.clone(),
                    template_ref.clone(),
                );
            }
        }

        // Resolve workdir once; both runtime branches use it.
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let expanded_workdir: Option<std::path::PathBuf> =
            effective_working_dir.as_ref().map(|wd| {
                if wd.starts_with('~') {
                    std::path::PathBuf::from(&home)
                        .join(wd.strip_prefix("~/").unwrap_or(&wd[1..]))
                } else {
                    std::path::PathBuf::from(wd)
                }
            });

        let mut cmd = if config.runtime == "claude-sdk" {
            // SDK worker path: run-worker.sh inside a tmux session.
            // The worker talks wrapper protocol directly to the daemon and
            // hosts its own ClaudeSDKClient. No alor-wrapper in the loop.
            if config.command.is_empty() {
                return cli_error(
                    correlation_id,
                    "claude-sdk runtime requires `command:` in yaml to point at run-worker.sh",
                );
            }
            let session_name = format!("alor-{instance_id}");

            let mut c = std::process::Command::new("tmux");
            c.args(["new-session", "-d", "-s", &session_name]);
            if let Some(ref wd) = expanded_workdir {
                c.arg("-c").arg(wd);
            }
            // Everything after `--` is the command line tmux runs inside.
            c.arg("--");
            c.arg(&config.command[0]);
            c.arg(&instance_id);
            if let Some(ref wd) = expanded_workdir {
                c.arg("--workdir").arg(wd);
            }
            if let Some(ref proj) = effective_project {
                c.arg("--project").arg(proj);
            }
            c
        } else {
            // Classic wrapper path.
            let wrapper_bin = match crate::daemon::config::find_wrapper_binary() {
                Ok(bin) => bin,
                Err(e) => {
                    return cli_error(
                        correlation_id,
                        &format!("wrapper binary not found: {e}"),
                    )
                }
            };
            let mut c = std::process::Command::new(&wrapper_bin);
            c.arg(&instance_id);
            if !config.command.is_empty() {
                c.arg("--command").arg(config.command.join(" "));
            }
            if let Some(ref wd) = expanded_workdir {
                c.arg("--workdir").arg(wd);
            }
            if let Some(ref sf) = config.startup_file {
                let expanded = if sf.starts_with('~') {
                    std::path::PathBuf::from(&home)
                        .join(sf.strip_prefix("~/").unwrap_or(&sf[1..]))
                } else {
                    std::path::PathBuf::from(sf)
                };
                c.arg("--startup-file").arg(expanded);
            }
            c
        };

        match cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
        {
            Ok(child) => {
                let pid = child.id();
                info!(agent = %payload.agent, instance_id = %instance_id, pid, "agent spawned via CLI");

                // For claude-sdk runtime, apply session-level mouse + history
                // AFTER new-session creates the session. Without mouse on, the
                // nested-tmux setup (alor-main pane running `tmux attach -t
                // this session`) can't forward wheel events to this session's
                // own copy-mode, so scrolling shows the outer pane's empty
                // scrollback instead of the worker's real history. Wrapper-
                // runtime agents already get this via ensure_session_defaults.
                if config.runtime == "claude-sdk" {
                    // set-option uses the BARE name — tmux 3.4 rejects
                    // the `=name` exact-match sigil on set-option
                    // specifically ("no such session"), even though it
                    // works for has-session and kill-session. We rely on
                    // the handle_spawn collision guard above and the
                    // validated alphanumeric agent_id so the bare name
                    // lands on the right session.
                    let session_name = format!("alor-{instance_id}");
                    let _ = std::process::Command::new("tmux")
                        .args(["set-option", "-t", &session_name, "mouse", "on"])
                        .output();
                    let _ = std::process::Command::new("tmux")
                        .args(["set-option", "-t", &session_name, "history-limit", "50000"])
                        .output();
                }

                {
                    let mut spawned = self.spawned.lock().await;
                    spawned.insert(instance_id.clone(), child);
                }
                // If the caller tagged this spawn with a task_id
                // (worker calling agent_spawn mid-task — see
                // orchestrator-py/tools.py::agent_spawn), record the
                // pairing so transition_task can warn if the worker
                // forgets to kill the instance before completing.
                if let Some(task_id) = payload.spawned_by_task {
                    self.app_state.record_task_spawn(task_id, &instance_id);
                }
                self.broadcast_event(
                    "agent.spawned",
                    serde_json::json!({"agent": &payload.agent, "instance_id": &instance_id, "pid": pid}),
                )
                .await;
                match Envelope::new(
                    MSG_CLI_RESPONSE,
                    serde_json::json!({"spawned": &instance_id, "pid": pid}),
                ) {
                    Ok(mut e) => {
                        e.correlation_id = correlation_id;
                        e
                    }
                    Err(_) => cli_error(correlation_id, "failed to build response"),
                }
            }
            Err(e) => cli_error(
                correlation_id,
                &format!("failed to spawn {}: {e}", payload.agent),
            ),
        }
    }

    /// Send an envelope to a specific wrapper.
    pub async fn send_to(&self, agent_id: &str, envelope: &Envelope) -> anyhow::Result<()> {
        let mut writers = self.writers.lock().await;
        let writer = writers.get_mut(agent_id)
            .ok_or_else(|| anyhow::anyhow!("no connection for agent {agent_id}"))?;

        let mut line = serde_json::to_string(envelope)?;
        line.push('\n');
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;

        Ok(())
    }

    /// Check if a wrapper is connected.
    pub async fn is_connected(&self, agent_id: &str) -> bool {
        self.writers.lock().await.contains_key(agent_id)
    }

    /// Test-only: inject a writer into the `writers` map to
    /// simulate a connected wrapper without a real socket accept
    /// flow. Used by unit tests in `commands.rs` that exercise
    /// `assign_task_inner`'s dispatch path, and anywhere else a
    /// test needs `is_connected == true` + a working `send_to`.
    ///
    /// Tests pair this with `tokio::net::UnixStream::pair()` — one
    /// half gets stashed here, the other is dropped. Writes into
    /// the stashed half succeed (kernel buffers) which is all the
    /// `send_to` path checks; the receive side isn't exercised.
    #[cfg(test)]
    pub(crate) async fn test_insert_writer(
        &self,
        agent_id: &str,
        writer: tokio::net::unix::OwnedWriteHalf,
    ) {
        self.writers.lock().await.insert(agent_id.to_string(), writer);
    }

    /// Mark an agent as a zombie: flip `connected` to false in state
    /// and broadcast the disconnect event. Called by
    /// `PaneManager::reconcile_panes` callers after it detects that
    /// an agent's tmux session has disappeared while state still
    /// thinks it's connected.
    ///
    /// Deliberately does NOT remove the agent from the `writers` map.
    /// The zombie wrapper process may still have a live daemon socket
    /// — we want to reach it (e.g. to send SHUTDOWN via kill_agent)
    /// even after flagging the UI state as disconnected. Writers get
    /// cleaned up by the normal socket-drop path (see the read-loop
    /// cleanup in `connection.rs`) when the wrapper actually dies.
    ///
    /// The `reason` field on the broadcast distinguishes this from a
    /// normal socket-drop disconnect so CLI event subscribers can
    /// branch if they care.
    pub async fn mark_agent_zombie(&self, agent_id: &str) {
        self.mark_agent_disconnected(agent_id, "zombie_auto_cleared").await;
    }

    /// Mark an agent as disconnected because it was just killed. Called
    /// from the `MSG_CLI_KILL` handler after the child process + tmux
    /// session have been taken down. Analogous to `mark_agent_zombie`
    /// but with a distinct `reason` so event subscribers can tell
    /// "orch explicitly killed this" apart from "reconcile detected a
    /// zombie and auto-cleaned up."
    ///
    /// Historical bug this addresses (task 2026-04-19 repro): the
    /// `cli.kill` handler used to take the tmux session down but
    /// never flip `state.connected` to false. For agents without a
    /// tracked daemon-spawned child the wrapper's socket wasn't
    /// closed by the kill path — so the natural "socket drop →
    /// normal disconnect cleanup" never fired and `agent_list`
    /// still showed `connected: true` with no tmux session. Classic
    /// zombie shape, except caused by the kill itself. Calling this
    /// method unconditionally from the kill path makes the state
    /// transition loudly authoritative.
    pub async fn mark_agent_killed(&self, agent_id: &str) {
        self.mark_agent_disconnected(agent_id, "killed").await;
    }

    /// Shared implementation for the disconnect-with-reason paths.
    /// Flips `state.connected` to false and broadcasts
    /// `agent.disconnected` with the supplied reason. Deliberately
    /// does NOT touch the `writers` map — kept wrapper sockets, if
    /// any, drop through the normal read-loop cleanup path when they
    /// actually close (see the handler in `connection.rs`). That
    /// preserves the ability to reach a still-live wrapper socket
    /// for a follow-up SHUTDOWN if needed.
    ///
    /// Idempotent on repeated calls: `set_agent_connected(id, false)`
    /// on an already-disconnected agent is a no-op for the flag
    /// value; broadcast goes out each time but subscribers are
    /// expected to tolerate duplicate disconnect events.
    async fn mark_agent_disconnected(&self, agent_id: &str, reason: &str) {
        if let Err(e) = self.app_state.set_agent_connected(agent_id, false) {
            warn!(
                agent_id,
                reason,
                "mark_agent_disconnected: set_agent_connected(false) failed: {e:#}"
            );
            return;
        }
        info!(
            agent_id,
            reason,
            "agent marked disconnected (connected -> false)"
        );
        self.broadcast_event(
            "agent.disconnected",
            serde_json::json!({
                "agent_id": agent_id,
                "reason": reason,
            }),
        )
        .await;
    }
}

