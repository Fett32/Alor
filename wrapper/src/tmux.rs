use anyhow::{bail, Context, Result};
use std::process::Command;
use std::thread;
use std::time::Duration;

/// Exact-match session target. Used with commands that take a
/// target-session (has-session, kill-session, set-option). Bare `=name`
/// resolves correctly at the session level.
fn session_target(name: &str) -> String {
    format!("={name}")
}

/// Exact-match pane target. tmux's target-pane grammar is
/// `session:window.pane`; a bare `=name` without the `:` is rejected
/// with "can't find pane". Trailing `:` picks the active pane of that
/// exact session, which is what we want for send-keys and capture-pane.
fn pane_target(name: &str) -> String {
    format!("={name}:")
}

/// Returns true if a tmux session with this name currently exists.
pub fn session_exists(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", &session_target(name)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a new detached tmux session running `command` in an optional working directory.
/// After creation, disables paste detection by setting assume-paste-time to 0.
pub fn create_session_with_dir(name: &str, command: &str, workdir: Option<&str>) -> Result<()> {
    let args: Vec<&str> = command.split_whitespace().collect();
    let mut cmd = Command::new("tmux");
    cmd.args(["new-session", "-d", "-s", name]);
    if let Some(dir) = workdir {
        cmd.args(["-c", dir]);
    }
    if !args.is_empty() {
        cmd.arg("--");
        cmd.args(&args);
    }
    let out = cmd.output().context("failed to spawn tmux")?;
    if !out.status.success() {
        bail!(
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    ensure_session_defaults(name);

    Ok(())
}

/// Inject `text` into the tmux session as a single block, followed by Enter.
pub fn send_keys(name: &str, text: &str) -> Result<()> {
    let t = pane_target(name);
    // Use -l (literal) so tmux doesn't interpret special sequences.
    let out = Command::new("tmux")
        .args(["send-keys", "-t", &t, "-l", text])
        .output()
        .context("failed to spawn tmux send-keys (text)")?;
    if !out.status.success() {
        bail!("tmux send-keys failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    // Small delay to let CLI buffer the text
    thread::sleep(Duration::from_millis(200));

    // Send a final Enter to submit
    let out = Command::new("tmux")
        .args(["send-keys", "-t", &t, "Enter"])
        .output()
        .context("failed to spawn tmux send-keys (Enter)")?;
    if !out.status.success() {
        bail!("tmux send-keys Enter failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// Apply Alor's default session options (mouse, scrollback, paste detection).
/// Called on session creation and also when reusing a pre-existing session.
///
/// set-option does NOT accept the `=name` exact-match sigil (tmux 3.4
/// returns "no such session" on that form) — so this uses the bare name.
/// Collision with another `alor-*` prefix would be a problem, but we
/// block same-name duplicates at spawn time via the handle_spawn
/// collision guard, so in practice the bare name lands on the right
/// session.
pub fn ensure_session_defaults(name: &str) {
    let _ = Command::new("tmux")
        .args(["set-option", "-t", name, "mouse", "on"])
        .output();
    let _ = Command::new("tmux")
        .args(["set-option", "-t", name, "history-limit", "50000"])
        .output();
    let _ = Command::new("tmux")
        .args(["set-option", "-t", name, "assume-paste-time", "0"])
        .output();
}

/// Capture the last `lines` lines of visible pane output.
/// Returns each line as a separate String.
pub fn capture_pane(name: &str, lines: usize) -> Result<Vec<String>> {
    let start = format!("-{}", lines);
    let t = pane_target(name);
    let out = Command::new("tmux")
        .args([
            "capture-pane",
            "-p",          // print to stdout
            "-t", &t,
            "-S", &start,  // start N lines back
        ])
        .output()
        .context("failed to spawn tmux")?;
    if !out.status.success() {
        bail!(
            "tmux capture-pane failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().map(|l| l.to_owned()).collect())
}
