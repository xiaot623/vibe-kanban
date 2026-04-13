use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
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
    execution_process::{ExecutionProcess, ExecutionProcessRunReason, ExecutionProcessStatus},
    session::Session,
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
use services::services::{
    container::ContainerService,
    execution_log_hub::ExecutionLogHub,
    queued_message::{QueueWaitError, QueuedMessageWaiter},
};
use tokio::sync::{RwLock, mpsc};
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexedChatMessage {
    role: String,
    content: String,
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
    session_index: OpenAiSessionIndex,
}

#[derive(Debug, Clone)]
struct OpenAiIndexedSession {
    session_id: Uuid,
    messages: Vec<IndexedChatMessage>,
    revision: u64,
}

#[derive(Clone, Default)]
struct OpenAiSessionIndex {
    entries: Arc<RwLock<Vec<OpenAiIndexedSession>>>,
    revisions: Arc<AtomicU64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrefixMatch {
    session_id: Uuid,
    prefix_len: usize,
}

impl OpenAiSessionIndex {
    async fn record_messages(&self, session_id: Uuid, messages: Vec<IndexedChatMessage>) {
        let revision = self.revisions.fetch_add(1, Ordering::Relaxed) + 1;
        let mut entries = self.entries.write().await;
        entries.push(OpenAiIndexedSession {
            session_id,
            messages,
            revision,
        });
    }

    async fn find_longest_prefix_match(
        &self,
        request_messages: &[IndexedChatMessage],
    ) -> Option<PrefixMatch> {
        let entries = self.entries.read().await;
        select_longest_prefix_match(&entries, request_messages)
    }
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

fn indexable_messages(messages: &[ChatMessage]) -> Vec<IndexedChatMessage> {
    let mut indexed = Vec::new();

    for message in messages {
        let Some(content) = message.content.as_text() else {
            continue;
        };
        let indexed_message = IndexedChatMessage {
            role: message.role.clone(),
            content: content.to_string(),
        };

        if indexed_message.role == "user"
            && indexed.last().is_some_and(|previous: &IndexedChatMessage| {
                previous.role == "user" && previous.content == indexed_message.content
            })
        {
            continue;
        }

        indexed.push(indexed_message);
    }

    indexed
}

fn transcript_from_messages(messages: &[IndexedChatMessage]) -> String {
    messages
        .iter()
        .map(|message| format!("[{}]: {}", message.role, message.content))
        .collect::<Vec<_>>()
        .join("\n")
}

fn follow_up_prompt_from_messages(messages: &[IndexedChatMessage]) -> String {
    let trailing_user_start = messages
        .iter()
        .rposition(|message| message.role != "user")
        .map_or(0, |index| index + 1);
    let trailing_user_messages = &messages[trailing_user_start..];

    if trailing_user_messages.is_empty() {
        return transcript_from_messages(messages);
    }

    trailing_user_messages
        .iter()
        .map(|message| message.content.as_str())
        .filter(|content| !content.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn select_longest_prefix_match(
    entries: &[OpenAiIndexedSession],
    request_messages: &[IndexedChatMessage],
) -> Option<PrefixMatch> {
    entries
        .iter()
        .filter(|entry| {
            entry.messages.len() < request_messages.len()
                && request_messages.starts_with(entry.messages.as_slice())
        })
        .max_by_key(|entry| (entry.messages.len(), entry.revision))
        .map(|entry| PrefixMatch {
            session_id: entry.session_id,
            prefix_len: entry.messages.len(),
        })
}

async fn session_has_running_non_dev_server_processes(
    deployment: &DeploymentImpl,
    session_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let processes =
        ExecutionProcess::find_by_session_id(&deployment.db().pool, session_id, false).await?;
    Ok(processes.iter().any(|process| {
        process.status == ExecutionProcessStatus::Running
            && process.run_reason != ExecutionProcessRunReason::DevServer
    }))
}

fn map_wait_error(error: QueueWaitError) -> (StatusCode, String) {
    match error {
        QueueWaitError::Overwritten => (
            StatusCode::CONFLICT,
            "Queued follow-up was replaced by a newer request".to_string(),
        ),
        QueueWaitError::Cancelled => (
            StatusCode::CONFLICT,
            "Queued follow-up was cancelled before it started".to_string(),
        ),
        QueueWaitError::Discarded => (
            StatusCode::CONFLICT,
            "Queued follow-up was discarded before it started".to_string(),
        ),
        QueueWaitError::StartFailed(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}

async fn wait_for_execution_result(deployment: &DeploymentImpl, exec_id: Uuid) -> Option<String> {
    let hubs = deployment.container().execution_log_hubs().clone();
    let hub = wait_for_hub(&hubs, exec_id).await?;
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
            Ok(NormalizedLogEvent::Finished) => break,
            _ => {}
        }
    }

    Some(last_assistant)
}

async fn wait_for_execution_stream(
    deployment: &DeploymentImpl,
    exec_id: Uuid,
    model: String,
) -> Sse<ReceiverStream<Result<Event, std::io::Error>>> {
    let hubs = deployment.container().execution_log_hubs().clone();
    let (tx, rx) = mpsc::channel::<Result<Event, std::io::Error>>(64);

    tokio::spawn(async move {
        let hub = wait_for_hub(&hubs, exec_id).await;
        let Some(hub) = hub else {
            tracing::warn!("No execution log hub found for exec {}", exec_id);
            return;
        };

        let completion_id = gen_id();
        let created = unix_now();

        let header = ChatCompletionChunk {
            id: completion_id.clone(),
            object: "chat.completion.chunk".to_string(),
            created,
            model: model.clone(),
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
                            model: model.clone(),
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
                        model: model.clone(),
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
    Sse::new(sse_stream)
}

async fn resolve_queued_execution(
    waiter: QueuedMessageWaiter,
) -> Result<Uuid, (StatusCode, String)> {
    waiter.wait_for_start().await.map_err(map_wait_error)
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

    let indexed_messages = indexable_messages(&req.messages);

    if indexed_messages.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "At least one text message is required",
        )
            .into_response();
    }

    if let Some(prefix_match) = state
        .session_index
        .find_longest_prefix_match(&indexed_messages)
        .await
    {
        let pool = &state.deployment.db().pool;
        let session = match Session::find_by_id(pool, prefix_match.session_id).await {
            Ok(Some(session)) => session,
            Ok(None) => {
                tracing::warn!(
                    "Indexed OpenAI session {} no longer exists; ignoring prefix match",
                    prefix_match.session_id
                );
                return create_new_openai_session(
                    state,
                    req,
                    executor_profile_id,
                    indexed_messages,
                )
                .await;
            }
            Err(err) => {
                tracing::error!(
                    "Failed to load indexed OpenAI session {}: {}",
                    prefix_match.session_id,
                    err
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to load matched session",
                )
                    .into_response();
            }
        };

        let workspace = match Workspace::find_by_id(pool, session.workspace_id).await {
            Ok(Some(workspace)) => workspace,
            Ok(None) => {
                tracing::warn!(
                    "Indexed OpenAI workspace {} no longer exists; ignoring prefix match",
                    session.workspace_id
                );
                return create_new_openai_session(
                    state,
                    req,
                    executor_profile_id,
                    indexed_messages,
                )
                .await;
            }
            Err(err) => {
                tracing::error!(
                    "Failed to load indexed workspace {}: {}",
                    session.workspace_id,
                    err
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to load matched workspace",
                )
                    .into_response();
            }
        };

        let follow_up_messages = &indexed_messages[prefix_match.prefix_len..];
        let follow_up_prompt = follow_up_prompt_from_messages(follow_up_messages);

        let exec_id =
            match session_has_running_non_dev_server_processes(&state.deployment, session.id).await
            {
                Ok(true) => {
                    let (queued_message, waiter) = state
                        .deployment
                        .queued_message_service()
                        .queue_message_with_waiter(
                            session.id,
                            db::models::scratch::DraftFollowUpData {
                                message: follow_up_prompt,
                                variant: executor_profile_id.variant.clone(),
                                executor: None,
                            },
                        );
                    tracing::info!(
                        "Queued OpenAI follow-up for session {} at {}",
                        queued_message.session_id,
                        queued_message.queued_at
                    );

                    match resolve_queued_execution(waiter).await {
                        Ok(exec_id) => exec_id,
                        Err((status, message)) => return (status, message).into_response(),
                    }
                }
                Ok(false) => match state
                    .deployment
                    .container()
                    .start_follow_up_execution(
                        &workspace,
                        &session,
                        follow_up_prompt,
                        executor_profile_id.variant.clone(),
                        Some(executor_profile_id.executor),
                    )
                    .await
                {
                    Ok(process) => process.id,
                    Err(err) => {
                        tracing::error!(
                            "Failed to start OpenAI follow-up for session {}: {}",
                            session.id,
                            err
                        );
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Failed to start follow-up execution",
                        )
                            .into_response();
                    }
                },
                Err(err) => {
                    tracing::error!(
                        "Failed to inspect running processes for session {}: {}",
                        session.id,
                        err
                    );
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to inspect session state",
                    )
                        .into_response();
                }
            };

        return respond_with_execution(state, req, indexed_messages, session.id, exec_id).await;
    }

    create_new_openai_session(state, req, executor_profile_id, indexed_messages).await
}

async fn create_new_openai_session(
    state: ProjectServerState,
    req: ChatCompletionRequest,
    executor_profile_id: ExecutorProfileId,
    indexed_messages: Vec<IndexedChatMessage>,
) -> axum::response::Response {
    let transcript = transcript_from_messages(&indexed_messages);

    let title = "API Trigger Task".to_string();

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
    let session = match Session::find_latest_by_workspace_id(pool, workspace.id).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            tracing::error!(
                "OpenAI workspace {} has no session after startup",
                workspace.id
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to find created session",
            )
                .into_response();
        }
        Err(err) => {
            tracing::error!(
                "Failed to load created session for workspace {}: {}",
                workspace.id,
                err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to find created session",
            )
                .into_response();
        }
    };

    state
        .session_index
        .record_messages(session.id, indexed_messages.clone())
        .await;

    respond_with_execution(state, req, indexed_messages, session.id, exec_process.id).await
}

async fn respond_with_execution(
    state: ProjectServerState,
    req: ChatCompletionRequest,
    request_messages: Vec<IndexedChatMessage>,
    session_id: Uuid,
    exec_id: Uuid,
) -> axum::response::Response {
    if req.stream {
        let stream = wait_for_execution_stream(&state.deployment, exec_id, req.model).await;
        let deployment = state.deployment.clone();
        let session_index = state.session_index.clone();
        tokio::spawn(async move {
            let assistant_text = wait_for_execution_result(&deployment, exec_id)
                .await
                .unwrap_or_default();
            let mut indexed_with_assistant = request_messages;
            indexed_with_assistant.push(IndexedChatMessage {
                role: "assistant".to_string(),
                content: assistant_text,
            });
            session_index
                .record_messages(session_id, indexed_with_assistant)
                .await;
        });
        return stream.into_response();
    }

    let assistant_text = wait_for_execution_result(&state.deployment, exec_id)
        .await
        .unwrap_or_default();

    let mut indexed_with_assistant = request_messages;
    indexed_with_assistant.push(IndexedChatMessage {
        role: "assistant".to_string(),
        content: assistant_text.clone(),
    });
    state
        .session_index
        .record_messages(session_id, indexed_with_assistant)
        .await;

    let completion = ChatCompletion {
        id: gen_id(),
        object: "chat.completion".to_string(),
        created: unix_now(),
        model: req.model,
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
        session_index: OpenAiSessionIndex::default(),
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

    use uuid::Uuid;

    use super::{
        ChatMessage, IndexedChatMessage, MessageContent, OpenAiIndexedSession, PrefixMatch,
        assistant_delta_for_upsert, follow_up_prompt_from_messages, indexable_messages,
        select_longest_prefix_match, transcript_from_messages,
    };

    fn msg(role: &str, content: &str) -> IndexedChatMessage {
        IndexedChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    fn chat_msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: MessageContent::Text(content.to_string()),
        }
    }

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

    #[test]
    fn prefix_match_requires_strictly_longer_request() {
        let session_id = Uuid::new_v4();
        let entries = vec![OpenAiIndexedSession {
            session_id,
            messages: vec![msg("system", "s"), msg("user", "u1")],
            revision: 1,
        }];

        assert_eq!(
            select_longest_prefix_match(
                &entries,
                &[
                    msg("system", "s"),
                    msg("user", "u1"),
                    msg("assistant", "a1"),
                    msg("user", "u2")
                ]
            ),
            Some(PrefixMatch {
                session_id,
                prefix_len: 2,
            })
        );

        assert_eq!(
            select_longest_prefix_match(&entries, &[msg("system", "s"), msg("user", "u1")]),
            None
        );
    }

    #[test]
    fn prefix_match_ignores_non_prefix_candidates() {
        let entries = vec![OpenAiIndexedSession {
            session_id: Uuid::new_v4(),
            messages: vec![msg("user", "x")],
            revision: 1,
        }];

        assert_eq!(
            select_longest_prefix_match(&entries, &[msg("user", "y"), msg("assistant", "z")]),
            None
        );
    }

    #[test]
    fn prefix_match_prefers_longest_then_latest() {
        let shorter_id = Uuid::new_v4();
        let older_long_id = Uuid::new_v4();
        let newer_long_id = Uuid::new_v4();
        let request = vec![
            msg("system", "s"),
            msg("user", "u1"),
            msg("assistant", "a1"),
            msg("user", "u2"),
        ];

        let entries = vec![
            OpenAiIndexedSession {
                session_id: shorter_id,
                messages: vec![msg("system", "s")],
                revision: 1,
            },
            OpenAiIndexedSession {
                session_id: older_long_id,
                messages: vec![msg("system", "s"), msg("user", "u1")],
                revision: 2,
            },
            OpenAiIndexedSession {
                session_id: newer_long_id,
                messages: vec![msg("system", "s"), msg("user", "u1")],
                revision: 3,
            },
        ];

        assert_eq!(
            select_longest_prefix_match(&entries, &request),
            Some(PrefixMatch {
                session_id: newer_long_id,
                prefix_len: 2,
            })
        );
    }

    #[test]
    fn indexable_messages_collapses_adjacent_duplicate_user_messages() {
        let messages = vec![
            chat_msg("system", "rules"),
            chat_msg("user", "更简洁一点"),
            chat_msg("user", "更简洁一点"),
            chat_msg("assistant", "ok"),
            chat_msg("user", "更简洁一点"),
        ];

        assert_eq!(
            indexable_messages(&messages),
            vec![
                msg("system", "rules"),
                msg("user", "更简洁一点"),
                msg("assistant", "ok"),
                msg("user", "更简洁一点"),
            ]
        );
    }

    #[test]
    fn follow_up_prompt_uses_only_trailing_user_messages() {
        let messages = vec![
            msg("assistant", "previous answer"),
            msg("user", "更简洁一点"),
        ];

        assert_eq!(follow_up_prompt_from_messages(&messages), "更简洁一点");
    }

    #[test]
    fn follow_up_prompt_falls_back_to_transcript_without_trailing_user() {
        let messages = vec![msg("assistant", "previous answer")];

        assert_eq!(
            follow_up_prompt_from_messages(&messages),
            "[assistant]: previous answer"
        );
    }

    #[test]
    fn transcript_uses_only_follow_up_suffix_messages() {
        let all = vec![
            msg("system", "rules"),
            msg("user", "u1"),
            msg("assistant", "a1"),
            msg("user", "u2"),
        ];

        assert_eq!(
            transcript_from_messages(&all[2..]),
            "[assistant]: a1\n[user]: u2"
        );
    }
}
