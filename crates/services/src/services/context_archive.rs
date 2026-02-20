use std::{
    collections::{BTreeMap, BTreeSet},
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, anyhow};
use chrono::Utc;
use db::models::{
    coding_agent_turn::CodingAgentTurn, execution_process::ExecutionProcess, project::Project,
    task::Task, workspace::Workspace,
};
use executors::logs::{ActionType, NormalizedEntry, NormalizedEntryType, ToolStatus};
use serde_json::{Map, Value, json};
use sqlx::SqlitePool;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const CONTEXT_FILE_NAME: &str = "CONTEXT.md";
const META_FILE_NAME: &str = "meta.json";
const META_SCHEMA_VERSION: u64 = 1;
const MAX_CONVERSATION_ENTRIES: usize = 8;
const MAX_TOOL_ENTRIES: usize = 16;
const MAX_TEXT_CHARS: usize = 1_500;

#[derive(Debug, Clone)]
pub struct ArchiveFiles {
    pub archive_dir: PathBuf,
    pub context_path: PathBuf,
    pub meta_path: PathBuf,
    pub archive_task_id: Uuid,
}

pub fn archive_root_dir() -> PathBuf {
    utils::assets::asset_dir().join("archive")
}

pub fn archive_task_id(task_id: Uuid, parent_task_id: Option<Uuid>) -> Uuid {
    parent_task_id.unwrap_or(task_id)
}

pub async fn resolve_archive_task_id(pool: &SqlitePool, task: &Task) -> anyhow::Result<Uuid> {
    let parent_task_id = if let Some(parent_workspace_id) = task.parent_workspace_id {
        Workspace::find_by_id(pool, parent_workspace_id)
            .await
            .with_context(|| {
                format!(
                    "failed to resolve parent workspace {} for task {}",
                    parent_workspace_id, task.id
                )
            })?
            .map(|workspace| workspace.task_id)
    } else {
        None
    };

    Ok(archive_task_id(task.id, parent_task_id))
}

pub async fn ensure_archive_files(
    pool: &SqlitePool,
    project: &Project,
    task: &Task,
    branch_name: &str,
) -> anyhow::Result<ArchiveFiles> {
    let archive_task_id = resolve_archive_task_id(pool, task).await?;
    let archive_dir = archive_root_dir()
        .join(&project.name)
        .join(archive_task_id.to_string())
        .join(branch_name);

    tokio::fs::create_dir_all(&archive_dir)
        .await
        .with_context(|| {
            format!(
                "failed to create archive directory {}",
                archive_dir.display()
            )
        })?;

    let context_path = archive_dir.join(CONTEXT_FILE_NAME);
    if tokio::fs::metadata(&context_path).await.is_err() {
        tokio::fs::write(&context_path, "").await.with_context(|| {
            format!(
                "failed to initialize context file {}",
                context_path.display()
            )
        })?;
    }

    let meta_path = archive_dir.join(META_FILE_NAME);
    if tokio::fs::metadata(&meta_path).await.is_err() {
        write_json_file(
            &meta_path,
            &json!({
                "schema_version": META_SCHEMA_VERSION,
            }),
        )
        .await?;
    }

    Ok(ArchiveFiles {
        archive_dir,
        context_path,
        meta_path,
        archive_task_id,
    })
}

pub async fn upsert_meta(meta_path: &Path, patch: Value) -> anyhow::Result<Value> {
    let Some(patch_obj) = patch.as_object() else {
        return Err(anyhow!("meta patch must be an object"));
    };

    let mut meta = read_meta_map(meta_path).await?;
    meta.insert(
        "schema_version".to_string(),
        Value::from(META_SCHEMA_VERSION),
    );

    for (key, value) in patch_obj {
        meta.insert(key.clone(), value.clone());
    }

    let merged = Value::Object(meta);
    write_json_file(meta_path, &merged).await?;
    Ok(merged)
}

pub async fn append_execution_context(
    pool: &SqlitePool,
    execution_id: Uuid,
    normalized_entries: &[NormalizedEntry],
) -> anyhow::Result<()> {
    let ctx = ExecutionProcess::load_context(pool, execution_id)
        .await
        .with_context(|| format!("failed to load execution context {}", execution_id))?;
    let archive_files = ensure_archive_files(pool, &ctx.project, &ctx.task, &ctx.workspace.branch)
        .await
        .with_context(|| {
            format!(
                "failed to ensure archive files for execution {} workspace {}",
                execution_id, ctx.workspace.id
            )
        })?;

    let turn = CodingAgentTurn::find_by_execution_process_id(pool, execution_id)
        .await
        .with_context(|| format!("failed to load coding agent turn for {execution_id}"))?;

    let section = build_execution_section(
        execution_id,
        &ctx.workspace.branch,
        &ctx.execution_process.run_reason,
        &ctx.execution_process.status,
        turn.as_ref().and_then(|t| t.prompt.as_deref()),
        turn.as_ref().and_then(|t| t.summary.as_deref()),
        normalized_entries,
    );

    let base_patch = base_meta_patch(
        &ctx.project,
        &ctx.task,
        archive_files.archive_task_id,
        &ctx.workspace.branch,
    );

    append_context_section(
        &archive_files,
        execution_id,
        &section,
        Value::Object(base_patch),
    )
    .await?;

    Ok(())
}

pub async fn mark_task_done_diff(
    pool: &SqlitePool,
    task: &Task,
    branch_name: &str,
) -> anyhow::Result<()> {
    let project = task
        .parent_project(pool)
        .await
        .with_context(|| format!("failed to load project for task {}", task.id))?
        .ok_or_else(|| anyhow!("project not found for task {}", task.id))?;

    let archive_files = ensure_archive_files(pool, &project, task, branch_name).await?;

    let mut patch = base_meta_patch(&project, task, archive_files.archive_task_id, branch_name);
    patch.insert(
        "task_done_at".to_string(),
        Value::String(task.updated_at.to_rfc3339()),
    );
    patch.insert("diff_additions".to_string(), json!(task.diff_additions));
    patch.insert("diff_deletions".to_string(), json!(task.diff_deletions));

    upsert_meta(&archive_files.meta_path, Value::Object(patch)).await?;
    Ok(())
}

fn base_meta_patch(
    project: &Project,
    task: &Task,
    archive_task_id: Uuid,
    branch_name: &str,
) -> Map<String, Value> {
    let mut patch = Map::new();
    patch.insert(
        "schema_version".to_string(),
        Value::from(META_SCHEMA_VERSION),
    );
    patch.insert(
        "project_id".to_string(),
        Value::String(project.id.to_string()),
    );
    patch.insert(
        "project_name".to_string(),
        Value::String(project.name.clone()),
    );
    patch.insert("task_id".to_string(), Value::String(task.id.to_string()));
    patch.insert(
        "archive_task_id".to_string(),
        Value::String(archive_task_id.to_string()),
    );
    patch.insert(
        "branch_name".to_string(),
        Value::String(branch_name.to_string()),
    );
    patch.insert(
        "task_status".to_string(),
        Value::String(task.status.to_string()),
    );
    patch.insert(
        "updated_at".to_string(),
        Value::String(Utc::now().to_rfc3339()),
    );
    patch
}

async fn append_context_section(
    archive_files: &ArchiveFiles,
    execution_id: Uuid,
    section_body: &str,
    base_patch: Value,
) -> anyhow::Result<bool> {
    let execution_id_str = execution_id.to_string();
    let marker = execution_marker(execution_id);

    let mut meta = read_meta_map(&archive_files.meta_path).await?;
    let mut execution_ids = execution_ids_from_meta(&meta);

    let existing_context = match tokio::fs::read_to_string(&archive_files.context_path).await {
        Ok(content) => content,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "failed to read archive context {}",
                    archive_files.context_path.display()
                )
            });
        }
    };

    if execution_ids.contains(&execution_id_str) || existing_context.contains(&marker) {
        execution_ids.insert(execution_id_str);
        merge_meta_patch(
            &mut meta,
            base_patch,
            execution_ids.into_iter().collect::<Vec<_>>(),
        )?;
        write_json_file(&archive_files.meta_path, &Value::Object(meta)).await?;
        return Ok(false);
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&archive_files.context_path)
        .await
        .with_context(|| {
            format!(
                "failed to open context file {} for append",
                archive_files.context_path.display()
            )
        })?;

    if !existing_context.trim().is_empty() {
        file.write_all(b"\n\n").await.with_context(|| {
            format!(
                "failed to append separator to {}",
                archive_files.context_path.display()
            )
        })?;
    }

    file.write_all(marker.as_bytes()).await.with_context(|| {
        format!(
            "failed to append marker to {}",
            archive_files.context_path.display()
        )
    })?;
    file.write_all(b"\n").await.with_context(|| {
        format!(
            "failed to append newline to {}",
            archive_files.context_path.display()
        )
    })?;
    file.write_all(section_body.as_bytes())
        .await
        .with_context(|| {
            format!(
                "failed to append section to {}",
                archive_files.context_path.display()
            )
        })?;

    execution_ids.insert(execution_id_str);
    merge_meta_patch(
        &mut meta,
        base_patch,
        execution_ids.into_iter().collect::<Vec<_>>(),
    )?;

    write_json_file(&archive_files.meta_path, &Value::Object(meta)).await?;
    Ok(true)
}

fn merge_meta_patch(
    meta: &mut Map<String, Value>,
    patch: Value,
    execution_ids: Vec<String>,
) -> anyhow::Result<()> {
    let Some(patch_obj) = patch.as_object() else {
        return Err(anyhow!("meta patch must be an object"));
    };

    meta.insert(
        "schema_version".to_string(),
        Value::from(META_SCHEMA_VERSION),
    );
    for (key, value) in patch_obj {
        meta.insert(key.clone(), value.clone());
    }
    meta.insert(
        "archived_execution_ids".to_string(),
        Value::Array(
            execution_ids
                .into_iter()
                .map(Value::String)
                .collect::<Vec<_>>(),
        ),
    );

    Ok(())
}

fn execution_ids_from_meta(meta: &Map<String, Value>) -> BTreeSet<String> {
    meta.get("archived_execution_ids")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default()
}

fn execution_marker(execution_id: Uuid) -> String {
    format!("<!-- execution_id:{execution_id} -->")
}

fn build_execution_section(
    execution_id: Uuid,
    branch_name: &str,
    run_reason: &db::models::execution_process::ExecutionProcessRunReason,
    execution_status: &db::models::execution_process::ExecutionProcessStatus,
    prompt: Option<&str>,
    summary: Option<&str>,
    normalized_entries: &[NormalizedEntry],
) -> String {
    let mut lines = Vec::new();
    lines.push(format!("## Execution {execution_id}"));
    lines.push(String::new());
    lines.push(format!("- Archived at: {}", Utc::now().to_rfc3339()));
    lines.push(format!(
        "- Branch: {}",
        compact_single_line(branch_name, MAX_TEXT_CHARS)
    ));
    lines.push(format!(
        "- Run reason: {}",
        format!("{run_reason:?}").to_lowercase()
    ));
    lines.push(format!(
        "- Execution status: {}",
        format!("{execution_status:?}").to_lowercase()
    ));

    if let Some(prompt) = prompt.and_then(non_empty) {
        lines.push(String::new());
        lines.push("### Prompt".to_string());
        lines.push(String::new());
        lines.push("```text".to_string());
        lines.push(compact_multiline(prompt, MAX_TEXT_CHARS));
        lines.push("```".to_string());
    }

    let conversation = collect_conversation_lines(normalized_entries);
    lines.push(String::new());
    lines.push("### Conversation".to_string());
    lines.push(String::new());
    if conversation.is_empty() {
        lines.push("- No conversation messages were captured.".to_string());
    } else {
        for line in conversation {
            lines.push(line);
        }
    }

    lines.push(String::new());
    lines.push("### Tool Summary".to_string());
    lines.push(String::new());
    for line in collect_tool_summary_lines(normalized_entries) {
        lines.push(line);
    }

    let summary = summary
        .and_then(non_empty)
        .map(ToOwned::to_owned)
        .or_else(|| find_last_assistant_message(normalized_entries));

    if let Some(summary) = summary {
        lines.push(String::new());
        lines.push("### Final Assistant Summary".to_string());
        lines.push(String::new());
        lines.push(compact_multiline(&summary, MAX_TEXT_CHARS));
    }

    lines.join("\n")
}

fn collect_conversation_lines(entries: &[NormalizedEntry]) -> Vec<String> {
    let mut lines = entries
        .iter()
        .filter_map(|entry| match entry.entry_type {
            NormalizedEntryType::UserMessage => Some(format!(
                "- **User**: {}",
                compact_single_line(&entry.content, MAX_TEXT_CHARS)
            )),
            NormalizedEntryType::AssistantMessage => Some(format!(
                "- **Assistant**: {}",
                compact_single_line(&entry.content, MAX_TEXT_CHARS)
            )),
            _ => None,
        })
        .collect::<Vec<_>>();

    if lines.len() > MAX_CONVERSATION_ENTRIES {
        lines = lines.split_off(lines.len() - MAX_CONVERSATION_ENTRIES);
    }

    lines
}

fn collect_tool_summary_lines(entries: &[NormalizedEntry]) -> Vec<String> {
    let mut per_tool: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_status: BTreeMap<String, usize> = BTreeMap::new();
    let mut detail_lines = Vec::new();

    for entry in entries {
        if let NormalizedEntryType::ToolUse {
            tool_name,
            action_type,
            status,
        } = &entry.entry_type
        {
            *per_tool.entry(tool_name.clone()).or_insert(0) += 1;
            *per_status
                .entry(tool_status_label(status).to_string())
                .or_insert(0) += 1;

            detail_lines.push(format!(
                "- {} `{}` -> {}",
                tool_status_label(status),
                tool_name,
                compact_single_line(&action_type_summary(action_type), MAX_TEXT_CHARS)
            ));
        }
    }

    if detail_lines.is_empty() {
        return vec!["- No tool calls were captured.".to_string()];
    }

    let mut lines = Vec::new();
    lines.push(format!("- Total tool calls: {}", detail_lines.len()));

    if !per_status.is_empty() {
        let status_text = per_status
            .iter()
            .map(|(status, count)| format!("{status}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("- Status counts: {status_text}"));
    }

    if !per_tool.is_empty() {
        let tool_text = per_tool
            .iter()
            .map(|(tool, count)| format!("{tool}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("- Tool counts: {tool_text}"));
    }

    if detail_lines.len() > MAX_TOOL_ENTRIES {
        detail_lines = detail_lines.split_off(detail_lines.len() - MAX_TOOL_ENTRIES);
    }

    lines.extend(detail_lines);
    lines
}

fn action_type_summary(action_type: &ActionType) -> String {
    match action_type {
        ActionType::FileRead { path } => format!("read {path}"),
        ActionType::FileEdit { path, changes } => {
            format!("edit {path} ({} change sets)", changes.len())
        }
        ActionType::CommandRun { command, .. } => format!("run {command}"),
        ActionType::Search { query } => format!("search {query}"),
        ActionType::WebFetch { url } => format!("fetch {url}"),
        ActionType::Tool {
            tool_name,
            arguments,
            ..
        } => {
            if let Some(arguments) = arguments {
                format!("{tool_name} {arguments}")
            } else {
                tool_name.clone()
            }
        }
        ActionType::TaskCreate { description } => format!("task {description}"),
        ActionType::PlanPresentation { plan } => format!("plan {plan}"),
        ActionType::TodoManagement { operation, todos } => {
            format!("todo {operation} ({} items)", todos.len())
        }
        ActionType::Other { description } => description.clone(),
    }
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

fn find_last_assistant_message(entries: &[NormalizedEntry]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|entry| matches!(entry.entry_type, NormalizedEntryType::AssistantMessage))
        .and_then(|entry| non_empty(&entry.content))
        .map(|content| compact_multiline(content, MAX_TEXT_CHARS))
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn compact_single_line(value: &str, max_chars: usize) -> String {
    let compact = value
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&compact, max_chars)
}

fn compact_multiline(value: &str, max_chars: usize) -> String {
    truncate_chars(value.trim(), max_chars)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    let truncated: String = value.chars().take(max_chars).collect();
    format!("{truncated}...")
}

async fn read_meta_map(meta_path: &Path) -> anyhow::Result<Map<String, Value>> {
    let content = match tokio::fs::read_to_string(meta_path).await {
        Ok(content) => content,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Map::new()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read meta file {}", meta_path.display()));
        }
    };

    if content.trim().is_empty() {
        return Ok(Map::new());
    }

    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse meta json {}", meta_path.display()))?;

    Ok(value.as_object().cloned().unwrap_or_default())
}

async fn write_json_file(path: &Path, value: &Value) -> anyhow::Result<()> {
    let serialized = serde_json::to_string_pretty(value)
        .with_context(|| format!("failed to serialize json for {}", path.display()))?;
    tokio::fs::write(path, format!("{serialized}\n"))
        .await
        .with_context(|| format!("failed to write json file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn archive_root_dir_uses_asset_dir_archive() {
        assert_eq!(
            archive_root_dir(),
            utils::assets::asset_dir().join("archive")
        );
    }

    #[test]
    fn archive_task_id_prefers_parent_task_id() {
        let task_id = Uuid::new_v4();
        let parent_task_id = Uuid::new_v4();

        assert_eq!(
            archive_task_id(task_id, Some(parent_task_id)),
            parent_task_id
        );
        assert_eq!(archive_task_id(task_id, None), task_id);
    }

    #[tokio::test]
    async fn upsert_meta_merges_done_diff_fields() {
        let temp_dir = tempdir().unwrap();
        let meta_path = temp_dir.path().join("meta.json");

        write_json_file(
            &meta_path,
            &json!({
                "schema_version": META_SCHEMA_VERSION,
                "task_status": "inreview",
                "diff_additions": null,
                "diff_deletions": null,
            }),
        )
        .await
        .unwrap();

        upsert_meta(
            &meta_path,
            json!({
                "task_status": "done",
                "task_done_at": "2026-02-20T00:00:00Z",
                "diff_additions": 42,
                "diff_deletions": null,
            }),
        )
        .await
        .unwrap();

        let merged: Value =
            serde_json::from_str(&tokio::fs::read_to_string(&meta_path).await.unwrap()).unwrap();
        assert_eq!(merged["task_status"], Value::String("done".to_string()));
        assert_eq!(
            merged["task_done_at"],
            Value::String("2026-02-20T00:00:00Z".to_string())
        );
        assert_eq!(merged["diff_additions"], Value::from(42));
        assert_eq!(merged["diff_deletions"], Value::Null);
    }

    #[tokio::test]
    async fn append_context_section_is_idempotent_per_execution_id() {
        let temp_dir = tempdir().unwrap();
        let archive_files = ArchiveFiles {
            archive_dir: temp_dir.path().to_path_buf(),
            context_path: temp_dir.path().join(CONTEXT_FILE_NAME),
            meta_path: temp_dir.path().join(META_FILE_NAME),
            archive_task_id: Uuid::new_v4(),
        };

        write_json_file(
            &archive_files.meta_path,
            &json!({
                "schema_version": META_SCHEMA_VERSION,
            }),
        )
        .await
        .unwrap();

        tokio::fs::write(&archive_files.context_path, "")
            .await
            .unwrap();

        let execution_id = Uuid::new_v4();
        let base_patch = json!({
            "task_status": "inprogress",
            "project_name": "demo",
        });

        let first = append_context_section(
            &archive_files,
            execution_id,
            "## Execution section",
            base_patch.clone(),
        )
        .await
        .unwrap();
        let second = append_context_section(
            &archive_files,
            execution_id,
            "## Execution section",
            base_patch,
        )
        .await
        .unwrap();

        assert!(first);
        assert!(!second);

        let context = tokio::fs::read_to_string(&archive_files.context_path)
            .await
            .unwrap();
        assert_eq!(
            context
                .matches(&execution_marker(execution_id))
                .collect::<Vec<_>>()
                .len(),
            1
        );

        let meta: Value = serde_json::from_str(
            &tokio::fs::read_to_string(&archive_files.meta_path)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            meta["archived_execution_ids"],
            Value::Array(vec![Value::String(execution_id.to_string())])
        );
    }
}
