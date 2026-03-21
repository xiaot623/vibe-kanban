use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, LazyLock},
};

use codex_app_server_protocol::{
    CommandExecutionStatus as AppCommandExecutionStatus, FileUpdateChange as AppFileUpdateChange,
    JSONRPCResponse, McpToolCallStatus as AppMcpToolCallStatus, NewConversationResponse,
    PatchApplyStatus as AppPatchApplyStatus, PatchChangeKind as AppPatchChangeKind,
    ServerNotification, ThreadItem as AppThreadItem, ThreadResumeResponse, ThreadStartResponse,
    TurnPlanStepStatus as AppTurnPlanStepStatus, WebSearchAction as AppWebSearchAction,
};
use codex_protocol::{openai_models::ReasoningEffort, protocol::McpInvocation};
use futures::StreamExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use workspace_utils::{
    approvals::ApprovalStatus, diff::normalize_unified_diff, msg_store::MsgStore,
    path::make_path_relative,
};

use crate::{
    approvals::ToolCallMetadata,
    executors::codex::session::SessionHandler,
    logs::{
        ActionType, CommandExitStatus, CommandRunResult, FileChange, NormalizedEntry,
        NormalizedEntryError, NormalizedEntryType, TodoItem, ToolResult, ToolResultValueType,
        ToolStatus,
        stderr_processor::normalize_stderr_logs,
        utils::{
            ConversationPatch, EntryIndexProvider,
            patch::{add_normalized_entry, replace_normalized_entry, upsert_normalized_entry},
        },
    },
};

trait ToNormalizedEntry {
    fn to_normalized_entry(&self) -> NormalizedEntry;
}

trait ToNormalizedEntryOpt {
    fn to_normalized_entry_opt(&self) -> Option<NormalizedEntry>;
}

#[derive(Default)]
struct StreamingText {
    index: usize,
    content: String,
}

#[derive(Default)]
struct CommandState {
    index: Option<usize>,
    command: String,
    stdout: String,
    stderr: String,
    formatted_output: Option<String>,
    status: ToolStatus,
    exit_code: Option<i32>,
    awaiting_approval: bool,
    call_id: String,
}

impl ToNormalizedEntry for CommandState {
    fn to_normalized_entry(&self) -> NormalizedEntry {
        let content = self.command.to_string();

        NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: "bash".to_string(),
                action_type: ActionType::CommandRun {
                    command: self.command.clone(),
                    result: Some(CommandRunResult {
                        exit_status: self
                            .exit_code
                            .map(|code| CommandExitStatus::ExitCode { code }),
                        output: if self.formatted_output.is_some() {
                            self.formatted_output.clone()
                        } else {
                            build_command_output(Some(&self.stdout), Some(&self.stderr))
                        },
                    }),
                },
                status: self.status.clone(),
            },
            content,
            metadata: serde_json::to_value(ToolCallMetadata {
                tool_call_id: self.call_id.clone(),
            })
            .ok(),
        }
    }
}

struct McpToolState {
    index: Option<usize>,
    invocation: McpInvocation,
    result: Option<ToolResult>,
    status: ToolStatus,
}

impl ToNormalizedEntry for McpToolState {
    fn to_normalized_entry(&self) -> NormalizedEntry {
        let tool_name = format!("mcp:{}:{}", self.invocation.server, self.invocation.tool);
        NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: tool_name.clone(),
                action_type: ActionType::Tool {
                    tool_name,
                    arguments: self.invocation.arguments.clone(),
                    result: self.result.clone(),
                },
                status: self.status.clone(),
            },
            content: self.invocation.tool.clone(),
            metadata: None,
        }
    }
}

#[derive(Default)]
struct WebSearchState {
    index: Option<usize>,
    query: Option<String>,
    status: ToolStatus,
}

impl ToNormalizedEntry for WebSearchState {
    fn to_normalized_entry(&self) -> NormalizedEntry {
        NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: "web_search".to_string(),
                action_type: ActionType::WebFetch {
                    url: self.query.clone().unwrap_or_else(|| "...".to_string()),
                },
                status: self.status.clone(),
            },
            content: self
                .query
                .clone()
                .unwrap_or_else(|| "Web search".to_string()),
            metadata: None,
        }
    }
}

#[derive(Default)]
struct PatchState {
    entries: Vec<PatchEntry>,
}

struct PatchEntry {
    index: Option<usize>,
    path: String,
    changes: Vec<FileChange>,
    status: ToolStatus,
    call_id: String,
}

impl ToNormalizedEntry for PatchEntry {
    fn to_normalized_entry(&self) -> NormalizedEntry {
        let content = self.path.clone();

        NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: "edit".to_string(),
                action_type: ActionType::FileEdit {
                    path: self.path.clone(),
                    changes: self.changes.clone(),
                },
                status: self.status.clone(),
            },
            content,
            metadata: serde_json::to_value(ToolCallMetadata {
                tool_call_id: self.call_id.clone(),
            })
            .ok(),
        }
    }
}

#[derive(Default)]
struct PlanState {
    index: Option<usize>,
    plan: String,
}

impl PlanState {
    fn to_normalized_entry(&self, call_id: &str) -> NormalizedEntry {
        NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: "ExitPlanMode".to_string(),
                action_type: ActionType::PlanPresentation {
                    plan: self.plan.clone(),
                },
                status: ToolStatus::Created,
            },
            content: self.plan.clone(),
            metadata: serde_json::to_value(ToolCallMetadata {
                tool_call_id: call_id.to_string(),
            })
            .ok(),
        }
    }
}

struct LogState {
    entry_index: EntryIndexProvider,
    assistant: Option<StreamingText>,
    thinking: Option<StreamingText>,
    plans: HashMap<String, PlanState>,
    commands: HashMap<String, CommandState>,
    mcp_tools: HashMap<String, McpToolState>,
    patches: HashMap<String, PatchState>,
    web_searches: HashMap<String, WebSearchState>,
    image_views: HashMap<String, usize>,
    in_plan_mode: bool,
}

enum StreamingTextKind {
    Assistant,
    Thinking,
}

impl LogState {
    fn new(entry_index: EntryIndexProvider) -> Self {
        Self {
            entry_index,
            assistant: None,
            thinking: None,
            plans: HashMap::new(),
            commands: HashMap::new(),
            mcp_tools: HashMap::new(),
            patches: HashMap::new(),
            web_searches: HashMap::new(),
            image_views: HashMap::new(),
            in_plan_mode: false,
        }
    }

    fn streaming_text_update(
        &mut self,
        content: String,
        type_: StreamingTextKind,
        mode: UpdateMode,
    ) -> (NormalizedEntry, usize, bool) {
        let index_provider = &self.entry_index;
        let entry = match type_ {
            StreamingTextKind::Assistant => &mut self.assistant,
            StreamingTextKind::Thinking => &mut self.thinking,
        };
        let is_new = entry.is_none();
        let (content, index) = if entry.is_none() {
            let index = index_provider.next();
            *entry = Some(StreamingText { index, content });
            (&entry.as_ref().unwrap().content, index)
        } else {
            let streaming_state = entry.as_mut().unwrap();
            match mode {
                UpdateMode::Append => streaming_state.content.push_str(&content),
                UpdateMode::Set => streaming_state.content = content,
            }
            (&streaming_state.content, streaming_state.index)
        };
        let normalized_entry = NormalizedEntry {
            timestamp: None,
            entry_type: match type_ {
                StreamingTextKind::Assistant => NormalizedEntryType::AssistantMessage,
                StreamingTextKind::Thinking => NormalizedEntryType::Thinking,
            },
            content: content.clone(),
            metadata: None,
        };
        (normalized_entry, index, is_new)
    }

    fn streaming_text_append(
        &mut self,
        content: String,
        type_: StreamingTextKind,
    ) -> (NormalizedEntry, usize, bool) {
        self.streaming_text_update(content, type_, UpdateMode::Append)
    }

    fn streaming_text_set(
        &mut self,
        content: String,
        type_: StreamingTextKind,
    ) -> (NormalizedEntry, usize, bool) {
        self.streaming_text_update(content, type_, UpdateMode::Set)
    }

    fn assistant_message_append(&mut self, content: String) -> (NormalizedEntry, usize, bool) {
        self.streaming_text_append(content, StreamingTextKind::Assistant)
    }

    fn thinking_append(&mut self, content: String) -> (NormalizedEntry, usize, bool) {
        self.streaming_text_append(content, StreamingTextKind::Thinking)
    }

    fn assistant_message(&mut self, content: String) -> (NormalizedEntry, usize, bool) {
        self.streaming_text_set(content, StreamingTextKind::Assistant)
    }

    fn thinking(&mut self, content: String) -> (NormalizedEntry, usize, bool) {
        self.streaming_text_set(content, StreamingTextKind::Thinking)
    }

    fn plan_update(
        &mut self,
        call_id: String,
        content: String,
        mode: UpdateMode,
    ) -> (NormalizedEntry, usize, bool) {
        let plan_state = self.plans.entry(call_id.clone()).or_default();
        let is_new = plan_state.index.is_none();
        let index = if let Some(index) = plan_state.index {
            match mode {
                UpdateMode::Append => plan_state.plan.push_str(&content),
                UpdateMode::Set => plan_state.plan = content,
            }
            index
        } else {
            let index = self.entry_index.next();
            plan_state.index = Some(index);
            plan_state.plan = content;
            index
        };

        (plan_state.to_normalized_entry(&call_id), index, is_new)
    }

    fn plan_append(&mut self, call_id: String, content: String) -> (NormalizedEntry, usize, bool) {
        self.plan_update(call_id, content, UpdateMode::Append)
    }

    fn plan_set(&mut self, call_id: String, content: String) -> (NormalizedEntry, usize, bool) {
        self.plan_update(call_id, content, UpdateMode::Set)
    }
}

enum UpdateMode {
    Append,
    Set,
}

fn format_turn_plan_step_status(status: &AppTurnPlanStepStatus) -> String {
    match status {
        AppTurnPlanStepStatus::Pending => "pending",
        AppTurnPlanStepStatus::InProgress => "in_progress",
        AppTurnPlanStepStatus::Completed => "completed",
    }
    .to_string()
}

fn command_status_to_tool_status(status: &AppCommandExecutionStatus) -> ToolStatus {
    match status {
        AppCommandExecutionStatus::InProgress => ToolStatus::Created,
        AppCommandExecutionStatus::Completed => ToolStatus::Success,
        AppCommandExecutionStatus::Failed | AppCommandExecutionStatus::Declined => {
            ToolStatus::Failed
        }
    }
}

fn patch_status_to_tool_status(status: &AppPatchApplyStatus) -> ToolStatus {
    match status {
        AppPatchApplyStatus::InProgress => ToolStatus::Created,
        AppPatchApplyStatus::Completed => ToolStatus::Success,
        AppPatchApplyStatus::Failed | AppPatchApplyStatus::Declined => ToolStatus::Failed,
    }
}

fn mcp_status_to_tool_status(status: &AppMcpToolCallStatus) -> ToolStatus {
    match status {
        AppMcpToolCallStatus::InProgress => ToolStatus::Created,
        AppMcpToolCallStatus::Completed => ToolStatus::Success,
        AppMcpToolCallStatus::Failed => ToolStatus::Failed,
    }
}

fn normalize_v2_file_changes(
    worktree_path: &str,
    changes: &[AppFileUpdateChange],
) -> Vec<(String, Vec<FileChange>)> {
    changes
        .iter()
        .map(|change| {
            let relative = make_path_relative(&change.path, worktree_path);
            let file_changes = match &change.kind {
                AppPatchChangeKind::Add => vec![FileChange::Write {
                    content: change.diff.clone(),
                }],
                AppPatchChangeKind::Delete => vec![FileChange::Delete],
                AppPatchChangeKind::Update { move_path } => {
                    let mut edits = Vec::new();
                    if let Some(dest) = move_path {
                        let dest_rel =
                            make_path_relative(dest.to_string_lossy().as_ref(), worktree_path);
                        edits.push(FileChange::Rename { new_path: dest_rel });
                    }
                    edits.push(FileChange::Edit {
                        unified_diff: normalize_unified_diff(&relative, &change.diff),
                        has_line_numbers: true,
                    });
                    edits
                }
            };
            (relative, file_changes)
        })
        .collect()
}

fn web_search_query_from_action(
    action: Option<&AppWebSearchAction>,
    fallback_query: &str,
) -> String {
    match action {
        Some(AppWebSearchAction::Search { query, queries }) => query
            .as_deref()
            .or_else(|| {
                queries
                    .as_ref()
                    .and_then(|qs| qs.first().map(String::as_str))
            })
            .map(str::to_string)
            .unwrap_or_else(|| fallback_query.to_string()),
        Some(AppWebSearchAction::OpenPage { url }) => {
            url.clone().unwrap_or_else(|| fallback_query.to_string())
        }
        Some(AppWebSearchAction::FindInPage { url, pattern }) => {
            let mut parts = Vec::new();
            if let Some(url) = url {
                parts.push(url.clone());
            }
            if let Some(pattern) = pattern {
                parts.push(format!("find:{pattern}"));
            }
            if parts.is_empty() {
                fallback_query.to_string()
            } else {
                parts.join(" ")
            }
        }
        Some(AppWebSearchAction::Other) | None => fallback_query.to_string(),
    }
}

fn handle_v2_item_started(
    item: AppThreadItem,
    state: &mut LogState,
    msg_store: &Arc<MsgStore>,
    entry_index: &EntryIndexProvider,
    worktree_path: &str,
) {
    match item {
        AppThreadItem::Plan { .. } => {
            state.in_plan_mode = true;
        }
        AppThreadItem::CommandExecution {
            id,
            command,
            status,
            aggregated_output,
            exit_code,
            ..
        } => {
            state.assistant = None;
            state.thinking = None;
            let command_state = state
                .commands
                .entry(id.clone())
                .or_insert_with(|| CommandState {
                    index: None,
                    command: command.clone(),
                    stdout: String::new(),
                    stderr: String::new(),
                    formatted_output: None,
                    status: ToolStatus::Created,
                    exit_code: None,
                    awaiting_approval: false,
                    call_id: id.clone(),
                });
            if command_state.command.is_empty() {
                command_state.command = command;
            }
            command_state.formatted_output = aggregated_output;
            command_state.exit_code = exit_code;
            command_state.awaiting_approval = false;
            command_state.status = command_status_to_tool_status(&status);

            if let Some(index) = command_state.index {
                replace_normalized_entry(&msg_store, index, command_state.to_normalized_entry());
            } else {
                let index = add_normalized_entry(
                    &msg_store,
                    &entry_index,
                    command_state.to_normalized_entry(),
                );
                command_state.index = Some(index);
            }
        }
        AppThreadItem::FileChange {
            id,
            changes,
            status,
        } => {
            state.assistant = None;
            state.thinking = None;
            let normalized = normalize_v2_file_changes(worktree_path, &changes);
            let patch_state = state.patches.entry(id.clone()).or_default();

            for entry in patch_state.entries.drain(..) {
                if let Some(index) = entry.index {
                    msg_store.push_patch(ConversationPatch::remove(index));
                }
            }

            let normalized_status = patch_status_to_tool_status(&status);
            for (path, file_changes) in normalized {
                let mut entry = PatchEntry {
                    index: None,
                    path,
                    changes: file_changes,
                    status: normalized_status.clone(),
                    call_id: id.clone(),
                };
                let index =
                    add_normalized_entry(&msg_store, &entry_index, entry.to_normalized_entry());
                entry.index = Some(index);
                patch_state.entries.push(entry);
            }
        }
        AppThreadItem::McpToolCall {
            id,
            server,
            tool,
            arguments,
            status,
            result,
            error,
            ..
        } => {
            state.assistant = None;
            state.thinking = None;
            let mut tool_state = McpToolState {
                index: None,
                invocation: McpInvocation {
                    server,
                    tool,
                    arguments: Some(arguments),
                },
                result: None,
                status: mcp_status_to_tool_status(&status),
            };
            if let Some(err) = error {
                tool_state.status = ToolStatus::Failed;
                tool_state.result = Some(ToolResult {
                    r#type: ToolResultValueType::Markdown,
                    value: Value::String(err.message),
                });
            } else if let Some(result) = result {
                if result
                    .content
                    .iter()
                    .all(|block| extract_mcp_text_content(block).is_some())
                {
                    tool_state.result = Some(ToolResult {
                        r#type: ToolResultValueType::Markdown,
                        value: Value::String(
                            result
                                .content
                                .iter()
                                .filter_map(extract_mcp_text_content)
                                .collect::<Vec<String>>()
                                .join("\n"),
                        ),
                    });
                } else {
                    tool_state.result = Some(ToolResult {
                        r#type: ToolResultValueType::Json,
                        value: result
                            .structured_content
                            .unwrap_or_else(|| Value::Array(result.content)),
                    });
                }
            }

            state.mcp_tools.insert(id.clone(), tool_state);
            let mcp_tool_state = state.mcp_tools.get_mut(&id).unwrap();
            if let Some(index) = mcp_tool_state.index {
                replace_normalized_entry(&msg_store, index, mcp_tool_state.to_normalized_entry());
            } else {
                let index = add_normalized_entry(
                    &msg_store,
                    &entry_index,
                    mcp_tool_state.to_normalized_entry(),
                );
                mcp_tool_state.index = Some(index);
            }
        }
        AppThreadItem::WebSearch { id, query, action } => {
            state.assistant = None;
            state.thinking = None;
            state.web_searches.insert(
                id.clone(),
                WebSearchState {
                    index: None,
                    query: Some(web_search_query_from_action(action.as_ref(), &query)),
                    status: ToolStatus::Created,
                },
            );
            let web_search_state = state.web_searches.get_mut(&id).unwrap();
            let normalized_entry = web_search_state.to_normalized_entry();
            let index = add_normalized_entry(&msg_store, &entry_index, normalized_entry);
            web_search_state.index = Some(index);
        }
        AppThreadItem::ImageView { id, path } => {
            state.assistant = None;
            state.thinking = None;
            let relative_path = make_path_relative(&path, worktree_path);
            let entry = NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::ToolUse {
                    tool_name: "view_image".to_string(),
                    action_type: ActionType::FileRead {
                        path: relative_path.clone(),
                    },
                    status: ToolStatus::Created,
                },
                content: relative_path,
                metadata: serde_json::to_value(ToolCallMetadata {
                    tool_call_id: id.clone(),
                })
                .ok(),
            };
            let index = add_normalized_entry(&msg_store, &entry_index, entry);
            state.image_views.insert(id, index);
        }
        _ => {}
    }
}

fn handle_v2_item_completed(
    item: AppThreadItem,
    state: &mut LogState,
    msg_store: &Arc<MsgStore>,
    entry_index: &EntryIndexProvider,
    worktree_path: &str,
) {
    match item {
        AppThreadItem::AgentMessage { text, .. } => {
            state.thinking = None;
            let (entry, index, is_new) = state.assistant_message(text);
            upsert_normalized_entry(&msg_store, index, entry, is_new);
            state.assistant = None;
        }
        AppThreadItem::Plan { id, text } => {
            let text = text.trim().to_string();
            if !text.is_empty() {
                state.in_plan_mode = true;
                state.assistant = None;
                state.thinking = None;
                let (entry, index, is_new) = state.plan_set(id, text);
                upsert_normalized_entry(&msg_store, index, entry, is_new);
            }
        }
        AppThreadItem::Reasoning {
            summary, content, ..
        } => {
            state.assistant = None;
            if !state.in_plan_mode {
                let text = if !content.is_empty() {
                    content.join("")
                } else {
                    summary.join("")
                };
                if !text.trim().is_empty() {
                    let (entry, index, is_new) = state.thinking(text);
                    upsert_normalized_entry(&msg_store, index, entry, is_new);
                }
            }
            state.thinking = None;
        }
        AppThreadItem::CommandExecution {
            id,
            command,
            status,
            aggregated_output,
            exit_code,
            ..
        } => {
            if let Some(mut command_state) = state.commands.remove(&id) {
                if command_state.command.is_empty() {
                    command_state.command = command;
                }
                command_state.formatted_output = aggregated_output;
                command_state.exit_code = exit_code;
                command_state.awaiting_approval = false;
                command_state.status = command_status_to_tool_status(&status);
                if let Some(index) = command_state.index {
                    replace_normalized_entry(
                        &msg_store,
                        index,
                        command_state.to_normalized_entry(),
                    );
                } else {
                    add_normalized_entry(
                        &msg_store,
                        &entry_index,
                        command_state.to_normalized_entry(),
                    );
                }
            } else {
                let command_state = CommandState {
                    index: None,
                    command,
                    stdout: String::new(),
                    stderr: String::new(),
                    formatted_output: aggregated_output,
                    status: command_status_to_tool_status(&status),
                    exit_code,
                    awaiting_approval: false,
                    call_id: id,
                };
                add_normalized_entry(
                    &msg_store,
                    &entry_index,
                    command_state.to_normalized_entry(),
                );
            }
        }
        AppThreadItem::FileChange {
            id,
            changes,
            status,
        } => {
            let normalized = normalize_v2_file_changes(worktree_path, &changes);
            let status = patch_status_to_tool_status(&status);
            if let Some(mut patch_state) = state.patches.remove(&id) {
                let mut iter = normalized.into_iter();
                for entry in &mut patch_state.entries {
                    if let Some((path, file_changes)) = iter.next() {
                        entry.path = path;
                        entry.changes = file_changes;
                    }
                    entry.status = status.clone();
                    if let Some(index) = entry.index {
                        replace_normalized_entry(&msg_store, index, entry.to_normalized_entry());
                    } else {
                        add_normalized_entry(&msg_store, &entry_index, entry.to_normalized_entry());
                    }
                }
                for (path, file_changes) in iter {
                    add_normalized_entry(
                        &msg_store,
                        &entry_index,
                        PatchEntry {
                            index: None,
                            path,
                            changes: file_changes,
                            status: status.clone(),
                            call_id: id.clone(),
                        }
                        .to_normalized_entry(),
                    );
                }
            } else {
                for (path, file_changes) in normalized {
                    add_normalized_entry(
                        &msg_store,
                        &entry_index,
                        PatchEntry {
                            index: None,
                            path,
                            changes: file_changes,
                            status: status.clone(),
                            call_id: id.clone(),
                        }
                        .to_normalized_entry(),
                    );
                }
            }
        }
        AppThreadItem::McpToolCall {
            id,
            server,
            tool,
            arguments,
            status,
            result,
            error,
            ..
        } => {
            if let Some(mut mcp_tool_state) = state.mcp_tools.remove(&id) {
                mcp_tool_state.status = mcp_status_to_tool_status(&status);
                if let Some(err) = error {
                    mcp_tool_state.status = ToolStatus::Failed;
                    mcp_tool_state.result = Some(ToolResult {
                        r#type: ToolResultValueType::Markdown,
                        value: Value::String(err.message),
                    });
                } else if let Some(result) = result {
                    if result
                        .content
                        .iter()
                        .all(|block| extract_mcp_text_content(block).is_some())
                    {
                        mcp_tool_state.result = Some(ToolResult {
                            r#type: ToolResultValueType::Markdown,
                            value: Value::String(
                                result
                                    .content
                                    .iter()
                                    .filter_map(extract_mcp_text_content)
                                    .collect::<Vec<String>>()
                                    .join("\n"),
                            ),
                        });
                    } else {
                        mcp_tool_state.result = Some(ToolResult {
                            r#type: ToolResultValueType::Json,
                            value: result
                                .structured_content
                                .unwrap_or_else(|| Value::Array(result.content)),
                        });
                    }
                }
                if let Some(index) = mcp_tool_state.index {
                    replace_normalized_entry(
                        &msg_store,
                        index,
                        mcp_tool_state.to_normalized_entry(),
                    );
                } else {
                    add_normalized_entry(
                        &msg_store,
                        &entry_index,
                        mcp_tool_state.to_normalized_entry(),
                    );
                }
            } else {
                let mut mcp_tool_state = McpToolState {
                    index: None,
                    invocation: McpInvocation {
                        server,
                        tool,
                        arguments: Some(arguments),
                    },
                    result: None,
                    status: mcp_status_to_tool_status(&status),
                };
                if let Some(err) = error {
                    mcp_tool_state.status = ToolStatus::Failed;
                    mcp_tool_state.result = Some(ToolResult {
                        r#type: ToolResultValueType::Markdown,
                        value: Value::String(err.message),
                    });
                } else if let Some(result) = result {
                    mcp_tool_state.result = Some(ToolResult {
                        r#type: ToolResultValueType::Json,
                        value: result
                            .structured_content
                            .unwrap_or_else(|| Value::Array(result.content)),
                    });
                }
                add_normalized_entry(
                    &msg_store,
                    &entry_index,
                    mcp_tool_state.to_normalized_entry(),
                );
            }
        }
        AppThreadItem::WebSearch { id, query, action } => {
            if let Some(mut entry) = state.web_searches.remove(&id) {
                entry.status = ToolStatus::Success;
                entry.query = Some(web_search_query_from_action(action.as_ref(), &query));
                if let Some(index) = entry.index {
                    replace_normalized_entry(&msg_store, index, entry.to_normalized_entry());
                } else {
                    add_normalized_entry(&msg_store, &entry_index, entry.to_normalized_entry());
                }
            } else {
                add_normalized_entry(
                    &msg_store,
                    &entry_index,
                    WebSearchState {
                        index: None,
                        query: Some(web_search_query_from_action(action.as_ref(), &query)),
                        status: ToolStatus::Success,
                    }
                    .to_normalized_entry(),
                );
            }
        }
        AppThreadItem::ImageView { id, path } => {
            let relative_path = make_path_relative(&path, worktree_path);
            let entry = NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::ToolUse {
                    tool_name: "view_image".to_string(),
                    action_type: ActionType::FileRead {
                        path: relative_path.clone(),
                    },
                    status: ToolStatus::Success,
                },
                content: relative_path,
                metadata: serde_json::to_value(ToolCallMetadata {
                    tool_call_id: id.clone(),
                })
                .ok(),
            };
            if let Some(index) = state.image_views.remove(&id) {
                replace_normalized_entry(&msg_store, index, entry);
            } else {
                add_normalized_entry(&msg_store, &entry_index, entry);
            }
        }
        _ => {}
    }
}

fn handle_server_notification(
    notification: ServerNotification,
    state: &mut LogState,
    msg_store: &Arc<MsgStore>,
    entry_index: &EntryIndexProvider,
    worktree_path: &str,
) {
    match notification {
        ServerNotification::SessionConfigured(session_configured) => {
            msg_store.push_session_id(session_configured.session_id.to_string());
            handle_model_params(
                session_configured.model,
                session_configured.reasoning_effort,
                msg_store,
                entry_index,
            );
        }
        ServerNotification::ThreadTokenUsageUpdated(event) => {
            let total_tokens = if event.token_usage.last.total_tokens > 0 {
                event.token_usage.last.total_tokens
            } else {
                event.token_usage.total.total_tokens
            }
            .max(0) as u32;
            let model_context_window =
                event.token_usage.model_context_window.unwrap_or_default().max(0) as u32;

            add_normalized_entry(
                msg_store,
                entry_index,
                NormalizedEntry {
                    timestamp: None,
                    entry_type: NormalizedEntryType::TokenUsageInfo(crate::logs::TokenUsageInfo {
                        total_tokens,
                        model_context_window,
                    }),
                    content: format!(
                        "Tokens used: {} / Context window: {}",
                        total_tokens, model_context_window
                    ),
                    metadata: None,
                },
            );
        }
        ServerNotification::TurnStarted(_) => {
            state.in_plan_mode = false;
            state.assistant = None;
            state.thinking = None;
            state.plans.clear();
        }
        ServerNotification::TurnCompleted(_) => {
            state.assistant = None;
            state.thinking = None;
            state.in_plan_mode = false;
            state.plans.clear();
        }
        ServerNotification::AgentMessageDelta(event) => {
            state.thinking = None;
            let (entry, index, is_new) = state.assistant_message_append(event.delta);
            upsert_normalized_entry(msg_store, index, entry, is_new);
        }
        ServerNotification::ReasoningTextDelta(event) => {
            state.assistant = None;
            if !state.in_plan_mode {
                let (entry, index, is_new) = state.thinking_append(event.delta);
                upsert_normalized_entry(msg_store, index, entry, is_new);
            }
        }
        ServerNotification::ReasoningSummaryTextDelta(event) => {
            state.assistant = None;
            if !state.in_plan_mode {
                let (entry, index, is_new) = state.thinking_append(event.delta);
                upsert_normalized_entry(msg_store, index, entry, is_new);
            }
        }
        ServerNotification::ReasoningSummaryPartAdded(_) => {
            state.assistant = None;
            state.thinking = None;
        }
        ServerNotification::PlanDelta(event) => {
            state.in_plan_mode = true;
            state.assistant = None;
            state.thinking = None;
            let (entry, index, is_new) = state.plan_append(event.item_id, event.delta);
            upsert_normalized_entry(msg_store, index, entry, is_new);
        }
        ServerNotification::ItemStarted(event) => {
            handle_v2_item_started(event.item, state, msg_store, entry_index, worktree_path);
        }
        ServerNotification::ItemCompleted(event) => {
            handle_v2_item_completed(event.item, state, msg_store, entry_index, worktree_path);
        }
        ServerNotification::CommandExecutionOutputDelta(event) => {
            let command_state = state
                .commands
                .entry(event.item_id.clone())
                .or_insert_with(|| CommandState {
                    index: None,
                    command: "command execution".to_string(),
                    stdout: String::new(),
                    stderr: String::new(),
                    formatted_output: None,
                    status: ToolStatus::Created,
                    exit_code: None,
                    awaiting_approval: false,
                    call_id: event.item_id.clone(),
                });
            if !event.delta.is_empty() {
                command_state.stdout.push_str(&event.delta);
            }
            if let Some(index) = command_state.index {
                replace_normalized_entry(msg_store, index, command_state.to_normalized_entry());
            } else {
                let index = add_normalized_entry(
                    msg_store,
                    entry_index,
                    command_state.to_normalized_entry(),
                );
                command_state.index = Some(index);
            }
        }
        ServerNotification::FileChangeOutputDelta(_event) => {}
        ServerNotification::McpToolCallProgress(event) => {
            if let Some(mcp_tool_state) = state.mcp_tools.get_mut(&event.item_id) {
                mcp_tool_state.result = Some(ToolResult {
                    r#type: ToolResultValueType::Markdown,
                    value: Value::String(event.message),
                });
                if let Some(index) = mcp_tool_state.index {
                    replace_normalized_entry(
                        msg_store,
                        index,
                        mcp_tool_state.to_normalized_entry(),
                    );
                }
            }
        }
        ServerNotification::TurnPlanUpdated(event) => {
            let todos: Vec<TodoItem> = event
                .plan
                .iter()
                .map(|item| TodoItem {
                    content: item.step.clone(),
                    status: format_turn_plan_step_status(&item.status),
                    priority: None,
                })
                .collect();
            let explanation = event
                .explanation
                .as_ref()
                .map(|text| text.trim())
                .filter(|text| !text.is_empty())
                .map(|text| text.to_string());
            let content = explanation.clone().unwrap_or_else(|| {
                if todos.is_empty() {
                    "Plan updated".to_string()
                } else {
                    format!("Plan updated ({} steps)", todos.len())
                }
            });

            add_normalized_entry(
                msg_store,
                entry_index,
                NormalizedEntry {
                    timestamp: None,
                    entry_type: NormalizedEntryType::ToolUse {
                        tool_name: "plan".to_string(),
                        action_type: ActionType::TodoManagement {
                            todos,
                            operation: "update".to_string(),
                        },
                        status: ToolStatus::Success,
                    },
                    content,
                    metadata: None,
                },
            );
        }
        ServerNotification::Error(event) => {
            add_normalized_entry(
                msg_store,
                entry_index,
                NormalizedEntry {
                    timestamp: None,
                    entry_type: NormalizedEntryType::ErrorMessage {
                        error_type: NormalizedEntryError::Other,
                    },
                    content: format!(
                        "Error: {} {:?}",
                        event.error.message, event.error.codex_error_info
                    ),
                    metadata: None,
                },
            );
        }
        ServerNotification::ContextCompacted(_) => {
            add_normalized_entry(
                msg_store,
                entry_index,
                NormalizedEntry {
                    timestamp: None,
                    entry_type: NormalizedEntryType::SystemMessage,
                    content: "Context compacted".to_string(),
                    metadata: None,
                },
            );
        }
        _ => {}
    }
}

pub fn normalize_logs(msg_store: Arc<MsgStore>, worktree_path: &Path) {
    let entry_index = EntryIndexProvider::start_from(&msg_store);
    normalize_stderr_logs(msg_store.clone(), entry_index.clone());

    let worktree_path_str = worktree_path.to_string_lossy().to_string();
    tokio::spawn(async move {
        let mut state = LogState::new(entry_index.clone());
        let mut stdout_lines = msg_store.stdout_lines_stream();

        while let Some(Ok(line)) = stdout_lines.next().await {
            if let Ok(error) = serde_json::from_str::<Error>(&line) {
                add_normalized_entry(&msg_store, &entry_index, error.to_normalized_entry());
                continue;
            }

            if let Ok(approval) = serde_json::from_str::<Approval>(&line) {
                if let Some(entry) = approval.to_normalized_entry_opt() {
                    add_normalized_entry(&msg_store, &entry_index, entry);
                }
                continue;
            }

            if let Ok(response) = serde_json::from_str::<JSONRPCResponse>(&line) {
                handle_jsonrpc_response(response, &msg_store, &entry_index);
                continue;
            }

            if let Ok(server_notification) = serde_json::from_str::<ServerNotification>(&line) {
                handle_server_notification(
                    server_notification,
                    &mut state,
                    &msg_store,
                    &entry_index,
                    &worktree_path_str,
                );
                continue;
            } else if let Some(session_id) = line
                .strip_prefix(r#"{"method":"sessionConfigured","params":{"sessionId":""#)
                .and_then(|suffix| SESSION_ID.captures(suffix).and_then(|caps| caps.get(1)))
            {
                // Best-effort extraction of session ID from logs in case the JSON parsing fails.
                // This could happen if the line is truncated due to size limits because it includes the full session history.
                msg_store.push_session_id(session_id.as_str().to_string());
                continue;
            }
        }
    });
}

fn handle_jsonrpc_response(
    response: JSONRPCResponse,
    msg_store: &Arc<MsgStore>,
    entry_index: &EntryIndexProvider,
) {
    if let Ok(response) = serde_json::from_value::<NewConversationResponse>(response.result.clone())
    {
        match SessionHandler::extract_session_id_from_rollout_path(response.rollout_path) {
            Ok(session_id) => msg_store.push_session_id(session_id),
            Err(err) => tracing::error!("failed to extract session id: {err}"),
        }

        handle_model_params(
            response.model,
            response.reasoning_effort,
            msg_store,
            entry_index,
        );
        return;
    }

    if let Ok(response) = serde_json::from_value::<ThreadStartResponse>(response.result.clone()) {
        msg_store.push_session_id(response.thread.id);
        handle_model_params(
            response.model,
            response.reasoning_effort,
            msg_store,
            entry_index,
        );
        return;
    }

    if let Ok(response) = serde_json::from_value::<ThreadResumeResponse>(response.result) {
        msg_store.push_session_id(response.thread.id);
        handle_model_params(
            response.model,
            response.reasoning_effort,
            msg_store,
            entry_index,
        );
    }
}

fn handle_model_params(
    model: String,
    reasoning_effort: Option<ReasoningEffort>,
    msg_store: &Arc<MsgStore>,
    entry_index: &EntryIndexProvider,
) {
    let mut params = vec![];
    params.push(format!("model: {model}"));
    if let Some(reasoning_effort) = reasoning_effort {
        params.push(format!("reasoning effort: {reasoning_effort}"));
    }

    add_normalized_entry(
        msg_store,
        entry_index,
        NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::SystemMessage,
            content: params.join("  ").to_string(),
            metadata: None,
        },
    );
}

fn build_command_output(stdout: Option<&str>, stderr: Option<&str>) -> Option<String> {
    let mut sections = Vec::new();
    if let Some(out) = stdout {
        let cleaned = out.trim();
        if !cleaned.is_empty() {
            sections.push(format!("stdout:\n{cleaned}"));
        }
    }
    if let Some(err) = stderr {
        let cleaned = err.trim();
        if !cleaned.is_empty() {
            sections.push(format!("stderr:\n{cleaned}"));
        }
    }

    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

fn extract_mcp_text_content(block: &Value) -> Option<String> {
    if block.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    block
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_string)
}

static SESSION_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})"#)
        .expect("valid regex")
});

#[derive(Serialize, Deserialize, Debug)]
pub enum Error {
    LaunchError { error: String },
    AuthRequired { error: String },
}

impl Error {
    pub fn launch_error(error: String) -> Self {
        Self::LaunchError { error }
    }
    pub fn auth_required(error: String) -> Self {
        Self::AuthRequired { error }
    }

    pub fn raw(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl ToNormalizedEntry for Error {
    fn to_normalized_entry(&self) -> NormalizedEntry {
        match self {
            Error::LaunchError { error } => NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::ErrorMessage {
                    error_type: NormalizedEntryError::Other,
                },
                content: error.clone(),
                metadata: None,
            },
            Error::AuthRequired { error } => NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::ErrorMessage {
                    error_type: NormalizedEntryError::SetupRequired,
                },
                content: error.clone(),
                metadata: None,
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Approval {
    ApprovalResponse {
        call_id: String,
        tool_name: String,
        approval_status: ApprovalStatus,
    },
}

impl Approval {
    pub fn approval_response(
        call_id: String,
        tool_name: String,
        approval_status: ApprovalStatus,
    ) -> Self {
        Self::ApprovalResponse {
            call_id,
            tool_name,
            approval_status,
        }
    }

    pub fn raw(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn display_tool_name(&self) -> String {
        let Self::ApprovalResponse { tool_name, .. } = self;
        match tool_name.as_str() {
            "codex.exec_command" => "Exec Command".to_string(),
            "codex.apply_patch" => "Edit".to_string(),
            "request_user_input" => "Request User Input".to_string(),
            other => other.to_string(),
        }
    }
}

impl ToNormalizedEntryOpt for Approval {
    fn to_normalized_entry_opt(&self) -> Option<NormalizedEntry> {
        let Self::ApprovalResponse {
            call_id: _,
            tool_name: _,
            approval_status,
        } = self;
        let tool_name = self.display_tool_name();

        match approval_status {
            ApprovalStatus::Pending => None,
            ApprovalStatus::Approved => None,
            ApprovalStatus::ProvidedInput { .. } => None,
            ApprovalStatus::Denied { reason } => Some(NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::UserFeedback {
                    denied_tool: tool_name.clone(),
                },
                content: reason
                    .clone()
                    .unwrap_or_else(|| "User denied this tool use request".to_string())
                    .trim()
                    .to_string(),
                metadata: None,
            }),
            ApprovalStatus::TimedOut => Some(NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::ErrorMessage {
                    error_type: NormalizedEntryError::Other,
                },
                content: format!("Approval timed out for tool {tool_name}"),
                metadata: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};

    use serde_json::json;
    use workspace_utils::{log_msg::LogMsg, msg_store::MsgStore};

    use super::*;
    use crate::{
        approvals::ToolCallMetadata,
        logs::{
            ActionType, NormalizedEntryType, ToolStatus,
            utils::patch::extract_normalized_entry_from_patch,
        },
    };

    fn create_temp_dir(test_name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vk-codex-normalize-{test_name}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("create temporary test directory");
        dir
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

    async fn wait_for_entry<F>(msg_store: &MsgStore, predicate: F) -> NormalizedEntry
    where
        F: Fn(&NormalizedEntry) -> bool,
    {
        for _ in 0..120 {
            let entries = collect_entries(msg_store);
            if let Some(entry) = entries.into_iter().find(|entry| predicate(entry)) {
                return entry;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for normalized entry");
    }

    fn push_json_line(msg_store: &MsgStore, value: Value) {
        msg_store.push_stdout(format!("{value}\n"));
    }

    #[tokio::test]
    async fn v2_agent_message_delta_and_completed_normalize_to_assistant_message() {
        let worktree = create_temp_dir("v2-agent-message");
        let msg_store = Arc::new(MsgStore::new());
        normalize_logs(msg_store.clone(), &worktree);

        push_json_line(
            msg_store.as_ref(),
            json!({
                "method": "item/agentMessage/delta",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "msg-1",
                    "delta": "Hel"
                }
            }),
        );
        push_json_line(
            msg_store.as_ref(),
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "type": "agentMessage",
                        "id": "msg-1",
                        "text": "Hello"
                    }
                }
            }),
        );

        let entry = wait_for_entry(msg_store.as_ref(), |entry| {
            matches!(entry.entry_type, NormalizedEntryType::AssistantMessage)
        })
        .await;
        assert_eq!(entry.content, "Hello");
        msg_store.push_finished();
    }

    #[tokio::test]
    async fn v2_command_started_output_and_completed_normalize() {
        let worktree = create_temp_dir("v2-command");
        let msg_store = Arc::new(MsgStore::new());
        normalize_logs(msg_store.clone(), &worktree);

        push_json_line(
            msg_store.as_ref(),
            json!({
                "method": "item/started",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "type": "commandExecution",
                        "id": "cmd-1",
                        "command": "echo hello",
                        "cwd": worktree.to_string_lossy(),
                        "processId": "proc-1",
                        "status": "inProgress",
                        "commandActions": [],
                        "aggregatedOutput": null,
                        "exitCode": null,
                        "durationMs": null
                    }
                }
            }),
        );
        push_json_line(
            msg_store.as_ref(),
            json!({
                "method": "item/commandExecution/outputDelta",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "cmd-1",
                    "delta": "hello\n"
                }
            }),
        );
        push_json_line(
            msg_store.as_ref(),
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "type": "commandExecution",
                        "id": "cmd-1",
                        "command": "echo hello",
                        "cwd": worktree.to_string_lossy(),
                        "processId": "proc-1",
                        "status": "completed",
                        "commandActions": [],
                        "aggregatedOutput": "stdout:\nhello",
                        "exitCode": 0,
                        "durationMs": 20
                    }
                }
            }),
        );

        let entry = wait_for_entry(msg_store.as_ref(), |entry| {
            let metadata = entry.metadata.clone();
            metadata
                .and_then(|value| serde_json::from_value::<ToolCallMetadata>(value).ok())
                .is_some_and(|metadata| metadata.tool_call_id == "cmd-1")
        })
        .await;

        match entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                status,
            } => {
                assert_eq!(tool_name, "bash");
                assert!(matches!(status, ToolStatus::Success));
                match action_type {
                    ActionType::CommandRun { command, result } => {
                        assert_eq!(command, "echo hello");
                        let result = result.expect("command result");
                        assert!(result.output.unwrap_or_default().contains("hello"));
                    }
                    other => panic!("expected command action, got {other:?}"),
                }
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        msg_store.push_finished();
    }

    #[tokio::test]
    async fn v2_file_change_started_and_completed_normalize() {
        let worktree = create_temp_dir("v2-file-change");
        let msg_store = Arc::new(MsgStore::new());
        normalize_logs(msg_store.clone(), &worktree);
        let changed_file = worktree.join("src/new_file.txt");

        push_json_line(
            msg_store.as_ref(),
            json!({
                "method": "item/started",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "type": "fileChange",
                        "id": "patch-1",
                        "changes": [{
                            "path": changed_file.to_string_lossy(),
                            "kind": { "type": "add" },
                            "diff": "hello world"
                        }],
                        "status": "inProgress"
                    }
                }
            }),
        );
        push_json_line(
            msg_store.as_ref(),
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "type": "fileChange",
                        "id": "patch-1",
                        "changes": [{
                            "path": changed_file.to_string_lossy(),
                            "kind": { "type": "add" },
                            "diff": "hello world"
                        }],
                        "status": "completed"
                    }
                }
            }),
        );

        let entry = wait_for_entry(msg_store.as_ref(), |entry| {
            let metadata = entry.metadata.clone();
            metadata
                .and_then(|value| serde_json::from_value::<ToolCallMetadata>(value).ok())
                .is_some_and(|metadata| metadata.tool_call_id == "patch-1")
        })
        .await;

        match entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                status,
            } => {
                assert_eq!(tool_name, "edit");
                assert!(matches!(status, ToolStatus::Success));
                match action_type {
                    ActionType::FileEdit { path, changes } => {
                        assert_eq!(path, "src/new_file.txt");
                        assert!(
                            changes
                                .iter()
                                .any(|change| matches!(change, FileChange::Write { content } if content == "hello world"))
                        );
                    }
                    other => panic!("expected file edit action, got {other:?}"),
                }
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        msg_store.push_finished();
    }

    #[tokio::test]
    async fn v2_plan_delta_and_completed_normalize() {
        let worktree = create_temp_dir("v2-plan");
        let msg_store = Arc::new(MsgStore::new());
        normalize_logs(msg_store.clone(), &worktree);

        push_json_line(
            msg_store.as_ref(),
            json!({
                "method": "item/plan/delta",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "plan-1",
                    "delta": "Step 1"
                }
            }),
        );
        push_json_line(
            msg_store.as_ref(),
            json!({
                "method": "item/plan/delta",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "plan-1",
                    "delta": "\nStep 2"
                }
            }),
        );
        push_json_line(
            msg_store.as_ref(),
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "type": "plan",
                        "id": "plan-1",
                        "text": "Step 1\nStep 2\nStep 3"
                    }
                }
            }),
        );

        let entry = wait_for_entry(msg_store.as_ref(), |entry| {
            let metadata = entry.metadata.clone();
            metadata
                .and_then(|value| serde_json::from_value::<ToolCallMetadata>(value).ok())
                .is_some_and(|metadata| metadata.tool_call_id == "plan-1")
        })
        .await;

        match entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                ..
            } => {
                assert_eq!(tool_name, "ExitPlanMode");
                match action_type {
                    ActionType::PlanPresentation { plan } => {
                        assert_eq!(plan, "Step 1\nStep 2\nStep 3");
                    }
                    other => panic!("expected plan presentation action, got {other:?}"),
                }
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        msg_store.push_finished();
    }

    #[tokio::test]
    async fn v2_thread_token_usage_updated_normalize() {
        let worktree = create_temp_dir("v2-token-usage");
        let msg_store = Arc::new(MsgStore::new());
        normalize_logs(msg_store.clone(), &worktree);

        push_json_line(
            msg_store.as_ref(),
            json!({
                "method": "thread/tokenUsage/updated",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "tokenUsage": {
                        "total": {
                            "totalTokens": 321,
                            "inputTokens": 300,
                            "cachedInputTokens": 0,
                            "outputTokens": 21,
                            "reasoningOutputTokens": 0
                        },
                        "last": {
                            "totalTokens": 321,
                            "inputTokens": 300,
                            "cachedInputTokens": 0,
                            "outputTokens": 21,
                            "reasoningOutputTokens": 0
                        },
                        "modelContextWindow": 128000
                    }
                }
            }),
        );

        let entry = wait_for_entry(msg_store.as_ref(), |entry| {
            matches!(entry.entry_type, NormalizedEntryType::TokenUsageInfo(_))
        })
        .await;

        match entry.entry_type {
            NormalizedEntryType::TokenUsageInfo(usage) => {
                assert_eq!(usage.total_tokens, 321);
                assert_eq!(usage.model_context_window, 128000);
            }
            other => panic!("expected token usage info, got {other:?}"),
        }
        assert_eq!(entry.content, "Tokens used: 321 / Context window: 128000");
        msg_store.push_finished();
    }

}
