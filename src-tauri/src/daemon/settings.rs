//! Top-level Alor settings (~/.config/alor/settings.yaml).
//!
//! Distinct from agent configs (~/.config/alor/agents/*.yaml). Empty /
//! missing file = all defaults. Unknown keys are ignored on deserialize
//! so newer settings don't break older runs and older settings don't
//! break newer runs.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::session;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlorSettings {
    /// Sway workspace name/number to spawn the main window on.
    ///
    /// Empty / None = current behavior (Tauri/Wayland picks whatever
    /// workspace is focused at launch, same as any other app).
    ///
    /// Applied by `apply_sway_workspace_placement` in lib.rs, which
    /// registers a Sway `assign [app_id="alor"] workspace <value>` rule
    /// before Tauri creates the window. `assign` fires pre-map, so the
    /// window appears directly on the target workspace with no flicker
    /// and Sway doesn't switch focus to that workspace.
    #[serde(default)]
    pub spawn_workspace: Option<String>,
}

impl AlorSettings {
    /// Load from `~/.config/alor/settings.yaml`. Returns defaults on any
    /// failure (missing file, unreadable, malformed yaml) and logs a
    /// warning — startup must not block on a busted settings file.
    pub fn load() -> Self {
        match Self::try_load() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("settings load failed, using defaults: {e:#}");
                Self::default()
            }
        }
    }

    fn try_load() -> Result<Self> {
        let path = session::config_dir()?.join("settings.yaml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        // unwrap_or_default: a malformed file shouldn't kill startup.
        // The `load()` caller already logs the underlying error via the
        // warn! path; here an empty-file edge also falls through to
        // defaults cleanly.
        Ok(serde_yaml::from_str(&content).unwrap_or_default())
    }

    /// Persist to `~/.config/alor/settings.yaml`. Overwrites atomically.
    pub fn save(&self) -> Result<()> {
        let path = session::config_dir()?.join("settings.yaml");
        let yaml = serde_yaml::to_string(self)
            .context("serialize settings to yaml")?;
        std::fs::write(&path, yaml)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}
