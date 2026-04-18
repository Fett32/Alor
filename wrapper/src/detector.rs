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
        IdleDetector { re, active_re }
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

    /// Convenience: check only the last `tail` lines.
    pub fn is_idle_tail(&self, lines: &[String], tail: usize) -> bool {
        let start = lines.len().saturating_sub(tail);
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
