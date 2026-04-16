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

    let app_state = match daemon::session::data_dir() {
        Ok(dir) => daemon::state::AppState::with_persistence(dir.join("state.json")),
        Err(e) => {
            tracing::warn!("no data dir, state will not persist: {e}");
            daemon::state::AppState::new()
        }
    };
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
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));

                // Kill stale wrapper processes from previous daemon runs
                daemon::session::kill_stale_wrappers();
                std::thread::sleep(std::time::Duration::from_millis(200));

                // Recovery: identify agents that have existing tmux sessions
                let mut to_launch = Vec::new();
                for (id, cfg) in configs_for_launch {
                    let session_name = format!("alor-{}", id);
                    let exists = tauri::async_runtime::block_on(pm_recovery.session_exists(&session_name));
                    
                    if cfg.autolaunch || exists {
                        if exists {
                            tracing::info!(agent_id = %id, "reclaiming existing tmux session");
                        }
                        to_launch.push((id, cfg));
                    }
                }

                let mut children = daemon::config::launch_wrappers(&to_launch);

                // Record child PIDs so next daemon startup can kill them.
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
            commands::kill_all_agents,
            commands::get_agents,
            commands::register_agent,
            commands::get_session_info,
            commands::terminal_attach,
            commands::terminal_send_keys,
            commands::terminal_resize,
            commands::pane_show,
            commands::pane_hide,
            commands::pane_list,
        ])
        .run(tauri::generate_context!())
        .expect("error running alor");
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
