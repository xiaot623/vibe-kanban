use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

use agent_client_protocol::{self as acp, SessionNotification};
use futures::StreamExt;
use regex::Regex;
use serde::Deserialize;
use workspace_utils::{approvals::ApprovalStatus, msg_store::MsgStore};

pub use super::AcpAgentHarness;
use super::AcpEvent;
use crate::{
    approvals::ToolCallMetadata,
    logs::{
        ActionType, FileChange, NormalizedEntry, NormalizedEntryError, NormalizedEntryType,
        TodoItem, ToolResult, ToolResultValueType, ToolStatus as LogToolStatus,
        stderr_processor::normalize_stderr_logs,
        utils::{ConversationPatch, EntryIndexProvider},
    },
};

const EXIT_PLAN_MODE_NAME: &str = "ExitPlanMode";
const GEMINI_EXIT_PLAN_TOOL_CALL_ID_PREFIX: &str = "exit_plan_mode-";
const GEMINI_PLAN_APPROVAL_TITLE_PREFIX: &str = "Requesting plan approval for:";
const OPENCODE_PLAN_EXIT_TOOL_CALL_ID_PREFIX: &str = "plan_exit-";
const OPENCODE_PLAN_EXIT_QUESTION_PREFIX: &str = "Plan at ";
const OPENCODE_PLAN_PATH_SEGMENT: &str = ".opencode/plans/";

pub fn normalize_logs(msg_store: Arc<MsgStore>, worktree_path: &Path) {
    // stderr normalization
    let entry_index = EntryIndexProvider::start_from(&msg_store);
    normalize_stderr_logs(msg_store.clone(), entry_index.clone());

    // stdout normalization (main loop)
    let worktree_path = worktree_path.to_path_buf();
    // Type aliases to simplify complex state types and appease clippy
    tokio::spawn(async move {
        type ToolStates = std::collections::HashMap<String, PartialToolCallData>;

        let mut stored_session_id = false;
        let mut streaming: StreamingState = StreamingState::default();
        let mut tool_states: ToolStates = HashMap::new();

        let mut stdout_lines = msg_store.stdout_lines_stream();
        while let Some(Ok(line)) = stdout_lines.next().await {
            if let Some(parsed) = AcpEventParser::parse_line(&line) {
                tracing::trace!("Parsed ACP line: {:?}", parsed);
                match parsed {
                    AcpEvent::SessionStart(id) => {
                        if !stored_session_id {
                            msg_store.push_session_id(id);
                            stored_session_id = true;
                        }
                    }
                    AcpEvent::ModelInfo(model) => {
                        let idx = entry_index.next();
                        let entry = NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::SystemMessage,
                            content: format!("model: {model}"),
                            metadata: None,
                        };
                        msg_store.push_patch(ConversationPatch::add_normalized_entry(idx, entry));
                    }
                    AcpEvent::Error(msg) => {
                        let idx = entry_index.next();
                        let entry = NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::ErrorMessage {
                                error_type: NormalizedEntryError::Other,
                            },
                            content: msg,
                            metadata: None,
                        };
                        msg_store.push_patch(ConversationPatch::add_normalized_entry(idx, entry));
                    }
                    AcpEvent::Done(_) => {
                        streaming.assistant_text = None;
                        streaming.thinking_text = None;
                    }
                    AcpEvent::Message(content) => {
                        streaming.thinking_text = None;
                        if let agent_client_protocol::ContentBlock::Text(text) = content {
                            let is_new = streaming.assistant_text.is_none();
                            if is_new {
                                if text.text == "\n" {
                                    continue;
                                }
                                let idx = entry_index.next();
                                streaming.assistant_text = Some(StreamingText {
                                    index: idx,
                                    content: String::new(),
                                });
                            }
                            if let Some(ref mut s) = streaming.assistant_text {
                                s.content.push_str(&text.text);
                                let entry = NormalizedEntry {
                                    timestamp: None,
                                    entry_type: NormalizedEntryType::AssistantMessage,
                                    content: s.content.clone(),
                                    metadata: None,
                                };
                                let patch = if is_new {
                                    ConversationPatch::add_normalized_entry(s.index, entry)
                                } else {
                                    ConversationPatch::replace(s.index, entry)
                                };
                                msg_store.push_patch(patch);
                            }
                        }
                    }
                    AcpEvent::Thought(content) => {
                        streaming.assistant_text = None;
                        if let agent_client_protocol::ContentBlock::Text(text) = content {
                            let is_new = streaming.thinking_text.is_none();
                            if is_new {
                                let idx = entry_index.next();
                                streaming.thinking_text = Some(StreamingText {
                                    index: idx,
                                    content: String::new(),
                                });
                            }
                            if let Some(ref mut s) = streaming.thinking_text {
                                s.content.push_str(&text.text);
                                let entry = NormalizedEntry {
                                    timestamp: None,
                                    entry_type: NormalizedEntryType::Thinking,
                                    content: s.content.clone(),
                                    metadata: None,
                                };
                                let patch = if is_new {
                                    ConversationPatch::add_normalized_entry(s.index, entry)
                                } else {
                                    ConversationPatch::replace(s.index, entry)
                                };
                                msg_store.push_patch(patch);
                            }
                        }
                    }
                    AcpEvent::Plan(plan) => {
                        streaming.assistant_text = None;
                        streaming.thinking_text = None;
                        let todos: Vec<TodoItem> = plan
                            .entries
                            .iter()
                            .map(|e| TodoItem {
                                content: e.content.clone(),
                                status: serde_json::to_value(&e.status)
                                    .ok()
                                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                                    .unwrap_or_else(|| "unknown".to_string()),
                                priority: serde_json::to_value(&e.priority)
                                    .ok()
                                    .and_then(|v| v.as_str().map(|s| s.to_string())),
                            })
                            .collect();

                        let idx = entry_index.next();
                        let entry = NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::ToolUse {
                                tool_name: "plan".to_string(),
                                action_type: ActionType::TodoManagement {
                                    todos,
                                    operation: "update".to_string(),
                                },
                                status: LogToolStatus::Success,
                            },
                            content: "Plan updated".to_string(),
                            metadata: None,
                        };
                        msg_store.push_patch(ConversationPatch::add_normalized_entry(idx, entry));
                    }
                    AcpEvent::AvailableCommands(cmds) => {
                        let mut body = String::from("Available commands:\n");
                        for c in &cmds {
                            body.push_str(&format!("- {}\n", c.name));
                        }
                        let idx = entry_index.next();
                        let entry = NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::SystemMessage,
                            content: body,
                            metadata: None,
                        };
                        msg_store.push_patch(ConversationPatch::add_normalized_entry(idx, entry));
                    }
                    AcpEvent::CurrentMode(mode_id) => {
                        let idx = entry_index.next();
                        let entry = NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::SystemMessage,
                            content: format!("Current mode: {}", mode_id.0),
                            metadata: None,
                        };
                        msg_store.push_patch(ConversationPatch::add_normalized_entry(idx, entry));
                    }
                    AcpEvent::Usage(update) => {
                        let idx = entry_index.next();
                        let entry = NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::TokenUsageInfo(
                                crate::logs::TokenUsageInfo {
                                    total_tokens: update.used.min(u32::MAX as u64) as u32,
                                    model_context_window: update.size.min(u32::MAX as u64) as u32,
                                },
                            ),
                            content: format!(
                                "Tokens used: {} / Context window: {}",
                                update.used, update.size
                            ),
                            metadata: None,
                        };
                        msg_store.push_patch(ConversationPatch::add_normalized_entry(idx, entry));
                    }
                    AcpEvent::RequestPermission(perm) => {
                        if let Ok(tc) = agent_client_protocol::ToolCall::try_from(perm.tool_call) {
                            handle_tool_call(
                                &tc,
                                &worktree_path,
                                &mut streaming,
                                &mut tool_states,
                                &entry_index,
                                &msg_store,
                            );
                        }
                    }
                    AcpEvent::ToolCall(tc) => handle_tool_call(
                        &tc,
                        &worktree_path,
                        &mut streaming,
                        &mut tool_states,
                        &entry_index,
                        &msg_store,
                    ),
                    AcpEvent::ToolUpdate(update) => {
                        let mut update = update;
                        if update.fields.title.is_none() {
                            update.fields.title = tool_states
                                .get(&update.tool_call_id.0.to_string())
                                .map(|s| s.title.clone())
                                .or_else(|| Some("".to_string()));
                        }
                        tracing::trace!("Got tool call update: {:?}", update);
                        if let Ok(tc) = agent_client_protocol::ToolCall::try_from(update.clone()) {
                            handle_tool_call(
                                &tc,
                                &worktree_path,
                                &mut streaming,
                                &mut tool_states,
                                &entry_index,
                                &msg_store,
                            );
                        } else {
                            tracing::debug!("Failed to convert tool call update to ToolCall");
                        }
                    }
                    AcpEvent::ApprovalResponse(resp) => {
                        tracing::trace!("Received approval response: {:?}", resp);
                        if let ApprovalStatus::Denied { reason } = resp.status {
                            let tool_name = tool_states
                                .get(&resp.tool_call_id)
                                .map(derive_tool_use_name)
                                .unwrap_or_default();
                            let idx = entry_index.next();
                            let entry = NormalizedEntry {
                                timestamp: None,
                                entry_type: NormalizedEntryType::UserFeedback {
                                    denied_tool: tool_name,
                                },
                                content: reason
                                    .clone()
                                    .unwrap_or_else(|| {
                                        "User denied this tool use request".to_string()
                                    })
                                    .trim()
                                    .to_string(),
                                metadata: None,
                            };
                            msg_store
                                .push_patch(ConversationPatch::add_normalized_entry(idx, entry));
                        }
                    }
                    AcpEvent::User(_) | AcpEvent::Other(_) => (),
                }
            }
        }

        fn handle_tool_call(
            tc: &agent_client_protocol::ToolCall,
            worktree_path: &Path,
            streaming: &mut StreamingState,
            tool_states: &mut ToolStates,
            entry_index: &EntryIndexProvider,
            msg_store: &Arc<MsgStore>,
        ) {
            streaming.assistant_text = None;
            streaming.thinking_text = None;
            let id = tc.tool_call_id.0.to_string();
            let is_new = !tool_states.contains_key(&id);
            let tool_data = tool_states.entry(id).or_default();
            tool_data.extend(tc, worktree_path);
            if is_new {
                tool_data.index = entry_index.next();
            }
            let action = map_to_action_type(tool_data, worktree_path);
            let entry = NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::ToolUse {
                    tool_name: derive_tool_use_name(tool_data),
                    action_type: action,
                    status: convert_tool_status(&tool_data.status),
                },
                content: get_tool_content(tool_data, worktree_path),
                metadata: serde_json::to_value(ToolCallMetadata {
                    tool_call_id: tool_data.id.0.to_string(),
                })
                .ok(),
            };
            let patch = if is_new {
                ConversationPatch::add_normalized_entry(tool_data.index, entry)
            } else {
                ConversationPatch::replace(tool_data.index, entry)
            };
            msg_store.push_patch(patch);
        }

        fn map_to_action_type(tc: &PartialToolCallData, worktree_path: &Path) -> ActionType {
            if is_exit_plan_tool_call(tc) {
                return ActionType::PlanPresentation {
                    plan: resolve_exit_plan_content(tc, worktree_path),
                };
            }

            match tc.kind {
                agent_client_protocol::ToolKind::Read => {
                    // Special-case: read_many_files style titles parsed via helper
                    if tc.id.0.starts_with("read_many_files") {
                        let result = collect_text_content(&tc.content).map(|text| ToolResult {
                            r#type: ToolResultValueType::Markdown,
                            value: serde_json::Value::String(text),
                        });
                        return ActionType::Tool {
                            tool_name: "read_many_files".to_string(),
                            arguments: Some(serde_json::Value::String(tc.title.clone())),
                            result,
                        };
                    }
                    ActionType::FileRead {
                        path: tc
                            .path
                            .clone()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string(),
                    }
                }
                agent_client_protocol::ToolKind::Edit => {
                    let changes = extract_file_changes(tc);
                    ActionType::FileEdit {
                        path: tc
                            .path
                            .clone()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string(),
                        changes,
                    }
                }
                agent_client_protocol::ToolKind::Execute => {
                    let command = AcpEventParser::parse_execute_command(tc);
                    // Prefer structured raw_output, else fallback to aggregated text content
                    let completed =
                        matches!(tc.status, agent_client_protocol::ToolCallStatus::Completed);
                    tracing::trace!(
                        "Mapping execute tool call, completed: {}, command: {}",
                        completed,
                        command
                    );
                    let tc_exit_status = match tc.status {
                        agent_client_protocol::ToolCallStatus::Completed => {
                            Some(crate::logs::CommandExitStatus::Success { success: true })
                        }
                        agent_client_protocol::ToolCallStatus::Failed => {
                            Some(crate::logs::CommandExitStatus::Success { success: false })
                        }
                        _ => None,
                    };

                    let result = if let Some(text) = collect_text_content(&tc.content) {
                        Some(crate::logs::CommandRunResult {
                            exit_status: tc_exit_status,
                            output: Some(text),
                        })
                    } else {
                        Some(crate::logs::CommandRunResult {
                            exit_status: tc_exit_status,
                            output: None,
                        })
                    };
                    ActionType::CommandRun { command, result }
                }
                agent_client_protocol::ToolKind::Delete => ActionType::FileEdit {
                    path: tc
                        .path
                        .clone()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                    changes: vec![FileChange::Delete],
                },
                agent_client_protocol::ToolKind::Search => {
                    let query = tc
                        .raw_input
                        .as_ref()
                        .and_then(|v| serde_json::from_value::<SearchArgs>(v.clone()).ok())
                        .map(|a| a.query)
                        .unwrap_or_else(|| tc.title.clone());
                    ActionType::Search { query }
                }
                agent_client_protocol::ToolKind::Fetch => {
                    let mut url = tc
                        .raw_input
                        .as_ref()
                        .and_then(|v| serde_json::from_value::<FetchArgs>(v.clone()).ok())
                        .map(|a| a.url)
                        .unwrap_or_default();
                    if url.is_empty() {
                        // Fallback: try to extract first URL from the title
                        if let Some(extracted) = extract_url_from_text(&tc.title) {
                            url = extracted;
                        }
                    }
                    ActionType::WebFetch { url }
                }
                agent_client_protocol::ToolKind::Think => {
                    let tool_name = extract_tool_name_from_id(tc.id.0.as_ref())
                        .unwrap_or_else(|| tc.title.clone());
                    // For think/save_memory, surface both title and aggregated text content as arguments
                    let text = collect_text_content(&tc.content);
                    let arguments = Some(match &text {
                        Some(t) => serde_json::json!({ "title": tc.title, "content": t }),
                        None => serde_json::json!({ "title": tc.title }),
                    });
                    let result = if let Some(output) = &tc.raw_output {
                        Some(ToolResult {
                            r#type: ToolResultValueType::Json,
                            value: output.clone(),
                        })
                    } else {
                        collect_text_content(&tc.content).map(|text| ToolResult {
                            r#type: ToolResultValueType::Markdown,
                            value: serde_json::Value::String(text),
                        })
                    };
                    ActionType::Tool {
                        tool_name,
                        arguments,
                        result,
                    }
                }
                agent_client_protocol::ToolKind::SwitchMode => ActionType::Other {
                    description: "switch_mode".to_string(),
                },
                agent_client_protocol::ToolKind::Other
                | agent_client_protocol::ToolKind::Move
                | _ => {
                    // Derive a friendlier tool name from the id if it looks like name-<digits>
                    let tool_name = extract_tool_name_from_id(tc.id.0.as_ref())
                        .unwrap_or_else(|| tc.title.clone());

                    // Some tools embed JSON args into the title instead of raw_input
                    let arguments = if let Some(raw) = &tc.raw_input {
                        Some(raw.clone())
                    } else if tc.title.trim_start().starts_with('{') {
                        // Title contains JSON arguments for the tool
                        serde_json::from_str::<serde_json::Value>(&tc.title).ok()
                    } else {
                        None
                    };
                    // Extract result: prefer raw_output (structured), else text content as Markdown
                    let result = if let Some(output) = &tc.raw_output {
                        Some(ToolResult {
                            r#type: ToolResultValueType::Json,
                            value: output.clone(),
                        })
                    } else {
                        collect_text_content(&tc.content).map(|text| ToolResult {
                            r#type: ToolResultValueType::Markdown,
                            value: serde_json::Value::String(text),
                        })
                    };
                    ActionType::Tool {
                        tool_name,
                        arguments,
                        result,
                    }
                }
            }
        }

        fn extract_file_changes(tc: &PartialToolCallData) -> Vec<FileChange> {
            let mut changes = Vec::new();
            for c in &tc.content {
                if let agent_client_protocol::ToolCallContent::Diff(diff) = c {
                    let path = diff.path.to_string_lossy().to_string();
                    let rel = if !path.is_empty() {
                        path
                    } else {
                        tc.path
                            .clone()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string()
                    };
                    let old_text = diff.old_text.as_deref().unwrap_or("");
                    if old_text.is_empty() {
                        changes.push(FileChange::Write {
                            content: diff.new_text.clone(),
                        });
                    } else {
                        let unified = workspace_utils::diff::create_unified_diff(
                            &rel,
                            old_text,
                            &diff.new_text,
                        );
                        changes.push(FileChange::Edit {
                            unified_diff: unified,
                            has_line_numbers: false,
                        });
                    }
                }
            }
            if changes.is_empty()
                && let Some(raw) = &tc.raw_input
                && let Ok(edit_input) = serde_json::from_value::<EditInput>(raw.clone())
            {
                if let Some(diff) = edit_input.diff {
                    changes.push(FileChange::Edit {
                        unified_diff: workspace_utils::diff::normalize_unified_diff(
                            &edit_input.file_path,
                            &diff,
                        ),
                        has_line_numbers: true,
                    });
                } else if let Some(old) = edit_input.old_string
                    && let Some(new) = edit_input.new_string
                {
                    changes.push(FileChange::Edit {
                        unified_diff: workspace_utils::diff::create_unified_diff(
                            &edit_input.file_path,
                            &old,
                            &new,
                        ),
                        has_line_numbers: false,
                    });
                }
            }
            changes
        }

        fn get_tool_content(tc: &PartialToolCallData, worktree_path: &Path) -> String {
            if is_exit_plan_tool_call(tc) {
                return resolve_exit_plan_content(tc, worktree_path);
            }

            match tc.kind {
                agent_client_protocol::ToolKind::Execute => {
                    AcpEventParser::parse_execute_command(tc)
                }
                agent_client_protocol::ToolKind::Think => "Saving memory".to_string(),
                agent_client_protocol::ToolKind::Other => {
                    let tool_name = extract_tool_name_from_id(tc.id.0.as_ref())
                        .unwrap_or_else(|| "tool".to_string());
                    if tc.title.is_empty() {
                        tool_name
                    } else {
                        format!("{}: {}", tool_name, tc.title)
                    }
                }
                agent_client_protocol::ToolKind::Read => {
                    if tc.id.0.starts_with("read_many_files") {
                        "Read files".to_string()
                    } else {
                        tc.path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| tc.title.clone())
                    }
                }
                _ => tc.title.clone(),
            }
        }

        fn extract_tool_name_from_id(id: &str) -> Option<String> {
            if let Some(idx) = id.rfind('-') {
                let (head, tail) = id.split_at(idx);
                if tail
                    .trim_start_matches('-')
                    .chars()
                    .all(|c| c.is_ascii_digit())
                {
                    return Some(head.to_string());
                }
            }
            None
        }

        fn extract_url_from_text(text: &str) -> Option<String> {
            // Simple URL extractor
            static URL_RE: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r#"https?://[^\s"')]+"#).expect("valid regex"));
            URL_RE.find(text).map(|m| m.as_str().to_string())
        }

        fn collect_text_content(
            content: &[agent_client_protocol::ToolCallContent],
        ) -> Option<String> {
            let mut out = String::new();
            for c in content {
                if let agent_client_protocol::ToolCallContent::Content(inner) = c
                    && let agent_client_protocol::ContentBlock::Text(t) = &inner.content
                {
                    out.push_str(&t.text);
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
            if out.is_empty() { None } else { Some(out) }
        }

        fn convert_tool_status(status: &agent_client_protocol::ToolCallStatus) -> LogToolStatus {
            match status {
                agent_client_protocol::ToolCallStatus::Pending
                | agent_client_protocol::ToolCallStatus::InProgress => LogToolStatus::Created,
                agent_client_protocol::ToolCallStatus::Completed => LogToolStatus::Success,
                agent_client_protocol::ToolCallStatus::Failed => LogToolStatus::Failed,
                _ => {
                    tracing::debug!("Unknown tool call status: {:?}", status);
                    LogToolStatus::Created
                }
            }
        }
    });
}

fn derive_tool_use_name(tc: &PartialToolCallData) -> String {
    if is_exit_plan_tool_call(tc) {
        EXIT_PLAN_MODE_NAME.to_string()
    } else {
        tc.title.clone()
    }
}

fn resolve_exit_plan_content(tc: &PartialToolCallData, worktree_path: &Path) -> String {
    if is_gemini_exit_plan_tool_call(tc.id.0.as_ref()) {
        return read_plan_from_title(&tc.title, worktree_path).unwrap_or_else(|| tc.title.clone());
    }

    parse_plan_path_from_title(&tc.title)
        .or_else(|| tc.raw_input.as_ref().and_then(parse_plan_path_from_value))
        .and_then(|path| read_plan_from_path(&path, worktree_path))
        .or_else(|| extract_plan_review_text(tc.raw_input.as_ref()))
        .or_else(|| collect_text_content_from_tool_call(tc))
        .unwrap_or_else(|| tc.title.clone())
}

fn collect_text_content_from_tool_call(tc: &PartialToolCallData) -> Option<String> {
    let mut out = String::new();
    for content in &tc.content {
        if let agent_client_protocol::ToolCallContent::Content(inner) = content
            && let agent_client_protocol::ContentBlock::Text(text) = &inner.content
        {
            out.push_str(&text.text);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn is_gemini_exit_plan_tool_call(tool_call_id: &str) -> bool {
    tool_call_id.starts_with(GEMINI_EXIT_PLAN_TOOL_CALL_ID_PREFIX)
}

fn is_opencode_plan_exit_tool_call(tool_call_id: &str) -> bool {
    tool_call_id.starts_with(OPENCODE_PLAN_EXIT_TOOL_CALL_ID_PREFIX)
}

fn strip_prefix_ignore_ascii_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|candidate| candidate.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

fn trim_opencode_markdown_wrappers(text: &str) -> &str {
    text.trim_start_matches(|c| {
        matches!(
            c,
            '"' | '\'' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '<' | '>'
        )
    })
    .trim_end_matches(|c| {
        matches!(
            c,
            '"' | '\'' | '`' | '.' | '*' | '_' | '[' | ']' | '(' | ')' | '<' | '>'
        )
    })
}

fn parse_embedded_opencode_plan_path(text: &str) -> Option<String> {
    let start = text.find(OPENCODE_PLAN_PATH_SEGMENT)?;
    let suffix = &text[start..];
    let end = suffix
        .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '}' | ',' | ';'))
        .unwrap_or(suffix.len());
    let candidate = trim_opencode_markdown_wrappers(&suffix[..end]);

    if candidate.is_empty() {
        None
    } else {
        Some(candidate.to_string())
    }
}

fn find_matching_string_in_value(
    value: &serde_json::Value,
    predicate: &impl Fn(&str) -> bool,
) -> Option<String> {
    match value {
        serde_json::Value::String(text) if predicate(text) => Some(text.clone()),
        serde_json::Value::Array(items) => items
            .iter()
            .find_map(|item| find_matching_string_in_value(item, predicate)),
        serde_json::Value::Object(map) => map
            .values()
            .find_map(|item| find_matching_string_in_value(item, predicate)),
        _ => None,
    }
}

fn is_exit_plan_tool_call(tc: &PartialToolCallData) -> bool {
    is_gemini_exit_plan_tool_call(tc.id.0.as_ref())
        || is_opencode_plan_exit_tool_call(tc.id.0.as_ref())
}

fn parse_plan_path_from_title(title: &str) -> Option<String> {
    // Gemini: "Requesting plan approval for: <path>"
    if let Some(raw_path) = strip_prefix_ignore_ascii_case(title, GEMINI_PLAN_APPROVAL_TITLE_PREFIX)
    {
        let raw_path = raw_path.trim();
        if raw_path.is_empty() {
            return None;
        }
        let normalized = raw_path.trim_matches(|c| matches!(c, '"' | '\'' | '`'));
        return if normalized.is_empty() {
            None
        } else {
            Some(normalized.to_string())
        };
    }

    // OpenCode: "Plan at <path> is complete. Would you like to switch to the build agent..."
    if let Some(rest) = strip_prefix_ignore_ascii_case(title, OPENCODE_PLAN_EXIT_QUESTION_PREFIX) {
        if let Some(end) = rest.to_ascii_lowercase().find(" is complete") {
            let raw_path = rest[..end].trim();
            if !raw_path.is_empty() {
                let normalized = raw_path.trim_matches(|c| matches!(c, '"' | '\'' | '`'));
                if !normalized.is_empty() {
                    return Some(normalized.to_string());
                }
            }
        }
    }

    parse_embedded_opencode_plan_path(title)
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
    read_plan_from_path(&plan_path, worktree_path)
}

fn parse_plan_path_from_value(value: &serde_json::Value) -> Option<String> {
    find_matching_string_in_value(value, &|text| parse_plan_path_from_title(text).is_some())
        .and_then(|text| parse_plan_path_from_title(&text))
        .or_else(|| {
            find_matching_string_in_value(value, &|text| {
                parse_embedded_opencode_plan_path(text).is_some()
            })
            .and_then(|text| parse_embedded_opencode_plan_path(&text))
        })
}

fn read_plan_from_path(plan_path: &str, worktree_path: &Path) -> Option<String> {
    let resolved_path = resolve_plan_path(&plan_path, worktree_path);

    match std::fs::read_to_string(&resolved_path) {
        Ok(plan) => Some(plan),
        Err(err) => {
            tracing::debug!(
                "Failed to read ACP plan file '{}': {}",
                resolved_path.display(),
                err
            );
            None
        }
    }
}

fn extract_plan_review_text(raw_input: Option<&serde_json::Value>) -> Option<String> {
    raw_input
        .and_then(|value| find_matching_string_in_value(value, &|text| !text.trim().is_empty()))
}

struct PartialToolCallData {
    index: usize,
    id: agent_client_protocol::ToolCallId,
    kind: agent_client_protocol::ToolKind,
    title: String,
    status: agent_client_protocol::ToolCallStatus,
    path: Option<PathBuf>,
    content: Vec<agent_client_protocol::ToolCallContent>,
    raw_input: Option<serde_json::Value>,
    raw_output: Option<serde_json::Value>,
}

impl PartialToolCallData {
    fn extend(&mut self, tc: &agent_client_protocol::ToolCall, worktree_path: &Path) {
        self.id = tc.tool_call_id.clone();
        if tc.kind != Default::default() {
            self.kind = tc.kind;
        }
        if !tc.title.is_empty() {
            self.title = tc.title.clone();
        }
        if tc.status != Default::default() {
            self.status = tc.status;
        }
        if !tc.locations.is_empty() {
            self.path = tc.locations.first().map(|l| {
                PathBuf::from(workspace_utils::path::make_path_relative(
                    &l.path.to_string_lossy(),
                    &worktree_path.to_string_lossy(),
                ))
            });
        }
        if !tc.content.is_empty() {
            self.content = tc.content.clone();
        }
        if tc.raw_input.is_some() {
            self.raw_input = tc.raw_input.clone();
        }
        if tc.raw_output.is_some() {
            self.raw_output = tc.raw_output.clone();
        }
    }
}

impl Default for PartialToolCallData {
    fn default() -> Self {
        Self {
            id: agent_client_protocol::ToolCallId::new(""),
            index: 0,
            kind: agent_client_protocol::ToolKind::default(),
            title: String::new(),
            status: Default::default(),
            path: None,
            content: Vec::new(),
            raw_input: None,
            raw_output: None,
        }
    }
}

struct AcpEventParser;

impl AcpEventParser {
    /// Parse a line that may contain an ACP event
    pub fn parse_line(line: &str) -> Option<AcpEvent> {
        let trimmed = line.trim();

        if let Ok(acp_event) = serde_json::from_str::<AcpEvent>(trimmed) {
            return Some(acp_event);
        }

        tracing::debug!("Failed to parse ACP raw log {trimmed}");

        None
    }

    /// Parse command from tool title (for execute tools)
    pub fn parse_execute_command(tc: &PartialToolCallData) -> String {
        if let Some(command) = tc.raw_input.as_ref().and_then(|value| {
            value
                .as_object()
                .and_then(|o| o.get("command").and_then(|v| v.as_str()))
        }) {
            return command.to_string();
        }
        let title = &tc.title;
        if let Some(command) = title.split(" [current working directory ").next() {
            command.trim().to_string()
        } else if let Some(command) = title.split(" (").next() {
            command.trim().to_string()
        } else {
            title.trim().to_string()
        }
    }
}

/// Result of parsing a line
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum ParsedLine {
    SessionId(String),
    Event(AcpEvent),
    Error(String),
    Done,
}

impl TryFrom<SessionNotification> for AcpEvent {
    type Error = ();

    fn try_from(notification: SessionNotification) -> Result<Self, ()> {
        let event = match notification.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => AcpEvent::Message(chunk.content),
            acp::SessionUpdate::AgentThoughtChunk(chunk) => AcpEvent::Thought(chunk.content),
            acp::SessionUpdate::ToolCall(tc) => AcpEvent::ToolCall(tc),
            acp::SessionUpdate::ToolCallUpdate(update) => AcpEvent::ToolUpdate(update),
            acp::SessionUpdate::Plan(plan) => AcpEvent::Plan(plan),
            acp::SessionUpdate::AvailableCommandsUpdate(update) => {
                AcpEvent::AvailableCommands(update.available_commands)
            }
            acp::SessionUpdate::CurrentModeUpdate(update) => {
                AcpEvent::CurrentMode(update.current_mode_id)
            }
            acp::SessionUpdate::UsageUpdate(update) => AcpEvent::Usage(update),
            _ => return Err(()),
        };
        Ok(event)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct SearchArgs {
    query: String,
}

#[derive(Debug, Clone, Deserialize)]
struct FetchArgs {
    url: String,
}

#[derive(Debug, Clone, Default)]
struct StreamingState {
    assistant_text: Option<StreamingText>,
    thinking_text: Option<StreamingText>,
}

#[derive(Debug, Clone)]
struct StreamingText {
    index: usize,
    content: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditInput {
    file_path: String,
    #[serde(default)]
    diff: Option<String>,
    #[serde(default)]
    old_string: Option<String>,
    #[serde(default)]
    new_string: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc, time::Duration};

    use workspace_utils::log_msg::LogMsg;

    use super::*;
    use crate::{
        approvals::ToolCallMetadata,
        logs::{
            ActionType, NormalizedEntry, NormalizedEntryType, ToolStatus,
            utils::patch::extract_normalized_entry_from_patch,
        },
    };

    fn create_temp_dir(test_name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vk-acp-normalize-{test_name}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("create temporary test directory");
        dir
    }

    fn latest_tool_use_entry(history: &[LogMsg]) -> Option<NormalizedEntry> {
        history.iter().rev().find_map(|msg| {
            if let LogMsg::JsonPatch(patch) = msg
                && let Some((_, entry)) = extract_normalized_entry_from_patch(patch)
                && matches!(entry.entry_type, NormalizedEntryType::ToolUse { .. })
            {
                return Some(entry);
            }
            None
        })
    }

    async fn wait_for_tool_use_entry(msg_store: &MsgStore) -> NormalizedEntry {
        for _ in 0..100 {
            if let Some(entry) = latest_tool_use_entry(&msg_store.get_history()) {
                return entry;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for normalized tool use entry");
    }

    async fn wait_for_token_usage_entry(msg_store: &MsgStore) -> NormalizedEntry {
        for _ in 0..100 {
            if let Some(entry) = msg_store.get_history().iter().rev().find_map(|msg| {
                if let LogMsg::JsonPatch(patch) = msg
                    && let Some((_, entry)) = extract_normalized_entry_from_patch(patch)
                    && matches!(entry.entry_type, NormalizedEntryType::TokenUsageInfo(_))
                {
                    return Some(entry);
                }
                None
            }) {
                return entry;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for normalized token usage entry");
    }

    async fn wait_for_system_message_entry(msg_store: &MsgStore) -> NormalizedEntry {
        for _ in 0..100 {
            if let Some(entry) = msg_store.get_history().iter().rev().find_map(|msg| {
                if let LogMsg::JsonPatch(patch) = msg
                    && let Some((_, entry)) = extract_normalized_entry_from_patch(patch)
                    && matches!(entry.entry_type, NormalizedEntryType::SystemMessage)
                {
                    return Some(entry);
                }
                None
            }) {
                return entry;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for normalized system message entry");
    }

    #[tokio::test]
    async fn exit_plan_mode_tool_call_normalizes_to_plan_presentation_entry() {
        let worktree = create_temp_dir("plan-presentation");
        let plan_text = "# Plan\n- step 1\n- step 2\n";
        let plan_path = worktree.join("plan.md");
        fs::write(&plan_path, plan_text).expect("write plan file");

        let msg_store = Arc::new(MsgStore::new());
        normalize_logs(msg_store.clone(), &worktree);

        let title = format!("Requesting plan approval for: {}", plan_path.display());
        let tool_call = agent_client_protocol::ToolCall::new("exit_plan_mode-7", title)
            .kind(agent_client_protocol::ToolKind::Other)
            .status(agent_client_protocol::ToolCallStatus::Pending);

        msg_store.push_stdout(format!(
            "{}\n",
            serde_json::to_string(&AcpEvent::ToolCall(tool_call))
                .expect("serialize tool call event")
        ));

        let entry = wait_for_tool_use_entry(msg_store.as_ref()).await;
        match entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                status,
            } => {
                assert_eq!(tool_name, EXIT_PLAN_MODE_NAME);
                assert!(matches!(status, ToolStatus::Created));
                match action_type {
                    ActionType::PlanPresentation { plan } => assert_eq!(plan, plan_text),
                    other => panic!("expected plan_presentation action type, got {other:?}"),
                }
            }
            other => panic!("expected tool_use entry, got {other:?}"),
        }
        assert_eq!(entry.content, plan_text);
        let metadata: ToolCallMetadata = serde_json::from_value(
            entry
                .metadata
                .expect("tool entry should include tool_call_id metadata"),
        )
        .expect("metadata should deserialize");
        assert_eq!(metadata.tool_call_id, "exit_plan_mode-7");
    }

    #[tokio::test]
    async fn exit_plan_mode_falls_back_to_title_when_plan_file_cannot_be_read() {
        let worktree = create_temp_dir("plan-fallback");
        let msg_store = Arc::new(MsgStore::new());
        normalize_logs(msg_store.clone(), &worktree);

        let title = "Requesting plan approval for: missing-plan.md".to_string();
        let tool_call = agent_client_protocol::ToolCall::new("exit_plan_mode-8", title.clone())
            .kind(agent_client_protocol::ToolKind::Other)
            .status(agent_client_protocol::ToolCallStatus::Pending);

        msg_store.push_stdout(format!(
            "{}\n",
            serde_json::to_string(&AcpEvent::ToolCall(tool_call))
                .expect("serialize tool call event")
        ));

        let entry = wait_for_tool_use_entry(msg_store.as_ref()).await;
        match entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                ..
            } => {
                assert_eq!(tool_name, EXIT_PLAN_MODE_NAME);
                match action_type {
                    ActionType::PlanPresentation { plan } => assert_eq!(plan, title),
                    other => panic!("expected plan_presentation action type, got {other:?}"),
                }
            }
            other => panic!("expected tool_use entry, got {other:?}"),
        }
        assert_eq!(entry.content, title);
    }

    #[tokio::test]
    async fn opencode_plan_exit_tool_call_normalizes_to_plan_presentation_entry() {
        let worktree = create_temp_dir("opencode-plan-presentation");
        let plan_text = "# OpenCode Plan\n- step A\n- step B\n";
        let plan_path = worktree.join("PLAN.md");
        fs::write(&plan_path, plan_text).expect("write plan file");

        let msg_store = Arc::new(MsgStore::new());
        normalize_logs(msg_store.clone(), &worktree);

        let title = format!(
            "Plan at {} is complete. Would you like to switch to the build agent and start implementing?",
            plan_path.display()
        );
        let tool_call = agent_client_protocol::ToolCall::new("plan_exit-1", title)
            .kind(agent_client_protocol::ToolKind::Other)
            .status(agent_client_protocol::ToolCallStatus::Pending);

        msg_store.push_stdout(format!(
            "{}\n",
            serde_json::to_string(&AcpEvent::ToolCall(tool_call))
                .expect("serialize tool call event")
        ));

        let entry = wait_for_tool_use_entry(msg_store.as_ref()).await;
        match entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                status,
            } => {
                assert_eq!(tool_name, EXIT_PLAN_MODE_NAME);
                assert!(matches!(status, ToolStatus::Created));
                match action_type {
                    ActionType::PlanPresentation { plan } => assert_eq!(plan, plan_text),
                    other => panic!("expected plan_presentation action type, got {other:?}"),
                }
            }
            other => panic!("expected tool_use entry, got {other:?}"),
        }
        assert_eq!(entry.content, plan_text);
        let metadata: ToolCallMetadata = serde_json::from_value(
            entry
                .metadata
                .expect("tool entry should include tool_call_id metadata"),
        )
        .expect("metadata should deserialize");
        assert_eq!(metadata.tool_call_id, "plan_exit-1");
    }

    #[tokio::test]
    async fn opencode_plan_exit_falls_back_to_title_when_plan_file_cannot_be_read() {
        let worktree = create_temp_dir("opencode-plan-fallback");
        let msg_store = Arc::new(MsgStore::new());
        normalize_logs(msg_store.clone(), &worktree);

        let title =
            "Plan at missing-plan.md is complete. Would you like to switch to the build agent?"
                .to_string();
        let tool_call = agent_client_protocol::ToolCall::new("plan_exit-2", title.clone())
            .kind(agent_client_protocol::ToolKind::Other)
            .status(agent_client_protocol::ToolCallStatus::Pending);

        msg_store.push_stdout(format!(
            "{}\n",
            serde_json::to_string(&AcpEvent::ToolCall(tool_call))
                .expect("serialize tool call event")
        ));

        let entry = wait_for_tool_use_entry(msg_store.as_ref()).await;
        match entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                ..
            } => {
                assert_eq!(tool_name, EXIT_PLAN_MODE_NAME);
                match action_type {
                    ActionType::PlanPresentation { plan } => assert_eq!(plan, title),
                    other => panic!("expected plan_presentation action type, got {other:?}"),
                }
            }
            other => panic!("expected tool_use entry, got {other:?}"),
        }
        assert_eq!(entry.content, title);
    }

    #[tokio::test]
    async fn usage_update_normalizes_to_token_usage_entry() {
        let worktree = create_temp_dir("usage-update");
        let msg_store = Arc::new(MsgStore::new());
        normalize_logs(msg_store.clone(), &worktree);

        let update = agent_client_protocol::UsageUpdate::new(321, 1_048_576);
        msg_store.push_stdout(format!(
            "{}\n",
            serde_json::to_string(&AcpEvent::Usage(update)).expect("serialize usage event")
        ));

        let entry = wait_for_token_usage_entry(msg_store.as_ref()).await;
        match entry.entry_type {
            NormalizedEntryType::TokenUsageInfo(usage) => {
                assert_eq!(usage.total_tokens, 321);
                assert_eq!(usage.model_context_window, 1_048_576);
            }
            other => panic!("expected token usage info entry, got {other:?}"),
        }
        assert_eq!(entry.content, "Tokens used: 321 / Context window: 1048576");
    }

    #[tokio::test]
    async fn model_info_normalizes_to_system_message_entry() {
        let worktree = create_temp_dir("model-info");
        let msg_store = Arc::new(MsgStore::new());
        normalize_logs(msg_store.clone(), &worktree);

        msg_store.push_stdout(format!(
            "{}\n",
            serde_json::to_string(&AcpEvent::ModelInfo("gpt-5.4".to_string()))
                .expect("serialize model info event")
        ));

        let entry = wait_for_system_message_entry(msg_store.as_ref()).await;
        assert!(matches!(
            entry.entry_type,
            NormalizedEntryType::SystemMessage
        ));
        assert_eq!(entry.content, "model: gpt-5.4");
    }
}
