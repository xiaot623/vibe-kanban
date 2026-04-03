use std::{collections::HashMap, path::Path, sync::Arc};

use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use workspace_utils::{approvals::ApprovalStatus, msg_store::MsgStore, path::make_path_relative};

use super::{
    EXIT_PLAN_MODE_NAME,
    plan_mode::{REQUEST_USER_INPUT_TOOL_NAME, detect_plan_exit_question},
    types::{
        MessageInfo, MessagePartDeltaEvent, MessageRole, OpencodeExecutorEvent, Part,
        PermissionAskedEvent, QuestionAskedEvent, SdkEvent, SdkTodo, SessionStatus, ToolPart,
        ToolStateUpdate,
    },
};
use crate::{
    approvals::ToolCallMetadata,
    logs::{
        ActionType, CommandExitStatus, CommandRunResult, FileChange, NormalizedEntry,
        NormalizedEntryError, NormalizedEntryType, TodoItem, ToolResult, ToolStatus,
        stderr_processor::normalize_stderr_logs,
        utils::{
            EntryIndexProvider,
            patch::{add_normalized_entry, replace_normalized_entry, upsert_normalized_entry},
        },
    },
};

fn system_message(content: String) -> NormalizedEntry {
    NormalizedEntry {
        timestamp: None,
        entry_type: NormalizedEntryType::SystemMessage,
        content,
        metadata: None,
    }
}

pub fn normalize_logs(msg_store: Arc<MsgStore>, worktree_path: &Path) {
    let entry_index = EntryIndexProvider::start_from(&msg_store);
    normalize_stderr_logs(msg_store.clone(), entry_index.clone());

    let worktree_path = worktree_path.to_path_buf();
    tokio::spawn(async move {
        let mut stored_session_id = false;
        let mut state = LogState::new(entry_index.clone(), msg_store.clone());

        let mut stdout_lines = msg_store.stdout_lines_stream();
        while let Some(Ok(line)) = stdout_lines.next().await {
            let Some(event) = parse_event(&line) else {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                add_normalized_entry(
                    &msg_store,
                    &entry_index,
                    system_message(trimmed.to_string()),
                );
                continue;
            };

            match event {
                OpencodeExecutorEvent::SessionStart { session_id } => {
                    if !stored_session_id {
                        msg_store.push_session_id(session_id);
                        stored_session_id = true;
                    }
                }
                OpencodeExecutorEvent::SdkEvent { event } => {
                    state
                        .handle_sdk_event(&event, &worktree_path, &msg_store)
                        .await;
                }
                OpencodeExecutorEvent::ApprovalResponse {
                    tool_call_id,
                    status,
                } => {
                    state.handle_approval_response(
                        &tool_call_id,
                        status,
                        &worktree_path,
                        &msg_store,
                    );
                }
                OpencodeExecutorEvent::Error { message } => {
                    let idx = entry_index.next();
                    msg_store.push_patch(
                        crate::logs::utils::ConversationPatch::add_normalized_entry(
                            idx,
                            NormalizedEntry {
                                timestamp: None,
                                entry_type: NormalizedEntryType::ErrorMessage {
                                    error_type: NormalizedEntryError::Other,
                                },
                                content: message,
                                metadata: None,
                            },
                        ),
                    );
                }
                OpencodeExecutorEvent::Done => {}
            }
        }
    });
}

fn parse_event(line: &str) -> Option<OpencodeExecutorEvent> {
    serde_json::from_str::<OpencodeExecutorEvent>(line.trim()).ok()
}

#[derive(Debug, Clone)]
struct StreamingText {
    index: usize,
    content: String,
    emitted: bool,
}

#[derive(Debug, Clone)]
enum UpdateMode {
    Append,
    Set,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartKind {
    Text,
    Reasoning,
    Other,
}

#[derive(Debug, Clone)]
struct PartIdentity {
    message_id: String,
    kind: PartKind,
}

#[derive(Default)]
struct LogState {
    entry_index: EntryIndexProvider,
    msg_store: Arc<MsgStore>,
    message_roles: HashMap<String, MessageRole>,
    part_identities: HashMap<String, PartIdentity>,
    assistant_text: HashMap<String, StreamingText>,
    thinking_text: HashMap<String, StreamingText>,
    tool_states: HashMap<String, ToolCallState>,
    approvals: HashMap<String, ApprovalStatus>,
    model_system_message_emitted: bool,
    todo_update_entry: Option<usize>,
    todo_update_fingerprint: Option<String>,
    retry_status_fingerprint: Option<String>,
}

impl LogState {
    fn new(entry_index: EntryIndexProvider, msg_store: Arc<MsgStore>) -> Self {
        Self {
            entry_index,
            msg_store,
            message_roles: HashMap::new(),
            part_identities: HashMap::new(),
            assistant_text: HashMap::new(),
            thinking_text: HashMap::new(),
            tool_states: HashMap::new(),
            approvals: HashMap::new(),
            model_system_message_emitted: false,
            todo_update_entry: None,
            todo_update_fingerprint: None,
            retry_status_fingerprint: None,
        }
    }

    async fn handle_sdk_event(
        &mut self,
        raw: &Value,
        worktree_path: &Path,
        msg_store: &Arc<MsgStore>,
    ) {
        let Some(event) = SdkEvent::parse(raw) else {
            let raw_text = raw.to_string();
            if !raw_text.trim().is_empty() {
                self.add_normalized_entry(system_message(format!(
                    "Unrecognized OpenCode SDK event: {raw_text}"
                )));
            }
            return;
        };

        match event {
            SdkEvent::MessageUpdated(event) => {
                let info = event.info;
                self.maybe_emit_model_system_message(&info);
                self.maybe_emit_token_usage(&info);
                self.message_roles.insert(info.id, info.role);
            }
            SdkEvent::MessagePartUpdated(event) => {
                self.handle_part_update(
                    event.part,
                    event.delta.as_deref(),
                    worktree_path,
                    msg_store,
                );
            }
            SdkEvent::MessagePartDelta(event) => {
                self.handle_part_delta(event, msg_store);
            }
            SdkEvent::TodoUpdated(event) => {
                self.handle_todo_updated(&event.todos, msg_store);
            }
            SdkEvent::SessionIdle | SdkEvent::SessionUpdated(_) => {}
            SdkEvent::SessionStatus(event) => {
                self.handle_session_status(event.status);
            }
            SdkEvent::SessionCompacted => {
                self.add_normalized_entry(system_message("Session compacted".to_string()));
            }
            SdkEvent::PermissionAsked(event) => {
                self.handle_permission_asked(event, worktree_path, msg_store);
            }
            SdkEvent::QuestionAsked(event) => {
                self.handle_question_asked(event, worktree_path, msg_store)
                    .await;
            }
            SdkEvent::QuestionReplied(event) => {
                tracing::debug!(
                    request_id = %event.request_id,
                    session_id = %event.session_id,
                    "OpenCode question replied"
                );
            }
            SdkEvent::PermissionReplied
            | SdkEvent::QuestionRejected
            | SdkEvent::MessageRemoved
            | SdkEvent::MessagePartRemoved
            | SdkEvent::CommandExecuted
            | SdkEvent::SessionDiff
            | SdkEvent::TuiSessionSelect => {}
            SdkEvent::SessionError(event) => {
                let (error_type, message) = match event.error {
                    Some(err) if err.kind() == "ProviderAuthError" => (
                        NormalizedEntryError::SetupRequired,
                        err.message()
                            .unwrap_or_else(|| format!("OpenCode session error: {}", err.raw)),
                    ),
                    Some(err) => (
                        NormalizedEntryError::Other,
                        format!("OpenCode session error: {}", err.raw),
                    ),
                    None => (
                        NormalizedEntryError::Other,
                        "OpenCode session error".to_string(),
                    ),
                };

                let idx = self.entry_index.next();
                self.add_normalized_entry_with_index(
                    idx,
                    NormalizedEntry {
                        timestamp: None,
                        entry_type: NormalizedEntryType::ErrorMessage { error_type },
                        content: message,
                        metadata: None,
                    },
                );
            }
            SdkEvent::Unknown { type_, properties } => {
                self.add_normalized_entry(system_message(format!(
                    "Unrecognized OpenCode SDK event type `{type_}`: {properties}"
                )));
            }
        }
    }

    fn handle_session_status(&mut self, status: SessionStatus) {
        match status {
            SessionStatus::Retry {
                attempt,
                message,
                next,
            } => {
                let fingerprint = format!("{attempt}:{next}:{message}");
                if self.retry_status_fingerprint.as_deref() == Some(fingerprint.as_str()) {
                    return;
                }
                self.retry_status_fingerprint = Some(fingerprint);

                self.add_normalized_entry(system_message(format!(
                    "OpenCode retry (attempt {attempt}): {message} (next in {next}ms)"
                )));
            }
            SessionStatus::Idle | SessionStatus::Busy | SessionStatus::Other => {}
        }
    }

    fn handle_todo_updated(&mut self, todos: &[SdkTodo], msg_store: &Arc<MsgStore>) {
        let fingerprint = fingerprint_todos(todos);
        if self.todo_update_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            return;
        }
        self.todo_update_fingerprint = Some(fingerprint);

        let mapped = todos
            .iter()
            .map(|todo| TodoItem {
                content: todo.content.clone(),
                status: todo.status.clone(),
                priority: Some(todo.priority.clone()),
            })
            .collect::<Vec<_>>();

        let entry = NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: "todo".to_string(),
                action_type: ActionType::TodoManagement {
                    todos: mapped,
                    operation: "update".to_string(),
                },
                status: ToolStatus::Success,
            },
            content: "TODO list updated".to_string(),
            metadata: None,
        };

        if let Some(index) = self.todo_update_entry {
            replace_normalized_entry(msg_store, index, entry);
        } else {
            let index = add_normalized_entry(msg_store, &self.entry_index, entry);
            self.todo_update_entry = Some(index);
        }
    }

    fn maybe_emit_model_system_message(&mut self, info: &MessageInfo) {
        if self.model_system_message_emitted {
            return;
        }

        let Some(model_id) = info.model_id() else {
            return;
        };
        let Some(provider_id) = info.provider_id() else {
            return;
        };

        self.add_normalized_entry(system_message(format!(
            "model: {model_id}  provider: {provider_id}"
        )));
        self.model_system_message_emitted = true;
    }

    fn maybe_emit_token_usage(&mut self, info: &MessageInfo) {
        if info.role != MessageRole::Assistant {
            return;
        }

        let Some(tokens) = info.tokens.as_ref() else {
            return;
        };

        let total_tokens = tokens.total_tokens().min(u32::MAX as u64) as u32;
        if total_tokens == 0 {
            return;
        }

        self.add_normalized_entry(NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::TokenUsageInfo(crate::logs::TokenUsageInfo {
                total_tokens,
                model_context_window: 0,
            }),
            content: format!("Tokens used: {} / Context window: 0", total_tokens),
            metadata: None,
        });
    }

    fn handle_part_update(
        &mut self,
        part: Part,
        delta: Option<&str>,
        worktree_path: &Path,
        msg_store: &Arc<MsgStore>,
    ) {
        match part {
            Part::Text(part) => {
                self.track_part_identity(part.id.as_deref(), &part.message_id, PartKind::Text);
                if self.message_roles.get(&part.message_id) != Some(&MessageRole::Assistant) {
                    tracing::debug!(
                        "Skipping text part for non-assistant message_id {}",
                        part.message_id
                    );
                    return;
                }

                let (text, mode) = if let Some(delta) = delta {
                    (delta, UpdateMode::Append)
                } else {
                    (part.text.as_str(), UpdateMode::Set)
                };

                let entry_index = self.entry_index.clone();
                update_streaming_text(
                    &entry_index,
                    text,
                    NormalizedEntryType::AssistantMessage,
                    &part.message_id,
                    &mut self.assistant_text,
                    msg_store,
                    mode,
                );
            }
            Part::Reasoning(part) => {
                self.track_part_identity(part.id.as_deref(), &part.message_id, PartKind::Reasoning);
                let (text, mode) = if let Some(delta) = delta {
                    (delta, UpdateMode::Append)
                } else {
                    (part.text.as_str(), UpdateMode::Set)
                };

                let entry_index = self.entry_index.clone();
                update_streaming_text(
                    &entry_index,
                    text,
                    NormalizedEntryType::Thinking,
                    &part.message_id,
                    &mut self.thinking_text,
                    msg_store,
                    mode,
                );
            }
            Part::Tool(part) => {
                let part = *part;
                self.track_part_identity(part.id.as_deref(), &part.message_id, PartKind::Other);
                if part.call_id.trim().is_empty() {
                    tracing::debug!(
                        "Skipping tool part with empty call_id for message_id {}",
                        part.message_id
                    );
                }

                let tool_state = self
                    .tool_states
                    .entry(part.call_id.clone())
                    .or_insert_with(|| ToolCallState::new(part.call_id.clone()));

                tool_state.set_approval_if_missing(self.approvals.get(&part.call_id).cloned());

                tool_state.update_from_part(part);
                let entry = tool_state.to_normalized_entry(worktree_path);
                if let Some(index) = tool_state.index {
                    replace_normalized_entry(msg_store, index, entry);
                } else {
                    let index = add_normalized_entry(msg_store, &self.entry_index, entry);
                    tool_state.index = Some(index);
                }
            }
            Part::Other => {}
        }
    }

    fn track_part_identity(&mut self, part_id: Option<&str>, message_id: &str, kind: PartKind) {
        let Some(part_id) = part_id.map(str::trim).filter(|part_id| !part_id.is_empty()) else {
            return;
        };

        self.part_identities.insert(
            part_id.to_string(),
            PartIdentity {
                message_id: message_id.to_string(),
                kind,
            },
        );
    }

    fn handle_part_delta(&mut self, event: MessagePartDeltaEvent, msg_store: &Arc<MsgStore>) {
        if event.field != "text" || event.delta.is_empty() {
            return;
        }

        let Some(part_identity) = self.part_identities.get(&event.part_id).cloned() else {
            return;
        };

        if part_identity.message_id != event.message_id {
            tracing::debug!(
                "Mismatched message_id for part_id {} (expected {}, got {}); routing by tracked part_id",
                event.part_id,
                part_identity.message_id,
                event.message_id
            );
        }

        let entry_index = self.entry_index.clone();
        match part_identity.kind {
            PartKind::Text => {
                if self.message_roles.get(&part_identity.message_id)
                    != Some(&MessageRole::Assistant)
                {
                    return;
                }

                update_streaming_text(
                    &entry_index,
                    &event.delta,
                    NormalizedEntryType::AssistantMessage,
                    &part_identity.message_id,
                    &mut self.assistant_text,
                    msg_store,
                    UpdateMode::Append,
                );
            }
            PartKind::Reasoning => {
                update_streaming_text(
                    &entry_index,
                    &event.delta,
                    NormalizedEntryType::Thinking,
                    &part_identity.message_id,
                    &mut self.thinking_text,
                    msg_store,
                    UpdateMode::Append,
                );
            }
            PartKind::Other => {}
        }
    }

    fn handle_approval_response(
        &mut self,
        tool_call_id: &str,
        status: ApprovalStatus,
        worktree_path: &Path,
        msg_store: &Arc<MsgStore>,
    ) {
        self.approvals
            .insert(tool_call_id.to_string(), status.clone());

        if let ApprovalStatus::Denied { reason } = &status {
            let tool_name = self
                .tool_states
                .get(tool_call_id)
                .map(|t| t.tool_name().to_string())
                .unwrap_or_else(|| "tool".to_string());

            let idx = self.entry_index.next();
            self.add_normalized_entry_with_index(
                idx,
                NormalizedEntry {
                    timestamp: None,
                    entry_type: NormalizedEntryType::UserFeedback {
                        denied_tool: tool_name,
                    },
                    content: reason
                        .clone()
                        .unwrap_or_else(|| "User denied this tool use request".to_string())
                        .trim()
                        .to_string(),
                    metadata: None,
                },
            );
        }

        let Some(tool_state) = self.tool_states.get_mut(tool_call_id) else {
            return;
        };

        tool_state.set_approval(status);

        let Some(index) = tool_state.index else {
            return;
        };

        replace_normalized_entry(
            msg_store,
            index,
            tool_state.to_normalized_entry(worktree_path),
        );
    }

    fn handle_permission_asked(
        &mut self,
        event: PermissionAskedEvent,
        worktree_path: &Path,
        msg_store: &Arc<MsgStore>,
    ) {
        let Some(tool) = event.tool else {
            self.add_normalized_entry(system_message(format!(
                "OpenCode permission requested: {}",
                event.permission
            )));
            return;
        };

        let call_id = tool.call_id.trim();
        if call_id.is_empty() {
            return;
        }

        let tool_state = self
            .tool_states
            .entry(call_id.to_string())
            .or_insert_with(|| ToolCallState::new(call_id.to_string()));

        // `permission` is an approval category (e.g. "edit", "bash"), not necessarily the tool
        // name ("write" vs "edit"). Only fall back to it when we haven't seen a tool name yet.
        if tool_state.tool_name() == "tool" {
            tool_state.set_tool_name(event.permission.clone());
        }

        // `permission.asked` can carry richer metadata than the initial tool part updates (e.g.
        // diffs for file edits). Store that data so users can review it before approving.
        if tool_state.other_metadata().is_none() && !event.metadata.is_null() {
            tool_state.set_other_metadata(event.metadata.clone());
        }

        if tool_state.file_edit_file_path().is_none() {
            if let Some(path) = extract_file_path_from_permission_metadata(&event.metadata) {
                tool_state.set_file_edit_file_path(path.to_string());
            } else if let Some(pattern) = event
                .patterns
                .iter()
                .find(|p| !p.trim().is_empty() && !p.contains('*') && !p.contains('?'))
            {
                tool_state.set_file_edit_file_path(pattern.trim().to_string());
            }
        }

        if let Some(diff) = extract_diff_from_metadata(&event.metadata)
            && !diff.trim().is_empty()
        {
            let should_update = match tool_state.file_edit_unified_diff() {
                None => true,
                Some(existing) => diff.len() > existing.len(),
            };
            if should_update {
                tool_state.set_file_edit_unified_diff(diff.to_string());
            }
        }

        let entry = tool_state.to_normalized_entry(worktree_path);
        if let Some(index) = tool_state.index {
            replace_normalized_entry(msg_store, index, entry);
        } else {
            let index = add_normalized_entry(msg_store, &self.entry_index, entry);
            tool_state.index = Some(index);
        }
    }

    async fn handle_question_asked(
        &mut self,
        event: QuestionAskedEvent,
        worktree_path: &Path,
        msg_store: &Arc<MsgStore>,
    ) {
        let call_id = event
            .tool
            .as_ref()
            .map(|tool| tool.call_id.trim().to_string())
            .filter(|call_id| !call_id.is_empty())
            .unwrap_or_else(|| event.id.clone());

        if call_id.trim().is_empty() {
            return;
        }

        let tool_state = self
            .tool_states
            .entry(call_id.clone())
            .or_insert_with(|| ToolCallState::new(call_id.clone()));

        tool_state.set_approval_if_missing(self.approvals.get(&call_id).cloned());

        if let Some(plan_exit) = detect_plan_exit_question(&event.questions) {
            let mut plan_content =
                if let Some(plan_relative_path) = plan_exit.plan_relative_path.as_deref() {
                    tokio::fs::read_to_string(worktree_path.join(plan_relative_path))
                        .await
                        .unwrap_or_default()
                } else {
                    String::new()
                };
            if plan_content.trim().is_empty() {
                plan_content = event
                    .questions
                    .first()
                    .map(|q| q.question.trim())
                    .filter(|q| !q.is_empty())
                    .map(str::to_string)
                    .unwrap_or_default();
            }

            tool_state.set_plan_presentation(plan_content);
        } else {
            tool_state.set_request_user_input(build_question_approval_input(&event));
        }

        let entry = tool_state.to_normalized_entry(worktree_path);
        if let Some(index) = tool_state.index {
            replace_normalized_entry(msg_store, index, entry);
        } else {
            let index = add_normalized_entry(msg_store, &self.entry_index, entry);
            tool_state.index = Some(index);
        }
    }

    fn add_normalized_entry(&mut self, entry: NormalizedEntry) -> usize {
        add_normalized_entry(&self.msg_store, &self.entry_index, entry)
    }

    fn add_normalized_entry_with_index(&mut self, index: usize, entry: NormalizedEntry) {
        self.msg_store
            .push_patch(crate::logs::utils::ConversationPatch::add_normalized_entry(
                index, entry,
            ));
    }
}

fn update_streaming_text(
    entry_index: &EntryIndexProvider,
    text: &str,
    entry_type: NormalizedEntryType,
    message_id: &str,
    map: &mut HashMap<String, StreamingText>,
    msg_store: &Arc<MsgStore>,
    mode: UpdateMode,
) {
    if text.is_empty() {
        return;
    }
    let state = map
        .entry(message_id.to_string())
        .or_insert_with(|| StreamingText {
            index: entry_index.next(),
            content: String::new(),
            emitted: false,
        });

    match mode {
        UpdateMode::Append => state.content.push_str(text),
        UpdateMode::Set => state.content = text.to_string(),
    }

    if state.content.trim().is_empty() {
        return;
    }

    let is_new = !state.emitted;
    state.emitted = true;

    let entry = NormalizedEntry {
        timestamp: None,
        entry_type,
        content: state.content.clone(),
        metadata: None,
    };
    upsert_normalized_entry(msg_store, state.index, entry, is_new);
}

#[derive(Debug, Clone)]
struct ToolCallState {
    index: Option<usize>,
    call_id: String,
    tool_name: String,
    state: ToolStateStatus,
    title: Option<String>,
    approval: Option<ApprovalStatus>,
    data: ToolData,
}

#[derive(Debug, Clone, Default)]
enum ToolData {
    #[default]
    Unknown,
    Bash {
        command: Option<String>,
        output: Option<String>,
        error: Option<String>,
        exit_code: Option<i32>,
    },
    Read {
        file_path: Option<String>,
    },
    FileEdit {
        kind: FileEditKind,
        file_path: Option<String>,
        write_content: Option<String>,
        unified_diff: Option<String>,
    },
    WebFetch {
        url: Option<String>,
    },
    Search {
        query: Option<String>,
    },
    Todo {
        operation: TodoOperation,
        todos: Vec<TodoItem>,
    },
    Task {
        description: Option<String>,
    },
    PlanPresentation {
        plan: String,
    },
    Other {
        input: Option<Value>,
        metadata: Option<Value>,
        output: Option<String>,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FileEditKind {
    #[default]
    Edit,
    Write,
    MultiEdit,
}

#[derive(Debug, Clone, Copy, Default)]
enum TodoOperation {
    Read,
    #[default]
    Write,
}

impl ToolCallState {
    fn new(call_id: String) -> Self {
        Self {
            index: None,
            call_id,
            tool_name: "tool".to_string(),
            state: ToolStateStatus::Unknown,
            title: None,
            approval: None,
            data: ToolData::Other {
                input: None,
                metadata: None,
                output: None,
                error: None,
            },
        }
    }

    fn tool_name(&self) -> &str {
        &self.tool_name
    }

    fn set_tool_name(&mut self, name: String) {
        if name.trim().is_empty() {
            return;
        }
        if matches!(self.data, ToolData::PlanPresentation { .. }) {
            return;
        }
        self.tool_name = name;
        self.maybe_promote();
    }

    fn set_approval_if_missing(&mut self, approval: Option<ApprovalStatus>) {
        if self.approval.is_none() {
            self.approval = approval;
        }
    }

    fn set_approval(&mut self, approval: ApprovalStatus) {
        self.approval = Some(approval);
    }

    fn set_plan_presentation(&mut self, plan: String) {
        self.tool_name = EXIT_PLAN_MODE_NAME.to_string();
        self.data = ToolData::PlanPresentation { plan };
    }

    fn set_request_user_input(&mut self, input: Value) {
        self.tool_name = REQUEST_USER_INPUT_TOOL_NAME.to_string();
        self.data = ToolData::Other {
            input: Some(input),
            metadata: None,
            output: None,
            error: None,
        };
    }

    fn tool_status(&self) -> ToolStatus {
        if let Some(ApprovalStatus::Denied { reason }) = &self.approval {
            return ToolStatus::Denied {
                reason: reason.clone(),
            };
        }
        if matches!(self.approval, Some(ApprovalStatus::TimedOut)) {
            return ToolStatus::TimedOut;
        }
        match self.state {
            ToolStateStatus::Completed => ToolStatus::Success,
            ToolStateStatus::Error => ToolStatus::Failed,
            _ => ToolStatus::Created,
        }
    }

    fn update_from_part(&mut self, part: ToolPart) {
        self.set_tool_name(part.tool.clone());

        let (input, output, metadata, error) = match &part.state {
            ToolStateUpdate::Pending { input } => {
                self.state = ToolStateStatus::Pending;
                (input.clone(), None, None, None)
            }
            ToolStateUpdate::Running {
                input,
                title,
                metadata,
            } => {
                self.state = ToolStateStatus::Running;
                if let Some(t) = title.as_ref().filter(|t| !t.trim().is_empty()) {
                    self.title = Some(t.clone());
                }
                (input.clone(), None, metadata.clone(), None)
            }
            ToolStateUpdate::Completed {
                input,
                output,
                title,
                metadata,
            } => {
                self.state = ToolStateStatus::Completed;
                if let Some(t) = title.as_ref().filter(|t| !t.trim().is_empty()) {
                    self.title = Some(t.clone());
                }
                (input.clone(), output.clone(), metadata.clone(), None)
            }
            ToolStateUpdate::Error {
                input,
                error,
                metadata,
            } => {
                self.state = ToolStateStatus::Error;
                let err = error.clone().filter(|e| !e.trim().is_empty());
                (input.clone(), None, metadata.clone(), err)
            }
            ToolStateUpdate::Unknown => (None, None, None, None),
        };

        self.apply_tool_data(input, output, metadata, error);
    }

    fn apply_tool_data(
        &mut self,
        input: Option<Value>,
        output: Option<String>,
        metadata: Option<Value>,
        error: Option<String>,
    ) {
        match &mut self.data {
            ToolData::Bash {
                command,
                output: out,
                error: err,
                exit_code,
            } => {
                if let Some(v) = input.and_then(|v| serde_json::from_value::<BashInput>(v).ok()) {
                    *command = Some(v.command);
                }
                if let Some(o) = output {
                    *out = Some(o);
                    *err = None;
                }
                if let Some(e) = error {
                    *err = Some(e);
                }
                if let Some(m) = metadata {
                    *exit_code = m.get("exit").and_then(Value::as_i64).map(|c| c as i32);
                }
            }
            ToolData::Read { file_path } => {
                if let Some(v) = input.and_then(|v| serde_json::from_value::<FilePathInput>(v).ok())
                {
                    *file_path = Some(v.file_path);
                }
            }
            ToolData::FileEdit {
                kind,
                file_path,
                write_content,
                unified_diff,
            } => {
                if let Some(inp) = input {
                    match kind {
                        FileEditKind::Write => {
                            if let Ok(v) = serde_json::from_value::<WriteInput>(inp) {
                                *file_path = Some(v.file_path);
                                *write_content = Some(v.content);
                            }
                        }
                        FileEditKind::Edit | FileEditKind::MultiEdit => {
                            if let Ok(v) = serde_json::from_value::<FilePathInput>(inp) {
                                *file_path = Some(v.file_path);
                            }
                        }
                    }
                }
                if matches!(kind, FileEditKind::Edit | FileEditKind::MultiEdit)
                    && let Some(m) = metadata
                    && let Some(d) = extract_diff_from_metadata(&m)
                {
                    *unified_diff = Some(d.to_string());
                }
            }
            ToolData::WebFetch { url } => {
                if let Some(u) =
                    input.and_then(|v| v.get("url").and_then(Value::as_str).map(str::to_string))
                {
                    *url = Some(u);
                }
            }
            ToolData::Search { query } => {
                if let Some(inp) = input {
                    *query = inp
                        .get("query")
                        .and_then(Value::as_str)
                        .or_else(|| inp.get("pattern").and_then(Value::as_str))
                        .map(str::to_string);
                }
            }
            ToolData::Todo { operation, todos } => {
                let source = match operation {
                    TodoOperation::Write => input,
                    TodoOperation::Read => metadata,
                };
                if let Some(v) =
                    source.and_then(|v| serde_json::from_value::<TodosContainer>(v).ok())
                {
                    *todos = v.todos;
                }
            }
            ToolData::Task { description } => {
                if let Some(d) = input.and_then(|v| {
                    v.get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }) {
                    *description = Some(d);
                }
            }
            ToolData::PlanPresentation { .. } => {}
            ToolData::Unknown => {
                // Upgrade Unknown to Other when we receive tool data
                self.data = ToolData::Other {
                    input: None,
                    metadata: None,
                    output: None,
                    error: None,
                };
                self.apply_tool_data(input, output, metadata, error);
            }
            ToolData::Other {
                input: inp,
                metadata: meta,
                output: out,
                error: err,
            } => {
                if let Some(i) = input {
                    *inp = Some(i);
                }
                if let Some(m) = metadata {
                    *meta = Some(m);
                }
                if let Some(o) = output {
                    *out = Some(o);
                    *err = None;
                }
                if let Some(e) = error {
                    *err = Some(e);
                }
            }
        }
    }

    /// Promote from generic/unknown to a specific tool type when tool name is recognized.
    fn maybe_promote(&mut self) {
        let (inp, meta, out, err) = match &self.data {
            ToolData::Other {
                input,
                metadata,
                output,
                error,
            } => (
                input.clone(),
                metadata.clone(),
                output.clone(),
                error.clone(),
            ),
            ToolData::Unknown => (None, None, None, None),
            _ => return, // Already promoted
        };

        self.data = match self.tool_name.as_str() {
            "bash" => ToolData::Bash {
                command: None,
                output: out.clone(),
                error: err.clone(),
                exit_code: None,
            },
            "read" => ToolData::Read { file_path: None },
            "edit" | "write" | "multiedit" => ToolData::FileEdit {
                kind: match self.tool_name.as_str() {
                    "write" => FileEditKind::Write,
                    "multiedit" => FileEditKind::MultiEdit,
                    _ => FileEditKind::Edit,
                },
                file_path: None,
                write_content: None,
                unified_diff: None,
            },
            "webfetch" => ToolData::WebFetch { url: None },
            "websearch" | "codesearch" | "grep" | "glob" => ToolData::Search { query: None },
            "todoread" | "todowrite" => ToolData::Todo {
                operation: if self.tool_name == "todoread" {
                    TodoOperation::Read
                } else {
                    TodoOperation::Write
                },
                todos: vec![],
            },
            "task" => ToolData::Task { description: None },
            _ => return,
        };

        // Re-apply the data we had (only needed for non-Bash tools as Bash already has out/err)
        self.apply_tool_data(inp, out, meta, err);
    }

    fn to_normalized_entry(&self, worktree_path: &Path) -> NormalizedEntry {
        let action_type = self.build_action_type(worktree_path);
        let content = self.build_content(&action_type);
        NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: self.tool_name.clone(),
                action_type,
                status: self.tool_status(),
            },
            content,
            metadata: serde_json::to_value(ToolCallMetadata {
                tool_call_id: self.call_id.clone(),
            })
            .ok(),
        }
    }

    fn build_action_type(&self, worktree_path: &Path) -> ActionType {
        match &self.data {
            ToolData::Bash {
                command,
                output,
                error,
                exit_code,
            } => ActionType::CommandRun {
                command: command.clone().unwrap_or_default(),
                result: Some(CommandRunResult {
                    exit_status: exit_code.map(|code| CommandExitStatus::ExitCode { code }),
                    output: output.as_deref().or(error.as_deref()).map(str::to_string),
                }),
            },
            ToolData::Read { file_path } => ActionType::FileRead {
                path: file_path
                    .as_deref()
                    .map(|p| make_relative_path(p, worktree_path))
                    .unwrap_or_default(),
            },
            ToolData::FileEdit {
                kind,
                file_path,
                write_content,
                unified_diff,
            } => {
                let path = file_path
                    .as_deref()
                    .map(|p| make_relative_path(p, worktree_path))
                    .unwrap_or_default();
                let changes = match kind {
                    FileEditKind::Write => write_content
                        .as_ref()
                        .filter(|s| !s.is_empty())
                        .map(|c| vec![FileChange::Write { content: c.clone() }])
                        .unwrap_or_default(),
                    FileEditKind::Edit | FileEditKind::MultiEdit => unified_diff
                        .as_ref()
                        .map(|d| {
                            vec![FileChange::Edit {
                                unified_diff: workspace_utils::diff::normalize_unified_diff(
                                    &path, d,
                                ),
                                has_line_numbers: true,
                            }]
                        })
                        .unwrap_or_default(),
                };
                ActionType::FileEdit { path, changes }
            }
            ToolData::WebFetch { url } => ActionType::WebFetch {
                url: url.clone().unwrap_or_default(),
            },
            ToolData::Search { query } => ActionType::Search {
                query: query.clone().unwrap_or_default(),
            },
            ToolData::Todo { operation, todos } => ActionType::TodoManagement {
                todos: todos.clone(),
                operation: match operation {
                    TodoOperation::Read => "read",
                    TodoOperation::Write => "write",
                }
                .to_string(),
            },
            ToolData::Task { description } => ActionType::TaskCreate {
                description: description.clone().unwrap_or_default(),
            },
            ToolData::PlanPresentation { plan } => {
                ActionType::PlanPresentation { plan: plan.clone() }
            }
            ToolData::Unknown => ActionType::Tool {
                tool_name: self.tool_name.clone(),
                arguments: None,
                result: None,
            },
            ToolData::Other {
                input,
                output,
                error,
                ..
            } => ActionType::Tool {
                tool_name: self.tool_name.clone(),
                arguments: input
                    .as_ref()
                    .and_then(|v| v.as_object().map(|_| v.clone())),
                result: output
                    .as_deref()
                    .or(error.as_deref())
                    .map(|o| ToolResult::markdown(o.to_string())),
            },
        }
    }

    fn build_content(&self, action_type: &ActionType) -> String {
        let content = match action_type {
            ActionType::CommandRun { command, .. } => command.clone(),
            ActionType::FileRead { path } => path.clone(),
            ActionType::FileEdit { path, .. } => path.clone(),
            ActionType::Search { query } => query.clone(),
            ActionType::WebFetch { url } => url.clone(),
            ActionType::PlanPresentation { plan } => plan.clone(),
            ActionType::TodoManagement { .. } => "TODO list updated".to_string(),
            _ => String::new(),
        }
        .trim()
        .to_string();

        if !content.is_empty() {
            content
        } else {
            self.title.as_deref().unwrap_or(&self.tool_name).to_string()
        }
    }

    /// Access to metadata for permission.asked handling
    fn other_metadata(&self) -> Option<&Value> {
        match &self.data {
            ToolData::Other { metadata, .. } => metadata.as_ref(),
            _ => None,
        }
    }

    fn set_other_metadata(&mut self, meta: Value) {
        if let ToolData::Other { metadata, .. } = &mut self.data {
            *metadata = Some(meta);
        }
    }

    fn file_edit_file_path(&self) -> Option<&str> {
        match &self.data {
            ToolData::FileEdit { file_path, .. } => file_path.as_deref(),
            _ => None,
        }
    }

    fn set_file_edit_file_path(&mut self, path: String) {
        if let ToolData::FileEdit { file_path, .. } = &mut self.data {
            *file_path = Some(path);
        }
    }

    fn file_edit_unified_diff(&self) -> Option<&str> {
        match &self.data {
            ToolData::FileEdit { unified_diff, .. } => unified_diff.as_deref(),
            _ => None,
        }
    }

    fn set_file_edit_unified_diff(&mut self, diff: String) {
        if let ToolData::FileEdit { unified_diff, .. } = &mut self.data {
            *unified_diff = Some(diff);
        }
    }
}

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
}

#[derive(Debug, Deserialize)]
struct FilePathInput {
    #[serde(rename = "filePath")]
    file_path: String,
}

#[derive(Debug, Deserialize)]
struct WriteInput {
    #[serde(rename = "filePath")]
    file_path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct TodosContainer {
    #[serde(default)]
    todos: Vec<TodoItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolStateStatus {
    Pending,
    Running,
    Completed,
    Error,
    Unknown,
}

fn make_relative_path(path: &str, worktree_path: &Path) -> String {
    make_path_relative(path, &worktree_path.to_string_lossy())
}

fn fingerprint_todos(todos: &[SdkTodo]) -> String {
    let mut parts = todos
        .iter()
        .map(|t| format!("{}:{}:{}:{}", t.id, t.status, t.priority, t.content))
        .collect::<Vec<_>>();
    parts.sort();
    parts.join("|")
}

fn extract_diff_from_metadata(metadata: &Value) -> Option<&str> {
    metadata.get("diff").and_then(Value::as_str).or_else(|| {
        metadata
            .get("results")
            .and_then(Value::as_array)
            .and_then(|results| results.last())
            .and_then(|last| last.get("diff"))
            .and_then(Value::as_str)
    })
}

fn extract_file_path_from_permission_metadata(metadata: &Value) -> Option<&str> {
    let candidate = metadata
        .get("filePath")
        .and_then(Value::as_str)
        .or_else(|| metadata.get("filepath").and_then(Value::as_str))
        .or_else(|| metadata.get("path").and_then(Value::as_str))
        .or_else(|| metadata.get("file").and_then(Value::as_str))?;

    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn build_question_approval_input(event: &QuestionAskedEvent) -> Value {
    let questions = event
        .questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let options = question
                .options
                .iter()
                .map(|option| {
                    let mut payload = serde_json::Map::new();
                    payload.insert("label".to_string(), Value::String(option.label.clone()));
                    payload.insert("value".to_string(), Value::String(option.label.clone()));
                    if let Some(description) = option
                        .description
                        .as_ref()
                        .map(|description| description.trim())
                        .filter(|description| !description.is_empty())
                    {
                        payload.insert(
                            "description".to_string(),
                            Value::String(description.to_string()),
                        );
                    }
                    Value::Object(payload)
                })
                .collect::<Vec<_>>();

            let mut payload = serde_json::Map::new();
            payload.insert("id".to_string(), Value::String(question_approval_id(index)));
            payload.insert(
                "header".to_string(),
                Value::String(
                    question
                        .header
                        .as_deref()
                        .map(str::trim)
                        .filter(|header| !header.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("Question {}", index + 1)),
                ),
            );
            payload.insert(
                "question".to_string(),
                Value::String(question.question.trim().to_string()),
            );
            payload.insert("options".to_string(), Value::Array(options));

            if question.multiple.unwrap_or(false) {
                payload.insert("multiSelectMin".to_string(), Value::Number(1.into()));
                payload.insert(
                    "multiSelectMax".to_string(),
                    Value::Number((question.options.len().max(1) as u64).into()),
                );
            }

            if let Some(custom) = question.custom {
                payload.insert("custom".to_string(), Value::Bool(custom));
            }

            Value::Object(payload)
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "id": event.id,
        "session_id": event.session_id,
        "questions": questions,
    })
}

fn question_approval_id(index: usize) -> String {
    format!("question_{}", index + 1)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use serde_json::{Value, json};
    use workspace_utils::{log_msg::LogMsg, msg_store::MsgStore};

    use super::*;
    use crate::{
        approvals::ToolCallMetadata,
        logs::{
            ActionType, NormalizedEntry, NormalizedEntryType, ToolStatus,
            utils::patch::extract_normalized_entry_from_patch,
        },
    };

    fn new_state() -> (LogState, Arc<MsgStore>) {
        let msg_store = Arc::new(MsgStore::new());
        let state = LogState::new(EntryIndexProvider::test_new(), msg_store.clone());
        (state, msg_store)
    }

    fn collect_entries(msg_store: &MsgStore) -> Vec<NormalizedEntry> {
        let mut entries = BTreeMap::new();
        for msg in msg_store.get_history() {
            let LogMsg::JsonPatch(patch) = msg else {
                continue;
            };

            if let Some((index, entry)) = extract_normalized_entry_from_patch(&patch) {
                entries.insert(index, entry);
            }
        }
        entries.into_values().collect()
    }

    #[tokio::test]
    async fn message_part_delta_appends_assistant_text() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.updated",
                    "properties": {
                        "info": {
                            "id": "message-1",
                            "role": "assistant"
                        }
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.updated",
                    "properties": {
                        "part": {
                            "type": "text",
                            "id": "part-1",
                            "messageID": "message-1",
                            "text": ""
                        }
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.delta",
                    "properties": {
                        "sessionID": "session-1",
                        "messageID": "message-1",
                        "partID": "part-1",
                        "field": "text",
                        "delta": "Hel"
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.delta",
                    "properties": {
                        "sessionID": "session-1",
                        "messageID": "message-1",
                        "partID": "part-1",
                        "field": "text",
                        "delta": "lo"
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        let entries = collect_entries(&msg_store);
        let assistant_entry = entries
            .iter()
            .find(|entry| matches!(entry.entry_type, NormalizedEntryType::AssistantMessage))
            .expect("assistant entry should be present");
        assert_eq!(assistant_entry.content, "Hello");
        assert!(!entries.iter().any(|entry| {
            matches!(entry.entry_type, NormalizedEntryType::SystemMessage)
                && entry
                    .content
                    .contains("Unrecognized OpenCode SDK event type")
        }));
    }

    #[tokio::test]
    async fn message_part_delta_appends_reasoning_text() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.updated",
                    "properties": {
                        "info": {
                            "id": "message-2",
                            "role": "assistant"
                        }
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.updated",
                    "properties": {
                        "part": {
                            "type": "reasoning",
                            "id": "part-2",
                            "messageID": "message-2",
                            "text": ""
                        }
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.delta",
                    "properties": {
                        "sessionID": "session-1",
                        "messageID": "message-2",
                        "partID": "part-2",
                        "field": "text",
                        "delta": "Think"
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.delta",
                    "properties": {
                        "sessionID": "session-1",
                        "messageID": "message-2",
                        "partID": "part-2",
                        "field": "text",
                        "delta": "ing"
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        let entries = collect_entries(&msg_store);
        let thinking_entry = entries
            .iter()
            .find(|entry| matches!(entry.entry_type, NormalizedEntryType::Thinking))
            .expect("thinking entry should be present");
        assert_eq!(thinking_entry.content, "Thinking");
    }

    #[tokio::test]
    async fn whitespace_only_streaming_text_is_filtered_until_visible_text_arrives() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.updated",
                    "properties": {
                        "info": {
                            "id": "message-ws-1",
                            "role": "assistant"
                        }
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.updated",
                    "properties": {
                        "part": {
                            "type": "text",
                            "id": "part-ws-text-1",
                            "messageID": "message-ws-1",
                            "text": ""
                        }
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.delta",
                    "properties": {
                        "sessionID": "session-1",
                        "messageID": "message-ws-1",
                        "partID": "part-ws-text-1",
                        "field": "text",
                        "delta": " \n\t"
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        tokio::time::sleep(Duration::from_millis(20)).await;
        let entries = collect_entries(&msg_store);
        assert!(
            !entries
                .iter()
                .any(|entry| matches!(entry.entry_type, NormalizedEntryType::AssistantMessage)),
            "whitespace-only assistant text should not emit an entry"
        );

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.delta",
                    "properties": {
                        "sessionID": "session-1",
                        "messageID": "message-ws-1",
                        "partID": "part-ws-text-1",
                        "field": "text",
                        "delta": "Hello"
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        let entries = collect_entries(&msg_store);
        let assistant_entry = entries
            .iter()
            .find(|entry| matches!(entry.entry_type, NormalizedEntryType::AssistantMessage))
            .expect("assistant entry should be present after visible content");
        assert_eq!(assistant_entry.content, " \n\tHello");

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.updated",
                    "properties": {
                        "part": {
                            "type": "reasoning",
                            "id": "part-ws-reasoning-1",
                            "messageID": "message-ws-1",
                            "text": ""
                        }
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.delta",
                    "properties": {
                        "sessionID": "session-1",
                        "messageID": "message-ws-1",
                        "partID": "part-ws-reasoning-1",
                        "field": "text",
                        "delta": "\n  "
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        tokio::time::sleep(Duration::from_millis(20)).await;
        let entries = collect_entries(&msg_store);
        assert!(
            !entries
                .iter()
                .any(|entry| matches!(entry.entry_type, NormalizedEntryType::Thinking)),
            "whitespace-only reasoning text should not emit an entry"
        );

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.delta",
                    "properties": {
                        "sessionID": "session-1",
                        "messageID": "message-ws-1",
                        "partID": "part-ws-reasoning-1",
                        "field": "text",
                        "delta": "Think"
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        let entries = collect_entries(&msg_store);
        let thinking_entry = entries
            .iter()
            .find(|entry| matches!(entry.entry_type, NormalizedEntryType::Thinking))
            .expect("thinking entry should be present after visible content");
        assert_eq!(thinking_entry.content, "\n  Think");
    }

    #[tokio::test]
    async fn message_updated_emits_token_usage_entry() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.updated",
                    "properties": {
                        "info": {
                            "id": "message-usage-1",
                            "role": "assistant",
                            "tokens": {
                                "input": 300,
                                "output": 21,
                                "reasoning": 9,
                                "cache": {
                                    "read": 10,
                                    "write": 1
                                }
                            }
                        }
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        let entries = collect_entries(&msg_store);
        let usage_entry = entries
            .iter()
            .find(|entry| matches!(entry.entry_type, NormalizedEntryType::TokenUsageInfo(_)))
            .expect("token usage entry should be present");

        match usage_entry.entry_type {
            NormalizedEntryType::TokenUsageInfo(ref usage) => {
                assert_eq!(usage.total_tokens, 341);
                assert_eq!(usage.model_context_window, 0);
            }
            _ => panic!("expected token usage entry"),
        }
        assert_eq!(usage_entry.content, "Tokens used: 341 / Context window: 0");
    }

    #[tokio::test]
    async fn message_part_delta_for_unknown_part_is_ignored() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.delta",
                    "properties": {
                        "sessionID": "session-1",
                        "messageID": "message-3",
                        "partID": "unknown-part",
                        "field": "text",
                        "delta": "ignored"
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        let entries = collect_entries(&msg_store);
        assert!(entries.is_empty());
    }

    fn create_temp_worktree(plan_content: &str) -> PathBuf {
        static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos();
        let id = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "opencode-normalize-{unique}-{id}-{}",
            std::process::id()
        ));
        fs::create_dir_all(root.join(".opencode/plans")).expect("create plan dir");
        fs::write(root.join(".opencode/plans/test-plan.md"), plan_content)
            .expect("write plan file");
        root
    }

    fn find_tool_entry<'a>(entries: &'a [NormalizedEntry], call_id: &str) -> &'a NormalizedEntry {
        entries
            .iter()
            .find(|entry| {
                let NormalizedEntryType::ToolUse { .. } = &entry.entry_type else {
                    return false;
                };

                entry
                    .metadata
                    .as_ref()
                    .and_then(|metadata| {
                        serde_json::from_value::<ToolCallMetadata>(metadata.clone()).ok()
                    })
                    .is_some_and(|metadata| metadata.tool_call_id == call_id)
            })
            .expect("tool entry should exist")
    }

    #[tokio::test]
    async fn question_asked_plan_exit_creates_plan_presentation_entry() {
        let (mut state, msg_store) = new_state();
        let plan_text = "# Plan\n- Investigate\n- Implement";
        let worktree_path = create_temp_worktree(plan_text);

        state.handle_sdk_event(
            &json!({
                "type": "question.asked",
                "properties": {
                    "id": "question-1",
                    "sessionID": "session-1",
                    "questions": [{
                        "question": "Plan at .opencode/plans/test-plan.md is complete. Would you like to switch to the build agent and start implementing?",
                        "header": "Plan Review",
                        "options": [
                            {"label": "Yes", "description": "Switch"},
                            {"label": "No", "description": "Stay"}
                        ],
                        "custom": false
                    }],
                    "tool": {
                        "messageID": "message-1",
                        "callID": "call-plan-1"
                    }
                }
            }),
            worktree_path.as_path(),
            &msg_store,
        ).await;

        let entries = collect_entries(&msg_store);
        let entry = find_tool_entry(&entries, "call-plan-1");
        assert_eq!(entry.content, plan_text);
        match &entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                status,
            } => {
                assert_eq!(tool_name, EXIT_PLAN_MODE_NAME);
                assert!(matches!(status, ToolStatus::Created));
                assert!(matches!(
                    action_type,
                    ActionType::PlanPresentation { plan } if plan == plan_text
                ));
            }
            _ => panic!("expected tool entry"),
        }

        let _ = fs::remove_dir_all(worktree_path);
    }

    #[tokio::test]
    async fn question_asked_plan_exit_without_tool_metadata_uses_question_id() {
        let (mut state, msg_store) = new_state();
        let plan_text = "# Plan\n- Research\n- Ship";
        let worktree_path = create_temp_worktree(plan_text);
        let absolute_plan_path = worktree_path
            .join(".opencode/plans/test-plan.md")
            .to_string_lossy()
            .to_string();

        state
            .handle_sdk_event(
                &json!({
                    "type": "question.asked",
                    "properties": {
                        "id": "question-no-tool",
                        "sessionID": "session-1",
                        "questions": [{
                            "question": format!(
                                "Plan ready at {absolute_plan_path}; switch to build mode now?"
                            ),
                            "header": "Plan Review",
                            "options": [
                                {"label": "Yes", "description": "Switch"},
                                {"label": "No", "description": "Stay"}
                            ],
                            "custom": false
                        }]
                    }
                }),
                worktree_path.as_path(),
                &msg_store,
            )
            .await;

        let entries = collect_entries(&msg_store);
        let entry = find_tool_entry(&entries, "question-no-tool");
        assert_eq!(entry.content, plan_text);
        match &entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                status,
            } => {
                assert_eq!(tool_name, EXIT_PLAN_MODE_NAME);
                assert!(matches!(status, ToolStatus::Created));
                assert!(matches!(
                    action_type,
                    ActionType::PlanPresentation { plan } if plan == plan_text
                ));
            }
            _ => panic!("expected tool entry"),
        }

        let _ = fs::remove_dir_all(worktree_path);
    }

    #[tokio::test]
    async fn question_asked_non_plan_creates_request_user_input_entry() {
        let (mut state, msg_store) = new_state();
        let worktree_path = create_temp_worktree("# Plan");

        state
            .handle_sdk_event(
                &json!({
                    "type": "question.asked",
                    "properties": {
                        "id": "question-input-1",
                        "sessionID": "session-1",
                        "questions": [{
                            "question": "Which format should I use?",
                            "header": "Format",
                            "options": [
                                {"label": "JSON", "description": "Structured output"},
                                {"label": "Markdown", "description": "Human-readable output"}
                            ],
                            "custom": true
                        }],
                        "tool": {
                            "messageID": "message-3",
                            "callID": "call-input-1"
                        }
                    }
                }),
                worktree_path.as_path(),
                &msg_store,
            )
            .await;

        let entries = collect_entries(&msg_store);
        let entry = find_tool_entry(&entries, "call-input-1");
        match &entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                status,
            } => {
                assert_eq!(tool_name, REQUEST_USER_INPUT_TOOL_NAME);
                assert!(matches!(status, ToolStatus::Created));
                match action_type {
                    ActionType::Tool {
                        tool_name,
                        arguments: Some(arguments),
                        ..
                    } => {
                        assert_eq!(tool_name, REQUEST_USER_INPUT_TOOL_NAME);
                        assert_eq!(arguments.get("id"), Some(&json!("question-input-1")));
                        assert_eq!(
                            arguments
                                .pointer("/questions/0/options/0/value")
                                .and_then(Value::as_str),
                            Some("JSON")
                        );
                    }
                    other => panic!("expected generic tool action, got {other:?}"),
                }
            }
            _ => panic!("expected tool entry"),
        }

        let _ = fs::remove_dir_all(worktree_path);
    }

    #[tokio::test]
    async fn question_asked_with_plan_path_but_non_plan_title_stays_request_user_input() {
        let (mut state, msg_store) = new_state();
        let worktree_path = create_temp_worktree("# Plan\n- Step");

        state
            .handle_sdk_event(
                &json!({
                    "type": "question.asked",
                    "properties": {
                        "id": "question-input-2",
                        "sessionID": "session-1",
                        "questions": [{
                            "question": "Plan at .opencode/plans/test-plan.md is complete. Continue?",
                            "header": "Build Agent",
                            "options": [
                                {"label": "Yes", "description": "Switch"},
                                {"label": "No", "description": "Stay"}
                            ],
                            "custom": false
                        }],
                        "tool": {
                            "messageID": "message-4",
                            "callID": "call-input-2"
                        }
                    }
                }),
                worktree_path.as_path(),
                &msg_store,
            )
            .await;

        let entries = collect_entries(&msg_store);
        let entry = find_tool_entry(&entries, "call-input-2");
        match &entry.entry_type {
            NormalizedEntryType::ToolUse { tool_name, .. } => {
                assert_eq!(tool_name, REQUEST_USER_INPUT_TOOL_NAME);
            }
            _ => panic!("expected tool entry"),
        }

        let _ = fs::remove_dir_all(worktree_path);
    }

    #[tokio::test]
    async fn todo_updated_without_todo_ids_is_normalized() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state
            .handle_sdk_event(
                &json!({
                    "type": "todo.updated",
                    "properties": {
                        "sessionID": "session-1",
                        "todos": [{
                            "content": "Inspect callback handling",
                            "status": "in_progress",
                            "priority": "high"
                        }]
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        let entries = collect_entries(&msg_store);
        let entry = entries
            .iter()
            .find(|entry| {
                matches!(
                    entry.entry_type,
                    NormalizedEntryType::ToolUse {
                        action_type: ActionType::TodoManagement { .. },
                        ..
                    }
                )
            })
            .expect("todo entry should be present");

        assert_eq!(entry.content, "TODO list updated");
        assert!(!entries.iter().any(|entry| {
            matches!(entry.entry_type, NormalizedEntryType::SystemMessage)
                && entry.content.contains("Unrecognized OpenCode SDK event")
        }));
    }

    #[tokio::test]
    async fn session_updated_is_ignored_without_unrecognized_event_message() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state
            .handle_sdk_event(
                &json!({
                    "type": "session.updated",
                    "properties": {
                        "sessionID": "ses_2ac5898b4ffenPeALw8QaicAqa",
                        "info": {
                            "id": "ses_2ac5898b4ffenPeALw8QaicAqa",
                            "slug": "nimble-forest",
                            "projectID": "ea65d2f433cf63cf7c5b6683e5562db26a9d773e",
                            "directory": "/tmp/worktree",
                            "title": "New session - 2026-04-03T14:02:53.131Z",
                            "version": "1.3.13",
                            "time": {
                                "created": 1775224973131_u64,
                                "updated": 1775224973189_u64
                            }
                        }
                    }
                }),
                worktree_path,
                &msg_store,
            )
            .await;

        let entries = collect_entries(&msg_store);
        assert!(!entries.iter().any(|entry| {
            matches!(entry.entry_type, NormalizedEntryType::SystemMessage)
                && entry.content.contains("Unrecognized OpenCode SDK event")
        }));
    }

    #[tokio::test]
    async fn plan_presentation_survives_tool_state_and_approval_updates() {
        let (mut state, msg_store) = new_state();
        let plan_text = "# Plan\n- Step 1\n- Step 2";
        let worktree_path = create_temp_worktree(plan_text);

        state.handle_sdk_event(
            &json!({
                "type": "question.asked",
                "properties": {
                    "id": "question-2",
                    "sessionID": "session-1",
                    "questions": [{
                        "question": "Plan at .opencode/plans/test-plan.md is complete. Would you like to switch to the build agent and start implementing?",
                        "header": "Plan Review",
                        "options": [
                            {"label": "Yes", "description": "Switch"},
                            {"label": "No", "description": "Stay"}
                        ],
                        "custom": false
                    }],
                    "tool": {
                        "messageID": "message-2",
                        "callID": "call-plan-2"
                    }
                }
            }),
            worktree_path.as_path(),
            &msg_store,
        ).await;

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.updated",
                    "properties": {
                        "part": {
                            "type": "tool",
                            "id": "part-1",
                            "messageID": "message-2",
                            "callID": "call-plan-2",
                            "tool": "plan_exit",
                            "state": {
                                "status": "running",
                                "title": "Waiting for plan approval"
                            }
                        }
                    }
                }),
                worktree_path.as_path(),
                &msg_store,
            )
            .await;

        state.handle_approval_response(
            "call-plan-2",
            ApprovalStatus::Approved,
            worktree_path.as_path(),
            &msg_store,
        );

        state
            .handle_sdk_event(
                &json!({
                    "type": "message.part.updated",
                    "properties": {
                        "part": {
                            "type": "tool",
                            "id": "part-1",
                            "messageID": "message-2",
                            "callID": "call-plan-2",
                            "tool": "plan_exit",
                            "state": {
                                "status": "completed",
                                "output": "done"
                            }
                        }
                    }
                }),
                worktree_path.as_path(),
                &msg_store,
            )
            .await;

        let entries = collect_entries(&msg_store);
        let entry = find_tool_entry(&entries, "call-plan-2");
        assert_eq!(entry.content, plan_text);
        match &entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                status,
            } => {
                assert_eq!(tool_name, EXIT_PLAN_MODE_NAME);
                assert!(matches!(status, ToolStatus::Success));
                assert!(matches!(
                    action_type,
                    ActionType::PlanPresentation { plan } if plan == plan_text
                ));
            }
            _ => panic!("expected tool entry"),
        }

        let _ = fs::remove_dir_all(worktree_path);
    }
}
