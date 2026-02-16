use std::{
    borrow::Cow,
    collections::VecDeque,
    io,
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;
use codex_app_server_protocol::{
    AddConversationListenerParams, AddConversationSubscriptionResponse, ApplyPatchApprovalResponse,
    ClientInfo, ClientNotification, ClientRequest, CommandExecutionApprovalDecision,
    CommandExecutionRequestApprovalResponse, DynamicToolCallResponse, ExecCommandApprovalResponse,
    FileChangeApprovalDecision, FileChangeRequestApprovalResponse, GetAuthStatusParams,
    GetAuthStatusResponse, InitializeCapabilities, InitializeParams, InitializeResponse, InputItem,
    JSONRPCError, JSONRPCNotification, JSONRPCRequest, JSONRPCResponse, NewConversationParams,
    NewConversationResponse, RequestId, ResumeConversationParams, ResumeConversationResponse,
    ReviewStartParams, ReviewStartResponse, ReviewTarget, SendUserMessageParams,
    SendUserMessageResponse, ServerNotification, ServerRequest, ToolRequestUserInputAnswer,
    ToolRequestUserInputResponse, TurnStartParams, TurnStartResponse, UserInput,
};
use codex_protocol::{
    ThreadId,
    config_types::CollaborationMode,
    items::TurnItem,
    protocol::{EventMsg, ReviewDecision},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{self, Value, json};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt, BufWriter},
    sync::Mutex,
};
use workspace_utils::approvals::ApprovalStatus;

use super::jsonrpc::{JsonRpcCallbacks, JsonRpcPeer};
use crate::{
    approvals::{ExecutorApprovalError, ExecutorApprovalService},
    executors::{ExecutorError, codex::normalize_logs::Approval},
};

const EXIT_PLAN_MODE_NAME: &str = "ExitPlanMode";

#[derive(Debug, Clone, Default)]
struct PendingPlanProposal {
    item_id: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct CodexEventNotificationParams {
    msg: EventMsg,
}

pub struct AppServerClient {
    rpc: OnceLock<JsonRpcPeer>,
    log_writer: LogWriter,
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
    conversation_id: Mutex<Option<ThreadId>>,
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
            conversation_id: Mutex::new(None),
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

    pub async fn new_conversation(
        &self,
        params: NewConversationParams,
    ) -> Result<NewConversationResponse, ExecutorError> {
        let request = ClientRequest::NewConversation {
            request_id: self.next_request_id(),
            params,
        };
        self.send_request(request, "newConversation").await
    }

    pub async fn resume_conversation(
        &self,
        rollout_path: std::path::PathBuf,
        overrides: NewConversationParams,
    ) -> Result<ResumeConversationResponse, ExecutorError> {
        let request = ClientRequest::ResumeConversation {
            request_id: self.next_request_id(),
            params: ResumeConversationParams {
                path: Some(rollout_path),
                overrides: Some(overrides),
                conversation_id: None,
                history: None,
            },
        };
        self.send_request(request, "resumeConversation").await
    }

    pub async fn add_conversation_listener(
        &self,
        conversation_id: codex_protocol::ThreadId,
    ) -> Result<AddConversationSubscriptionResponse, ExecutorError> {
        let request = ClientRequest::AddConversationListener {
            request_id: self.next_request_id(),
            params: AddConversationListenerParams {
                conversation_id,
                experimental_raw_events: false,
            },
        };
        self.send_request(request, "addConversationListener").await
    }

    pub async fn send_user_message(
        &self,
        conversation_id: codex_protocol::ThreadId,
        message: String,
    ) -> Result<SendUserMessageResponse, ExecutorError> {
        let request = ClientRequest::SendUserMessage {
            request_id: self.next_request_id(),
            params: SendUserMessageParams {
                conversation_id,
                items: vec![InputItem::Text {
                    text: message,
                    text_elements: Vec::new(),
                }],
            },
        };
        self.send_request(request, "sendUserMessage").await
    }

    pub async fn start_turn(
        &self,
        conversation_id: codex_protocol::ThreadId,
        message: String,
        collaboration_mode: Option<CollaborationMode>,
    ) -> Result<TurnStartResponse, ExecutorError> {
        let request = ClientRequest::TurnStart {
            request_id: self.next_request_id(),
            params: TurnStartParams {
                thread_id: conversation_id.to_string(),
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

    pub async fn get_auth_status(&self) -> Result<GetAuthStatusResponse, ExecutorError> {
        let request = ClientRequest::GetAuthStatus {
            request_id: self.next_request_id(),
            params: GetAuthStatusParams {
                include_token: Some(true),
                refresh_token: Some(false),
            },
        };
        self.send_request(request, "getAuthStatus").await
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
        self.send_request(request, "reviewStart").await
    }

    async fn handle_server_request(
        &self,
        peer: &JsonRpcPeer,
        request: ServerRequest,
    ) -> Result<(), ExecutorError> {
        match request {
            ServerRequest::ApplyPatchApproval { request_id, params } => {
                let input = serde_json::to_value(&params)
                    .map_err(|err| ExecutorError::Io(io::Error::other(err.to_string())))?;
                let status = match self
                    .request_tool_approval("edit", input, &params.call_id)
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
                            params.call_id,
                            "codex.apply_patch".to_string(),
                            status.clone(),
                        )
                        .raw(),
                    )
                    .await?;
                let (decision, feedback) = self.review_decision(&status).await?;
                let response = ApplyPatchApprovalResponse { decision };
                send_server_response(peer, request_id, response).await?;
                if let Some(message) = feedback {
                    tracing::debug!("queueing patch denial feedback: {message}");
                    self.enqueue_feedback(message).await;
                }
                Ok(())
            }
            ServerRequest::ExecCommandApproval { request_id, params } => {
                let input = serde_json::to_value(&params)
                    .map_err(|err| ExecutorError::Io(io::Error::other(err.to_string())))?;
                let status = match self
                    .request_tool_approval("bash", input, &params.call_id)
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
                            params.call_id,
                            "codex.exec_command".to_string(),
                            status.clone(),
                        )
                        .raw(),
                    )
                    .await?;

                let (decision, feedback) = self.review_decision(&status).await?;
                let response = ExecCommandApprovalResponse { decision };
                send_server_response(peer, request_id, response).await?;
                if let Some(message) = feedback {
                    tracing::debug!("queueing exec denial feedback: {message}");
                    self.enqueue_feedback(message).await;
                }
                Ok(())
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
                    ApprovalStatus::Approved => CommandExecutionApprovalDecision::Accept,
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
                    ApprovalStatus::Approved => FileChangeApprovalDecision::Accept,
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
                // Vibe Kanban does not currently support prompting users for this interactive tool.
                let answers = params
                    .questions
                    .into_iter()
                    .map(|question| {
                        (
                            question.id,
                            ToolRequestUserInputAnswer {
                                answers: Vec::new(),
                            },
                        )
                    })
                    .collect();
                send_server_response(peer, request_id, ToolRequestUserInputResponse { answers })
                    .await
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

    pub async fn register_session(&self, conversation_id: &ThreadId) -> Result<(), ExecutorError> {
        {
            let mut guard = self.conversation_id.lock().await;
            guard.replace(conversation_id.clone());
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

    async fn review_decision(
        &self,
        status: &ApprovalStatus,
    ) -> Result<(ReviewDecision, Option<String>), ExecutorError> {
        if self.auto_approve {
            return Ok((ReviewDecision::ApprovedForSession, None));
        }

        let outcome = match status {
            ApprovalStatus::Approved => (ReviewDecision::Approved, None),
            ApprovalStatus::Denied { reason } => {
                let feedback = reason
                    .as_ref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                if feedback.is_some() {
                    (ReviewDecision::Abort, feedback)
                } else {
                    (ReviewDecision::Denied, None)
                }
            }
            ApprovalStatus::TimedOut => (ReviewDecision::Denied, None),
            ApprovalStatus::Pending => (ReviewDecision::Denied, None),
        };
        Ok(outcome)
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

        let Some(conversation_id) = self.conversation_id.lock().await.clone() else {
            tracing::warn!(
                "pending Codex feedback but conversation id unavailable; dropping {} messages",
                messages.len()
            );
            return;
        };

        for message in messages {
            let trimmed = message.trim();
            if trimmed.is_empty() {
                continue;
            }
            self.spawn_feedback_message(conversation_id, trimmed.to_string());
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

    async fn observe_codex_event(&self, event: EventMsg) {
        match event {
            EventMsg::TurnStarted(_) | EventMsg::TurnAborted(_) => {
                self.clear_pending_plan().await;
            }
            EventMsg::PlanDelta(event) => {
                self.append_plan_delta(event.item_id, event.delta).await;
            }
            EventMsg::ItemCompleted(event) => {
                if let TurnItem::Plan(plan_item) = event.item {
                    self.set_plan_text(plan_item.id, plan_item.text).await;
                }
            }
            _ => {}
        }
    }

    async fn request_pending_plan_approval(&self) -> Result<(), ExecutorError> {
        // Plan approvals always require user review — bypass auto_approve.
        let approvals = match &self.approvals {
            Some(a) => a.clone(),
            None => {
                self.clear_pending_plan().await;
                return Ok(());
            }
        };

        let proposal = {
            let mut guard = self.pending_plan.lock().await;
            guard.take()
        };
        let Some(proposal) = proposal else {
            return Ok(());
        };

        let plan_text = proposal.text.trim().to_string();
        if plan_text.is_empty() {
            return Ok(());
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
                    status,
                )
                .raw(),
            )
            .await?;
        Ok(())
    }

    fn spawn_feedback_message(&self, conversation_id: ThreadId, feedback: String) {
        let peer = self.rpc().clone();
        let request = ClientRequest::SendUserMessage {
            request_id: peer.next_request_id(),
            params: SendUserMessageParams {
                conversation_id,
                items: vec![InputItem::Text {
                    text: format!("User feedback: {feedback}"),
                    text_elements: Vec::new(),
                }],
            },
        };
        tokio::spawn(async move {
            if let Err(err) = peer
                .request::<SendUserMessageResponse, _>(
                    request_id(&request),
                    &request,
                    "sendUserMessage",
                )
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
        let raw =
            if let Ok(mut server_notification) = serde_json::from_str::<ServerNotification>(raw) {
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

        let method = notification.method.as_str();
        if method.starts_with("codex/event")
            && let Some(params) = notification.params.clone()
            && let Ok(parsed) = serde_json::from_value::<CodexEventNotificationParams>(params)
        {
            self.observe_codex_event(parsed.msg).await;
        }

        if !method.starts_with("codex/event") {
            return Ok(false);
        }

        if method.ends_with("turn_aborted") {
            tracing::debug!("codex turn aborted; flushing feedback queue");
            self.clear_pending_plan().await;
            self.flush_pending_feedback().await;
            return Ok(false);
        }

        let has_finished = method
            .strip_prefix("codex/event/")
            .is_some_and(|suffix| matches!(suffix, "task_complete" | "turn_complete"));

        if has_finished {
            self.request_pending_plan_approval().await?;
        }

        Ok(has_finished)
    }

    async fn on_non_json(&self, raw: &str) -> Result<(), ExecutorError> {
        self.log_writer.log_raw(raw).await?;
        Ok(())
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
        | ClientRequest::NewConversation { request_id, .. }
        | ClientRequest::GetAuthStatus { request_id, .. }
        | ClientRequest::ResumeConversation { request_id, .. }
        | ClientRequest::AddConversationListener { request_id, .. }
        | ClientRequest::SendUserMessage { request_id, .. }
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
