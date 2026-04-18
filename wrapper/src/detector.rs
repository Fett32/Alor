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
            // "Composer <model>" footer line. Live-probe during
            // consolidation task showed this line present in idle state.
            // KNOWN SOFT SPOT: if `Composer` also renders during
            // generation (unconfirmed in prod), idle detection could
            // false-fire mid-task and emit a premature task-complete.
            // Worth revisiting once cursor workers see real traffic;
            // the first agent to hit this will surface it.
            AgentKind::Cursor => r"^\s*Composer\s",
            AgentKind::Default => r"^[\$>\+]\s*$",
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
    /// are subsumed here: they couldn't represent per-prompt ack keys,
    /// which broke for multi-stage runtimes like codex.
    pub fn prompt_acks(&self) -> &'static [(&'static str, &'static str)] {
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
}

impl IdleDetector {
    pub fn new(kind: &AgentKind) -> Self {
        let re = Regex::new(kind.pattern())
            .unwrap_or_else(|_| Regex::new(r"^[\$>]\s*$").unwrap());
        IdleDetector { re }
    }

    /// Returns true if any of the provided lines match the idle pattern.
    pub fn is_idle(&self, lines: &[String]) -> bool {
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
        // Must not false-match arbitrary text even if "Composer" appears
        // mid-line (word-start anchored via `^\s*`).
        assert!(!d.is_idle(&lines(&["Running Composer 2 Fast now"])));
        assert!(!d.is_idle(&lines(&["$ "])));
        // Still rejects non-cursor idle prompts.
        assert!(!d.is_idle(&lines(&["codex> "])));
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

    // ---- prompt_acks per-kind ----

    #[test]
    fn prompt_acks_per_kind() {
        // ClaudeCode: single distinctive intro line. Unused in practice
        // (claude yamls are claude-sdk runtime) but future-proofs a
        // hypothetical wrapper-runtime claude slot.
        assert_eq!(
            AgentKind::ClaudeCode.prompt_acks(),
            &[("Quick safety check", "1")]
        );

        // Codex: TWO ordered prompts. Update prompt first (ack "3" =
        // Skip until next version), then the trust prompt (ack "1" =
        // Yes, continue). Order matters — the wrapper polls through
        // them as the CLI renders them sequentially.
        assert_eq!(
            AgentKind::Codex.prompt_acks(),
            &[
                ("Update available!", "3"),
                ("Do you trust the contents", "1"),
            ]
        );
        // Explicit ordering assertion — the update prompt MUST come
        // first so we never accidentally ack the trust prompt with
        // "3" (which would map to a non-existent option).
        let codex_prompts = AgentKind::Codex.prompt_acks();
        assert_eq!(codex_prompts.len(), 2);
        assert_eq!(codex_prompts[0].0, "Update available!");
        assert_eq!(codex_prompts[1].0, "Do you trust the contents");

        // Gemini: distinctive "Trusting a folder" intro, ack "1".
        assert_eq!(
            AgentKind::Gemini.prompt_acks(),
            &[("Trusting a folder", "1")]
        );

        // Cursor: box header "Workspace Trust Required", ack "a".
        // Confirmed via live probe in /tmp and $HOME — cursor
        // re-prompts every session, doesn't persist trust state.
        assert_eq!(
            AgentKind::Cursor.prompt_acks(),
            &[("Workspace Trust Required", "a")]
        );

        // Default: empty, no catch-all. Keeps fail-open semantics for
        // unknown runtimes — we'd rather a new CLI hang once at its
        // trust dialog than splat unrelated keystrokes into its pane.
        assert_eq!(
            AgentKind::Default.prompt_acks(),
            &[] as &[(&str, &str)]
        );
    }
}
