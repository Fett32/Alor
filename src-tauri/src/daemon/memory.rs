/// Memory Hub: manages cross-agent memory synchronization via symlinks
/// and provides the append-write surface for automated distillation.
///
/// Two distinct surfaces live here:
///
/// 1. **`link_agent_memory`** — the original API. Physically moves an
///    agent's private memory file into the project's hub directory and
///    leaves a symlink back at the original path. Preserves read-time
///    compatibility for the owning agent (it still reads from its usual
///    location) while making the file discoverable to any other agent
///    that reads the hub.
///
/// 2. **`append_to_hub_file` + `auto_distill_task_completion`** — the
///    distillation surface added for the gemini-alor audit item #7. Lets
///    the daemon (on task.complete) and the CLI (via `memory.append`)
///    write structured notes into the hub without requiring a full
///    `link_agent_memory`-style rename dance. Bounded file size so a
///    runaway dispatcher can't grow `automation_log.md` unboundedly.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::fs;
use std::io::Write;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::symlink;

// ---------------------------------------------------------------------------
// Append + distillation surface (audit item #7)
// ---------------------------------------------------------------------------

/// Name of the per-project hub file that receives automatic task-complete
/// distillation entries. Each entry is one line appended on every
/// `task.complete` carrying a non-empty summary + non-null project. See
/// `auto_distill_task_completion` for the entry shape.
pub const AUTOMATION_LOG_BASENAME: &str = "automation_log.md";

/// Cap on the on-disk size of `automation_log.md`. When an append would
/// push the file above this, we head-trim (drop the oldest lines) so the
/// log stays a rolling window of the most recent completions. 256 KiB
/// covers months of normal dispatch activity without becoming expensive
/// to read back via `memory_get`; adjust upward if retention turns out
/// to be the bottleneck.
pub const AUTOMATION_LOG_MAX_BYTES: usize = 256 * 1024;

/// Cap on a single `memory_append` payload. Bigger entries should go
/// into their own named hub file via multiple appends, not one giant
/// write that dominates the rolling log. Keeps any single call
/// bounded and predictable.
pub const MEMORY_APPEND_MAX_BYTES: usize = 16 * 1024;

/// Minimum safe basename check, shared across both Rust callsites
/// (the daemon's `cli.memory.append` handler and the internal
/// `auto_distill_task_completion` path) so we only have one definition
/// of "safe" to audit. Mirrors `wrapper::server::routing::is_safe_hub_basename`
/// but kept module-local to avoid dragging the wrapper crate into
/// `daemon::memory`'s import graph. The routing-side version remains
/// the canonical one for the CLI handler; this copy is defense-in-depth
/// for any direct `memory::` callers.
fn is_safe_basename(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return false;
    }
    let p = std::path::Path::new(name);
    let mut comps = p.components();
    matches!(
        (comps.next(), comps.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

/// Append `text` to the named file inside the given project's Memory Hub.
/// Creates the hub directory + file if missing. If the on-disk size
/// would exceed `trim_to_max_bytes`, head-trim the file so the most
/// recent content is preserved. Ensures the appended block ends with
/// exactly one newline so subsequent appends don't run together.
///
/// Errors:
///   - Invalid project name (via `project::memory_hub_dir`).
///   - `file_name` fails the safe-basename check (`.`, `..`, path
///     separators, NUL bytes).
///   - Filesystem error creating the hub dir, opening the file, or
///     performing the trim rewrite.
pub fn append_to_hub_file(
    project: &str,
    file_name: &str,
    text: &str,
    trim_to_max_bytes: Option<usize>,
) -> Result<PathBuf> {
    if !is_safe_basename(file_name) {
        bail!("unsafe hub filename: {:?}", file_name);
    }
    let hub_dir = super::project::memory_hub_dir(project)?;
    super::project::ensure_project_dirs(project)?;
    let hub_path = hub_dir.join(file_name);

    // Normalize the appended block: always ends with exactly one \n.
    let mut block = text.to_string();
    while block.ends_with('\n') {
        block.pop();
    }
    block.push('\n');

    // Head-trim if the post-append size would exceed the cap. We rewrite
    // the file with a tail slice of the existing content + the new block,
    // walking back to a UTF-8 char boundary + line start so we never
    // split a codepoint or leave a dangling partial line at the top.
    if let Some(max_bytes) = trim_to_max_bytes {
        let existing_bytes = match fs::metadata(&hub_path) {
            Ok(m) => m.len() as usize,
            Err(_) => 0,
        };
        let projected = existing_bytes.saturating_add(block.len());
        if projected > max_bytes {
            let existing = fs::read(&hub_path).unwrap_or_default();
            // Target: keep at most `max_bytes - block.len()` bytes of the
            // old content. If the new block alone exceeds the cap (should
            // be guarded at the call site with MEMORY_APPEND_MAX_BYTES,
            // but belt-and-braces here), keep only the block.
            let keep_old = max_bytes.saturating_sub(block.len());
            let start = existing.len().saturating_sub(keep_old);
            let mut kept = &existing[start..];
            // Walk forward to the next line start so we never leave a
            // half-line at the head. Skip past the first \n if one
            // exists within the first few bytes of the kept slice.
            if let Some(nl) = kept.iter().position(|&b| b == b'\n') {
                // If we're already near enough to the start that dropping
                // to the next newline loses too much, stay. In practice
                // automation_log entries are short (<200 B each) so
                // dropping one partial line is fine.
                kept = &kept[nl + 1..];
            }
            let mut new_content = Vec::with_capacity(kept.len() + block.len());
            new_content.extend_from_slice(kept);
            new_content.extend_from_slice(block.as_bytes());
            fs::write(&hub_path, &new_content)
                .with_context(|| format!("rewrite {}", hub_path.display()))?;
            tracing::debug!(
                project,
                file = %file_name,
                trimmed_from = existing_bytes,
                new_size = new_content.len(),
                "memory.append: head-trimmed oversized hub file"
            );
            return Ok(hub_path);
        }
    }

    // Normal append path.
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&hub_path)
        .with_context(|| format!("open-append {}", hub_path.display()))?;
    f.write_all(block.as_bytes())
        .with_context(|| format!("append to {}", hub_path.display()))?;
    Ok(hub_path)
}

/// Format + append a task-complete distillation entry to the project's
/// automation log. Called from the daemon's `task.complete` handler
/// whenever the completing task has a project and a non-empty summary.
/// Cheap: one `append_to_hub_file` call; no LLM involvement.
///
/// Entry shape (single line):
///   `- <iso8601> · <agent_id> · <task_id_short> · <title> — <summary>`
///
/// `task_id_short` is the first 8 hex chars of the task UUID, which is
/// enough to disambiguate in practice and keeps the log scan-friendly
/// at a glance. The full task_id is always recoverable via `task_get`
/// if a reader needs it.
///
/// Best-effort: swallowed errors land as a `warn!` log line so a
/// misconfigured hub directory doesn't fail the task completion. The
/// caller (routing.rs) treats this as fire-and-forget.
pub fn auto_distill_task_completion(
    project: &str,
    agent_id: &str,
    task_id: &Uuid,
    title: &str,
    summary: &str,
) -> Result<()> {
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let task_id_short = &task_id.to_string()[..8];
    // Flatten interior newlines + control bytes so each entry stays
    // exactly one line — otherwise a multi-paragraph summary would
    // break the `-` bullet shape and make the log harder to scan.
    let title_one_line = title.replace(['\n', '\r'], " ").trim().to_string();
    let summary_one_line = summary.replace(['\n', '\r'], " ").trim().to_string();
    let entry = format!(
        "- {timestamp} · {agent_id} · {task_id_short} · {title_one_line} — {summary_one_line}"
    );
    append_to_hub_file(
        project,
        AUTOMATION_LOG_BASENAME,
        &entry,
        Some(AUTOMATION_LOG_MAX_BYTES),
    )?;
    Ok(())
}

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

    // Refuse to pull anything out of ~/.claude/. This API does an
    // `fs::rename` of source_path into the hub and replaces the
    // original with a symlink — that's destructive against the user's
    // shared Claude Code config tree (memory dir, settings, sessions).
    // If a workflow needs to surface MEMORY.md to a worker, copy it
    // into the hub or hand it off via a TASK BRIEF — don't link it.
    if let Some(home) = std::env::var_os("HOME") {
        let claude_root = PathBuf::from(&home).join(".claude");
        if source_path.starts_with(&claude_root) {
            bail!(
                "refusing to link {}: path is under {} (the user's Claude \
                 Code config tree). link_agent_memory does a destructive \
                 rename — copy the file into the hub or pass it via the \
                 task brief instead.",
                source_path.display(),
                claude_root.display()
            );
        }
    }

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use uuid::Uuid;

    /// XDG_DATA_HOME is process-global; parallel cargo test runs would
    /// race on `memory_hub_dir` resolution. Serialize every test that
    /// points XDG at a tmpdir via the shared lock in
    /// `super::super::test_env` so project.rs + memory.rs tests all
    /// wait on the SAME mutex.
    fn with_tmp_xdg<F: FnOnce(&TempDir)>(f: F) {
        let tmp = TempDir::new().expect("tempdir");
        let guard = crate::daemon::test_env::XDG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        f(&tmp);
        if let Some(p) = prev {
            std::env::set_var("XDG_DATA_HOME", p);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        drop(guard);
    }

    #[test]
    fn is_safe_basename_accepts_and_rejects_as_documented() {
        assert!(is_safe_basename("notes.md"));
        assert!(is_safe_basename("automation_log.md"));
        assert!(is_safe_basename("file-with-dashes_and_underscores.txt"));

        assert!(!is_safe_basename(""));
        assert!(!is_safe_basename("."));
        assert!(!is_safe_basename(".."));
        assert!(!is_safe_basename("a/b"));
        assert!(!is_safe_basename("a\\b"));
        assert!(!is_safe_basename("../escape.md"));
        assert!(!is_safe_basename("/abs/path.md"));
        assert!(!is_safe_basename("null\0byte"));
    }

    #[test]
    fn append_creates_new_hub_file_with_trailing_newline() {
        with_tmp_xdg(|_tmp| {
            let path = append_to_hub_file(
                "testproj", "notes.md", "hello world", None,
            )
            .expect("append ok");
            let content = std::fs::read_to_string(&path).expect("read back");
            assert_eq!(content, "hello world\n",
                "fresh file seeded with the provided text + exactly one trailing LF");
        });
    }

    #[test]
    fn append_adds_to_existing_file_and_separates_entries_with_newline() {
        with_tmp_xdg(|_tmp| {
            append_to_hub_file("testproj", "notes.md", "first entry", None)
                .expect("first append");
            let path = append_to_hub_file(
                "testproj", "notes.md", "second entry", None,
            )
            .expect("second append");
            let content = std::fs::read_to_string(&path).expect("read back");
            assert_eq!(
                content, "first entry\nsecond entry\n",
                "two appends land as two lines; no run-together, no double-blank",
            );
        });
    }

    #[test]
    fn append_strips_any_number_of_trailing_newlines_from_input() {
        with_tmp_xdg(|_tmp| {
            append_to_hub_file("testproj", "notes.md", "line\n\n\n", None)
                .expect("append with trailing newlines");
            let path = append_to_hub_file(
                "testproj", "notes.md", "next\n", None,
            )
            .expect("second append");
            let content = std::fs::read_to_string(&path).expect("read back");
            assert_eq!(
                content, "line\nnext\n",
                "caller's trailing newline count is normalized to exactly one",
            );
        });
    }

    #[test]
    fn append_rejects_unsafe_basenames() {
        with_tmp_xdg(|_tmp| {
            let err = append_to_hub_file("testproj", "../escape.md", "data", None)
                .expect_err("must reject `..`");
            assert!(
                format!("{err}").contains("unsafe hub filename"),
                "error message should name the guard: {err}"
            );
            assert!(append_to_hub_file("testproj", "a/b.md", "data", None).is_err());
            assert!(append_to_hub_file("testproj", "", "data", None).is_err());
        });
    }

    #[test]
    fn append_head_trims_when_over_cap() {
        with_tmp_xdg(|_tmp| {
            // Seed a file past the 200-byte cap (deliberately tiny for
            // the test). Appending a small entry should head-trim the
            // oldest content so the file stays bounded.
            let cap = 200;
            // Each line is 20 bytes ("line-NNN payload"), seed 20 lines
            // = 400 bytes, well over the cap.
            for i in 0..20 {
                append_to_hub_file(
                    "testproj",
                    "automation_log.md",
                    &format!("line-{i:03} payload"),
                    Some(cap),
                )
                .expect("seed append");
            }
            let path = crate::daemon::project::memory_hub_dir("testproj")
                .expect("hub dir")
                .join("automation_log.md");
            let final_content = std::fs::read_to_string(&path).expect("read back");
            assert!(
                final_content.len() <= cap,
                "post-trim file must be within the cap: len={} cap={cap}",
                final_content.len()
            );
            // The most recent entries survived; the oldest were evicted.
            assert!(
                final_content.contains("line-019 payload"),
                "latest entry preserved"
            );
            assert!(
                !final_content.contains("line-000 payload"),
                "oldest entry trimmed"
            );
            // No partial line at the head.
            let first_line = final_content.lines().next().unwrap_or("");
            assert!(
                first_line.starts_with("line-"),
                "first line is intact, not a mid-line fragment: {first_line:?}"
            );
        });
    }

    #[test]
    fn auto_distill_produces_expected_entry_shape() {
        with_tmp_xdg(|_tmp| {
            let task_id = Uuid::parse_str("deadbeef-1234-5678-9abc-0123456789ab")
                .expect("valid uuid");
            auto_distill_task_completion(
                "testproj",
                "claude-alor",
                &task_id,
                "[T1] Fix the thing",
                "Done. Tests green.",
            )
            .expect("distill ok");
            let path = crate::daemon::project::memory_hub_dir("testproj")
                .expect("hub dir")
                .join(AUTOMATION_LOG_BASENAME);
            let content = std::fs::read_to_string(&path).expect("read back");
            // Shape: "- <iso> · <agent> · <task8> · <title> — <summary>\n"
            assert!(content.starts_with("- "), "bullet marker");
            assert!(content.contains(" · claude-alor · "), "agent id");
            assert!(content.contains(" · deadbeef · "), "8-char task-id short");
            assert!(
                content.contains(" · [T1] Fix the thing — Done. Tests green."),
                "title + em-dash + summary: {content:?}"
            );
            assert!(content.ends_with('\n'), "terminated with LF");
            // ISO8601 UTC: 20 chars wide (YYYY-MM-DDTHH:MM:SSZ).
            let after_bullet = &content[2..];
            let iso_chunk = &after_bullet[..20];
            assert!(
                iso_chunk.ends_with('Z')
                    && iso_chunk.contains('T')
                    && iso_chunk.contains('-')
                    && iso_chunk.contains(':'),
                "iso timestamp shape: {iso_chunk:?}"
            );
        });
    }

    #[test]
    fn auto_distill_flattens_multiline_summary_into_one_log_line() {
        with_tmp_xdg(|_tmp| {
            let task_id = Uuid::new_v4();
            auto_distill_task_completion(
                "testproj",
                "codex-alor",
                &task_id,
                "Multi-line\ntitle",
                "Line one\nLine two\n\nParagraph two",
            )
            .expect("distill ok");
            let path = crate::daemon::project::memory_hub_dir("testproj")
                .expect("hub dir")
                .join(AUTOMATION_LOG_BASENAME);
            let content = std::fs::read_to_string(&path).expect("read back");
            // Exactly one entry line (ends with \n); no interior LFs.
            assert_eq!(
                content.matches('\n').count(), 1,
                "single-line entry regardless of caller's newlines: {content:?}"
            );
            assert!(content.contains("Multi-line title"));
            assert!(content.contains("Line one Line two  Paragraph two"));
        });
    }

    #[test]
    fn link_agent_memory_refuses_paths_under_dot_claude() {
        // Repoint HOME so the guard's `~/.claude` check triggers against
        // a path we own, not the developer's actual config tree.
        let tmp = TempDir::new().expect("tempdir");
        let guard = crate::daemon::test_env::XDG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev_home = std::env::var("HOME").ok();
        let prev_xdg = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("XDG_DATA_HOME", tmp.path());

        let danger = tmp
            .path()
            .join(".claude/projects/-home-fett/memory/MEMORY.md");
        let err = link_agent_memory("testproj", "claude", danger.clone())
            .expect_err("must refuse paths under ~/.claude/");
        let msg = format!("{err}");
        assert!(
            msg.contains("refusing to link") && msg.contains(".claude"),
            "error must name the guard: {msg}"
        );

        // A path outside ~/.claude/ still works (sanity: we didn't break
        // the happy path with the guard).
        let safe_src = tmp.path().join("scratch_memory.md");
        std::fs::write(&safe_src, "seed\n").expect("write seed");
        link_agent_memory("testproj", "claude", safe_src)
            .expect("non-claude paths still link");

        if let Some(h) = prev_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(x) = prev_xdg {
            std::env::set_var("XDG_DATA_HOME", x);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        drop(guard);
    }
}
