//! Startup-time staleness guard: compare the wrapper binary's build
//! timestamp (stamped by `build.rs`) against the mtimes of every
//! `.rs` file under its source tree; warn if any source is newer than
//! the binary.
//!
//! ## Motivation
//!
//! T6 (filed by T4 diagnosis `a5060a1f`). The T2 pane-capture ship
//! landed code + passed `cargo test` but silently didn't run in
//! production because `target/{debug,release}/alor-wrapper` was
//! never rebuilt. The symptom (cursor-alor task-completes with null
//! summary/details) looked like a framing/preamble bug until a full
//! diagnosis round traced it back to the stale binary.
//!
//! Any time `cargo test` is run without `cargo build` — or the
//! inverse on a wrapper-local edit session — the same class-of-bug
//! recurs. This guard surfaces it at launch time instead of letting
//! it ship broken.
//!
//! ## Design (option C.1 from T4)
//!
//! Warn only, never refuse to start. During routine dev you edit a
//! source file and relaunch the wrapper mid-change all the time; a
//! hard gate would nag constantly. A warn is noisy enough to catch
//! attention at log review but doesn't disrupt active debugging.
//!
//! Options C.2 (gate at daemon spawn) and C.3 (CI-side) were rejected
//! per the T6 brief — C.2's false-positive rate during dev is too
//! high, and Alor has no CI today for C.3 to attach to.
//!
//! ## Single-box caveat
//!
//! The runtime compares mtimes of source files using a build-time-
//! embedded absolute path. On a distributed install where the binary
//! runs on a different host than `wrapper/src/` lives, that path
//! won't exist and the check silently skips (`SourceDirUnavailable`).
//! Alor is single-box today so this is fine; if that changes, swap
//! to a compiled-in source-hash comparison (embed `sha256(src/*.rs)`
//! at build time, re-hash nothing at runtime — the staleness we
//! care about is "binary was built from different source than what's
//! on disk," and a hash is strictly stronger than mtime for that).

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Tolerance (seconds) allowed between `build_ts` and newest source
/// mtime before we call the binary stale. 2 s covers filesystem mtime
/// granularity (1 s on ext4 / HFS+, higher on FAT) plus the small
/// window between build-script execution and the final linker write.
/// The motivating T4 drift was minutes-to-hours, so a 2 s floor
/// catches real cases without flapping on the boundary.
const DRIFT_TOLERANCE_SECS: u64 = 2;

/// Result of a staleness comparison. The caller dispatches on this
/// to decide logging level — warn on `Stale`, debug on
/// `SourceDirUnavailable`, silent on `Fresh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StalenessReport {
    /// Binary build timestamp is >= the newest source file mtime
    /// (within `DRIFT_TOLERANCE_SECS`). Nothing to report.
    Fresh,
    /// Source directory doesn't exist or isn't readable at runtime —
    /// distributed install or relocated binary. Not a staleness
    /// signal; the check simply isn't applicable here.
    SourceDirUnavailable(PathBuf),
    /// Some source file is newer than the binary. `drift_secs` is
    /// the positive distance `newest_mtime - build_ts`; callers can
    /// use it to format a human-readable "last built Ns ago" line.
    Stale {
        newest_file: PathBuf,
        newest_mtime_secs: u64,
        build_ts_secs: u64,
        drift_secs: u64,
    },
}

/// Walk `source_dir` recursively for `*.rs` files, find the newest
/// mtime, and compare to `build_ts_secs`. Pure and side-effect-free
/// (reads the filesystem but doesn't modify it); unit-tested.
///
/// Recursive rather than flat because `wrapper/src/` may sprout
/// subdirectories in future (the tauri-side server.rs refactor
/// already did this for `src-tauri/src/wrapper/server/{mod,
/// connection,routing}.rs`). A flat walk would silently bypass the
/// check on any file added under a new submodule dir.
///
/// Walk errors (permissions, I/O) degrade to `SourceDirUnavailable`
/// rather than `Fresh` — a partial walk could miss the newest file
/// and give a false-negative, so bailing out is safer than
/// pretending everything's fine.
pub fn check_staleness(build_ts_secs: u64, source_dir: &Path) -> StalenessReport {
    if !source_dir.exists() {
        return StalenessReport::SourceDirUnavailable(source_dir.to_path_buf());
    }

    let newest = match walk_newest_rs(source_dir) {
        Ok(n) => n,
        Err(_) => return StalenessReport::SourceDirUnavailable(source_dir.to_path_buf()),
    };

    // Empty source dir: no files to compare. Treat as Fresh — there's
    // nothing we can warn about, and the most likely real-world
    // cause (someone swapped the dir without swapping the binary) is
    // indistinguishable at this layer from "new crate, no sources yet."
    let Some((newest_file, newest_mtime_secs)) = newest else {
        return StalenessReport::Fresh;
    };

    let drift = newest_mtime_secs.saturating_sub(build_ts_secs);
    if drift > DRIFT_TOLERANCE_SECS {
        StalenessReport::Stale {
            newest_file,
            newest_mtime_secs,
            build_ts_secs,
            drift_secs: drift,
        }
    } else {
        StalenessReport::Fresh
    }
}

/// Recursive walk collecting the newest `.rs` mtime + path. I/O
/// errors propagate so the caller can downgrade to
/// `SourceDirUnavailable` (false-negatives are strictly worse than
/// false-positives for this guard).
fn walk_newest_rs(dir: &Path) -> std::io::Result<Option<(PathBuf, u64)>> {
    let mut newest: Option<(PathBuf, u64)> = None;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let md = entry.metadata()?;
        if md.is_dir() {
            if let Some(sub) = walk_newest_rs(&path)? {
                if newest.as_ref().map_or(true, |(_, s)| sub.1 > *s) {
                    newest = Some(sub);
                }
            }
        } else if md.is_file()
            && path.extension().and_then(|s| s.to_str()) == Some("rs")
        {
            let mtime = md
                .modified()?
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if newest.as_ref().map_or(true, |(_, s)| mtime > *s) {
                newest = Some((path, mtime));
            }
        }
    }
    Ok(newest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::time::{Duration, SystemTime};

    /// Create `dir/name` with its mtime set to `ts_secs` past
    /// `UNIX_EPOCH`. Uses `File::set_modified` (stable since 1.75).
    fn write_file_with_mtime(dir: &Path, name: &str, ts_secs: u64) {
        let path = dir.join(name);
        let mut f = File::create(&path).expect("create fixture");
        f.write_all(b"// fixture\n").expect("write fixture");
        let t = UNIX_EPOCH + Duration::from_secs(ts_secs);
        f.set_modified(t).expect("set mtime");
    }

    /// Unique tmp dir per test + per process so parallel `cargo test`
    /// runs don't collide on the same fixture path.
    fn tmpdir(tag: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "alor-staleness-{tag}-{}-{}",
            std::process::id(),
            now
        ));
        fs::create_dir_all(&base).expect("mkdir tmp");
        base
    }

    #[test]
    fn fresh_when_build_strictly_newer_than_every_source() {
        // Baseline: a just-rebuilt binary. Every source was modified
        // well before the build timestamp → Fresh.
        let dir = tmpdir("fresh");
        let build_ts = 1_000_000;
        write_file_with_mtime(&dir, "main.rs", build_ts - 3600);
        write_file_with_mtime(&dir, "detector.rs", build_ts - 60);
        assert_eq!(check_staleness(build_ts, &dir), StalenessReport::Fresh);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_when_any_source_newer_than_build() {
        // T6 core case: source was edited after the last build.
        // Drift of 300 s is well past the 2 s tolerance → Stale.
        // Also verifies the Stale variant reports the NEWEST file
        // specifically (not the first one encountered).
        let dir = tmpdir("stale");
        let build_ts = 1_000_000;
        write_file_with_mtime(&dir, "main.rs", build_ts - 3600);
        write_file_with_mtime(&dir, "detector.rs", build_ts + 300);
        match check_staleness(build_ts, &dir) {
            StalenessReport::Stale {
                newest_file,
                newest_mtime_secs,
                build_ts_secs,
                drift_secs,
            } => {
                assert!(
                    newest_file.ends_with("detector.rs"),
                    "newest file must be the one past the build ts: {newest_file:?}"
                );
                assert_eq!(newest_mtime_secs, build_ts + 300);
                assert_eq!(build_ts_secs, build_ts);
                assert_eq!(drift_secs, 300);
            }
            other => panic!("expected Stale, got {other:?}"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fresh_within_drift_tolerance() {
        // 1-second drift is inside the 2 s tolerance (filesystem
        // granularity + build-to-link window). Must NOT fire; false
        // positives here erode the warn's signal-to-noise and the
        // whole guard gets muted by operators.
        let dir = tmpdir("tolerance-in");
        let build_ts = 1_000_000;
        write_file_with_mtime(&dir, "main.rs", build_ts + 1);
        write_file_with_mtime(&dir, "detector.rs", build_ts + 2);
        assert_eq!(check_staleness(build_ts, &dir), StalenessReport::Fresh);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_strictly_past_tolerance_boundary() {
        // 3 s drift is the first value that trips the guard past the
        // 2 s tolerance. Lock the boundary so a drive-by change to
        // DRIFT_TOLERANCE_SECS surfaces loudly.
        let dir = tmpdir("tolerance-out");
        let build_ts = 1_000_000;
        write_file_with_mtime(&dir, "main.rs", build_ts + 3);
        assert!(matches!(
            check_staleness(build_ts, &dir),
            StalenessReport::Stale { drift_secs: 3, .. }
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ignores_non_rust_files() {
        // A fresh Cargo.toml or README.md next to the sources must
        // not fire the guard. Only `.rs` extensions participate.
        // (Cargo.toml changes DO trigger a rebuild via the default
        // build-script rerun rules, so this doesn't mask the real
        // signal — the rebuild updates the stamp and Fresh is
        // still correct after.)
        let dir = tmpdir("nonrust");
        let build_ts = 1_000_000;
        write_file_with_mtime(&dir, "main.rs", build_ts - 60);
        // Newer-than-build `.txt` → must still be Fresh.
        write_file_with_mtime(&dir, "notes.txt", build_ts + 3600);
        write_file_with_mtime(&dir, "Cargo.toml", build_ts + 3600);
        assert_eq!(check_staleness(build_ts, &dir), StalenessReport::Fresh);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walks_recursively_into_subdirs() {
        // wrapper/src is flat today but the tauri-side refactor
        // already split `src-tauri/src/wrapper/server.rs` into a
        // sibling dir. If wrapper ever does the same, a flat walk
        // would silently bypass the check for anything under the
        // new submodule. Recursive walk catches it.
        let dir = tmpdir("recursive");
        let build_ts = 1_000_000;
        write_file_with_mtime(&dir, "main.rs", build_ts - 60);
        let sub = dir.join("nested");
        fs::create_dir(&sub).expect("mkdir sub");
        write_file_with_mtime(&sub, "inner.rs", build_ts + 500);
        match check_staleness(build_ts, &dir) {
            StalenessReport::Stale {
                newest_file,
                drift_secs,
                ..
            } => {
                assert!(newest_file.ends_with("inner.rs"));
                assert_eq!(drift_secs, 500);
            }
            other => panic!("expected Stale from recursive walk, got {other:?}"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn source_dir_unavailable_when_missing() {
        // Distributed install / relocated binary: source dir at
        // build-time path doesn't exist on the runtime host. Must
        // return SourceDirUnavailable so the caller can log at
        // `debug!` rather than `warn!` — an absent source tree is
        // not a staleness signal, the check just isn't applicable.
        let missing = PathBuf::from("/nonexistent/alor/wrapper/src");
        assert!(matches!(
            check_staleness(1_000_000, &missing),
            StalenessReport::SourceDirUnavailable(_)
        ));
    }

    #[test]
    fn empty_source_dir_is_fresh() {
        // No `.rs` files → nothing to compare. Contract completeness:
        // pick Fresh over Stale because an empty tree doesn't carry
        // any "source newer than binary" evidence. Won't hit in
        // practice but keeps the function total.
        let dir = tmpdir("empty");
        assert_eq!(
            check_staleness(1_000_000, &dir),
            StalenessReport::Fresh
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_ts_zero_is_fresh_when_sources_not_past_tolerance() {
        // If build.rs somehow failed to stamp (build_ts_secs = 0),
        // main.rs short-circuits before calling this function, but
        // the function itself must stay well-defined for that
        // input: all positive mtimes are past a zero build ts →
        // every sane tree would look Stale. This test locks the
        // current semantics against a refactor that "helpfully"
        // special-cases zero; the main.rs caller is responsible for
        // the short-circuit, not this helper.
        let dir = tmpdir("zero-build-ts");
        write_file_with_mtime(&dir, "main.rs", 3_600);
        assert!(matches!(
            check_staleness(0, &dir),
            StalenessReport::Stale { drift_secs: 3600, .. }
        ));
        fs::remove_dir_all(&dir).ok();
    }
}
