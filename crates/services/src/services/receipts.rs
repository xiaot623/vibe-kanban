use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use db::{
    models::{execution_process::ExecutionProcess, task::TaskStatus},
    task_state::{
        TaskStateTransition,
        dispatcher::TaskStateDispatcher,
        handler::{TransitionFilter, fn_handler},
    },
};
use executors::{
    executors::BaseCodingAgent,
    receipts::{ExecutorReceiptSupport, ReceiptCommand},
};
use resvg::tiny_skia;
use serde_json::Value;
use sqlx::{Row, SqlitePool};
use teloxide::{payloads::SendPhotoSetters, prelude::Requester, types::InputFile};
use tokio::process::Command;
use utils::{assets::asset_dir, shell::resolve_executable_path};
use uuid::Uuid;

static RECEIPTS_HANDLER_REGISTERED: OnceLock<()> = OnceLock::new();
const RECEIPT_SESSION_TEMPLATE: &str = include_str!("../../../../assets/receipts/session.svg");

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiptExecution {
    execution_process_id: Uuid,
    agent_session_id: String,
    executor: BaseCodingAgent,
}

#[derive(Debug, Clone, PartialEq)]
struct ReceiptTokenBreakdown {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct ReceiptModelBreakdown {
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    total_tokens: u64,
    total_cost: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct ReceiptUsage {
    executor: BaseCodingAgent,
    session_id: String,
    models: Vec<String>,
    token_breakdown: ReceiptTokenBreakdown,
    model_breakdowns: Vec<ReceiptModelBreakdown>,
    total_tokens: u64,
    total_cost: f64,
    last_activity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiptPaths {
    directory: PathBuf,
    file_path: PathBuf,
}

#[derive(Clone)]
pub struct ReceiptsService {
    output_root: PathBuf,
    runner: Arc<dyn ReceiptCommandRunner>,
}

impl ReceiptsService {
    pub fn new() -> Self {
        Self::with_runner_and_root(
            Arc::new(RealReceiptCommandRunner),
            asset_dir().join("receipts"),
        )
    }

    fn with_runner_and_root(runner: Arc<dyn ReceiptCommandRunner>, output_root: PathBuf) -> Self {
        Self {
            output_root,
            runner,
        }
    }

    pub async fn register_once(dispatcher: &Arc<TaskStateDispatcher>) {
        if RECEIPTS_HANDLER_REGISTERED.get().is_some() {
            return;
        }

        let service = Arc::new(Self::new());
        let handler_service = Arc::clone(&service);
        let handler = fn_handler(
            "receipts_generate_on_done",
            TransitionFilter::new().to(vec![TaskStatus::Done]),
            move |ctx, transition| {
                let service = Arc::clone(&handler_service);
                Box::pin(async move {
                    if !should_generate_receipt(transition) {
                        return;
                    }

                    if let Err(error) = service.handle_done_transition(&ctx.pool, transition).await
                    {
                        tracing::warn!(
                            task_id = %transition.task_id(),
                            error = %error,
                            "Skipping executor session receipt generation"
                        );
                    }
                })
            },
        );

        dispatcher.register_handler(handler).await;
        let _ = RECEIPTS_HANDLER_REGISTERED.set(());
    }

    async fn handle_done_transition(
        &self,
        pool: &SqlitePool,
        transition: &TaskStateTransition,
    ) -> anyhow::Result<()> {
        let Some(execution) = find_latest_receipt_execution(pool, transition.task.id).await? else {
            tracing::info!(
                task_id = %transition.task_id(),
                "No coding-agent execution with agent session id found for completed task"
            );
            return Ok(());
        };

        let Some(command) =
            resolve_receipt_command(execution.executor, &execution.agent_session_id).await
        else {
            tracing::info!(
                task_id = %transition.task_id(),
                executor = %execution.executor,
                session_id = execution.agent_session_id,
                "Executor does not define receipt usage lookup"
            );
            return Ok(());
        };
        let raw_json = match self.runner.run(&command).await {
            Ok(output) => output,
            Err(error) => {
                tracing::warn!(
                    task_id = %transition.task_id(),
                    executor = %execution.executor,
                    session_id = execution.agent_session_id,
                    error = %error,
                    "Usage lookup failed for receipt generation"
                );
                return Ok(());
            }
        };

        let usage = match normalize_usage_json(
            execution.executor,
            &execution.agent_session_id,
            &raw_json,
        ) {
            Ok(usage) => usage,
            Err(error) => {
                tracing::warn!(
                    task_id = %transition.task_id(),
                    executor = %execution.executor,
                    session_id = execution.agent_session_id,
                    error = %error,
                    "Usage JSON did not contain a matching session"
                );
                return Ok(());
            }
        };

        let paths = reserve_receipt_path(
            &self.output_root,
            transition.task.updated_at,
            usage.executor,
            &transition.task.title,
        )?;
        fs::create_dir_all(&paths.directory).with_context(|| {
            format!(
                "failed to create receipt directory {}",
                paths.directory.display()
            )
        })?;

        let png_bytes =
            render_receipt_png(&transition.task.title, transition.task.updated_at, &usage)
                .context("failed to render SVG to PNG for storage")?;
        fs::write(&paths.file_path, png_bytes)
            .with_context(|| format!("failed to write receipt {}", paths.file_path.display()))?;
        tracing::info!(
            task_id = %transition.task_id(),
            receipt_path = %paths.file_path.display(),
            executor = %usage.executor,
            session_id = usage.session_id,
            "Generated executor session receipt"
        );

        if let Err(error) = self
            .send_receipt_png_if_enabled(&transition.task.title, transition.task.updated_at, &usage)
            .await
        {
            tracing::warn!(
                task_id = %transition.task_id(),
                executor = %usage.executor,
                session_id = usage.session_id,
                error = %error,
                "Failed to send executor session receipt to Telegram"
            );
        }

        Ok(())
    }

    async fn send_receipt_png_if_enabled(
        &self,
        task_title: &str,
        done_at: DateTime<Utc>,
        usage: &ReceiptUsage,
    ) -> anyhow::Result<()> {
        let Some(tg) = crate::services::telegram::notifier::get_context().await else {
            return Ok(());
        };

        let config = tg.config.read().await;
        if !config.telegram.enabled || !config.telegram.send_session_receipt {
            return Ok(());
        }

        let png_bytes = render_receipt_png(task_title, done_at, usage)?;
        let file_name = format!(
            "session-receipt-{}.png",
            sanitize_filename_segment(task_title).trim_matches('_')
        );

        tg.bot
            .send_photo(
                tg.chat_id,
                InputFile::memory(png_bytes).file_name(if file_name == "session-receipt-.png" {
                    "session-receipt.png".to_string()
                } else {
                    file_name
                }),
            )
            .caption(format!("Session receipt: {}", task_title))
            .await
            .context("telegram send_photo failed")?;

        Ok(())
    }
}

#[async_trait]
trait ReceiptCommandRunner: Send + Sync {
    async fn run(&self, command: &ReceiptCommand) -> anyhow::Result<String>;
}

struct RealReceiptCommandRunner;

#[async_trait]
impl ReceiptCommandRunner for RealReceiptCommandRunner {
    async fn run(&self, command: &ReceiptCommand) -> anyhow::Result<String> {
        let output = Command::new(&command.program)
            .args(&command.args)
            .output()
            .await
            .with_context(|| format!("failed to spawn {}", format_command(command)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(anyhow!(
                "{} exited with status {}{}",
                format_command(command),
                output.status,
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            ));
        }

        String::from_utf8(output.stdout)
            .with_context(|| format!("{} returned non-UTF8 output", format_command(command)))
    }
}

fn should_generate_receipt(transition: &TaskStateTransition) -> bool {
    transition.is_change()
        && !transition.is_creation()
        && transition.to_status() == &TaskStatus::Done
}

async fn find_latest_receipt_execution(
    pool: &SqlitePool,
    task_id: Uuid,
) -> anyhow::Result<Option<ReceiptExecution>> {
    let row = sqlx::query(
        r#"SELECT
                ep.id as execution_process_id,
                cat.agent_session_id as agent_session_id,
                ep.executor_action as executor_action
           FROM execution_processes ep
           JOIN sessions s ON s.id = ep.session_id
           JOIN workspaces w ON w.id = s.workspace_id
           LEFT JOIN coding_agent_turns cat ON cat.execution_process_id = ep.id
           WHERE w.task_id = $1
             AND ep.run_reason = 'codingagent'
             AND ep.dropped = FALSE
           ORDER BY ep.created_at DESC
           LIMIT 1"#,
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let execution_process_id = row.try_get::<Uuid, _>("execution_process_id")?;
    let Some(agent_session_id) = row
        .try_get::<Option<String>, _>("agent_session_id")?
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };

    let execution = ExecutionProcess::find_by_id(pool, execution_process_id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    let executor = execution
        .executor_action()?
        .base_executor()
        .ok_or_else(|| anyhow!("execution action had no coding executor"))?;

    Ok(Some(ReceiptExecution {
        execution_process_id,
        agent_session_id,
        executor,
    }))
}

async fn resolve_receipt_command(
    executor: BaseCodingAgent,
    agent_session_id: &str,
) -> Option<ReceiptCommand> {
    let command_spec = executor.receipt_command_spec(agent_session_id)?;
    if resolve_command_parts(&command_spec.primary).await.is_some() {
        return Some(command_spec.primary);
    }
    Some(command_spec.fallback)
}

async fn resolve_command_parts(command: &ReceiptCommand) -> Option<()> {
    resolve_executable_path(&command.program).await.map(|_| ())
}

fn normalize_usage_json(
    executor: BaseCodingAgent,
    agent_session_id: &str,
    raw_json: &str,
) -> anyhow::Result<ReceiptUsage> {
    let value = parse_usage_json_value(raw_json)?;
    let sessions = extract_sessions(&value);
    let Some(session) = match_usage_session(&sessions, agent_session_id) else {
        return Err(anyhow!("no session matched agent session id"));
    };

    let mut models = collect_models(session);
    let mut model_breakdowns = collect_model_breakdowns(session);
    if models.is_empty() {
        models = model_breakdowns
            .iter()
            .map(|breakdown| breakdown.model.clone())
            .collect();
    }
    if model_breakdowns.is_empty() {
        model_breakdowns = models
            .iter()
            .map(|model| ReceiptModelBreakdown {
                model: model.clone(),
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                total_tokens: 0,
                total_cost: 0.0,
            })
            .collect();
    }

    let input_tokens = read_u64(session, &["inputTokens", "input_tokens"]).unwrap_or(0);
    let output_tokens = read_u64(session, &["outputTokens", "output_tokens"]).unwrap_or(0);
    let cache_creation_tokens =
        read_u64(session, &["cacheCreationTokens", "cache_creation_tokens"]).unwrap_or(0);
    let cache_read_tokens = read_u64(
        session,
        &["cacheReadTokens", "cache_read_tokens", "cachedInputTokens"],
    )
    .unwrap_or(0);
    let total_tokens = read_u64(session, &["totalTokens", "total_tokens"]).unwrap_or_else(|| {
        input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens
    });
    let total_cost = read_f64(session, &["totalCost", "total_cost", "costUSD"]).unwrap_or(0.0);
    let last_activity = read_string(session, &["lastActivity", "last_activity"])
        .unwrap_or_else(|| "Unknown".to_string());

    Ok(ReceiptUsage {
        executor,
        session_id: agent_session_id.to_string(),
        models,
        token_breakdown: ReceiptTokenBreakdown {
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
        },
        model_breakdowns,
        total_tokens,
        total_cost,
        last_activity,
    })
}

fn parse_usage_json_value(raw_json: &str) -> anyhow::Result<Value> {
    let trimmed = raw_json.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Ok(value);
    }

    if let Some(pos) = raw_json.find(|c| c == '{' || c == '[') {
        if let Ok(value) = serde_json::from_str(&raw_json[pos..]) {
            return Ok(value);
        }
    }

    for offset in json_payload_offsets(raw_json) {
        if let Ok(value) = serde_json::from_str(&raw_json[offset..]) {
            return Ok(value);
        }
    }

    serde_json::from_str(raw_json).context("failed to parse usage JSON")
}

fn json_payload_offsets(raw_json: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    offsets.extend(raw_json.match_indices('\n').map(|(idx, _)| idx + 1));

    let mut json_offsets = Vec::new();
    for line_offset in offsets {
        let slice = if line_offset == 0 {
            raw_json
        } else {
            &raw_json[line_offset..]
        };
        let trimmed = slice.trim_start();
        let whitespace = slice.len().saturating_sub(trimmed.len());
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            json_offsets.push(line_offset + whitespace);
        }
    }

    json_offsets.sort_unstable();
    json_offsets.dedup();
    json_offsets
}

fn extract_sessions<'a>(value: &'a Value) -> Vec<&'a Value> {
    if let Some(sessions) = value.get("sessions").and_then(Value::as_array) {
        return sessions.iter().collect();
    }
    if let Some(session) = value.get("session") {
        return vec![session];
    }
    if value.is_object() {
        return vec![value];
    }
    Vec::new()
}

fn match_usage_session<'a>(sessions: &[&'a Value], agent_session_id: &str) -> Option<&'a Value> {
    let exact = sessions.iter().copied().find(|session| {
        let Some(session_id) = session_identifier(session) else {
            return false;
        };
        let session_variants = canonical_session_identifiers(&session_id);
        let agent_variants = canonical_session_identifiers(agent_session_id);

        session_variants.iter().any(|session_variant| {
            agent_variants
                .iter()
                .any(|agent_variant| agent_variant == session_variant)
        })
    });
    if exact.is_some() {
        return exact;
    }

    sessions.iter().copied().find(|session| {
        let Some(session_id) = session_identifier(session) else {
            return false;
        };
        let session_variants = canonical_session_identifiers(&session_id);
        let agent_variants = canonical_session_identifiers(agent_session_id);

        session_variants.iter().any(|session_variant| {
            agent_variants.iter().any(|agent_variant| {
                session_variant.ends_with(agent_variant) || agent_variant.ends_with(session_variant)
            })
        })
    })
}

fn session_identifier(session: &Value) -> Option<String> {
    read_string(session, &["sessionId", "sessionID", "session_id", "id"])
}

fn canonical_session_identifiers(raw: &str) -> Vec<String> {
    let mut variants = Vec::new();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return variants;
    }

    variants.push(trimmed.to_string());

    if let Some(file_name) = Path::new(trimmed)
        .file_name()
        .and_then(|value| value.to_str())
    {
        variants.push(file_name.to_string());
        if let Some(stem) = file_name.strip_suffix(".jsonl") {
            variants.push(stem.to_string());
            if let Some((_, suffix)) = stem.rsplit_once('_') {
                variants.push(suffix.to_string());
            }
        }
    }

    variants.sort_unstable();
    variants.dedup();
    variants
}

fn collect_models(session: &Value) -> Vec<String> {
    let mut models = BTreeSet::new();
    if let Some(values) = session.get("modelsUsed").and_then(Value::as_array) {
        for value in values {
            if let Some(model) = value
                .as_str()
                .map(str::trim)
                .filter(|model| !model.is_empty())
            {
                models.insert(model.to_string());
            }
        }
    } else if let Some(map) = session.get("models").and_then(Value::as_object) {
        for key in map.keys() {
            models.insert(key.to_string());
        }
    }
    models.into_iter().collect()
}

fn collect_model_breakdowns(session: &Value) -> Vec<ReceiptModelBreakdown> {
    let mut breakdowns = Vec::new();

    if let Some(array) = session.get("modelBreakdowns").and_then(Value::as_array) {
        breakdowns.extend(array.iter().filter_map(|entry| {
            let model = read_string(entry, &["model", "modelName", "model_name"])?;
            let input_tokens = read_u64(entry, &["inputTokens", "input_tokens"]).unwrap_or(0);
            let output_tokens = read_u64(entry, &["outputTokens", "output_tokens"]).unwrap_or(0);
            let cache_creation_tokens =
                read_u64(entry, &["cacheCreationTokens", "cache_creation_tokens"]).unwrap_or(0);
            let cache_read_tokens = read_u64(
                entry,
                &["cacheReadTokens", "cache_read_tokens", "cachedInputTokens"],
            )
            .unwrap_or(0);
            let total_tokens =
                read_u64(entry, &["totalTokens", "total_tokens"]).unwrap_or_else(|| {
                    input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens
                });
            let total_cost =
                read_f64(entry, &["totalCost", "total_cost", "costUSD", "cost"]).unwrap_or(0.0);

            Some(ReceiptModelBreakdown {
                model,
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
                total_tokens,
                total_cost,
            })
        }));
    } else if let Some(map) = session.get("models").and_then(Value::as_object) {
        breakdowns.extend(map.iter().map(|(model, entry)| {
            let input_tokens = read_u64(entry, &["inputTokens", "input_tokens"]).unwrap_or(0);
            let output_tokens = read_u64(entry, &["outputTokens", "output_tokens"]).unwrap_or(0);
            let cache_creation_tokens =
                read_u64(entry, &["cacheCreationTokens", "cache_creation_tokens"]).unwrap_or(0);
            let cache_read_tokens = read_u64(
                entry,
                &["cacheReadTokens", "cache_read_tokens", "cachedInputTokens"],
            )
            .unwrap_or(0);
            let total_tokens =
                read_u64(entry, &["totalTokens", "total_tokens"]).unwrap_or_else(|| {
                    input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens
                });
            let total_cost =
                read_f64(entry, &["totalCost", "total_cost", "costUSD"]).unwrap_or(0.0);

            ReceiptModelBreakdown {
                model: model.clone(),
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
                total_tokens,
                total_cost,
            }
        }));
    }

    breakdowns
}

fn read_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|field| match field {
            Value::Number(number) => number
                .as_u64()
                .or_else(|| number.as_i64().map(|n| n as u64)),
            Value::String(raw) => raw.parse::<u64>().ok(),
            _ => None,
        })
    })
}

fn read_f64(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|field| match field {
            Value::Number(number) => number.as_f64(),
            Value::String(raw) => raw.parse::<f64>().ok(),
            _ => None,
        })
    })
}

fn read_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|field| {
            field
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
        })
    })
}

fn reserve_receipt_path(
    output_root: &Path,
    done_at: DateTime<Utc>,
    executor: BaseCodingAgent,
    task_title: &str,
) -> anyhow::Result<ReceiptPaths> {
    let local_time = done_at.with_timezone(&Local);
    let month = local_time.format("%Y%m").to_string();
    let timestamp = local_time.format("%Y%m%d%H%M%S").to_string();
    let directory = output_root.join(&month);
    let executor_segment = sanitize_filename_segment(executor.receipt_slug());
    let title_segment = sanitize_filename_segment(task_title);
    let stem = format!("{timestamp}{executor_segment}-{title_segment}");

    let mut counter = 1usize;
    loop {
        let suffix = if counter == 1 {
            String::new()
        } else {
            format!("-{counter}")
        };
        let file_path = directory.join(format!("{stem}{suffix}.png"));
        if !file_path.exists() {
            return Ok(ReceiptPaths {
                directory,
                file_path,
            });
        }
        counter += 1;
    }
}

fn sanitize_filename_segment(value: &str) -> String {
    let mut sanitized = String::new();
    let mut previous_dash = false;

    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            previous_dash = false;
            Some(ch.to_ascii_lowercase())
        } else if !previous_dash {
            previous_dash = true;
            Some('-')
        } else {
            None
        };

        if let Some(ch) = mapped {
            sanitized.push(ch);
        }
    }

    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "task".to_string()
    } else {
        trimmed.to_string()
    }
}

fn render_receipt_svg(task_title: &str, done_at: DateTime<Utc>, usage: &ReceiptUsage) -> String {
    let mut model_rows = String::new();
    let mut current_y = 250; // Starting Y position for models (increased from 220)

    for breakdown in &usage.model_breakdowns {
        let title_y = current_y;
        let stats_y_start = current_y + 25;

        let section = format!(
            "<g transform=\"translate(20, {title_y})\">\n              <text x=\"0\" y=\"0\" class=\"text bold\">{model}</text>\n              <text x=\"360\" y=\"0\" class=\"text bold\" text-anchor=\"end\">{cost}</text>\n            </g>\n            <g transform=\"translate(20, {stats_y_start})\">\n              <text x=\"0\" y=\"0\" class=\"muted\">Input tokens</text><text x=\"360\" y=\"0\" class=\"muted\" text-anchor=\"end\">{input}</text>\n              <text x=\"0\" y=\"20\" class=\"muted\">Output tokens</text><text x=\"360\" y=\"20\" class=\"muted\" text-anchor=\"end\">{output}</text>\n              <text x=\"0\" y=\"40\" class=\"muted\">Cache write</text><text x=\"360\" y=\"40\" class=\"muted\" text-anchor=\"end\">{cache_write}</text>\n              <text x=\"0\" y=\"60\" class=\"muted\">Cache read</text><text x=\"360\" y=\"60\" class=\"muted\" text-anchor=\"end\">{cache_read}</text>\n            </g>",
            model = escape_html(&breakdown.model),
            cost = format_currency(breakdown.total_cost),
            input = format_integer(breakdown.input_tokens),
            output = format_integer(breakdown.output_tokens),
            cache_write = format_integer(breakdown.cache_creation_tokens),
            cache_read = format_integer(breakdown.cache_read_tokens),
            title_y = title_y,
            stats_y_start = stats_y_start
        );

        model_rows.push_str(&section);
        model_rows.push_str("\n");
        current_y += 110; // Increment for next model (increased from 100 for better spacing)
    }

    let mut session_id_short = usage.session_id.clone();
    if session_id_short.len() > 24 {
        session_id_short.truncate(24);
        session_id_short.push_str("...");
    }

    let mut task_title_short = task_title.to_string();
    if task_title_short.len() > 30 {
        task_title_short.truncate(30);
        task_title_short.push_str("...");
    }

    // Last model stats ended at (current_y - 110) + 25 + 60 = current_y - 25
    // We want divider at last_stat + 25 = current_y
    // total_y_minus_30 = current_y => total_y = current_y + 30
    let total_y = current_y + 30;
    
    // Total text is at total_y. Last line of Total is at total_y.
    // We want divider at total_y + 25.
    // footer_y_minus_30 = total_y + 25 => footer_y = total_y + 55
    let footer_y = total_y + 55;
    
    // Last footer line is at footer_y + 24.
    // We want divider at footer_y + 24 + 25 = footer_y + 49.
    // credits_y_minus_30 = footer_y + 49 => credits_y = footer_y + 79
    let credits_y = footer_y + 79;
    
    let total_height = credits_y + 50;

    let svg_icon = agent_icon_svg(usage.executor);
    let svg_icon = if svg_icon.contains("width=\"16\"") {
        svg_icon.replace("width=\"16\"", "width=\"64\"").replace("height=\"16\"", "height=\"64\"")
    } else {
        svg_icon.replace("width=\"24\"", "width=\"64\"").replace("height=\"24\"", "height=\"64\"")
    };

    render_template(
        RECEIPT_SESSION_TEMPLATE,
        &[
            ("{{icon_svg}}", &svg_icon),
            ("{{task_title_short}}", &escape_html(&task_title_short)),
            (
                "{{executor_name}}",
                &escape_html(usage.executor.receipt_display_name()),
            ),
            ("{{session_id_short}}", &escape_html(&session_id_short)),
            ("{{total_cost}}", &format_currency(usage.total_cost)),
            (
                "{{done_at}}",
                &escape_html(
                    &done_at
                        .with_timezone(&Local)
                        .format("%b %d, %Y, %I:%M %p")
                        .to_string(),
                ),
            ),
            ("{{model_rows}}", &model_rows),
            ("{{total_height}}", &total_height.to_string()),
            (
                "{{total_height_minus_8}}",
                &(total_height - 8).to_string(),
            ),
            ("{{total_height_plus_10}}", &(total_height + 10).to_string()),
            ("{{total_y}}", &total_y.to_string()),
            ("{{total_y_minus_30}}", &(total_y - 30).to_string()),
            ("{{footer_y}}", &footer_y.to_string()),
            ("{{footer_y_minus_30}}", &(footer_y - 30).to_string()),
            ("{{credits_y}}", &credits_y.to_string()),
            ("{{credits_y_minus_30}}", &(credits_y - 30).to_string()),
        ],
    )
}

fn render_receipt_png(
    task_title: &str,
    done_at: DateTime<Utc>,
    usage: &ReceiptUsage,
) -> anyhow::Result<Vec<u8>> {
    let svg_content = render_receipt_svg(task_title, done_at, usage);

    let mut fontdb = usvg::fontdb::Database::new();
    fontdb.load_system_fonts();

    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_str(&svg_content, &opt, &fontdb).context("failed to parse SVG")?;

    let pixmap_size = tree.size().to_int_size();
    let scale = 3.0;
    let mut pixmap = tiny_skia::Pixmap::new(
        (pixmap_size.width() as f32 * scale) as u32,
        (pixmap_size.height() as f32 * scale) as u32,
    )
    .context("failed to create pixmap")?;

    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    let png_bytes = pixmap.encode_png().context("failed to encode PNG")?;
    Ok(png_bytes)
}

fn render_template(template: &str, replacements: &[(&str, &str)]) -> String {
    replacements
        .iter()
        .fold(template.to_string(), |rendered, (token, value)| {
            rendered.replace(token, value)
        })
}

fn agent_icon_svg(executor: BaseCodingAgent) -> &'static str {
    match executor {
        BaseCodingAgent::ClaudeCode => {
            include_str!("../../../../frontend/public/agents/claude-light.svg")
        }
        BaseCodingAgent::Codex => {
            include_str!("../../../../frontend/public/agents/codex-light.svg")
        }
        BaseCodingAgent::Opencode => {
            include_str!("../../../../frontend/public/agents/opencode-light.svg")
        }
        BaseCodingAgent::Pi => {
            include_str!("../../../../frontend/public/agents/opencode-light.svg")
        }
        BaseCodingAgent::Gemini => {
            include_str!("../../../../frontend/public/agents/gemini-light.svg")
        }
    }
}

fn format_integer(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn format_currency(value: f64) -> String {
    format!("${value:.2}")
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn format_command(command: &ReceiptCommand) -> String {
    let mut parts = Vec::with_capacity(command.args.len() + 1);
    parts.push(command.program.clone());
    parts.extend(command.args.clone());
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use db::models::{
        coding_agent_turn::{CodingAgentTurn, CreateCodingAgentTurn},
        execution_process::{
            CreateExecutionProcess, ExecutionProcessRunReason, ExecutionProcessStatus,
        },
        project::{CreateProject, Project},
        session::{CreateSession, Session},
        task::{CreateTask, Task},
        workspace::{CreateWorkspace, Workspace},
    };
    use executors::{
        actions::{
            ExecutorAction, ExecutorActionType, coding_agent_initial::CodingAgentInitialRequest,
        },
        profile::ExecutorProfileId,
    };
    use tempfile::TempDir;
    use tokio::time::{Duration, sleep, timeout};

    use super::*;

    #[derive(Clone)]
    struct MockReceiptCommandRunner {
        output: Arc<String>,
    }

    #[async_trait]
    impl ReceiptCommandRunner for MockReceiptCommandRunner {
        async fn run(&self, _command: &ReceiptCommand) -> anyhow::Result<String> {
            Ok(self.output.as_ref().clone())
        }
    }

    fn mock_service(output_root: PathBuf, json: &str) -> Arc<ReceiptsService> {
        Arc::new(ReceiptsService::with_runner_and_root(
            Arc::new(MockReceiptCommandRunner {
                output: Arc::new(json.to_string()),
            }),
            output_root,
        ))
    }

    fn sample_transition(
        from_status: Option<TaskStatus>,
        to_status: TaskStatus,
    ) -> TaskStateTransition {
        let now = Utc::now();
        let task = Task {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            title: "Ship receipt".to_string(),
            description: None,
            status: to_status,
            parent_workspace_id: None,
            source_cron_task_id: None,
            diff_additions: None,
            diff_deletions: None,
            created_at: now,
            updated_at: now,
        };
        TaskStateTransition::new(task, from_status)
    }

    #[test]
    fn filters_real_done_transitions() {
        assert!(should_generate_receipt(&sample_transition(
            Some(TaskStatus::InProgress),
            TaskStatus::Done,
        )));
        assert!(!should_generate_receipt(&sample_transition(
            None,
            TaskStatus::Done,
        )));
        assert!(!should_generate_receipt(&sample_transition(
            Some(TaskStatus::Done),
            TaskStatus::Done,
        )));
    }

    #[test]
    fn normalizes_usage_json() {
        let usage = normalize_usage_json(
            BaseCodingAgent::Codex,
            "session-123",
            r#"{
              "sessions": [{
                "sessionId": "session-123",
                "inputTokens": 1200,
                "outputTokens": 3400,
                "cacheCreationTokens": 100,
                "cacheReadTokens": 250,
                "totalTokens": 4950,
                "totalCost": 12.34,
                "lastActivity": "2026-05-01T12:00:00Z",
                "modelsUsed": ["gpt-5", "gpt-5-mini"],
                "modelBreakdowns": [{
                  "model": "gpt-5",
                  "inputTokens": 1000,
                  "outputTokens": 3000,
                  "cacheCreationTokens": 80,
                  "cacheReadTokens": 200,
                  "totalTokens": 4280,
                  "totalCost": 10.10
                }, {
                  "model": "gpt-5-mini",
                  "inputTokens": 200,
                  "outputTokens": 400,
                  "cacheCreationTokens": 20,
                  "cacheReadTokens": 50,
                  "totalTokens": 670,
                  "totalCost": 2.24
                }]
              }]
            }"#,
        )
        .unwrap();

        assert_eq!(usage.session_id, "session-123");
        assert_eq!(
            usage.models,
            vec!["gpt-5".to_string(), "gpt-5-mini".to_string()]
        );
        assert_eq!(usage.total_tokens, 4950);
        assert_eq!(usage.total_cost, 12.34);
        assert_eq!(usage.model_breakdowns.len(), 2);
    }

    #[test]
    fn handles_json_with_noise_prefix() {
        let raw = r#"[@ccusage/opencode]  WARN  Fetching latest model pricing from LiteLLM...

[@ccusage/opencode] ℹ Loaded pricing for 2691 models
{
  "sessions": [{
    "sessionID": "ses_123",
    "totalTokens": 100,
    "totalCost": 0.5
  }]
}"#;
        let usage = normalize_usage_json(BaseCodingAgent::Opencode, "ses_123", raw).unwrap();
        assert_eq!(usage.session_id, "ses_123");
        assert_eq!(usage.total_tokens, 100);
        assert_eq!(usage.total_cost, 0.5);
    }

    #[test]
    fn generates_unique_filenames() {
        let temp_dir = TempDir::new().unwrap();
        let output_root = temp_dir.path().join("receipts");
        let done_at = Utc.with_ymd_and_hms(2026, 5, 1, 9, 8, 7).unwrap();

        let first =
            reserve_receipt_path(&output_root, done_at, BaseCodingAgent::Codex, "Fix login!")
                .unwrap();
        fs::create_dir_all(&first.directory).unwrap();
        fs::write(&first.file_path, "one").unwrap();

        let second =
            reserve_receipt_path(&output_root, done_at, BaseCodingAgent::Codex, "Fix login!")
                .unwrap();

        let local_time = done_at.with_timezone(&Local);
        let expected_timestamp = local_time.format("%Y%m%d%H%M%S").to_string();

        assert_eq!(
            first.file_path.file_name().unwrap().to_string_lossy(),
            format!("{}codex-fix-login.png", expected_timestamp)
        );
        assert_eq!(
            second.file_path.file_name().unwrap().to_string_lossy(),
            format!("{}codex-fix-login-2.png", expected_timestamp)
        );
    }

    #[test]
    fn renders_receipt_svg() {
        let usage = ReceiptUsage {
            executor: BaseCodingAgent::ClaudeCode,
            session_id: "session-abc".to_string(),
            models: vec!["sonnet-4".to_string()],
            token_breakdown: ReceiptTokenBreakdown {
                input_tokens: 1,
                output_tokens: 2,
                cache_creation_tokens: 3,
                cache_read_tokens: 4,
            },
            model_breakdowns: vec![ReceiptModelBreakdown {
                model: "sonnet-4".to_string(),
                input_tokens: 1,
                output_tokens: 2,
                cache_creation_tokens: 3,
                cache_read_tokens: 4,
                total_tokens: 10,
                total_cost: 1.25,
            }],
            total_tokens: 10,
            total_cost: 1.25,
            last_activity: "2026-05-01T10:00:00Z".to_string(),
        };

        let svg = super::render_receipt_svg(
            "Wrap up feature",
            Utc.with_ymd_and_hms(2026, 5, 1, 10, 30, 0).unwrap(),
            &usage,
        );

        assert!(svg.contains("Wrap up feature"));
        assert!(svg.contains("session-abc"));
        assert!(svg.contains("sonnet-4"));
        assert!(svg.contains("$1.25"));
    }

    #[test]
    fn renders_receipt_png() {
        let usage = ReceiptUsage {
            executor: BaseCodingAgent::Codex,
            session_id: "session-abc".to_string(),
            models: vec!["sonnet-4".to_string(), "gpt-5".to_string()],
            token_breakdown: ReceiptTokenBreakdown {
                input_tokens: 1_200,
                output_tokens: 900,
                cache_creation_tokens: 150,
                cache_read_tokens: 300,
            },
            model_breakdowns: vec![ReceiptModelBreakdown {
                model: "sonnet-4".to_string(),
                input_tokens: 1_200,
                output_tokens: 900,
                cache_creation_tokens: 150,
                cache_read_tokens: 300,
                total_tokens: 2_550,
                total_cost: 1.25,
            }],
            total_tokens: 2_550,
            total_cost: 1.25,
            last_activity: "2026-05-01T10:15:00Z".to_string(),
        };

        let png = render_receipt_png(
            "Wrap up feature",
            Utc.with_ymd_and_hms(2026, 5, 1, 10, 30, 0).unwrap(),
            &usage,
        )
        .expect("render PNG");

        assert!(png.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]));
        assert!(png.len() > 10_000);
    }

    #[tokio::test]
    async fn writes_receipt_when_task_transitions_to_done() {
        let db = db::DBService::new().await.unwrap();
        let temp_dir = TempDir::new().unwrap();
        let service = mock_service(
            temp_dir.path().join("receipts"),
            r#"{
              "sessions": [{
                "sessionId": "session-xyz",
                "inputTokens": 10,
                "outputTokens": 20,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 0,
                "totalTokens": 30,
                "totalCost": 0.42,
                "lastActivity": "2026-05-01T00:00:00Z",
                "modelsUsed": ["gpt-5"],
                "modelBreakdowns": [{
                  "model": "gpt-5",
                  "inputTokens": 10,
                  "outputTokens": 20,
                  "totalTokens": 30,
                  "totalCost": 0.42
                }]
              }]
            }"#,
        );

        let project = Project::create(
            &db.pool,
            &CreateProject {
                name: "Receipts".to_string(),
                repositories: vec![],
            },
            Uuid::new_v4(),
        )
        .await
        .unwrap();

        let task = Task::create(
            &db.pool,
            &CreateTask {
                project_id: project.id,
                title: "Render receipt".to_string(),
                description: None,
                status: Some(TaskStatus::InProgress),
                parent_workspace_id: None,
                source_cron_task_id: None,
                image_ids: None,
            },
            Uuid::new_v4(),
        )
        .await
        .unwrap();

        let workspace = Workspace::create(
            &db.pool,
            &CreateWorkspace {
                branch: "receipt-branch".to_string(),
                agent_working_dir: None,
            },
            Uuid::new_v4(),
            task.id,
        )
        .await
        .unwrap();

        let session = Session::create(
            &db.pool,
            &CreateSession {
                executor: Some("CODEX".to_string()),
            },
            Uuid::new_v4(),
            workspace.id,
        )
        .await
        .unwrap();

        let action = ExecutorAction::new(
            ExecutorActionType::CodingAgentInitialRequest(CodingAgentInitialRequest {
                prompt: "Implement it".to_string(),
                executor_profile_id: ExecutorProfileId::new(BaseCodingAgent::Codex),
                working_dir: None,
            }),
            None,
        );

        let execution = ExecutionProcess::create(
            &db.pool,
            &CreateExecutionProcess {
                session_id: session.id,
                executor_action: action,
                run_reason: ExecutionProcessRunReason::CodingAgent,
            },
            Uuid::new_v4(),
            &[],
        )
        .await
        .unwrap();
        ExecutionProcess::update_completion(
            &db.pool,
            execution.id,
            ExecutionProcessStatus::Completed,
            Some(0),
        )
        .await
        .unwrap();

        let _turn = CodingAgentTurn::create(
            &db.pool,
            &CreateCodingAgentTurn {
                execution_process_id: execution.id,
                prompt: Some("Implement it".to_string()),
            },
            Uuid::new_v4(),
        )
        .await
        .unwrap();
        CodingAgentTurn::update_agent_session_id(&db.pool, execution.id, "session-xyz")
            .await
            .unwrap();

        let done_task = Task::update(
            &db.pool,
            task.id,
            task.project_id,
            task.title.clone(),
            None,
            TaskStatus::Done,
            None,
        )
        .await
        .unwrap();

        let transition = TaskStateTransition::new(done_task, Some(TaskStatus::InProgress));
        service
            .handle_done_transition(&db.pool, &transition)
            .await
            .unwrap();

        timeout(Duration::from_secs(3), async {
            loop {
                let month_dir = temp_dir
                    .path()
                    .join("receipts")
                    .join(Utc::now().format("%Y%m").to_string());
                let files = fs::read_dir(&month_dir)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .collect::<Vec<_>>();
                if files.len() == 1 {
                    let png = fs::read(files[0].path()).unwrap();
                    assert!(png.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]));
                    assert!(png.len() > 10_000);
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }
}
