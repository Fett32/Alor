/// System tray integration for Alor.
///
/// Provides a tray icon with:
///   - Left-click to toggle window visibility
///   - Right-click context menu with status, show/hide, and quit
///   - Icon change on task completion (feeds into future notifications)

use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};

use crate::daemon::state::AppState;

const TRAY_ID: &str = "alor-tray";

/// Build and register the system tray. Call once during app setup.
pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app)?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(load_icon()?)
        .tooltip("Alor")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            match event.id().as_ref() {
                "show_hide" => toggle_window(app),
                "quit" => app.exit(0),
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

/// Rebuild the context menu with live status, then apply it to the tray.
pub fn refresh_menu(app: &AppHandle) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        match build_menu(app) {
            Ok(menu) => {
                if let Err(e) = tray.set_menu(Some(menu)) {
                    tracing::warn!("failed to update tray menu: {e}");
                }
            }
            Err(e) => tracing::warn!("failed to build tray menu: {e}"),
        }
    }
}

/// Swap the tray icon to signal a task completed.
pub fn set_task_complete_icon(app: &AppHandle) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(icon) = load_alert_icon() {
            let _ = tray.set_icon(Some(icon));
        }
    }
}

/// Restore the default tray icon (e.g. after the user acknowledges completion).
pub fn set_default_icon(app: &AppHandle) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(icon) = load_icon() {
            let _ = tray.set_icon(Some(icon));
        }
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let status_text = status_summary(app);
    let status_item = MenuItem::new(app, &status_text, false, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let show_hide = MenuItem::with_id(app, "show_hide", "Show / Hide", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Alor", true, None::<&str>)?;

    let separator2 = PredefinedMenuItem::separator(app)?;

    let menu = Menu::new(app)?;
    menu.append(&show_hide)?;
    menu.append(&separator)?;
    menu.append(&status_item)?;
    menu.append(&separator2)?;
    menu.append(&quit)?;
    Ok(menu)
}

fn status_summary(app: &AppHandle) -> String {
    let state = app.state::<AppState>();
    let agents = state.all_agents();
    let tasks = state.all_tasks();

    let connected = agents.iter().filter(|a| a.connected).count();
    let running = tasks
        .iter()
        .filter(|t| {
            matches!(
                t.state,
                crate::daemon::state::TaskState::Accepted
            )
        })
        .count();

    format!(
        "Agents: {connected}/{total} connected  |  Tasks running: {running}",
        total = agents.len()
    )
}

fn toggle_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }
}

fn load_icon() -> tauri::Result<Image<'static>> {
    let bytes = include_bytes!("../icons/32x32.png");
    Image::from_bytes(bytes)
}

fn load_alert_icon() -> tauri::Result<Image<'static>> {
    // Use the larger icon as a visual distinction for task-complete state.
    // A dedicated alert icon can replace this later.
    let bytes = include_bytes!("../icons/128x128.png");
    Image::from_bytes(bytes)
}
