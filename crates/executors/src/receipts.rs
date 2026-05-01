use std::{collections::BTreeSet, path::Path};

use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use thiserror::Error;
use tokio::process::Command;
use workspace_utils::shell::resolve_executable_path;

use crate::executors::{BaseCodingAgent, claude, codex, opencode, pi};

const CLAUDE_CODE_TEMPLATE: &str = include_str!("../../../assets/receipts/claude_code.svg");
const CODEX_TEMPLATE: &str = include_str!("../../../assets/receipts/codex.svg");
const OPENCODE_TEMPLATE: &str = include_str!("../../../assets/receipts/opencode.svg");
const PI_TEMPLATE: &str = include_str!("../../../assets/receipts/pi.svg");

#[derive(Debug, Clone, Copy)]
pub struct ReceiptRequest<'a> {
    pub agent_session_id: &'a str,
    pub task_title: &'a str,
    pub done_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum ReceiptError {
    #[error("executor does not support receipts")]
    UnsupportedExecutor,
    #[error("failed to spawn {command}: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
    #[error("{command} exited with status {status}{stderr}")]
    Failed {
        command: String,
        status: std::process::ExitStatus,
        stderr: String,
    },
    #[error("{command} returned non-UTF8 output")]
    NonUtf8 { command: String },
    #[error("failed to parse usage JSON")]
    Json(#[from] serde_json::Error),
    #[error("usage JSON did not contain a matching session")]
    SessionNotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptCommand {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptCommandSpec {
    pub primary: ReceiptCommand,
    pub fallback: ReceiptCommand,
}

#[async_trait]
pub trait ExecutorReceiptSupport {
    fn receipt_command_spec(&self, agent_session_id: &str) -> Option<ReceiptCommandSpec>;
    fn receipt_slug(&self) -> &'static str;
    fn receipt_display_name(&self) -> &'static str;
    async fn generate_receipt_svg(
        &self,
        request: ReceiptRequest<'_>,
    ) -> Result<String, ReceiptError>;
}

#[async_trait]
impl ExecutorReceiptSupport for BaseCodingAgent {
    fn receipt_command_spec(&self, agent_session_id: &str) -> Option<ReceiptCommandSpec> {
        match self {
            BaseCodingAgent::ClaudeCode => Some(claude::receipt_command_spec(agent_session_id)),
            BaseCodingAgent::Codex => Some(codex::receipt_command_spec(agent_session_id)),
            BaseCodingAgent::Opencode => Some(opencode::receipt_command_spec(agent_session_id)),
            BaseCodingAgent::Pi => Some(pi::receipt_command_spec(agent_session_id)),
            BaseCodingAgent::Gemini => None,
        }
    }

    fn receipt_slug(&self) -> &'static str {
        match self {
            BaseCodingAgent::ClaudeCode => "claude-code",
            BaseCodingAgent::Codex => "codex",
            BaseCodingAgent::Opencode => "opencode",
            BaseCodingAgent::Pi => "pi",
            BaseCodingAgent::Gemini => "gemini",
        }
    }

    fn receipt_display_name(&self) -> &'static str {
        match self {
            BaseCodingAgent::ClaudeCode => "Claude Code",
            BaseCodingAgent::Codex => "Codex",
            BaseCodingAgent::Opencode => "OpenCode",
            BaseCodingAgent::Pi => "Pi",
            BaseCodingAgent::Gemini => "Gemini",
        }
    }

    async fn generate_receipt_svg(
        &self,
        request: ReceiptRequest<'_>,
    ) -> Result<String, ReceiptError> {
        let command_spec = self
            .receipt_command_spec(request.agent_session_id)
            .ok_or(ReceiptError::UnsupportedExecutor)?;
        let raw_json = run_receipt_command_spec(&command_spec).await?;
        let value = parse_usage_json_value(&raw_json)?;
        let sessions = extract_sessions(&value);
        let session = match_usage_session(&sessions, request.agent_session_id)
            .ok_or(ReceiptError::SessionNotFound)?;

        match self {
            BaseCodingAgent::ClaudeCode => {
                let params = ClaudeCodeReceiptParams::from_session(request, session)?;
                Ok(render_receipt_template(
                    CLAUDE_CODE_TEMPLATE,
                    self,
                    &params.into_render_params(),
                ))
            }
            BaseCodingAgent::Codex => {
                let params = CodexReceiptParams::from_session(request, session)?;
                Ok(render_receipt_template(
                    CODEX_TEMPLATE,
                    self,
                    &params.into_render_params(),
                ))
            }
            BaseCodingAgent::Opencode => {
                let params = OpencodeReceiptParams::from_session(request, session)?;
                Ok(render_receipt_template(
                    OPENCODE_TEMPLATE,
                    self,
                    &params.into_render_params(),
                ))
            }
            BaseCodingAgent::Pi => {
                let params = PiReceiptParams::from_session(request, session)?;
                Ok(render_receipt_template(
                    PI_TEMPLATE,
                    self,
                    &params.into_render_params(),
                ))
            }
            BaseCodingAgent::Gemini => Err(ReceiptError::UnsupportedExecutor),
        }
    }
}

pub fn package_command(package_name: &str, path_binary: &str) -> ReceiptCommandSpec {
    ReceiptCommandSpec {
        primary: ReceiptCommand {
            program: path_binary.to_string(),
            args: vec!["session".to_string(), "--json".to_string()],
        },
        fallback: ReceiptCommand {
            program: "npx".to_string(),
            args: vec![
                "--yes".to_string(),
                format!("{package_name}@latest"),
                "session".to_string(),
                "--json".to_string(),
            ],
        },
    }
}

async fn run_receipt_command_spec(spec: &ReceiptCommandSpec) -> Result<String, ReceiptError> {
    if resolve_executable_path(&spec.primary.program)
        .await
        .is_some()
    {
        if let Ok(output) = run_receipt_command(&spec.primary).await {
            return Ok(output);
        }
    }
    run_receipt_command(&spec.fallback).await
}

async fn run_receipt_command(command: &ReceiptCommand) -> Result<String, ReceiptError> {
    let formatted = format_command(command);
    let output = Command::new(&command.program)
        .args(&command.args)
        .output()
        .await
        .map_err(|source| ReceiptError::Spawn {
            command: formatted.clone(),
            source,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ReceiptError::Failed {
            command: formatted,
            status: output.status,
            stderr: if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            },
        });
    }

    String::from_utf8(output.stdout).map_err(|_| ReceiptError::NonUtf8 { command: formatted })
}

fn parse_usage_json_value(raw_json: &str) -> Result<Value, ReceiptError> {
    let trimmed = raw_json.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Ok(value);
    }

    for offset in json_payload_offsets(raw_json) {
        if let Ok(value) = serde_json::from_str(&raw_json[offset..]) {
            return Ok(value);
        }
    }

    serde_json::from_str(raw_json).map_err(ReceiptError::Json)
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
    read_string(
        session,
        &["sessionId", "sessionID", "session_id", "id", "sessionFile"],
    )
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

#[derive(Debug, Clone, PartialEq)]
struct OptionalTokenBreakdown {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
struct OptionalModelBreakdown {
    model: String,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cost: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
struct RenderParams {
    task_title: String,
    session_id: String,
    done_at: DateTime<Utc>,
    metadata_rows: Vec<(String, String)>,
    summary_rows: Vec<(String, String)>,
    model_breakdowns: Vec<OptionalModelBreakdown>,
}

struct ClaudeCodeReceiptParams(RenderParams);
struct CodexReceiptParams(RenderParams);
struct OpencodeReceiptParams(RenderParams);
struct PiReceiptParams(RenderParams);

impl ClaudeCodeReceiptParams {
    fn from_session(request: ReceiptRequest<'_>, session: &Value) -> Result<Self, ReceiptError> {
        let session_id =
            read_string(session, &["sessionId"]).ok_or(ReceiptError::SessionNotFound)?;
        let token_breakdown = OptionalTokenBreakdown {
            input_tokens: read_u64(session, &["inputTokens"]),
            output_tokens: read_u64(session, &["outputTokens"]),
            cache_creation_tokens: read_u64(session, &["cacheCreationTokens"]),
            cache_read_tokens: read_u64(session, &["cacheReadTokens"]),
            cached_input_tokens: None,
            reasoning_output_tokens: None,
            total_tokens: read_u64(session, &["totalTokens"]),
        };
        let model_breakdowns = collect_model_breakdowns(session, &["modelName"], &["cost"]);
        let mut metadata_rows = Vec::new();
        add_models_used(&mut metadata_rows, session);

        Ok(Self(RenderParams {
            task_title: request.task_title.to_string(),
            session_id,
            done_at: request.done_at,
            metadata_rows,
            summary_rows: summary_rows(token_breakdown, read_f64(session, &["totalCost"])),
            model_breakdowns,
        }))
    }

    fn into_render_params(self) -> RenderParams {
        self.0
    }
}

impl CodexReceiptParams {
    fn from_session(request: ReceiptRequest<'_>, session: &Value) -> Result<Self, ReceiptError> {
        let session_id = read_string(session, &["sessionId", "sessionFile"])
            .ok_or(ReceiptError::SessionNotFound)?;
        let token_breakdown = OptionalTokenBreakdown {
            input_tokens: read_u64(session, &["inputTokens"]),
            output_tokens: read_u64(session, &["outputTokens"]),
            cache_creation_tokens: None,
            cache_read_tokens: None,
            cached_input_tokens: read_u64(session, &["cachedInputTokens"]),
            reasoning_output_tokens: read_u64(session, &["reasoningOutputTokens"]),
            total_tokens: read_u64(session, &["totalTokens"]),
        };
        let mut metadata_rows = Vec::new();
        add_string_row(
            &mut metadata_rows,
            "Session file",
            session,
            &["sessionFile"],
        );
        add_string_row(&mut metadata_rows, "Directory", session, &["directory"]);

        Ok(Self(RenderParams {
            task_title: request.task_title.to_string(),
            session_id,
            done_at: request.done_at,
            metadata_rows,
            summary_rows: summary_rows(token_breakdown, read_f64(session, &["costUSD"])),
            model_breakdowns: collect_models_map(session),
        }))
    }

    fn into_render_params(self) -> RenderParams {
        self.0
    }
}

impl OpencodeReceiptParams {
    fn from_session(request: ReceiptRequest<'_>, session: &Value) -> Result<Self, ReceiptError> {
        let session_id =
            read_string(session, &["sessionID"]).ok_or(ReceiptError::SessionNotFound)?;
        let token_breakdown = OptionalTokenBreakdown {
            input_tokens: read_u64(session, &["inputTokens"]),
            output_tokens: read_u64(session, &["outputTokens"]),
            cache_creation_tokens: read_u64(session, &["cacheCreationTokens"]),
            cache_read_tokens: read_u64(session, &["cacheReadTokens"]),
            cached_input_tokens: None,
            reasoning_output_tokens: None,
            total_tokens: read_u64(session, &["totalTokens"]),
        };
        let mut metadata_rows = Vec::new();
        add_string_row(&mut metadata_rows, "Title", session, &["sessionTitle"]);
        add_string_row(&mut metadata_rows, "Parent", session, &["parentID"]);
        add_models_used(&mut metadata_rows, session);

        Ok(Self(RenderParams {
            task_title: request.task_title.to_string(),
            session_id,
            done_at: request.done_at,
            metadata_rows,
            summary_rows: summary_rows(token_breakdown, read_f64(session, &["totalCost"])),
            model_breakdowns: Vec::new(),
        }))
    }

    fn into_render_params(self) -> RenderParams {
        self.0
    }
}

impl PiReceiptParams {
    fn from_session(request: ReceiptRequest<'_>, session: &Value) -> Result<Self, ReceiptError> {
        let session_id =
            read_string(session, &["sessionId"]).ok_or(ReceiptError::SessionNotFound)?;
        let token_breakdown = OptionalTokenBreakdown {
            input_tokens: read_u64(session, &["inputTokens"]),
            output_tokens: read_u64(session, &["outputTokens"]),
            cache_creation_tokens: read_u64(session, &["cacheCreationTokens"]),
            cache_read_tokens: read_u64(session, &["cacheReadTokens"]),
            cached_input_tokens: None,
            reasoning_output_tokens: None,
            total_tokens: read_u64(session, &["totalTokens"]),
        };
        let mut metadata_rows = Vec::new();
        add_string_row(
            &mut metadata_rows,
            "Project path",
            session,
            &["projectPath"],
        );
        add_string_row(&mut metadata_rows, "Source", session, &["source"]);
        add_models_used(&mut metadata_rows, session);

        Ok(Self(RenderParams {
            task_title: request.task_title.to_string(),
            session_id,
            done_at: request.done_at,
            metadata_rows,
            summary_rows: summary_rows(token_breakdown, read_f64(session, &["totalCost"])),
            model_breakdowns: collect_model_breakdowns(session, &["modelName"], &["cost"]),
        }))
    }

    fn into_render_params(self) -> RenderParams {
        self.0
    }
}

fn summary_rows(
    token_breakdown: OptionalTokenBreakdown,
    total_cost: Option<f64>,
) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    add_optional_u64(&mut rows, "Input tokens", token_breakdown.input_tokens);
    add_optional_u64(&mut rows, "Output tokens", token_breakdown.output_tokens);
    add_optional_u64(
        &mut rows,
        "Cache write",
        token_breakdown.cache_creation_tokens,
    );
    add_optional_u64(&mut rows, "Cache read", token_breakdown.cache_read_tokens);
    add_optional_u64(
        &mut rows,
        "Cached input",
        token_breakdown.cached_input_tokens,
    );
    add_optional_u64(
        &mut rows,
        "Reasoning output",
        token_breakdown.reasoning_output_tokens,
    );
    add_optional_u64(&mut rows, "Total tokens", token_breakdown.total_tokens);
    if let Some(cost) = total_cost {
        rows.push(("Total cost".to_string(), format_currency(cost)));
    }
    rows
}

fn collect_model_breakdowns(
    session: &Value,
    model_keys: &[&str],
    cost_keys: &[&str],
) -> Vec<OptionalModelBreakdown> {
    let Some(array) = session.get("modelBreakdowns").and_then(Value::as_array) else {
        return Vec::new();
    };

    array
        .iter()
        .filter_map(|entry| {
            let model = read_string(entry, model_keys)?;
            Some(OptionalModelBreakdown {
                model,
                input_tokens: read_u64(entry, &["inputTokens"]),
                output_tokens: read_u64(entry, &["outputTokens"]),
                cache_creation_tokens: read_u64(entry, &["cacheCreationTokens"]),
                cache_read_tokens: read_u64(entry, &["cacheReadTokens"]),
                cached_input_tokens: read_u64(entry, &["cachedInputTokens"]),
                reasoning_output_tokens: read_u64(entry, &["reasoningOutputTokens"]),
                total_tokens: read_u64(entry, &["totalTokens"]),
                cost: read_f64(entry, cost_keys),
            })
        })
        .collect()
}

fn collect_models_map(session: &Value) -> Vec<OptionalModelBreakdown> {
    let Some(map) = session.get("models").and_then(Value::as_object) else {
        return Vec::new();
    };

    map.iter()
        .map(|(model, entry)| OptionalModelBreakdown {
            model: model.clone(),
            input_tokens: read_u64(entry, &["inputTokens"]),
            output_tokens: read_u64(entry, &["outputTokens"]),
            cache_creation_tokens: None,
            cache_read_tokens: None,
            cached_input_tokens: read_u64(entry, &["cachedInputTokens"]),
            reasoning_output_tokens: read_u64(entry, &["reasoningOutputTokens"]),
            total_tokens: read_u64(entry, &["totalTokens"]),
            cost: read_f64(entry, &["costUSD"]),
        })
        .collect()
}

fn add_string_row(rows: &mut Vec<(String, String)>, label: &str, value: &Value, keys: &[&str]) {
    if let Some(text) = read_string(value, keys) {
        rows.push((label.to_string(), text));
    }
}

fn add_models_used(rows: &mut Vec<(String, String)>, session: &Value) {
    let models = collect_models(session);
    if !models.is_empty() {
        rows.push(("Models".to_string(), models.join(", ")));
    }
}

fn add_optional_u64(rows: &mut Vec<(String, String)>, label: &str, value: Option<u64>) {
    if let Some(value) = value {
        rows.push((label.to_string(), format_integer(value)));
    }
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

fn render_receipt_template(
    template: &str,
    executor: &BaseCodingAgent,
    params: &RenderParams,
) -> String {
    let total_cost = params
        .summary_rows
        .iter()
        .find_map(|(k, v)| (k == "Total cost").then(|| v.clone()));
    let mut current_y = 250;
    let model_rows = render_model_rows(
        &params.model_breakdowns,
        total_cost.as_deref(),
        &mut current_y,
    );

    let footer_y = current_y + 55;
    let credits_y = footer_y + 79;
    let total_height = credits_y + 50;

    let mut session_id_short = params.session_id.clone();
    if session_id_short.len() > 24 {
        session_id_short.truncate(24);
        session_id_short.push_str("...");
    }

    let mut task_title_short = params.task_title.clone();
    if task_title_short.len() > 30 {
        task_title_short.truncate(30);
        task_title_short.push_str("...");
    }

    let mut svg_icon = agent_icon_svg(*executor).to_string();
    if svg_icon.contains("width=\"16\"") {
        svg_icon = svg_icon
            .replace("width=\"16\"", "width=\"64\"")
            .replace("height=\"16\"", "height=\"64\"");
    } else {
        svg_icon = svg_icon
            .replace("width=\"24\"", "width=\"64\"")
            .replace("height=\"24\"", "height=\"64\"");
    }

    render_template(
        template,
        &[
            ("{{icon_svg}}", &svg_icon),
            ("{{task_title_short}}", &escape_html(&task_title_short)),
            (
                "{{executor_name}}",
                &escape_html(executor.receipt_display_name()),
            ),
            ("{{session_id_short}}", &escape_html(&session_id_short)),
            (
                "{{done_at}}",
                &escape_html(
                    &params
                        .done_at
                        .with_timezone(&Local)
                        .format("%b %d, %Y, %I:%M %p")
                        .to_string(),
                ),
            ),
            ("{{model_rows}}", &model_rows),
            ("{{total_height}}", &total_height.to_string()),
            ("{{total_height_minus_8}}", &(total_height - 8).to_string()),
            ("{{total_height_plus_10}}", &(total_height + 10).to_string()),
            ("{{footer_y}}", &footer_y.to_string()),
            ("{{footer_y_minus_30}}", &(footer_y - 30).to_string()),
            ("{{credits_y}}", &credits_y.to_string()),
            ("{{credits_y_minus_30}}", &(credits_y - 30).to_string()),
        ],
    )
}

fn render_key_value_rows(rows: &[(String, String)], current_y: &mut i32) -> String {
    let mut svg = String::new();
    for (label, value) in rows {
        svg.push_str(&format!(
            "<g transform=\"translate(20, {y})\">\n              <text x=\"0\" y=\"0\" class=\"text\">{label}</text>\n              <text x=\"360\" y=\"0\" class=\"text\" text-anchor=\"end\">{value}</text>\n            </g>\n",
            y = *current_y,
            label = escape_html(label),
            value = escape_html(&truncate(value, 30)),
        ));
        *current_y += 28;
    }
    svg
}

fn render_model_rows(
    models: &[OptionalModelBreakdown],
    total_cost: Option<&str>,
    current_y: &mut i32,
) -> String {
    let mut svg = String::new();
    for breakdown in models {
        let title_y = *current_y + 12;
        svg.push_str(&format!(
            "<g transform=\"translate(20, {title_y})\">\n              <text x=\"0\" y=\"0\" class=\"text bold\">{model}</text>{cost}\n            </g>\n",
            model = escape_html(&truncate(&breakdown.model, 32)),
            cost = breakdown
                .cost
                .map(|cost| format!(
                    "\n              <text x=\"360\" y=\"0\" class=\"text bold\" text-anchor=\"end\">{}</text>",
                    format_currency(cost)
                ))
                .unwrap_or_default()
        ));
        *current_y += 37;

        let mut rows = Vec::new();
        add_optional_u64(&mut rows, "Input tokens", breakdown.input_tokens);
        add_optional_u64(&mut rows, "Output tokens", breakdown.output_tokens);
        add_optional_u64(&mut rows, "Cache write", breakdown.cache_creation_tokens);
        add_optional_u64(&mut rows, "Cache read", breakdown.cache_read_tokens);
        add_optional_u64(&mut rows, "Cached input", breakdown.cached_input_tokens);
        add_optional_u64(
            &mut rows,
            "Reasoning output",
            breakdown.reasoning_output_tokens,
        );
        add_optional_u64(&mut rows, "Total tokens", breakdown.total_tokens);
        svg.push_str(&render_key_value_rows(&rows, current_y));
    }

    if let Some(cost) = total_cost {
        *current_y += 14;
        svg.push_str(&format!(
            "<g transform=\"translate(20, {y})\">\n              <text x=\"0\" y=\"0\" class=\"text bold\">TOTAL</text>\n              <text x=\"360\" y=\"0\" class=\"text bold\" text-anchor=\"end\">{cost}</text>\n            </g>\n",
            y = *current_y,
            cost = escape_html(cost),
        ));
        *current_y += 37;
    }

    svg
}

fn read_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|field| match field {
            Value::Number(number) => number
                .as_u64()
                .or_else(|| number.as_i64().and_then(|n| u64::try_from(n).ok())),
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
            include_str!("../../../frontend/public/agents/claude-light.svg")
        }
        BaseCodingAgent::Codex => include_str!("../../../frontend/public/agents/codex-light.svg"),
        BaseCodingAgent::Opencode => {
            include_str!("../../../frontend/public/agents/opencode-light.svg")
        }
        BaseCodingAgent::Pi => include_str!("../../../frontend/public/agents/pi-mono-light.svg"),
        BaseCodingAgent::Gemini => include_str!("../../../frontend/public/agents/gemini-light.svg"),
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

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated = value.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
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

    use super::*;

    #[test]
    fn selects_executor_receipt_commands() {
        let claude = BaseCodingAgent::ClaudeCode
            .receipt_command_spec("session-1")
            .unwrap();
        assert_eq!(claude.primary.program, "ccusage");
        assert_eq!(
            claude.primary.args,
            vec!["session", "--id", "session-1", "--json"]
        );
        assert_eq!(claude.fallback.program, "npx");
        assert_eq!(
            claude.fallback.args,
            vec![
                "--yes",
                "ccusage@latest",
                "session",
                "--id",
                "session-1",
                "--json"
            ]
        );

        let codex = BaseCodingAgent::Codex
            .receipt_command_spec("session-1")
            .unwrap();
        assert_eq!(codex.primary.program, "ccusage-codex");
        assert_eq!(
            codex.primary.args,
            vec!["session", "--json", "--id", "session-1"]
        );
        assert_eq!(
            codex.fallback.args,
            vec![
                "--yes",
                "@ccusage/codex@latest",
                "session",
                "--json",
                "--id",
                "session-1"
            ]
        );

        assert!(
            BaseCodingAgent::Gemini
                .receipt_command_spec("unsupported")
                .is_none()
        );
    }

    #[tokio::test]
    async fn receipt_command_spec_uses_fallback_when_primary_fails() {
        let spec = ReceiptCommandSpec {
            primary: ReceiptCommand {
                program: "false".to_string(),
                args: Vec::new(),
            },
            fallback: ReceiptCommand {
                program: "printf".to_string(),
                args: vec!["{\"ok\":true}".to_string()],
            },
        };

        let output = run_receipt_command_spec(&spec).await.unwrap();
        assert_eq!(output, "{\"ok\":true}");
    }

    fn request() -> ReceiptRequest<'static> {
        ReceiptRequest {
            agent_session_id: "session-1",
            task_title: "Ship receipt",
            done_at: Utc.with_ymd_and_hms(2026, 5, 1, 10, 30, 0).unwrap(),
        }
    }

    #[test]
    fn parses_noisy_opencode_stdout_and_matches_session() {
        let raw = r#"[@ccusage/opencode] WARN loading pricing

{
  "sessions": [
    {
      "sessionID": "other",
      "totalTokens": 1
    },
    {
      "sessionID": "session-1",
      "sessionTitle": "Receipt work",
      "parentID": "parent-1",
      "inputTokens": 10,
      "outputTokens": 20,
      "totalTokens": 30,
      "totalCost": 0.5,
      "modelsUsed": ["opencode-model"]
    }
  ]
}"#;
        let value = parse_usage_json_value(raw).unwrap();
        let sessions = extract_sessions(&value);
        let session = match_usage_session(&sessions, "session-1").unwrap();
        let params = OpencodeReceiptParams::from_session(request(), session).unwrap();
        let svg = render_receipt_template(
            OPENCODE_TEMPLATE,
            &BaseCodingAgent::Opencode,
            &params.into_render_params(),
        );

        assert!(svg.contains("Receipt work"));
        assert!(svg.contains("Parent"));
        assert!(svg.contains("Total cost"));
        assert!(svg.contains("$0.50"));
        assert!(!svg.contains("Directory"));
    }

    #[test]
    fn codex_renders_only_codex_fields_and_keeps_real_zeroes() {
        let value = serde_json::json!({
            "sessions": [{
                "sessionId": "session-1",
                "sessionFile": "/tmp/session-1.jsonl",
                "directory": "/work/project",
                "inputTokens": 0,
                "cachedInputTokens": 12,
                "outputTokens": 5,
                "reasoningOutputTokens": 0,
                "totalTokens": 17,
                "costUSD": 0.0,
                "models": {
                    "gpt-5": {
                        "inputTokens": 0,
                        "cachedInputTokens": 12,
                        "outputTokens": 5,
                        "reasoningOutputTokens": 0,
                        "totalTokens": 17
                    }
                }
            }]
        });
        let sessions = extract_sessions(&value);
        let session = match_usage_session(&sessions, "session-1").unwrap();
        let params = CodexReceiptParams::from_session(request(), session).unwrap();
        let svg = render_receipt_template(
            CODEX_TEMPLATE,
            &BaseCodingAgent::Codex,
            &params.into_render_params(),
        );

        assert!(svg.contains("Session file"));
        assert!(svg.contains("Directory"));
        assert!(svg.contains("Cached input"));
        assert!(svg.contains("Reasoning output"));
        assert!(svg.contains(">0<"));
        assert!(svg.contains("$0.00"));
        assert!(!svg.contains("Project path"));
        assert!(!svg.contains("Cache write"));
    }

    #[test]
    fn missing_fields_are_not_rendered_as_zeroes() {
        let value = serde_json::json!({
            "sessionId": "session-1",
            "modelsUsed": ["claude-sonnet"],
            "modelBreakdowns": [{
                "modelName": "claude-sonnet",
                "cost": 1.25
            }]
        });
        let params = ClaudeCodeReceiptParams::from_session(request(), &value).unwrap();
        let svg = render_receipt_template(
            CLAUDE_CODE_TEMPLATE,
            &BaseCodingAgent::ClaudeCode,
            &params.into_render_params(),
        );

        assert!(svg.contains("claude-sonnet"));
        assert!(svg.contains("$1.25"));
        assert!(!svg.contains("Input tokens"));
        assert!(!svg.contains("Total tokens"));
        assert!(!svg.contains(">0<"));
    }

    #[test]
    fn pi_template_uses_pi_supported_fields() {
        let value = serde_json::json!({
            "sessionId": "session-1",
            "projectPath": "/repo",
            "source": "pi",
            "inputTokens": 3,
            "outputTokens": 4,
            "totalCost": 0.25,
            "modelsUsed": ["pi-model"],
            "modelBreakdowns": [{
                "modelName": "pi-model",
                "cost": 0.25
            }]
        });
        let params = PiReceiptParams::from_session(request(), &value).unwrap();
        let svg = render_receipt_template(
            PI_TEMPLATE,
            &BaseCodingAgent::Pi,
            &params.into_render_params(),
        );

        assert!(svg.contains("Project path"));
        assert!(svg.contains("Source"));
        assert!(svg.contains("pi-model"));
        assert!(svg.contains("$0.25"));
        assert!(!svg.contains("Session file"));
        assert!(!svg.contains("Parent"));
    }
}
