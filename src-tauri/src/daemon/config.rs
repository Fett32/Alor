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

/// Force spawn a single wrapper regardless of autolaunch setting.
pub fn force_launch_wrapper(id: String, cfg: AgentConfig) -> Option<Child> {
    let wrapper_bin = match find_wrapper_binary() {
        Ok(bin) => bin,
        Err(e) => {
            tracing::warn!("cannot force launch wrapper: {e}");
            return None;
        }
    };

    let mut cmd = std::process::Command::new(&wrapper_bin);
    cmd.arg(&id);

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
            tracing::info!(agent_id = %id, pid = child.id(), "wrapper force-launched");
            Some(child)
        }
        Err(e) => {
            tracing::error!(agent_id = %id, "failed to force launch wrapper: {e}");
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

