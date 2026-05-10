/// Tauri IPC command handlers.
///
/// These are the functions the frontend calls via `invoke(...)`.
/// All commands receive the shared AppState via Tauri's managed state.

use crate::daemon::session::SessionInfo;
use crate::daemon::settings::AlorSettings;
use crate::daemon::state::{Agent, AppState, Task, TaskState};
use crate::terminal::bridge::TerminalBridge;
use crate::terminal::pane_manager::PaneManager;
use crate::wrapper::protocol::{DaemonShutdown, Envelope, TaskAssign, MSG_SHUTDOWN, MSG_TASK_ASSIGN};
use crate::wrapper::server::SocketServer;
use tauri::State;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Error helper — Tauri commands must return String errors for serialization
// ---------------------------------------------------------------------------

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ---------------------------------------------------------------------------
// Task commands
// ---------------------------------------------------------------------------

/// Return all tasks (unsorted).
#[tauri::command]
pub fn get_tasks(state: State<'_, AppState>) -> Vec<Task> {
    state.all_tasks()
}

/// Return one task by UUID string.
#[tauri::command]
pub fn get_task(state: State<'_, AppState>, id: String) -> Result<Task, String> {
    let uuid = Uuid::parse_str(&id).map_err(err)?;
    state.get_task(uuid).ok_or_else(|| format!("task {id} not found"))
}

/// Create a new task and assign it to an agent.
///
/// If agent_id is provided and the wrapper is connected, the task is
/// dispatched to the wrapper immediately via the full state-machine
/// path: `assign_task_to_agent` runs `Pending → Assigned` under lock,
/// wire-sends the `task.assign` envelope, then registers the
/// accept-handshake watchdog (bounded timeout → revert-to-pending on
/// no ack).
///
/// **Pre-fix bug (codex-alor audit).** Earlier this handler set
/// `task.assigned_to` directly, called `add_task`, and shipped
/// `task.assign` on the wire — but never transitioned the task out
/// of `Pending`. The worker's subsequent `task.accept` then tried a
/// `Pending → Accepted` transition, which is illegal (only
/// `Assigned → Accepted` is legal), so the daemon logged
/// `"transition to Accepted failed: ..."` and the task stayed in
/// `Pending` forever. It also skipped the `max_concurrent` capacity
/// check the CLI path (`cli.assign` in
/// `wrapper/server/routing.rs::MSG_CLI_ASSIGN`) enforces. Errors from
/// the revert-to-Stale step were silently swallowed via `let _ =
/// ...`.
///
/// **Fix**: mirror the `cli.assign` arm — check capacity, call
/// `assign_task_to_agent` for the atomic state+metadata transition,
/// send the envelope, register the watchdog. All transition errors
/// propagate via `Result` so the UI surfaces them instead of
/// silently landing a stale Pending task.
#[tauri::command]
pub async fn assign_task(
    state: State<'_, AppState>,
    server: State<'_, SocketServer>,
    title: String,
    description: String,
    agent_id: Option<String>,
    project: Option<String>,
) -> Result<Task, String> {
    // Delegate to the state-reference inner so tests can drive this
    // path without constructing a Tauri `State<'_, _>` fixture.
    assign_task_inner(&state, &server, title, description, agent_id, project).await
}

/// Implementation of `assign_task` that takes plain references
/// instead of Tauri managed-state wrappers. Enables unit tests in
/// this file (see `#[cfg(test)]` below) to drive the full dispatch
/// path — create-task → capacity-check → state transition → wire
/// send → watchdog registration — using a fake `SocketServer`
/// constructed via `with_configs(...)`.
pub(crate) async fn assign_task_inner(
    state: &AppState,
    server: &SocketServer,
    title: String,
    description: String,
    agent_id: Option<String>,
    project: Option<String>,
) -> Result<Task, String> {
    let mut task = Task::new(title.clone(), description.clone());
    task.project = project.clone();
    // NOTE: deliberately NOT setting `task.assigned_to` here — that
    // field is the business of `assign_task_to_agent`, which sets
    // it atomically alongside the `Pending → Assigned` transition.
    // Pre-fix, setting it here early created a zombie shape:
    // assigned_to populated but state still Pending.
    let task_id = task.id;
    state.add_task(task.clone());

    tracing::info!(
        task_id = %task_id,
        agent = ?agent_id,
        project = ?project,
        "task created"
    );

    let Some(aid) = agent_id else {
        // No agent specified → leave task in Pending for later
        // orchestrator dispatch. Nothing to wire-send.
        return state
            .get_task(task_id)
            .ok_or_else(|| "task not found".to_string());
    };

    // -----------------------------------------------------------
    // Agent specified — run the full dispatch path.
    // -----------------------------------------------------------

    // Gate 1: is the wrapper connected? If not, task stays Pending
    // (recoverable — operator can retry once the wrapper reconnects).
    // We deliberately do NOT try to transition Pending → Stale here
    // because that edge is not legal in the state machine; pre-fix
    // the `let _ = ...` swallowed that exact illegal-transition
    // error, hiding the bug. Mirrors `cli.assign` — it returns an
    // error and leaves the task untouched.
    if !server.is_connected(&aid).await {
        tracing::warn!(agent_id = %aid, task_id = %task_id, "wrapper not connected");
        return Err(format!(
            "agent {aid} is not connected; task {task_id} stays Pending for manual retry"
        ));
    }

    // Gate 2: capacity check. Mirrors cli.assign's
    // `agent_active_task_count` vs `max_concurrent` check — without
    // this, the UI path could push a slot past its declared limit
    // (especially dangerous for max_concurrent=1 agents, which are
    // the default). Task stays Pending so the operator can either
    // wait for the slot to free or reassign elsewhere.
    let active = state.agent_active_task_count(&aid);
    let max = state
        .get_agent(&aid)
        .map(|a| a.max_concurrent as usize)
        .unwrap_or(1);
    if active >= max {
        tracing::warn!(
            agent_id = %aid,
            active,
            max,
            task_id = %task_id,
            "agent at capacity; task stays Pending"
        );
        return Err(format!(
            "agent {aid} at capacity ({active}/{max}); spawn a sibling slot or wait",
        ));
    }

    // Build the dispatch envelope. If a project is attached, prepend
    // the TASK BRIEF (key files + docs) so the worker has context.
    let dispatch_description = match project.as_deref() {
        Some(name) => match crate::daemon::project::load_profile(name) {
            Ok(Some(profile)) => {
                crate::daemon::project::build_task_brief(&profile, &title, &description)
            }
            Ok(None) => {
                tracing::warn!(project = %name, "project profile not found; dispatching raw description");
                description.clone()
            }
            Err(e) => {
                tracing::warn!(project = %name, "failed to load project profile: {e}; dispatching raw description");
                description.clone()
            }
        },
        None => description.clone(),
    };
    let payload = TaskAssign {
        task_id,
        title: title.clone(),
        description: dispatch_description,
        timeout_secs: None,
    };
    let envelope = Envelope::new(MSG_TASK_ASSIGN, &payload).map_err(err)?;

    // THE FIX: atomic state+metadata transition. `assign_task_to_agent`
    // runs `Pending → Assigned` under one lock, sets `assigned_to`,
    // records the task on the agent's history, and emits
    // `tasks-changed` + `agents-changed`. Must run BEFORE the wire
    // send so the daemon-side state is correct when the worker's
    // subsequent `task.accept` (if successful) fires the legal
    // `Assigned → Accepted` transition.
    state
        .assign_task_to_agent(task_id, &aid)
        .map_err(err)?;

    // Wire send. On failure, revert to Stale — propagate both
    // errors if the revert itself fails (better than swallowing
    // and leaving a zombie Assigned state).
    if let Err(send_err) = server.send_to(&aid, &envelope).await {
        tracing::error!(agent_id = %aid, "dispatch failed after assign: {send_err}");
        if let Err(stale_err) = state.transition_task(task_id, TaskState::Stale) {
            return Err(format!(
                "dispatch failed: {send_err}; and failed to mark Stale: {stale_err}",
            ));
        }
        return Err(format!("dispatch failed: {send_err}"));
    }

    // Accept-handshake watchdog. Mirrors cli.assign — bounds the
    // window that a task can sit in `Assigned` if the worker never
    // sends `task.accept`. After `ACCEPT_ACK_TIMEOUT_SECS`, the
    // watchdog reverts the task via
    // `revert_assignment_on_accept_timeout` (Assigned → Pending or
    // Assigned → AcceptFailed depending on retry count).
    server
        .register_and_spawn_accept_watchdog(task_id, aid.clone())
        .await;

    tracing::info!(task_id = %task_id, agent_id = %aid, "task dispatched to wrapper");

    state
        .get_task(task_id)
        .ok_or_else(|| "task not found".to_string())
}

/// Cancel a task by UUID string.
///
/// Accepts any current state:
///   - Non-terminal (Pending/Assigned/Accepted/…) → normal cancel edge.
///   - STALE / COMPLETED / REJECTED / TIMED_OUT → user-initiated
///     finalize-from-terminal; state machine allows it as bookkeeping
///     (see `TaskState::can_transition_to`). The UI surfaces a confirm
///     prompt before firing this RPC so the user acknowledges they're
///     re-labeling an already-done task.
///   - CANCELLED → no-op, returns the task unchanged. Matches the
///     "double-click shouldn't error" contract enforced in
///     `AppState::transition_task`.
#[tauri::command]
pub fn cancel_task(state: State<'_, AppState>, id: String) -> Result<Task, String> {
    let uuid = Uuid::parse_str(&id).map_err(err)?;
    state
        .transition_task(uuid, TaskState::Cancelled)
        .map_err(err)
}

/// Approve a proposed task, transitioning it to Staged and notifying the agent.
#[tauri::command]
pub async fn approve_task(
    state: State<'_, AppState>,
    server: State<'_, SocketServer>,
    id: String,
) -> Result<Task, String> {
    let uuid = Uuid::parse_str(&id).map_err(err)?;
    let task = state
        .transition_task(uuid, TaskState::Staged)
        .map_err(err)?;

    // If task is assigned to an agent, notify them of the approval.
    // We reuse MSG_TASK_ASSIGN to tell them to proceed with the now-approved work.
    if let Some(ref aid) = task.assigned_to {
        if server.is_connected(aid).await {
            let payload = TaskAssign {
                task_id: task.id,
                title: task.title.clone(),
                description: format!("APPROVED: {}", task.description),
                timeout_secs: None,
            };
            let envelope = Envelope::new(MSG_TASK_ASSIGN, &payload).map_err(err)?;
            let _ = server.send_to(aid, &envelope).await;
        }
    }

    Ok(task)
}

// ---------------------------------------------------------------------------
// Agent commands
// ---------------------------------------------------------------------------

/// Return all registered agents.
#[tauri::command]
pub fn get_agents(state: State<'_, AppState>) -> Vec<Agent> {
    state.all_agents()
}

/// Register a new agent.  If an agent with this ID already exists it is
/// overwritten (useful for reconnects).
#[tauri::command]
pub fn register_agent(
    state: State<'_, AppState>,
    id: String,
    name: String,
    tmux_session: Option<String>,
) -> Agent {
    let mut agent = Agent::new(id, name);
    agent.tmux_session = tmux_session;
    let clone = agent.clone();
    state.register_agent(agent);
    tracing::info!(agent_id = %clone.id, name = %clone.name, "agent registered via IPC");
    clone
}

/// Spawn a new agent process based on its config.
///
/// Runtime-aware: dispatches to the right launcher per the yaml's
/// `runtime` field. Pre-fix this always called `force_launch_wrapper`
/// which silently mis-launched `claude-sdk` yamls — the UI button
/// would appear to succeed but the agent never showed up because
/// alor-wrapper can't drive a claude-sdk worker. Fixed in lockstep
/// with the CLI spawn path (`wrapper/server/agent_lifecycle.rs::
/// handle_spawn`) so both Tauri IPC and orch-facing `cli.spawn`
/// honor `runtime`. See codex-alor-report.md concern #1.
#[tauri::command]
pub async fn spawn_agent(
    _state: State<'_, AppState>,
    agent: String,
    role: Option<String>,
) -> Result<(), String> {
    use crate::daemon::config;

    // Load config for just this slot.
    let configs = config::load_agent_configs().map_err(err)?;
    let mut target_cfg = configs.into_iter()
        .find(|(id, _)| id == &agent)
        .ok_or_else(|| format!("agent config {agent} not found"))?;

    if let Some(r) = role {
        target_cfg.1.role = r;
    }

    tracing::info!(
        agent_id = %target_cfg.0,
        runtime = %target_cfg.1.runtime,
        "manually spawning agent via Tauri IPC"
    );
    crate::daemon::config::force_launch_agent(target_cfg.0, target_cfg.1);
    Ok(())
}

/// Kill a specific running agent and its tmux session.
#[tauri::command]
pub async fn kill_agent(
    state: State<'_, AppState>,
    pm: State<'_, PaneManager>,
    server: State<'_, SocketServer>,
    id: String,
) -> Result<(), String> {
    // 1. Tell the wrapper to shut down gracefully
    if server.is_connected(&id).await {
        let env = Envelope::new(MSG_SHUTDOWN, &DaemonShutdown {}).map_err(err)?;
        let _ = server.send_to(&id, &env).await;
        // Small grace period for wrapper exit
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // 2. Kill the specific tmux session (backup in case wrapper is stuck).
    // The leading `=` forces exact match — without it, `alor-claude` would
    // prefix-match `alor-claude-alor` and kill the wrong session.
    let session = format!("=alor-{}", id);
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &session])
        .output();

    // 3. Clear internal state
    let _ = state.set_agent_connected(&id, false).map_err(err)?;

    pm.remove_agent_pane(&id).await.map_err(err)?;
    Ok(())
}

/// Permanently delete a template-spawned agent instance from state.
/// Kills any running process/session first, then tombstones the row.
/// Refuses to delete core yaml slots — kill those instead.
#[tauri::command]
pub async fn delete_agent(
    state: State<'_, AppState>,
    pm: State<'_, PaneManager>,
    server: State<'_, SocketServer>,
    id: String,
) -> Result<(), String> {
    // Guard: only template-derived instances are tombstone-safe.
    match state.get_agent(&id) {
        Some(agent) if agent.template.is_none() => {
            return Err(format!(
                "refusing to delete '{id}': not a template instance. Kill it instead."
            ));
        }
        None => return Err(format!("agent '{id}' not found")),
        _ => {}
    }

    // Graceful shutdown first (best-effort, same as kill_agent).
    if server.is_connected(&id).await {
        let env = Envelope::new(MSG_SHUTDOWN, &DaemonShutdown {}).map_err(err)?;
        let _ = server.send_to(&id, &env).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Kill tmux session (exact-match; see kill_agent for the prefix trap).
    let session = format!("=alor-{}", id);
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &session])
        .output();

    // Tombstone the row.
    state.remove_agent(&id);

    // Drop the pane from alor-main if we had one.
    let _ = pm.remove_agent_pane(&id).await;
    Ok(())
}

/// Typed-confirm token the caller must send to authorize
/// `kill_all_agents`. The UI collects it via `prompt("Type KILL to
/// confirm:")` and passes the result as the `confirm` parameter;
/// anything else is rejected. See the fn docstring for full
/// rationale.
pub const KILL_ALL_CONFIRM_TOKEN: &str = "KILL";

/// Worker-process name patterns `pkill -f` should target.
/// Enumerated as module-level data so tests can assert the list
/// hasn't silently drifted (SDK workers missed, script renames,
/// etc.) and so operators have one place to audit if a new runtime
/// gets added.
///
/// - `alor-wrapper`: the classic Rust wrapper binary.
/// - `run-worker.sh`: the shim script that launches a claude-sdk
///   worker via the vendored venv — `pkill -f` matches the command
///   line, which includes this path when tmux wraps it.
/// - `orchestrator-py/worker.py`: the actual Python worker entry
///   point visible to pkill via `/home/.../orchestrator-py/worker.py`
///   on the command line of the python interpreter spawned by
///   run-worker.sh.
///
/// Keep this list exhaustive. Pre-fix missing `worker.py` here
/// meant SDK workers survived `kill_all_agents` silently — which
/// was the claude-alor / cursor-alor audit concern. Both shapes are
/// now caught.
pub const WORKER_PROCESS_PATTERNS: &[&str] =
    &["alor-wrapper", "run-worker.sh", "orchestrator-py/worker.py"];

/// Graceful-shutdown wait AFTER `daemon.shutdown` is sent and BEFORE
/// the first SIGTERM sweep. Gives SDK workers enough time to flush
/// the outbox and close the socket cleanly — pre-fix 100ms was far
/// too short; workers routinely lost a final `task.complete` that
/// was still draining.
const KILL_SHUTDOWN_GRACE_MS: u64 = 1500;

/// Wait between the SIGTERM sweep and the SIGKILL fallback. Gives
/// any process that caught SIGTERM a chance to clean up (close file
/// descriptors, flush logs, etc.) before we escalate.
const KILL_SIGTERM_GRACE_MS: u64 = 500;

/// Validate the typed-confirm token for `kill_all_agents`. Returns
/// `Ok(())` iff `confirm` exactly matches `KILL_ALL_CONFIRM_TOKEN`;
/// otherwise a caller-facing error string explaining the rejection.
///
/// Factored out so unit tests can cover the gate semantics without
/// constructing Tauri `State<'_, _>` fixtures (which `kill_all_agents`
/// requires and which would need a managed-state scaffold to stand
/// up in a test harness).
pub(crate) fn validate_kill_confirm(confirm: &str) -> Result<(), String> {
    if confirm == KILL_ALL_CONFIRM_TOKEN {
        return Ok(());
    }
    Err(format!(
        "kill_all_agents refused: confirmation token must be exactly {:?} (got {:?}); aborting with no side effects",
        KILL_ALL_CONFIRM_TOKEN, confirm
    ))
}

/// Kill all running agents and their tmux sessions.
///
/// **Requires typed confirmation.** Caller must pass
/// `confirm = "KILL"` (see `KILL_ALL_CONFIRM_TOKEN`) or the command
/// is rejected with no side effects. The UI collects the token via
/// a typed prompt (`src/main.js` kill-all button); programmatic
/// callers MUST include the token too — this is a trip-wire for
/// accidental invocations, not a security boundary.
///
/// Kill order:
///   1. **Graceful**: `daemon.shutdown` envelope to every connected
///      agent. SDK workers + wrappers both honor this — worker.py
///      sets stop, flushes outbox, exits cleanly; alor-wrapper's
///      handler closes the SDK + exits.
///   2. **Wait** `KILL_SHUTDOWN_GRACE_MS` (1500ms). Pre-fix 100ms
///      was too short; workers mid-flush lost outbox frames.
///   3. **Tmux session teardown**: kill every `alor-*` session plus
///      `alor-main` specifically (covers the list-sessions-fail
///      edge).
///   4. **SIGTERM sweep**: `pkill -TERM -f <pattern>` for every
///      pattern in `WORKER_PROCESS_PATTERNS`. Catches survivors
///      that didn't respond to the graceful shutdown (hung SDK
///      turn, socket already closed, etc.).
///   5. **Wait** `KILL_SIGTERM_GRACE_MS` (500ms) — lets processes
///      that caught SIGTERM tidy up.
///   6. **SIGKILL fallback**: `pkill -KILL -f <pattern>` for the
///      same patterns. Unconditional escalation — processes that
///      survived SIGTERM get killed hard.
///   7. **State clear**: drop agent state, clear pane map, broadcast
///      `tasks-changed`.
///
/// Idempotent: calling twice is safe (second call finds nothing
/// connected, tmux kill-session on a dead target no-ops, pkill on
/// no-match no-ops).
#[tauri::command]
pub async fn kill_all_agents(
    state: State<'_, AppState>,
    pm: State<'_, PaneManager>,
    server: State<'_, SocketServer>,
    confirm: String,
) -> Result<(), String> {
    // Typed-confirm gate. Reject with no side effects — return a
    // clear error the UI can surface verbatim. Literal equality
    // only: no case-insensitive match, no trimming beyond what the
    // JS side does before sending.
    validate_kill_confirm(&confirm)?;

    // Phase 1: graceful shutdown notice to every connected agent.
    let agents = state.all_agents();
    for agent in &agents {
        if server.is_connected(&agent.id).await {
            let env = Envelope::new(MSG_SHUTDOWN, &DaemonShutdown {}).map_err(err)?;
            let _ = server.send_to(&agent.id, &env).await;
        }
    }
    // Phase 2: grace window for workers to flush and exit.
    tokio::time::sleep(std::time::Duration::from_millis(KILL_SHUTDOWN_GRACE_MS))
        .await;

    // Phase 3: tmux session teardown. Enumerate + kill every `alor-*`
    // plus the display session specifically.
    if let Ok(out) = std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
    {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for session in stdout.lines().filter(|s| s.starts_with("alor-")) {
                let _ = std::process::Command::new("tmux")
                    .args(["kill-session", "-t", &format!("={session}")])
                    .output();
            }
        }
    }
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", "alor-main"])
        .output();

    // Phase 4: SIGTERM sweep. Covers both tmux-managed wrappers and
    // bare SDK workers (workers launched via run-worker.sh / direct
    // python worker.py). Unlike the pre-fix single pkill -9 pass,
    // we give SIGTERM a chance first so processes that wanted to
    // clean up (flush logs, close FDs) can do so.
    for pattern in WORKER_PROCESS_PATTERNS {
        let _ = std::process::Command::new("pkill")
            .args(["-TERM", "-f", pattern])
            .output();
    }

    // Phase 5: short wait so SIGTERM-handled processes finish
    // cleanup before the kill escalation.
    tokio::time::sleep(std::time::Duration::from_millis(KILL_SIGTERM_GRACE_MS))
        .await;

    // Phase 6: SIGKILL fallback. Unconditional — survivors at this
    // point are hung (SIGTERM-ignoring or stuck in uninterruptible
    // I/O). pkill no-ops on no-match, so re-killing is safe.
    for pattern in WORKER_PROCESS_PATTERNS {
        let _ = std::process::Command::new("pkill")
            .args(["-KILL", "-f", pattern])
            .output();
    }

    // Phase 7: clear internal state.
    state.clear_all_agents();
    pm.clear_all_panes().await;
    state.emit_event("tasks-changed");

    Ok(())
}

// ---------------------------------------------------------------------------
// Session info
// ---------------------------------------------------------------------------

/// Return lightweight session metadata (started_at, pid, version).
/// The SessionInfo is created once at startup and stored in Tauri managed state.
#[tauri::command]
pub fn get_session_info(info: State<'_, SessionInfo>) -> Result<SessionInfo, String> {
    Ok(info.inner().clone())
}

// ---------------------------------------------------------------------------
// Terminal commands (PTY relay to alor-main)
// ---------------------------------------------------------------------------

/// Check if the PTY relay is running.
#[tauri::command]
pub async fn terminal_attach(
    bridge: State<'_, TerminalBridge>,
) -> Result<bool, String> {
    Ok(bridge.is_running().await)
}

/// Send keystrokes to the PTY (tmux routes to active pane).
#[tauri::command]
pub async fn terminal_send_keys(
    bridge: State<'_, TerminalBridge>,
    keys: String,
) -> Result<(), String> {
    bridge.send_keys(&keys).await.map_err(err)
}

/// Resize the PTY to match xterm.js dimensions.
#[tauri::command]
pub async fn terminal_resize(
    bridge: State<'_, TerminalBridge>,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    bridge.resize(cols, rows).await.map_err(err)
}

/// Read the X11 PRIMARY selection and inject it into the PTY.
///
/// Triggered by the frontend on middle-click. The webview doesn't fire a
/// browser paste event for middle-click on non-contenteditable elements, so
/// we synthesise the paste here: read PRIMARY via xclip (or wl-paste on
/// Wayland), then write it into the PTY exactly as if the user had typed it.
/// Empty selections are a silent no-op.
#[tauri::command]
pub async fn terminal_paste_primary(
    bridge: State<'_, TerminalBridge>,
) -> Result<(), String> {
    let text = read_primary_selection().await;
    if text.is_empty() {
        return Ok(());
    }
    bridge.send_keys(&text).await.map_err(err)
}

/// Push a string into the X11 PRIMARY selection.  Called from the frontend
/// whenever xterm.js selection changes so that middle-click paste (here and
/// in any other app on the desktop) pastes exactly what the user just
/// highlighted. Silent on failure — writing PRIMARY is best-effort.
#[tauri::command]
pub async fn terminal_set_primary(text: String) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    if text.is_empty() {
        return Ok(());
    }

    // Write to BOTH X11 PRIMARY and Wayland primary selection. On Sway
    // these are separate surfaces: XWayland apps read xclip's selection,
    // native Wayland apps read wl-copy's. Populate both so middle-click
    // works regardless of which kind of window the user is pasting into.
    async fn pipe(cmd: &str, args: &[&str], text: &str) {
        use tokio::process::Command as TokioCommand;
        if let Ok(mut child) = TokioCommand::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes()).await;
                drop(stdin);
                let _ = child.wait().await;
            }
        }
    }

    pipe("xclip", &["-i", "-selection", "primary"], &text).await;
    pipe("wl-copy", &["--primary"], &text).await;

    Ok(())
}

/// Read the PRIMARY selection from whichever of wl-paste or xclip has
/// content. Wayland first because Sway keeps X11 and Wayland primary
/// separate; a selection made in a native Wayland app only lives in
/// the Wayland side. Falls through to xclip for XWayland-sourced text.
/// Returns an empty string if both are empty or both fail.
async fn read_primary_selection() -> String {
    use tokio::process::Command;

    // wl-paste --primary --no-newline
    if let Ok(out) = Command::new("wl-paste")
        .args(["--primary", "--no-newline"])
        .output()
        .await
    {
        if out.status.success() && !out.stdout.is_empty() {
            return String::from_utf8_lossy(&out.stdout).into_owned();
        }
    }

    // xclip -selection primary -o
    if let Ok(out) = Command::new("xclip")
        .args(["-selection", "primary", "-o"])
        .output()
        .await
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).into_owned();
        }
    }

    String::new()
}

// ---------------------------------------------------------------------------
// Pane management commands
// ---------------------------------------------------------------------------

/// Show an agent's pane in alor-main.
#[tauri::command]
pub async fn pane_show(
    pm: State<'_, PaneManager>,
    agent_id: String,
) -> Result<(), String> {
    pm.add_agent_pane(&agent_id).await.map_err(err)
}

/// Hide an agent's pane from alor-main.
#[tauri::command]
pub async fn pane_hide(
    pm: State<'_, PaneManager>,
    agent_id: String,
) -> Result<(), String> {
    pm.remove_agent_pane(&agent_id).await.map_err(err)
}

/// Get list of agents with visible panes.
#[tauri::command]
pub async fn pane_list(
    pm: State<'_, PaneManager>,
) -> Result<Vec<String>, String> {
    Ok(pm.visible_agents().await)
}

/// Force a re-tile of alor-main panes right now. Useful when the layout has
/// drifted from a manual drag, an add/remove didn't fire recently, or the
/// user just wants things evened out.
#[tauri::command]
pub async fn pane_rebalance(pm: State<'_, PaneManager>) -> Result<(), String> {
    pm.rebalance().await;
    Ok(())
}

/// Safety-net reconciliation: ensures every connected agent has a pane
/// in alor-main, adding any that are missing. Idempotent; fast on the
/// happy path (just a map lookup per agent). Exposed so the frontend
/// can invoke it on init and after agents-changed events — the UI-side
/// equivalent of the startup sweep in lib.rs.
///
/// Also auto-clears zombies: agents marked `connected: true` whose
/// tmux session has disappeared get flipped to `connected: false`
/// with an `agent.disconnected` broadcast (reason:
/// `zombie_auto_cleared`). See SocketServer::mark_agent_zombie.
#[tauri::command]
pub async fn pane_reconcile(
    state: State<'_, AppState>,
    pm: State<'_, PaneManager>,
    server: State<'_, SocketServer>,
) -> Result<(), String> {
    let report = pm.reconcile_panes(&state).await;
    for zombie_id in &report.zombies {
        server.mark_agent_zombie(zombie_id).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Return the current AlorSettings. Falls through to defaults if the
/// settings file is missing or unreadable (load() logs internally).
#[tauri::command]
pub fn get_settings() -> Result<AlorSettings, String> {
    Ok(AlorSettings::load())
}

/// Persist the provided AlorSettings to ~/.config/alor/settings.yaml.
/// Takes effect on next Alor launch (no reactive-apply for v1).
#[tauri::command]
pub fn set_settings(settings: AlorSettings) -> Result<(), String> {
    settings.save().map_err(err)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- kill_all_agents typed-confirm gate ----

    #[test]
    fn kill_confirm_accepts_exact_token() {
        // The happy path. The gate is literal equality against
        // `KILL_ALL_CONFIRM_TOKEN` — no ambiguity.
        assert!(validate_kill_confirm(KILL_ALL_CONFIRM_TOKEN).is_ok());
        assert!(validate_kill_confirm("KILL").is_ok());
    }

    #[test]
    fn kill_confirm_rejects_empty_and_near_miss() {
        // Trip-wire cases the gate exists for: an empty string from
        // a Cancel-that-got-to-the-backend, a lowercased or
        // otherwise-cased near-miss from a user who typed fast.
        assert!(validate_kill_confirm("").is_err());
        assert!(validate_kill_confirm("kill").is_err());
        assert!(validate_kill_confirm("Kill").is_err());
        assert!(validate_kill_confirm("KILL ").is_err(), "trailing space");
        assert!(validate_kill_confirm(" KILL").is_err(), "leading space");
        assert!(validate_kill_confirm("KILL!").is_err());
        assert!(validate_kill_confirm("yes").is_err());
        assert!(validate_kill_confirm("y").is_err());
    }

    #[test]
    fn kill_confirm_error_message_is_actionable() {
        // UI surfaces the error verbatim; it should name the
        // required token and show what was actually passed so the
        // operator can correct.
        let err = validate_kill_confirm("oops").unwrap_err();
        assert!(err.contains("kill_all_agents"), "error names the command");
        assert!(err.contains("KILL"), "error names the required token");
        assert!(err.contains("oops"), "error shows the rejected input");
        assert!(
            err.contains("no side effects"),
            "error reassures the caller nothing was killed"
        );
    }

    // ---- WORKER_PROCESS_PATTERNS exhaustiveness ----

    #[test]
    fn worker_process_patterns_cover_all_runtimes() {
        // Locked-list guard: every worker runtime currently
        // supported must have a matching pkill pattern here.
        // Pre-fix the SDK worker (`run-worker.sh` + `worker.py`)
        // was missing, and `kill_all_agents` silently left SDK
        // workers running after clicking the UI button. If you add
        // a new runtime, add its process-line match here — or the
        // new runtime becomes orphan-immune to kill_all_agents.

        // alor-wrapper — the classic Rust wrapper binary.
        assert!(
            WORKER_PROCESS_PATTERNS.iter().any(|p| *p == "alor-wrapper"),
            "alor-wrapper pattern missing"
        );
        // run-worker.sh — the claude-sdk launch shim.
        assert!(
            WORKER_PROCESS_PATTERNS.iter().any(|p| *p == "run-worker.sh"),
            "run-worker.sh pattern missing"
        );
        // orchestrator-py/worker.py — the actual SDK worker entry
        // point (python interpreter process, matched by the path
        // component on its command line).
        assert!(
            WORKER_PROCESS_PATTERNS
                .iter()
                .any(|p| *p == "orchestrator-py/worker.py"),
            "orchestrator-py/worker.py pattern missing"
        );
        // At most 3 patterns for now — if this grows, update the
        // docstring on WORKER_PROCESS_PATTERNS.
        assert_eq!(
            WORKER_PROCESS_PATTERNS.len(),
            3,
            "pattern list grew unexpectedly; update docs + this guard"
        );
    }

    #[test]
    fn kill_all_confirm_token_is_stable() {
        // The UI + any external callers pin on this literal. Don't
        // drift it silently — if it changes, update the UI
        // prompt text + this assertion together.
        assert_eq!(KILL_ALL_CONFIRM_TOKEN, "KILL");
    }

    // ---------------------------------------------------------------
    // assign_task state-machine path (codex-alor audit fix).
    //
    // Tests the inner `assign_task_inner` helper so we don't need
    // Tauri `State<'_, _>` fixtures. Uses the SAME `SocketServer`
    // test shape the wrapper/server/tests.rs module uses:
    // `SocketServer::with_configs(...)` + `AppState::new()` +
    // `PaneManager::new()`.
    // ---------------------------------------------------------------

    use crate::daemon::state::{Agent, TaskState};
    use crate::terminal::pane_manager::PaneManager;
    use crate::wrapper::server::SocketServer;

    fn server_for_assign_test() -> (SocketServer, AppState) {
        let app_state = AppState::new();
        let server = SocketServer::with_configs(
            app_state.clone(),
            PaneManager::new(),
            vec![],
        );
        (server, app_state)
    }

    /// Inject a connected writer for `agent_id` by pushing a dummy
    /// `OwnedWriteHalf` into the server's `writers` map. Gives
    /// `server.is_connected(agent_id)` → true and `server.send_to`
    /// → success (bytes go into an in-memory socketpair and get
    /// discarded).
    async fn fake_connect(server: &SocketServer, agent_id: &str) {
        // socketpair gives us a real UnixStream pair. Stash one
        // half with the server as its writer; LEAK the peer so the
        // kernel buffer stays open — if we drop the peer, the next
        // write on our half fires EPIPE (Broken pipe, os error 32).
        // `std::mem::forget` is fine here: tests are short-lived
        // and this is all test-only code. Production never touches
        // `test_insert_writer`.
        let (a, b) = tokio::net::UnixStream::pair()
            .expect("UnixStream::pair for test");
        let (_read, write) = a.into_split();
        std::mem::forget(b);
        server.test_insert_writer(agent_id, write).await;
    }

    fn seed_agent(
        state: &AppState,
        agent_id: &str,
        max_concurrent: u8,
    ) {
        let mut agent = Agent::new(agent_id, agent_id);
        agent.max_concurrent = max_concurrent;
        state.register_agent(agent);
    }

    // ---- happy path ----

    #[tokio::test]
    async fn assign_task_transitions_pending_to_assigned_via_state_machine() {
        // The core regression test. Pre-fix:
        //   - task.add_task put it in Pending
        //   - assigned_to was set manually to the agent id
        //   - task.assign was wire-sent
        //   - NO transition to Assigned
        //   - worker's task.accept tried Pending→Accepted (illegal)
        //   - daemon logged "transition to Accepted failed" and
        //     task sat in Pending forever.
        //
        // Post-fix, assign_task_to_agent is called → state moves
        // Pending→Assigned → worker's task.accept then does the
        // legal Assigned→Accepted.
        let (server, state) = server_for_assign_test();
        seed_agent(&state, "claude-alor", 1);
        fake_connect(&server, "claude-alor").await;

        let task = assign_task_inner(
            &state,
            &server,
            "T".to_string(),
            "body".to_string(),
            Some("claude-alor".to_string()),
            None,
        )
        .await
        .expect("assign_task_inner ok");

        assert_eq!(
            task.state,
            TaskState::Assigned,
            "post-fix: task must be in Assigned state after dispatch (not Pending)"
        );
        assert_eq!(
            task.assigned_to.as_deref(),
            Some("claude-alor"),
            "assigned_to set by assign_task_to_agent, not by manual field mutation"
        );

        // And now the worker-side half of the handshake: a legal
        // Assigned→Accepted transition must succeed (the pre-fix
        // bug was that this transition was from Pending, which
        // failed). Directly exercise the state machine.
        state
            .transition_task(task.id, TaskState::Accepted)
            .expect("Assigned→Accepted is legal");
        let after = state.get_task(task.id).expect("present");
        assert_eq!(after.state, TaskState::Accepted);
    }

    #[tokio::test]
    async fn assign_task_without_agent_stays_pending() {
        // No agent specified → task lives in Pending until an
        // external dispatcher picks it up. No transition, no
        // assigned_to.
        let (server, state) = server_for_assign_test();

        let task = assign_task_inner(
            &state,
            &server,
            "orphan".to_string(),
            "body".to_string(),
            None,
            None,
        )
        .await
        .expect("ok");

        assert_eq!(task.state, TaskState::Pending);
        assert_eq!(task.assigned_to, None);
    }

    // ---- capacity enforcement ----

    #[tokio::test]
    async fn assign_task_rejects_over_capacity() {
        // Seed a max_concurrent=1 agent with one active Accepted
        // task. A second assign_task for the same agent MUST NOT
        // silently succeed — pre-fix it did (no capacity check at
        // all on the Tauri path, unlike cli.assign which has one).
        // Task stays Pending (recoverable) — operator can wait for
        // the slot to free and retry, or reassign elsewhere.
        let (server, state) = server_for_assign_test();
        seed_agent(&state, "claude-alor", 1);
        fake_connect(&server, "claude-alor").await;

        // Seed one active task to peg the slot.
        let first = assign_task_inner(
            &state,
            &server,
            "first".to_string(),
            "body".to_string(),
            Some("claude-alor".to_string()),
            None,
        )
        .await
        .expect("first assign ok");
        state
            .transition_task(first.id, TaskState::Accepted)
            .expect("Assigned→Accepted");
        assert_eq!(state.agent_active_task_count("claude-alor"), 1);

        // Second assign → must fail with a capacity error, NOT
        // silently succeed.
        let err = assign_task_inner(
            &state,
            &server,
            "second".to_string(),
            "body".to_string(),
            Some("claude-alor".to_string()),
            None,
        )
        .await
        .expect_err("capacity check must reject");
        assert!(
            err.contains("at capacity") || err.contains("capacity"),
            "capacity error message expected, got: {err}"
        );

        // The rejected second task was created (add_task ran) and
        // STAYS in Pending — not Stale, not transitioned. The
        // state-machine doesn't allow Pending→Stale, and rejecting
        // before any transition keeps the task recoverable.
        let all = state.all_tasks();
        let second = all
            .iter()
            .find(|t| t.title == "second")
            .expect("second task persisted");
        assert_eq!(
            second.state,
            TaskState::Pending,
            "over-capacity task must stay Pending (recoverable), not mutated"
        );
        assert_eq!(
            second.assigned_to, None,
            "over-capacity task must NOT have assigned_to set (gate rejected before assign_task_to_agent)",
        );
    }

    #[tokio::test]
    async fn assign_task_rejects_when_wrapper_not_connected() {
        // Agent exists in state but no writer → not connected.
        // Pre-fix: handler tried `state.transition_task(Stale)`
        // silently via `let _ = ...`, hiding the illegal-
        // transition error (Pending→Stale isn't legal). Post-fix:
        // returns Err, task stays Pending (recoverable — operator
        // can retry once wrapper reconnects).
        let (server, state) = server_for_assign_test();
        seed_agent(&state, "claude-alor", 1);
        // Deliberately NOT fake_connect — agent registered but
        // unconnected.

        let err = assign_task_inner(
            &state,
            &server,
            "unreachable".to_string(),
            "body".to_string(),
            Some("claude-alor".to_string()),
            None,
        )
        .await
        .expect_err("must error when wrapper not connected");
        assert!(
            err.contains("not connected"),
            "error names the connection state: {err}"
        );
        assert!(
            err.contains("Pending"),
            "error mentions the recoverable state: {err}"
        );

        // Task persists in Pending — recoverable.
        let all = state.all_tasks();
        let t = all
            .iter()
            .find(|t| t.title == "unreachable")
            .expect("task persisted despite error");
        assert_eq!(t.state, TaskState::Pending);
        assert_eq!(t.assigned_to, None);
    }

    // ---- error propagation (no more silent transition swallowing) ----

    #[tokio::test]
    async fn assign_task_over_capacity_surfaces_error_with_context() {
        // Sibling of the capacity test above — specifically asserts
        // the error message shape so operators + UI can display it
        // usefully. Pre-fix there was no error AT ALL here.
        let (server, state) = server_for_assign_test();
        seed_agent(&state, "claude-alor", 1);
        fake_connect(&server, "claude-alor").await;

        // First assign pegs the slot.
        let first = assign_task_inner(
            &state, &server, "a".to_string(), "b".to_string(),
            Some("claude-alor".to_string()), None,
        ).await.expect("ok");
        state.transition_task(first.id, TaskState::Accepted).unwrap();

        // Second must fail with:
        //   - the agent id,
        //   - the active count,
        //   - the max,
        //   - a suggestion.
        let err = assign_task_inner(
            &state, &server, "b".to_string(), "body".to_string(),
            Some("claude-alor".to_string()), None,
        ).await.unwrap_err();

        assert!(err.contains("claude-alor"), "err names the agent: {err}");
        assert!(err.contains("1/1"), "err shows active/max ratio: {err}");
        assert!(
            err.contains("sibling slot") || err.contains("wait"),
            "err suggests remediation: {err}",
        );
    }

    // ---- assigned_to NOT set until state transitions succeed ----

    #[tokio::test]
    async fn assign_task_does_not_preset_assigned_to_before_transition() {
        // Pre-fix the handler set `task.assigned_to = Some(aid)`
        // BEFORE `state.add_task`, creating a task that was in
        // Pending state WITH an assigned_to field populated. That
        // shape is a known zombie — assigned_to implies ownership,
        // Pending implies not-yet-assigned. This test pins that we
        // never create that shape.
        let (server, state) = server_for_assign_test();
        seed_agent(&state, "claude-alor", 1);
        // No fake_connect → the unconnected path returns Err
        // early; task persists in Pending with assigned_to None.
        let _err = assign_task_inner(
            &state, &server,
            "t".to_string(), "body".to_string(),
            Some("claude-alor".to_string()), None,
        ).await.expect_err("unconnected must Err");

        // Find the persisted task — it was added before the gate
        // rejected, so all_tasks() includes it.
        let all = state.all_tasks();
        let t = all.iter().find(|t| t.title == "t").expect("persisted");
        assert_eq!(
            t.assigned_to, None,
            "unconnected-path task must not preset assigned_to (pre-fix bug)"
        );
        assert_eq!(t.state, TaskState::Pending);
    }
}
