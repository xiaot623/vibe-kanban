use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    io,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;
use codex_app_server_protocol::{
    ApplyPatchApprovalResponse, AskForApproval, ClientInfo, ClientNotification, ClientRequest,
    CommandExecutionApprovalDecision, CommandExecutionRequestApprovalResponse,
    DynamicToolCallResponse, ExecCommandApprovalResponse, FileChangeApprovalDecision,
    FileChangeRequestApprovalResponse, GetAccountParams, GetAccountResponse,
    InitializeCapabilities, InitializeParams, InitializeResponse, JSONRPCError,
    JSONRPCNotification, JSONRPCRequest, JSONRPCResponse, RequestId, ReviewStartParams,
    ReviewStartResponse, ReviewTarget, SandboxMode, ServerNotification, ServerRequest,
    ThreadResumeParams, ThreadResumeResponse, ThreadStartParams, ThreadStartResponse,
    ToolRequestUserInputAnswer, ToolRequestUserInputQuestion, ToolRequestUserInputResponse,
    TurnStartParams, TurnStartResponse, UserInput,
};
use codex_protocol::{
    config_types::CollaborationMode,
    protocol::ReviewDecision,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{self, Value, json};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt, BufWriter},
    sync::Mutex,
};
use workspace_utils::approvals::ApprovalStatus;

use super::jsonrpc::{JsonRpcCallbacks, JsonRpcPeer};
use crate::{
    approvals::{ExecutorApprovalError, ExecutorApprovalService},
    executors::{
        ExecutorError,
        codex::normalize_logs::{Approval, Error as LogError},
    },
};

const EXIT_PLAN_MODE_NAME: &str = "ExitPlanMode";
const REQUEST_USER_INPUT_TOOL_NAME: &str = "request_user_input";

#[derive(Debug, Clone, Default)]
struct PendingPlanProposal {
    item_id: String,
    text: String,
}

#[derive(Debug, Clone, Default)]
pub struct SessionConfigParams {
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub cwd: Option<String>,
    pub approval_policy: Option<AskForApproval>,
    pub sandbox: Option<SandboxMode>,
    pub config: Option<HashMap<String, Value>>,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
}

impl SessionConfigParams {
    fn into_thread_start(self) -> ThreadStartParams {
        ThreadStartParams {
            model: self.model,
            model_provider: self.model_provider,
            cwd: self.cwd,
            approval_policy: self.approval_policy,
            sandbox: self.sandbox,
            config: self.config,
            base_instructions: self.base_instructions,
            developer_instructions: self.developer_instructions,
            ..Default::default()
        }
    }

    fn into_thread_resume(self, thread_id: String, path: Option<PathBuf>) -> ThreadResumeParams {
        ThreadResumeParams {
            thread_id,
            path,
            model: self.model,
            model_provider: self.model_provider,
            cwd: self.cwd,
            approval_policy: self.approval_policy,
            sandbox: self.sandbox,
            config: self.config,
            base_instructions: self.base_instructions,
            developer_instructions: self.developer_instructions,
            ..Default::default()
        }
    }
}

pub struct AppServerClient {
    rpc: OnceLock<JsonRpcPeer>,
    log_writer: LogWriter,
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
    thread_id: Mutex<Option<String>>,
    pending_feedback: Mutex<VecDeque<String>>,
    pending_plan: Mutex<Option<PendingPlanProposal>>,
    auto_approve: bool,
}

impl AppServerClient {
    pub fn new(
        log_writer: LogWriter,
        approvals: Option<Arc<dyn ExecutorApprovalService>>,
        auto_approve: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            rpc: OnceLock::new(),
            log_writer,
            approvals,
            auto_approve,
            thread_id: Mutex::new(None),
            pending_feedback: Mutex::new(VecDeque::new()),
            pending_plan: Mutex::new(None),
        })
    }

    pub fn connect(&self, peer: JsonRpcPeer) {
        let _ = self.rpc.set(peer);
    }

    fn rpc(&self) -> &JsonRpcPeer {
        self.rpc.get().expect("Codex RPC peer not attached")
    }

    pub async fn initialize(&self) -> Result<(), ExecutorError> {
        let request = ClientRequest::Initialize {
            request_id: self.next_request_id(),
            params: InitializeParams {
                client_info: ClientInfo {
                    name: "vibe-codex-executor".to_string(),
                    title: None,
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                capabilities: Some(InitializeCapabilities {
                    experimental_api: true,
                    opt_out_notification_methods: None,
                }),
            },
        };

        self.send_request::<InitializeResponse>(request, "initialize")
            .await?;
        self.send_message(&ClientNotification::Initialized).await
    }

    pub async fn thread_start(
        &self,
        params: SessionConfigParams,
    ) -> Result<ThreadStartResponse, ExecutorError> {
        let request = ClientRequest::ThreadStart {
            request_id: self.next_request_id(),
            params: params.into_thread_start(),
        };
        self.send_request(request, "thread/start").await
    }

    pub async fn thread_resume(
        &self,
        thread_id: String,
        path: Option<PathBuf>,
        overrides: SessionConfigParams,
    ) -> Result<ThreadResumeResponse, ExecutorError> {
        let request = ClientRequest::ThreadResume {
            request_id: self.next_request_id(),
            params: overrides.into_thread_resume(thread_id, path),
        };
        self.send_request(request, "thread/resume").await
    }

    pub async fn start_turn(
        &self,
        thread_id: String,
        message: String,
        collaboration_mode: Option<CollaborationMode>,
    ) -> Result<TurnStartResponse, ExecutorError> {
        let request = ClientRequest::TurnStart {
            request_id: self.next_request_id(),
            params: TurnStartParams {
                thread_id,
                input: vec![UserInput::Text {
                    text: message,
                    text_elements: Vec::new(),
                }],
                collaboration_mode,
                ..Default::default()
            },
        };
        self.send_request(request, "turn/start").await
    }

    pub async fn get_account(&self) -> Result<GetAccountResponse, ExecutorError> {
        let request = ClientRequest::GetAccount {
            request_id: self.next_request_id(),
            params: GetAccountParams {
                refresh_token: false,
            },
        };
        self.send_request(request, "account/read").await
    }

    pub async fn start_review(
        &self,
        thread_id: String,
        target: ReviewTarget,
    ) -> Result<ReviewStartResponse, ExecutorError> {
        let request = ClientRequest::ReviewStart {
            request_id: self.next_request_id(),
            params: ReviewStartParams {
                thread_id,
                target,
                delivery: None,
            },
        };
        self.send_request(request, "review/start").await
    }

    async fn handle_server_request(
        &self,
        peer: &JsonRpcPeer,
        request: ServerRequest,
    ) -> Result<(), ExecutorError> {
        match request {
            ServerRequest::ApplyPatchApproval { request_id, .. } => {
                self.reject_legacy_server_request(
                    peer,
                    request_id,
                    ApplyPatchApprovalResponse {
                        decision: ReviewDecision::Denied,
                    },
                    "applyPatchApproval",
                )
                .await
            }
            ServerRequest::ExecCommandApproval { request_id, .. } => {
                self.reject_legacy_server_request(
                    peer,
                    request_id,
                    ExecCommandApprovalResponse {
                        decision: ReviewDecision::Denied,
                    },
                    "execCommandApproval",
                )
                .await
            }
            ServerRequest::CommandExecutionRequestApproval { request_id, params } => {
                let input = serde_json::to_value(&params)
                    .map_err(|err| ExecutorError::Io(io::Error::other(err.to_string())))?;
                let status = match self
                    .request_tool_approval("bash", input, &params.item_id)
                    .await
                {
                    Ok(status) => status,
                    Err(err) => {
                        tracing::error!("failed to request command approval: {err}");
                        ApprovalStatus::Denied {
                            reason: Some("approval service error".to_string()),
                        }
                    }
                };
                self.log_writer
                    .log_raw(
                        &Approval::approval_response(
                            params.item_id.clone(),
                            "codex.exec_command".to_string(),
                            status.clone(),
                        )
                        .raw(),
                    )
                    .await?;
                let feedback = match &status {
                    ApprovalStatus::Denied { reason } => reason
                        .as_ref()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string()),
                    _ => None,
                };
                let decision = match status {
                    ApprovalStatus::Approved | ApprovalStatus::ProvidedInput { .. } => {
                        CommandExecutionApprovalDecision::Accept
                    }
                    ApprovalStatus::Denied { .. }
                    | ApprovalStatus::TimedOut
                    | ApprovalStatus::Pending => CommandExecutionApprovalDecision::Decline,
                };
                send_server_response(
                    peer,
                    request_id,
                    CommandExecutionRequestApprovalResponse { decision },
                )
                .await?;
                if let Some(message) = feedback {
                    tracing::debug!("queueing exec denial feedback: {message}");
                    self.enqueue_feedback(message).await;
                }
                Ok(())
            }
            ServerRequest::FileChangeRequestApproval { request_id, params } => {
                let input = serde_json::to_value(&params)
                    .map_err(|err| ExecutorError::Io(io::Error::other(err.to_string())))?;
                let status = match self
                    .request_tool_approval("edit", input, &params.item_id)
                    .await
                {
                    Ok(status) => status,
                    Err(err) => {
                        tracing::error!("failed to request patch approval: {err}");
                        ApprovalStatus::Denied {
                            reason: Some("approval service error".to_string()),
                        }
                    }
                };
                self.log_writer
                    .log_raw(
                        &Approval::approval_response(
                            params.item_id.clone(),
                            "codex.apply_patch".to_string(),
                            status.clone(),
                        )
                        .raw(),
                    )
                    .await?;
                let feedback = match &status {
                    ApprovalStatus::Denied { reason } => reason
                        .as_ref()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string()),
                    _ => None,
                };
                let decision = match status {
                    ApprovalStatus::Approved | ApprovalStatus::ProvidedInput { .. } => {
                        FileChangeApprovalDecision::Accept
                    }
                    ApprovalStatus::Denied { .. }
                    | ApprovalStatus::TimedOut
                    | ApprovalStatus::Pending => FileChangeApprovalDecision::Decline,
                };
                send_server_response(
                    peer,
                    request_id,
                    FileChangeRequestApprovalResponse { decision },
                )
                .await?;
                if let Some(message) = feedback {
                    tracing::debug!("queueing patch denial feedback: {message}");
                    self.enqueue_feedback(message).await;
                }
                Ok(())
            }
            ServerRequest::ToolRequestUserInput { request_id, params } => {
                let item_id = params.item_id.clone();
                let questions = params.questions.clone();
                let input = json!({
                    "questions": questions,
                });

                let status = match self
                    .request_tool_approval(REQUEST_USER_INPUT_TOOL_NAME, input, &item_id)
                    .await
                {
                    Ok(status) => status,
                    Err(err) => {
                        tracing::error!("failed to request user input approval: {err}");
                        ApprovalStatus::Denied {
                            reason: Some("approval service error".to_string()),
                        }
                    }
                };

                self.log_writer
                    .log_raw(
                        &Approval::approval_response(
                            item_id,
                            REQUEST_USER_INPUT_TOOL_NAME.to_string(),
                            status.clone(),
                        )
                        .raw(),
                    )
                    .await?;

                if let ApprovalStatus::Denied { reason } = &status
                    && let Some(message) =
                        reason.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty())
                {
                    tracing::debug!("queueing request_user_input denial feedback: {message}");
                    self.enqueue_feedback(message.to_string()).await;
                }

                let response = tool_request_user_input_response(&params.questions, &status);
                send_server_response(peer, request_id, response).await
            }
            ServerRequest::DynamicToolCall {
                request_id,
                params: _,
            } => {
                send_server_response(
                    peer,
                    request_id,
                    DynamicToolCallResponse {
                        content_items: Vec::new(),
                        success: false,
                    },
                )
                .await
            }
            ServerRequest::ChatgptAuthTokensRefresh {
                request_id: _,
                params: _,
            } => {
                tracing::error!("received unsupported chatgpt auth refresh request");
                Err(
                    ExecutorApprovalError::RequestFailed("unsupported server request".to_string())
                        .into(),
                )
            }
        }
    }

    async fn reject_legacy_server_request<T>(
        &self,
        peer: &JsonRpcPeer,
        request_id: RequestId,
        response: T,
        method_name: &str,
    ) -> Result<(), ExecutorError>
    where
        T: Serialize,
    {
        let message = format!(
            "unsupported legacy Codex server request `{method_name}` in v2-only executor mode"
        );
        tracing::error!("{message}");
        self.log_writer
            .log_raw(&LogError::launch_error(message.clone()).raw())
            .await?;
        send_server_response(peer, request_id, response).await?;
        Err(ExecutorApprovalError::RequestFailed(message).into())
    }

    async fn request_tool_approval(
        &self,
        tool_name: &str,
        tool_input: Value,
        tool_call_id: &str,
    ) -> Result<ApprovalStatus, ExecutorError> {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if self.auto_approve {
            return Ok(ApprovalStatus::Approved);
        }
        Ok(self
            .approvals
            .as_ref()
            .ok_or(ExecutorApprovalError::ServiceUnavailable)?
            .request_tool_approval(tool_name, tool_input, tool_call_id)
            .await?)
    }

    pub async fn register_session(&self, thread_id: &str) -> Result<(), ExecutorError> {
        {
            let mut guard = self.thread_id.lock().await;
            guard.replace(thread_id.to_string());
        }
        self.flush_pending_feedback().await;
        Ok(())
    }

    async fn send_message<M>(&self, message: &M) -> Result<(), ExecutorError>
    where
        M: Serialize + Sync,
    {
        self.rpc().send(message).await
    }

    async fn send_request<R>(&self, request: ClientRequest, label: &str) -> Result<R, ExecutorError>
    where
        R: DeserializeOwned + std::fmt::Debug,
    {
        let request_id = request_id(&request);
        self.rpc().request(request_id, &request, label).await
    }

    fn next_request_id(&self) -> RequestId {
        self.rpc().next_request_id()
    }

    async fn enqueue_feedback(&self, message: String) {
        if message.trim().is_empty() {
            return;
        }
        let mut guard = self.pending_feedback.lock().await;
        guard.push_back(message);
    }

    async fn flush_pending_feedback(&self) {
        let messages: Vec<String> = {
            let mut guard = self.pending_feedback.lock().await;
            guard.drain(..).collect()
        };

        if messages.is_empty() {
            return;
        }

        let Some(thread_id) = self.thread_id.lock().await.clone() else {
            tracing::warn!(
                "pending Codex feedback but thread id unavailable; dropping {} messages",
                messages.len()
            );
            return;
        };

        for message in messages {
            let trimmed = message.trim();
            if trimmed.is_empty() {
                continue;
            }
            self.spawn_feedback_message(thread_id.clone(), trimmed.to_string());
        }
    }

    async fn clear_pending_plan(&self) {
        let mut guard = self.pending_plan.lock().await;
        *guard = None;
    }

    async fn append_plan_delta(&self, item_id: String, delta: String) {
        let mut guard = self.pending_plan.lock().await;
        match guard.as_mut() {
            Some(plan) if plan.item_id == item_id => plan.text.push_str(&delta),
            _ => {
                *guard = Some(PendingPlanProposal {
                    item_id,
                    text: delta,
                })
            }
        }
    }

    async fn set_plan_text(&self, item_id: String, text: String) {
        if text.trim().is_empty() {
            return;
        }
        let mut guard = self.pending_plan.lock().await;
        match guard.as_mut() {
            Some(plan) if plan.item_id == item_id => plan.text = text,
            _ => *guard = Some(PendingPlanProposal { item_id, text }),
        }
    }

    async fn observe_server_notification(&self, notification: &ServerNotification) {
        match notification {
            ServerNotification::TurnStarted(_) => {
                self.clear_pending_plan().await;
            }
            ServerNotification::PlanDelta(event) => {
                self.append_plan_delta(event.item_id.clone(), event.delta.clone())
                    .await;
            }
            ServerNotification::ItemCompleted(event) => {
                if let codex_app_server_protocol::ThreadItem::Plan { id, text } = &event.item {
                    self.set_plan_text(id.clone(), text.clone()).await;
                }
            }
            _ => {}
        }
    }

    async fn handle_notification(
        &self,
        raw: &str,
        _notification: JSONRPCNotification,
    ) -> Result<bool, ExecutorError> {
        let parsed_server_notification = serde_json::from_str::<ServerNotification>(raw).ok();
        let raw = if let Some(mut server_notification) = parsed_server_notification.clone() {
            if let ServerNotification::SessionConfigured(session_configured) =
                &mut server_notification
            {
                // history can be large, which might get truncated during transmission, corrupting the JSON line and losing valuable session and model information.
                session_configured.initial_messages = None;
                Cow::Owned(serde_json::to_string(&server_notification)?)
            } else {
                Cow::Borrowed(raw)
            }
        } else {
            Cow::Borrowed(raw)
        };
        self.log_writer.log_raw(&raw).await?;

        if let Some(server_notification) = parsed_server_notification.as_ref() {
            self.observe_server_notification(server_notification).await;
        }

        let has_finished = matches!(
            parsed_server_notification,
            Some(ServerNotification::TurnCompleted(_))
        );
        if !has_finished {
            return Ok(false);
        }

        if has_finished {
            let plan_approved = self.request_pending_plan_approval().await?;
            if !plan_approved {
                // Plan was denied — send the queued feedback to Codex so it
                // can generate a revised plan in a new turn.
                self.flush_pending_feedback().await;
                return Ok(false);
            }
        }

        Ok(has_finished)
    }

    async fn request_pending_plan_approval(&self) -> Result<bool, ExecutorError> {
        // Plan approvals always require user review — bypass auto_approve.
        let approvals = match &self.approvals {
            Some(a) => a.clone(),
            None => {
                self.clear_pending_plan().await;
                return Ok(true);
            }
        };

        let proposal = {
            let mut guard = self.pending_plan.lock().await;
            guard.take()
        };
        let Some(proposal) = proposal else {
            return Ok(true);
        };

        let plan_text = proposal.text.trim().to_string();
        if plan_text.is_empty() {
            return Ok(true);
        }

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let status = match approvals
            .request_tool_approval(
                EXIT_PLAN_MODE_NAME,
                json!({ "plan": plan_text }),
                &proposal.item_id,
            )
            .await
        {
            Ok(status) => status,
            Err(err) => {
                tracing::error!("failed to request plan approval: {err}");
                ApprovalStatus::Denied {
                    reason: Some("approval service error".to_string()),
                }
            }
        };

        self.log_writer
            .log_raw(
                &Approval::approval_response(
                    proposal.item_id,
                    EXIT_PLAN_MODE_NAME.to_string(),
                    status.clone(),
                )
                .raw(),
            )
            .await?;

        match status {
            ApprovalStatus::Approved | ApprovalStatus::ProvidedInput { .. } => Ok(true),
            ApprovalStatus::Denied { reason } => {
                let feedback = reason
                    .as_ref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "User denied the plan".to_string());
                tracing::debug!("plan denied; queueing feedback: {feedback}");
                self.enqueue_feedback(feedback).await;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn spawn_feedback_message(&self, thread_id: String, feedback: String) {
        let peer = self.rpc().clone();
        let request = ClientRequest::TurnStart {
            request_id: peer.next_request_id(),
            params: TurnStartParams {
                thread_id,
                input: vec![UserInput::Text {
                    text: format!("User feedback: {feedback}"),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        };
        tokio::spawn(async move {
            if let Err(err) = peer
                .request::<TurnStartResponse, _>(request_id(&request), &request, "turn/start")
                .await
            {
                tracing::error!("failed to send feedback follow-up message: {err}");
            }
        });
    }
}

#[async_trait]
impl JsonRpcCallbacks for AppServerClient {
    async fn on_request(
        &self,
        peer: &JsonRpcPeer,
        raw: &str,
        request: JSONRPCRequest,
    ) -> Result<(), ExecutorError> {
        self.log_writer.log_raw(raw).await?;
        match ServerRequest::try_from(request.clone()) {
            Ok(server_request) => self.handle_server_request(peer, server_request).await,
            Err(err) => {
                tracing::debug!("Unhandled server request `{}`: {err}", request.method);
                let response = JSONRPCResponse {
                    id: request.id,
                    result: Value::Null,
                };
                peer.send(&response).await
            }
        }
    }

    async fn on_response(
        &self,
        _peer: &JsonRpcPeer,
        raw: &str,
        _response: &JSONRPCResponse,
    ) -> Result<(), ExecutorError> {
        self.log_writer.log_raw(raw).await
    }

    async fn on_error(
        &self,
        _peer: &JsonRpcPeer,
        raw: &str,
        _error: &JSONRPCError,
    ) -> Result<(), ExecutorError> {
        self.log_writer.log_raw(raw).await
    }

    async fn on_notification(
        &self,
        _peer: &JsonRpcPeer,
        raw: &str,
        notification: JSONRPCNotification,
    ) -> Result<bool, ExecutorError> {
        self.handle_notification(raw, notification).await
    }

    async fn on_non_json(&self, raw: &str) -> Result<(), ExecutorError> {
        self.log_writer.log_raw(raw).await?;
        Ok(())
    }
}

fn tool_request_user_input_response(
    questions: &[ToolRequestUserInputQuestion],
    status: &ApprovalStatus,
) -> ToolRequestUserInputResponse {
    let mut answers = questions
        .iter()
        .map(|question| {
            (
                question.id.clone(),
                ToolRequestUserInputAnswer {
                    answers: Vec::new(),
                },
            )
        })
        .collect::<HashMap<_, _>>();

    if let ApprovalStatus::ProvidedInput { input } = status {
        for (question_id, values) in parse_request_user_input_answers(input) {
            if let Some(answer) = answers.get_mut(&question_id) {
                answer.answers = values;
            }
        }
    }

    ToolRequestUserInputResponse { answers }
}

fn parse_request_user_input_answers(input: &Value) -> HashMap<String, Vec<String>> {
    let answers_source = input
        .get("answers")
        .or_else(|| input.get("updated_input").and_then(|v| v.get("answers")))
        .unwrap_or(input);

    let Some(raw_answers) = answers_source.as_object() else {
        return HashMap::new();
    };

    raw_answers
        .iter()
        .map(|(question_id, value)| (question_id.clone(), parse_answer_values(value)))
        .collect()
}

fn parse_answer_values(value: &Value) -> Vec<String> {
    match value {
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|answer| !answer.is_empty())
            .map(str::to_string)
            .collect(),
        Value::Object(record) => record
            .get("answers")
            .map(parse_answer_values)
            .unwrap_or_default(),
        Value::String(single) => {
            let trimmed = single.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![trimmed.to_string()]
            }
        }
        _ => Vec::new(),
    }
}

async fn send_server_response<T>(
    peer: &JsonRpcPeer,
    request_id: RequestId,
    response: T,
) -> Result<(), ExecutorError>
where
    T: Serialize,
{
    let payload = JSONRPCResponse {
        id: request_id,
        result: serde_json::to_value(response)
            .map_err(|err| ExecutorError::Io(io::Error::other(err.to_string())))?,
    };

    peer.send(&payload).await
}

fn request_id(request: &ClientRequest) -> RequestId {
    match request {
        ClientRequest::Initialize { request_id, .. }
        | ClientRequest::ThreadStart { request_id, .. }
        | ClientRequest::ThreadResume { request_id, .. }
        | ClientRequest::GetAccount { request_id, .. }
        | ClientRequest::TurnStart { request_id, .. }
        | ClientRequest::ReviewStart { request_id, .. } => request_id.clone(),
        _ => unreachable!("request_id called for unsupported request variant"),
    }
}

#[derive(Clone)]
pub struct LogWriter {
    writer: Arc<Mutex<BufWriter<Box<dyn AsyncWrite + Send + Unpin>>>>,
}

impl LogWriter {
    pub fn new(writer: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Self {
            writer: Arc::new(Mutex::new(BufWriter::new(Box::new(writer)))),
        }
    }

    pub async fn log_raw(&self, raw: &str) -> Result<(), ExecutorError> {
        let mut guard = self.writer.lock().await;
        guard
            .write_all(raw.as_bytes())
            .await
            .map_err(ExecutorError::Io)?;
        guard.write_all(b"\n").await.map_err(ExecutorError::Io)?;
        guard.flush().await.map_err(ExecutorError::Io)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use workspace_utils::approvals::ApprovalStatus;

    use super::*;
    use crate::approvals::{ExecutorApprovalError, ExecutorApprovalService};

    #[derive(Debug, Clone)]
    struct ApprovalCall {
        tool_name: String,
        tool_input: Value,
        tool_call_id: String,
    }

    #[derive(Debug)]
    struct MockApprovalService {
        status: ApprovalStatus,
        calls: Mutex<Vec<ApprovalCall>>,
    }

    impl MockApprovalService {
        fn new(status: ApprovalStatus) -> Self {
            Self {
                status,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<ApprovalCall> {
            self.calls.lock().expect("lock approval calls").clone()
        }
    }

    #[async_trait]
    impl ExecutorApprovalService for MockApprovalService {
        async fn request_tool_approval(
            &self,
            tool_name: &str,
            tool_input: Value,
            tool_call_id: &str,
        ) -> Result<ApprovalStatus, ExecutorApprovalError> {
            self.calls
                .lock()
                .expect("lock approval calls")
                .push(ApprovalCall {
                    tool_name: tool_name.to_string(),
                    tool_input,
                    tool_call_id: tool_call_id.to_string(),
                });
            Ok(self.status.clone())
        }
    }

    fn new_client(approvals: Option<Arc<dyn ExecutorApprovalService>>) -> Arc<AppServerClient> {
        AppServerClient::new(LogWriter::new(tokio::io::sink()), approvals, false)
    }

    fn make_notification(method: &str, params: Option<Value>) -> (String, JSONRPCNotification) {
        let notification = JSONRPCNotification {
            method: method.to_string(),
            params,
        };
        let raw = serde_json::to_string(&notification).expect("serialize notification");
        (raw, notification)
    }

    #[tokio::test]
    async fn turn_completed_notification_marks_finished() {
        let client = new_client(None);
        let (raw, notification) = make_notification(
            "turn/completed",
            Some(json!({
                "threadId": "thread-1",
                "turn": {
                    "id": "turn-1",
                    "items": [],
                    "status": "completed",
                    "error": null
                }
            })),
        );

        let finished = client
            .handle_notification(&raw, notification)
            .await
            .expect("handle turn/completed notification");
        assert!(finished);
    }

    #[tokio::test]
    async fn v2_plan_notifications_still_trigger_plan_approval_on_turn_complete() {
        let approval_service = Arc::new(MockApprovalService::new(ApprovalStatus::Denied {
            reason: Some("needs edits".to_string()),
        }));
        let client = new_client(Some(
            approval_service.clone() as Arc<dyn ExecutorApprovalService>
        ));

        let (raw, notification) = make_notification(
            "item/plan/delta",
            Some(json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "plan-1",
                "delta": "Step A"
            })),
        );
        let finished = client
            .handle_notification(&raw, notification)
            .await
            .expect("handle plan delta");
        assert!(!finished);

        let (raw, notification) = make_notification(
            "item/completed",
            Some(json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "item": {
                    "type": "plan",
                    "id": "plan-1",
                    "text": "Step A\nStep B"
                }
            })),
        );
        let finished = client
            .handle_notification(&raw, notification)
            .await
            .expect("handle completed plan item");
        assert!(!finished);

        let (raw, notification) = make_notification(
            "turn/completed",
            Some(json!({
                "threadId": "thread-1",
                "turn": {
                    "id": "turn-1",
                    "items": [],
                    "status": "completed",
                    "error": null
                }
            })),
        );
        let finished = client
            .handle_notification(&raw, notification)
            .await
            .expect("handle turn completed");
        assert!(!finished, "denied plan approval should block completion");

        let calls = approval_service.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_name, EXIT_PLAN_MODE_NAME);
        assert_eq!(calls[0].tool_call_id, "plan-1");
        assert_eq!(calls[0].tool_input, json!({ "plan": "Step A\nStep B" }));
    }
}
