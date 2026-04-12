use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{
        IntoResponse,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use db::models::{
    task::{CreateTask, Task},
    workspace::{CreateWorkspace, Workspace},
    workspace_repo::{CreateWorkspaceRepo, WorkspaceRepo},
};
use deployment::Deployment;
use executors::{
    logs::{NormalizedEntryType, NormalizedLogEvent},
    profile::{ExecutorConfigs, ExecutorProfileId},
};
use futures_util::StreamExt;
use local_deployment::OpenAiProjectServerHandles;
use serde::{Deserialize, Serialize};
use services::services::{container::ContainerService, execution_log_hub::ExecutionLogHub};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::DeploymentImpl;

// ──────────────────────────────────────────────────────────────────────────────
// OpenAI-compatible wire types
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct OpenAiModel {
    id: String,
    object: String,
    created: u64,
    owned_by: String,
}

#[derive(Debug, Serialize)]
struct OpenAiModelList {
    object: String,
    data: Vec<OpenAiModel>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    fn as_text(&self) -> Option<&str> {
        match self {
            MessageContent::Text(s) => Some(s.as_str()),
            MessageContent::Parts(parts) => {
                // Return first text part
                parts.iter().find_map(|p| {
                    if p.r#type == "text" {
                        p.text.as_deref()
                    } else {
                        None
                    }
                })
            }
        }
    }

    fn has_non_text_parts(&self) -> bool {
        match self {
            MessageContent::Text(_) => false,
            MessageContent::Parts(parts) => parts.iter().any(|p| p.r#type != "text"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ContentPart {
    r#type: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    role: String,
    content: MessageContent,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ChatCompletionChoice {
    index: u32,
    message: AssistantMessage,
    finish_reason: String,
}

#[derive(Debug, Serialize)]
struct AssistantMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ChatCompletion {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Debug, Serialize)]
struct StreamDelta {
    role: Option<String>,
    content: Option<String>,
}

#[derive(Debug, Serialize)]
struct StreamChoice {
    index: u32,
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatCompletionChunk {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<StreamChoice>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn gen_id() -> String {
    format!("chatcmpl-{}", Uuid::new_v4().as_simple())
}

fn assistant_delta_for_upsert(
    sent_assistant_entries: &mut HashMap<usize, String>,
    index: usize,
    content: &str,
) -> Option<String> {
    if content.trim().is_empty() {
        return None;
    }

    let Some(previous) = sent_assistant_entries.get_mut(&index) else {
        sent_assistant_entries.insert(index, content.to_string());
        return Some(content.to_string());
    };

    if content == previous {
        return None;
    }

    let Some(delta) = content.strip_prefix(previous.as_str()) else {
        tracing::warn!(
            "OpenAI streaming cannot represent non-append assistant update at index {index}"
        );
        *previous = content.to_string();
        return None;
    };

    *previous = content.to_string();
    if delta.is_empty() {
        None
    } else {
        Some(delta.to_string())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Model list endpoint
// ──────────────────────────────────────────────────────────────────────────────

async fn list_models() -> axum::Json<OpenAiModelList> {
    let profiles = ExecutorConfigs::get_cached();
    let created = unix_now();

    let mut models = Vec::new();
    for (executor, config) in &profiles.executors {
        // Always include DEFAULT variant
        models.push(OpenAiModel {
            id: format!("{executor}-DEFAULT"),
            object: "model".to_string(),
            created,
            owned_by: "vibe-kanban".to_string(),
        });
        // Add other variants
        for variant in config.variant_names() {
            models.push(OpenAiModel {
                id: format!("{executor}-{variant}"),
                object: "model".to_string(),
                created,
                owned_by: "vibe-kanban".to_string(),
            });
        }
    }

    axum::Json(OpenAiModelList {
        object: "list".to_string(),
        data: models,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Parse model string into ExecutorProfileId
// ──────────────────────────────────────────────────────────────────────────────

fn parse_model(model: &str) -> Option<ExecutorProfileId> {
    // Try to find the longest matching executor prefix.
    // Model format: "{EXECUTOR}-{VARIANT}" or "{EXECUTOR}-DEFAULT"
    let profiles = ExecutorConfigs::get_cached();

    for executor in profiles.executors.keys() {
        let prefix = format!("{executor}-");
        if let Some(variant_part) = model.strip_prefix(&prefix) {
            let variant = if variant_part.eq_ignore_ascii_case("DEFAULT") {
                None
            } else {
                Some(variant_part.to_string())
            };
            return Some(ExecutorProfileId {
                executor: executor.clone(),
                variant,
            });
        }
    }

    None
}

// ──────────────────────────────────────────────────────────────────────────────
// State shared with the per-project server
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ProjectServerState {
    deployment: DeploymentImpl,
    project_id: Uuid,
}

// ──────────────────────────────────────────────────────────────────────────────
// Helper: wait for the execution log hub to appear
// ──────────────────────────────────────────────────────────────────────────────

type HubMap = std::sync::Arc<
    tokio::sync::RwLock<std::collections::HashMap<Uuid, std::sync::Arc<ExecutionLogHub>>>,
>;

async fn wait_for_hub(hubs: &HubMap, exec_id: Uuid) -> Option<std::sync::Arc<ExecutionLogHub>> {
    for _ in 0..20 {
        {
            let map = hubs.read().await;
            if let Some(hub) = map.get(&exec_id) {
                return Some(hub.clone());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

// ──────────────────────────────────────────────────────────────────────────────
// Chat completions endpoint
// ──────────────────────────────────────────────────────────────────────────────

async fn chat_completions(
    State(state): State<ProjectServerState>,
    axum::Json(req): axum::Json<ChatCompletionRequest>,
) -> impl IntoResponse {
    // Validate: reject non-text content parts
    for msg in &req.messages {
        if msg.content.has_non_text_parts() {
            return (
                StatusCode::BAD_REQUEST,
                "Only text content parts are supported",
            )
                .into_response();
        }
    }

    let executor_profile_id = match parse_model(&req.model) {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Unknown model: {}", req.model),
            )
                .into_response();
        }
    };

    // Build prompt from messages
    let transcript: String = req
        .messages
        .iter()
        .filter_map(|m| m.content.as_text().map(|t| format!("[{}]: {}", m.role, t)))
        .collect::<Vec<_>>()
        .join("\n");

    // Use the last user message as the task title
    let last_user_text = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.as_text())
        .unwrap_or("API Task")
        .to_string();

    let title = if last_user_text.len() > 100 {
        format!("{}...", &last_user_text[..100])
    } else {
        last_user_text.clone()
    };

    let pool = &state.deployment.db().pool;

    // Get project repositories
    let repos = match state
        .deployment
        .project()
        .get_repositories(pool, state.project_id)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                "Failed to get repositories for project {}: {}",
                state.project_id,
                e
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to get project repositories",
            )
                .into_response();
        }
    };

    if repos.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "Project has no repositories configured",
        )
            .into_response();
    }

    // Create task
    let task_id = Uuid::new_v4();
    let create_task = CreateTask {
        project_id: state.project_id,
        title: title.clone(),
        description: Some(transcript),
        status: None,
        parent_workspace_id: None,
        source_cron_task_id: None,
        image_ids: None,
    };

    let task = match Task::create(pool, &create_task, task_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Failed to create task: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to create task").into_response();
        }
    };

    // Determine agent_working_dir
    let agent_working_dir = if repos.len() == 1 {
        Some(repos[0].name.clone())
    } else {
        None
    };

    // Create workspace
    let workspace_id = Uuid::new_v4();
    let git_branch_name = state
        .deployment
        .container()
        .git_branch_from_workspace(&workspace_id, &task.title)
        .await;

    let workspace = match Workspace::create(
        pool,
        &CreateWorkspace {
            branch: git_branch_name,
            agent_working_dir,
        },
        workspace_id,
        task.id,
    )
    .await
    {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("Failed to create workspace: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create workspace",
            )
                .into_response();
        }
    };

    // Associate repos with workspace - use current branch from git
    let git = state.deployment.git().clone();
    let workspace_repos: Vec<CreateWorkspaceRepo> = repos
        .iter()
        .map(|r| {
            let target_branch = git
                .get_current_branch(&r.path)
                .unwrap_or_else(|_| "main".to_string());
            CreateWorkspaceRepo {
                repo_id: r.id,
                target_branch,
            }
        })
        .collect();

    if let Err(e) = WorkspaceRepo::create_many(pool, workspace.id, &workspace_repos).await {
        tracing::error!("Failed to associate repos with workspace: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to configure workspace repos",
        )
            .into_response();
    }

    // Start execution
    let exec_process = match state
        .deployment
        .container()
        .start_workspace(&workspace, executor_profile_id.clone())
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Failed to start workspace: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to start task execution",
            )
                .into_response();
        }
    };

    let exec_id = exec_process.id;
    let model_str = req.model.clone();

    if req.stream {
        let hubs = state.deployment.container().execution_log_hubs().clone();
        let (tx, rx) = mpsc::channel::<Result<Event, std::io::Error>>(64);

        tokio::spawn(async move {
            let hub = wait_for_hub(&hubs, exec_id).await;
            let Some(hub) = hub else {
                tracing::warn!("No execution log hub found for exec {}", exec_id);
                return;
            };

            let completion_id = gen_id();
            let created = unix_now();

            // Send role header chunk
            let header = ChatCompletionChunk {
                id: completion_id.clone(),
                object: "chat.completion.chunk".to_string(),
                created,
                model: model_str.clone(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: StreamDelta {
                        role: Some("assistant".to_string()),
                        content: Some(String::new()),
                    },
                    finish_reason: None,
                }],
            };
            if let Ok(data) = serde_json::to_string(&header) {
                let _ = tx.send(Ok(Event::default().data(data))).await;
            }

            let mut stream = hub.normalized_history_plus_stream();
            let mut sent_assistant_entries = HashMap::new();
            loop {
                match stream.next().await {
                    Some(Ok(NormalizedLogEvent::UpsertEntry { index, entry })) => {
                        if matches!(entry.entry_type, NormalizedEntryType::AssistantMessage)
                            && let Some(delta) = assistant_delta_for_upsert(
                                &mut sent_assistant_entries,
                                index,
                                &entry.content,
                            )
                        {
                            let chunk = ChatCompletionChunk {
                                id: completion_id.clone(),
                                object: "chat.completion.chunk".to_string(),
                                created,
                                model: model_str.clone(),
                                choices: vec![StreamChoice {
                                    index: 0,
                                    delta: StreamDelta {
                                        role: None,
                                        content: Some(delta),
                                    },
                                    finish_reason: None,
                                }],
                            };
                            if let Ok(data) = serde_json::to_string(&chunk) {
                                if tx.send(Ok(Event::default().data(data))).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Some(Ok(NormalizedLogEvent::Finished)) | None => {
                        let stop_chunk = ChatCompletionChunk {
                            id: completion_id.clone(),
                            object: "chat.completion.chunk".to_string(),
                            created,
                            model: model_str.clone(),
                            choices: vec![StreamChoice {
                                index: 0,
                                delta: StreamDelta {
                                    role: None,
                                    content: None,
                                },
                                finish_reason: Some("stop".to_string()),
                            }],
                        };
                        if let Ok(data) = serde_json::to_string(&stop_chunk) {
                            let _ = tx.send(Ok(Event::default().data(data))).await;
                        }
                        let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
                        return;
                    }
                    Some(Ok(NormalizedLogEvent::RemoveEntry { index })) => {
                        sent_assistant_entries.remove(&index);
                    }
                    Some(Err(_)) => {}
                }
            }
        });

        let sse_stream = ReceiverStream::new(rx);
        Sse::new(sse_stream).into_response()
    } else {
        // Non-streaming: wait for completion
        let hubs = state.deployment.container().execution_log_hubs().clone();

        let assistant_text = tokio::spawn(async move {
            let hub = wait_for_hub(&hubs, exec_id).await;
            let Some(hub) = hub else {
                tracing::warn!("No execution log hub found for exec {}", exec_id);
                return String::new();
            };

            let mut stream = hub.normalized_history_plus_stream();
            let mut last_assistant = String::new();

            while let Some(event) = stream.next().await {
                match event {
                    Ok(NormalizedLogEvent::UpsertEntry { entry, .. }) => {
                        if matches!(entry.entry_type, NormalizedEntryType::AssistantMessage)
                            && !entry.content.trim().is_empty()
                        {
                            last_assistant = entry.content.clone();
                        }
                    }
                    Ok(NormalizedLogEvent::Finished) => {
                        break;
                    }
                    _ => {}
                }
            }

            last_assistant
        })
        .await
        .unwrap_or_default();

        let completion = ChatCompletion {
            id: gen_id(),
            object: "chat.completion".to_string(),
            created: unix_now(),
            model: model_str,
            choices: vec![ChatCompletionChoice {
                index: 0,
                message: AssistantMessage {
                    role: "assistant".to_string(),
                    content: assistant_text,
                },
                finish_reason: "stop".to_string(),
            }],
        };

        axum::Json(completion).into_response()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Server lifecycle management
// ──────────────────────────────────────────────────────────────────────────────

fn build_project_router(state: ProjectServerState) -> Router {
    Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state)
}

/// Start (or restart) the OpenAI-compat server for a single project.
/// Stops any existing server for that project first.
/// Returns an error if binding the port fails.
pub async fn start_project_server(
    project_id: &str,
    port: u16,
    deployment: DeploymentImpl,
    handles: &OpenAiProjectServerHandles,
) -> Result<(), String> {
    // Stop existing server for this project
    stop_project_server(project_id, handles).await;

    let project_uuid = match Uuid::parse_str(project_id) {
        Ok(id) => id,
        Err(e) => return Err(format!("Invalid project id: {e}")),
    };

    let bind_addr = format!("127.0.0.1:{port}");
    let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
        Ok(l) => l,
        Err(e) => return Err(format!("Failed to bind port {port}: {e}")),
    };

    let project_id_owned = project_id.to_string();
    tracing::info!(
        "OpenAI-compat server for project {project_id_owned} started on http://{bind_addr}"
    );

    let state = ProjectServerState {
        deployment,
        project_id: project_uuid,
    };
    let router = build_project_router(state);

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!("OpenAI-compat server error for project {project_id_owned}: {e}");
        }
    });

    handles.lock().await.insert(project_id.to_string(), handle);
    Ok(())
}

/// Stop the OpenAI-compat server for a single project.
pub async fn stop_project_server(project_id: &str, handles: &OpenAiProjectServerHandles) {
    let handle = handles.lock().await.remove(project_id);
    if let Some(h) = handle {
        h.abort();
        let _ = h.await;
        tracing::info!("OpenAI-compat server for project {project_id} stopped");
    }
}

/// Start all enabled project servers from config on backend startup.
pub async fn start_all_from_config(
    deployment: &DeploymentImpl,
    handles: &OpenAiProjectServerHandles,
) {
    let configs = {
        let config = deployment.config().read().await;
        config.openai_compatible_api_projects.clone()
    };

    for (project_id, api_config) in &configs {
        if !api_config.enabled {
            continue;
        }

        // Verify project exists in DB
        let uuid = match Uuid::parse_str(project_id) {
            Ok(id) => id,
            Err(_) => {
                tracing::warn!("OpenAI API config has invalid project ID: {project_id}, skipping");
                continue;
            }
        };

        match db::models::project::Project::find_by_id(&deployment.db().pool, uuid).await {
            Ok(Some(_)) => {
                if let Err(e) =
                    start_project_server(project_id, api_config.port, deployment.clone(), handles)
                        .await
                {
                    tracing::warn!(
                        "Failed to start OpenAI-compat server for project {project_id}: {e}"
                    );
                }
            }
            Ok(None) => {
                tracing::warn!(
                    "OpenAI API config references non-existent project {project_id}, skipping"
                );
            }
            Err(e) => {
                tracing::warn!("Error checking project {project_id} for OpenAI API startup: {e}");
            }
        }
    }
}

// Keep the type re-exported for routes
pub use services::services::config::ProjectOpenAiApiConfig as OpenAiApiConfig;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::assistant_delta_for_upsert;

    #[test]
    fn assistant_delta_for_upsert_sends_only_appended_suffix() {
        let mut sent = HashMap::new();

        assert_eq!(
            assistant_delta_for_upsert(&mut sent, 3, "你好"),
            Some("你好".to_string())
        );
        assert_eq!(
            assistant_delta_for_upsert(&mut sent, 3, "你好。需要"),
            Some("。需要".to_string())
        );
        assert_eq!(assistant_delta_for_upsert(&mut sent, 3, "你好。需要"), None);
        assert_eq!(
            assistant_delta_for_upsert(&mut sent, 3, "你好。需要我"),
            Some("我".to_string())
        );
    }

    #[test]
    fn assistant_delta_for_upsert_tracks_entries_independently() {
        let mut sent = HashMap::new();

        assert_eq!(
            assistant_delta_for_upsert(&mut sent, 1, "first"),
            Some("first".to_string())
        );
        assert_eq!(
            assistant_delta_for_upsert(&mut sent, 2, "second"),
            Some("second".to_string())
        );
        assert_eq!(
            assistant_delta_for_upsert(&mut sent, 1, "first entry"),
            Some(" entry".to_string())
        );
    }

    #[test]
    fn assistant_delta_for_upsert_skips_non_append_replacements() {
        let mut sent = HashMap::new();

        assert_eq!(
            assistant_delta_for_upsert(&mut sent, 1, "hello"),
            Some("hello".to_string())
        );
        assert_eq!(assistant_delta_for_upsert(&mut sent, 1, "goodbye"), None);
        assert_eq!(
            assistant_delta_for_upsert(&mut sent, 1, "goodbye!"),
            Some("!".to_string())
        );
    }
}
