/// Memory Hub: manages cross-agent memory synchronization via symlinks.
///
/// This module provides the logic to "move and link" private agent memories
/// into the Alor central Hub for a project.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::fs;

#[cfg(unix)]
use std::os::unix::fs::symlink;

fn expand_tilde(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if s == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    path
}

pub fn link_agent_memory(
    project_name: &str,
    agent_name: &str,
    source_path: PathBuf,
) -> Result<PathBuf> {
    let source_path = expand_tilde(source_path);
    let hub_dir = super::project::memory_hub_dir(project_name)?;
    
    // Ensure project hub exists
    super::project::ensure_project_dirs(project_name)?;

    let file_name = source_path.file_name()
        .ok_or_else(|| anyhow::anyhow!("invalid source path: no filename"))?;
    
    // Physical file in the hub
    let hub_path = hub_dir.join(format!("{}_{}", agent_name, file_name.to_string_lossy()));

    // 1. If hub file doesn't exist, move source to hub
    if !hub_path.exists() {
        if source_path.exists() {
            tracing::info!(
                "migrating memory from {} to hub {}", 
                source_path.display(), 
                hub_path.display()
            );
            fs::rename(&source_path, &hub_path)
                .with_context(|| format!("failed to move {} to hub", source_path.display()))?;
        } else {
            // Source doesn't exist either, create empty hub file
            fs::write(&hub_path, "# Alor Memory Hub\n")?;
        }
    }

    // 2. Ensure source_path is a symlink back to hub_path
    #[cfg(unix)]
    {
        if source_path.exists() {
            let metadata = fs::symlink_metadata(&source_path)?;
            if metadata.is_symlink() {
                // Already a symlink, verify target
                let target = fs::read_link(&source_path)?;
                if target == hub_path {
                    return Ok(hub_path);
                }
                // Target is different, delete old symlink
                fs::remove_file(&source_path)?;
            } else {
                // Physical file exists (collision), back it up
                let backup = source_path.with_extension("alor_backup");
                tracing::warn!(
                    "collision at {}: backing up to {}", 
                    source_path.display(), 
                    backup.display()
                );
                fs::rename(&source_path, &backup)?;
            }
        }

        // Create the symlink
        symlink(&hub_path, &source_path)
            .with_context(|| format!("failed to symlink {} -> {}", source_path.display(), hub_path.display()))?;
    }

    Ok(hub_path)
}
