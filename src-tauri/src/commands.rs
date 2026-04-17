/// Tauri IPC command handlers.
///
/// These are the functions the frontend calls via `invoke(...)`.
/// All commands receive the shared AppState via Tauri's managed state.

use crate::daemon::session::SessionInfo;
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
/// If agent_id is provided and the wrapper is connected, the task is sent
/// to the wrapper immediately. If dispatch fails, task transitions to Stale.
#[tauri::command]
pub async fn assign_task(
    state: State<'_, AppState>,
    server: State<'_, SocketServer>,
    title: String,
    description: String,
    agent_id: Option<String>,
    project: Option<String>,
) -> Result<Task, String> {
    let mut task = Task::new(title.clone(), description.clone());
    task.project = project.clone();

    if let Some(ref aid) = agent_id {
        task.assigned_to = Some(aid.clone());
    }

    let task_id = task.id;
    state.add_task(task.clone());

    tracing::info!(
        task_id = %task_id,
        agent = ?agent_id,
        project = ?project,
        "task created"
    );

    // Send to wrapper if agent is specified
    if let Some(ref aid) = agent_id {
        if !server.is_connected(aid).await {
            // Agent not connected - mark task as Stale immediately
            tracing::warn!(agent_id = aid, "wrapper not connected, marking task Stale");
            let _ = state.transition_task(task_id, TaskState::Stale);
            return state.get_task(task_id).ok_or_else(|| "task not found".to_string());
        }

        let dispatch_description = match project.as_deref() {
            Some(name) => match crate::daemon::project::load_profile(name) {
                Ok(Some(profile)) => {
                    crate::daemon::project::build_task_brief(&profile, &title, &description)
                }
                _ => description.clone(),
            },
            None => description.clone(),
        };

        let payload = TaskAssign {
            task_id,
            title,
            description: dispatch_description,
            timeout_secs: None,
        };

        let envelope = Envelope::new(MSG_TASK_ASSIGN, &payload).map_err(err)?;

        if let Err(e) = server.send_to(aid, &envelope).await {
            // Dispatch failed - mark task as Stale
            tracing::error!(agent_id = aid, "dispatch failed: {e}, marking task Stale");
            let _ = state.transition_task(task_id, TaskState::Stale);
            return state.get_task(task_id).ok_or_else(|| "task not found".to_string());
        }

        tracing::info!(task_id = %task_id, agent_id = aid, "task dispatched to wrapper");
    }

    state.get_task(task_id).ok_or_else(|| "task not found".to_string())
}

/// Cancel a task by UUID string.
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

/// Spawn a new agent process based on its config kind.
#[tauri::command]
pub async fn spawn_agent(
    state: State<'_, AppState>,
    agent: String,
    role: Option<String>,
) -> Result<(), String> {
    use crate::daemon::config;
    
    // Load config for just this kind
    let configs = config::load_agent_configs().map_err(err)?;
    let mut target_cfg = configs.into_iter()
        .find(|(id, _)| id == &agent)
        .ok_or_else(|| format!("agent config {agent} not found"))?;

    if let Some(r) = role {
        target_cfg.1.role = r;
    }

    // Launch it
    tracing::info!(agent_id = %target_cfg.0, "manually spawning agent");
    crate::daemon::config::force_launch_wrapper(target_cfg.0, target_cfg.1);
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

/// Kill all running agents and their tmux sessions.
#[tauri::command]
pub async fn kill_all_agents(
    state: State<'_, AppState>,
    pm: State<'_, PaneManager>,
    server: State<'_, SocketServer>,
) -> Result<(), String> {
    // 1. Tell all wrappers to shut down
    let agents = state.all_agents();
    for agent in &agents {
        if server.is_connected(&agent.id).await {
            let env = Envelope::new(MSG_SHUTDOWN, &DaemonShutdown {}).map_err(err)?;
            let _ = server.send_to(&agent.id, &env).await;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 2. Kill all tmux sessions managed by Alor (alor-* prefix).
    // Enumerate natively instead of shelling out through bash -c.
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

    // 3. Kill the main display session specifically (covers the case where
    //    list-sessions failed or alor-main wasn't caught above).
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", "alor-main"])
        .output();

    // 4. Kill all worker processes (backup). Covers both the tmux wrapper
    //    path (alor-wrapper) and the SDK Python workers launched via
    //    run-worker.sh / worker.py. The tmux kill above usually handles
    //    these, but pkill is belt-and-suspenders for stuck processes.
    for pattern in ["alor-wrapper", "run-worker.sh", "orchestrator-py/worker.py"] {
        let _ = std::process::Command::new("pkill")
            .args(["-9", "-f", pattern])
            .output();
    }

    // 5. Clear internal state
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
