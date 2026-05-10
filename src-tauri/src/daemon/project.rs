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

/// Merge a `cli.project.save`-shaped payload into an existing profile.
///
/// This is the merge half of the save handler, pulled out as a pure
/// function so the merge semantics are unit-testable without a
/// `State<'_, _>` scaffold (the handler in
/// `wrapper/server/routing.rs` is the only call site).
///
/// Semantics:
///   - `existing = Some(p)`: start from the on-disk profile; every
///     field not explicitly set in the payload is preserved. This
///     is the critical property the 2026-04-20 fix restores —
///     pre-fix the handler built a FRESH `ProjectProfile::new()`
///     and merged the payload into it, which silently dropped
///     `memory_hub`, `notes`, and any other field not in
///     `CliProjectSave`.
///   - `existing = None`: start from `ProjectProfile::new(name)`
///     (all defaults). The merge then seeds whatever the payload
///     provides; unspecified fields land at their defaults. This
///     matches the "creating a fresh project" case.
///
/// Per-field rules (`Option::Some` = overwrite, `None` = preserve):
///   - `description: Some(s)` → set; `None` → keep existing.
///   - `root_dir: Some(s)` → set to `Some(s)`; `None` → keep
///     existing (no way to explicitly clear via this API, deliberate).
///   - `stack`, `key_files`, `doc_paths: Some(v)` → overwrite with
///     `v` (including `Some(vec![])` to clear); `None` → preserve.
///   - `memory_index: Some(s)` → set to `Some(s)`; `None` → preserve.
///   - `memory_hub`, `notes`: NEVER touched by this function — they
///     ride along on `existing` verbatim. If a caller ever wants
///     to update them via save, this function needs new parameters.
pub fn merge_profile(
    existing: Option<ProjectProfile>,
    name: &str,
    description: Option<String>,
    root_dir: Option<String>,
    stack: Option<Vec<String>>,
    key_files: Option<Vec<String>>,
    doc_paths: Option<Vec<String>>,
    memory_index: Option<String>,
) -> ProjectProfile {
    let mut profile = existing.unwrap_or_else(|| ProjectProfile::new(name));
    // Name always comes from the caller — if the payload targets a
    // different name than the loaded profile (e.g. rename-in-place),
    // the payload wins. Caller is responsible for name validation;
    // `save_profile` re-validates anyway.
    profile.name = name.to_string();
    if let Some(desc) = description {
        profile.description = desc;
    }
    if let Some(rd) = root_dir {
        profile.root_dir = Some(rd);
    }
    if let Some(s) = stack {
        profile.stack = s;
    }
    if let Some(kf) = key_files {
        profile.key_files = kf;
    }
    if let Some(dp) = doc_paths {
        profile.doc_paths = dp;
    }
    if let Some(mi) = memory_index {
        profile.memory_index = Some(mi);
    }
    profile
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an existing profile with ALL fields populated — tests
    /// assert which of those survive a payload with only a subset.
    fn populated_profile() -> ProjectProfile {
        ProjectProfile {
            name: "alor".to_string(),
            description: "existing description".to_string(),
            stack: vec!["rust".to_string(), "tauri".to_string()],
            root_dir: Some("/home/fett/Projects/Alor".to_string()),
            key_files: vec!["src/main.rs".to_string()],
            doc_paths: vec!["~/Obsidian/Alor".to_string()],
            memory_index: Some("/old/path/to/memory".to_string()),
            memory_hub: Some("/home/fett/.local/share/alor/hubs/alor".to_string()),
            notes: vec!["note 1".to_string(), "note 2".to_string()],
        }
    }

    #[test]
    fn merge_preserves_memory_hub_when_payload_omits_it() {
        // The motivating bug: pre-fix, save_profile handler built
        // a fresh ProjectProfile, which has memory_hub=None by
        // default. Any round-trip through the save handler erased
        // the hub path. Post-fix, merge_profile starts from the
        // existing profile so memory_hub stays populated unless
        // the caller explicitly touches it.
        let existing = populated_profile();
        let merged = merge_profile(
            Some(existing.clone()),
            "alor",
            Some("new description".to_string()),
            None, None, None, None, None,
        );
        assert_eq!(merged.memory_hub, existing.memory_hub);
        assert_eq!(merged.description, "new description");
    }

    #[test]
    fn merge_preserves_notes_when_payload_omits_them() {
        // Same shape as memory_hub: notes isn't in CliProjectSave,
        // so the handler can never touch it, so the merge must
        // preserve it.
        let existing = populated_profile();
        let merged = merge_profile(
            Some(existing.clone()),
            "alor",
            None,
            Some("/new/root".to_string()),
            None, None, None, None,
        );
        assert_eq!(merged.notes, existing.notes);
        assert_eq!(merged.root_dir, Some("/new/root".to_string()));
    }

    #[test]
    fn merge_with_all_none_preserves_every_mergeable_field() {
        // Degenerate payload — only `name`, everything else None.
        // Result must be byte-identical to `existing` except for
        // `name` (which always comes from the payload).
        let existing = populated_profile();
        let merged = merge_profile(
            Some(existing.clone()),
            "alor",
            None, None, None, None, None, None,
        );
        assert_eq!(merged.name, existing.name);
        assert_eq!(merged.description, existing.description);
        assert_eq!(merged.stack, existing.stack);
        assert_eq!(merged.root_dir, existing.root_dir);
        assert_eq!(merged.key_files, existing.key_files);
        assert_eq!(merged.doc_paths, existing.doc_paths);
        assert_eq!(merged.memory_index, existing.memory_index);
        assert_eq!(merged.memory_hub, existing.memory_hub);
        assert_eq!(merged.notes, existing.notes);
    }

    #[test]
    fn merge_overwrites_only_provided_fields() {
        // Payload sets description + stack; everything else preserved.
        let existing = populated_profile();
        let merged = merge_profile(
            Some(existing.clone()),
            "alor",
            Some("new description".to_string()),
            None,
            Some(vec!["rust".to_string(), "python".to_string()]),
            None, None, None,
        );
        assert_eq!(merged.description, "new description");
        assert_eq!(merged.stack, vec!["rust".to_string(), "python".to_string()]);
        // Preserved:
        assert_eq!(merged.root_dir, existing.root_dir);
        assert_eq!(merged.key_files, existing.key_files);
        assert_eq!(merged.doc_paths, existing.doc_paths);
        assert_eq!(merged.memory_index, existing.memory_index);
        assert_eq!(merged.memory_hub, existing.memory_hub);
        assert_eq!(merged.notes, existing.notes);
    }

    #[test]
    fn merge_empty_vec_explicitly_clears() {
        // `Some(vec![])` IS a meaningful payload — it means "clear
        // this field". Contrast with `None` = "preserve". The
        // current UI shape doesn't distinguish these visually, but
        // the wire protocol does.
        let existing = populated_profile();
        let merged = merge_profile(
            Some(existing),
            "alor",
            None, None,
            Some(vec![]),  // clear stack
            Some(vec![]),  // clear key_files
            None, None,
        );
        assert!(merged.stack.is_empty(), "empty stack payload clears");
        assert!(merged.key_files.is_empty(), "empty key_files payload clears");
    }

    #[test]
    fn merge_with_no_existing_creates_from_defaults() {
        // Fresh project case — no on-disk profile yet, payload
        // seeds the fields it specifies, everything else defaults.
        let merged = merge_profile(
            None,
            "brand-new",
            Some("desc".to_string()),
            Some("/home/fett/Projects/NewThing".to_string()),
            None, None, None, None,
        );
        assert_eq!(merged.name, "brand-new");
        assert_eq!(merged.description, "desc");
        assert_eq!(merged.root_dir, Some("/home/fett/Projects/NewThing".to_string()));
        // Defaults elsewhere:
        assert!(merged.stack.is_empty());
        assert!(merged.key_files.is_empty());
        assert!(merged.doc_paths.is_empty());
        assert_eq!(merged.memory_index, None);
        assert_eq!(merged.memory_hub, None);
        assert!(merged.notes.is_empty());
    }

    #[test]
    fn merge_rewrites_name_from_payload() {
        // The payload's `name` always wins — lets callers target a
        // specific file even if the loaded profile somehow has a
        // different `name` field. (Shouldn't happen under normal
        // operation, but a hand-edited yaml could drift.)
        let mut existing = populated_profile();
        existing.name = "old-name".to_string();
        let merged = merge_profile(
            Some(existing),
            "new-name",
            None, None, None, None, None, None,
        );
        assert_eq!(merged.name, "new-name");
    }

    #[test]
    fn merge_memory_index_some_sets_field() {
        let existing = populated_profile();
        let merged = merge_profile(
            Some(existing),
            "alor",
            None, None, None, None, None,
            Some("/new/memory/index".to_string()),
        );
        assert_eq!(merged.memory_index, Some("/new/memory/index".to_string()));
    }

    #[test]
    fn merge_round_trip_through_save_and_load_preserves_memory_hub() {
        // End-to-end integration-ish test: set up a tempdir,
        // point XDG_DATA_HOME at it, save a profile with
        // memory_hub populated, then simulate a UI round-trip
        // (load → save without memory_hub in the payload) via the
        // real merge_profile + save_profile / load_profile code
        // paths. Assert memory_hub survives.
        use tempfile::TempDir;
        let tmp = TempDir::new().expect("tempdir");
        // Serialize to avoid XDG_DATA_HOME races with other tests.
        // Uses the shared lock in crate::daemon::test_env so every
        // test that mutates XDG_DATA_HOME across submodules
        // (currently project.rs + memory.rs) synchronizes against
        // ONE mutex; per-module locks would deadlock-free but NOT
        // mutually exclude, defeating the point.
        let _guard = crate::daemon::test_env::XDG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", tmp.path());

        // Phase 1: seed disk with a populated profile.
        let seeded = populated_profile();
        save_profile(&seeded).expect("seed save");

        // Phase 2: simulate the UI's save — loads existing, merges
        // a payload that sets ONLY description, saves result.
        let on_disk = load_profile("alor").expect("load ok").expect("loaded");
        assert_eq!(
            on_disk.memory_hub, seeded.memory_hub,
            "phase 1: memory_hub must survive seed+load"
        );
        let merged = merge_profile(
            Some(on_disk),
            "alor",
            Some("ui edit".to_string()),
            None, None, None, None, None,
        );
        save_profile(&merged).expect("merged save");

        // Phase 3: reload from disk and assert memory_hub + notes
        // survived the round-trip.
        let reloaded = load_profile("alor")
            .expect("reload ok")
            .expect("reloaded");
        assert_eq!(
            reloaded.memory_hub, seeded.memory_hub,
            "memory_hub lost on round-trip"
        );
        assert_eq!(
            reloaded.notes, seeded.notes,
            "notes lost on round-trip"
        );
        assert_eq!(
            reloaded.description, "ui edit",
            "description should reflect the payload"
        );
        // And the field we didn't touch (stack) survived too.
        assert_eq!(reloaded.stack, seeded.stack);

        // Restore env.
        if let Some(p) = prev {
            std::env::set_var("XDG_DATA_HOME", p);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }
}

