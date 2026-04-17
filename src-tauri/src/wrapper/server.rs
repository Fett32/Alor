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

use crate::daemon::config::AgentConfig;
use crate::daemon::project;
use crate::daemon::state::{AppState, Task, TaskState};
use crate::terminal::pane_manager::PaneManager;
use crate::wrapper::protocol::{
    CliAgentEnsureRunning, CliAgentSendMessage, CliAssign, CliDelete, CliKill, CliMemoryGet,
    CliProjectGet, CliProjectSave, CliSpawn, CliTaskCancel, CliTaskCreate,
    CliTaskComplete as CliTaskCompletePayload, CliTaskGet, Envelope, TaskAccept, TaskAssign,
    TaskBlocked, TaskComplete, TaskPropose, UserIntervention, WorkerOrchResponse,
    WorkerUserInput, WrapperError, WrapperRegister, MSG_CLI_AGENT_ENSURE_RUNNING,
    MSG_CLI_AGENT_SEND_MESSAGE, MSG_CLI_ASSIGN, MSG_CLI_DELETE, MSG_CLI_ERROR,
    MSG_CLI_EVENT_STREAM, MSG_CLI_KILL, MSG_CLI_MEMORY_GET, MSG_CLI_PROJECT_GET,
    MSG_CLI_PROJECT_LIST, MSG_CLI_PROJECT_SAVE, MSG_CLI_RESPONSE, MSG_CLI_SPAWN,
    MSG_CLI_STATUS, MSG_CLI_TASK_CANCEL, MSG_CLI_TASK_COMPLETE, MSG_CLI_TASK_CREATE,
    MSG_CLI_TASK_GET, MSG_CLI_TASK_LIST, MSG_ERROR, MSG_EVENT, MSG_REGISTER,
    MSG_CLI_INTEGRATIONS_GET, MSG_STATUS_RESPONSE, MSG_TASK_ACCEPT, MSG_TASK_ASSIGN,
    MSG_TASK_BLOCKED, MSG_TASK_COMPLETE, MSG_TASK_PROPOSE, MSG_USER_INTERVENTION,
    MSG_WORKER_ORCH_RESPONSE, MSG_WORKER_USER_INPUT, WORKER_ECHO_SENTINEL_BEGIN,
    WORKER_ECHO_SENTINEL_END,
};
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
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

    /// Handle one wrapper connection.
    async fn handle_connection(&self, stream: UnixStream) -> Result<()> {
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

        // Add agent pane to alor-main
        if let Err(e) = self.pane_manager.add_agent_pane(&agent_id).await {
            warn!(agent_id = %agent_id, "failed to add pane: {e:#}");
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

    /// Handle one envelope from a wrapper.
    async fn handle_message(&self, agent_id: &str, env: Envelope) {
        match env.kind.as_str() {
            MSG_TASK_ACCEPT => {
                if let Ok(payload) = env.decode_payload::<TaskAccept>() {
                    info!(agent_id, task_id = %payload.task_id, "task accepted");
                    if let Err(e) = self.app_state.transition_task(payload.task_id, TaskState::Accepted) {
                        warn!("transition to Accepted failed: {e}");
                    }
                    self.broadcast_event(
                        "task.accepted",
                        json!({"task_id": payload.task_id.to_string(), "agent_id": agent_id}),
                    )
                    .await;
                }
            }

            MSG_TASK_PROPOSE => {
                if let Ok(payload) = env.decode_payload::<TaskPropose>() {
                    info!(agent_id, task_id = %payload.task_id, "task proposal received");
                    if let Err(e) = self.app_state.propose_task(
                        payload.task_id,
                        payload.brief,
                        payload.diff,
                    ) {
                        warn!("propose_task failed: {e}");
                    }
                    self.broadcast_event(
                        "task.proposed",
                        json!({"task_id": payload.task_id.to_string(), "agent_id": agent_id}),
                    )
                    .await;
                }
            }

            MSG_TASK_COMPLETE => {
                if let Ok(payload) = env.decode_payload::<TaskComplete>() {
                    info!(agent_id, task_id = %payload.task_id, "task complete");
                    let task_title = self.app_state.get_task(payload.task_id)
                        .map(|t| t.title.clone())
                        .unwrap_or_default();
                    if let Some(ref s) = payload.summary {
                        self.app_state.set_task_summary(payload.task_id, s.clone());
                    }
                    // Only broadcast task.completed if the transition actually
                    // succeeded — otherwise the orch hears "done" but state
                    // still says not-done.
                    match self.app_state.transition_task(payload.task_id, TaskState::Completed) {
                        Ok(_) => {
                            self.broadcast_event(
                                "task.completed",
                                json!({
                                    "task_id": payload.task_id.to_string(),
                                    "agent_id": agent_id,
                                    "summary": payload.summary,
                                }),
                            )
                            .await;
                        }
                        Err(e) => {
                            warn!("transition to Completed failed: {e}; not broadcasting");
                        }
                    }

                    // Notify orchestrator by injecting into its tmux session,
                    // but only if the session appears idle (at a prompt).
                    let agent_id_owned = agent_id.to_string();
                    let notify_msg = format!(
                        "[Alor] Task completed by {}: \"{}\" ({})",
                        agent_id_owned, task_title, &payload.task_id.to_string()[..8]
                    );
                    tokio::spawn(async move {
                        // Check if orchestrator is at a prompt before injecting.
                        let capture = tokio::process::Command::new("tmux")
                            .args(["capture-pane", "-p", "-t", "alor-orchestrator", "-S", "-3"])
                            .output()
                            .await;
                        let is_idle = match capture {
                            Ok(out) if out.status.success() => {
                                let text = String::from_utf8_lossy(&out.stdout);
                                // Scan the last few lines for any prompt character.
                                // Gemini's TUI puts the prompt mid-screen with a
                                // status bar below, so checking only the last line
                                // misses it. Also handles Claude (❯), bash ($/%),
                                // and generic (>) prompts.
                                text.lines()
                                    .rev()
                                    .take(5)
                                    .any(|line| {
                                        let t = line.trim();
                                        t == "❯" || t == "$" || t == "%" || t == ">"
                                            || t.starts_with("> ")
                                            || t.starts_with("❯ ")
                                            || t.ends_with('❯')
                                            || t.ends_with('>')
                                            || t.ends_with('$')
                                            || t.ends_with('%')
                                    })
                            }
                            _ => false,
                        };

                        if is_idle {
                            let _ = tokio::process::Command::new("tmux")
                                .args(["send-keys", "-t", "alor-orchestrator", "-l", &notify_msg])
                                .output()
                                .await;
                            let _ = tokio::process::Command::new("tmux")
                                .args(["send-keys", "-t", "alor-orchestrator", "Enter"])
                                .output()
                                .await;
                        } else {
                            tracing::info!(
                                "orchestrator busy, skipping tmux injection for: {notify_msg}"
                            );
                        }
                    });
                }
            }

            MSG_TASK_BLOCKED => {
                if let Ok(payload) = env.decode_payload::<TaskBlocked>() {
                    info!(agent_id, task_id = %payload.task_id, reason = %payload.reason, "task blocked");
                    if let Err(e) = self.app_state.transition_task(payload.task_id, TaskState::Blocked) {
                        warn!("transition to Blocked failed: {e}");
                    }
                    self.broadcast_event(
                        "task.blocked",
                        json!({
                            "task_id": payload.task_id.to_string(),
                            "agent_id": agent_id,
                            "reason": payload.reason,
                        }),
                    )
                    .await;
                }
            }

            MSG_USER_INTERVENTION => {
                // Ignore the payload's agent_id — use the connection-bound id
                // so a wrapper can't flag intervention on behalf of another.
                if env.decode_payload::<UserIntervention>().is_ok() {
                    info!(agent_id, "user intervention recorded");
                    let affected = self.app_state.record_user_intervention(agent_id);
                    let task_ids: Vec<String> =
                        affected.iter().map(|id| id.to_string()).collect();
                    self.broadcast_event(
                        "user.intervention",
                        json!({
                            "agent_id": agent_id,
                            "task_ids": task_ids,
                        }),
                    )
                    .await;
                }
            }

            MSG_WORKER_USER_INPUT => {
                // SDK-worker stdin forwarded as an event. agent_id is the
                // connection-bound one so a worker can't impersonate others.
                if let Ok(payload) = env.decode_payload::<WorkerUserInput>() {
                    info!(
                        agent_id,
                        during_task = payload.during_task,
                        bytes = payload.text.len(),
                        "worker user input received"
                    );
                    self.broadcast_event(
                        "worker.user_input",
                        json!({
                            "agent_id": agent_id,
                            "text": payload.text,
                            "during_task": payload.during_task,
                            "task_id": payload.task_id.map(|t| t.to_string()),
                        }),
                    )
                    .await;
                }
            }

            MSG_WORKER_ORCH_RESPONSE => {
                // Mirror of MSG_WORKER_USER_INPUT for the orch→worker→orch
                // reply direction. The worker fires this after the SDK turn
                // triggered by a sentinel-prefixed `cli.agent.send_message`
                // completes (ResultMessage). correlation_id is the uuid the
                // daemon embedded in the outbound sentinel so orch can match
                // the reply to its originating send.
                if let Ok(payload) = env.decode_payload::<WorkerOrchResponse>() {
                    info!(
                        agent_id,
                        correlation_id = %payload.correlation_id,
                        during_task = payload.during_task,
                        bytes = payload.text.len(),
                        "worker orch response received"
                    );
                    self.broadcast_event(
                        "worker.orch_response",
                        json!({
                            "agent_id": agent_id,
                            "correlation_id": payload.correlation_id.to_string(),
                            "text": payload.text,
                            "during_task": payload.during_task,
                            "task_id": payload.task_id.map(|t| t.to_string()),
                        }),
                    )
                    .await;
                }
            }

            MSG_STATUS_RESPONSE => {
                // Could update heartbeat timestamp here
                debug!(agent_id, "status response received");
            }

            MSG_ERROR => {
                if let Ok(payload) = env.decode_payload::<WrapperError>() {
                    error!(agent_id, message = %payload.message, "wrapper error");
                    self.broadcast_event(
                        "wrapper.error",
                        json!({
                            "agent_id": agent_id,
                            "message": payload.message,
                        }),
                    )
                    .await;
                }
            }

            other => {
                warn!(agent_id, kind = other, "unknown message type");
            }
        }
    }

    /// Handle a CLI message and return a response envelope.
    async fn handle_cli_message(&self, env: Envelope) -> Envelope {
        let correlation_id = env.correlation_id;

        match env.kind.as_str() {
            MSG_CLI_STATUS => {
                let agents = self.app_state.all_agents();
                let tasks = self.app_state.all_tasks();
                let connected: Vec<String> = self.writers.lock().await.keys().cloned().collect();
                match Envelope::new(
                    MSG_CLI_RESPONSE,
                    json!({
                        "agents": agents,
                        "tasks": tasks,
                        "connected": connected,
                    }),
                ) {
                    Ok(mut e) => {
                        e.correlation_id = correlation_id;
                        e
                    }
                    Err(_) => cli_error(correlation_id, "failed to build status response"),
                }
            }

            MSG_CLI_TASK_LIST => {
                let tasks = self.app_state.all_tasks();
                match Envelope::new(MSG_CLI_RESPONSE, json!({"tasks": tasks})) {
                    Ok(mut e) => {
                        e.correlation_id = correlation_id;
                        e
                    }
                    Err(_) => cli_error(correlation_id, "failed to build task list"),
                }
            }

            MSG_CLI_TASK_GET => {
                match env.decode_payload::<CliTaskGet>() {
                    Ok(payload) => match self.app_state.get_task(payload.task_id) {
                        Some(task) => match Envelope::new(MSG_CLI_RESPONSE, json!({"task": task})) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to serialize task"),
                        },
                        None => cli_error(correlation_id, &format!("task {} not found", payload.task_id)),
                    },
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_TASK_CREATE => {
                match env.decode_payload::<CliTaskCreate>() {
                    Ok(payload) => {
                        let mut task = Task::new(&payload.title, &payload.description);
                        task.project = payload.project.clone();
                        let task_id = task.id;
                        self.app_state.add_task(task);
                        info!(task_id = %task_id, title = %payload.title, "task created via CLI");
                        self.broadcast_event(
                            "task.created",
                            json!({"task_id": task_id.to_string(), "title": payload.title}),
                        )
                        .await;
                        match Envelope::new(
                            MSG_CLI_RESPONSE,
                            json!({"task_id": task_id.to_string()}),
                        ) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_TASK_CANCEL => {
                match env.decode_payload::<CliTaskCancel>() {
                    Ok(payload) => {
                        // Try Cancelled first, fall back to Rejected
                        let result = self
                            .app_state
                            .transition_task(payload.task_id, TaskState::Cancelled)
                            .or_else(|_| {
                                self.app_state
                                    .transition_task(payload.task_id, TaskState::Rejected)
                            });
                        match result {
                            Ok(_task) => {
                                self.broadcast_event(
                                    "task.cancelled",
                                    json!({"task_id": payload.task_id.to_string()}),
                                )
                                .await;
                                match Envelope::new(
                                    MSG_CLI_RESPONSE,
                                    json!({"cancelled": payload.task_id.to_string()}),
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
                                &format!("failed to cancel task: {e}"),
                            ),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_TASK_COMPLETE => {
                match env.decode_payload::<CliTaskCompletePayload>() {
                    Ok(payload) => {
                        // Stash summary *before* the transition so it is
                        // visible in the Completed task snapshot emitted by
                        // `tasks-changed` / `task.completed`.
                        if let Some(ref summary) = payload.summary {
                            self.app_state
                                .set_task_summary(payload.task_id, summary.clone());
                        }
                        match self
                            .app_state
                            .transition_task(payload.task_id, TaskState::Completed)
                        {
                            Ok(_task) => {
                                self.broadcast_event(
                                    "task.completed",
                                    json!({"task_id": payload.task_id.to_string()}),
                                )
                                .await;
                                match Envelope::new(
                                    MSG_CLI_RESPONSE,
                                    json!({"completed": payload.task_id.to_string()}),
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
                                &format!("failed to complete task: {e}"),
                            ),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_ASSIGN => {
                match env.decode_payload::<CliAssign>() {
                    Ok(payload) => {
                        // Enforce max_concurrent BEFORE doing any work. Counts
                        // non-terminal, non-pending tasks already assigned to
                        // this agent. If over capacity, reject cleanly so the
                        // caller can queue or spawn a sibling slot.
                        let active = self.app_state.agent_active_task_count(&payload.agent_id);
                        let max = self
                            .app_state
                            .get_agent(&payload.agent_id)
                            .map(|a| a.max_concurrent as usize)
                            .unwrap_or(1);
                        if active >= max {
                            return cli_error(
                                correlation_id,
                                &format!(
                                    "agent {} at capacity ({}/{}); spawn a sibling slot or wait",
                                    payload.agent_id, active, max
                                ),
                            );
                        }

                        // Look up task (no lock contention).
                        let task = match self.app_state.get_task(payload.task_id) {
                            Some(t) => t,
                            None => {
                                return cli_error(
                                    correlation_id,
                                    &format!("task {} not found", payload.task_id),
                                )
                            }
                        };

                        // If the task is tied to a project, prepend a TASK BRIEF
                        // with key files + docs so the agent has context.
                        let dispatch_description = match task.project.as_deref() {
                            Some(name) => match project::load_profile(name) {
                                Ok(Some(profile)) => project::build_task_brief(
                                    &profile,
                                    &task.title,
                                    &task.description,
                                ),
                                Ok(None) => {
                                    warn!(project = %name, "project profile not found; dispatching raw description");
                                    task.description.clone()
                                }
                                Err(e) => {
                                    warn!(project = %name, "failed to load project profile: {e}; dispatching raw description");
                                    task.description.clone()
                                }
                            },
                            None => task.description.clone(),
                        };

                        // Build the envelope before locking writers.
                        let assign_env = match Envelope::new(
                            MSG_TASK_ASSIGN,
                            TaskAssign {
                                task_id: task.id,
                                title: task.title.clone(),
                                description: dispatch_description,
                                timeout_secs: None,
                            },
                        ) {
                            Ok(e) => e,
                            Err(e) => {
                                return cli_error(
                                    correlation_id,
                                    &format!("failed to build assign envelope: {e}"),
                                )
                            }
                        };

                        // Hold the writers lock around persist+send so the
                        // agent can't disconnect between the two, and so
                        // state.json has the assignment before the wrapper
                        // hears about it (persist-before-broadcast).
                        {
                            let mut writers = self.writers.lock().await;
                            if !writers.contains_key(&payload.agent_id) {
                                return cli_error(
                                    correlation_id,
                                    &format!("agent {} is not connected", payload.agent_id),
                                );
                            }

                            // Persist assignment first.
                            if let Err(e) = self
                                .app_state
                                .assign_task_to_agent(payload.task_id, &payload.agent_id)
                            {
                                return cli_error(
                                    correlation_id,
                                    &format!("failed to update task state: {e}"),
                                );
                            }

                            let writer = writers.get_mut(&payload.agent_id).unwrap();
                            let mut line = match serde_json::to_string(&assign_env) {
                                Ok(l) => l,
                                Err(e) => {
                                    return cli_error(
                                        correlation_id,
                                        &format!("failed to serialize: {e}"),
                                    );
                                }
                            };
                            line.push('\n');
                            if let Err(e) = writer.write_all(line.as_bytes()).await {
                                // Wire write failed after we persisted — mark
                                // Stale so the slot doesn't stay at capacity.
                                let _ = self
                                    .app_state
                                    .transition_task(payload.task_id, TaskState::Stale);
                                return cli_error(
                                    correlation_id,
                                    &format!("failed to send to agent: {e}"),
                                );
                            }
                            let _ = writer.flush().await;
                        }

                        self.broadcast_event(
                            "task.assigned",
                            json!({
                                "task_id": payload.task_id.to_string(),
                                "agent_id": payload.agent_id,
                            }),
                        )
                        .await;

                        match Envelope::new(
                            MSG_CLI_RESPONSE,
                            json!({
                                "assigned": payload.task_id.to_string(),
                                "agent_id": payload.agent_id,
                            }),
                        ) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_SPAWN => {
                match env.decode_payload::<CliSpawn>() {
                    Ok(payload) => self.handle_spawn(correlation_id, payload).await,
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_DELETE => {
                match env.decode_payload::<CliDelete>() {
                    Ok(payload) => {
                        // Refuse to delete a template or fixed-slot yaml id —
                        // only template-spawned instances are tombstone-safe.
                        let is_yaml_slot = self
                            .agent_configs
                            .iter()
                            .any(|(id, _)| id == &payload.instance);
                        if is_yaml_slot {
                            return cli_error(
                                correlation_id,
                                &format!(
                                    "refusing to delete '{}': core yaml slot. Kill it instead.",
                                    payload.instance
                                ),
                            );
                        }

                        // Make sure any running process/session is gone first —
                        // delete implies kill, so don't leave an orphan.
                        {
                            let mut spawned = self.spawned.lock().await;
                            if let Some(mut child) = spawned.remove(&payload.instance) {
                                let _ = child.kill();
                            }
                        }
                        // `=` prefix = tmux exact-match, so `alor-claude` can't
                        // accidentally kill `alor-claude-alor`.
                        let tmux_session = format!("=alor-{}", payload.instance);
                        let _ = std::process::Command::new("tmux")
                            .args(["kill-session", "-t", &tmux_session])
                            .output();

                        let removed = self.app_state.remove_agent(&payload.instance);
                        if !removed {
                            return cli_error(
                                correlation_id,
                                &format!("agent '{}' not found in state", payload.instance),
                            );
                        }

                        info!(instance = %payload.instance, "agent tombstoned via CLI");
                        self.broadcast_event(
                            "agent.deleted",
                            json!({"instance": &payload.instance}),
                        )
                        .await;

                        match Envelope::new(
                            MSG_CLI_RESPONSE,
                            json!({"deleted": payload.instance}),
                        ) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_KILL => {
                match env.decode_payload::<CliKill>() {
                    Ok(payload) => {
                        // Try to kill the child process
                        let killed_child = {
                            let mut spawned = self.spawned.lock().await;
                            if let Some(mut child) = spawned.remove(&payload.instance) {
                                // Send SIGTERM
                                let _ = child.kill();
                                true
                            } else {
                                false
                            }
                        };

                        // Also kill the tmux session (exact-match target).
                        let tmux_session = format!("=alor-{}", payload.instance);
                        let _ = std::process::Command::new("tmux")
                            .args(["kill-session", "-t", &tmux_session])
                            .output();

                        let msg = if killed_child {
                            format!("killed process and tmux session for {}", payload.instance)
                        } else {
                            format!("killed tmux session for {} (no tracked child)", payload.instance)
                        };

                        info!(instance = %payload.instance, "{msg}");

                        match Envelope::new(MSG_CLI_RESPONSE, json!({"killed": payload.instance, "message": msg})) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_PROJECT_LIST => {
                match project::list_profiles() {
                    Ok(profiles) => {
                        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
                        match Envelope::new(MSG_CLI_RESPONSE, json!({"projects": names})) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("failed to list projects: {e}")),
                }
            }

            MSG_CLI_PROJECT_GET => {
                match env.decode_payload::<CliProjectGet>() {
                    Ok(payload) => match project::load_profile(&payload.name) {
                        Ok(Some(profile)) => {
                            match Envelope::new(MSG_CLI_RESPONSE, json!({"project": profile})) {
                                Ok(mut e) => {
                                    e.correlation_id = correlation_id;
                                    e
                                }
                                Err(_) => cli_error(correlation_id, "failed to serialize project"),
                            }
                        }
                        Ok(None) => cli_error(
                            correlation_id,
                            &format!("project '{}' not found", payload.name),
                        ),
                        Err(e) => cli_error(
                            correlation_id,
                            &format!("failed to load project: {e}"),
                        ),
                    },
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_PROJECT_SAVE => {
                match env.decode_payload::<CliProjectSave>() {
                    Ok(payload) => {
                        let mut profile = project::ProjectProfile::new(&payload.name);
                        if let Some(desc) = payload.description {
                            profile.description = desc;
                        }
                        if let Some(root) = payload.root_dir {
                            profile.root_dir = Some(root);
                        }
                        if let Some(stack) = payload.stack {
                            profile.stack = stack;
                        }
                        if let Some(kf) = payload.key_files {
                            profile.key_files = kf;
                        }
                        if let Some(dp) = payload.doc_paths {
                            profile.doc_paths = dp;
                        }
                        if let Some(ref mi) = payload.memory_index {
                            profile.memory_index = Some(mi.clone());
                        }

                        if let Err(e) = project::save_profile(&profile) {
                            return cli_error(
                                correlation_id,
                                &format!("failed to save project: {e}"),
                            );
                        }

                        let mut hub_path_str: Option<String> = None;
                        if let Some(mi) = payload.memory_index.as_deref() {
                            let agent = payload.memory_agent.as_deref().unwrap_or("claude");
                            match crate::daemon::memory::link_agent_memory(
                                &payload.name,
                                agent,
                                std::path::PathBuf::from(mi),
                            ) {
                                Ok(hub_path) => {
                                    info!(
                                        project = %payload.name,
                                        agent = %agent,
                                        hub_path = %hub_path.display(),
                                        "agent memory linked into hub"
                                    );
                                    hub_path_str = Some(hub_path.display().to_string());
                                }
                                Err(e) => {
                                    return cli_error(
                                        correlation_id,
                                        &format!("failed to link agent memory: {e}"),
                                    );
                                }
                            }
                        }

                        match Envelope::new(
                            MSG_CLI_RESPONSE,
                            json!({
                                "saved": payload.name,
                                "hub_path": hub_path_str,
                            }),
                        ) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_AGENT_ENSURE_RUNNING => {
                match env.decode_payload::<CliAgentEnsureRunning>() {
                    Ok(payload) => {
                        if payload.agent_id.is_empty()
                            || payload.agent_id.len() > 64
                            || !payload
                                .agent_id
                                .chars()
                                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                        {
                            return cli_error(
                                correlation_id,
                                &format!("invalid agent_id: {:?}", payload.agent_id),
                            );
                        }

                        // Already connected? No-op.
                        if self.writers.lock().await.contains_key(&payload.agent_id) {
                            match Envelope::new(
                                MSG_CLI_RESPONSE,
                                json!({
                                    "agent_id": &payload.agent_id,
                                    "spawned": false,
                                    "reason": "already connected",
                                }),
                            ) {
                                Ok(mut e) => {
                                    e.correlation_id = correlation_id;
                                    return e;
                                }
                                Err(_) => {
                                    return cli_error(correlation_id, "failed to build response")
                                }
                            }
                        }

                        // Idempotent spawn: first try yaml-declared slot, then
                        // a state-persisted instance that was spawned from a
                        // template. Everything else is an error.
                        let has_config = self
                            .agent_configs
                            .iter()
                            .any(|(id, _)| id == &payload.agent_id);

                        let spawn_payload = if has_config {
                            CliSpawn {
                                name: Some(payload.agent_id.clone()),
                                agent: payload.agent_id.clone(),
                                role: None,
                                project: None,
                                working_dir: None,
                            }
                        } else if let Some(existing) = self.app_state.get_agent(&payload.agent_id) {
                            match existing.template.as_deref() {
                                Some(tmpl)
                                    if self
                                        .agent_configs
                                        .iter()
                                        .any(|(id, _)| id == tmpl) =>
                                {
                                    CliSpawn {
                                        name: Some(payload.agent_id.clone()),
                                        agent: tmpl.to_string(),
                                        role: None,
                                        project: existing.project.clone(),
                                        working_dir: existing.working_dir.clone(),
                                    }
                                }
                                _ => {
                                    return cli_error(
                                        correlation_id,
                                        &format!(
                                            "agent '{}' has no yaml config and no usable template",
                                            payload.agent_id
                                        ),
                                    )
                                }
                            }
                        } else {
                            return cli_error(
                                correlation_id,
                                &format!(
                                    "no yaml config for agent '{}'; use agent_spawn with an explicit base instead",
                                    payload.agent_id
                                ),
                            );
                        };
                        let resp = self.handle_spawn(correlation_id, spawn_payload).await;
                        // Re-label the success envelope to expose `spawned: true`.
                        if resp.kind == MSG_CLI_RESPONSE {
                            match Envelope::new(
                                MSG_CLI_RESPONSE,
                                json!({
                                    "agent_id": &payload.agent_id,
                                    "spawned": true,
                                }),
                            ) {
                                Ok(mut e) => {
                                    e.correlation_id = correlation_id;
                                    return e;
                                }
                                Err(_) => {
                                    return cli_error(correlation_id, "failed to build response")
                                }
                            }
                        }
                        resp
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_AGENT_SEND_MESSAGE => {
                match env.decode_payload::<CliAgentSendMessage>() {
                    Ok(payload) => {
                        if payload.agent_id.is_empty()
                            || payload.agent_id.len() > 64
                            || !payload
                                .agent_id
                                .chars()
                                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                        {
                            return cli_error(
                                correlation_id,
                                &format!("invalid agent_id: {:?}", payload.agent_id),
                            );
                        }
                        // Exact-match target — `alor-claude` must not fall
                        // through to `alor-claude-alor` and deliver to the
                        // wrong worker.  tmux's target syntax splits: bare
                        // `=name` resolves a session (what has-session
                        // wants), but send-keys takes a target-pane so we
                        // need `=name:` to pick the active pane of the
                        // exact session.  Before fixing this split,
                        // send-keys silently failed with "can't find pane"
                        // and the daemon reported success to the orch
                        // anyway — every agent_send_message was a no-op.
                        let session_target = format!("=alor-{}", payload.agent_id);
                        let pane_target = format!("{session_target}:");
                        let session_exists = tokio::process::Command::new("tmux")
                            .args(["has-session", "-t", &session_target])
                            .output()
                            .await
                            .map(|o| o.status.success())
                            .unwrap_or(false);
                        if !session_exists {
                            return cli_error(
                                correlation_id,
                                &format!("no tmux session for agent {}", payload.agent_id),
                            );
                        }
                        // Runtime-type gate. BEGIN/END framing is only
                        // safe for `claude-sdk` workers whose stdin loop
                        // implements the sentinel state machine
                        // (orchestrator-py/worker.py). For any other
                        // runtime (wrapper, future codex/gemini) the
                        // sentinel bytes would land verbatim in the
                        // CLI's pty — visible garbage to the user plus
                        // a wedged send from the orch's POV (no
                        // `worker.orch_response` will ever echo back).
                        //
                        // Fail-closed: unknown/unresolvable agents are
                        // rejected rather than silently downgraded, so
                        // a caller passing a typo'd agent_id gets a
                        // clear error instead of a framed send to an
                        // agent we can't prove is SDK-backed.
                        //
                        // Callers that genuinely want to reach a
                        // wrapper-runtime worker must set
                        // `suppress_echo: false` explicitly.
                        if payload.suppress_echo
                            && !is_framed_send_allowed(
                                &self.agent_configs,
                                &self.app_state,
                                &payload.agent_id,
                            )
                        {
                            return cli_error(
                                correlation_id,
                                &format!(
                                    "framed send (suppress_echo=true) not supported for agent '{}': \
                                     only claude-sdk runtime workers implement BEGIN/END framing. \
                                     Retry with suppress_echo=false to inject raw text.",
                                    payload.agent_id
                                ),
                            );
                        }
                        // If the caller asked to suppress the stdin echo
                        // (orch → SDK-worker path), wrap the payload in
                        // BEGIN/END framing so the worker's stdin state
                        // machine can (a) skip emitting `worker.user_input`
                        // across the whole frame, (b) accumulate all body
                        // lines before dispatching a single SDK turn, and
                        // (c) echo the same correlation_id back in the
                        // `worker.orch_response` event after the turn
                        // completes.
                        //
                        // On-wire format (fed to `tmux send-keys -l` — tmux
                        // converts each literal `\n` into an Enter
                        // keystroke, so BEGIN / each body line / END land
                        // as separate read_line calls in the worker):
                        //
                        //   {BEGIN}{uuid}\n
                        //   <multi-line body>\n
                        //   {END}{uuid}\n
                        //
                        // The trailing `\n` after END is mandatory — without
                        // it the END marker sits in prompt_toolkit's buffer
                        // unsubmitted and the worker never closes the frame.
                        // For that reason the caller's `submit` flag is
                        // ignored on the suppress_echo path; the framing
                        // submits every marker for us.
                        let send_correlation_id: Option<Uuid> = if payload.suppress_echo {
                            Some(Uuid::new_v4())
                        } else {
                            None
                        };
                        let effective_text = match send_correlation_id {
                            Some(cid) => format!(
                                "{}{}\n{}\n{}{}\n",
                                WORKER_ECHO_SENTINEL_BEGIN,
                                cid,
                                payload.text,
                                WORKER_ECHO_SENTINEL_END,
                                cid,
                            ),
                            None => payload.text.clone(),
                        };
                        let send_out = tokio::process::Command::new("tmux")
                            .args(["send-keys", "-t", &pane_target, "-l", &effective_text])
                            .output()
                            .await;
                        let send_ok = match &send_out {
                            Ok(o) if o.status.success() => true,
                            Ok(o) => {
                                return cli_error(
                                    correlation_id,
                                    &format!(
                                        "tmux send-keys rejected target {pane_target}: {}",
                                        String::from_utf8_lossy(&o.stderr).trim()
                                    ),
                                );
                            }
                            Err(e) => {
                                return cli_error(
                                    correlation_id,
                                    &format!("tmux send-keys failed: {e}"),
                                );
                            }
                        };
                        let _ = send_ok; // consume
                        // Only the non-framed (Fett-typed style) path honors
                        // the explicit submit flag — framed sends already
                        // carry their own Enter via the trailing \n above.
                        if payload.submit && send_correlation_id.is_none() {
                            let _ = tokio::process::Command::new("tmux")
                                .args(["send-keys", "-t", &pane_target, "Enter"])
                                .output()
                                .await;
                        }
                        info!(
                            agent_id = %payload.agent_id,
                            submit = payload.submit,
                            bytes = payload.text.len(),
                            suppress_echo = payload.suppress_echo,
                            send_correlation_id = ?send_correlation_id,
                            "cli.agent.send_message"
                        );
                        let response_payload = json!({
                            "sent": payload.agent_id,
                            "submit": payload.submit,
                            // Only present when suppress_echo=true — callers
                            // use this to match the subsequent
                            // `worker.orch_response` event back to this send.
                            "correlation_id": send_correlation_id.map(|c| c.to_string()),
                        });
                        match Envelope::new(MSG_CLI_RESPONSE, response_payload) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_MEMORY_GET => {
                match env.decode_payload::<CliMemoryGet>() {
                    Ok(payload) => {
                        let hub_dir = match project::memory_hub_dir(&payload.project) {
                            Ok(d) => d,
                            Err(e) => {
                                return cli_error(
                                    correlation_id,
                                    &format!("invalid project: {e}"),
                                )
                            }
                        };
                        if !hub_dir.exists() {
                            match Envelope::new(
                                MSG_CLI_RESPONSE,
                                json!({"project": payload.project, "files": {}}),
                            ) {
                                Ok(mut e) => {
                                    e.correlation_id = correlation_id;
                                    return e;
                                }
                                Err(_) => {
                                    return cli_error(
                                        correlation_id,
                                        "failed to build response",
                                    )
                                }
                            }
                        }
                        let entries = match std::fs::read_dir(&hub_dir) {
                            Ok(it) => it,
                            Err(e) => {
                                return cli_error(
                                    correlation_id,
                                    &format!("read hub dir: {e}"),
                                )
                            }
                        };
                        let mut files = serde_json::Map::new();
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if !path.is_file() {
                                continue;
                            }
                            let name = match path.file_name().and_then(|s| s.to_str()) {
                                Some(n) => n.to_string(),
                                None => continue,
                            };
                            match std::fs::metadata(&path) {
                                Ok(meta) if meta.len() > 1_048_576 => {
                                    files.insert(
                                        name,
                                        json!({
                                            "truncated": true,
                                            "size": meta.len(),
                                        }),
                                    );
                                    continue;
                                }
                                _ => {}
                            }
                            match std::fs::read_to_string(&path) {
                                Ok(content) => {
                                    files.insert(name, json!(content));
                                }
                                Err(e) => {
                                    files.insert(name, json!({"error": e.to_string()}));
                                }
                            }
                        }
                        match Envelope::new(
                            MSG_CLI_RESPONSE,
                            json!({
                                "project": payload.project,
                                "files": files,
                            }),
                        ) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_INTEGRATIONS_GET => {
                let config_dir = match crate::daemon::session::config_dir() {
                    Ok(d) => d,
                    Err(e) => return cli_error(correlation_id, &format!("config dir error: {e}")),
                };
                let path = config_dir.join("integrations.yaml");

                let integrations = if path.exists() {
                    match std::fs::read_to_string(&path) {
                        Ok(content) => match serde_yaml::from_str::<serde_json::Value>(&content) {
                            Ok(v) => v,
                            Err(e) => json!({"error": format!("invalid yaml: {e}")}),
                        },
                        Err(e) => json!({"error": format!("read error: {e}")}),
                    }
                } else {
                    json!({"sources": []})
                };

                match Envelope::new(MSG_CLI_RESPONSE, integrations) {
                    Ok(mut e) => {
                        e.correlation_id = correlation_id;
                        e
                    }
                    Err(_) => cli_error(correlation_id, "failed to build response"),
                }
            }

            other => cli_error(correlation_id, &format!("unknown CLI command: {other}")),
        }
    }

    /// Generate a unique instance ID for an agent kind.
    /// If `base` is not already taken, returns it as-is.
    /// Otherwise appends `-2`, `-3`, etc. until a free ID is found.
    fn unique_instance_id(&self, base: &str) -> String {
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
    async fn handle_spawn(&self, correlation_id: Uuid, payload: CliSpawn) -> Envelope {
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
                self.broadcast_event(
                    "agent.spawned",
                    json!({"agent": &payload.agent, "instance_id": &instance_id, "pid": pid}),
                )
                .await;
                match Envelope::new(
                    MSG_CLI_RESPONSE,
                    json!({"spawned": &instance_id, "pid": pid}),
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

    /// Broadcast an event to all event stream subscribers.
    async fn broadcast_event(&self, event_type: &str, data: serde_json::Value) {
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
        let handles: Vec<(u64, Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>)> = {
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

    /// Send an envelope to a specific wrapper.
    pub async fn send_to(&self, agent_id: &str, envelope: &Envelope) -> Result<()> {
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

}

fn cli_error(correlation_id: Uuid, message: &str) -> Envelope {
    Envelope {
        kind: MSG_CLI_ERROR.to_string(),
        correlation_id,
        payload: serde_json::json!({"error": message}),
    }
}

/// Runtime layer that hosts `claude-sdk` workers and speaks the
/// BEGIN/END framing state machine in its stdin loop (orchestrator-py's
/// worker.py). Any other runtime (wrapper, future codex/gemini) has a
/// raw pty with no framing awareness — injecting sentinel-wrapped
/// payloads would splat literal `__ALOR_ORCH_ECHO_BEGIN__<uuid>` lines
/// into the CLI's prompt.
const SDK_FRAMED_RUNTIME: &str = "claude-sdk";

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
fn resolve_agent_runtime(
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
fn is_framed_send_allowed(
    agent_configs: &[(String, AgentConfig)],
    app_state: &AppState,
    agent_id: &str,
) -> bool {
    matches!(
        resolve_agent_runtime(agent_configs, app_state, agent_id).as_deref(),
        Some(SDK_FRAMED_RUNTIME)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::state::Agent;

    /// Build a minimal AgentConfig with just `runtime` set; every other
    /// field defaults. Kept here rather than in config.rs because these
    /// tests are the only thing that need a programmatically-built
    /// config (production code deserializes from yaml).
    fn cfg(runtime: &str) -> AgentConfig {
        // Round-trip through yaml — simpler than manually filling every
        // field, and exercises the same defaults the loader uses in
        // production.
        let yaml = format!("runtime: {runtime}\n");
        serde_yaml::from_str::<AgentConfig>(&yaml).expect("valid test yaml")
    }

    #[test]
    fn framed_send_allowed_for_claude_sdk_slot() {
        let configs = vec![("alor".to_string(), cfg("claude-sdk"))];
        let state = AppState::new();
        assert!(is_framed_send_allowed(&configs, &state, "alor"));
    }

    #[test]
    fn framed_send_rejected_for_wrapper_slot() {
        // The latent hazard: default runtime is "wrapper", and a
        // framed send to one would splat literal BEGIN/END markers
        // into the CLI's pty.
        let configs = vec![("codex".to_string(), cfg("wrapper"))];
        let state = AppState::new();
        assert!(!is_framed_send_allowed(&configs, &state, "codex"));
    }

    #[test]
    fn framed_send_rejected_for_unknown_agent() {
        // Fail-closed: no config + no state entry means we can't prove
        // it's SDK-framed-safe, so reject.
        let configs = vec![("alor".to_string(), cfg("claude-sdk"))];
        let state = AppState::new();
        assert!(!is_framed_send_allowed(&configs, &state, "ghost"));
    }

    #[test]
    fn framed_send_allowed_for_claude_sdk_template_instance() {
        // Template-spawned instance: no direct config entry, but its
        // `template` field points at a yaml slot whose runtime is
        // claude-sdk. Must resolve transitively.
        let configs = vec![("claude".to_string(), cfg("claude-sdk"))];
        let state = AppState::new();
        let mut instance = Agent::new("claude-mandaspace", "claude-mandaspace");
        instance.template = Some("claude".to_string());
        state.register_agent(instance);
        assert!(is_framed_send_allowed(
            &configs,
            &state,
            "claude-mandaspace"
        ));
    }

    #[test]
    fn framed_send_rejected_for_wrapper_template_instance() {
        // Same transitive lookup, but the template is a wrapper
        // runtime — the gate must still refuse.
        let configs = vec![("codex".to_string(), cfg("wrapper"))];
        let state = AppState::new();
        let mut instance = Agent::new("codex-scratch", "codex-scratch");
        instance.template = Some("codex".to_string());
        state.register_agent(instance);
        assert!(!is_framed_send_allowed(&configs, &state, "codex-scratch"));
    }

    #[test]
    fn framed_send_rejected_when_template_points_at_missing_config() {
        // Defensive: if an instance's template id no longer resolves
        // (e.g. yaml was deleted between spawn and now), we can't
        // prove runtime, so fail-closed.
        let configs: Vec<(String, AgentConfig)> = vec![];
        let state = AppState::new();
        let mut instance = Agent::new("orphan", "orphan");
        instance.template = Some("gone".to_string());
        state.register_agent(instance);
        assert!(!is_framed_send_allowed(&configs, &state, "orphan"));
    }

    #[test]
    fn resolve_runtime_direct_slot_wins_over_template() {
        // If an agent_id exists as both a direct yaml slot AND a
        // state-persisted instance (shouldn't happen in practice, but
        // belts-and-suspenders), the direct config is authoritative.
        let configs = vec![("alor".to_string(), cfg("claude-sdk"))];
        let state = AppState::new();
        let mut instance = Agent::new("alor", "alor");
        instance.template = Some("some-wrapper-template".to_string());
        state.register_agent(instance);
        assert_eq!(
            resolve_agent_runtime(&configs, &state, "alor").as_deref(),
            Some("claude-sdk")
        );
    }
}
