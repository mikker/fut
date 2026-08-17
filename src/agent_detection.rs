use crate::domain::AgentState;

pub const IDLE_CONFIRMATIONS: u8 = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodexProbe {
    NotRunning,
    Screen(String),
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodexDetection {
    pub state: AgentState,
    pub rule: &'static str,
}

pub fn is_codex_process(name: &str, command: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if name == "codex" || name.starts_with("codex-") {
        return true;
    }

    let command = command.to_ascii_lowercase();
    (name == "node" || name.starts_with("node-"))
        && command.split_whitespace().skip(1).any(|argument| {
            argument
                .split(['/', '\\'])
                .any(|component| component == "codex" || component == "codex.js")
        })
}

pub fn detect_codex(screen: &str) -> CodexDetection {
    let lower = screen.to_ascii_lowercase();
    let live = after_last_prompt_marker(&lower);

    if live.contains("press enter to confirm or esc to cancel")
        || live.contains("enter to submit answer")
        || live.contains("enter to submit all")
        || live.contains("allow command?")
    {
        return detection(AgentState::Blocked, "live_blocker");
    }
    if lower.contains("do you trust the contents of this directory?")
        || lower.contains("[y/n]")
        || lower.contains("yes (y)")
    {
        return detection(AgentState::Blocked, "confirmation_prompt");
    }
    if has_live_working_indicator(screen) {
        return detection(AgentState::Working, "working_indicator");
    }

    detection(AgentState::Idle, "idle_fallback")
}

fn detection(state: AgentState, rule: &'static str) -> CodexDetection {
    CodexDetection { state, rule }
}

fn after_last_prompt_marker(text: &str) -> &str {
    ["›", "❯", "> "]
        .into_iter()
        .filter_map(|marker| text.rfind(marker).map(|index| &text[index..]))
        .min_by_key(|text| text.len())
        .unwrap_or(text)
}

fn has_live_working_indicator(text: &str) -> bool {
    let lines = bottom_non_empty_lines(text, 6);
    let Some(index) = lines.iter().rposition(|line| {
        let line = line.trim_start();
        (line.starts_with("• Working (") || line.starts_with("◦ Working ("))
            && line.contains("esc to interrupt")
    }) else {
        return false;
    };

    !lines[index + 1..].iter().any(|line| {
        let line = line.trim_start();
        line.starts_with('•')
            || line.starts_with('■')
            || line.starts_with('✓')
            || line.starts_with('✗')
    })
}

fn bottom_non_empty_lines(text: &str, count: usize) -> Vec<&str> {
    let mut lines = text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(count)
        .collect::<Vec<_>>();
    lines.reverse();
    lines
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CodexStabilizer {
    published: Option<AgentState>,
    idle_confirmations: u8,
}

impl CodexStabilizer {
    pub fn observe(&mut self, detected: AgentState) -> Option<AgentState> {
        if self.published == Some(AgentState::Working) && detected == AgentState::Idle {
            self.idle_confirmations += 1;
            if self.idle_confirmations < IDLE_CONFIRMATIONS {
                return None;
            }
        } else {
            self.idle_confirmations = 0;
        }
        if self.published == Some(detected) {
            return Some(detected);
        }
        self.published = Some(detected);
        self.idle_confirmations = 0;
        Some(detected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_codex_process_variants() {
        assert!(is_codex_process("codex", "codex"));
        assert!(is_codex_process(
            "codex-aarch64-apple-darwin",
            "codex-aarch64-apple-darwin"
        ));
        assert!(is_codex_process(
            "node",
            "node /opt/node_modules/@openai/codex/bin/codex.js"
        ));
        assert!(!is_codex_process("node", "node server.js"));
    }

    #[test]
    fn classifies_representative_codex_screens() {
        assert_eq!(
            detect_codex("› Build it\n\n• Working (3s • esc to interrupt)").state,
            AgentState::Working
        );
        assert_eq!(
            detect_codex("› Run command?\nAllow command?\npress enter to confirm or esc to cancel")
                .state,
            AgentState::Blocked
        );
        assert_eq!(
            detect_codex("Do you trust the contents of this directory?\n[y/n]").state,
            AgentState::Blocked
        );
        assert_eq!(detect_codex("Done.\n\n› ").state, AgentState::Idle);
    }

    #[test]
    fn blocker_in_old_transcript_does_not_override_live_prompt() {
        assert_eq!(
            detect_codex("Allow command?\n› new prompt").state,
            AgentState::Idle
        );
    }

    #[test]
    fn completed_block_after_working_indicator_is_idle() {
        let screen = "◦ Working (3s • esc to interrupt)\n\n• Implemented the change.\n\n› ";
        assert_eq!(detect_codex(screen).state, AgentState::Idle);

        let interrupted = "◦ Working (3s • esc to interrupt)\n■ Conversation interrupted\n› ";
        assert_eq!(detect_codex(interrupted).state, AgentState::Idle);
    }

    #[test]
    fn prompt_footer_does_not_hide_live_working_indicator() {
        let screen =
            "◦ Working (3s • esc to interrupt)\n\n› Use /skills to list skills\nmodel · /work";
        assert_eq!(detect_codex(screen).state, AgentState::Working);
    }

    #[test]
    fn working_to_idle_requires_three_observations() {
        let mut stabilizer = CodexStabilizer::default();
        assert_eq!(
            stabilizer.observe(AgentState::Working),
            Some(AgentState::Working)
        );
        assert_eq!(stabilizer.observe(AgentState::Idle), None);
        assert_eq!(stabilizer.observe(AgentState::Idle), None);
        assert_eq!(stabilizer.observe(AgentState::Idle), Some(AgentState::Idle));
    }
}
