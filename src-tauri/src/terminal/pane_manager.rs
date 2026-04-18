/// Pane manager — controls the alor-main tmux session layout.
///
/// Architecture:
///   Each agent has its own durable tmux session (alor-<agent>).
///   alor-main is a display session whose panes attach to agent sessions.
///   This module manages pane creation, removal, and layout in alor-main.
///
///   Pane command: `TMUX='' tmux new-session -A -t alor-<agent>`
///   This attaches to the agent session inside a pane, keeping it durable.
///   If alor-main dies, agent sessions survive. If the agent session dies,
///   the pane exits and can be respawned.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;

const MAIN_SESSION: &str = "alor-main";
/// Maximum right-side columns before we start stacking more aggressively.
const MAX_RIGHT_COLUMNS: usize = 3;

/// Shell snippet piped from tmux `copy-pipe-no-clear` on drag-select.
/// Syncs the selection to BOTH X11 (xclip) and Wayland (wl-copy) surfaces
/// because Sway keeps their primary/clipboard buffers separate — a
/// selection written only to one is invisible to apps on the other side
/// (e.g. native-Wayland terminals vs XWayland browsers). We write all
/// four: X11 clipboard, X11 primary, Wayland clipboard, Wayland primary.
/// Errors are suppressed — this is best-effort and should never fail the
/// tmux copy pipeline.
pub(crate) const COPY_PIPE_CMD: &str = r#"T=$(cat); \
command -v xclip  >/dev/null 2>&1 && printf '%s' "$T" | xclip -i -selection clipboard >/dev/null 2>&1; \
command -v xclip  >/dev/null 2>&1 && printf '%s' "$T" | xclip -i -selection primary   >/dev/null 2>&1; \
command -v wl-copy>/dev/null 2>&1 && printf '%s' "$T" | wl-copy                       >/dev/null 2>&1; \
command -v wl-copy>/dev/null 2>&1 && printf '%s' "$T" | wl-copy --primary             >/dev/null 2>&1; \
true"#;

// ---------------------------------------------------------------------------
// Pane tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PaneInfo {
    /// tmux pane ID (e.g. "%5")
    pane_id: String,
    /// Which right-side column this pane belongs to (0-based). Orchestrator has None.
    column: Option<usize>,
}

// ---------------------------------------------------------------------------
// PaneManager
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct PaneManager {
    /// Maps agent_id to pane info
    panes: Arc<Mutex<HashMap<String, PaneInfo>>>,
}

impl PaneManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Public wrapper around the internal `rebalance_layout` so the UI can
    /// force a reflow on demand (e.g. after a manual drag leaves things
    /// proportioned wrong, or when add/remove hasn't fired recently).
    pub async fn rebalance(&self) {
        self.rebalance_layout().await;
    }

    /// Propagate the display-related env vars from the Rust process into
    /// tmux's global environment.  Without this, tmux sessions inherit
    /// only whatever the tmux server was launched with — commonly DISPLAY
    /// but not WAYLAND_DISPLAY, which makes the copy-pipe's `wl-copy`
    /// silently fail when it tries to talk to a Wayland socket it can't
    /// find.  Called on startup so every session created after this has
    /// the right env.
    async fn propagate_display_env(&self) {
        // Collect (name, value) pairs once so we reuse them across every
        // existing alor-* session below.
        let vars: Vec<(&str, String)> = ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "DISPLAY"]
            .iter()
            .filter_map(|v| std::env::var(v).ok().map(|val| (*v, val)))
            .collect();
        if vars.is_empty() {
            return;
        }

        // Global: inherited by every new-session.
        for (k, v) in &vars {
            let _ = Command::new("tmux")
                .args(["set-environment", "-g", k, v])
                .output()
                .await;
        }

        // Existing sessions (reclaimed from a previous boot) need an
        // explicit push — they're frozen with whatever env they had at
        // creation. Iterate and overwrite for every alor-* session.
        let sessions = Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()
            .await;
        if let Ok(out) = sessions {
            if out.status.success() {
                let names = String::from_utf8_lossy(&out.stdout).into_owned();
                for name in names.lines().filter(|n| n.starts_with("alor-")) {
                    let target = format!("={name}");
                    for (k, v) in &vars {
                        let _ = Command::new("tmux")
                            .args(["set-environment", "-t", &target, k, v])
                            .output()
                            .await;
                    }
                }
            }
        }
    }

    /// Push the tmux options that govern mouse to selection/paste behaviour.
    /// Called on session create and also when reusing a session — the options
    /// are scoped to the session so they don't leak into the user's own tmux.
    async fn apply_tmux_mouse_config(&self) {
        // Make sure the copy-pipe shell can actually reach xclip/wl-copy.
        self.propagate_display_env().await;

        // mouse on: scroll, click-to-focus, drag borders to resize, drag body
        // to select (enters copy-mode).
        let _ = Command::new("tmux")
            .args(["set-option", "-t", MAIN_SESSION, "mouse", "on"])
            .output()
            .await;

        // Larger scrollback than default 2000.
        let _ = Command::new("tmux")
            .args(["set-option", "-t", MAIN_SESSION, "history-limit", "50000"])
            .output()
            .await;

        // Drag-end in copy-mode: pipe selection to xclip (clipboard + primary)
        // and DO NOT exit copy-mode. `copy-pipe-no-clear` preserves the visible
        // highlight so the user can see exactly what was copied. A single
        // click (rebound below) dismisses it. Press Escape / q / Enter too.
        for table in &["copy-mode", "copy-mode-vi"] {
            let _ = Command::new("tmux")
                .args([
                    "bind-key", "-T", table,
                    "MouseDragEnd1Pane",
                    "send-keys", "-X", "copy-pipe-no-clear", COPY_PIPE_CMD,
                ])
                .output()
                .await;

            // Single click (tap, no drag) inside copy-mode → cancel
            // on MOUSE-UP. Exits copy-mode, user can type again.
            //
            // Rebinding MouseUp1Pane (and NOT MouseDown1Pane) is what
            // makes drag-select work in a scrolled-up pane. tmux
            // distinguishes tap from drag natively: a completed tap
            // fires `MouseUp1Pane`, a drag fires `MouseDragEnd1Pane`
            // (caught above) and never fires MouseUp. A prior version
            // of this code bound `cancel` to MouseDown, which snapped
            // the viewport to the bottom the instant the user pressed
            // the button, killing drag-select before it began.
            //
            // `unbind-key -T <table> MouseDown1Pane` clears the prior
            // binding on tmux servers that predate this change — a
            // fresh Alor start against a long-running tmux would
            // otherwise inherit the old behavior.
            let _ = Command::new("tmux")
                .args([
                    "unbind-key", "-T", table,
                    "MouseDown1Pane",
                ])
                .output()
                .await;
            let _ = Command::new("tmux")
                .args([
                    "bind-key", "-T", table,
                    "MouseUp1Pane",
                    "send-keys", "-X", "cancel",
                ])
                .output()
                .await;
        }

        // mode-style: soft purple highlight so selection is clearly visible
        // against the dark background. Matches the UI accent.
        let _ = Command::new("tmux")
            .args([
                "set-option", "-t", MAIN_SESSION,
                "mode-style", "bg=#3d3580,fg=#ffffff",
            ])
            .output()
            .await;

        // NOTE: middle-click paste is handled in the frontend (it intercepts
        // the button-1 mousedown before xterm.js forwards it to tmux, reads
        // X11 PRIMARY, and writes back to the PTY). This keeps the paste
        // source under our control and works identically on X11 and Wayland.
    }

    /// Ensure alor-main exists. Creates it if needed.
    /// Called once at startup.
    pub async fn ensure_main_session(&self) -> Result<()> {
        if self.session_exists(MAIN_SESSION).await {
            tracing::info!("reusing existing {MAIN_SESSION} session");
            // Ensure session options/bindings are set (may have been lost if
            // the session predates the current mouse config).
            self.apply_tmux_mouse_config().await;
            self.scan_existing_panes().await;
            return Ok(());
        }

        // Create with a placeholder — first real agent pane will replace it.
        let output = Command::new("tmux")
            .args([
                "new-session", "-d", "-s", MAIN_SESSION,
                "-x", "200", "-y", "50",
            ])
            .output()
            .await
            .context("create alor-main session")?;

        if !output.status.success() {
            anyhow::bail!(
                "tmux new-session failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Mouse, scrollback, selection/clipboard bindings, highlight colour.
        self.apply_tmux_mouse_config().await;

        // Put a status message in the initial pane.
        let _ = Command::new("tmux")
            .args([
                "send-keys", "-t", MAIN_SESSION,
                "echo 'Alor — waiting for agents...'", "Enter",
            ])
            .output()
            .await;

        tracing::info!("created {MAIN_SESSION} session");
        Ok(())
    }

    /// Returns true if tmux knows about a pane with this id.
    /// Used to detect stale entries in the `panes` map (see
    /// `add_agent_pane` idempotency check).
    ///
    /// `tmux display-message -t %id` is NOT usable for this —
    /// invalid targets silently fall back to the current pane and
    /// return success. `tmux list-panes -t %id` correctly exits
    /// non-zero when the pane is gone.
    pub(crate) async fn pane_exists(&self, pane_id: &str) -> bool {
        Command::new("tmux")
            .args(["list-panes", "-t", pane_id])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Test-only: seed a `panes` map entry without going through
    /// `add_agent_pane` (which needs a live alor-main session). Lets
    /// the stale-entry eviction test set up the preconditions.
    #[cfg(test)]
    pub(crate) async fn test_insert_pane(&self, agent_id: &str, pane_id: &str) {
        self.panes.lock().await.insert(
            agent_id.to_string(),
            PaneInfo {
                pane_id: pane_id.to_string(),
                column: Some(0),
            },
        );
    }

    /// Test-only: expose whether an agent_id is currently tracked in
    /// the map (without caring what the pane_id is).
    #[cfg(test)]
    pub(crate) async fn test_has_entry(&self, agent_id: &str) -> bool {
        self.panes.lock().await.contains_key(agent_id)
    }

    /// Add an agent's session as a pane in alor-main.
    /// The pane runs `tmux attach -t alor-<agent>` so the agent session
    /// stays durable even if alor-main is destroyed.
    ///
    /// Idempotent with stale-entry eviction: if the `panes` map
    /// already tracks this agent, we verify the recorded pane still
    /// exists in tmux before short-circuiting. External pane death —
    /// the agent's tmux session crashes, its attach command exits,
    /// the pane closes — leaves the map entry orphaned, which used
    /// to cause the bug fixed by task 1ed52762: re-connecting agents
    /// silently returned Ok here without ever adding a new pane, so
    /// `connected: true` + sidebar row + working Kill button but no
    /// pane in alor-main. Now we evict the stale entry and fall
    /// through to the layout code.
    pub async fn add_agent_pane(&self, agent_id: &str) -> Result<()> {
        let mut panes = self.panes.lock().await;

        // Already has a tracked pane — verify it's still alive.
        if let Some(info) = panes.get(agent_id).cloned() {
            if self.pane_exists(&info.pane_id).await {
                tracing::debug!(
                    agent_id,
                    pane_id = %info.pane_id,
                    "agent already has a pane in {MAIN_SESSION}"
                );
                return Ok(());
            }
            tracing::warn!(
                agent_id,
                pane_id = %info.pane_id,
                "tracked pane no longer exists in tmux (external death); evicting stale entry and re-adding"
            );
            panes.remove(agent_id);
        }

        let agent_session = format!("alor-{agent_id}");
        let pane_count = self.count_panes().await;

        // The attach command — TMUX='' prevents "sessions should be nested" error.
        // `=` forces exact-match so `alor-claude` can't attach to `alor-claude-alor`.
        let attach_cmd = format!("TMUX='' tmux attach -t ={agent_session}");

        let is_orchestrator = agent_id == "orchestrator";

        // Layout strategy:
        //   Orchestrator = full-height left column (pane 0, column=None).
        //   Other agents fill columns to the right.
        //   New column (-h split) until MAX_RIGHT_COLUMNS, then stack (-v split)
        //   within the column with fewest panes.

        // Count agents per right-side column.
        let mut column_counts: HashMap<usize, usize> = HashMap::new();
        let mut column_panes: HashMap<usize, String> = HashMap::new(); // column → any pane_id in it
        let mut max_column: Option<usize> = None;
        for info in panes.values() {
            if let Some(col) = info.column {
                *column_counts.entry(col).or_insert(0) += 1;
                column_panes.entry(col).or_insert_with(|| info.pane_id.clone());
                max_column = Some(max_column.map_or(col, |m: usize| m.max(col)));
            }
        }
        let num_columns = column_counts.len();

        let (pane_id, column) = if pane_count <= 1 && panes.is_empty() {
            // First real agent — use the existing initial pane.
            let _ = Command::new("tmux")
                .args(["send-keys", "-t", &format!("{MAIN_SESSION}:0.0"), "C-c"])
                .output()
                .await;

            let output = Command::new("tmux")
                .args([
                    "respawn-pane", "-k",
                    "-t", &format!("{MAIN_SESSION}:0.0"),
                    &attach_cmd,
                ])
                .output()
                .await
                .context("respawn initial pane")?;

            if !output.status.success() {
                anyhow::bail!(
                    "respawn-pane failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            let pid = self.get_pane_id(MAIN_SESSION, 0).await
                .unwrap_or_else(|| "%0".to_string());
            let col = if is_orchestrator { None } else { Some(0) };
            (pid, col)

        } else if is_orchestrator {
            // Orchestrator always gets a new column on the left via -h split,
            // then we swap it to pane 0.
            let pid = self.split_horizontal(MAIN_SESSION, &attach_cmd).await?;
            // Swap to position 0 so it's on the left.
            let _ = Command::new("tmux")
                .args(["swap-pane", "-s", &pid, "-t", &format!("{MAIN_SESSION}:0.0")])
                .output()
                .await;
            let pid = self.get_pane_id(MAIN_SESSION, 0).await
                .unwrap_or(pid);
            (pid, None)

        } else if num_columns < MAX_RIGHT_COLUMNS {
            // Room for a new column — split horizontally (new column to the right).
            let pid = self.split_horizontal(MAIN_SESSION, &attach_cmd).await?;
            let col = max_column.map_or(0, |m| m + 1);
            (pid, Some(col))

        } else {
            // All columns full — stack in the column with fewest panes.
            let target_col = (0..num_columns)
                .min_by_key(|c| column_counts.get(c).copied().unwrap_or(0))
                .unwrap_or(0);

            let split_target = column_panes.get(&target_col)
                .cloned()
                .unwrap_or_else(|| format!("{MAIN_SESSION}:0.1"));

            let pid = self.split_vertical(&split_target, &attach_cmd).await?;
            (pid, Some(target_col))
        };

        // Rebalance: orchestrator gets left column, right side evens out.
        self.rebalance_layout().await;

        tracing::info!(agent_id, pane_id = %pane_id, column = ?column, "added agent pane to {MAIN_SESSION}");

        panes.insert(agent_id.to_string(), PaneInfo {
            pane_id,
            column,
        });

        Ok(())
    }

    /// Remove an agent's pane from alor-main.
    pub async fn remove_agent_pane(&self, agent_id: &str) -> Result<()> {
        let mut panes = self.panes.lock().await;

        let info = match panes.remove(agent_id) {
            Some(info) => info,
            None => return Ok(()),  // No pane to remove.
        };

        // Don't kill the last pane — tmux would destroy the session.
        let pane_count = self.count_panes().await;
        if pane_count <= 1 {
            tracing::info!(agent_id, "last pane, keeping placeholder");
            // Respawn as a placeholder instead of killing.
            let _ = Command::new("tmux")
                .args([
                    "respawn-pane", "-k",
                    "-t", &info.pane_id,
                    "echo", "Alor — waiting for agents...",
                ])
                .output()
                .await;
            
            panes.remove(agent_id);
            return Ok(());
        }

        let output = Command::new("tmux")
            .args(["kill-pane", "-t", &info.pane_id])
            .output()
            .await
            .context("kill agent pane")?;

        if !output.status.success() {
            tracing::warn!(
                agent_id,
                "kill-pane failed (pane may already be gone): {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        self.rebalance_layout().await;

        panes.remove(agent_id);

        tracing::info!(agent_id, "removed agent pane from {MAIN_SESSION}");
        Ok(())
    }

    /// Return list of agent IDs that have visible panes.
    pub async fn visible_agents(&self) -> Vec<String> {
        self.panes.lock().await.keys().cloned().collect()
    }

    /// Reconcile the alor-main pane layout against live agent state.
    ///
    /// Iterates every connected agent and, for each whose tmux session
    /// actually exists, ensures a pane attaching to it is present in
    /// alor-main. `add_agent_pane` is idempotent, so agents that
    /// already have a pane are no-ops.
    ///
    /// Motivating bug (task 883bdc60 / re-dispatch 1ed52762): an agent
    /// could reach `connected: true` in state with a healthy tmux
    /// session but NOT be rendered as a pane in alor-main — so Fett
    /// saw the green status dot and kill button in the sidebar, but
    /// the agent's terminal never appeared in the main view. Root
    /// cause is likely one of:
    ///   * `add_agent_pane` failed silently during `wrapper.register`
    ///     (line 260 of server.rs — only logged `warn`, not
    ///     propagated).
    ///   * The daemon restarted before the pane was added, and
    ///     `scan_existing_panes` on the next boot couldn't find the
    ///     pane because it was never created.
    ///   * A race where the wrapper connected before
    ///     `ensure_main_session` finished.
    /// Reconciliation is the safety net: whatever class of failure led
    /// to the miss, one scan fixes it.
    ///
    /// Does NOT touch agent state. If a connected agent's tmux session
    /// is gone (a zombie where only the wrapper process remains), this
    /// logs a warning and skips — leaving the decision of whether to
    /// flip `connected` back to `false` to higher-level paths.
    pub async fn reconcile_panes(&self, state: &crate::daemon::state::AppState) {
        let agents = state.all_agents();
        let mut added = 0usize;
        let mut skipped_zombie = 0usize;
        for agent in agents {
            if !agent.connected {
                continue;
            }
            let session_name = agent
                .tmux_session
                .clone()
                .unwrap_or_else(|| format!("alor-{}", agent.id));
            if !self.session_exists(&session_name).await {
                tracing::warn!(
                    agent_id = %agent.id,
                    session = %session_name,
                    "reconcile_panes: agent marked connected but tmux session is gone; skipping"
                );
                skipped_zombie += 1;
                continue;
            }
            // Quick win: if we already track a pane, no-op silently.
            {
                let panes = self.panes.lock().await;
                if panes.contains_key(&agent.id) {
                    continue;
                }
            }
            match self.add_agent_pane(&agent.id).await {
                Ok(()) => {
                    tracing::info!(
                        agent_id = %agent.id,
                        "reconcile_panes: added missing pane for connected agent"
                    );
                    added += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent.id,
                        "reconcile_panes: add_agent_pane failed: {e:#}"
                    );
                }
            }
        }
        if added > 0 || skipped_zombie > 0 {
            tracing::info!(
                added,
                skipped_zombie,
                "reconcile_panes complete"
            );
        }
    }

    /// Clear all tracked panes from the in-memory map.
    pub async fn clear_all_panes(&self) {
        self.panes.lock().await.clear();
    }

    /// Split horizontally (new column to the right).
    async fn split_horizontal(&self, session: &str, cmd: &str) -> Result<String> {
        let output = Command::new("tmux")
            .args([
                "split-window", "-t", session,
                "-h",  // new column
                "-P", "-F", "#{pane_id}",
                cmd,
            ])
            .env("TMUX", "")
            .output()
            .await
            .context("split-window -h")?;

        if !output.status.success() {
            // Retry once after rebalancing
            self.rebalance_layout().await;
            let output = Command::new("tmux")
                .args([
                    "split-window", "-t", session,
                    "-h",
                    "-P", "-F", "#{pane_id}",
                    cmd,
                ])
                .env("TMUX", "")
                .output()
                .await
                .context("split-window -h retry")?;

            if !output.status.success() {
                anyhow::bail!(
                    "split-window -h failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Split vertically within an existing pane (stack top/bottom).
    async fn split_vertical(&self, target: &str, cmd: &str) -> Result<String> {
        let output = Command::new("tmux")
            .args([
                "split-window", "-t", target,
                "-v",  // stack within column
                "-P", "-F", "#{pane_id}",
                cmd,
            ])
            .env("TMUX", "")
            .output()
            .await
            .context("split-window -v")?;

        if !output.status.success() {
            // Retry once after rebalancing
            self.rebalance_layout().await;
            let output = Command::new("tmux")
                .args([
                    "split-window", "-t", target,
                    "-v",
                    "-P", "-F", "#{pane_id}",
                    cmd,
                ])
                .env("TMUX", "")
                .output()
                .await
                .context("split-window -v retry")?;

            if !output.status.success() {
                anyhow::bail!(
                    "split-window -v failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Rebalance the pane layout in alor-main.
    /// Applies main-vertical first (orchestrator full left, agents stacked right),
    /// then switches to even-vertical within the right column so all pane
    /// borders remain draggable. Named layouts lock borders; the even-*
    /// re-layout converts it to a custom geometry that tmux lets you drag.
    async fn rebalance_layout(&self) {
        // Step 1: main-vertical sets the overall shape.
        let _ = Command::new("tmux")
            .args(["select-layout", "-t", MAIN_SESSION, "main-vertical"])
            .output()
            .await;

        // Step 2: even out the right-side panes so tmux treats borders as
        // manually-set (draggable). We do this by selecting even-vertical
        // on the non-orchestrator panes. A simpler approach: just re-select
        // the same layout with -E (spread evenly) which makes it a custom
        // layout internally.
        let _ = Command::new("tmux")
            .args(["select-layout", "-t", MAIN_SESSION, "-E"])
            .output()
            .await;
    }

    /// Count current panes in alor-main.
    async fn count_panes(&self) -> usize {
        let output = Command::new("tmux")
            .args(["list-panes", "-t", MAIN_SESSION, "-F", "#{pane_id}"])
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .count()
            }
            _ => 0,
        }
    }

    /// Get the pane ID for a specific pane index.
    async fn get_pane_id(&self, session: &str, index: usize) -> Option<String> {
        let target = format!("{session}:0.{index}");
        let output = Command::new("tmux")
            .args(["display-message", "-t", &target, "-p", "#{pane_id}"])
            .output()
            .await
            .ok()?;

        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            None
        }
    }

    /// Scan existing panes in alor-main and populate the in-memory map.
    /// Called on startup when reusing an existing session to prevent duplicates.
    async fn scan_existing_panes(&self) {
        let output = Command::new("tmux")
            .args([
                "list-panes", "-t", MAIN_SESSION,
                "-F", "#{pane_id} #{pane_start_command}",
            ])
            .output()
            .await;

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return,
        };

        let mut panes = self.panes.lock().await;
        let text = String::from_utf8_lossy(&output.stdout);

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Format: "%5 TMUX='' tmux attach -t alor-claude"
            // Extract pane_id and agent_id from the attach target.
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() < 2 {
                continue;
            }
            let pane_id = parts[0].to_string();
            let cmd = parts[1];

            // Look for "alor-" in the command.
            if let Some(pos) = cmd.find("alor-") {
                let after = &cmd[pos + 5..]; // skip "alor-" (5 chars)
                let agent_id = after
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches('"')
                    .to_string();

                if !agent_id.is_empty() && !panes.contains_key(&agent_id) {
                    // Orchestrator has no column; others get auto-assigned.
                    let column = if agent_id == "orchestrator" {
                        None
                    } else {
                        // Assign to next available column based on scan order.
                        let existing_cols: std::collections::HashSet<usize> = panes.values()
                            .filter_map(|p| p.column)
                            .collect();
                        let next = (0..).find(|c| !existing_cols.contains(c)).unwrap_or(0);
                        Some(next)
                    };
                    tracing::info!(agent_id, pane_id, column = ?column, "found existing pane on scan");
                    panes.insert(agent_id.clone(), PaneInfo {
                        pane_id,
                        column,
                    });
                }
            }
        }

        tracing::info!(count = panes.len(), "scanned existing panes in {MAIN_SESSION}");
    }

    /// Check if a tmux session exists.
    pub async fn session_exists(&self, name: &str) -> bool {
        // `=` forces exact match.  Without it, tmux's prefix matching
        // reports `alor-claude` as existing whenever `alor-claude-alor`
        // exists — causing the `claude` yaml template to "reclaim" the
        // instance's session on boot and giving two agents the same pane.
        let exact = format!("={name}");
        Command::new("tmux")
            .args(["has-session", "-t", &exact])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a disposable tmux session and return (session_name, first_pane_id).
    /// Caller is responsible for kill-session on cleanup.
    async fn make_test_session() -> Option<(String, String)> {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let session = format!("alor-pm-test-{nonce}");
        let ok = Command::new("tmux")
            .args(["new-session", "-d", "-s", &session, "sleep", "300"])
            .output()
            .await
            .ok()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
        // Grab the first pane's id.
        let out = Command::new("tmux")
            .args(["list-panes", "-t", &session, "-F", "#{pane_id}"])
            .output()
            .await
            .ok()?;
        let pane_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Some((session, pane_id))
    }

    async fn kill_session(session: &str) {
        let exact = format!("={session}");
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &exact])
            .output()
            .await;
    }

    /// Skip the test gracefully if tmux isn't on PATH or isn't usable
    /// (CI runners without tmux, sandbox environments).
    async fn tmux_available() -> bool {
        Command::new("tmux")
            .arg("-V")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn pane_exists_returns_true_for_live_pane_and_false_after_kill() {
        if !tmux_available().await {
            eprintln!("tmux not available, skipping");
            return;
        }
        let (session, pane_id) = match make_test_session().await {
            Some(v) => v,
            None => {
                eprintln!("failed to create test session, skipping");
                return;
            }
        };

        let pm = PaneManager::new();

        // Alive: list-panes succeeds.
        assert!(
            pm.pane_exists(&pane_id).await,
            "pane_exists should be true for a freshly-created pane {pane_id}"
        );

        // Kill the session → pane dies with it.
        kill_session(&session).await;

        // Dead: list-panes fails.
        assert!(
            !pm.pane_exists(&pane_id).await,
            "pane_exists should be false for a dead pane {pane_id}"
        );
    }

    #[tokio::test]
    async fn pane_exists_rejects_bogus_id() {
        if !tmux_available().await {
            eprintln!("tmux not available, skipping");
            return;
        }
        let pm = PaneManager::new();
        // Very high pane id nothing else is likely using. `list-panes -t
        // %999999` returns "can't find pane" with exit 1.
        assert!(!pm.pane_exists("%999999").await);
    }

    #[tokio::test]
    async fn add_agent_pane_evicts_stale_entry_when_tracked_pane_is_dead() {
        // Regression test for task 1ed52762 root cause. Before this
        // fix, add_agent_pane would early-return Ok() whenever the
        // `panes` map had an entry for the agent — even if the tracked
        // pane had been killed externally. The agent would stay
        // connected with the sidebar rendering it, but no pane would
        // ever appear in alor-main.
        //
        // This test doesn't exercise the full layout machinery (which
        // requires a live alor-main session). It verifies the specific
        // pre-condition eviction: after seeding a stale entry and
        // letting pane_exists confirm it's dead, the stale entry is
        // gone. The actual re-add path is exercised live (see the
        // failure-mode discussion in add_agent_pane's docstring).
        if !tmux_available().await {
            eprintln!("tmux not available, skipping");
            return;
        }

        let pm = PaneManager::new();

        // Seed a stale entry: agent_id "pm-test-ghost" → pane_id "%999998"
        // (almost certainly does not exist).
        pm.test_insert_pane("pm-test-ghost", "%999998").await;
        assert!(pm.test_has_entry("pm-test-ghost").await);

        // add_agent_pane would try to lay out a pane in alor-main,
        // which doesn't exist in the test environment — that's an
        // expected failure we ignore. What we CARE about is whether
        // the stale entry was evicted before the layout attempt.
        let _ = pm.add_agent_pane("pm-test-ghost").await;

        // Entry should be evicted — either because the layout code
        // added a fresh entry (if alor-main existed) or because the
        // stale-entry eviction ran before the layout code failed.
        // What matters: the ORIGINAL bogus %999998 is no longer in
        // the map.
        //
        // We can't reliably assert the entry is absent (some CI
        // environments might have an alor-main from a prior test run
        // and leave a new entry in place). Instead, assert that the
        // tracked pane_id (if still present) is NOT the stale one.
        let panes = pm.panes.lock().await;
        if let Some(info) = panes.get("pm-test-ghost") {
            assert_ne!(
                info.pane_id, "%999998",
                "stale pane_id must not survive eviction"
            );
        }
    }
}
