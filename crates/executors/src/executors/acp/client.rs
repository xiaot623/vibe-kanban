use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use agent_client_protocol::{self as acp, ErrorCode};
use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, warn};
use workspace_utils::approvals::ApprovalStatus;

use crate::{
    approvals::{ExecutorApprovalError, ExecutorApprovalService},
    executors::acp::{AcpEvent, ApprovalResponse},
};

const EXIT_PLAN_MODE_NAME: &str = "ExitPlanMode";
const GEMINI_EXIT_PLAN_TOOL_CALL_ID_PREFIX: &str = "exit_plan_mode-";
const GEMINI_PLAN_APPROVAL_TITLE_PREFIX: &str = "Requesting plan approval for:";

/// ACP client that handles agent-client protocol communication
#[derive(Clone)]
pub struct AcpClient {
    event_tx: mpsc::UnboundedSender<AcpEvent>,
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
    feedback_queue: Arc<Mutex<Vec<String>>>,
    worktree_path: PathBuf,
}

impl AcpClient {
    /// Create a new ACP client
    pub fn new(
        event_tx: mpsc::UnboundedSender<AcpEvent>,
        approvals: Option<Arc<dyn ExecutorApprovalService>>,
        worktree_path: PathBuf,
    ) -> Self {
        Self {
            event_tx,
            approvals,
            feedback_queue: Arc::new(Mutex::new(Vec::new())),
            worktree_path,
        }
    }

    pub fn record_user_prompt_event(&self, prompt: &str) {
        self.send_event(AcpEvent::User(prompt.to_string()));
    }

    /// Send an event to the event channel
    fn send_event(&self, event: AcpEvent) {
        if let Err(e) = self.event_tx.send(event) {
            warn!("Failed to send ACP event: {}", e);
        }
    }

    /// Queue a user feedback message to be sent after a denial.
    pub async fn enqueue_feedback(&self, message: String) {
        let trimmed = message.trim().to_string();
        if !trimmed.is_empty() {
            let mut q = self.feedback_queue.lock().await;
            q.push(trimmed);
        }
    }

    /// Drain and return queued feedback messages.
    pub async fn drain_feedback(&self) -> Vec<String> {
        let mut q = self.feedback_queue.lock().await;
        q.drain(..).collect()
    }
}

fn is_gemini_exit_plan_tool_call(tool_call_id: &str) -> bool {
    tool_call_id.starts_with(GEMINI_EXIT_PLAN_TOOL_CALL_ID_PREFIX)
}

fn normalize_tool_name(tool_call_id: &str, fallback_title: Option<&str>) -> String {
    if is_gemini_exit_plan_tool_call(tool_call_id) {
        EXIT_PLAN_MODE_NAME.to_string()
    } else {
        fallback_title
            .filter(|title| !title.trim().is_empty())
            .unwrap_or("tool")
            .to_string()
    }
}

fn parse_plan_path_from_title(title: &str) -> Option<String> {
    let raw_path = title
        .strip_prefix(GEMINI_PLAN_APPROVAL_TITLE_PREFIX)?
        .trim();
    if raw_path.is_empty() {
        return None;
    }

    let normalized = raw_path.trim_matches(|c| matches!(c, '"' | '\'' | '`'));
    if normalized.is_empty() {
        None
    } else {
        Some(normalized.to_string())
    }
}

fn resolve_plan_path(plan_path: &str, worktree_path: &Path) -> PathBuf {
    let path = PathBuf::from(plan_path);
    if path.is_relative() {
        worktree_path.join(path)
    } else {
        path
    }
}

fn read_plan_from_title(title: &str, worktree_path: &Path) -> Option<String> {
    let plan_path = parse_plan_path_from_title(title)?;
    let resolved_path = resolve_plan_path(&plan_path, worktree_path);

    match std::fs::read_to_string(&resolved_path) {
        Ok(plan) => Some(plan),
        Err(err) => {
            debug!(
                "Failed to read plan file for Gemini exit plan mode (path: {}): {}",
                resolved_path.display(),
                err
            );
            None
        }
    }
}

fn build_approval_input(
    tool_call: &acp::ToolCallUpdate,
    tool_call_id: &str,
    worktree_path: &Path,
) -> serde_json::Value {
    let mut payload = serde_json::json!({ "tool_call": tool_call });

    if !is_gemini_exit_plan_tool_call(tool_call_id) {
        return payload;
    }

    let title = tool_call.fields.title.as_deref().unwrap_or_default();
    let plan = read_plan_from_title(title, worktree_path).unwrap_or_else(|| title.to_string());
    let plan_path = parse_plan_path_from_title(title);

    if let Some(obj) = payload.as_object_mut() {
        obj.insert("plan".to_string(), serde_json::Value::String(plan));
        if let Some(path) = plan_path {
            obj.insert("plan_path".to_string(), serde_json::Value::String(path));
        }
    }

    payload
}

#[async_trait(?Send)]
impl acp::Client for AcpClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse, acp::Error> {
        self.send_event(AcpEvent::RequestPermission(args.clone()));

        if self.approvals.is_none() {
            // Auto-approve with best available option when no approval service is configured
            let chosen_option = args
                .options
                .iter()
                .find(|o| matches!(o.kind, acp::PermissionOptionKind::AllowAlways))
                .or_else(|| {
                    args.options
                        .iter()
                        .find(|o| matches!(o.kind, acp::PermissionOptionKind::AllowOnce))
                })
                .or_else(|| args.options.first());

            let outcome = if let Some(opt) = chosen_option {
                debug!("Auto-approving permission with option: {}", opt.option_id);
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    opt.option_id.clone(),
                ))
            } else {
                warn!("No permission options available, cancelling");
                acp::RequestPermissionOutcome::Cancelled
            };

            return Ok(acp::RequestPermissionResponse::new(outcome));
        }

        let tool_call_id = args.tool_call.tool_call_id.0.to_string();
        let tool_name = normalize_tool_name(&tool_call_id, args.tool_call.fields.title.as_deref());
        let tool_input = build_approval_input(&args.tool_call, &tool_call_id, &self.worktree_path);
        let status = match self
            .approvals
            .as_ref()
            .ok_or(ExecutorApprovalError::ServiceUnavailable)
            .map_err(|_| acp::Error::invalid_request())?
            .request_tool_approval(&tool_name, tool_input, &tool_call_id)
            .await
        {
            Ok(s) => s,
            Err(err) => {
                warn!("Failed to request tool approval: {}", err);
                return Err(acp::Error::new(
                    ErrorCode::INTERNAL_ERROR.code,
                    format!("Approval request failed: {}", err),
                ));
            }
        };

        // Map our ApprovalStatus to ACP outcome
        let outcome = match &status {
            ApprovalStatus::Approved | ApprovalStatus::ProvidedInput { .. } => {
                let chosen = args
                    .options
                    .iter()
                    .find(|o| matches!(o.kind, acp::PermissionOptionKind::AllowOnce));
                if let Some(opt) = chosen {
                    acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                        opt.option_id.clone(),
                    ))
                } else {
                    tracing::error!("No suitable approval option found, cancelling");
                    return Err(acp::Error::invalid_request());
                }
            }
            ApprovalStatus::Denied { reason } => {
                // If user provided a reason, queue it to send after denial
                if let Some(feedback) = reason.as_ref() {
                    self.enqueue_feedback(feedback.clone()).await;
                }
                let chosen = args
                    .options
                    .iter()
                    .find(|o| matches!(o.kind, acp::PermissionOptionKind::RejectOnce));
                if let Some(opt) = chosen {
                    acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                        opt.option_id.clone(),
                    ))
                } else {
                    warn!("No permission options for denial, cancelling");
                    acp::RequestPermissionOutcome::Cancelled
                }
            }
            ApprovalStatus::TimedOut => {
                warn!("Approval timed out");
                acp::RequestPermissionOutcome::Cancelled
            }
            ApprovalStatus::Pending => {
                // This should not occur after waiter resolves
                warn!("Approval resolved to Pending");
                acp::RequestPermissionOutcome::Cancelled
            }
        };

        self.send_event(AcpEvent::ApprovalResponse(ApprovalResponse {
            tool_call_id: tool_call_id.clone(),
            status: status.clone(),
        }));

        Ok(acp::RequestPermissionResponse::new(outcome))
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> Result<(), acp::Error> {
        // Convert to typed events
        let event = match args.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => Some(AcpEvent::Message(chunk.content)),
            acp::SessionUpdate::AgentThoughtChunk(chunk) => Some(AcpEvent::Thought(chunk.content)),
            acp::SessionUpdate::ToolCall(tc) => Some(AcpEvent::ToolCall(tc)),
            acp::SessionUpdate::ToolCallUpdate(update) => Some(AcpEvent::ToolUpdate(update)),
            acp::SessionUpdate::Plan(plan) => Some(AcpEvent::Plan(plan)),
            _ => Some(AcpEvent::Other(args)),
        };

        if let Some(event) = event {
            self.send_event(event);
        }

        Ok(())
    }

    // File system operations - not implemented as we don't expose FS
    async fn write_text_file(
        &self,
        _args: acp::WriteTextFileRequest,
    ) -> Result<acp::WriteTextFileResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _args: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    // Terminal operations - not implemented
    async fn create_terminal(
        &self,
        _args: acp::CreateTerminalRequest,
    ) -> Result<acp::CreateTerminalResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _args: acp::TerminalOutputRequest,
    ) -> Result<acp::TerminalOutputResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: acp::ReleaseTerminalRequest,
    ) -> Result<acp::ReleaseTerminalResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: acp::WaitForTerminalExitRequest,
    ) -> Result<acp::WaitForTerminalExitResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _args: acp::KillTerminalCommandRequest,
    ) -> Result<acp::KillTerminalCommandResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    // Extension methods
    async fn ext_method(&self, _args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _args: acp::ExtNotification) -> Result<(), acp::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;

    fn create_temp_dir(test_name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vk-acp-client-{test_name}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("create temporary test directory");
        dir
    }

    #[test]
    fn normalize_tool_name_maps_exit_plan_mode_prefix() {
        let tool_name = normalize_tool_name("exit_plan_mode-42", Some("Request approval"));
        assert_eq!(tool_name, EXIT_PLAN_MODE_NAME);
    }

    #[test]
    fn build_approval_input_reads_plan_from_title_path() {
        let worktree_path = create_temp_dir("read-plan");
        let plan_path = worktree_path.join("plan.md");
        let plan_text = "# Plan\n- Step 1\n- Step 2\n";
        fs::write(&plan_path, plan_text).expect("write plan file");

        let title = format!("Requesting plan approval for: {}", plan_path.display());
        let tool_call = acp::ToolCallUpdate::new(
            "exit_plan_mode-101",
            acp::ToolCallUpdateFields::new().title(title),
        );

        let payload = build_approval_input(&tool_call, "exit_plan_mode-101", &worktree_path);
        assert_eq!(payload["plan"].as_str(), Some(plan_text));
        assert_eq!(
            payload["plan_path"].as_str(),
            Some(plan_path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn build_approval_input_falls_back_to_title_when_plan_file_missing() {
        let worktree_path = create_temp_dir("plan-fallback");
        let title = "Requesting plan approval for: missing-plan.md".to_string();
        let tool_call = acp::ToolCallUpdate::new(
            "exit_plan_mode-202",
            acp::ToolCallUpdateFields::new().title(title.clone()),
        );

        let payload = build_approval_input(&tool_call, "exit_plan_mode-202", &worktree_path);
        assert_eq!(payload["plan"].as_str(), Some(title.as_str()));
        assert_eq!(payload["plan_path"].as_str(), Some("missing-plan.md"));
    }
}
