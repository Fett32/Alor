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
    std::fs::write(&path, json)
        .with_context(|| format!("write wrapper pids {}", path.display()))?;
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
        std::fs::write(&path, json)
            .with_context(|| format!("write session file {}", path.display()))?;
        Ok(())
    }

}
