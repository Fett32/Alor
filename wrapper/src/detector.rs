use regex::Regex;

/// Known agent types with their idle-prompt patterns and trust-dialog
/// handling data. Adding a new runtime is a single match-arm addition
/// per method — no callsite changes elsewhere.
#[derive(Debug, Clone)]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    Gemini,
    Cursor,
    /// Generic shell or unknown agent.
    Default,
}

impl AgentKind {
    /// Infer agent kind from the agent name string (e.g. "claude", "codex").
    pub fn from_name(name: &str) -> Self {
        let lower = name.to_lowercase();
        if lower.contains("claude") {
            AgentKind::ClaudeCode
        } else if lower.contains("codex") {
            AgentKind::Codex
        } else if lower.contains("gemini") {
            AgentKind::Gemini
        } else if lower.contains("cursor") {
            AgentKind::Cursor
        } else {
            AgentKind::Default
        }
    }

    fn pattern(&self) -> &str {
        match self {
            AgentKind::ClaudeCode => r"^[❯\$]\s*$",
            // Codex 0.120+ uses a TUI: input prompt line starts with
            // U+203A (right-pointing angle quote `›`) followed by a
            // rotating placeholder ("Run /review on my current
            // changes", "Find and fix a bug in @filename", etc.), then
            // a blank line, then a `[model] [mode] · [cwd]` footer.
            // The pre-0.120 shell-style `codex>` prompt is gone.
            //
            // Match on the `›` input line — stable across placeholder
            // rotation. Complement with `active_pattern()` for the
            // thinking-phase guard; see also the run_loop's
            // `content_changed` guard which handles the streaming
            // phase where `esc to interrupt` is NOT visible.
            AgentKind::Codex => r"^›\s",
            // Gemini CLI shows " >   Type your message" as its idle prompt.
            AgentKind::Gemini => r"^\s*>\s+(Type your message|$)",
            // Cursor uses an alt-screen TUI with no traditional trailing
            // prompt. Closest stable signal is the persistent
            // "Composer <model>" footer line. Live-probe captured it
            // in both idle and mid-generation states — the footer
            // alone is a necessary but NOT sufficient idle signal.
            // See `active_pattern()` for the complementary guard.
            AgentKind::Cursor => r"^\s*Composer\s",
            AgentKind::Default => r"^[\$>\+]\s*$",
        }
    }

    /// Runtime-specific "actively generating" anti-pattern. When any
    /// line in the idle-detection window matches this, the runtime is
    /// NOT idle even if the main `pattern()` does — covers the case
    /// where a runtime's idle footer stays visible throughout
    /// generation.
    ///
    /// Cursor: live-probe captured `ctrl+c to stop` in the right-
    /// aligned input hint during every mid-generation sample
    /// (Composing / Thinking / tool-use phases) and absent in every
    /// idle sample. Without this guard the `Composer <model>` footer
    /// would false-fire mid-task and emit a premature task-complete.
    /// Probe captures saved to session history during this change's
    /// recon: 1 idle + 12 mid-gen samples confirmed the split.
    ///
    /// Codex 0.120+: `• Working (Ns • esc to interrupt)` status line
    /// is visible during the thinking phase (pre-streaming). Once
    /// streaming starts the "Working" line is replaced by response
    /// text and `esc to interrupt` is no longer on the pane — that
    /// gap is handled by run_loop's `content_changed` guard rather
    /// than here. The anti-pattern narrows "is_idle true" to "idle
    /// or streaming", and the wrapper-level stability check converts
    /// "streaming" to "idle-timer-reset" via content_changed.
    ///
    /// Other runtimes: None — their `pattern()` already encodes a
    /// bottom-anchored shell prompt that naturally disappears during
    /// streaming output, so no additional anti-pattern is needed.
    fn active_pattern(&self) -> Option<&str> {
        match self {
            AgentKind::Cursor => Some(r"ctrl\+c to stop"),
            AgentKind::Codex => Some(r"esc to interrupt"),
            _ => None,
        }
    }

    /// How many trailing lines of the tmux capture to scan for the
    /// idle pattern. 5 is fine for bottom-anchored shell-style
    /// prompts (claude/codex/gemini/default) — the idle marker is
    /// always on (or very near) the last visible line when those
    /// runtimes are idle.
    ///
    /// Cursor draws a TUI with its `Composer <model>` footer a few
    /// lines from the bottom of the pane, and the lines below it are
    /// blank padding. A live probe showed the footer sitting at
    /// offset 6 from the bottom on a fresh post-ack capture (pane
    /// -y 50, capture 20 lines total, Composer at line 15) — tail-5
    /// missed it, causing a silent missed-idle. 15 lines gives 2.5x
    /// headroom over the worst case observed and still leaves plenty
    /// of margin against the `ctrl+c to stop` anti-pattern (which
    /// tends to land one line above Composer, so the larger window
    /// also catches the not-idle signal when generation is active).
    ///
    /// Codex 0.120 draws a TUI similar in shape to cursor's: the `›`
    /// input prompt line sits above a `[model] [mode] · [cwd]`
    /// footer, with empty padding rows below. A live 40-line capture
    /// showed the `›` input at line 29 / footer at line 31 out of 40
    /// (9 and 11 lines from the bottom). tail-5 misses both. More
    /// critically, the thinking-phase `esc to interrupt` marker sits
    /// ~17 lines from the bottom in the same sample — so even
    /// tail-15 (cursor's window) misses it and fails to block idle
    /// mid-thinking. 20 gives a comfortable margin for all three
    /// anchor points (`esc to interrupt`, `›` input, footer) while
    /// staying below the full 50-line capture size.
    ///
    /// Making this runtime-specific (rather than bumping a global
    /// constant) keeps the other runtimes' detection cheap and tight.
    pub fn idle_tail_window(&self) -> usize {
        match self {
            AgentKind::Cursor => 15,
            AgentKind::Codex => 20,
            _ => 5,
        }
    }

    /// Regex that matches the line marking "bottom edge of the
    /// assistant's most recent reply" in this runtime's pane capture —
    /// i.e. the runtime's input-prompt line. Everything ABOVE the
    /// lowest match in a captured pane is the reply (plus surrounding
    /// padding we'll trim).
    ///
    /// For runtimes whose idle detector already anchors on the input
    /// line (codex `›`, gemini `>`), this is the same expression as
    /// `pattern()`. Cursor is the exception: `pattern()` anchors on
    /// the `Composer …` footer that sits BELOW the input placeholder,
    /// so extraction needs its own anchor (`^\s*→\s`) one line higher.
    ///
    /// Returning `None` disables extraction for that runtime; callers
    /// should treat the reply as uncapturable and send a synthetic
    /// "(no text captured — …)" placeholder.
    pub fn reply_cutoff_pattern(&self) -> Option<&str> {
        match self {
            AgentKind::Codex => Some(r"^›\s"),
            AgentKind::Cursor => Some(r"^\s*→\s"),
            AgentKind::Gemini => Some(r"^\s*>\s+(Type your message|$)"),
            AgentKind::ClaudeCode => Some(r"^[❯\$]\s*$"),
            AgentKind::Default => Some(r"^[\$>\+]\s*$"),
        }
    }

    /// Extract the assistant's most recent reply from a pane capture.
    /// Called at `task.complete` fire time so wrapper-runtime workers
    /// (cursor, codex, gemini) populate `task.details` the way the
    /// claude-sdk worker already does via its own SDK-stream accumulator.
    ///
    /// Heuristic:
    ///   1. Find the LOWEST line in `lines` matching `reply_cutoff_pattern()`
    ///      (the runtime's input-prompt line). That's the bottom of the
    ///      reply region.
    ///   2. Take everything above. Strip leading and trailing blank
    ///      lines — most TUIs pad the reply with one or two empty lines
    ///      before the input row.
    ///   3. Return `None` if nothing meaningful remains.
    ///
    /// Known limitations (documented as a follow-up, not blockers):
    ///   - Multi-turn tasks: the pane's scrollback can contain prior
    ///     request/response pairs. Without a per-runtime "start of
    ///     current turn" anchor (codex shows a left-bar `▌` on user
    ///     lines; cursor doesn't visibly mark turn boundaries), we
    ///     return the whole region. Wrapper-runtime workers currently
    ///     don't accept framed `agent_send_message` mid-task anyway
    ///     (see hub note framing_begin_end.md §Scope), so the pane
    ///     typically has a single turn when complete fires.
    ///   - Reply longer than the capture window: the caller is
    ///     responsible for picking a deep enough capture; a 400-line
    ///     capture at fire-time with tmux history-limit 50000 covers
    ///     every realistic reply. Longer replies get truncated to the
    ///     last N lines — partial-reply detection (see mid-codeblock
    ///     warning in `looks_truncated`) surfaces the warning.
    pub fn extract_reply(&self, lines: &[String]) -> Option<String> {
        let cutoff_src = self.reply_cutoff_pattern()?;
        let cutoff_re = Regex::new(cutoff_src).ok()?;
        let cutoff_idx = lines.iter().rposition(|l| cutoff_re.is_match(l.trim_end()))?;
        let mut upper = &lines[..cutoff_idx];
        // Strip trailing blank lines.
        while let Some(last) = upper.last() {
            if last.trim().is_empty() {
                upper = &upper[..upper.len() - 1];
            } else {
                break;
            }
        }
        // Strip leading blank lines.
        let mut start = 0;
        while start < upper.len() && upper[start].trim().is_empty() {
            start += 1;
        }
        let body_lines = &upper[start..];
        if body_lines.is_empty() {
            return None;
        }
        let joined = body_lines.join("\n");
        // `trim_end` to drop any trailing whitespace that survived
        // the blank-line pass; we keep leading whitespace because
        // markdown indentation (code blocks, lists) is meaningful.
        let trimmed = joined.trim_end();
        if trimmed.is_empty() {
            return None;
        }
        // Peel CLI banner + echoed task brief off the top. A fresh-
        // session cursor pane (the live smoke-test shape from T5)
        // looks like:
        //   "Cursor Agent / v<ver> / hint: …"   ← banner
        //   <2 blank lines>
        //   "[TASK TITLE] / <echoed description>"  ← brief echo
        //   <2 blank lines>
        //   "<worker reply>"
        // with TWO blank lines between each segment. Intra-reply
        // paragraph breaks use ONE blank line (standard markdown),
        // so splitting on "\n\n\n" (three-or-more consecutive
        // newlines == turn boundary) cleanly isolates the worker's
        // reply from the banner + echoed brief sitting above it.
        // Multi-turn panes with several prior request/reply pairs
        // visible also collapse to the most recent reply via the
        // same rule — rsplit_once anchors on the LAST boundary.
        Some(strip_to_last_turn(trimmed).to_string())
    }

    /// Prompts to auto-acknowledge on startup, in the order the runtime
    /// typically renders them. Each entry is
    /// `(substring-to-match, key-to-send-as-ack)`. Multiple entries per
    /// kind handle sequential prompts — e.g. codex 0.120 shows an
    /// update prompt FIRST, then the trust prompt; both need acking
    /// with different keys.
    ///
    /// Per-runtime because each CLI phrases its dialog differently.
    /// Returning an empty slice means "no dialog expected" — the
    /// caller's polling loop short-circuits and never fires.
    ///
    /// Kept as substrings (not regex) so lookups stay `str::contains`
    /// cheap and the match is robust to minor TUI layout changes.
    /// Previous `trust_prompt_hints()` + `trust_ack_key()` methods
    /// were subsumed here: they couldn't represent per-prompt ack
    /// keys, which broke for multi-stage runtimes like codex.
    pub fn trust_prompts(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            // Claude Code's trust dialog opens with
            //   "Quick safety check: Is this a project you created or one you trust?"
            // and offers "1. Yes, I trust this folder" / "2. No, exit".
            // The "trust this folder" option text happens to collide
            // with codex's dialog wording, so we match on the
            // distinctive intro line instead.
            //
            // All current claude yamls are runtime: claude-sdk
            // (run-worker.sh) and never hit this CLI prompt, so the
            // entry is future-proofing — a wrapper-runtime claude slot
            // won't hang on trust-ack just because nobody remembered
            // to add an entry when registering it.
            AgentKind::ClaudeCode => &[("Quick safety check", "1")],

            // Codex 0.120+ renders TWO sequential prompts on cold
            // launch in an untrusted dir: first the update-available
            // prompt (offers "3. Skip until next version" — pick that
            // so we never auto-update), then the trust-dir prompt
            // (offers "1. Yes, continue"). The polling loop in main.rs
            // sees each in turn and acks with the paired key.
            //
            // The trust-dialog text drifted from the pre-0.120 wording
            // ("trust this folder") to "Do you trust the contents of
            // this directory?" — hence the current match.
            AgentKind::Codex => &[
                ("Update available!", "3"),
                ("Do you trust the contents", "1"),
            ],

            // Gemini: distinctive "Trusting a folder" dialog intro.
            AgentKind::Gemini => &[("Trusting a folder", "1")],

            // Cursor: box header "Workspace Trust Required". Cursor
            // re-prompts every session (doesn't persist trust state),
            // so auto-ack is essential, not just polish.
            AgentKind::Cursor => &[("Workspace Trust Required", "a")],

            // Default / unknown runtimes: empty — the polling loop
            // skips entirely, we don't splat keystrokes into a CLI
            // whose prompts we don't know.
            AgentKind::Default => &[],
        }
    }
}

// ---------------------------------------------------------------------------
// Reply extraction helpers — shared across all wrapper runtimes.
// Placed alongside the per-runtime TUI knowledge so everything the
// wrapper knows about a given CLI's pane shape lives in one file.
// ---------------------------------------------------------------------------

/// Peel CLI banner + any prior-turn echoes off the top of a captured
/// reply body. Called from `extract_reply` after blank-line trimming.
///
/// Heuristic: TUIs separate turns (banner, echoed task brief, worker
/// reply, subsequent turns) with TWO blank lines, which renders as
/// three consecutive newlines in the joined body. Intra-reply
/// paragraph breaks use ONE blank line (two newlines). So a split on
/// `\n\n\n` identifies turn boundaries without false-splitting
/// multi-paragraph replies. Keeping the last chunk isolates the most
/// recent worker reply.
///
/// If no triple-newline exists (single-turn, no banner — the shape
/// already exercised by pre-polish tests), returns the body unchanged.
/// This keeps the helper safe to apply universally across cursor /
/// codex / gemini rather than gating per-runtime — the hot cases for
/// codex and gemini don't have triple newlines and fall through.
fn strip_to_last_turn(body: &str) -> &str {
    match body.rsplit_once("\n\n\n") {
        // Strip any remaining leading newlines from the tail — a
        // separator of 4+ newlines leaves one behind after `rsplit_once`.
        // Preserve other leading whitespace (indentation is meaningful
        // inside markdown code blocks / lists; see `extract_reply`'s
        // "we keep leading whitespace" contract).
        Some((_prior, tail)) => tail.trim_start_matches('\n').trim_end(),
        None => body,
    }
}

/// Terse-summary byte target. Matches `orchestrator-py/worker.py`'s
/// `TERSE_TARGET = 400` so wrapper-runtime completions and claude-sdk
/// completions produce the same split shape from the daemon's
/// perspective. 512 B is the server cap; 400 B leaves ~112 B headroom
/// so daemon-side truncation rarely fires.
pub const REPLY_TERSE_TARGET: usize = 400;

/// Upper bound on the `details` body. Matches worker.py's
/// `DETAILS_MAX = 64 * 1024`. Daemon re-caps at 1 MiB as a safety net.
pub const REPLY_DETAILS_MAX: usize = 64 * 1024;

/// Placeholder sent in `task.details` when the pane capture yielded
/// nothing. Distinguishes "wrapper captured an empty reply" from the
/// pre-fix "wrapper sent no details field at all" state — both leave
/// the frontend task-card blank, but post-fix the placeholder makes
/// the empty-capture case diagnosable without a log dive.
pub const EMPTY_CAPTURE_PLACEHOLDER: &str = "(no text captured — pane was blank)";

/// Split a captured reply body into (summary, details) using the same
/// policy as `orchestrator-py/worker.py`:
///
///   - Empty / whitespace-only body: `(None, None)`.
///   - Body fits in `REPLY_TERSE_TARGET` bytes: summary-only.
///   - Body larger: summary = first paragraph (up to `\n\n`) within
///     the first `REPLY_TERSE_TARGET` bytes, falling back to a hard
///     UTF-8-safe cut + `…` marker when no paragraph break exists in
///     that window. Details = full body, capped at
///     `REPLY_DETAILS_MAX` with a `\n…[truncated]` marker.
pub fn split_summary_details(body: &str) -> (Option<String>, Option<String>) {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return (None, None);
    }
    let encoded = trimmed.as_bytes();
    if encoded.len() <= REPLY_TERSE_TARGET {
        return (Some(trimmed.to_string()), None);
    }

    // UTF-8-safe head cut: back up to a char boundary so the daemon
    // never sees a truncated multi-byte codepoint.
    let mut head_end = REPLY_TERSE_TARGET;
    while head_end > 0 && !trimmed.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let head = &trimmed[..head_end];
    let summary = match head.find("\n\n") {
        Some(i) => head[..i].trim_end().to_string(),
        None => {
            // No paragraph break in the first 400 B — synthesize a
            // terse marker so the orch event formatter can still hint
            // at the truncation via `has_details`.
            format!("{}…", head.trim_end())
        }
    };

    let details = if encoded.len() > REPLY_DETAILS_MAX {
        let mut end = REPLY_DETAILS_MAX;
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n…[truncated]", &trimmed[..end])
    } else {
        trimmed.to_string()
    };

    (Some(summary), Some(details))
}

/// Heuristic: does this body look like the wrapper caught the reply
/// mid-stream? Right now the only signal we trust is an odd number of
/// ```` ``` ```` fences — a code block that started but never closed.
/// The content-stability guard in `decide_poll_actions` should prevent
/// this case at the timing layer; if this returns true at fire time,
/// that's a sign `IDLE_STABLE_SECS` needs more tuning.
pub fn looks_truncated(body: &str) -> bool {
    let fences = body.matches("```").count();
    fences % 2 == 1
}

/// Compiled idle-pattern detector.
pub struct IdleDetector {
    re: Regex,
    /// Compiled from `AgentKind::active_pattern()`. When Some, the
    /// detector reports NOT-idle whenever any scanned line matches
    /// this regex — overrides a `re` match. See cursor case in
    /// `active_pattern()` for the motivating example.
    active_re: Option<Regex>,
    /// Cached from `AgentKind::idle_tail_window()` at construction.
    /// The number of trailing pane lines `is_idle_tail` scans. Most
    /// runtimes use 5; cursor's TUI needs a wider window because its
    /// idle footer sits several lines from the bottom of the pane.
    tail_window: usize,
}

impl IdleDetector {
    pub fn new(kind: &AgentKind) -> Self {
        let re = Regex::new(kind.pattern())
            .unwrap_or_else(|_| Regex::new(r"^[\$>]\s*$").unwrap());
        // Silently skip a malformed active_pattern — a regex error
        // here shouldn't crash the wrapper, and the worst outcome is
        // reverting to pre-guard behavior (potential false-fire,
        // loud-test-visible).
        let active_re = kind.active_pattern().and_then(|p| Regex::new(p).ok());
        let tail_window = kind.idle_tail_window();
        IdleDetector { re, active_re, tail_window }
    }

    /// Returns the runtime-specific scan window size used by
    /// `is_idle_tail`. Exposed so callers that also want to hash /
    /// fingerprint the trailing pane content can keep that window
    /// aligned with the idle-detection one (see wrapper/src/main.rs
    /// intervention-detection fingerprint).
    pub fn tail_window(&self) -> usize {
        self.tail_window
    }

    /// Returns true if any of the provided lines match the idle pattern
    /// AND no line matches the runtime's active-generation anti-
    /// pattern (if it has one). The anti-pattern check wins — it's a
    /// hard "definitely not idle" signal.
    pub fn is_idle(&self, lines: &[String]) -> bool {
        if let Some(ref active) = self.active_re {
            // Don't trim — active markers can appear anywhere on the
            // line (e.g. cursor's right-aligned `ctrl+c to stop` hint).
            if lines.iter().any(|l| active.is_match(l)) {
                return false;
            }
        }
        lines.iter().any(|l| self.re.is_match(l.trim_end()))
    }

    /// Convenience: check the last `tail_window` lines (runtime-
    /// specific). The window size is fixed at construction from
    /// `AgentKind::idle_tail_window()`, so callers don't have to
    /// track per-runtime sizing themselves.
    pub fn is_idle_tail(&self, lines: &[String]) -> bool {
        let start = lines.len().saturating_sub(self.tail_window);
        self.is_idle(&lines[start..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn claude_idle() {
        let d = IdleDetector::new(&AgentKind::ClaudeCode);
        assert!(d.is_idle(&lines(&["❯ "])));
        assert!(d.is_idle(&lines(&["❯"])));
        assert!(d.is_idle(&lines(&["$ "])));
        assert!(d.is_idle(&lines(&["$"])));
        assert!(!d.is_idle(&lines(&["❯ some output"])));
        assert!(!d.is_idle(&lines(&["$ running something"])));
    }

    #[test]
    fn codex_idle() {
        // Codex 0.120 TUI: input prompt is `› <placeholder>`.
        // Placeholder rotates between session loads ("Find and fix a
        // bug in @filename", "Run /review on my current changes",
        // etc.) so the test matrix covers the leading `›\s` anchor
        // across a handful of live-captured placeholders rather than
        // any specific trailing text.
        let d = IdleDetector::new(&AgentKind::Codex);
        assert!(d.is_idle(&lines(&["› Find and fix a bug in @filename"])));
        assert!(d.is_idle(&lines(&["› Run /review on my current changes"])));
        assert!(d.is_idle(&lines(&["› Write tests for @filename"])));
        assert!(d.is_idle(&lines(&["› Explain this codebase"])));
        // Mid-line `›` must not match — pattern is `^›\s` anchored.
        assert!(!d.is_idle(&lines(&["Running › foo"])));
        // Old pre-0.120 `codex>` prompt: explicitly no longer idle.
        // Drift guard — if someone re-adds legacy compat without
        // noticing the TUI change, this fires.
        assert!(!d.is_idle(&lines(&["codex> "])));
        assert!(!d.is_idle(&lines(&["$ "])));
    }

    #[test]
    fn codex_fresh_session_idle_caught_by_wider_window() {
        // Verbatim 40-line tmux capture of codex 0.120 post-startup
        // idle state (after the intro banner + update-available box),
        // captured via the wrapper-exact command:
        //   `tmux capture-pane -p -t '=<session>:' -S -50`.
        //
        // `› Find and fix a bug in @filename` lands at line 22 /
        // index 21 (10 lines above the bottom); `  gpt-5.4 default
        // · ~/Projects/Alor` footer at line 24 / index 23 (8 lines
        // above the bottom). The pre-fix tail-5 window scanned only
        // lines 36..40 (all blank) and missed both, keeping
        // is_idle false — that's the "codex never completes"
        // symptom: IDLE_STABLE_SECS never accumulates because the
        // detector can't see the prompt. tail-20 (the new
        // per-runtime window) reaches `›` and registers idle
        // correctly.
        //
        // Note: the `• Booting MCP server: codex_apps (2s • esc to
        // interrupt)` line sits at line 19 / index 18 — 2 lines
        // ABOVE the tail-20 scan window. During real run_loop
        // execution this is fine because the wrapper only enters
        // run_loop after the trust-ack polling budget (up to 30s
        // boot grace), by which point MCP boot has finished and
        // the line has scrolled off. The test exercises the
        // post-boot state; `codex_working_phase_blocks_idle_fire`
        // covers the mid-work case where `esc to interrupt` IS in
        // the tail.
        let d = IdleDetector::new(&AgentKind::Codex);
        let fresh_capture = lines(&[
            "",
            "╭─────────────────────────────────────────────────╮",
            "│ ✨ Update available! 0.120.0 -> 0.121.0         │",
            "│ Run npm install -g @openai/codex to update.     │",
            "│                                                 │",
            "│ See full release notes:                         │",
            "│ https://github.com/openai/codex/releases/latest │",
            "╰─────────────────────────────────────────────────╯",
            "",
            "╭───────────────────────────────────────╮",
            "│ >_ OpenAI Codex (v0.120.0)            │",
            "│                                       │",
            "│ model:     gpt-5.4   /model to change │",
            "│ directory: ~/Projects/Alor            │",
            "╰───────────────────────────────────────╯",
            "",
            "  Tip: New Use /fast to enable our fastest inference at 2X plan usage.",
            "",
            "• Booting MCP server: codex_apps (2s • esc to interrupt)",
            "",
            "",
            "› Find and fix a bug in @filename",
            "",
            "  gpt-5.4 default · ~/Projects/Alor",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ]);
        assert!(
            d.is_idle_tail(&fresh_capture),
            "codex fresh post-boot capture with `›` input prompt in tail-20 must register as idle"
        );

        // Regression guard 1: confirm the previous tail-5 window
        // would have missed it. Without this assertion the test
        // could silently become a tautology after a fixture change.
        let start_5 = fresh_capture.len() - 5;
        assert!(
            !d.is_idle(&fresh_capture[start_5..]),
            "regression guard: `›` is NOT in the last 5 lines — \
             if this fires, the fixture drifted and the wider-window \
             test no longer demonstrates the old bug"
        );

        // Regression guard 2: the `esc to interrupt` boot marker
        // stays OUT of tail-20 in this fixture, which is why idle
        // fires. If the fixture drifts such that the marker slides
        // in, the run_loop-level stability guard still handles the
        // "pane changing" case, but this specific test is written
        // for the post-boot-banner scenario — assert the precondition.
        let start_20 = fresh_capture.len() - 20;
        let tail_text = fresh_capture[start_20..].join("\n");
        assert!(
            tail_text.contains("› Find and fix a bug in @filename"),
            "fixture invariant: `›` prompt must be inside tail-20"
        );
        assert!(
            !tail_text.contains("esc to interrupt"),
            "fixture invariant: `esc to interrupt` MCP-boot marker must be ABOVE tail-20"
        );
    }

    #[test]
    fn codex_post_boot_idle() {
        // Verbatim post-generation idle capture (codex probe, same
        // session, after the response completes). The boot MCP line
        // has scrolled off; no `esc to interrupt` anywhere in the
        // tail. `› <placeholder>` is present and primary matches.
        let d = IdleDetector::new(&AgentKind::Codex);
        let idle_tail = lines(&[
            "  28. Another perfect number.",
            "  29. Large prime right before 30.",
            "  30. A full, round stopping point.",
            "",
            "",
            "› Explain this codebase",
            "",
            "  gpt-5.4 default · ~/Projects/Alor",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ]);
        assert!(
            d.is_idle_tail(&idle_tail),
            "codex post-generation idle (with `›` in view, no `esc to interrupt`) must register as idle"
        );
    }

    #[test]
    fn codex_working_phase_blocks_idle_fire() {
        // Verbatim mid-thinking capture: `• Working (2s • esc to
        // interrupt)` visible above the `› <placeholder>` input line.
        // The primary pattern matches `›`, but the active-pattern
        // `esc to interrupt` wins and blocks idle — mirroring the
        // cursor `ctrl+c to stop` guard shape.
        let d = IdleDetector::new(&AgentKind::Codex);
        let working_tail = lines(&[
            "",
            "› write a 3-sentence description of a dog",
            "",
            "",
            "• Working (2s • esc to interrupt)",
            "",
            "",
            "› Find and fix a bug in @filename",
            "",
            "  gpt-5.4 default · ~/Projects/Alor",
            "",
            "",
            "",
            "",
            "",
        ]);
        assert!(
            !d.is_idle_tail(&working_tail),
            "codex mid-thinking (Working ... esc to interrupt) must NOT be idle"
        );
    }

    #[test]
    fn codex_streaming_primary_matches_but_wrapper_stability_guard_handles_it() {
        // Verbatim mid-streaming capture: codex is actively streaming
        // response tokens. The `• Working` line is GONE (replaced by
        // the response), `esc to interrupt` is ABSENT from the tail,
        // and the `› <placeholder>` input line is still visible.
        //
        // At the detector level, is_idle_tail returns TRUE here —
        // which is correct given the inputs (primary matches, no
        // anti-pattern present). The wrapper's run_loop converts
        // that to "NOT task complete" by requiring pane stability
        // (!content_changed) across IDLE_STABLE_SECS — and the pane
        // changes every poll during streaming.
        //
        // This test locks in the detector-layer contract so the
        // stability-guard responsibility stays where it belongs:
        // run_loop, not IdleDetector.
        let d = IdleDetector::new(&AgentKind::Codex);
        let streaming_tail = lines(&[
            "• 1. First number; the counting starts here.",
            "  2. The only even prime.",
            "  3. Stable triangle number.",
            "  4. Smallest square after 1.",
            "  5. Common counting base for fingers.",
            "",
            "",
            "› Explain this codebase",
            "",
            "  gpt-5.4 default · ~/Projects/Alor",
            "",
            "",
            "",
            "",
            "",
        ]);
        assert!(
            d.is_idle_tail(&streaming_tail),
            "detector layer: streaming without `esc to interrupt` + visible `›` prompt → is_idle true (wrapper stability guard handles the real premature-fire)"
        );
    }

    #[test]
    fn gemini_idle() {
        let d = IdleDetector::new(&AgentKind::Gemini);
        assert!(d.is_idle(&lines(&[" >   Type your message or @path/to/file"])));
        assert!(d.is_idle(&lines(&[" >   Type your message"])));
        assert!(!d.is_idle(&lines(&["generating response..."])));
        assert!(!d.is_idle(&lines(&["$ "])));
    }

    #[test]
    fn cursor_idle() {
        let d = IdleDetector::new(&AgentKind::Cursor);
        // Persistent footer line captured from live cursor-agent probe.
        assert!(d.is_idle(&lines(&["  Composer 2 Fast"])));
        assert!(d.is_idle(&lines(&["Composer auto"])));
        // Full tail-5 window captured from wrapper-equivalent
        // `tmux capture-pane -S -50` when cursor is idle post-startup.
        // Input line shows the fresh placeholder, NO ctrl+c hint.
        assert!(d.is_idle(&lines(&[
            "  → Plan, search, build anything",
            "",
            "",
            "  Composer 2 Fast",
            "  /tmp/alor-cursor-probe",
        ])));
        // Must not false-match arbitrary text even if "Composer" appears
        // mid-line (word-start anchored via `^\s*`).
        assert!(!d.is_idle(&lines(&["Running Composer 2 Fast now"])));
        assert!(!d.is_idle(&lines(&["$ "])));
        // Still rejects non-cursor idle prompts.
        assert!(!d.is_idle(&lines(&["codex> "])));
    }

    #[test]
    fn idle_tail_windows_are_per_runtime() {
        // Locked-values guard: the size numbers are security- /
        // correctness-relevant (smaller → missed idle, larger → wider
        // scan cost). Drift should be deliberate.
        assert_eq!(AgentKind::ClaudeCode.idle_tail_window(), 5);
        assert_eq!(AgentKind::Gemini.idle_tail_window(), 5);
        assert_eq!(AgentKind::Default.idle_tail_window(), 5);
        // Cursor needs a wider window — see `idle_tail_window()`
        // docstring for the probe-derived sizing.
        assert_eq!(AgentKind::Cursor.idle_tail_window(), 15);
        // Codex 0.120 TUI needs wider still — `esc to interrupt`
        // status can sit ~17 lines from the bottom. See docstring.
        assert_eq!(AgentKind::Codex.idle_tail_window(), 20);
    }

    #[test]
    fn detector_exposes_configured_tail_window() {
        // IdleDetector should surface the per-kind window so callers
        // (e.g. wrapper/main.rs intervention fingerprint) can keep
        // their own tail slicing aligned.
        assert_eq!(IdleDetector::new(&AgentKind::ClaudeCode).tail_window(), 5);
        assert_eq!(IdleDetector::new(&AgentKind::Cursor).tail_window(), 15);
    }

    #[test]
    fn cursor_fresh_session_idle_caught_by_wider_window() {
        // Regression test for the missed-idle flagged in d175037's
        // wrap-up. Captured verbatim from a live cursor-agent fresh
        // post-trust-ack state via the wrapper-exact command
        // (`tmux capture-pane -p -t '=<session>:' -S -50`):
        //
        // 20-line capture. `Composer 2 Fast` sits at line 15 — that's
        // offset 6 from the bottom. The previous fixed tail=5 scan
        // would have missed it (all 5 trailing lines are blank),
        // leaving the detector silent even though the runtime was
        // fully idle and waiting for input.
        //
        // With the per-runtime window (Cursor → 15), the scan reaches
        // the Composer line and fires correctly. Asserted both ways
        // below so a regression of either the window size or the
        // idle pattern surfaces clearly.
        let fresh_idle_capture = lines(&[
            "cursor-agent",
            "fett@Fett-Linux:/tmp/alor-cursor-probe2$ cursor-agent",
            "",
            "",
            "  Cursor Agent",
            "  v2026.04.17-479fd04",
            "  hint: /auto-run to skip all approvals",
            "",
            "",
            "",
            "",
            "  → a",
            "",
            "",
            "  Composer 2 Fast",
            "  /tmp/alor-cursor-probe2",
            "",
            "",
            "",
            "",
        ]);

        let d = IdleDetector::new(&AgentKind::Cursor);
        assert!(
            d.is_idle_tail(&fresh_idle_capture),
            "Cursor fresh-session idle should be detected via is_idle_tail with the runtime's tail_window"
        );

        // Sanity: confirm the old 5-line window would have missed it.
        // Using the same scan logic directly on the bottom 5 lines.
        let start = fresh_idle_capture.len() - 5;
        assert!(
            !d.is_idle(&fresh_idle_capture[start..]),
            "Regression guard: Composer is NOT in the last 5 lines of the fixture — if this assertion starts failing, the fixture has drifted and the test no longer exercises the bug it was written for"
        );
    }

    #[test]
    fn cursor_active_generation_blocks_idle_fire() {
        // Regression test for the residual risk flagged in 9c863aa8.
        // Captured tail-5 from a live cursor-agent mid-generation
        // sample via the exact wrapper command
        // (`tmux capture-pane -p -t '=<session>:' -S -50`) — verbatim.
        //
        // BOTH the `Composer 2 Fast` footer AND the `ctrl+c to stop`
        // right-aligned input hint are present in this window. Before
        // the active_pattern guard, the detector fired idle on this
        // state and would have emitted a premature task.complete.
        // With the guard, `ctrl+c to stop` blocks the idle fire.
        let d = IdleDetector::new(&AgentKind::Cursor);
        let midgen_tail = lines(&[
            "  → Add a follow-up                                                                    ctrl+c to stop",
            "",
            "",
            "  Composer 2 Fast",
            "  /tmp/alor-cursor-probe",
        ]);
        assert!(
            !d.is_idle(&midgen_tail),
            "mid-generation tail must NOT be classified as idle (Composer footer + ctrl+c to stop hint both present)"
        );

        // Also verify the other mid-gen variants captured during the
        // probe: percentage-in-footer post-first-gen, Thinking spinner
        // earlier, etc. All share the `ctrl+c to stop` marker.
        let midgen_tail_with_pct = lines(&[
            "  → Add a follow-up                                                                    ctrl+c to stop",
            "",
            "",
            "  Composer 2 Fast · 3.9%",
            "  /tmp/alor-cursor-probe",
        ]);
        assert!(
            !d.is_idle(&midgen_tail_with_pct),
            "mid-gen with context-% footer must NOT be idle"
        );

        // Degenerate case: Composer alone is still idle — the guard
        // only fires when ctrl+c-to-stop is actually present.
        assert!(d.is_idle(&lines(&["  Composer 2 Fast"])));
    }

    #[test]
    fn cursor_approval_gate_not_idle() {
        // Regression test for the third distinct cursor non-idle state
        // identified during the live verification probe (task 037b77ba
        // follow-up to 1b5196c7 — cursor_false_completion suspicion).
        //
        // When cursor-agent prompts the user to approve a shell
        // command, the approval box takes over the bottom of the
        // pane:
        //
        //     Run this command?
        //     Not in allowlist: <cmd>
        //      → Run (once) (y)
        //        Add Shell(<bin>) to allowlist? (tab)
        //        Auto-run everything (shift+tab)
        //        Skip (esc or n)
        //                                              ctrl+r to review changed files
        //
        // Critical observation from the live probe: the
        // `Composer <model>` footer is pushed off the viewport
        // entirely by the approval box. So the detector's primary
        // pattern `^\s*Composer\s` doesn't match — `is_idle` returns
        // false via the primary-pattern miss, not via the
        // `ctrl+c to stop` anti-pattern (which is ALSO absent here;
        // cursor shows `ctrl+r to review changed files` instead).
        //
        // The fixture below is the verbatim tail-15 captured from a
        // live cursor-agent probe via the wrapper-exact command
        // (`tmux capture-pane -p -t '=<session>:' -S -50`). Across
        // 50 consecutive 200ms polls over 11 seconds of the approval
        // gate, the detector reported 0 idle samples — well below
        // the ~15 consecutive samples that `IDLE_STABLE_SECS = 3.0`
        // would need to fire a false task.complete.
        let d = IdleDetector::new(&AgentKind::Cursor);
        let approval_gate_tail = lines(&[
            "",
            " ┌───────────────────────────────────────────────────────────────────────────────────────────────────┐",
            " │ $  wc -l /tmp/alor-cursor-fc-fixture/test.txt in .                                                │",
            " └───────────────────────────────────────────────────────────────────────────────────────────────────┘",
            "",
            "",
            "  Run this command?",
            "  Not in allowlist: wc -l /tmp/alor-cursor-fc-fixture/test.txt",
            "   → Run (once) (y)",
            "     Add Shell(wc) to allowlist? (tab)",
            "     Auto-run everything (shift+tab)",
            "     Skip (esc or n)",
            "",
            "",
            "                                                                       ctrl+r to review changed files",
        ]);

        // Primary assertion: the whole tail must NOT be classified as
        // idle. If this starts failing, the detector would emit a
        // premature task.complete during every shell-command approval
        // gate — exactly the 883bdc60 / 1b5196c7 symptom.
        assert!(
            !d.is_idle(&approval_gate_tail),
            "cursor approval-gate tail must NOT be classified as idle"
        );

        // Defensive: verify the fixture genuinely exercises the
        // primary-pattern-miss path rather than accidentally tripping
        // some other guard. If a future detector refactor loosens
        // the pattern (e.g. matches "Run" or "ctrl+r"), this
        // assertion surfaces the drift loudly instead of letting
        // the main is_idle check carry a stale regression meaning.
        let primary = Regex::new(AgentKind::Cursor.pattern())
            .expect("primary pattern compiles");
        for line in &approval_gate_tail {
            assert!(
                !primary.is_match(line.trim_end()),
                "primary pattern `{}` must NOT match approval-gate line `{}` — \
                 if this fails, the fixture no longer exercises the \
                 Composer-pushed-off-viewport scenario the test was written for",
                AgentKind::Cursor.pattern(),
                line,
            );
        }
    }

    #[test]
    fn default_idle() {
        let d = IdleDetector::new(&AgentKind::Default);
        assert!(d.is_idle(&lines(&["$ "])));
        assert!(d.is_idle(&lines(&["> "])));
        assert!(!d.is_idle(&lines(&["$ running something"])));
    }

    // ---- reply extraction ----

    #[test]
    fn extract_codex_post_boot_reply() {
        // Post-boot idle capture: a numbered list reply above the
        // `›` input line, with the `default · ~/Projects/Alor` footer
        // below. Extraction should return the numbered items and
        // strip the input line + footer + blank padding.
        let lines = lines(&[
            "  28. Another perfect number.",
            "  29. Large prime right before 30.",
            "  30. A full, round stopping point.",
            "",
            "",
            "› Explain this codebase",
            "",
            "  gpt-5.4 default · ~/Projects/Alor",
            "",
            "",
            "",
        ]);
        let body = AgentKind::Codex.extract_reply(&lines)
            .expect("codex reply extractable");
        assert!(body.starts_with("  28. Another perfect number."),
            "body starts with the reply content, not the padding: {body:?}");
        assert!(body.ends_with("A full, round stopping point."),
            "body ends at the last non-blank reply line: {body:?}");
        assert!(!body.contains("›"),
            "input line should be excluded: {body:?}");
        assert!(!body.contains("gpt-5.4"),
            "footer should be excluded: {body:?}");
    }

    #[test]
    fn extract_cursor_post_reply() {
        // Synthetic post-reply capture modelled on a cursor TUI at
        // idle: assistant prose, blank, `→` input placeholder, blank,
        // Composer footer, workdir line, padding.
        let lines = lines(&[
            "I found the TaskList render at src/components/TaskList.js:456.",
            "",
            "The summary element is built inside buildTaskCard(...).",
            "",
            "",
            "  → Add a follow-up",
            "",
            "",
            "  Composer 2 Fast",
            "  /home/fett/Projects/Alor",
            "",
            "",
        ]);
        let body = AgentKind::Cursor.extract_reply(&lines)
            .expect("cursor reply extractable");
        assert!(body.contains("TaskList render"),
            "reply prose preserved: {body:?}");
        assert!(body.contains("buildTaskCard"),
            "multi-paragraph reply preserved: {body:?}");
        assert!(!body.contains("Add a follow-up"),
            "`→` input line stripped: {body:?}");
        assert!(!body.contains("Composer"),
            "Composer footer stripped: {body:?}");
        assert!(!body.contains("/home/fett/Projects/Alor"),
            "workdir line stripped (below Composer): {body:?}");
    }

    #[test]
    fn extract_gemini_post_reply() {
        let lines = lines(&[
            "Here's the answer to your question about X.",
            "",
            "Key points:",
            "  - First thing",
            "  - Second thing",
            "",
            " > Type your message or @path/to/file",
            "",
            "",
        ]);
        let body = AgentKind::Gemini.extract_reply(&lines)
            .expect("gemini reply extractable");
        assert!(body.starts_with("Here's the answer"));
        assert!(body.contains("Second thing"));
        assert!(!body.contains("Type your message"));
    }

    #[test]
    fn extract_empty_pane_returns_none() {
        // Just the input prompt, no reply above it.
        let lines = lines(&[
            "",
            "",
            "› Find and fix a bug in @filename",
            "",
            "  gpt-5.4 default · ~/Projects/Alor",
            "",
        ]);
        assert!(
            AgentKind::Codex.extract_reply(&lines).is_none(),
            "no reply text above input → None, so caller can substitute the empty-capture placeholder"
        );
    }

    #[test]
    fn extract_no_cutoff_line_returns_none() {
        // No `›` line anywhere — e.g. a crashed CLI or captured pane
        // during the trust-dialog phase. Extraction returns None;
        // caller substitutes the placeholder.
        let lines = lines(&[
            "Some random pane content",
            "with no input prompt markers",
            "",
        ]);
        assert!(AgentKind::Codex.extract_reply(&lines).is_none());
        assert!(AgentKind::Cursor.extract_reply(&lines).is_none());
        assert!(AgentKind::Gemini.extract_reply(&lines).is_none());
    }

    #[test]
    fn extract_cursor_multiturn_uses_lowest_cutoff() {
        // Two `→` lines visible in scrollback (unusual but possible
        // after scrollback replay or a TUI redraw). `rposition` should
        // pick the LAST one — that's the current input line — and
        // everything above it is the reply region.
        //
        // In practice for cursor this mostly guards against a reply
        // whose markdown happens to quote the placeholder text
        // verbatim. The second `→` (input) wins; the first (quoted)
        // stays inside the captured body.
        let lines = lines(&[
            "Reply line 1",
            "  → this is a quoted arrow inside the reply, not the input",
            "Reply line 3",
            "",
            "",
            "  → Add a follow-up",
            "",
            "  Composer 2 Fast",
        ]);
        let body = AgentKind::Cursor.extract_reply(&lines)
            .expect("reply extractable");
        assert!(body.contains("Reply line 1"));
        assert!(body.contains("quoted arrow inside the reply"),
            "first `→` is part of the reply, should be kept");
        assert!(body.contains("Reply line 3"));
        assert!(!body.contains("Add a follow-up"),
            "input line (second `→`) stripped");
    }

    #[test]
    fn extract_strips_trailing_blanks_but_preserves_leading_indent() {
        // Indentation inside a reply (markdown list, code block) must
        // survive; only fully-blank trailing lines are stripped.
        let lines = lines(&[
            "    code block line 1",
            "    code block line 2",
            "",
            "",
            "› input",
            "",
        ]);
        let body = AgentKind::Codex.extract_reply(&lines).expect("has reply");
        assert!(body.starts_with("    code block line 1"),
            "leading indentation preserved: {body:?}");
        assert!(body.ends_with("    code block line 2"),
            "trailing blank stripped, last indented line kept: {body:?}");
    }

    // ---- strip_to_last_turn ----

    #[test]
    fn strip_to_last_turn_splits_on_triple_newline() {
        // Turn boundary: two blank lines between segments ⇒ three
        // consecutive newlines in the joined body. `rsplit_once`
        // anchors on the LAST boundary, so multi-segment bodies
        // collapse to the most recent segment.
        let body = "banner\n\n\nbrief echo\n\n\nworker reply";
        assert_eq!(strip_to_last_turn(body), "worker reply");
    }

    #[test]
    fn strip_to_last_turn_preserves_intra_reply_paragraph_break() {
        // Intra-reply paragraph break: one blank line = two newlines.
        // Must NOT trigger the split — otherwise legitimate multi-
        // paragraph replies would be truncated to their last paragraph.
        let body = "Reply paragraph 1.\n\nReply paragraph 2 with details.";
        assert_eq!(strip_to_last_turn(body), body,
            "single blank line between paragraphs is not a turn boundary");
    }

    #[test]
    fn strip_to_last_turn_no_boundary_returns_unchanged() {
        // No triple newline anywhere — pre-polish shape (codex /
        // gemini post-boot fixtures, short single-turn cursor reply).
        // Must pass through verbatim.
        assert_eq!(strip_to_last_turn("just a single line"), "just a single line");
        assert_eq!(strip_to_last_turn("line1\nline2"), "line1\nline2");
        assert_eq!(strip_to_last_turn(""), "");
    }

    #[test]
    fn strip_to_last_turn_handles_extra_separator_newlines() {
        // A separator of 4+ newlines leaves one behind in the tail
        // after `rsplit_once` on the 3-newline needle. The helper's
        // `trim_start_matches('\n')` mops that up so the returned
        // body has no leading blank line.
        let body = "banner\n\n\n\nreply";
        assert_eq!(strip_to_last_turn(body), "reply");
        let body5 = "banner\n\n\n\n\nreply";
        assert_eq!(strip_to_last_turn(body5), "reply");
    }

    #[test]
    fn strip_to_last_turn_preserves_markdown_indent() {
        // Markdown code-block / list indentation inside the last turn
        // survives. Only the leading BLANK / newline slice is trimmed,
        // not leading spaces on the first content line.
        let body = "banner\n\n\n    code_block_line\n    second_line";
        assert_eq!(
            strip_to_last_turn(body),
            "    code_block_line\n    second_line"
        );
    }

    #[test]
    fn extract_cursor_fresh_session_strips_banner_and_brief_echo() {
        // Verbatim shape of the T5 smoke-test capture that motivated
        // T7. Pre-polish extract_reply returned the whole banner +
        // echoed task brief + reply concatenation (390 B body, summary
        // paragraph = just the banner). Post-polish only the worker's
        // reply survives.
        let lines = lines(&[
            "  Cursor Agent",
            "  v2026.04.17-479fd04",
            "  hint: /auto-run to skip all approvals",
            "",
            "",
            "  [VALIDATE] T7 smoke test",
            "  Just print 'T7 smoke ok' and stop.",
            "",
            "",
            "  T7 smoke ok",
            "",
            "",
            "  → Add a follow-up",
            "",
            "",
            "  Composer 2 Fast",
            "  /home/fett/Projects/Alor",
        ]);
        let body = AgentKind::Cursor.extract_reply(&lines)
            .expect("reply extractable");
        // Only the worker reply survives. 2-space cursor-TUI indent
        // is preserved (it's the pane's left margin; stripping it
        // risks losing legitimate markdown indentation in other
        // replies — pre-existing contract).
        assert_eq!(body, "  T7 smoke ok",
            "banner + brief echo stripped, reply survives: {body:?}");
    }

    #[test]
    fn extract_cursor_multi_task_history_keeps_only_last_reply() {
        // Multi-task pane: T5 reply + T8 brief + T8 reply all visible
        // above the current `→` input. Worker's MOST RECENT reply is
        // what the orchestrator wants — prior completions already
        // have their own task records. `rsplit_once` at the LAST
        // triple-newline gives that.
        let lines = lines(&[
            "  Cursor Agent",
            "  v2026.04.17-479fd04",
            "",
            "",
            "  [T5] smoke",
            "  Print 'T5 ok'.",
            "",
            "",
            "  T5 ok",
            "",
            "",
            "  [T8] later smoke",
            "  Print 'T8 ok'.",
            "",
            "",
            "  T8 ok",
            "",
            "",
            "  → Add a follow-up",
        ]);
        let body = AgentKind::Cursor.extract_reply(&lines).expect("has reply");
        assert_eq!(body, "  T8 ok",
            "rsplit_once isolates the last turn even with 3+ prior turns in scrollback: {body:?}");
    }

    #[test]
    fn extract_multi_paragraph_reply_survives_intact() {
        // Regression guard: the T7 polish must NOT truncate a reply
        // whose own paragraphs are separated by a single blank line.
        // This fixture has a 3-paragraph reply above the cursor cutoff;
        // all three paragraphs must survive extraction.
        let lines = lines(&[
            "First paragraph of the reply.",
            "",
            "Second paragraph with more detail.",
            "",
            "Third paragraph concluding.",
            "",
            "",
            "  → Add a follow-up",
        ]);
        let body = AgentKind::Cursor.extract_reply(&lines).expect("has reply");
        assert!(body.contains("First paragraph"),
            "multi-paragraph reply: para 1 survives: {body:?}");
        assert!(body.contains("Second paragraph"),
            "multi-paragraph reply: para 2 survives: {body:?}");
        assert!(body.contains("Third paragraph"),
            "multi-paragraph reply: para 3 survives: {body:?}");
    }

    // ---- split_summary_details ----

    #[test]
    fn split_empty_returns_none_none() {
        assert_eq!(split_summary_details(""), (None, None));
        assert_eq!(split_summary_details("   \n  \n"), (None, None));
    }

    #[test]
    fn split_short_body_is_summary_only() {
        let body = "Short verdict. Nothing more.";
        let (summary, details) = split_summary_details(body);
        assert_eq!(summary.as_deref(), Some("Short verdict. Nothing more."));
        assert!(details.is_none(), "short body → no details: {details:?}");
    }

    #[test]
    fn split_long_body_with_paragraph_break_splits_at_break() {
        // First paragraph under 400 B; second paragraph pushes total
        // over the terse target. Note: `split_summary_details` trims
        // the body first, so we compare against the trimmed form.
        let verdict = "Fixed the bug in commands.rs:147. Tests green.";
        let long_tail = "\n\n".to_string() + &"Detailed notes: ".repeat(40);
        let body = format!("{verdict}{long_tail}");
        let (summary, details) = split_summary_details(&body);
        assert_eq!(summary.as_deref(), Some(verdict),
            "summary = first paragraph before \\n\\n: {summary:?}");
        // Details carries the trimmed body — trailing whitespace from
        // the "Detailed notes: " repeat gets stripped by the
        // function's leading `body.trim()`.
        assert_eq!(details.as_deref(), Some(body.trim()),
            "details = full body (trimmed) when under DETAILS_MAX");
    }

    #[test]
    fn split_long_body_without_paragraph_break_hard_cuts_with_marker() {
        // 500 B of prose without any paragraph break → hard-cut at
        // REPLY_TERSE_TARGET with `…` marker.
        let body = "x".repeat(500);
        let (summary, details) = split_summary_details(&body);
        let s = summary.expect("summary present");
        assert!(s.ends_with('…'), "hard-cut carries `…` marker: {s:?}");
        assert!(s.len() <= REPLY_TERSE_TARGET + "…".len(),
            "summary stays near the terse target");
        assert_eq!(details.as_deref(), Some(body.as_str()),
            "full body lives in details");
    }

    #[test]
    fn split_preserves_utf8_at_char_boundary() {
        // Multi-byte char straddling the 400 B boundary would be
        // truncated mid-codepoint without the char_boundary walk-back.
        // This fixture puts a 3-byte char right at the boundary.
        let filler = "x".repeat(REPLY_TERSE_TARGET - 2);
        let body = format!("{filler}€€€€ tail");  // `€` is 3 bytes each in UTF-8
        let (summary, _details) = split_summary_details(&body);
        let s = summary.expect("has summary");
        // Round-trip: the summary must decode cleanly as UTF-8 (String
        // guarantees this at the type level; we're really asserting
        // no trailing lone-byte hexits sneak through the hard-cut).
        assert!(s.len() <= REPLY_TERSE_TARGET + "…".len() + 4,
            "stays within a handful of bytes of target: len={}", s.len());
        // ASCII filler prefix survives intact.
        assert!(s.starts_with(&"x".repeat(REPLY_TERSE_TARGET - 2)));
    }

    #[test]
    fn split_details_capped_at_max() {
        // Body larger than REPLY_DETAILS_MAX gets truncated with the
        // `\n…[truncated]` marker.
        let body = "y".repeat(REPLY_DETAILS_MAX + 1000);
        let (_summary, details) = split_summary_details(&body);
        let d = details.expect("details present");
        assert!(d.ends_with("\n…[truncated]"),
            "oversized details carry truncation marker: last-bytes={:?}",
            &d[d.len().saturating_sub(20)..]);
        assert!(d.len() <= REPLY_DETAILS_MAX + "\n…[truncated]".len(),
            "details capped near REPLY_DETAILS_MAX");
    }

    // ---- looks_truncated ----

    #[test]
    fn looks_truncated_detects_unclosed_codeblock() {
        assert!(looks_truncated("Here is code:\n```rust\nfn main() {"),
            "odd fence count → looks partial");
        assert!(!looks_truncated("Here is code:\n```rust\nfn main() {}\n```"),
            "balanced fences → complete");
        assert!(!looks_truncated("No code here, just prose."));
        assert!(!looks_truncated(""));
    }

    // ---- from_name routing ----

    #[test]
    fn from_name_routes_known_runtimes() {
        assert!(matches!(AgentKind::from_name("claude"), AgentKind::ClaudeCode));
        assert!(matches!(AgentKind::from_name("claude-alor"), AgentKind::ClaudeCode));
        assert!(matches!(AgentKind::from_name("codex"), AgentKind::Codex));
        assert!(matches!(AgentKind::from_name("codex-reviewer"), AgentKind::Codex));
        assert!(matches!(AgentKind::from_name("gemini"), AgentKind::Gemini));
        assert!(matches!(AgentKind::from_name("cursor"), AgentKind::Cursor));
        assert!(matches!(AgentKind::from_name("cursor-agent"), AgentKind::Cursor));
        assert!(matches!(AgentKind::from_name("weird-unknown"), AgentKind::Default));
    }

    // ---- trust_prompts per-kind ----

    #[test]
    fn trust_prompts_per_kind() {
        // ClaudeCode: single distinctive intro line. Unused in practice
        // (claude yamls are claude-sdk runtime) but future-proofs a
        // hypothetical wrapper-runtime claude slot.
        assert_eq!(
            AgentKind::ClaudeCode.trust_prompts(),
            &[("Quick safety check", "1")]
        );

        // Codex: TWO ordered prompts. Update prompt first (ack "3" =
        // Skip until next version), then the trust prompt (ack "1" =
        // Yes, continue). Order matters — the wrapper polls through
        // them as the CLI renders them sequentially.
        assert_eq!(
            AgentKind::Codex.trust_prompts(),
            &[
                ("Update available!", "3"),
                ("Do you trust the contents", "1"),
            ]
        );
        // Explicit ordering assertion — the update prompt MUST come
        // first so we never accidentally ack the trust prompt with
        // "3" (which would map to a non-existent option).
        let codex_prompts = AgentKind::Codex.trust_prompts();
        assert_eq!(codex_prompts.len(), 2);
        assert_eq!(codex_prompts[0].0, "Update available!");
        assert_eq!(codex_prompts[1].0, "Do you trust the contents");

        // Gemini: distinctive "Trusting a folder" intro, ack "1".
        assert_eq!(
            AgentKind::Gemini.trust_prompts(),
            &[("Trusting a folder", "1")]
        );

        // Cursor: box header "Workspace Trust Required", ack "a".
        // Confirmed via live probe in /tmp and $HOME — cursor
        // re-prompts every session, doesn't persist trust state.
        assert_eq!(
            AgentKind::Cursor.trust_prompts(),
            &[("Workspace Trust Required", "a")]
        );

        // Default: empty, no catch-all. Keeps fail-open semantics for
        // unknown runtimes — we'd rather a new CLI hang once at its
        // trust dialog than splat unrelated keystrokes into its pane.
        assert_eq!(
            AgentKind::Default.trust_prompts(),
            &[] as &[(&str, &str)]
        );
    }
}
