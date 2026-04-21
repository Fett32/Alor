//! Build script for `alor-wrapper`. Stamps the current build timestamp
//! and the absolute path to `wrapper/src/` into the compiled binary as
//! rustc environment variables, so the runtime can compare source
//! mtimes to the build timestamp and warn on stale binaries.
//!
//! T6 / T4 diagnosis (a5060a1f): the T2 pane-capture ship landed code
//! + passed unit tests but silently didn't run in production because
//! `target/{debug,release}/alor-wrapper` was never rebuilt. The
//! symptom (cursor-alor task-completes with null summary/details)
//! looked like a framing bug for a full diagnosis round before it
//! traced back to "stale binary." The runtime guard in
//! `src/staleness.rs` catches that class-of-bug at launch time.
//!
//! No `cargo:rerun-if-changed=...` directive: emitting any one of
//! those overrides cargo's default "rerun on any package-file change"
//! behavior, and the default is exactly what we want — the timestamp
//! should refresh whenever the wrapper is actually rebuilt. Emitting
//! no rerun-if-* keeps the semantics aligned without maintenance.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Unix seconds at build-script execution. Granularity of 1s matches
    // the filesystem mtime granularity we compare against at runtime;
    // using ns would just overflow the drift-tolerance window in ways
    // that trip filesystem precision boundaries.
    let build_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=WRAPPER_BUILD_TIMESTAMP_SECS={}", build_secs);

    // Absolute path to the src/ dir at build time. Meaningful only when
    // the built binary runs on the same host — Alor's single-box-today
    // assumption. On a hypothetical distributed install this path
    // won't exist at runtime and the staleness check silently skips
    // (returns SourceDirUnavailable). If we ever go distributed, swap
    // in a compiled-in source-hash comparison instead.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is always set by cargo when running build scripts");
    println!("cargo:rustc-env=WRAPPER_SOURCE_DIR={}/src", manifest_dir);
}
