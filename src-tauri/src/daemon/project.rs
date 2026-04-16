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

/// Validate a project name is safe for use as a path component.
/// Rejects empty strings, names longer than 64 chars, and any characters
/// outside alphanumeric / dash / underscore. Prevents path traversal.
pub fn validate_project_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("project name is empty");
    }
    if name.len() > 64 {
        anyhow::bail!("project name too long (max 64 chars): {name:?}");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        anyhow::bail!("invalid project name (allowed: alphanumeric, dash, underscore): {name:?}");
    }
    Ok(())
}

/// Get the memory hub directory for a specific project.
pub fn memory_hub_dir(project_name: &str) -> Result<PathBuf> {
    validate_project_name(project_name)?;
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
    validate_project_name(name)?;
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

/// Save a project profile. Writes to a temp file then atomically renames.
pub fn save_profile(profile: &ProjectProfile) -> Result<PathBuf> {
    validate_project_name(&profile.name)?;
    let dir = ensure_projects_dir()?;
    let path = dir.join(format!("{}.yaml", profile.name));
    let tmp = dir.join(format!(".{}.yaml.tmp", profile.name));

    let content = serde_yaml::to_string(profile)
        .context("serialize project profile")?;
    std::fs::write(&tmp, content)
        .with_context(|| format!("write temp {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;

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

/// Build a TASK BRIEF that will be prepended to a task description when
/// the task is dispatched to an agent. Gives the wrapper a stable reference
/// frame (project, key files, docs) without needing to look it up itself.
pub fn build_task_brief(
    profile: &ProjectProfile,
    task_title: &str,
    task_description: &str,
) -> String {
    let mut buf = String::new();

    buf.push_str("## Project\n");
    buf.push_str(&profile.name);
    if !profile.description.is_empty() {
        buf.push_str(" — ");
        buf.push_str(&profile.description);
    }
    buf.push('\n');

    if let Some(root) = &profile.root_dir {
        buf.push_str("Root: ");
        buf.push_str(root);
        buf.push('\n');
    }

    if !profile.key_files.is_empty() {
        buf.push_str("\n## Key files\n");
        for kf in &profile.key_files {
            buf.push_str("- ");
            buf.push_str(kf);
            buf.push('\n');
        }
    }

    if !profile.doc_paths.is_empty() {
        buf.push_str("\n## Documentation\n");
        for dp in &profile.doc_paths {
            buf.push_str("- ");
            buf.push_str(dp);
            buf.push('\n');
        }
    }

    buf.push_str("\n## Task\n");
    buf.push_str(task_title);
    buf.push_str("\n\n");
    buf.push_str(task_description);

    buf
}

fn load_one_profile(path: &Path) -> Result<ProjectProfile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let profile: ProjectProfile = serde_yaml::from_str(&content)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(profile)
}

