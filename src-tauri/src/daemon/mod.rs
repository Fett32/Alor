pub mod config;
pub mod project;
pub mod session;
pub mod settings;
pub mod state;
pub mod memory;

/// Test-only helpers shared across the daemon submodules.
///
/// Currently hosts `XDG_LOCK`: a single process-wide mutex every test
/// that mutates `XDG_DATA_HOME` must hold for the duration of its
/// run. Without a single shared mutex, each `#[cfg(test)] mod tests`
/// submodule would spin up its OWN local static and parallel cargo
/// test runs across modules would still race each other (the locks
/// don't mutually exclude — they're different Mutex instances).
#[cfg(test)]
pub(crate) mod test_env {
    pub(crate) static XDG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
