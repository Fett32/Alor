/// Agent configuration loaded from ~/.config/alor/agents/*.yaml.
///
/// Each YAML file defines one agent. The filename (minus .yaml) is the agent ID.
/// On startup, all configs are loaded and agents are auto-registered.
/// Agents with `autolaunch: true` get their wrappers spawned automatically.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Child;

use super::session;
use super::state::{Agent, AppState};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns the user's home directory as a PathBuf.
fn dirs_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
}

// ---------------------------------------------------------------------------
// Daemon-wide config
// ---------------------------------------------------------------------------

/// Daemon-level operator knobs, loaded from `~/.config/alor/daemon.yaml`.
///
/// Distinct from the per-agent YAMLs in `~/.config/alor/agents/*.yaml`
/// — this file (if present) tunes the daemon as a whole, not any one
/// slot. Absent file = defaults. Parse failure = defaults with a warn
/// log; startup must not block on a malformed config.
///
/// Example `~/.config/alor/daemon.yaml`:
/// ```yaml
/// max_terminal_tasks_retained: 500
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct DaemonConfig {
    /// Maximum terminal-state tasks retained in live `state.json`.
    /// Oldest-first eviction by `updated_at` (UUID tiebreak) on every
    /// save. `0` = no cap (unbounded growth, pre-T14 behaviour).
    /// Default `1000`.
    ///
    /// Size rationale: 1000 terminals ≈ 2 MiB of state.json assuming
    /// ~2 KiB per serialized task. Cheap to load on boot and cheap
    /// to serialize on every persist; leaves plenty of headroom for
    /// the orchestrator's routine `task_list` scans without dragging
    /// the whole tail along.
    #[serde(default = "default_max_terminal_tasks_retained")]
    pub max_terminal_tasks_retained: usize,
}

fn default_max_terminal_tasks_retained() -> usize {
    // Matches `state.rs::DEFAULT_MAX_TERMINAL_RETAINED`. Kept as a
    // free fn here rather than a re-export so serde can resolve it
    // from the #[serde(default = "...")] attribute without a
    // path-visibility dance.
    1000
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            max_terminal_tasks_retained: default_max_terminal_tasks_retained(),
        }
    }
}

/// Load `~/.config/alor/daemon.yaml` into a `DaemonConfig`, falling
/// back to `DaemonConfig::default()` on any non-fatal error (absent
/// file, parse error, unreadable). Errors are logged at `warn!` so
/// an operator can see the config wasn't picked up without the
/// daemon refusing to start.
pub fn load_daemon_config() -> DaemonConfig {
    let config_dir = match session::config_dir() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("could not resolve config dir for daemon.yaml: {e:#}");
            return DaemonConfig::default();
        }
    };
    let path = config_dir.join("daemon.yaml");
    if !path.exists() {
        tracing::debug!(
            path = %path.display(),
            "no daemon.yaml present; using default daemon config"
        );
        return DaemonConfig::default();
    }
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                "failed to read daemon.yaml: {e}; using defaults"
            );
            return DaemonConfig::default();
        }
    };
    match serde_yaml::from_str::<DaemonConfig>(&contents) {
        Ok(cfg) => {
            tracing::info!(
                max_terminal_tasks_retained = cfg.max_terminal_tasks_retained,
                "loaded daemon config from {}",
                path.display()
            );
            cfg
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                "failed to parse daemon.yaml: {e}; using defaults"
            );
            DaemonConfig::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Config structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    /// Display symbol for the UI.
    #[serde(default)]
    pub identity: Option<String>,
    /// Agent role: coder, orchestrator, reviewer, etc.
    #[serde(default = "default_role")]
    pub role: String,
    /// Command to launch the agent in its tmux session.
    #[serde(default)]
    pub command: Vec<String>,
    /// Whether to auto-launch a wrapper for this agent on startup.
    #[serde(default)]
    pub autolaunch: bool,
    /// Working directory for the agent session.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Startup file to inject into the agent session.
    #[serde(default)]
    pub startup_file: Option<String>,
    /// Project this slot is bound to; null for generic/unscoped agents.
    #[serde(default)]
    pub project: Option<String>,
    /// Task kinds this slot is good at (e.g. "implementation", "review").
    /// Orchestrator uses this as a routing hint.
    #[serde(default)]
    pub use_for: Vec<String>,
    /// Tier classification: heavy | mid | light.
    #[serde(default = "default_tier")]
    pub tier: String,
    /// Max simultaneous non-terminal tasks before the daemon rejects new
    /// assignments on this slot.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u8,
    /// Runtime layer used to spawn this agent:
    ///   - "wrapper" (default): launches via alor-wrapper, which runs the
    ///     `command` inside a tmux session and scrapes its output.
    ///   - "claude-sdk": launches run-worker.sh inside a tmux session; the
    ///     Python worker talks the wrapper wire protocol directly to the
    ///     daemon and hosts a ClaudeSDKClient internally.
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// If true, this yaml declares a *template* rather than a fixed slot.
    /// Templates are not auto-registered; the orchestrator spawns instances
    /// from them at runtime with a project + working_dir override.
    #[serde(default)]
    pub template: bool,
}

fn default_role() -> String {
    "coder".to_string()
}

fn default_tier() -> String {
    "mid".to_string()
}

fn default_max_concurrent() -> u8 {
    1
}

fn default_runtime() -> String {
    "wrapper".to_string()
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load all agent configs from ~/.config/alor/agents/*.yaml.
/// Returns (agent_id, config) pairs. The agent_id is the filename stem.
pub fn load_agent_configs() -> Result<Vec<(String, AgentConfig)>> {
    let config_dir = session::config_dir()?;
    let agents_dir = config_dir.join("agents");

    if !agents_dir.exists() {
        tracing::info!("no agents config dir at {}", agents_dir.display());
        return Ok(vec![]);
    }

    let mut configs = Vec::new();

    let entries = std::fs::read_dir(&agents_dir)
        .with_context(|| format!("failed to read {}", agents_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }

        let agent_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());

        if let Some(id) = agent_id {
            match load_one(&path) {
                Ok(cfg) => {
                    tracing::info!(agent_id = %id, role = %cfg.role, "loaded agent config");
                    configs.push((id, cfg));
                }
                Err(e) => {
                    tracing::warn!("failed to load {}: {e:#}", path.display());
                }
            }
        }
    }

    Ok(configs)
}

fn load_one(path: &Path) -> Result<AgentConfig> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let config: AgentConfig = serde_yaml::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(config)
}

/// Register agents from config into AppState.
/// Only registers agents that aren't already registered.
pub fn register_agents_from_config(state: &AppState, configs: &[(String, AgentConfig)]) {
    let existing = state.all_agents();
    let existing_ids: std::collections::HashSet<&str> =
        existing.iter().map(|a| a.id.as_str()).collect();

    for (id, cfg) in configs {
        if cfg.template {
            tracing::debug!(agent_id = %id, "template config, not registering as slot");
            continue;
        }
        if existing_ids.contains(id.as_str()) {
            tracing::debug!(agent_id = %id, "agent already registered, skipping");
            continue;
        }

        let display_name = if let Some(ref identity) = cfg.identity {
            format!("{identity} {id}")
        } else {
            id.clone()
        };

        let mut agent = Agent::new(id.clone(), display_name);
        agent.tmux_session = Some(format!("alor-{id}"));
        agent.project = cfg.project.clone();
        agent.tier = cfg.tier.clone();
        agent.max_concurrent = cfg.max_concurrent;
        state.register_agent(agent);

        tracing::info!(
            agent_id = %id,
            role = %cfg.role,
            project = ?cfg.project,
            tier = %cfg.tier,
            max = cfg.max_concurrent,
            "agent registered from config"
        );
    }
}

// ---------------------------------------------------------------------------
// Wrapper auto-launch
// ---------------------------------------------------------------------------

/// Find the alor-wrapper binary. Looks next to the current executable first,
/// then falls back to PATH.
pub fn find_wrapper_binary() -> Result<PathBuf> {
    // Try next to the current executable (workspace builds put both in target/debug/).
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.parent().unwrap_or(Path::new(".")).join("alor-wrapper");
        if sibling.exists() {
            return Ok(sibling);
        }
    }

    // Fall back to PATH lookup.
    if let Ok(output) = std::process::Command::new("which")
        .arg("alor-wrapper")
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    anyhow::bail!("alor-wrapper binary not found")
}

/// Spawn wrapper processes for agents provided in the list.
/// Returns the child processes so the caller can track/kill them.
pub fn launch_wrappers(configs: &[(String, AgentConfig)]) -> Vec<(String, Child)> {
    launch_wrappers_internal(configs, true) // Force true because we explicitly want these
}

/// Launch plan for a given yaml slot. Mirrors the runtime branch in
/// `wrapper/server/agent_lifecycle.rs::handle_spawn` so the CLI and
/// Tauri paths pick the same shape per runtime. Factored out as a
/// pure enum + dispatch fn so unit tests can verify the split
/// without spawning real processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LaunchPlan {
    /// `claude-sdk` runtime — `tmux new-session -d` running the
    /// yaml `command[0]` (typically `run-worker.sh`) with the agent
    /// id + optional workdir/project flags.
    ClaudeSdk,
    /// Default/wrapper runtime — exec the `alor-wrapper` binary with
    /// the agent id + optional `--command`, `--workdir`,
    /// `--startup-file` flags.
    Wrapper,
}

/// Pure dispatch: pick the launch plan from the yaml's `runtime`
/// field. The string `"claude-sdk"` is the only value that routes
/// to the SDK path; every other value (including `"wrapper"`,
/// unrecognized strings, or the default) goes to the classic
/// wrapper path.
pub(crate) fn plan_launch(cfg: &AgentConfig) -> LaunchPlan {
    if cfg.runtime == "claude-sdk" {
        LaunchPlan::ClaudeSdk
    } else {
        LaunchPlan::Wrapper
    }
}

/// Force spawn a single agent regardless of autolaunch setting.
/// Runtime-aware: dispatches to the alor-wrapper path OR the
/// claude-sdk (run-worker.sh via tmux) path based on the yaml's
/// `runtime` field.
///
/// Pre-rename this was `force_launch_wrapper` and always shelled to
/// alor-wrapper — which silently mis-launched `claude-sdk` yamls
/// (they can't be driven by alor-wrapper; they need the dedicated
/// Python worker). The split-brain vs the CLI spawn path
/// (`wrapper/server/agent_lifecycle.rs::handle_spawn`, which DID
/// honor runtime) was the codex-alor audit #1 finding. Both paths
/// now share the `plan_launch` dispatch and build their commands to
/// the same shape per runtime.
pub fn force_launch_agent(id: String, cfg: AgentConfig) -> Option<Child> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let expanded_workdir: Option<PathBuf> = cfg.working_dir.as_ref().map(|wd| {
        if wd.starts_with('~') {
            PathBuf::from(&home).join(wd.strip_prefix("~/").unwrap_or(&wd[1..]))
        } else {
            PathBuf::from(wd)
        }
    });

    let mut cmd = match plan_launch(&cfg) {
        LaunchPlan::ClaudeSdk => {
            // SDK worker path: `tmux new-session -d -s alor-<id> --
            // <run-worker.sh> <id> [--workdir WD] [--project PROJ]`.
            // Python worker talks the wrapper wire protocol directly
            // to the daemon and hosts its own ClaudeSDKClient. No
            // alor-wrapper in the loop.
            if cfg.command.is_empty() {
                tracing::error!(
                    agent_id = %id,
                    "claude-sdk runtime requires `command:` in yaml (typically run-worker.sh)"
                );
                return None;
            }
            let session_name = format!("alor-{id}");
            let mut c = std::process::Command::new("tmux");
            c.args(["new-session", "-d", "-s", &session_name]);
            if let Some(ref wd) = expanded_workdir {
                c.arg("-c").arg(wd);
            }
            // Everything after `--` is the command tmux runs inside
            // the detached session.
            c.arg("--");
            c.arg(&cfg.command[0]);
            c.arg(&id);
            if let Some(ref wd) = expanded_workdir {
                c.arg("--workdir").arg(wd);
            }
            if let Some(ref proj) = cfg.project {
                c.arg("--project").arg(proj);
            }
            c
        }
        LaunchPlan::Wrapper => {
            // Classic wrapper path: `alor-wrapper <id> [--command ...]
            // [--workdir WD] [--startup-file PATH]`.
            let wrapper_bin = match find_wrapper_binary() {
                Ok(bin) => bin,
                Err(e) => {
                    tracing::warn!("cannot force launch wrapper: {e}");
                    return None;
                }
            };
            let mut c = std::process::Command::new(&wrapper_bin);
            c.arg(&id);
            if !cfg.command.is_empty() {
                c.arg("--command").arg(cfg.command.join(" "));
            }
            if let Some(ref wd) = expanded_workdir {
                c.arg("--workdir").arg(wd);
            }
            if let Some(ref sf) = cfg.startup_file {
                let expanded = if sf.starts_with('~') {
                    PathBuf::from(&home).join(sf.strip_prefix("~/").unwrap_or(&sf[1..]))
                } else {
                    PathBuf::from(sf)
                };
                c.arg("--startup-file").arg(expanded);
            }
            c
        }
    };

    match cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
    {
        Ok(child) => {
            tracing::info!(
                agent_id = %id,
                pid = child.id(),
                runtime = %cfg.runtime,
                "agent force-launched"
            );
            // Claude-sdk sessions need mouse + history-limit applied
            // AFTER new-session creates them; the nested-tmux in
            // alor-main's pane can't scroll correctly without them.
            // Mirrors `agent_lifecycle::handle_spawn`'s
            // post-new-session fixup. Wrapper-runtime agents get the
            // same options applied inside alor-wrapper's own
            // `ensure_session_defaults` so no action needed here.
            if matches!(plan_launch(&cfg), LaunchPlan::ClaudeSdk) {
                let session_name = format!("alor-{id}");
                let _ = std::process::Command::new("tmux")
                    .args(["set-option", "-t", &session_name, "mouse", "on"])
                    .output();
                let _ = std::process::Command::new("tmux")
                    .args(["set-option", "-t", &session_name, "history-limit", "50000"])
                    .output();
            }
            Some(child)
        }
        Err(e) => {
            tracing::error!(
                agent_id = %id,
                runtime = %cfg.runtime,
                "failed to force launch agent: {e}"
            );
            None
        }
    }
}

fn launch_wrappers_internal(configs: &[(String, AgentConfig)], force: bool) -> Vec<(String, Child)> {
    let wrapper_bin = match find_wrapper_binary() {
        Ok(bin) => {
            tracing::info!(path = %bin.display(), "found alor-wrapper binary");
            bin
        }
        Err(e) => {
            tracing::warn!("cannot launch wrappers: {e}");
            return vec![];
        }
    };

    let mut children = Vec::new();

    for (id, cfg) in configs {
        if !force && !cfg.autolaunch {
            tracing::debug!(agent_id = %id, "autolaunch disabled, skipping");
            continue;
        }

        let mut cmd = std::process::Command::new(&wrapper_bin);
        cmd.arg(id);

        // Pass the command if specified.
        if !cfg.command.is_empty() {
            cmd.arg("--command").arg(cfg.command.join(" "));
        }

        // Pass working directory with ~ expansion.
        if let Some(ref wd) = cfg.working_dir {
            let expanded = if wd.starts_with('~') {
                dirs_home().join(wd.strip_prefix("~/").unwrap_or(&wd[1..]))
            } else {
                PathBuf::from(wd)
            };
            cmd.arg("--workdir").arg(expanded);
        }

        // Pass startup file with ~ expansion.
        if let Some(ref sf) = cfg.startup_file {
            let expanded = if sf.starts_with('~') {
                dirs_home().join(sf.strip_prefix("~/").unwrap_or(&sf[1..]))
            } else {
                PathBuf::from(sf)
            };
            cmd.arg("--startup-file").arg(expanded);
        }

        match cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
        {
            Ok(child) => {
                tracing::info!(agent_id = %id, pid = child.id(), "wrapper launched (force={force})");
                children.push((id.clone(), child));
            }
            Err(e) => {
                tracing::error!(agent_id = %id, "failed to launch wrapper: {e}");
            }
        }
    }

    children
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal AgentConfig constructor for tests — every field
    /// populated with a sensible default, `runtime` parameterized.
    fn cfg_with_runtime(runtime: &str) -> AgentConfig {
        AgentConfig {
            identity: None,
            role: default_role(),
            command: vec!["/fake/path/to/command".to_string()],
            autolaunch: false,
            startup_file: None,
            working_dir: None,
            project: None,
            use_for: vec![],
            tier: default_tier(),
            max_concurrent: default_max_concurrent(),
            runtime: runtime.to_string(),
            template: false,
        }
    }

    // ---- plan_launch dispatch ----

    #[test]
    fn plan_launch_claude_sdk_picks_sdk_path() {
        // The motivating case: `runtime: claude-sdk` yamls must
        // route to the SDK launcher (tmux + run-worker.sh), NOT
        // alor-wrapper. Pre-fix, `force_launch_wrapper` always
        // picked the wrapper path and silently mis-launched these.
        let cfg = cfg_with_runtime("claude-sdk");
        assert_eq!(plan_launch(&cfg), LaunchPlan::ClaudeSdk);
    }

    #[test]
    fn plan_launch_wrapper_picks_wrapper_path() {
        // Explicit "wrapper" — the classic case.
        let cfg = cfg_with_runtime("wrapper");
        assert_eq!(plan_launch(&cfg), LaunchPlan::Wrapper);
    }

    #[test]
    fn plan_launch_default_runtime_picks_wrapper_path() {
        // An older yaml (or one that omits runtime entirely)
        // deserializes with default_runtime() == "wrapper". Must
        // stay on the wrapper path — no silent claude-sdk routing.
        let cfg = cfg_with_runtime(&default_runtime());
        assert_eq!(plan_launch(&cfg), LaunchPlan::Wrapper);
    }

    #[test]
    fn plan_launch_unknown_runtime_falls_back_to_wrapper() {
        // Unknown runtime strings (typo in yaml, future value not
        // yet handled) MUST NOT accidentally route to the SDK path —
        // the SDK launcher wouldn't know what to do with them. Keep
        // fail-closed toward the more conservative wrapper path.
        for bogus in ["CLAUDE-SDK", "claude", "sdk", "", "node", "codex-sdk"] {
            let cfg = cfg_with_runtime(bogus);
            assert_eq!(
                plan_launch(&cfg),
                LaunchPlan::Wrapper,
                "runtime={bogus:?} must fall back to Wrapper"
            );
        }
    }

    #[test]
    fn plan_launch_is_case_sensitive() {
        // Defensive check — `"claude-sdk"` is the ONE valid SDK
        // token, lowercase, hyphenated. Any casing drift silently
        // falling back to Wrapper is the SAFE failure mode (worst
        // case: a case-mangled SDK yaml doesn't launch; operator
        // fixes the yaml).
        assert_eq!(plan_launch(&cfg_with_runtime("Claude-SDK")), LaunchPlan::Wrapper);
        assert_eq!(plan_launch(&cfg_with_runtime("claude-SDK")), LaunchPlan::Wrapper);
        assert_eq!(plan_launch(&cfg_with_runtime("CLAUDE-SDK")), LaunchPlan::Wrapper);
        // But the canonical form must route correctly.
        assert_eq!(plan_launch(&cfg_with_runtime("claude-sdk")), LaunchPlan::ClaudeSdk);
    }

    // ---- default_runtime invariant ----

    #[test]
    fn default_runtime_is_wrapper_not_sdk() {
        // If `default_runtime` ever changes to "claude-sdk", every
        // old yaml that omitted the field would silently start
        // trying to launch via run-worker.sh. That's a footgun —
        // pin it.
        assert_eq!(default_runtime(), "wrapper");
    }
}

