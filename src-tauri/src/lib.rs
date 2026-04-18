mod commands;
mod daemon;
mod tray;
mod wrapper;
mod terminal;

use tauri::{Emitter, Listener, Manager, WindowEvent};
use terminal::bridge::TerminalBridge;
use terminal::pane_manager::PaneManager;
use tracing_subscriber::{fmt, EnvFilter};
use wrapper::server::SocketServer;

pub fn run() {
    // Init tracing: ALOR_LOG=debug or default info
    fmt()
        .with_env_filter(
            EnvFilter::try_from_env("ALOR_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Ensure runtime directories exist before Tauri starts
    if let Err(e) = daemon::session::ensure_dirs() {
        eprintln!("alor: failed to create runtime dirs: {e}");
    }

    // Sway workspace placement must happen BEFORE Tauri creates the
    // main window — `assign` is a pre-map rule, so registering it here
    // means the wayland surface appears directly on the target
    // workspace with no flicker and no focus steal. Empty / unset
    // setting = falls through to default Tauri/Wayland behavior.
    let settings = daemon::settings::AlorSettings::load();
    if let Some(ref ws) = settings.spawn_workspace {
        if !ws.trim().is_empty() {
            apply_sway_workspace_placement(ws.trim());
        }
    }

    let app_state = match daemon::session::data_dir() {
        Ok(dir) => daemon::state::AppState::with_persistence(dir.join("state.json")),
        Err(e) => {
            tracing::warn!("no data dir, state will not persist: {e}");
            daemon::state::AppState::new()
        }
    };
    // Sweep terminal tasks (Completed/Cancelled/Rejected/TimedOut/Stale) out
    // of live state into tasks-archive.json on every startup.  Idempotent.
    match daemon::session::tasks_archive_file() {
        Ok(archive_path) => {
            let archived = app_state.archive_terminal_tasks(&archive_path);
            if archived > 0 {
                tracing::info!(
                    archived,
                    path = %archive_path.display(),
                    "swept terminal tasks into archive"
                );
            } else {
                tracing::debug!("no terminal tasks to archive");
            }
        }
        Err(e) => {
            tracing::warn!("could not resolve archive path: {e}");
        }
    }
    // Load agent configs from ~/.config/alor/agents/*.yaml and register them.
    let agent_configs = match daemon::config::load_agent_configs() {
        Ok(configs) => {
            daemon::config::register_agents_from_config(&app_state, &configs);
            configs
        }
        Err(e) => {
            tracing::warn!("failed to load agent configs: {e:#}");
            vec![]
        }
    };

    let pane_manager = PaneManager::new();
    let socket_server = SocketServer::with_configs(app_state.clone(), pane_manager.clone(), agent_configs.clone());
    let terminal_bridge = TerminalBridge::new();
    let terminal_bridge_clone = terminal_bridge.clone();
    let session_info = daemon::session::SessionInfo::current();
    if let Err(e) = session_info.write() {
        tracing::warn!("failed to write session file: {e}");
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(app_state.clone())
        .manage(socket_server.clone())
        .manage(terminal_bridge)
        .manage(session_info)
        .manage(pane_manager.clone())
        .on_window_event(|window, event| {
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    // Minimize to tray instead of quitting.
                    api.prevent_close();
                    let _ = window.hide();
                }
                WindowEvent::Focused(true) => {
                    // Restore default tray icon when window regains focus.
                    tray::set_default_icon(window.app_handle());
                }
                _ => {}
            }
        })
        .setup(move |app| {
            // Wire up app handle so AppState can emit push events.
            app_state.set_app_handle(app.handle().clone());

            // Set up system tray.
            if let Err(e) = tray::setup(app.handle()) {
                tracing::error!("failed to set up system tray: {e}");
            }

            // Refresh tray menu when agents or tasks change.
            let handle = app.handle().clone();
            app.listen("tasks-changed", move |_| {
                tray::refresh_menu(&handle);
            });
            let handle = app.handle().clone();
            app.listen("agents-changed", move |_| {
                tray::refresh_menu(&handle);
            });

            // Flash the tray icon and send desktop notification when a task completes.
            let handle = app.handle().clone();
            app.listen("task-completed", move |event| {
                tray::set_task_complete_icon(&handle);

                let title = serde_json::from_str::<String>(event.payload())
                    .unwrap_or_default();
                notify_task_completed(&title);
            });

            // Create alor-main tmux session.
            let pm_main = pane_manager.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = pm_main.ensure_main_session().await {
                    tracing::error!("failed to create alor-main: {e:#}");
                }
            });

            // Spawn the socket server in the background
            let server = socket_server.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = server.run().await {
                    tracing::error!("socket server error: {e:#}");
                }
            });

            // Auto-launch wrappers for agents with autolaunch: true OR existing sessions.
            let configs_for_launch = agent_configs;
            let pm_recovery = pane_manager.clone();
            let app_state_recovery = app_state.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));

                // Kill stale wrapper processes from previous daemon runs.
                // The saved PID file at this point may include both
                // yaml-slot wrappers AND template-instance wrappers from
                // the previous boot (see second-pass save below).
                daemon::session::kill_stale_wrappers();
                std::thread::sleep(std::time::Duration::from_millis(200));

                // ---- Pass 1: yaml-registered fixed slots ----
                //
                // Identify agents that have existing tmux sessions.
                // Templates are recipes, not slots — never reclaim them
                // even if `session_exists` were to misfire.  SDK-runtime
                // workers also self-heal: their Python process reconnects
                // to the new daemon socket on its own. Launching an
                // alor-wrapper for them causes a duplicate registration
                // collision that the daemon rejects.
                let mut yaml_to_launch = Vec::new();
                for (id, cfg) in &configs_for_launch {
                    if cfg.template {
                        continue;
                    }
                    if cfg.runtime == "claude-sdk" {
                        tracing::debug!(
                            agent_id = %id,
                            "skipping boot reclaim for claude-sdk runtime; worker self-reconnects"
                        );
                        continue;
                    }
                    let session_name = format!("alor-{}", id);
                    let exists = tauri::async_runtime::block_on(pm_recovery.session_exists(&session_name));

                    if cfg.autolaunch || exists {
                        if exists {
                            tracing::info!(agent_id = %id, "reclaiming existing tmux session");
                        }
                        yaml_to_launch.push((id.clone(), cfg.clone()));
                    }
                }

                let mut children = daemon::config::launch_wrappers(&yaml_to_launch);

                // ---- Pass 2: wrapper-runtime template-spawned instances ----
                //
                // Template instances (e.g. `codex-alor`, `gemini-mandaforge`)
                // live only in state.json with a `template` back-reference
                // — they have no yaml of their own. The yaml loop above
                // skips them. Daemon-spawned Child processes die on restart;
                // tmux sessions survive (tmux is its own daemon). This pass
                // relaunches the alor-wrapper so it reconnects to the
                // orphaned tmux session and re-registers with the daemon.
                //
                // claude-sdk template instances are skipped for the same
                // reason as in pass 1: the python worker self-reconnects
                // via its own socket path.
                //
                // Reclaim policy: if the tmux session is gone, we treat
                // that as user-intentional (explicit `tmux kill-session`
                // or `kill_agent`) and do NOT resurrect — matches the
                // fixed-slot behavior where a killed session stays dead
                // until explicitly ensured-running.
                //
                // TEST_PLAN for post-restart verification:
                //   Pre-restart:
                //     agent_spawn(agent="codex", project="alor",
                //                 working_dir="~/Projects/Alor")
                //     - confirm state.json has codex-alor with
                //       template=Some("codex")
                //     - confirm wrapper is running, codex-alor shows
                //       connected=true in agent_list
                //   Restart Alor.
                //   Post-restart:
                //     - confirm wrapper for codex-alor auto-spawns
                //       (no manual agent_ensure_running)
                //     - confirm agent_list still shows codex-alor
                //       connected=true within ~5s of restart
                //     - bonus: kill wrapper PID while keeping tmux
                //       session alive, restart again, verify next
                //       startup reclaims it again
                let mut tmpl_to_launch = Vec::new();
                for agent in app_state_recovery.all_agents() {
                    let template_id = match agent.template.as_deref() {
                        Some(t) => t,
                        None => continue, // fixed-slot descendant; pass 1 handled it
                    };
                    let tmpl_cfg = match configs_for_launch.iter().find(|(id, _)| id == template_id) {
                        Some((_, cfg)) => cfg,
                        None => {
                            tracing::warn!(
                                agent_id = %agent.id,
                                template = %template_id,
                                "template-backed instance references a missing template; skipping reclaim"
                            );
                            continue;
                        }
                    };
                    if tmpl_cfg.runtime == "claude-sdk" {
                        tracing::debug!(
                            agent_id = %agent.id,
                            "skipping reclaim for claude-sdk template instance"
                        );
                        continue;
                    }
                    let session_name = format!("alor-{}", agent.id);
                    let exists = tauri::async_runtime::block_on(pm_recovery.session_exists(&session_name));
                    if !exists {
                        // User killed the session; don't resurrect.
                        continue;
                    }
                    // Clone the template config and overlay the instance's
                    // persisted working_dir so the wrapper launches with
                    // the right cwd — the instance was parameterized
                    // specifically for that project.
                    let mut effective = tmpl_cfg.clone();
                    if let Some(wd) = agent.working_dir.clone() {
                        effective.working_dir = Some(wd);
                    }
                    tracing::info!(
                        agent_id = %agent.id,
                        template = %template_id,
                        "reclaiming wrapper-runtime template instance"
                    );
                    tmpl_to_launch.push((agent.id.clone(), effective));
                }
                children.extend(daemon::config::launch_wrappers(&tmpl_to_launch));

                // Record all child PIDs (yaml + template-instance) so
                // next daemon startup can kill them.
                let pids: Vec<u32> = children.iter().map(|(_, c)| c.id()).collect();
                if let Err(e) = daemon::session::save_wrapper_pids(&pids) {
                    tracing::warn!("failed to save wrapper PIDs: {e}");
                }

                for (_id, ref mut child) in &mut children {
                    let _ = child.wait();
                }
            });

            // Start PTY relay for alor-main.
            // The relay spawns `tmux attach` in a real PTY and reads its output.
            // We need a short delay to ensure alor-main exists first.
            let handle = app.handle().clone();
            let bridge = terminal_bridge_clone;
            tauri::async_runtime::spawn(async move {
                // Wait for alor-main to be created.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                // Start the PTY relay (blocking call to set up PTY).
                let reader = match tokio::task::spawn_blocking({
                    let bridge = bridge.clone();
                    move || bridge.start_relay()
                })
                .await
                {
                    Ok(Ok(reader)) => reader,
                    Ok(Err(e)) => {
                        tracing::error!("PTY relay failed to start: {e:#}");
                        return;
                    }
                    Err(e) => {
                        tracing::error!("PTY relay task panicked: {e}");
                        return;
                    }
                };

                // Read PTY output in a blocking thread and emit to frontend.
                // The first ~300ms of output is tmux negotiation noise — drain
                // it, then force a clean redraw and start forwarding.
                tokio::task::spawn_blocking(move || {
                    use std::io::Read as _;
                    let mut reader = reader;
                    let mut buf = [0u8; 4096];

                    // Phase 1: drain initial negotiation noise.
                    let drain_until = std::time::Instant::now()
                        + std::time::Duration::from_millis(500);
                    while std::time::Instant::now() < drain_until {
                        match reader.read(&mut buf) {
                            Ok(0) => {
                                tracing::info!("PTY relay EOF during drain");
                                return;
                            }
                            Ok(n) => {
                                tracing::debug!("drained {n} bytes of PTY negotiation");
                            }
                            Err(e) => {
                                tracing::warn!("PTY read error during drain: {e}");
                                return;
                            }
                        }
                    }

                    // Force tmux to redraw cleanly after drain.
                    let _ = std::process::Command::new("tmux")
                        .args(["refresh-client", "-t", "alor-main"])
                        .output();

                    // Phase 2: forward PTY output to frontend.
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) => {
                                tracing::info!("PTY relay EOF");
                                break;
                            }
                            Ok(n) => {
                                let data = String::from_utf8_lossy(&buf[..n]).to_string();
                                let chunk = terminal::bridge::TerminalChunk { data };
                                if let Err(e) = handle.emit("terminal-output", &chunk) {
                                    tracing::warn!("emit failed: {e}");
                                }
                            }
                            Err(e) => {
                                tracing::warn!("PTY read error: {e}");
                                break;
                            }
                        }
                    }
                })
                .await
                .ok();
            });

            // Force a UI sync on startup so existing agents are visible immediately.
            let app_state_sync = app_state.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                app_state_sync.emit_event("agents-changed");
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_tasks,
            commands::get_task,
            commands::assign_task,
            commands::cancel_task,
            commands::approve_task,
            commands::spawn_agent,
            commands::kill_agent,
            commands::delete_agent,
            commands::kill_all_agents,
            commands::get_agents,
            commands::register_agent,
            commands::get_session_info,
            commands::terminal_attach,
            commands::terminal_send_keys,
            commands::terminal_resize,
            commands::terminal_paste_primary,
            commands::terminal_set_primary,
            commands::pane_show,
            commands::pane_hide,
            commands::pane_list,
            commands::pane_rebalance,
            commands::get_settings,
            commands::set_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error running alor");
}

/// Register a Sway `assign` rule that places the Alor main window on
/// the configured workspace. Called at startup before Tauri creates
/// any windows.
///
/// `assign` fires pre-map, so the wayland surface appears directly on
/// the target workspace — no flicker (unlike `for_window ... move`
/// which fires post-map), and Sway doesn't switch focus to that
/// workspace if it isn't already current.
///
/// Non-fatal on every failure path: if swaymsg isn't in PATH, isn't on
/// Sway, or rejects the criteria, we log and continue. Startup must
/// never block on compositor availability.
///
/// Users who prefer pure-Sway-config can skip this setting entirely and
/// add one of the following to their sway config instead:
///
/// ```text
/// assign [app_id="alor"] workspace 5
/// # or, for post-map movement + no focus steal:
/// for_window [app_id="alor"] move container to workspace 5
/// no_focus [app_id="alor"]
/// ```
///
/// Both are equivalent / better than what this function installs — the
/// function exists so Alor ships with a built-in path that doesn't
/// require editing the compositor's config.
fn apply_sway_workspace_placement(workspace: &str) {
    // Reject shell-meta and criteria-breaking characters. Sway
    // workspace names in practice are numeric or simple identifiers;
    // restricting to this set keeps the swaymsg payload safe from
    // injection via a poisoned settings.yaml.
    if workspace.is_empty()
        || !workspace
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':'))
    {
        tracing::warn!(
            workspace,
            "spawn_workspace has unsafe characters; skipping assign rule"
        );
        return;
    }
    let cmd = format!(r#"assign [app_id="alor"] workspace {workspace}"#);
    match std::process::Command::new("swaymsg").arg(&cmd).output() {
        Ok(out) if out.status.success() => {
            tracing::info!(workspace, "registered sway assign rule for alor");
        }
        Ok(out) => {
            tracing::warn!(
                exit = ?out.status.code(),
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "swaymsg assign rule registration failed"
            );
        }
        Err(e) => {
            // swaymsg missing, not on Sway, etc. Debug-level: common
            // and benign on non-Sway sessions.
            tracing::debug!("swaymsg unavailable, skipping placement: {e}");
        }
    }
}

/// Send a desktop notification via notify-send.
fn notify_task_completed(title: &str) {
    let body = if title.is_empty() {
        "A task has completed.".to_string()
    } else {
        format!("Completed: {title}")
    };

    std::thread::spawn(move || {
        let _ = std::process::Command::new("notify-send")
            .args(["--app-name=Alor", "Alor", &body])
            .output();
    });
}
