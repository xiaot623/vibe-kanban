use crate::executors::{BaseCodingAgent, claude, codex, opencode, pi};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptCommand {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptCommandSpec {
    pub primary: ReceiptCommand,
    pub fallback: ReceiptCommand,
}

pub trait ExecutorReceiptSupport {
    fn receipt_command_spec(&self, agent_session_id: &str) -> Option<ReceiptCommandSpec>;
    fn receipt_slug(&self) -> &'static str;
    fn receipt_display_name(&self) -> &'static str;
}

impl ExecutorReceiptSupport for BaseCodingAgent {
    fn receipt_command_spec(&self, agent_session_id: &str) -> Option<ReceiptCommandSpec> {
        match self {
            BaseCodingAgent::ClaudeCode => Some(claude::receipt_command_spec(agent_session_id)),
            BaseCodingAgent::Codex => Some(codex::receipt_command_spec(agent_session_id)),
            BaseCodingAgent::Opencode => Some(opencode::receipt_command_spec(agent_session_id)),
            BaseCodingAgent::Pi => Some(pi::receipt_command_spec(agent_session_id)),
            BaseCodingAgent::Gemini => None,
        }
    }

    fn receipt_slug(&self) -> &'static str {
        match self {
            BaseCodingAgent::ClaudeCode => "claude-code",
            BaseCodingAgent::Codex => "codex",
            BaseCodingAgent::Opencode => "opencode",
            BaseCodingAgent::Pi => "pi",
            BaseCodingAgent::Gemini => "gemini",
        }
    }

    fn receipt_display_name(&self) -> &'static str {
        match self {
            BaseCodingAgent::ClaudeCode => "Claude Code",
            BaseCodingAgent::Codex => "Codex",
            BaseCodingAgent::Opencode => "OpenCode",
            BaseCodingAgent::Pi => "Pi",
            BaseCodingAgent::Gemini => "Gemini",
        }
    }
}

pub fn package_command(package_name: &str, path_binary: &str) -> ReceiptCommandSpec {
    ReceiptCommandSpec {
        primary: ReceiptCommand {
            program: path_binary.to_string(),
            args: vec!["session".to_string(), "--json".to_string()],
        },
        fallback: ReceiptCommand {
            program: "npx".to_string(),
            args: vec![
                "--yes".to_string(),
                format!("{package_name}@latest"),
                "session".to_string(),
                "--json".to_string(),
            ],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_executor_receipt_commands() {
        let claude = BaseCodingAgent::ClaudeCode
            .receipt_command_spec("session-1")
            .unwrap();
        assert_eq!(claude.primary.program, "ccusage");
        assert_eq!(
            claude.primary.args,
            vec!["session", "--id", "session-1", "--json"]
        );
        assert_eq!(claude.fallback.program, "npx");
        assert_eq!(
            claude.fallback.args,
            vec![
                "--yes",
                "ccusage@latest",
                "session",
                "--id",
                "session-1",
                "--json"
            ]
        );

        let codex = BaseCodingAgent::Codex
            .receipt_command_spec("session-1")
            .unwrap();
        assert_eq!(codex.primary.program, "ccusage-codex");
        assert_eq!(
            codex.primary.args,
            vec!["session", "--json", "--id", "session-1"]
        );
        assert_eq!(
            codex.fallback.args,
            vec![
                "--yes",
                "@ccusage/codex@latest",
                "session",
                "--json",
                "--id",
                "session-1"
            ]
        );

        assert!(
            BaseCodingAgent::Gemini
                .receipt_command_spec("unsupported")
                .is_none()
        );
    }
}
