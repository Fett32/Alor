/// Session persistence helpers.
///
/// Alor uses three directories:
///   config  → ~/.config/alor/
///   data    → ~/.local/share/alor/
///   sockets → /tmp/alor/
///
/// `ensure_dirs()` must be called at startup before any path is used.

use anyhow::Context;
use directories::ProjectDirs;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

fn project_dirs() -> anyhow::Result<ProjectDirs> {
    ProjectDirs::from("", "", "alor")
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))
}

pub fn config_dir() -> anyhow::Result<PathBuf> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

pub fn data_dir() -> anyhow::Result<PathBuf> {
    Ok(project_dirs()?.data_dir().to_path_buf())
}

pub fn socket_dir() -> PathBuf {
    PathBuf::from("/tmp/alor")
}

// ---------------------------------------------------------------------------
// Ensure all directories exist at startup
// ---------------------------------------------------------------------------

pub fn ensure_dirs() -> anyhow::Result<()> {
    let config = config_dir().context("config dir")?;
    let data = data_dir().context("data dir")?;
    let sockets = socket_dir();

    std::fs::create_dir_all(&config)
        .with_context(|| format!("create {}", config.display()))?;
    std::fs::create_dir_all(&data)
        .with_context(|| format!("create {}", data.display()))?;
    std::fs::create_dir_all(&sockets)
        .with_context(|| format!("create {}", sockets.display()))?;
    // Restrict socket dir to current user only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(&sockets, perms)
            .with_context(|| format!("chmod 0700 {}", sockets.display()))?;
    }

    tracing::debug!(
        config = %config.display(),
        data = %data.display(),
        sockets = %sockets.display(),
        "runtime directories ready"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Session file (simple JSON snapshot)
// ---------------------------------------------------------------------------

/// Path to the running-session snapshot file.
pub fn session_file() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("session.json"))
}

/// Path to the terminal-task archive file.  Terminal tasks (COMPLETED,
/// CANCELLED, REJECTED, TIMED_OUT, STALE) are swept out of live state.json
/// into this file on every daemon startup so the running state stays lean.
pub fn tasks_archive_file() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("tasks-archive.json"))
}

/// Lightweight metadata written to disk so the UI can show "last session" info.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionInfo {
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub pid: u32,
    pub version: String,
}

impl SessionInfo {
    pub fn current() -> Self {
        Self {
            started_at: chrono::Utc::now(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Wrapper PID tracking
// ---------------------------------------------------------------------------

/// Path to the file that records PIDs of wrapper processes we launched.
pub fn wrapper_pids_file() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("wrapper_pids.json"))
}

/// Save wrapper PIDs to disk so we can kill exactly these on restart.
/// Appends to the existing file if it exists.
pub fn save_wrapper_pids(new_pids: &[u32]) -> anyhow::Result<()> {
    let path = wrapper_pids_file()?;
    let mut pids = if path.exists() {
        let json = std::fs::read_to_string(&path)?;
        serde_json::from_str::<Vec<u32>>(&json).unwrap_or_default()
    } else {
        Vec::new()
    };

    pids.extend_from_slice(new_pids);
    pids.sort_unstable();
    pids.dedup();

    let json = serde_json::to_string(&pids)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)
        .with_context(|| format!("write temp wrapper pids {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename wrapper pids {}", path.display()))?;
    Ok(())
}

/// Load and kill stale wrapper PIDs from a previous daemon run.
/// Removes the PID file afterward.
pub fn kill_stale_wrappers() {
    let path = match wrapper_pids_file() {
        Ok(p) => p,
        Err(_) => return,
    };
    if !path.exists() {
        return;
    }

    match std::fs::read_to_string(&path) {
        Ok(json) => {
            if let Ok(pids) = serde_json::from_str::<Vec<u32>>(&json) {
                for pid in &pids {
                    // Only kill if the process is actually a alor-wrapper.
                    // SIGTERM (15) gives it a chance to clean up.
                    let cmdline_path = format!("/proc/{pid}/cmdline");
                    if let Ok(cmdline) = std::fs::read_to_string(&cmdline_path) {
                        if cmdline.contains("alor-wrapper") {
                            tracing::info!(pid, "killing stale wrapper");
                            let _ = std::process::Command::new("kill")
                                .args(["-TERM", &pid.to_string()])
                                .output();
                        } else {
                            tracing::debug!(pid, "PID no longer a wrapper, skipping");
                        }
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!("failed to read wrapper pids file: {e}");
        }
    }

    // Remove the stale file.
    let _ = std::fs::remove_file(&path);
}

impl SessionInfo {

    pub fn write(&self) -> anyhow::Result<()> {
        let path = session_file()?;
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .with_context(|| format!("write temp session file {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("rename session file {}", path.display()))?;
        Ok(())
    }

}

// ---------------------------------------------------------------------------
// Orphaned-session sweeper
// ---------------------------------------------------------------------------
//
// Context: crashed / force-killed agents can leave behind `alor-<id>`
// tmux sessions without a corresponding entry in state.json OR a yaml
// config. Those sessions accumulate over time — invisible to the UI,
// invisible to `cli.agent.list`, but they persist and clutter
// `tmux ls`. The sweeper runs on daemon boot, enumerates `alor-*`
// sessions, cross-references them against the union of
// `app_state.all_agents()` + `agent_configs`, and kills any it can't
// account for.
//
// Protected sessions (never killed by the sweeper, regardless of
// agent state): `alor-main` is the UI's display session, not an
// agent slot. It's the one hard-coded exception. Every other
// `alor-<id>` session must correspond to a known agent id to
// survive the sweep.

/// Separate `alor-*` sessions from non-alor ones AND from known
/// agents. Pure function — takes the session list + known-id set,
/// returns the names of sessions that should be killed.
///
/// Factored out so the policy ("what counts as orphaned") is
/// unit-testable without shelling to tmux. Call-site in
/// `sweep_orphaned_agent_sessions` handles the tmux enumeration +
/// kill commands.
///
/// Rules:
///   1. Non-`alor-*` sessions ignored entirely (not our business).
///   2. `alor-main` ALWAYS kept (display session, not an agent).
///   3. `alor-<id>` kept iff `id` is in `known_ids`. Otherwise
///      orphaned.
pub(crate) fn pick_orphan_sessions(
    all_sessions: &[String],
    known_ids: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut orphans = Vec::new();
    for session in all_sessions {
        if session == "alor-main" {
            continue; // protected
        }
        let id = match session.strip_prefix("alor-") {
            Some(id) if !id.is_empty() => id,
            _ => continue, // non-alor session, not our concern
        };
        if !known_ids.contains(id) {
            orphans.push(session.clone());
        }
    }
    orphans
}

/// Enumerate `alor-*` tmux sessions and kill any whose id doesn't
/// correspond to a known agent (per `app_state.all_agents()` ∪
/// `agent_configs`).
///
/// Called from `lib.rs` on daemon boot, on a short delay so
/// reclaimed wrappers have had a chance to register (their state
/// entries are loaded from state.json synchronously at startup, so
/// they're already in `all_agents()`, but the wrapper may still be
/// mid-session-create for autolaunch slots). Safe to call multiple
/// times — idempotent, no-ops when no orphans exist.
///
/// Logs every killed session at warn level so an operator auditing
/// the daemon log can see what got swept and why (a session's
/// presence in the log but not in state usually means a state
/// corruption or a prior agent_delete that didn't kill the tmux
/// session).
pub fn sweep_orphaned_agent_sessions(
    app_state: &super::state::AppState,
    agent_configs: &[(String, super::config::AgentConfig)],
) {
    // Enumerate tmux sessions. If tmux is unavailable or
    // list-sessions fails, silently bail — nothing to sweep.
    let output = match std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(_) => {
            tracing::debug!("orphan sweeper: tmux list-sessions returned non-zero; likely no server");
            return;
        }
        Err(e) => {
            tracing::debug!("orphan sweeper: tmux list-sessions failed to spawn: {e}");
            return;
        }
    };

    let all_sessions: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect();

    // Union of state + yaml-config IDs. State covers runtime
    // template instances + fixed slots that were ever registered;
    // agent_configs covers every yaml declaration (fixed slot OR
    // template) so a template yaml name doesn't get swept if
    // someone manually created its session for testing.
    let mut known: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for agent in app_state.all_agents() {
        known.insert(agent.id);
    }
    for (id, _) in agent_configs {
        known.insert(id.clone());
    }

    let orphans = pick_orphan_sessions(&all_sessions, &known);

    if orphans.is_empty() {
        tracing::debug!(
            scanned = all_sessions.len(),
            known = known.len(),
            "orphan sweeper: no orphaned sessions found"
        );
        return;
    }

    tracing::warn!(
        count = orphans.len(),
        "orphan sweeper: killing {} orphaned tmux session(s); agents unknown to both state and yaml configs",
        orphans.len(),
    );
    for session in &orphans {
        tracing::warn!(session = %session, "killing orphaned tmux session");
        // Exact-match (`=`) so a substring collision with an agent
        // named similarly can't accidentally get swept.
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &format!("={session}")])
            .output();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn known(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn sessions(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pick_orphans_keeps_known_agents() {
        // Sessions that match known agent ids must be preserved.
        let all = sessions(&[
            "alor-claude-alor",
            "alor-codex",
            "alor-orchestrator",
        ]);
        let k = known(&["claude-alor", "codex", "orchestrator"]);
        assert_eq!(pick_orphan_sessions(&all, &k), Vec::<String>::new());
    }

    #[test]
    fn pick_orphans_kills_unknown_alor_sessions() {
        // Sessions matching the `alor-*` pattern but without a
        // known agent id are orphaned — this is the motivating case.
        let all = sessions(&[
            "alor-claude-alor",       // known
            "alor-ghost-instance",    // orphan
            "alor-old-template-inst", // orphan
        ]);
        let k = known(&["claude-alor"]);
        let orphans = pick_orphan_sessions(&all, &k);
        assert_eq!(
            orphans,
            vec![
                "alor-ghost-instance".to_string(),
                "alor-old-template-inst".to_string(),
            ]
        );
    }

    #[test]
    fn pick_orphans_protects_alor_main() {
        // alor-main is the UI display session, NOT an agent. Even
        // when `main` is not in the known set, it must survive the
        // sweep — killing it would take down the UI's terminal pane
        // until the next daemon restart rebuilds it.
        let all = sessions(&["alor-main", "alor-claude-alor"]);
        let k = known(&["claude-alor"]); // no "main"
        assert_eq!(pick_orphan_sessions(&all, &k), Vec::<String>::new());
    }

    #[test]
    fn pick_orphans_ignores_non_alor_sessions() {
        // Any session without the `alor-` prefix is not our
        // business — user's own tmux, unrelated dev sessions, etc.
        // Sweep must pass them through untouched.
        let all = sessions(&[
            "cobblemon",           // user session
            "my-dev-work",         // user session
            "alor-ghost",          // orphan — should be killed
            "alor-claude-alor",    // known
            "some-random",         // user session
        ]);
        let k = known(&["claude-alor"]);
        let orphans = pick_orphan_sessions(&all, &k);
        assert_eq!(orphans, vec!["alor-ghost".to_string()]);
    }

    #[test]
    fn pick_orphans_ignores_bare_alor_dash() {
        // `alor-` (prefix only, no id after) shouldn't count as a
        // known or orphan — the `id` slice would be empty. Skip.
        let all = sessions(&["alor-", "alor-real-agent"]);
        let k = known(&["real-agent"]);
        let orphans = pick_orphan_sessions(&all, &k);
        assert!(
            orphans.is_empty(),
            "bare `alor-` session should be skipped, not swept"
        );
    }

    #[test]
    fn pick_orphans_handles_empty_known_set() {
        // Fresh daemon boot with no agents registered AND no yaml
        // configs — every `alor-<id>` session except alor-main is
        // an orphan. Edge case: dev-machine with an unrelated
        // `alor-foo` session from a previous test harness.
        let all = sessions(&["alor-main", "alor-stale-1", "alor-stale-2"]);
        let k: HashSet<String> = HashSet::new();
        let orphans = pick_orphan_sessions(&all, &k);
        assert_eq!(
            orphans,
            vec![
                "alor-stale-1".to_string(),
                "alor-stale-2".to_string(),
            ],
            "alor-main preserved; both alor-* orphans swept"
        );
    }

    #[test]
    fn pick_orphans_handles_empty_session_list() {
        // No tmux server running → no sessions. Sweep is a no-op.
        let all: Vec<String> = vec![];
        let k = known(&["claude-alor"]);
        assert_eq!(pick_orphan_sessions(&all, &k), Vec::<String>::new());
    }

    #[test]
    fn pick_orphans_exact_prefix_not_substring() {
        // `alorsomething` (no dash) must NOT be treated as an
        // alor-* session. Prefix check uses `alor-`, not `alor`.
        let all = sessions(&["alorsomething", "alor-real"]);
        let k = known(&["real"]);
        let orphans = pick_orphan_sessions(&all, &k);
        assert!(
            !orphans.contains(&"alorsomething".to_string()),
            "non-prefixed session must pass through"
        );
    }

    #[test]
    fn pick_orphans_yaml_slot_counted_even_if_no_running_agent() {
        // Case from the design doc: a yaml slot exists (`claude`),
        // but no agent entry in state (fresh install, never
        // connected). If a session `alor-claude` exists on disk
        // (user pre-created it, or it's leftover from a prior
        // install), it must NOT be swept — the yaml knows about it
        // and a wrapper launch could reclaim it. This is why the
        // `known_ids` set is UNION of state + yaml configs, not
        // intersection.
        let all = sessions(&["alor-claude"]);
        let k = known(&["claude"]); // yaml slot, not in state yet
        assert_eq!(pick_orphan_sessions(&all, &k), Vec::<String>::new());
    }
}
