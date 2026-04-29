use std::{collections::HashMap, env, path::Path, sync::Arc};

use futures::StreamExt;
use serde_json::Value;
use workspace_utils::{approvals::ApprovalStatus, msg_store::MsgStore, path::make_path_relative};

use super::types::{PiExecutorEvent, extract_agent_end_error};
use crate::{
    approvals::ToolCallMetadata,
    logs::{
        ActionType, CommandExitStatus, CommandRunResult, FileChange, NormalizedEntry,
        NormalizedEntryError, NormalizedEntryType, ToolResult, ToolStatus,
        stderr_processor::normalize_stderr_logs,
        utils::{
            EntryIndexProvider,
            patch::{add_normalized_entry, replace_normalized_entry, upsert_normalized_entry},
        },
    },
};

#[derive(Debug, Clone)]
struct StreamingText {
    index: usize,
    content: String,
}

#[derive(Debug, Clone, Copy)]
enum UpdateMode {
    Append,
    Set,
}

#[derive(Debug, Clone)]
struct ToolCallState {
    index: Option<usize>,
    tool_call_id: String,
    tool_name: String,
    input: Option<Value>,
    output: Option<Value>,
    status: ToolStatus,
}

impl ToolCallState {
    fn new(tool_call_id: String, tool_name: String) -> Self {
        Self {
            index: None,
            tool_call_id,
            tool_name,
            input: None,
            output: None,
            status: ToolStatus::Created,
        }
    }

    fn to_normalized_entry(&self, worktree_path: &Path) -> NormalizedEntry {
        let (action_type, content) = self.to_action_and_content(worktree_path);
        NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: self.tool_name.clone(),
                action_type,
                status: self.status.clone(),
            },
            content,
            metadata: serde_json::to_value(ToolCallMetadata {
                tool_call_id: self.tool_call_id.clone(),
            })
            .ok(),
        }
    }

    fn to_action_and_content(&self, worktree_path: &Path) -> (ActionType, String) {
        match self.tool_name.as_str() {
            "read" => {
                let path = self
                    .input
                    .as_ref()
                    .and_then(|input| {
                        extract_first_string(input, &["/path", "/file", "/file_path"])
                    })
                    .map(|path| make_relative_path(&path, worktree_path))
                    .unwrap_or_default();
                (ActionType::FileRead { path: path.clone() }, path)
            }
            "write" => {
                let path = self
                    .input
                    .as_ref()
                    .and_then(|input| {
                        extract_first_string(input, &["/path", "/file", "/file_path"])
                    })
                    .map(|path| make_relative_path(&path, worktree_path))
                    .unwrap_or_default();
                let content = self
                    .input
                    .as_ref()
                    .and_then(|input| extract_first_string(input, &["/content", "/text"]))
                    .unwrap_or_default();
                (
                    ActionType::FileEdit {
                        path: path.clone(),
                        changes: vec![FileChange::Write { content }],
                    },
                    path,
                )
            }
            "edit" | "hashline_edit" => {
                let path = self
                    .input
                    .as_ref()
                    .and_then(|input| {
                        extract_first_string(input, &["/path", "/file", "/file_path"])
                    })
                    .map(|path| make_relative_path(&path, worktree_path))
                    .unwrap_or_default();

                let diff = self
                    .input
                    .as_ref()
                    .and_then(|input| {
                        extract_first_string(input, &["/diff", "/patch", "/unified_diff"])
                    })
                    .or_else(|| {
                        self.output.as_ref().and_then(|output| {
                            extract_first_string(output, &["/diff", "/patch", "/unified_diff"])
                        })
                    });

                let changes = diff
                    .map(|diff| {
                        vec![FileChange::Edit {
                            unified_diff: diff,
                            has_line_numbers: false,
                        }]
                    })
                    .unwrap_or_default();

                (
                    ActionType::FileEdit {
                        path: path.clone(),
                        changes,
                    },
                    path,
                )
            }
            "bash" => {
                let command = self
                    .input
                    .as_ref()
                    .and_then(|input| extract_first_string(input, &["/command", "/cmd"]))
                    .unwrap_or_default();
                let output = self.output.as_ref().map(tool_output_text);
                let exit_code = self.output.as_ref().and_then(|value| {
                    extract_first_i32(
                        value,
                        &[
                            "/exit_code",
                            "/exitCode",
                            "/code",
                            "/details/exit_code",
                            "/details/exitCode",
                            "/details/code",
                        ],
                    )
                });

                (
                    ActionType::CommandRun {
                        command: command.clone(),
                        result: if output.is_none() && exit_code.is_none() {
                            None
                        } else {
                            Some(CommandRunResult {
                                exit_status: exit_code
                                    .map(|code| CommandExitStatus::ExitCode { code }),
                                output,
                            })
                        },
                    },
                    command,
                )
            }
            "grep" | "find" => {
                let query = self
                    .input
                    .as_ref()
                    .and_then(|input| {
                        extract_first_string(input, &["/query", "/pattern", "/path", "/glob"])
                    })
                    .unwrap_or_default();
                (
                    ActionType::Search {
                        query: query.clone(),
                    },
                    query,
                )
            }
            "ls" => {
                let path = self
                    .input
                    .as_ref()
                    .and_then(|input| extract_first_string(input, &["/path"]))
                    .map(|path| make_relative_path(&path, worktree_path))
                    .unwrap_or_else(|| ".".to_string());
                (ActionType::FileRead { path: path.clone() }, path)
            }
            _ => (
                ActionType::Tool {
                    tool_name: self.tool_name.clone(),
                    arguments: self.input.clone(),
                    result: self.output.as_ref().map(tool_result),
                },
                self.tool_name.clone(),
            ),
        }
    }
}

struct LogState {
    entry_index: EntryIndexProvider,
    msg_store: Arc<MsgStore>,
    assistant: Option<StreamingText>,
    thinking: Option<StreamingText>,
    tools: HashMap<String, ToolCallState>,
    debug_events: bool,
}

impl LogState {
    fn new(entry_index: EntryIndexProvider, msg_store: Arc<MsgStore>) -> Self {
        Self {
            entry_index,
            msg_store,
            assistant: None,
            thinking: None,
            tools: HashMap::new(),
            debug_events: is_debug_events_enabled(),
        }
    }

    fn handle_pi_event(&mut self, method: &str, payload: &Value, worktree_path: &Path) {
        match method {
            "message_update" => self.handle_message_update(payload),
            "tool_execution_start" => self.handle_tool_execution_start(payload, worktree_path),
            "tool_execution_update" => self.handle_tool_execution_update(payload, worktree_path),
            "tool_execution_end" => self.handle_tool_execution_end(payload, worktree_path),
            "agent_end" => {
                if let Some(error) = extract_agent_end_error(payload) {
                    self.add_error_entry(error);
                }
            }
            _ => {
                if self.debug_events {
                    self.add_system_entry(format!(
                        "[pi-debug] unhandled event method={method} payload={payload}"
                    ));
                }
            }
        }
    }

    fn handle_message_update(&mut self, payload: &Value) {
        let assistant_event = payload.pointer("/assistantMessageEvent").unwrap_or(payload);
        let event_type = extract_first_string(assistant_event, &["/type"])
            .unwrap_or_default()
            .to_ascii_lowercase();

        let assistant_set = if event_type.starts_with("text_") {
            extract_first_string(assistant_event, &["/content", "/text"])
        } else {
            extract_first_string(assistant_event, &["/text"])
        };
        if let Some(text) = assistant_set {
            self.update_streaming_text(
                UpdateMode::Set,
                &text,
                NormalizedEntryType::AssistantMessage,
            );
        }
        let assistant_delta = if event_type == "text_delta" {
            extract_first_string(assistant_event, &["/delta", "/text_delta", "/textDelta"])
        } else {
            extract_first_string(assistant_event, &["/text_delta", "/textDelta"])
        };
        if let Some(delta) = assistant_delta {
            self.update_streaming_text(
                UpdateMode::Append,
                &delta,
                NormalizedEntryType::AssistantMessage,
            );
        }

        let thinking_set = if event_type.starts_with("thinking_") {
            extract_first_string(assistant_event, &["/content", "/thinking"])
        } else {
            extract_first_string(assistant_event, &["/thinking"])
        };
        if let Some(text) = thinking_set {
            self.update_streaming_text(UpdateMode::Set, &text, NormalizedEntryType::Thinking);
        }
        let thinking_delta = if event_type == "thinking_delta" {
            extract_first_string(
                assistant_event,
                &["/delta", "/thinking_delta", "/thinkingDelta"],
            )
        } else {
            extract_first_string(assistant_event, &["/thinking_delta", "/thinkingDelta"])
        };
        if let Some(delta) = thinking_delta {
            self.update_streaming_text(UpdateMode::Append, &delta, NormalizedEntryType::Thinking);
        }
    }

    fn handle_tool_execution_update(&mut self, payload: &Value, worktree_path: &Path) {
        let Some(tool_call_id) = extract_first_string(
            payload,
            &["/tool_call_id", "/toolCallId", "/call_id", "/callId", "/id"],
        ) else {
            return;
        };

        let tool_name =
            extract_first_string(payload, &["/tool_name", "/toolName", "/name", "/tool"]);
        let input = extract_first_value(payload, &["/input", "/arguments", "/args", "/tool_input"]);
        let partial_output = extract_first_value(
            payload,
            &["/partial_result", "/partialResult", "/output", "/result"],
        );

        let tool_state = self.tools.entry(tool_call_id.clone()).or_insert_with(|| {
            ToolCallState::new(
                tool_call_id.clone(),
                tool_name.clone().unwrap_or_else(|| "tool".to_string()),
            )
        });

        if let Some(tool_name) = tool_name {
            tool_state.tool_name = tool_name;
        }
        if tool_state.input.is_none() {
            tool_state.input = input;
        }
        if partial_output.is_some() {
            tool_state.output = partial_output;
        }
        if !matches!(tool_state.status, ToolStatus::PendingApproval { .. }) {
            tool_state.status = ToolStatus::Created;
        }

        self.upsert_tool_entry(tool_call_id, worktree_path);
    }

    fn handle_tool_execution_start(&mut self, payload: &Value, worktree_path: &Path) {
        let tool_call_id = extract_first_string(
            payload,
            &["/tool_call_id", "/toolCallId", "/call_id", "/callId", "/id"],
        )
        .unwrap_or_else(|| format!("tool-{}", self.entry_index.current()));
        let tool_name =
            extract_first_string(payload, &["/tool_name", "/toolName", "/name", "/tool"]);
        let input = extract_first_value(payload, &["/input", "/arguments", "/args", "/tool_input"]);

        let tool_state = self.tools.entry(tool_call_id.clone()).or_insert_with(|| {
            ToolCallState::new(
                tool_call_id.clone(),
                tool_name.clone().unwrap_or_else(|| "tool".to_string()),
            )
        });

        if let Some(tool_name) = tool_name {
            tool_state.tool_name = tool_name;
        }
        if tool_state.input.is_none() {
            tool_state.input = input;
        }

        if !matches!(tool_state.status, ToolStatus::PendingApproval { .. }) {
            tool_state.status = ToolStatus::Created;
        }

        self.upsert_tool_entry(tool_call_id, worktree_path);
    }

    fn handle_tool_execution_end(&mut self, payload: &Value, worktree_path: &Path) {
        let Some(tool_call_id) = extract_first_string(
            payload,
            &["/tool_call_id", "/toolCallId", "/call_id", "/callId", "/id"],
        ) else {
            return;
        };

        let tool_name =
            extract_first_string(payload, &["/tool_name", "/toolName", "/name", "/tool"]);
        let output = extract_first_value(
            payload,
            &["/output", "/result", "/tool_output", "/response"],
        );
        let input = extract_first_value(payload, &["/input", "/arguments", "/args", "/tool_input"]);
        let status_string = extract_first_string(payload, &["/status", "/state"])
            .map(|status| status.to_lowercase())
            .unwrap_or_default();
        let has_error = payload
            .pointer("/error")
            .is_some_and(|value| !value.is_null())
            || extract_first_bool(payload, &["/isError", "/is_error", "/result/isError"])
                .unwrap_or(false)
            || status_string == "error"
            || status_string == "failed";

        let tool_state = self.tools.entry(tool_call_id.clone()).or_insert_with(|| {
            ToolCallState::new(
                tool_call_id.clone(),
                tool_name.clone().unwrap_or_else(|| "tool".to_string()),
            )
        });

        if let Some(tool_name) = tool_name {
            tool_state.tool_name = tool_name;
        }
        if tool_state.input.is_none() {
            tool_state.input = input;
        }
        if output.is_some() {
            tool_state.output = output;
        }

        if !matches!(
            tool_state.status,
            ToolStatus::Denied { .. } | ToolStatus::TimedOut
        ) {
            tool_state.status = if has_error {
                ToolStatus::Failed
            } else {
                ToolStatus::Success
            };
        }

        self.upsert_tool_entry(tool_call_id, worktree_path);
    }

    fn handle_approval_pending(
        &mut self,
        tool_call_id: String,
        tool_name: String,
        tool_input: Value,
        approval_id: String,
        requested_at: chrono::DateTime<chrono::Utc>,
        timeout_at: chrono::DateTime<chrono::Utc>,
        worktree_path: &Path,
    ) {
        let tool_state = self
            .tools
            .entry(tool_call_id.clone())
            .or_insert_with(|| ToolCallState::new(tool_call_id.clone(), tool_name.clone()));

        tool_state.tool_name = tool_name;
        if tool_state.input.is_none() {
            tool_state.input = Some(tool_input);
        }

        tool_state.status = ToolStatus::PendingApproval {
            approval_id,
            requested_at,
            timeout_at,
        };

        self.upsert_tool_entry(tool_call_id, worktree_path);
    }

    fn handle_approval_result(
        &mut self,
        tool_call_id: &str,
        status: ApprovalStatus,
        worktree_path: &Path,
    ) {
        let mut feedback_entry: Option<NormalizedEntry> = None;
        let tool_state = self
            .tools
            .entry(tool_call_id.to_string())
            .or_insert_with(|| ToolCallState::new(tool_call_id.to_string(), "tool".to_string()));

        if let Some(mapped_status) = ToolStatus::from_approval_status(&status) {
            tool_state.status = mapped_status;
        }

        let tool_name = tool_state.tool_name.clone();
        match status {
            ApprovalStatus::Denied { reason } => {
                feedback_entry = Some(NormalizedEntry {
                    timestamp: None,
                    entry_type: NormalizedEntryType::UserFeedback {
                        denied_tool: tool_name,
                    },
                    content: reason
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| "User denied this tool use request".to_string()),
                    metadata: None,
                });
            }
            ApprovalStatus::TimedOut => {
                feedback_entry = Some(NormalizedEntry {
                    timestamp: None,
                    entry_type: NormalizedEntryType::UserFeedback {
                        denied_tool: tool_name.clone(),
                    },
                    content: format!("Approval timed out for tool {}", tool_name),
                    metadata: None,
                });
            }
            ApprovalStatus::Approved
            | ApprovalStatus::Pending
            | ApprovalStatus::ProvidedInput { .. } => {}
        }

        if let Some(entry) = feedback_entry {
            self.add_normalized_entry(entry);
        }

        self.upsert_tool_entry(tool_call_id.to_string(), worktree_path);
    }

    fn update_streaming_text(
        &mut self,
        mode: UpdateMode,
        text: &str,
        entry_type: NormalizedEntryType,
    ) {
        if text.is_empty() {
            return;
        }

        let target = match entry_type {
            NormalizedEntryType::AssistantMessage => &mut self.assistant,
            NormalizedEntryType::Thinking => &mut self.thinking,
            _ => return,
        };

        let is_new = target.is_none();
        let state = target.get_or_insert_with(|| StreamingText {
            index: self.entry_index.next(),
            content: String::new(),
        });

        match mode {
            UpdateMode::Append => state.content.push_str(text),
            UpdateMode::Set => state.content = text.to_string(),
        }

        upsert_normalized_entry(
            &self.msg_store,
            state.index,
            NormalizedEntry {
                timestamp: None,
                entry_type,
                content: state.content.clone(),
                metadata: None,
            },
            is_new,
        );
    }

    fn upsert_tool_entry(&mut self, tool_call_id: String, worktree_path: &Path) {
        let Some(tool_state) = self.tools.get_mut(&tool_call_id) else {
            return;
        };

        let entry = tool_state.to_normalized_entry(worktree_path);
        if let Some(index) = tool_state.index {
            replace_normalized_entry(&self.msg_store, index, entry);
        } else {
            let index = add_normalized_entry(&self.msg_store, &self.entry_index, entry);
            tool_state.index = Some(index);
        }
    }

    fn add_error_entry(&self, message: String) {
        self.add_normalized_entry(NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ErrorMessage {
                error_type: NormalizedEntryError::Other,
            },
            content: message,
            metadata: None,
        });
    }

    fn add_system_entry(&self, content: String) {
        self.add_normalized_entry(NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::SystemMessage,
            content,
            metadata: None,
        });
    }

    fn add_model_metadata(&self, model: Option<String>, reasoning_effort: Option<String>) {
        let mut params = Vec::new();
        if let Some(model) = model {
            params.push(format!("model: {model}"));
        }
        if let Some(reasoning_effort) = reasoning_effort {
            params.push(format!("reasoning effort: {reasoning_effort}"));
        }
        if params.is_empty() {
            return;
        }

        self.add_system_entry(params.join("  "));
    }

    fn add_normalized_entry(&self, entry: NormalizedEntry) {
        add_normalized_entry(&self.msg_store, &self.entry_index, entry);
    }
}

pub fn normalize_logs(msg_store: Arc<MsgStore>, worktree_path: &Path) {
    let entry_index = EntryIndexProvider::start_from(&msg_store);
    normalize_stderr_logs(msg_store.clone(), entry_index.clone());

    let worktree_path = worktree_path.to_path_buf();
    tokio::spawn(async move {
        let mut session_stored = false;
        let mut state = LogState::new(entry_index, msg_store.clone());
        let mut stdout_lines = msg_store.stdout_lines_stream();

        while let Some(Ok(line)) = stdout_lines.next().await {
            let Some(event) = parse_event(&line) else {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    state.add_system_entry(trimmed.to_string());
                }
                continue;
            };

            match event {
                PiExecutorEvent::ModelMetadata {
                    model,
                    reasoning_effort,
                } => {
                    state.add_model_metadata(model, reasoning_effort);
                }
                PiExecutorEvent::SessionStart { session_id } => {
                    if !session_stored {
                        msg_store.push_session_id(session_id);
                        session_stored = true;
                    }
                }
                PiExecutorEvent::PiEvent {
                    method, payload, ..
                } => {
                    state.handle_pi_event(&method, &payload, &worktree_path);
                }
                PiExecutorEvent::ApprovalPending {
                    tool_call_id,
                    tool_name,
                    tool_input,
                    approval_id,
                    requested_at,
                    timeout_at,
                } => {
                    state.handle_approval_pending(
                        tool_call_id,
                        tool_name,
                        tool_input,
                        approval_id,
                        requested_at,
                        timeout_at,
                        &worktree_path,
                    );
                }
                PiExecutorEvent::ApprovalResult {
                    tool_call_id,
                    status,
                } => {
                    state.handle_approval_result(&tool_call_id, status, &worktree_path);
                }
                PiExecutorEvent::ProtocolError { message } => {
                    state.add_error_entry(message);
                }
                PiExecutorEvent::Done => {}
            }
        }
    });
}

fn parse_event(line: &str) -> Option<PiExecutorEvent> {
    serde_json::from_str::<PiExecutorEvent>(line.trim()).ok()
}

fn extract_first_string(value: &Value, pointers: &[&str]) -> Option<String> {
    pointers.iter().find_map(|pointer| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn extract_first_i32(value: &Value, pointers: &[&str]) -> Option<i32> {
    pointers.iter().find_map(|pointer| {
        value
            .pointer(pointer)
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
    })
}

fn extract_first_bool(value: &Value, pointers: &[&str]) -> Option<bool> {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_bool))
}

fn extract_first_value(value: &Value, pointers: &[&str]) -> Option<Value> {
    pointers.iter().find_map(|pointer| {
        value
            .pointer(pointer)
            .filter(|value| !value.is_null())
            .cloned()
    })
}

fn stringify_value(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

fn tool_output_text(value: &Value) -> String {
    structured_content_text(value).unwrap_or_else(|| stringify_value(value))
}

fn tool_result(value: &Value) -> ToolResult {
    if let Some(text) = structured_content_text(value) {
        ToolResult::markdown(text)
    } else if value.is_string() {
        ToolResult::markdown(stringify_value(value))
    } else {
        ToolResult::json(value.clone())
    }
}

fn structured_content_text(value: &Value) -> Option<String> {
    let content = value
        .pointer("/content")
        .or_else(|| value.pointer("/result/content"))?;

    match content {
        Value::String(value) => Some(value.clone()).filter(|value| !value.trim().is_empty()),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(content_item_text)
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        }
        Value::Object(_) => content_item_text(content),
        _ => None,
    }
}

fn content_item_text(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Object(_) => extract_first_string(value, &["/text", "/content", "/value"]),
        _ => None,
    }
    .filter(|value| !value.trim().is_empty())
}

fn make_relative_path(path: &str, worktree_path: &Path) -> String {
    make_path_relative(path, &worktree_path.to_string_lossy())
}

fn is_debug_events_enabled() -> bool {
    env::var("VK_PI_DEBUG_EVENTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use serde_json::json;
    use workspace_utils::{log_msg::LogMsg, msg_store::MsgStore};

    use super::*;
    use crate::logs::{
        NormalizedEntry, NormalizedEntryType, utils::patch::extract_normalized_entry_from_patch,
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

    #[test]
    fn assistant_text_streaming_appends_text_delta() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state.handle_pi_event(
            "message_update",
            &json!({
                "assistantMessageEvent": {
                    "text_delta": "Hel"
                }
            }),
            worktree_path,
        );
        state.handle_pi_event(
            "message_update",
            &json!({
                "assistantMessageEvent": {
                    "text_delta": "lo"
                }
            }),
            worktree_path,
        );

        let entries = collect_entries(&msg_store);
        let assistant = entries
            .iter()
            .find(|entry| matches!(entry.entry_type, NormalizedEntryType::AssistantMessage))
            .expect("assistant entry should exist");
        assert_eq!(assistant.content, "Hello");
    }

    #[test]
    fn assistant_text_streaming_from_agent_event_delta() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state.handle_pi_event(
            "message_update",
            &json!({
                "assistantMessageEvent": {
                    "type": "text_delta",
                    "delta": "Hel"
                }
            }),
            worktree_path,
        );
        state.handle_pi_event(
            "message_update",
            &json!({
                "assistantMessageEvent": {
                    "type": "text_delta",
                    "delta": "lo"
                }
            }),
            worktree_path,
        );

        let entries = collect_entries(&msg_store);
        let assistant = entries
            .iter()
            .find(|entry| matches!(entry.entry_type, NormalizedEntryType::AssistantMessage))
            .expect("assistant entry should exist");
        assert_eq!(assistant.content, "Hello");
    }

    #[test]
    fn tool_start_and_end_transition_to_success() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state.handle_pi_event(
            "tool_execution_start",
            &json!({
                "id": "tool-1",
                "tool_name": "bash",
                "input": { "command": "echo hello" }
            }),
            worktree_path,
        );
        state.handle_pi_event(
            "tool_execution_end",
            &json!({
                "id": "tool-1",
                "status": "success",
                "output": "hello"
            }),
            worktree_path,
        );

        let entries = collect_entries(&msg_store);
        let tool_entry = entries
            .iter()
            .find(|entry| matches!(entry.entry_type, NormalizedEntryType::ToolUse { .. }))
            .expect("tool entry should exist");

        let NormalizedEntryType::ToolUse {
            action_type,
            status,
            ..
        } = &tool_entry.entry_type
        else {
            unreachable!();
        };

        assert!(matches!(status, ToolStatus::Success));
        assert!(matches!(action_type, ActionType::CommandRun { .. }));
    }

    #[test]
    fn tool_execution_update_sets_partial_output() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state.handle_pi_event(
            "tool_execution_start",
            &json!({
                "toolCallId": "tool-1",
                "toolName": "bash",
                "args": { "command": "echo hello" }
            }),
            worktree_path,
        );
        state.handle_pi_event(
            "tool_execution_update",
            &json!({
                "toolCallId": "tool-1",
                "toolName": "bash",
                "partialResult": "hel"
            }),
            worktree_path,
        );

        let entries = collect_entries(&msg_store);
        let tool_entry = entries
            .iter()
            .find(|entry| matches!(entry.entry_type, NormalizedEntryType::ToolUse { .. }))
            .expect("tool entry should exist");

        let NormalizedEntryType::ToolUse { action_type, .. } = &tool_entry.entry_type else {
            unreachable!();
        };
        let ActionType::CommandRun { result, .. } = action_type else {
            panic!("expected command run action");
        };
        let output = result
            .as_ref()
            .and_then(|result| result.output.clone())
            .expect("tool output should be set");
        assert_eq!(output, "hel");
    }

    #[test]
    fn tool_execution_end_uses_is_error_flag() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state.handle_pi_event(
            "tool_execution_end",
            &json!({
                "id": "tool-1",
                "tool_name": "bash",
                "isError": true,
                "result": {
                    "content": [{ "type": "text", "text": "command failed" }],
                    "details": { "exitCode": 2 }
                }
            }),
            worktree_path,
        );

        let entries = collect_entries(&msg_store);
        let tool_entry = entries
            .iter()
            .find(|entry| matches!(entry.entry_type, NormalizedEntryType::ToolUse { .. }))
            .expect("tool entry should exist");

        let NormalizedEntryType::ToolUse {
            action_type,
            status,
            ..
        } = &tool_entry.entry_type
        else {
            unreachable!();
        };

        assert!(matches!(status, ToolStatus::Failed));
        let ActionType::CommandRun { result, .. } = action_type else {
            panic!("expected command run action");
        };
        let result = result.as_ref().expect("command result should be set");
        assert_eq!(result.output.as_deref(), Some("command failed"));
        assert!(matches!(
            result.exit_status,
            Some(CommandExitStatus::ExitCode { code: 2 })
        ));
    }

    #[test]
    fn generic_tool_result_prefers_structured_content_text() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");

        state.handle_pi_event(
            "tool_execution_end",
            &json!({
                "id": "tool-1",
                "tool_name": "fetch",
                "result": {
                    "content": [
                        { "type": "text", "text": "first line" },
                        { "type": "text", "text": "second line" }
                    ],
                    "details": { "url": "https://example.com" }
                }
            }),
            worktree_path,
        );

        let entries = collect_entries(&msg_store);
        let tool_entry = entries
            .iter()
            .find(|entry| matches!(entry.entry_type, NormalizedEntryType::ToolUse { .. }))
            .expect("tool entry should exist");

        let NormalizedEntryType::ToolUse { action_type, .. } = &tool_entry.entry_type else {
            unreachable!();
        };
        let ActionType::Tool { result, .. } = action_type else {
            panic!("expected generic tool action");
        };
        let result = result.as_ref().expect("tool result should be set");
        assert!(matches!(
            result.r#type,
            crate::logs::ToolResultValueType::Markdown
        ));
        assert_eq!(result.value.as_str(), Some("first line\nsecond line"));
    }

    #[test]
    fn approval_denied_and_timed_out_emit_feedback() {
        let (mut state, msg_store) = new_state();
        let worktree_path = Path::new("/tmp");
        let now = Utc::now();

        state.handle_approval_pending(
            "tool-deny".to_string(),
            "bash".to_string(),
            json!({"command":"rm -rf /"}),
            "approval-1".to_string(),
            now,
            now,
            worktree_path,
        );
        state.handle_approval_result(
            "tool-deny",
            ApprovalStatus::Denied {
                reason: Some("Denied by user".to_string()),
            },
            worktree_path,
        );

        state.handle_approval_pending(
            "tool-timeout".to_string(),
            "edit".to_string(),
            json!({"path":"file.txt"}),
            "approval-2".to_string(),
            now,
            now,
            worktree_path,
        );
        state.handle_approval_result("tool-timeout", ApprovalStatus::TimedOut, worktree_path);

        let entries = collect_entries(&msg_store);

        assert!(entries.iter().any(|entry| {
            matches!(
                entry.entry_type,
                NormalizedEntryType::UserFeedback { ref denied_tool }
                if denied_tool == "bash"
            ) && entry.content == "Denied by user"
        }));

        assert!(entries.iter().any(|entry| {
            matches!(
                entry.entry_type,
                NormalizedEntryType::UserFeedback { ref denied_tool }
                if denied_tool == "edit"
            ) && entry.content.contains("Approval timed out")
        }));
    }

    #[test]
    fn model_metadata_emits_system_entry() {
        let (state, msg_store) = new_state();

        state.add_model_metadata(Some("gpt-5.3-codex".to_string()), Some("xhigh".to_string()));

        let entries = collect_entries(&msg_store);
        let model_entry = entries
            .iter()
            .find(|entry| matches!(entry.entry_type, NormalizedEntryType::SystemMessage))
            .expect("model metadata entry should exist");
        assert_eq!(
            model_entry.content,
            "model: gpt-5.3-codex  reasoning effort: xhigh"
        );
    }
}
