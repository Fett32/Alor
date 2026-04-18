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
            AgentKind::Codex => r"^codex>",
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
    /// Other runtimes: None — their `pattern()` already encodes a
    /// bottom-anchored shell prompt that naturally disappears during
    /// streaming output, so no additional anti-pattern is needed.
    fn active_pattern(&self) -> Option<&str> {
        match self {
            AgentKind::Cursor => Some(r"ctrl\+c to stop"),
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
    /// Making this runtime-specific (rather than bumping a global
    /// constant) keeps the other runtimes' detection cheap and tight.
    pub fn idle_tail_window(&self) -> usize {
        match self {
            AgentKind::Cursor => 15,
            _ => 5,
        }
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
        let d = IdleDetector::new(&AgentKind::Codex);
        assert!(d.is_idle(&lines(&["codex> "])));
        assert!(!d.is_idle(&lines(&["$ "])));
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
        assert_eq!(AgentKind::Codex.idle_tail_window(), 5);
        assert_eq!(AgentKind::Gemini.idle_tail_window(), 5);
        assert_eq!(AgentKind::Default.idle_tail_window(), 5);
        // Cursor needs a wider window — see `idle_tail_window()`
        // docstring for the probe-derived sizing.
        assert_eq!(AgentKind::Cursor.idle_tail_window(), 15);
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
    fn default_idle() {
        let d = IdleDetector::new(&AgentKind::Default);
        assert!(d.is_idle(&lines(&["$ "])));
        assert!(d.is_idle(&lines(&["> "])));
        assert!(!d.is_idle(&lines(&["$ running something"])));
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
