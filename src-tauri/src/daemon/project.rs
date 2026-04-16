/// Project profiles: metadata about projects the orchestrator manages.
///
/// Stored at ~/.local/share/alor/projects/<name>.yaml
/// Only the orchestrator writes these. Agents read them via pointers.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::session;

// ---------------------------------------------------------------------------
// Project profile
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectProfile {
    /// Project name (matches filename stem).
    pub name: String,
    /// Short description of the project.
    #[serde(default)]
    pub description: String,
    /// Tech stack (e.g. ["rust", "tauri", "javascript"]).
    #[serde(default)]
    pub stack: Vec<String>,
    /// Root directory of the project.
    #[serde(default)]
    pub root_dir: Option<String>,
    /// Key files the orchestrator should know about.
    #[serde(default)]
    pub key_files: Vec<String>,
    /// Paths to relevant documentation (Obsidian, READMEs, etc.).
    #[serde(default)]
    pub doc_paths: Vec<String>,
    /// Path to Claude memory index, if applicable.
    #[serde(default)]
    pub memory_index: Option<String>,
    /// Path to the Alor memory hub directory for this project.
    #[serde(default)]
    pub memory_hub: Option<String>,
    /// Free-form notes from the orchestrator.
    #[serde(default)]
    pub notes: Vec<String>,
}

impl ProjectProfile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            stack: Vec::new(),
            root_dir: None,
            key_files: Vec::new(),
            doc_paths: Vec::new(),
            memory_index: None,
            memory_hub: None,
            notes: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Get the projects directory path.
fn projects_dir() -> Result<PathBuf> {
    let data = session::data_dir()?;
    let dir = data.join("projects");
    Ok(dir)
}

/// Get the memory hub directory for a specific project.
pub fn memory_hub_dir(project_name: &str) -> Result<PathBuf> {
    let data = session::data_dir()?;
    let dir = data.join("hubs").join(project_name);
    Ok(dir)
}

/// Ensure the projects directory exists.
pub fn ensure_projects_dir() -> Result<PathBuf> {
    let dir = projects_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}

/// Ensure project-specific directories (hub, etc) exist.
pub fn ensure_project_dirs(project_name: &str) -> Result<()> {
    let hub = memory_hub_dir(project_name)?;
    std::fs::create_dir_all(&hub)
        .with_context(|| format!("create memory hub {}", hub.display()))?;
    Ok(())
}

/// Load a project profile by name.
pub fn load_profile(name: &str) -> Result<Option<ProjectProfile>> {
    let dir = projects_dir()?;
    let path = dir.join(format!("{name}.yaml"));

    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let profile: ProjectProfile = serde_yaml::from_str(&content)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(profile))
}

/// Save a project profile.
pub fn save_profile(profile: &ProjectProfile) -> Result<PathBuf> {
    let dir = ensure_projects_dir()?;
    let path = dir.join(format!("{}.yaml", profile.name));

    let content = serde_yaml::to_string(profile)
        .context("serialize project profile")?;
    std::fs::write(&path, content)
        .with_context(|| format!("write {}", path.display()))?;

    tracing::info!(project = %profile.name, "project profile saved to {}", path.display());
    Ok(path)
}

/// List all project profiles.
pub fn list_profiles() -> Result<Vec<ProjectProfile>> {
    let dir = projects_dir()?;
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut profiles = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .with_context(|| format!("read {}", dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }

        match load_one_profile(&path) {
            Ok(p) => profiles.push(p),
            Err(e) => tracing::warn!("failed to load {}: {e:#}", path.display()),
        }
    }

    Ok(profiles)
}

fn load_one_profile(path: &Path) -> Result<ProjectProfile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let profile: ProjectProfile = serde_yaml::from_str(&content)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(profile)
}

