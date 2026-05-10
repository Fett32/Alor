//! Shared helpers for integration tests under `src-tauri/tests/`.
//!
//! Rust treats `tests/common/mod.rs` (note the subdirectory + mod.rs) as
//! a shared module, not its own test binary. Any integration test file
//! that needs these helpers declares `mod common;` at the top and then
//! uses `common::…` paths.
//!
//! Currently exposes a tmux-runnability probe — see `tmux_runnable` for
//! why the obvious "`tmux -V` works" check is insufficient.

#![allow(dead_code)] // Helpers here may be used by only a subset of the
                    // integration test binaries. Unused-ones per-binary
                    // would otherwise warn.

use tokio::process::Command;
use tokio::sync::OnceCell;

/// Outcome of the tmux-runnability probe.
#[derive(Clone, Debug)]
pub enum TmuxProbe {
    /// Binary is on PATH AND we successfully created + killed a
    /// throwaway session in this environment.
    Runnable,
    /// Probe failed — binary missing, non-zero exit, denied socket
    /// access (sandbox), or spawn error. `String` carries the reason
    /// for the skip message.
    Unavailable(String),
}

// Cached probe result. Session create/kill is comparatively expensive
// and a single outcome applies to every test in the binary — re-probing
// per test would just be waste.
static TMUX_PROBE: OnceCell<TmuxProbe> = OnceCell::const_new();

/// Probe whether `tmux` can actually drive a session in the current
/// environment, not just whether the binary resolves. The common
/// sandbox failure mode is `tmux -V` succeeding (binary present) but
/// `tmux new-session` failing with "Operation not permitted" because
/// the sandbox denies access to `/tmp/tmux-<uid>/`. That shape breaks
/// any test that only guards on `tmux -V`.
///
/// Result is memoized for the lifetime of the test binary. First call
/// runs the probe; subsequent calls return the cached outcome.
pub async fn tmux_runnable() -> &'static TmuxProbe {
    TMUX_PROBE
        .get_or_init(|| async {
            // 1. Binary resolution.
            let vout = match Command::new("tmux").arg("-V").output().await {
                Ok(o) => o,
                Err(e) => {
                    return TmuxProbe::Unavailable(format!(
                        "tmux -V failed to spawn: {e}"
                    ));
                }
            };
            if !vout.status.success() {
                return TmuxProbe::Unavailable(format!(
                    "tmux -V exited {}: {}",
                    vout.status,
                    String::from_utf8_lossy(&vout.stderr).trim()
                ));
            }

            // 2. Disposable session probe. `true` exits immediately, so
            //    the session naturally dies after one tick — but we
            //    still explicitly kill it below in case tmux keeps it
            //    alive as a detached shell (-d semantics vary by
            //    version).
            let probe_name = format!(
                "alor-tmux-probe-{}",
                uuid::Uuid::new_v4().simple()
            );
            let sout = match Command::new("tmux")
                .args([
                    "new-session", "-d", "-s", &probe_name, "true",
                ])
                .output()
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    return TmuxProbe::Unavailable(format!(
                        "tmux new-session failed to spawn: {e}"
                    ));
                }
            };
            if !sout.status.success() {
                return TmuxProbe::Unavailable(format!(
                    "tmux new-session failed: {}",
                    String::from_utf8_lossy(&sout.stderr).trim()
                ));
            }

            // Best-effort cleanup. If it's already gone (because `true`
            // exited), kill-session returns non-zero; we ignore.
            let exact = format!("={probe_name}");
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &exact])
                .output()
                .await;

            TmuxProbe::Runnable
        })
        .await
}

/// Convenience: return true if tmux is runnable, else emit a clear
/// `eprintln!` skip message tagged with the caller-supplied label and
/// return false. Usage pattern:
///
/// ```ignore
/// if !common::skip_if_tmux_not_runnable("my_test").await { return; }
/// ```
///
/// The label shows up in `cargo test` output so the skip reason is
/// traceable to a specific test when something goes sideways.
pub async fn skip_if_tmux_not_runnable(test_label: &str) -> bool {
    match tmux_runnable().await {
        TmuxProbe::Runnable => true,
        TmuxProbe::Unavailable(reason) => {
            eprintln!(
                "[{test_label}] skipping: tmux not runnable in this \
                 environment ({reason})"
            );
            false
        }
    }
}
