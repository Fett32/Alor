use regex::Regex;

/// Known agent types with their idle-prompt patterns.
#[derive(Debug, Clone)]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    Gemini,
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
            AgentKind::Default => r"^[\$>\+]\s*$",
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
    fn default_idle() {
        let d = IdleDetector::new(&AgentKind::Default);
        assert!(d.is_idle(&lines(&["$ "])));
        assert!(d.is_idle(&lines(&["> "])));
        assert!(!d.is_idle(&lines(&["$ running something"])));
    }
}
