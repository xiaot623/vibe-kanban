use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use db::models::{
    execution_process::{ExecutionContext, ExecutionProcess},
    execution_process_lark_wiki_doc::{
        ExecutionProcessLarkWikiDoc, UpsertExecutionProcessLarkWikiDoc,
    },
    execution_process_lark_wiki_month_node::{
        ExecutionProcessLarkWikiMonthNode, UpsertExecutionProcessLarkWikiMonthNode,
    },
};
use executors::{
    actions::{ExecutorAction, ExecutorActionType},
    logs::{ActionType, NormalizedEntry, NormalizedEntryType, ToolResultValueType, ToolStatus},
};
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::services::config::{Config, TelegramConfig};

const LARK_CLI_BIN: &str = "lark-cli";
const LARK_TITLE_MAX_CHARS: usize = 240;

#[derive(Debug, Clone)]
pub struct LarkWikiMirrorConfig {
    pub space_id: String,
}

impl LarkWikiMirrorConfig {
    pub fn from_telegram_config(config: &TelegramConfig) -> Option<Self> {
        if !config.enabled || !config.lark_wiki_enabled {
            return None;
        }

        Some(Self {
            space_id: normalized_optional_string(config.lark_wiki_space_id.as_deref())?,
        })
    }
}

pub fn normalize_lark_wiki_config(config: &mut Config) -> bool {
    let normalized = normalized_optional_string(config.telegram.lark_wiki_space_id.as_deref());
    if config.telegram.lark_wiki_space_id == normalized {
        return false;
    }

    config.telegram.lark_wiki_space_id = normalized;
    true
}

#[derive(Debug, Clone)]
pub struct LarkWikiDocMeta {
    pub doc_id: String,
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LarkWikiMonthNodeMeta {
    pub node_token: String,
    pub obj_token: String,
    pub title: String,
}

pub async fn get_execution_process_lark_wiki_url(
    pool: &SqlitePool,
    execution_process_id: &Uuid,
) -> Option<String> {
    match ExecutionProcessLarkWikiDoc::find_url_by_execution_process_id(pool, *execution_process_id)
        .await
    {
        Ok(url) => url,
        Err(err) => {
            tracing::warn!(
                "Failed to load Lark Wiki URL for execution_process_id={}: {}",
                execution_process_id,
                err
            );
            None
        }
    }
}

#[async_trait]
pub trait LarkWikiClient: Send + Sync {
    async fn list_root_nodes(&self, space_id: &str) -> Result<Vec<LarkWikiMonthNodeMeta>>;
    async fn create_month_node(&self, space_id: &str, month: &str)
    -> Result<LarkWikiMonthNodeMeta>;
    async fn create_doc(
        &self,
        parent_node_token: &str,
        title: &str,
        markdown: &str,
    ) -> Result<LarkWikiDocMeta>;
    async fn update_doc(
        &self,
        doc_id: &str,
        title: &str,
        markdown: &str,
    ) -> Result<LarkWikiDocMeta>;
}

#[derive(Default)]
struct LarkCliClient;

#[async_trait]
impl LarkWikiClient for LarkCliClient {
    async fn list_root_nodes(&self, space_id: &str) -> Result<Vec<LarkWikiMonthNodeMeta>> {
        let params = serde_json::json!({
            "space_id": space_id,
            "page_size": 50,
        })
        .to_string();
        let output = run_lark_cli(
            &[
                "wiki",
                "nodes",
                "list",
                "--as",
                "user",
                "--page-all",
                "--params",
                &params,
            ],
            None,
        )
        .await?;

        parse_lark_wiki_node_list(&output)
    }

    async fn create_month_node(
        &self,
        space_id: &str,
        month: &str,
    ) -> Result<LarkWikiMonthNodeMeta> {
        let output = run_lark_cli(
            &[
                "wiki",
                "+node-create",
                "--as",
                "user",
                "--space-id",
                space_id,
                "--title",
                month,
            ],
            None,
        )
        .await?;

        parse_lark_wiki_node_meta(&output, month)
    }

    async fn create_doc(
        &self,
        parent_node_token: &str,
        title: &str,
        markdown: &str,
    ) -> Result<LarkWikiDocMeta> {
        let output = run_lark_cli(
            &[
                "docs",
                "+create",
                "--as",
                "user",
                "--wiki-node",
                parent_node_token,
                "--title",
                title,
                "--markdown",
                "-",
            ],
            Some(markdown),
        )
        .await?;

        parse_lark_doc_meta(&output, title)
    }

    async fn update_doc(
        &self,
        doc_id: &str,
        title: &str,
        markdown: &str,
    ) -> Result<LarkWikiDocMeta> {
        let output = run_lark_cli(
            &[
                "docs",
                "+update",
                "--as",
                "user",
                "--doc",
                doc_id,
                "--mode",
                "append",
                "--new-title",
                title,
                "--markdown",
                "-",
            ],
            Some(markdown),
        )
        .await?;

        let mut meta = parse_lark_doc_meta(&output, title)?;
        if meta.doc_id.is_empty() {
            meta.doc_id = doc_id.to_string();
        }
        Ok(meta)
    }
}

async fn run_lark_cli(args: &[&str], stdin: Option<&str>) -> Result<String> {
    let mut child = tokio::process::Command::new(LARK_CLI_BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {LARK_CLI_BIN}"))?;

    if let Some(input) = stdin
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin
            .write_all(input.as_bytes())
            .await
            .context("failed to write to lark-cli stdin")?;
    }

    let output = child
        .wait_with_output()
        .await
        .context("failed to wait for lark-cli")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("lark-cli exited with {}: {}", output.status, stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn parse_lark_doc_meta(output: &str, fallback_title: &str) -> Result<LarkWikiDocMeta> {
    let value: Value =
        serde_json::from_str(output.trim()).context("failed to parse lark-cli JSON")?;
    let url = find_string_field(&value, &["doc_url", "url", "document_url"])
        .context("lark-cli response missing doc_url")?;
    let doc_id = find_string_field(&value, &["doc_id", "document_id", "token"])
        .or_else(|| extract_lark_url_token(&url))
        .context("lark-cli response missing doc_id")?;
    let title = find_string_field(&value, &["title"]).unwrap_or_else(|| fallback_title.to_string());

    Ok(LarkWikiDocMeta { doc_id, url, title })
}

pub fn parse_lark_wiki_node_meta(
    output: &str,
    fallback_title: &str,
) -> Result<LarkWikiMonthNodeMeta> {
    let value: Value =
        serde_json::from_str(output.trim()).context("failed to parse lark-cli JSON")?;
    find_wiki_node_meta(&value, fallback_title).context("lark-cli response missing wiki node")
}

pub fn parse_lark_wiki_node_list(output: &str) -> Result<Vec<LarkWikiMonthNodeMeta>> {
    let value: Value =
        serde_json::from_str(output.trim()).context("failed to parse lark-cli JSON")?;
    let mut nodes = Vec::new();
    collect_wiki_node_meta(&value, &mut nodes);
    Ok(nodes)
}

pub async fn append_stage_summary_markdown(
    pool: &SqlitePool,
    execution_process_id: Uuid,
    config: &TelegramConfig,
    entries: Vec<NormalizedEntry>,
) -> Option<String> {
    let mirror_config = LarkWikiMirrorConfig::from_telegram_config(config)?;
    let client = LarkCliClient;

    append_stage_summary_markdown_with_client(
        pool,
        execution_process_id,
        &mirror_config.space_id,
        &client,
        entries,
    )
    .await
}

async fn append_stage_summary_markdown_with_client(
    pool: &SqlitePool,
    execution_process_id: Uuid,
    space_id: &str,
    client: &dyn LarkWikiClient,
    entries: Vec<NormalizedEntry>,
) -> Option<String> {
    let ctx = match ExecutionProcess::load_context(pool, execution_process_id).await {
        Ok(ctx) => ctx,
        Err(err) => {
            tracing::warn!(
                "Failed to load execution context for Lark Wiki summary append execution {}: {}",
                execution_process_id,
                err
            );
            return None;
        }
    };
    let current_execution_doc =
        match ExecutionProcessLarkWikiDoc::find_by_execution_process_id(pool, execution_process_id)
            .await
        {
            Ok(doc) => doc,
            Err(err) => {
                tracing::warn!(
                    "Failed to find current Lark Wiki doc for execution {}: {}",
                    execution_process_id,
                    err
                );
                return None;
            }
        };
    if let Some(doc) = current_execution_doc {
        if !doc.is_pending_claim() {
            return Some(doc.url);
        }

        return find_session_lark_wiki_doc(pool, ctx.execution_process.session_id)
            .await
            .ok()
            .flatten()
            .map(|doc| doc.url);
    }
    let user_input = user_input_for_context(&ctx);
    let markdown = render_entries_markdown(entries, user_input.as_deref())?;
    let doc_title = doc_title_for_execution(
        &ctx.task.title,
        ctx.execution_process.executor_action().ok(),
        ctx.execution_process.started_at,
    );

    let existing_doc =
        match find_session_lark_wiki_doc(pool, ctx.execution_process.session_id).await {
            Ok(doc) => doc,
            Err(err) => {
                tracing::warn!(
                    "Failed to find existing Lark Wiki doc for session {}: {}",
                    ctx.execution_process.session_id,
                    err
                );
                return None;
            }
        };

    let existing_url = existing_doc.as_ref().map(|doc| doc.url.clone());
    match ExecutionProcessLarkWikiDoc::try_claim_append(pool, execution_process_id, &doc_title)
        .await
    {
        Ok(true) => {}
        Ok(false) => return existing_url,
        Err(err) => {
            tracing::warn!(
                "Failed to claim Lark Wiki append for execution {}: {}",
                execution_process_id,
                err
            );
            return existing_url;
        }
    }

    let update_result = match existing_doc {
        Some(doc) => client.update_doc(&doc.doc_id, &doc_title, &markdown).await,
        None => {
            let month_node = match resolve_lark_wiki_month_node(
                Some(pool),
                client,
                space_id,
                ctx.execution_process.started_at,
            )
            .await
            {
                Ok(month_node) => month_node,
                Err(err) => {
                    tracing::warn!(
                        "Failed to resolve Lark Wiki month node for execution {}: {}",
                        execution_process_id,
                        err
                    );
                    release_lark_wiki_pending_claim(pool, execution_process_id).await;
                    return existing_url;
                }
            };
            client
                .create_doc(&month_node.node_token, &doc_title, &markdown)
                .await
        }
    };

    let meta = match update_result {
        Ok(meta) => meta,
        Err(err) => {
            tracing::warn!(
                "Lark Wiki stage summary append failed for execution {}: {}",
                execution_process_id,
                err
            );
            release_lark_wiki_pending_claim(pool, execution_process_id).await;
            return existing_url;
        }
    };

    let doc = UpsertExecutionProcessLarkWikiDoc {
        doc_id: &meta.doc_id,
        url: &meta.url,
        title: &meta.title,
    };
    if let Err(err) = ExecutionProcessLarkWikiDoc::upsert(pool, execution_process_id, &doc).await {
        tracing::warn!(
            "Failed to persist Lark Wiki metadata for execution {}: {}",
            execution_process_id,
            err
        );
    }

    Some(meta.url)
}

async fn release_lark_wiki_pending_claim(pool: &SqlitePool, execution_process_id: Uuid) {
    if let Err(err) =
        ExecutionProcessLarkWikiDoc::release_pending_claim(pool, execution_process_id).await
    {
        tracing::warn!(
            "Failed to release Lark Wiki pending claim for execution {}: {}",
            execution_process_id,
            err
        );
    }
}

fn user_input_for_context(ctx: &ExecutionContext) -> Option<String> {
    let action = ctx.execution_process.executor_action().ok()?;
    match &action.typ {
        ExecutorActionType::CodingAgentInitialRequest(_) => {
            let title = &ctx.task.title;
            let detail = ctx.task.description.as_deref().unwrap_or("");
            let text = if detail.is_empty() {
                title.clone()
            } else {
                format!("{}\n{}", title, detail)
            };
            Some(text)
        }
        ExecutorActionType::CodingAgentFollowUpRequest(req) => Some(req.prompt.clone()),
        _ => None,
    }
}

async fn find_session_lark_wiki_doc(
    pool: &SqlitePool,
    session_id: Uuid,
) -> Result<Option<ExecutionProcessLarkWikiDoc>, sqlx::Error> {
    let processes = ExecutionProcess::find_by_session_id(pool, session_id, false).await?;
    for process in processes {
        if let Some(doc) =
            ExecutionProcessLarkWikiDoc::find_by_execution_process_id(pool, process.id).await?
            && !doc.is_pending_claim()
        {
            return Ok(Some(doc));
        }
    }

    Ok(None)
}

fn render_entries_markdown(
    entries: Vec<NormalizedEntry>,
    user_input: Option<&str>,
) -> Option<String> {
    let has_user_message = entries
        .iter()
        .any(|entry| matches!(entry.entry_type, NormalizedEntryType::UserMessage));
    let mut sections = Vec::new();

    if !has_user_message && let Some(input) = user_input.filter(|input| !input.trim().is_empty()) {
        sections.push(render_text_entry("User", input));
    }

    sections.extend(entries.iter().filter_map(render_entry_markdown));

    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n---\n\n"))
    }
}

fn find_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(text) = map.get(*key).and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    return Some(text.to_string());
                }
            }

            map.values()
                .find_map(|value| find_string_field(value, keys))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_string_field(value, keys)),
        _ => None,
    }
}

fn find_wiki_node_meta(value: &Value, fallback_title: &str) -> Option<LarkWikiMonthNodeMeta> {
    match value {
        Value::Object(map) => {
            if let Some(meta) = wiki_node_meta_from_object(map, fallback_title) {
                return Some(meta);
            }

            map.values()
                .find_map(|value| find_wiki_node_meta(value, fallback_title))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_wiki_node_meta(value, fallback_title)),
        _ => None,
    }
}

fn collect_wiki_node_meta(value: &Value, nodes: &mut Vec<LarkWikiMonthNodeMeta>) {
    match value {
        Value::Object(map) => {
            if let Some(meta) = wiki_node_meta_from_object(map, "") {
                nodes.push(meta);
            } else {
                for value in map.values() {
                    collect_wiki_node_meta(value, nodes);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_wiki_node_meta(value, nodes);
            }
        }
        _ => {}
    }
}

fn wiki_node_meta_from_object(
    map: &serde_json::Map<String, Value>,
    fallback_title: &str,
) -> Option<LarkWikiMonthNodeMeta> {
    let node_token = map
        .get("node_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())?
        .to_string();
    let obj_token = map
        .get("obj_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("")
        .to_string();
    let title = map
        .get("title")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback_title)
        .to_string();

    Some(LarkWikiMonthNodeMeta {
        node_token,
        obj_token,
        title,
    })
}

fn extract_lark_url_token(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    for marker in ["/wiki/", "/docx/", "/docs/"] {
        if let Some((_, token)) = trimmed.rsplit_once(marker) {
            let token = token.split(['?', '#', '/']).next().unwrap_or("");
            if !token.trim().is_empty() {
                return Some(token.to_string());
            }
        }
    }

    None
}

pub fn lark_wiki_month_for_started_at(started_at: DateTime<Utc>) -> String {
    started_at.with_timezone(&Local).format("%Y%m").to_string()
}

async fn resolve_lark_wiki_month_node(
    pool: Option<&SqlitePool>,
    client: &dyn LarkWikiClient,
    space_id: &str,
    started_at: DateTime<Utc>,
) -> Result<LarkWikiMonthNodeMeta> {
    let month = lark_wiki_month_for_started_at(started_at);

    if let Some(pool) = pool {
        if let Some(cached) =
            ExecutionProcessLarkWikiMonthNode::find_by_space_and_month(pool, space_id, &month)
                .await?
        {
            return Ok(LarkWikiMonthNodeMeta {
                node_token: cached.node_token,
                obj_token: cached.obj_token,
                title: cached.title,
            });
        }
    }

    let root_nodes = client.list_root_nodes(space_id).await?;
    let month_node =
        if let Some(month_node) = root_nodes.into_iter().find(|node| node.title == month) {
            month_node
        } else {
            client.create_month_node(space_id, &month).await?
        };

    if let Some(pool) = pool {
        let node = UpsertExecutionProcessLarkWikiMonthNode {
            node_token: &month_node.node_token,
            obj_token: &month_node.obj_token,
            title: &month_node.title,
        };
        ExecutionProcessLarkWikiMonthNode::upsert(pool, space_id, &month, &node).await?;
    }

    Ok(month_node)
}

pub fn doc_title_for_execution(
    task_title: &str,
    action: Option<&ExecutorAction>,
    started_at: DateTime<Utc>,
) -> String {
    let date = started_at
        .with_timezone(&Local)
        .format("%Y%m%d")
        .to_string();
    let title = normalized_optional_string(Some(task_title)).unwrap_or_else(|| "Execution".into());
    let (executor, variant) = executor_variant_display(action);
    truncate_chars(
        &format!("{date} {title} {executor} {variant}"),
        LARK_TITLE_MAX_CHARS,
    )
}

fn executor_variant_display(action: Option<&ExecutorAction>) -> (String, String) {
    let Some(action) = action else {
        return ("unknown".to_string(), "default".to_string());
    };

    let profile = match action.typ() {
        ExecutorActionType::CodingAgentInitialRequest(request) => {
            Some(&request.executor_profile_id)
        }
        ExecutorActionType::CodingAgentFollowUpRequest(request) => {
            Some(&request.executor_profile_id)
        }
        ExecutorActionType::ReviewRequest(request) => Some(&request.executor_profile_id),
        ExecutorActionType::ScriptRequest(_) => None,
    };

    profile
        .map(|profile| {
            (
                profile.executor.to_string().to_ascii_lowercase(),
                profile
                    .variant
                    .as_deref()
                    .unwrap_or("DEFAULT")
                    .to_ascii_lowercase(),
            )
        })
        .unwrap_or_else(|| ("unknown".to_string(), "default".to_string()))
}

fn render_entry_markdown(entry: &NormalizedEntry) -> Option<String> {
    match &entry.entry_type {
        NormalizedEntryType::UserMessage => Some(render_text_entry("User", &entry.content)),
        NormalizedEntryType::AssistantMessage => {
            Some(render_text_entry("Assistant", &entry.content))
        }
        NormalizedEntryType::SystemMessage => Some(render_text_entry("System", &entry.content)),
        NormalizedEntryType::Thinking => Some(render_text_entry("Thinking", &entry.content)),
        NormalizedEntryType::UserFeedback { denied_tool } => Some(render_text_entry(
            &format!("Denied By User: {denied_tool}"),
            &entry.content,
        )),
        NormalizedEntryType::ErrorMessage { .. } => {
            Some(format!("## Error\n\n{}", fenced_code(&entry.content)))
        }
        NormalizedEntryType::ToolUse {
            tool_name,
            action_type,
            status,
        } => Some(render_tool_use_markdown(tool_name, action_type, status)),
        NormalizedEntryType::NextAction {
            failed,
            execution_processes,
            needs_setup,
        } => Some(format!(
            "## Next Action\n\n- failed: `{failed}`\n- execution_processes: `{execution_processes}`\n- needs_setup: `{needs_setup}`"
        )),
        NormalizedEntryType::Loading | NormalizedEntryType::TokenUsageInfo(_) => None,
    }
}

fn render_text_entry(title: &str, content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        format!("## {title}")
    } else {
        format!("## {title}\n\n{trimmed}")
    }
}

fn render_tool_use_markdown(
    tool_name: &str,
    action_type: &ActionType,
    status: &ToolStatus,
) -> String {
    let mut out = format!("## {} [{}]\n", tool_name.trim(), tool_status_label(status));

    match action_type {
        ActionType::FileRead { path } => push_labeled_code(&mut out, "Path", path),
        ActionType::FileEdit { path, changes } => {
            push_labeled_code(&mut out, "Path", path);
            for change in changes {
                match change {
                    executors::logs::FileChange::Write { content } => {
                        push_labeled_code(&mut out, "Write", content)
                    }
                    executors::logs::FileChange::Delete => push_label(&mut out, "Delete"),
                    executors::logs::FileChange::Rename { new_path } => {
                        push_labeled_code(&mut out, "Rename To", new_path)
                    }
                    executors::logs::FileChange::Edit { unified_diff, .. } => {
                        push_labeled_code(&mut out, "Diff", unified_diff)
                    }
                }
            }
        }
        ActionType::CommandRun { command, result } => {
            push_labeled_code(&mut out, "Args", command);
            if let Some(result) = result {
                if let Some(exit_status) = &result.exit_status {
                    out.push_str(&format!(
                        "\n**Exit status:** `{}`\n",
                        format_exit_status(exit_status)
                    ));
                }
                if let Some(output) = result.output.as_deref()
                    && !tool_name.eq_ignore_ascii_case("bash")
                {
                    push_labeled_code(&mut out, "Output", output);
                }
            }
        }
        ActionType::Search { query } => push_labeled_code(&mut out, "Query", query),
        ActionType::WebFetch { url } => {
            out.push_str(&format!("\n**URL:** <{url}>\n"));
        }
        ActionType::Tool {
            arguments, result, ..
        } => {
            if let Some(arguments) = arguments {
                push_labeled_code(&mut out, "Args", &pretty_json(arguments));
            }
            if let Some(result) = result {
                match result.r#type {
                    ToolResultValueType::Markdown => {
                        if let Some(markdown) = result.value.as_str() {
                            out.push_str("\n**Result:**\n\n");
                            out.push_str(markdown.trim());
                            out.push('\n');
                        } else {
                            push_labeled_code(&mut out, "Result", &pretty_json(&result.value));
                        }
                    }
                    ToolResultValueType::Json => {
                        push_labeled_code(&mut out, "Result", &pretty_json(&result.value));
                    }
                }
            }
        }
        ActionType::TaskCreate { description }
        | ActionType::PlanPresentation { plan: description }
        | ActionType::Other { description } => {
            out.push('\n');
            out.push_str(description.trim());
            out.push('\n');
        }
        ActionType::TodoManagement { todos, operation } => {
            out.push_str(&format!("\n**Operation:** `{operation}`\n"));
            for todo in todos {
                let priority = todo
                    .priority
                    .as_ref()
                    .map(|priority| format!(" ({priority})"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "- [{}] {}{}\n",
                    todo.status, todo.content, priority
                ));
            }
        }
    }

    out.trim_end().to_string()
}

fn push_label(out: &mut String, label: &str) {
    out.push_str(&format!("\n**{label}**\n"));
}

fn push_labeled_code(out: &mut String, label: &str, text: &str) {
    out.push_str(&format!("\n**{label}:**\n\n{}\n", fenced_code(text)));
}

pub fn fenced_code(text: &str) -> String {
    let max_run = max_backtick_run(text);
    let fence = "`".repeat((max_run + 1).max(3));
    format!("{fence}\n{text}\n{fence}")
}

fn max_backtick_run(text: &str) -> usize {
    let mut max = 0usize;
    let mut current = 0usize;
    for ch in text.chars() {
        if ch == '`' {
            current += 1;
            max = max.max(current);
        } else {
            current = 0;
        }
    }
    max
}

fn tool_status_label(status: &ToolStatus) -> &'static str {
    match status {
        ToolStatus::Created => "created",
        ToolStatus::Success => "success",
        ToolStatus::Failed => "failed",
        ToolStatus::Denied { .. } => "denied",
        ToolStatus::PendingApproval { .. } => "pending_approval",
        ToolStatus::TimedOut => "timed_out",
    }
}

fn format_exit_status(status: &executors::logs::CommandExitStatus) -> String {
    match status {
        executors::logs::CommandExitStatus::ExitCode { code } => format!("exit_code({code})"),
        executors::logs::CommandExitStatus::Success { success } => {
            format!("success({success})")
        }
    }
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }

    input.chars().take(max_chars).collect()
}

fn normalized_optional_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use executors::{
        actions::{
            ExecutorAction, coding_agent_follow_up::CodingAgentFollowUpRequest,
            coding_agent_initial::CodingAgentInitialRequest,
        },
        executors::BaseCodingAgent,
        profile::ExecutorProfileId,
    };

    use super::*;

    #[derive(Default)]
    struct MockLarkWikiClient {
        creates: StdMutex<Vec<(String, String, String)>>,
        updates: StdMutex<Vec<(String, String, String)>>,
        root_nodes: StdMutex<Vec<LarkWikiMonthNodeMeta>>,
        month_creates: StdMutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl LarkWikiClient for MockLarkWikiClient {
        async fn list_root_nodes(&self, _space_id: &str) -> Result<Vec<LarkWikiMonthNodeMeta>> {
            Ok(self.root_nodes.lock().unwrap().clone())
        }

        async fn create_month_node(
            &self,
            space_id: &str,
            month: &str,
        ) -> Result<LarkWikiMonthNodeMeta> {
            self.month_creates
                .lock()
                .unwrap()
                .push((space_id.to_string(), month.to_string()));
            Ok(LarkWikiMonthNodeMeta {
                node_token: format!("node-{month}"),
                obj_token: format!("obj-{month}"),
                title: month.to_string(),
            })
        }

        async fn create_doc(
            &self,
            parent_node_token: &str,
            title: &str,
            markdown: &str,
        ) -> Result<LarkWikiDocMeta> {
            self.creates.lock().unwrap().push((
                parent_node_token.to_string(),
                title.to_string(),
                markdown.to_string(),
            ));
            Ok(LarkWikiDocMeta {
                doc_id: "doc-1".to_string(),
                url: "https://lark.example/doc-1".to_string(),
                title: title.to_string(),
            })
        }

        async fn update_doc(
            &self,
            doc_id: &str,
            title: &str,
            markdown: &str,
        ) -> Result<LarkWikiDocMeta> {
            self.updates.lock().unwrap().push((
                doc_id.to_string(),
                title.to_string(),
                markdown.to_string(),
            ));
            Ok(LarkWikiDocMeta {
                doc_id: doc_id.to_string(),
                url: "https://lark.example/doc-1".to_string(),
                title: title.to_string(),
            })
        }
    }

    #[test]
    fn mirror_config_requires_enabled_telegram_lark_and_space() {
        let mut config = TelegramConfig::default();
        assert!(LarkWikiMirrorConfig::from_telegram_config(&config).is_none());

        config.enabled = true;
        config.lark_wiki_enabled = true;
        config.lark_wiki_space_id = Some("  space-1  ".to_string());
        let mirror = LarkWikiMirrorConfig::from_telegram_config(&config).unwrap();
        assert_eq!(mirror.space_id, "space-1");
    }

    #[test]
    fn stage_summary_markdown_prepends_user_input_when_missing_from_entries() {
        let markdown = render_entries_markdown(
            vec![NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::AssistantMessage,
                content: "done".to_string(),
                metadata: None,
            }],
            Some("initial task"),
        )
        .expect("markdown should render");

        assert!(markdown.starts_with("## User\n\ninitial task"));
        assert!(markdown.contains("## Assistant\n\ndone"));
    }

    #[test]
    fn lark_title_uses_local_date_task_executor_and_variant() {
        let action = ExecutorAction::new(
            ExecutorActionType::CodingAgentInitialRequest(CodingAgentInitialRequest {
                prompt: "hello".to_string(),
                executor_profile_id: ExecutorProfileId::with_variant(
                    BaseCodingAgent::Codex,
                    "PLAN".to_string(),
                ),
                working_dir: None,
            }),
            None,
        );
        let started_at = DateTime::parse_from_rfc3339("2026-04-25T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let title = doc_title_for_execution("feat: add xxx", Some(&action), started_at);

        assert!(title.ends_with(" feat: add xxx codex plan"));
        assert!(title.starts_with("20260425 "));
    }

    #[test]
    fn fenced_code_uses_longer_fence_than_content() {
        let rendered = fenced_code("a\n```nested```\n````");
        assert!(rendered.starts_with("`````\n"));
        assert!(rendered.ends_with("\n`````"));
    }

    #[test]
    fn parses_top_level_and_nested_lark_json() {
        let top = parse_lark_doc_meta(
            r#"{"doc_id":"d1","doc_url":"https://x","title":"T"}"#,
            "fallback",
        )
        .unwrap();
        assert_eq!(top.doc_id, "d1");
        assert_eq!(top.url, "https://x");
        assert_eq!(top.title, "T");

        let nested = parse_lark_doc_meta(
            r#"{"data":{"document":{"doc_id":"d2","doc_url":"https://y"}}}"#,
            "fallback",
        )
        .unwrap();
        assert_eq!(nested.doc_id, "d2");
        assert_eq!(nested.url, "https://y");
        assert_eq!(nested.title, "fallback");

        let wiki_url = parse_lark_doc_meta(
            r#"{"doc_url":"https://example.feishu.cn/wiki/wiki-token","title":"T"}"#,
            "fallback",
        )
        .unwrap();
        assert_eq!(wiki_url.doc_id, "wiki-token");
    }

    #[test]
    fn parses_wiki_node_responses() {
        let created = parse_lark_wiki_node_meta(
            r#"{"data":{"node":{"node_token":"node-1","obj_token":"obj-1","title":"202604"}}}"#,
            "fallback",
        )
        .unwrap();
        assert_eq!(
            created,
            LarkWikiMonthNodeMeta {
                node_token: "node-1".to_string(),
                obj_token: "obj-1".to_string(),
                title: "202604".to_string(),
            }
        );

        let listed = parse_lark_wiki_node_list(
            r#"{"data":{"items":[{"node_token":"node-1","obj_token":"obj-1","title":"202603"},{"node_token":"node-2","obj_token":"obj-2","title":"202604"}]}}"#,
        )
        .unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[1].title, "202604");
    }

    #[test]
    fn lark_wiki_month_uses_execution_started_at_local_time() {
        let started_at = DateTime::parse_from_rfc3339("2026-04-25T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        assert_eq!(lark_wiki_month_for_started_at(started_at), "202604");
    }

    async fn create_month_nodes_table(pool: &SqlitePool) {
        sqlx::query(
            r#"CREATE TABLE execution_process_lark_wiki_month_nodes (
                space_id TEXT NOT NULL,
                month TEXT NOT NULL,
                node_token TEXT NOT NULL,
                obj_token TEXT NOT NULL,
                title TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (space_id, month)
            )"#,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn month_node_cache_hit_skips_lark_cli_resolution() {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        create_month_nodes_table(&pool).await;
        ExecutionProcessLarkWikiMonthNode::upsert(
            &pool,
            "space-1",
            "202604",
            &UpsertExecutionProcessLarkWikiMonthNode {
                node_token: "cached-node",
                obj_token: "cached-obj",
                title: "202604",
            },
        )
        .await
        .unwrap();
        let client = MockLarkWikiClient::default();
        let started_at = DateTime::parse_from_rfc3339("2026-04-25T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let node = resolve_lark_wiki_month_node(Some(&pool), &client, "space-1", started_at)
            .await
            .unwrap();

        assert_eq!(node.node_token, "cached-node");
        assert!(client.month_creates.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn month_node_root_list_hit_is_cached() {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        create_month_nodes_table(&pool).await;
        let client = MockLarkWikiClient::default();
        client
            .root_nodes
            .lock()
            .unwrap()
            .push(LarkWikiMonthNodeMeta {
                node_token: "listed-node".to_string(),
                obj_token: "listed-obj".to_string(),
                title: "202604".to_string(),
            });
        let started_at = DateTime::parse_from_rfc3339("2026-04-25T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let node = resolve_lark_wiki_month_node(Some(&pool), &client, "space-1", started_at)
            .await
            .unwrap();

        assert_eq!(node.node_token, "listed-node");
        assert!(client.month_creates.lock().unwrap().is_empty());
        let cached =
            ExecutionProcessLarkWikiMonthNode::find_by_space_and_month(&pool, "space-1", "202604")
                .await
                .unwrap()
                .unwrap();
        assert_eq!(cached.node_token, "listed-node");
    }

    #[tokio::test]
    async fn month_node_list_miss_creates_and_caches() {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        create_month_nodes_table(&pool).await;
        let client = MockLarkWikiClient::default();
        let started_at = DateTime::parse_from_rfc3339("2026-04-25T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let node = resolve_lark_wiki_month_node(Some(&pool), &client, "space-1", started_at)
            .await
            .unwrap();

        assert_eq!(node.node_token, "node-202604");
        assert_eq!(
            client.month_creates.lock().unwrap().as_slice(),
            &[("space-1".to_string(), "202604".to_string())]
        );
        let cached =
            ExecutionProcessLarkWikiMonthNode::find_by_space_and_month(&pool, "space-1", "202604")
                .await
                .unwrap()
                .unwrap();
        assert_eq!(cached.node_token, "node-202604");
    }

    async fn create_lark_summary_test_schema(pool: &SqlitePool) {
        for sql in [
            r#"CREATE TABLE projects (
                id BLOB PRIMARY KEY,
                name TEXT NOT NULL,
                default_agent_working_dir TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )"#,
            r#"CREATE TABLE tasks (
                id BLOB PRIMARY KEY,
                project_id BLOB NOT NULL,
                title TEXT NOT NULL,
                description TEXT,
                status TEXT NOT NULL,
                parent_workspace_id BLOB,
                source_cron_task_id BLOB,
                diff_additions INTEGER,
                diff_deletions INTEGER,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )"#,
            r#"CREATE TABLE workspaces (
                id BLOB PRIMARY KEY,
                task_id BLOB NOT NULL,
                container_ref TEXT,
                branch TEXT NOT NULL DEFAULT 'main',
                agent_working_dir TEXT,
                setup_completed_at TEXT,
                archived INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0,
                name TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )"#,
            r#"CREATE TABLE sessions (
                id BLOB PRIMARY KEY,
                workspace_id BLOB NOT NULL,
                executor TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )"#,
            r#"CREATE TABLE execution_processes (
                id BLOB PRIMARY KEY,
                session_id BLOB NOT NULL,
                run_reason TEXT NOT NULL,
                executor_action TEXT NOT NULL,
                status TEXT NOT NULL,
                exit_code INTEGER,
                dropped INTEGER NOT NULL DEFAULT 0,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )"#,
            r#"CREATE TABLE repos (
                id BLOB PRIMARY KEY,
                path TEXT NOT NULL,
                name TEXT NOT NULL,
                display_name TEXT NOT NULL,
                setup_script TEXT,
                cleanup_script TEXT,
                copy_files TEXT,
                parallel_setup_script INTEGER NOT NULL DEFAULT 0,
                dev_server_script TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )"#,
            r#"CREATE TABLE workspace_repos (
                id BLOB PRIMARY KEY,
                workspace_id BLOB NOT NULL,
                repo_id BLOB NOT NULL,
                target_branch TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )"#,
            r#"CREATE TABLE execution_process_lark_wiki_docs (
                execution_process_id BLOB PRIMARY KEY NOT NULL,
                doc_id TEXT NOT NULL,
                url TEXT NOT NULL,
                title TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )"#,
        ] {
            sqlx::query(sql).execute(pool).await.unwrap();
        }
        create_month_nodes_table(pool).await;
    }

    async fn insert_lark_summary_test_context(
        pool: &SqlitePool,
        session_id: Uuid,
        execution_ids: &[Uuid],
    ) {
        let now = Utc::now();
        let project_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();

        sqlx::query("INSERT INTO projects (id, name, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind(project_id)
            .bind("Project")
            .bind(now)
            .bind(now)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO tasks (id, project_id, title, description, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(task_id)
        .bind(project_id)
        .bind("Task title")
        .bind("Task detail")
        .bind("todo")
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workspaces (id, task_id, branch, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(workspace_id)
        .bind(task_id)
        .bind("main")
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (id, workspace_id, executor, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(workspace_id)
        .bind("GEMINI")
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        for (idx, execution_id) in execution_ids.iter().enumerate() {
            let action = if idx == 0 {
                ExecutorAction::new(
                    ExecutorActionType::CodingAgentInitialRequest(CodingAgentInitialRequest {
                        prompt: "initial prompt".to_string(),
                        executor_profile_id: ExecutorProfileId {
                            executor: BaseCodingAgent::Gemini,
                            variant: None,
                        },
                        working_dir: None,
                    }),
                    None,
                )
            } else {
                ExecutorAction::new(
                    ExecutorActionType::CodingAgentFollowUpRequest(CodingAgentFollowUpRequest {
                        prompt: "follow-up prompt".to_string(),
                        session_id: "agent-session".to_string(),
                        executor_profile_id: ExecutorProfileId {
                            executor: BaseCodingAgent::Gemini,
                            variant: None,
                        },
                        working_dir: None,
                    }),
                    None,
                )
            };
            sqlx::query(
                "INSERT INTO execution_processes (
                    id, session_id, run_reason, executor_action, status, dropped,
                    started_at, created_at, updated_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(*execution_id)
            .bind(session_id)
            .bind("codingagent")
            .bind(serde_json::to_string(&action).unwrap())
            .bind("completed")
            .bind(false)
            .bind(now + chrono::Duration::seconds(idx as i64))
            .bind(now + chrono::Duration::seconds(idx as i64))
            .bind(now + chrono::Duration::seconds(idx as i64))
            .execute(pool)
            .await
            .unwrap();
        }
    }

    fn assistant_entry(content: &str) -> NormalizedEntry {
        NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::AssistantMessage,
            content: content.to_string(),
            metadata: None,
        }
    }

    #[tokio::test]
    async fn final_write_creates_doc_with_full_normalized_entries_and_returns_url() {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        let session_id = Uuid::new_v4();
        let exec1 = Uuid::new_v4();
        create_lark_summary_test_schema(&pool).await;
        insert_lark_summary_test_context(&pool, session_id, &[exec1]).await;
        let client = MockLarkWikiClient::default();

        let url = append_stage_summary_markdown_with_client(
            &pool,
            exec1,
            "space-1",
            &client,
            vec![assistant_entry("first"), assistant_entry("final")],
        )
        .await;

        assert_eq!(url.as_deref(), Some("https://lark.example/doc-1"));
        let creates = client.creates.lock().unwrap();
        assert_eq!(creates.len(), 1);
        assert!(creates[0].2.contains("first"));
        assert!(creates[0].2.contains("final"));
        assert_eq!(
            get_execution_process_lark_wiki_url(&pool, &exec1)
                .await
                .as_deref(),
            Some("https://lark.example/doc-1")
        );
    }

    #[tokio::test]
    async fn final_write_reuses_existing_session_doc_for_follow_up_execution() {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        let session_id = Uuid::new_v4();
        let exec1 = Uuid::new_v4();
        let exec2 = Uuid::new_v4();
        create_lark_summary_test_schema(&pool).await;
        insert_lark_summary_test_context(&pool, session_id, &[exec1, exec2]).await;
        let client = MockLarkWikiClient::default();

        let first_url = append_stage_summary_markdown_with_client(
            &pool,
            exec1,
            "space-1",
            &client,
            vec![assistant_entry("first")],
        )
        .await;
        let second_url = append_stage_summary_markdown_with_client(
            &pool,
            exec2,
            "space-1",
            &client,
            vec![assistant_entry("second")],
        )
        .await;

        assert_eq!(first_url.as_deref(), Some("https://lark.example/doc-1"));
        assert_eq!(second_url.as_deref(), Some("https://lark.example/doc-1"));
        assert_eq!(client.creates.lock().unwrap().len(), 1);
        let updates = client.updates.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, "doc-1");
        assert!(updates[0].2.contains("second"));
        assert_eq!(
            get_execution_process_lark_wiki_url(&pool, &exec2)
                .await
                .as_deref(),
            Some("https://lark.example/doc-1")
        );
    }

    #[tokio::test]
    async fn final_write_skips_duplicate_append_when_current_execution_has_url() {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        let session_id = Uuid::new_v4();
        let exec1 = Uuid::new_v4();
        create_lark_summary_test_schema(&pool).await;
        insert_lark_summary_test_context(&pool, session_id, &[exec1]).await;
        let doc = UpsertExecutionProcessLarkWikiDoc {
            doc_id: "doc-existing",
            url: "https://lark.example/existing",
            title: "existing",
        };
        ExecutionProcessLarkWikiDoc::upsert(&pool, exec1, &doc)
            .await
            .unwrap();
        let client = MockLarkWikiClient::default();

        let url = append_stage_summary_markdown_with_client(
            &pool,
            exec1,
            "space-1",
            &client,
            vec![assistant_entry("duplicate")],
        )
        .await;

        assert_eq!(url.as_deref(), Some("https://lark.example/existing"));
        assert!(client.creates.lock().unwrap().is_empty());
        assert!(client.updates.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn final_write_skips_append_when_current_execution_is_pending_claimed() {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        let session_id = Uuid::new_v4();
        let exec1 = Uuid::new_v4();
        let exec2 = Uuid::new_v4();
        create_lark_summary_test_schema(&pool).await;
        insert_lark_summary_test_context(&pool, session_id, &[exec1, exec2]).await;
        let doc = UpsertExecutionProcessLarkWikiDoc {
            doc_id: "doc-existing",
            url: "https://lark.example/existing",
            title: "existing",
        };
        ExecutionProcessLarkWikiDoc::upsert(&pool, exec1, &doc)
            .await
            .unwrap();
        assert!(
            ExecutionProcessLarkWikiDoc::try_claim_append(&pool, exec2, "pending")
                .await
                .unwrap()
        );
        let client = MockLarkWikiClient::default();

        let url = append_stage_summary_markdown_with_client(
            &pool,
            exec2,
            "space-1",
            &client,
            vec![assistant_entry("second")],
        )
        .await;

        assert_eq!(url.as_deref(), Some("https://lark.example/existing"));
        assert!(client.creates.lock().unwrap().is_empty());
        assert!(client.updates.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn final_write_returns_url_for_telegram_tail_line() {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        let session_id = Uuid::new_v4();
        let exec1 = Uuid::new_v4();
        let exec2 = Uuid::new_v4();
        create_lark_summary_test_schema(&pool).await;
        insert_lark_summary_test_context(&pool, session_id, &[exec1, exec2]).await;
        let doc = UpsertExecutionProcessLarkWikiDoc {
            doc_id: "doc-existing",
            url: "https://lark.example/existing",
            title: "existing",
        };
        ExecutionProcessLarkWikiDoc::upsert(&pool, exec1, &doc)
            .await
            .unwrap();
        let client = MockLarkWikiClient::default();

        let url = append_stage_summary_markdown_with_client(
            &pool,
            exec2,
            "missing-space",
            &client,
            vec![assistant_entry("second")],
        )
        .await;

        assert_eq!(url.as_deref(), Some("https://lark.example/doc-1"));
    }
}
