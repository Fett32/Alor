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

    /// Substring hints for matching the runtime's first-launch
    /// trust-folder dialog. Per-runtime because each CLI phrases the
    /// dialog differently; returning an empty slice means "no dialog
    /// expected, skip the trust-ack step."
    ///
    /// Kept as substrings (not regex) so lookups stay `str::contains`
    /// cheap and the match is robust to minor TUI layout changes.
    pub fn trust_prompt_hints(&self) -> &'static [&'static str] {
        match self {
            // Claude Code's trust dialog opens with
            //   "Quick safety check: Is this a project you created or one you trust?"
            // and offers "1. Yes, I trust this folder" / "2. No, exit".
            // The "trust this folder" option text happens to collide
            // with codex's dialog wording, but this refactor matches on
            // the distinctive intro line instead so each runtime keeps
            // its own signal.
            //
            // In the current registry all claude yamls are runtime:
            // claude-sdk (run-worker.sh) and never hit this CLI prompt,
            // so the hint is future-proofing: a wrapper-runtime claude
            // slot won't hang on trust-ack just because nobody
            // remembered to add hints when registering it.
            AgentKind::ClaudeCode => &["Quick safety check"],
            AgentKind::Codex => &["trust this folder"],
            AgentKind::Gemini => &["Trusting a folder"],
            AgentKind::Cursor => &["Workspace Trust Required"],
            AgentKind::Default => &[],
        }
    }

    /// Key to inject to acknowledge the trust dialog when one of the
    /// `trust_prompt_hints()` is matched. No-op for kinds with empty
    /// hints — the caller never reaches this for them.
    pub fn trust_ack_key(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "1",
            AgentKind::Codex => "1",
            AgentKind::Gemini => "1",
            AgentKind::Cursor => "a",
            // Default's hints are empty so this value is unreachable
            // at runtime; locked to the no-op empty string anyway so
            // a future hint addition has to make an explicit ack-key
            // choice, not inherit a stale "1".
            AgentKind::Default => "",
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

    // ---- trust-prompt data ----

    #[test]
    fn trust_prompt_hints_per_kind() {
        // Claude: matches the distinctive intro line "Quick safety
        // check" (not the "trust this folder" option text, which is
        // also present in codex's dialog — each runtime keeps its own
        // signal under the principled refactor).
        assert_eq!(
            AgentKind::ClaudeCode.trust_prompt_hints(),
            &["Quick safety check"]
        );

        // Codex: the classic "Trust this folder" wording.
        assert_eq!(
            AgentKind::Codex.trust_prompt_hints(),
            &["trust this folder"]
        );

        // Gemini: distinctive "Trusting a folder" dialog intro.
        assert_eq!(
            AgentKind::Gemini.trust_prompt_hints(),
            &["Trusting a folder"]
        );

        // Cursor: box header "Workspace Trust Required" (confirmed via
        // live probe in /tmp and $HOME — cursor re-prompts every
        // session, doesn't persist trust state).
        assert_eq!(
            AgentKind::Cursor.trust_prompt_hints(),
            &["Workspace Trust Required"]
        );

        // Default: empty, no catch-all. Keeps fail-open semantics for
        // unknown runtimes — we'd rather a new CLI hang once at its
        // trust dialog than splat unrelated keystrokes into its pane.
        assert_eq!(AgentKind::Default.trust_prompt_hints(), &[] as &[&str]);
    }

    #[test]
    fn trust_ack_key_per_kind() {
        assert_eq!(AgentKind::ClaudeCode.trust_ack_key(), "1");
        assert_eq!(AgentKind::Codex.trust_ack_key(), "1");
        assert_eq!(AgentKind::Gemini.trust_ack_key(), "1");
        assert_eq!(AgentKind::Cursor.trust_ack_key(), "a");
        // Default's value is unused (empty hints skip the ack path
        // entirely), locked here anyway to prevent drift.
        assert_eq!(AgentKind::Default.trust_ack_key(), "");
    }
}
