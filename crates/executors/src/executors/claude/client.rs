use std::sync::Arc;

use tokio::process::Command;
use workspace_utils::approvals::ApprovalStatus;

use crate::{
    approvals::{ExecutorApprovalError, ExecutorApprovalService},
    env::RepoContext,
    executors::{
        ExecutorError,
        claude::{
            ClaudeJson,
            types::{PermissionResult, PermissionUpdate},
        },
        codex::client::LogWriter,
    },
};

const EXIT_PLAN_MODE_NAME: &str = "ExitPlanMode";
const ASK_USER_QUESTION_NAME: &str = "AskUserQuestion";
pub const AUTO_APPROVE_CALLBACK_ID: &str = "AUTO_APPROVE_CALLBACK_ID";
pub const STOP_GIT_CHECK_CALLBACK_ID: &str = "STOP_GIT_CHECK_CALLBACK_ID";

/// Claude Agent client with control protocol support
pub struct ClaudeAgentClient {
    log_writer: LogWriter,
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
    auto_approve: bool, // true when approvals is None
    repo_context: RepoContext,
}

impl ClaudeAgentClient {
    /// Create a new client with optional approval service
    pub fn new(
        log_writer: LogWriter,
        approvals: Option<Arc<dyn ExecutorApprovalService>>,
        repo_context: RepoContext,
    ) -> Arc<Self> {
        let auto_approve = approvals.is_none();
        Arc::new(Self {
            log_writer,
            approvals,
            auto_approve,
            repo_context,
        })
    }

    async fn handle_approval(
        &self,
        tool_use_id: String,
        tool_name: String,
        tool_input: serde_json::Value,
    ) -> Result<PermissionResult, ExecutorError> {
        // Use approval service to request tool approval
        let approval_service = self
            .approvals
            .as_ref()
            .ok_or(ExecutorApprovalError::ServiceUnavailable)?;
        let status = approval_service
            .request_tool_approval(&tool_name, tool_input.clone(), &tool_use_id)
            .await;
        match status {
            Ok(status) => {
                // Log the approval response so we it appears in the executor logs
                self.log_writer
                    .log_raw(&serde_json::to_string(&ClaudeJson::ApprovalResponse {
                        call_id: tool_use_id.clone(),
                        tool_name: tool_name.clone(),
                        approval_status: status.clone(),
                    })?)
                    .await?;

                if tool_name == ASK_USER_QUESTION_NAME {
                    return Ok(match status {
                        ApprovalStatus::ProvidedInput { input } => PermissionResult::Allow {
                            updated_input: input,
                            updated_permissions: None,
                        },
                        ApprovalStatus::Denied { reason } => PermissionResult::Deny {
                            message: reason.unwrap_or("Denied by user".to_string()),
                            interrupt: Some(false),
                        },
                        ApprovalStatus::TimedOut => PermissionResult::Deny {
                            message: "Approval request timed out".to_string(),
                            interrupt: Some(false),
                        },
                        ApprovalStatus::Approved => PermissionResult::Deny {
                            message: "AskUserQuestion requires submitted answers in updated_input.answers"
                                .to_string(),
                            interrupt: Some(false),
                        },
                        ApprovalStatus::Pending => PermissionResult::Deny {
                            message: "Approval still pending (unexpected)".to_string(),
                            interrupt: Some(false),
                        },
                    });
                }

                match status {
                    ApprovalStatus::Approved => {
                        if tool_name == EXIT_PLAN_MODE_NAME {
                            Ok(PermissionResult::Deny {
                                message:
                                    "Plan accepted. A subtask has been created to implement the plan."
                                        .to_string(),
                                interrupt: Some(true),
                            })
                        } else {
                            Ok(PermissionResult::Allow {
                                updated_input: tool_input,
                                updated_permissions: None,
                            })
                        }
                    }
                    ApprovalStatus::ProvidedInput { input } => Ok(PermissionResult::Allow {
                        updated_input: input,
                        updated_permissions: None,
                    }),
                    ApprovalStatus::Denied { reason } => {
                        let message = reason.unwrap_or("Denied by user".to_string());
                        Ok(PermissionResult::Deny {
                            message,
                            interrupt: Some(false),
                        })
                    }
                    ApprovalStatus::TimedOut => Ok(PermissionResult::Deny {
                        message: "Approval request timed out".to_string(),
                        interrupt: Some(false),
                    }),
                    ApprovalStatus::Pending => Ok(PermissionResult::Deny {
                        message: "Approval still pending (unexpected)".to_string(),
                        interrupt: Some(false),
                    }),
                }
            }
            Err(e) => {
                tracing::error!("Tool approval request failed: {e}");
                Ok(PermissionResult::Deny {
                    message: "Tool approval request failed".to_string(),
                    interrupt: Some(false),
                })
            }
        }
    }

    pub async fn on_can_use_tool(
        &self,
        tool_name: String,
        input: serde_json::Value,
        _permission_suggestions: Option<Vec<PermissionUpdate>>,
        tool_use_id: Option<String>,
    ) -> Result<PermissionResult, ExecutorError> {
        if self.auto_approve {
            if tool_name == ASK_USER_QUESTION_NAME {
                return Ok(PermissionResult::Deny {
                    message:
                        "AskUserQuestion requires structured answers but approvals are disabled"
                            .to_string(),
                    interrupt: Some(false),
                });
            }
            Ok(PermissionResult::Allow {
                updated_input: input,
                updated_permissions: None,
            })
        } else if let Some(latest_tool_use_id) = tool_use_id {
            self.handle_approval(latest_tool_use_id, tool_name, input)
                .await
        } else {
            // Auto approve tools with no matching tool_use_id
            // tool_use_id is undocumented so this may not be possible
            tracing::warn!(
                "No tool_use_id available for tool '{}', cannot request approval",
                tool_name
            );
            Ok(PermissionResult::Allow {
                updated_input: input,
                updated_permissions: None,
            })
        }
    }

    pub async fn on_hook_callback(
        &self,
        callback_id: String,
        _input: serde_json::Value,
        _tool_use_id: Option<String>,
    ) -> Result<serde_json::Value, ExecutorError> {
        // Stop hook git check - uses `decision` (approve/block) and `reason` fields
        if callback_id == STOP_GIT_CHECK_CALLBACK_ID {
            return Ok(check_git_status(&self.repo_context).await);
        }

        if self.auto_approve {
            Ok(serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
                    "permissionDecisionReason": "Auto-approved by SDK"
                }
            }))
        } else {
            match callback_id.as_str() {
                AUTO_APPROVE_CALLBACK_ID => Ok(serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "permissionDecisionReason": "Approved by SDK"
                    }
                })),
                _ => {
                    // Hook callbacks is only used to forward approval requests to can_use_tool.
                    // This works because `ask` decision in hook callback triggers a can_use_tool request
                    // https://docs.claude.com/en/api/agent-sdk/permissions#permission-flow-diagram
                    Ok(serde_json::json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PreToolUse",
                            "permissionDecision": "ask",
                            "permissionDecisionReason": "Forwarding to canusetool service"
                        }
                    }))
                }
            }
        }
    }

    pub async fn on_non_control(&self, line: &str) -> Result<(), ExecutorError> {
        // Forward all non-control messages to stdout
        self.log_writer.log_raw(line).await
    }
}

/// Check for uncommitted git changes across all repos in the workspace.
/// Returns a Stop hook response using `decision` (approve/block) and `reason` fields.
async fn check_git_status(repo_context: &RepoContext) -> serde_json::Value {
    let repo_paths = repo_context.repo_paths();

    if repo_paths.is_empty() {
        return serde_json::json!({"decision": "approve"});
    }

    let mut all_status = String::new();

    for repo_path in &repo_paths {
        if !repo_path.join(".git").exists() {
            continue;
        }

        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(repo_path)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await;

        if let Ok(out) = output
            && !out.stdout.is_empty()
        {
            let status = String::from_utf8_lossy(&out.stdout);
            all_status.push_str(&format!("\n{}:\n{}", repo_path.display(), status));
        }
    }

    if all_status.is_empty() {
        // No uncommitted changes in any repo
        serde_json::json!({"decision": "approve"})
    } else {
        // Has uncommitted changes, block stop
        serde_json::json!({
            "decision": "block",
            "reason": format!(
                "There are uncommitted changes. Please stage and commit them now with a descriptive commit message.{}",
                all_status
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::approvals::ExecutorApprovalService;

    struct StaticApprovalService {
        status: ApprovalStatus,
    }

    #[async_trait::async_trait]
    impl ExecutorApprovalService for StaticApprovalService {
        async fn request_tool_approval(
            &self,
            _tool_name: &str,
            _tool_input: serde_json::Value,
            _tool_call_id: &str,
        ) -> Result<ApprovalStatus, ExecutorApprovalError> {
            Ok(self.status.clone())
        }
    }

    fn client_with_status(status: ApprovalStatus) -> Arc<ClaudeAgentClient> {
        ClaudeAgentClient::new(
            LogWriter::new(tokio::io::sink()),
            Some(Arc::new(StaticApprovalService { status })),
            RepoContext::default(),
        )
    }

    #[tokio::test]
    async fn ask_user_question_with_provided_input_allows_with_updated_input() {
        let expected_input = json!({
            "questions": [],
            "answers": {
                "q1": ["option_a"]
            }
        });
        let client = client_with_status(ApprovalStatus::ProvidedInput {
            input: expected_input.clone(),
        });

        let result = client
            .on_can_use_tool(
                ASK_USER_QUESTION_NAME.to_string(),
                json!({"questions": []}),
                None,
                Some("tool-use-1".to_string()),
            )
            .await
            .unwrap();

        match result {
            PermissionResult::Allow { updated_input, .. } => {
                assert_eq!(updated_input, expected_input)
            }
            other => panic!("expected allow result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_user_question_denied_or_timed_out_returns_deny() {
        for status in [
            ApprovalStatus::Denied {
                reason: Some("no".to_string()),
            },
            ApprovalStatus::TimedOut,
        ] {
            let client = client_with_status(status);
            let result = client
                .on_can_use_tool(
                    ASK_USER_QUESTION_NAME.to_string(),
                    json!({"questions": []}),
                    None,
                    Some("tool-use-1".to_string()),
                )
                .await
                .unwrap();

            assert!(matches!(result, PermissionResult::Deny { .. }));
        }
    }

    #[tokio::test]
    async fn ask_user_question_approved_without_answers_returns_explained_deny() {
        let client = client_with_status(ApprovalStatus::Approved);
        let result = client
            .on_can_use_tool(
                ASK_USER_QUESTION_NAME.to_string(),
                json!({"questions": []}),
                None,
                Some("tool-use-1".to_string()),
            )
            .await
            .unwrap();

        match result {
            PermissionResult::Deny { message, .. } => {
                assert!(message.contains("updated_input.answers"));
            }
            other => panic!("expected deny result, got {other:?}"),
        }
    }
}
