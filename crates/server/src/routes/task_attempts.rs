pub mod codex_setup;
pub mod gh_cli_setup;
pub mod images;
pub mod pr;
pub mod util;
pub mod workspace_summary;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use axum::{
    Extension, Json, Router,
    extract::{
        Query, State,
        ws::{WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, header},
    middleware::from_fn_with_state,
    response::{IntoResponse, Json as ResponseJson, Response},
    routing::{get, patch, post, put},
};
use db::models::{
    coding_agent_turn::CodingAgentTurn,
    execution_process::{ExecutionProcess, ExecutionProcessRunReason, ExecutionProcessStatus},
    merge::{Merge, MergeStatus, PrMerge, PullRequestInfo},
    project::SearchResult,
    repo::{Repo, RepoError},
    session::{CreateSession, Session},
    task::{CreateTask, Task, TaskRelationships, TaskStatus, TaskWithAttemptStatus},
    workspace::{CreateWorkspace, ReviewCommand, Workspace, WorkspaceError},
    workspace_repo::{CreateWorkspaceRepo, RepoWithTargetBranch, WorkspaceRepo},
};
use deployment::Deployment;
use executors::{
    actions::{
        ExecutorAction, ExecutorActionType,
        script::{ScriptContext, ScriptRequest, ScriptRequestLanguage},
    },
    executors::{CodingAgent, ExecutorError},
    profile::{ExecutorConfigs, ExecutorProfileId},
};
use git2::BranchType;
use markdown2pdf::{MdpError, config::ConfigSource};
use serde::{Deserialize, Serialize};
use services::services::{
    container::ContainerService,
    context_archive,
    file_search::SearchQuery,
    git::{ConflictOp, DiffTarget, GitCliError, GitServiceError},
    workspace_manager::WorkspaceManager,
};
use sqlx::Error as SqlxError;
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{
    DeploymentImpl, error::ApiError, middleware::load_workspace_middleware,
    routes::task_attempts::gh_cli_setup::GhCliSetupError,
};

#[derive(Debug, Deserialize, Serialize, TS)]
pub struct RebaseTaskAttemptRequest {
    pub repo_id: Uuid,
    pub old_base_branch: Option<String>,
    pub new_base_branch: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, TS)]
pub struct AbortConflictsRequest {
    pub repo_id: Uuid,
}

#[derive(Debug, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum GitOperationError {
    MergeConflicts { message: String, op: ConflictOp },
    RebaseInProgress,
}

#[derive(Debug, Deserialize)]
pub struct TaskAttemptQuery {
    pub task_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct DiffStreamQuery {
    #[serde(default)]
    pub stats_only: bool,
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceStreamQuery {
    pub archived: Option<bool>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportContextFormat {
    Md,
    Pdf,
}

#[derive(Debug, Deserialize)]
pub struct ExportContextQuery {
    pub format: ExportContextFormat,
}

#[derive(Debug, Deserialize, TS)]
pub struct UpdateWorkspace {
    pub archived: Option<bool>,
    pub pinned: Option<bool>,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, TS)]
pub struct SaveReviewCommandRequest {
    pub markdown_text: String,
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, TS)]
pub struct UpdateReviewCommandStatusRequest {
    pub solved: bool,
    pub reason: Option<String>,
}

pub async fn get_task_attempts(
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<TaskAttemptQuery>,
) -> Result<ResponseJson<ApiResponse<Vec<Workspace>>>, ApiError> {
    let pool = &deployment.db().pool;
    let workspaces = Workspace::fetch_all(pool, query.task_id).await?;
    Ok(ResponseJson(ApiResponse::success(workspaces)))
}

pub async fn get_workspace_count(
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<i64>>, ApiError> {
    let pool = &deployment.db().pool;
    let count = Workspace::count_all(pool).await?;
    Ok(ResponseJson(ApiResponse::success(count)))
}

pub async fn get_task_attempt(
    Extension(workspace): Extension<Workspace>,
) -> Result<ResponseJson<ApiResponse<Workspace>>, ApiError> {
    Ok(ResponseJson(ApiResponse::success(workspace)))
}

pub async fn update_workspace(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<UpdateWorkspace>,
) -> Result<ResponseJson<ApiResponse<Workspace>>, ApiError> {
    let pool = &deployment.db().pool;
    Workspace::update(
        pool,
        workspace.id,
        request.archived,
        request.pinned,
        request.name.as_deref(),
    )
    .await?;
    let updated = Workspace::find_by_id(pool, workspace.id)
        .await?
        .ok_or(WorkspaceError::TaskNotFound)?;
    Ok(ResponseJson(ApiResponse::success(updated)))
}

pub async fn get_review_command(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<Option<ReviewCommand>>>, ApiError> {
    let pool = &deployment.db().pool;
    let command = Workspace::get_review_command(pool, workspace.id).await?;
    Ok(ResponseJson(ApiResponse::success(command)))
}

pub async fn get_unsolved_review_command(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<Vec<ReviewCommand>>>, ApiError> {
    let pool = &deployment.db().pool;
    let commands = Workspace::list_unsolved_review_commands(pool, workspace.id).await?;
    Ok(ResponseJson(ApiResponse::success(commands)))
}

pub async fn save_review_command(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<SaveReviewCommandRequest>,
) -> Result<ResponseJson<ApiResponse<ReviewCommand>>, ApiError> {
    let markdown_text = payload.markdown_text.trim();
    if markdown_text.is_empty() {
        return Err(ApiError::BadRequest(
            "Review command markdown must not be empty".to_string(),
        ));
    }

    let reason = payload
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let pool = &deployment.db().pool;
    let command = Workspace::save_review_command(pool, workspace.id, markdown_text, reason).await?;
    Ok(ResponseJson(ApiResponse::success(command)))
}

pub async fn delete_review_command(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    let pool = &deployment.db().pool;
    Workspace::clear_review_command(pool, workspace.id).await?;
    Ok(ResponseJson(ApiResponse::success(())))
}

pub async fn update_review_command_status(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<UpdateReviewCommandStatusRequest>,
) -> Result<ResponseJson<ApiResponse<ReviewCommand>>, ApiError> {
    let reason = payload
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let pool = &deployment.db().pool;
    let command = Workspace::set_review_command_status(pool, workspace.id, payload.solved, reason)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => {
                ApiError::BadRequest("Review command not found for this workspace".to_string())
            }
            other => ApiError::Database(other),
        })?;
    Ok(ResponseJson(ApiResponse::success(command)))
}

#[derive(Debug, Serialize, Deserialize, ts_rs::TS)]
pub struct CreateTaskAttemptBody {
    pub task_id: Uuid,
    pub executor_profile_id: ExecutorProfileId,
    pub repos: Vec<WorkspaceRepoInput>,
}

#[derive(Debug, Deserialize, Default)]
pub struct CreateTaskAttemptQuery {
    pub expected_task_status: Option<TaskStatus>,
}

#[derive(Debug, Serialize, Deserialize, ts_rs::TS)]
pub struct WorkspaceRepoInput {
    pub repo_id: Uuid,
    pub target_branch: String,
}

#[derive(Debug, Deserialize, Serialize, TS)]
pub struct RunAgentSetupRequest {
    pub executor_profile_id: ExecutorProfileId,
}

#[derive(Debug, Deserialize, Serialize, TS)]
pub struct StartReviewSubtaskRequest {
    pub executor_profile_id: ExecutorProfileId,
}

#[derive(Debug, Serialize, TS)]
pub struct RunAgentSetupResponse {}

#[axum::debug_handler]
pub async fn create_task_attempt(
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<CreateTaskAttemptQuery>,
    Json(payload): Json<CreateTaskAttemptBody>,
) -> Result<ResponseJson<ApiResponse<Workspace>>, ApiError> {
    let executor_profile_id = payload.executor_profile_id.clone();

    if payload.repos.is_empty() {
        return Err(ApiError::BadRequest(
            "At least one repository is required".to_string(),
        ));
    }

    let pool = &deployment.db().pool;
    let task = Task::find_by_id(&deployment.db().pool, payload.task_id)
        .await?
        .ok_or(SqlxError::RowNotFound)?;

    if let Some(expected_status) = query.expected_task_status
        && task.status != expected_status
    {
        return Err(ApiError::BadRequest(format!(
            "Task status must be '{}' for this operation",
            expected_status
        )));
    }

    // Compute agent_working_dir based on repo count:
    // - Single repo: use repo name as working dir (agent runs in repo directory)
    // - Multiple repos: use None (agent runs in workspace root)
    let agent_working_dir = if payload.repos.len() == 1 {
        let repo = Repo::find_by_id(pool, payload.repos[0].repo_id)
            .await?
            .ok_or(RepoError::NotFound)?;
        Some(repo.name)
    } else {
        None
    };

    let attempt_id = Uuid::new_v4();
    let git_branch_name = deployment
        .container()
        .git_branch_from_workspace(&attempt_id, &task.title)
        .await;

    let workspace = Workspace::create(
        pool,
        &CreateWorkspace {
            branch: git_branch_name.clone(),
            agent_working_dir,
        },
        attempt_id,
        payload.task_id,
    )
    .await?;

    let workspace_repos: Vec<CreateWorkspaceRepo> = payload
        .repos
        .iter()
        .map(|r| CreateWorkspaceRepo {
            repo_id: r.repo_id,
            target_branch: r.target_branch.clone(),
        })
        .collect();

    WorkspaceRepo::create_many(pool, workspace.id, &workspace_repos).await?;
    if let Err(err) = deployment
        .container()
        .start_workspace(&workspace, executor_profile_id.clone())
        .await
    {
        tracing::error!("Failed to start task attempt: {}", err);
    }

    deployment
        .track_if_analytics_allowed(
            "task_attempt_started",
            serde_json::json!({
                "task_id": workspace.task_id.to_string(),
                "variant": &executor_profile_id.variant,
                "executor": &executor_profile_id.executor,
                "workspace_id": workspace.id.to_string(),
                "repository_count": payload.repos.len(),
            }),
        )
        .await;

    tracing::info!("Created attempt for task {}", task.id);

    Ok(ResponseJson(ApiResponse::success(workspace)))
}

#[axum::debug_handler]
pub async fn run_agent_setup(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<RunAgentSetupRequest>,
) -> Result<ResponseJson<ApiResponse<RunAgentSetupResponse>>, ApiError> {
    let executor_profile_id = payload.executor_profile_id;
    let config = ExecutorConfigs::get_cached();
    let coding_agent = config.get_coding_agent_or_default(&executor_profile_id);
    match coding_agent {
        CodingAgent::Codex(codex) => {
            codex_setup::run_codex_setup(&deployment, &workspace, &codex).await?;
        }
        _ => return Err(ApiError::Executor(ExecutorError::SetupHelperNotSupported)),
    }

    deployment
        .track_if_analytics_allowed(
            "agent_setup_script_executed",
            serde_json::json!({
                "executor_profile_id": executor_profile_id.to_string(),
                "workspace_id": workspace.id.to_string(),
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(RunAgentSetupResponse {})))
}

#[axum::debug_handler]
pub async fn start_review_subtask(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<StartReviewSubtaskRequest>,
) -> Result<ResponseJson<ApiResponse<TaskWithAttemptStatus>>, ApiError> {
    let pool = &deployment.db().pool;

    let parent_task = workspace
        .parent_task(pool)
        .await?
        .ok_or(WorkspaceError::TaskNotFound)?;

    let project = parent_task
        .parent_project(pool)
        .await?
        .ok_or(WorkspaceError::ProjectNotFound)?;

    let archive_context = context_archive::load_archive_context_for_workspace(pool, &workspace)
        .await
        .map_err(|err| {
            tracing::error!(
                "Failed to load archived context for workspace {}: {err:#}",
                workspace.id
            );
            ApiError::BadRequest("Failed to load archived context".to_string())
        })?
        .ok_or_else(|| ApiError::BadRequest("No archived context available yet".to_string()))?;

    let current_workspace_repos = WorkspaceRepo::find_by_workspace_id(pool, workspace.id).await?;
    if current_workspace_repos.is_empty() {
        return Err(ApiError::BadRequest(
            "At least one repository is required".to_string(),
        ));
    }

    let mut repos = Vec::with_capacity(current_workspace_repos.len());
    for workspace_repo in &current_workspace_repos {
        let repo = Repo::find_by_id(pool, workspace_repo.repo_id)
            .await?
            .ok_or(RepoError::NotFound)?;

        if !deployment
            .git()
            .check_branch_exists(&repo.path, &workspace.branch)?
        {
            return Err(ApiError::BadRequest(format!(
                "Branch '{}' does not exist in repository '{}'",
                workspace.branch, repo.name
            )));
        }

        repos.push(repo);
    }

    let review_task_title = build_review_subtask_title(&parent_task.title);
    let review_task_description =
        build_review_subtask_description(&parent_task, &workspace.branch, &archive_context);

    let task = Task::create(
        pool,
        &CreateTask {
            project_id: project.id,
            title: review_task_title,
            description: Some(review_task_description),
            status: Some(TaskStatus::Todo),
            parent_workspace_id: Some(workspace.id),
            source_cron_task_id: None,
            image_ids: None,
        },
        Uuid::new_v4(),
    )
    .await?;

    deployment
        .track_if_analytics_allowed(
            "task_created",
            serde_json::json!({
                "task_id": task.id.to_string(),
                "project_id": task.project_id,
                "has_description": task.description.is_some(),
                "has_images": false,
            }),
        )
        .await;

    let attempt_id = Uuid::new_v4();
    let git_branch_name = deployment
        .container()
        .git_branch_from_workspace(&attempt_id, &task.title)
        .await;

    let agent_working_dir = if repos.len() == 1 {
        Some(repos[0].name.clone())
    } else {
        None
    };

    let review_workspace = Workspace::create(
        pool,
        &CreateWorkspace {
            branch: git_branch_name,
            agent_working_dir,
        },
        attempt_id,
        task.id,
    )
    .await?;

    let workspace_repos: Vec<CreateWorkspaceRepo> = current_workspace_repos
        .into_iter()
        .map(|repo| CreateWorkspaceRepo {
            repo_id: repo.repo_id,
            target_branch: workspace.branch.clone(),
        })
        .collect();
    WorkspaceRepo::create_many(pool, review_workspace.id, &workspace_repos).await?;

    let is_attempt_running = deployment
        .container()
        .start_workspace(&review_workspace, payload.executor_profile_id.clone())
        .await
        .inspect_err(|err| tracing::error!("Failed to start review subtask attempt: {}", err))
        .is_ok();

    deployment
        .track_if_analytics_allowed(
            "task_attempt_started",
            serde_json::json!({
                "task_id": task.id.to_string(),
                "executor": &payload.executor_profile_id.executor,
                "variant": &payload.executor_profile_id.variant,
                "workspace_id": review_workspace.id.to_string(),
                "repository_count": repos.len(),
                "parent_workspace_id": workspace.id.to_string(),
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(TaskWithAttemptStatus {
        task,
        has_in_progress_attempt: is_attempt_running,
        last_attempt_failed: false,
        executor: payload.executor_profile_id.executor.to_string(),
    })))
}

#[axum::debug_handler]
pub async fn stream_task_attempt_diff_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<DiffStreamQuery>,
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> impl IntoResponse {
    let _ = Workspace::touch(&deployment.db().pool, workspace.id).await;

    let stats_only = params.stats_only;
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_task_attempt_diff_ws(socket, deployment, workspace, stats_only).await
        {
            tracing::warn!("diff WS closed: {}", e);
        }
    })
}

async fn handle_task_attempt_diff_ws(
    socket: WebSocket,
    deployment: DeploymentImpl,
    workspace: Workspace,
    stats_only: bool,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt, TryStreamExt};
    use utils::log_msg::LogMsg;

    let stream = deployment
        .container()
        .stream_diff(&workspace, stats_only)
        .await?;

    let mut stream = stream.map_ok(|msg: LogMsg| msg.to_ws_message_unchecked());

    let (mut sender, mut receiver) = socket.split();

    loop {
        tokio::select! {
            // Wait for next stream item
            item = stream.next() => {
                match item {
                    Some(Ok(msg)) => {
                        if sender.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        tracing::error!("stream error: {}", e);
                        break;
                    }
                    None => break,
                }
            }
            // Detect client disconnection
            msg = receiver.next() => {
                if msg.is_none() {
                    break;
                }
            }
        }
    }
    Ok(())
}

pub async fn stream_workspaces_ws(
    ws: WebSocketUpgrade,
    Query(query): Query<WorkspaceStreamQuery>,
    State(deployment): State<DeploymentImpl>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_workspaces_ws(socket, deployment, query.archived, query.limit).await
        {
            tracing::warn!("workspaces WS closed: {}", e);
        }
    })
}

async fn handle_workspaces_ws(
    socket: WebSocket,
    deployment: DeploymentImpl,
    archived: Option<bool>,
    limit: Option<i64>,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt, TryStreamExt};

    let mut stream = deployment
        .events()
        .stream_workspaces_raw(archived, limit)
        .await?
        .map_ok(|msg| msg.to_ws_message_unchecked());

    let (mut sender, mut receiver) = socket.split();

    loop {
        tokio::select! {
            item = stream.next() => {
                match item {
                    Some(Ok(msg)) => {
                        if sender.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        tracing::error!("stream error: {}", e);
                        break;
                    }
                    None => break,
                }
            }
            msg = receiver.next() => {
                if msg.is_none() {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize, TS)]
pub struct MergeTaskAttemptRequest {
    pub repo_id: Uuid,
}

#[derive(Debug, Deserialize, Serialize, TS)]
pub struct PushTaskAttemptRequest {
    pub repo_id: Uuid,
}

#[axum::debug_handler]
pub async fn merge_task_attempt(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<MergeTaskAttemptRequest>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    let pool = &deployment.db().pool;

    let workspace_repo =
        WorkspaceRepo::find_by_workspace_and_repo_id(pool, workspace.id, request.repo_id)
            .await?
            .ok_or(RepoError::NotFound)?;

    let repo = Repo::find_by_id(pool, workspace_repo.repo_id)
        .await?
        .ok_or(RepoError::NotFound)?;

    let container_ref = deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;
    let workspace_path = Path::new(&container_ref);
    let worktree_path = workspace_path.join(repo.name);

    let task = workspace
        .parent_task(pool)
        .await?
        .ok_or(ApiError::Workspace(WorkspaceError::TaskNotFound))?;
    let task_uuid_str = task.id.to_string();
    let first_uuid_section = task_uuid_str.split('-').next().unwrap_or(&task_uuid_str);

    let mut commit_message = format!("{} (vibe-kanban {})", task.title, first_uuid_section);

    // Add description on next line if it exists
    if let Some(description) = &task.description
        && !description.trim().is_empty()
    {
        commit_message.push_str("\n\n");
        commit_message.push_str(description);
    }

    // Compute diff stats BEFORE merging, since after merge the branch
    // will be identical to base and diff would show +0/-0
    let diff_stats = compute_diff_stats_before_merge(
        &deployment,
        &workspace,
        &repo.path,
        &worktree_path,
        &workspace_repo.target_branch,
    );

    let merge_commit_id = deployment.git().merge_changes(
        &repo.path,
        &worktree_path,
        &workspace.branch,
        &workspace_repo.target_branch,
        &commit_message,
    )?;

    Merge::create_direct(
        pool,
        workspace.id,
        workspace_repo.repo_id,
        &workspace_repo.target_branch,
        &merge_commit_id,
    )
    .await?;

    // Store diff stats on the task before updating status (which triggers notifications)
    if let Some((added, removed)) = diff_stats {
        let _ = Task::update_diff_stats(pool, task.id, added as i32, removed as i32).await;
    }
    let done_task = Task::update_status(pool, task.id, TaskStatus::Done).await?;
    if let Err(err) =
        context_archive::mark_task_done_diff(pool, &done_task, &workspace.branch).await
    {
        tracing::warn!(
            "Failed to sync Done diff archive meta for task {}: {}",
            done_task.id,
            err
        );
    }
    if !workspace.pinned {
        Workspace::set_archived(pool, workspace.id, true).await?;
    }

    // Stop any running dev servers for this workspace
    let dev_servers =
        ExecutionProcess::find_running_dev_servers_by_workspace(pool, workspace.id).await?;

    for dev_server in dev_servers {
        tracing::info!(
            "Stopping dev server {} for completed task attempt {}",
            dev_server.id,
            workspace.id
        );

        if let Err(e) = deployment
            .container()
            .stop_execution(&dev_server, ExecutionProcessStatus::Killed)
            .await
        {
            tracing::error!(
                "Failed to stop dev server {} for task attempt {}: {}",
                dev_server.id,
                workspace.id,
                e
            );
        }
    }

    deployment
        .track_if_analytics_allowed(
            "task_attempt_merged",
            serde_json::json!({
                "task_id": task.id.to_string(),
                "workspace_id": workspace.id.to_string(),
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(())))
}

pub async fn push_task_attempt_branch(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<PushTaskAttemptRequest>,
) -> Result<ResponseJson<ApiResponse<(), PushError>>, ApiError> {
    let pool = &deployment.db().pool;

    let workspace_repo =
        WorkspaceRepo::find_by_workspace_and_repo_id(pool, workspace.id, request.repo_id)
            .await?
            .ok_or(RepoError::NotFound)?;

    let repo = Repo::find_by_id(pool, workspace_repo.repo_id)
        .await?
        .ok_or(RepoError::NotFound)?;

    let container_ref = deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;
    let workspace_path = Path::new(&container_ref);
    let worktree_path = workspace_path.join(&repo.name);

    match deployment
        .git()
        .push_to_remote(&worktree_path, &workspace.branch, false)
    {
        Ok(_) => Ok(ResponseJson(ApiResponse::success(()))),
        Err(GitServiceError::GitCLI(GitCliError::PushRejected(_))) => Ok(ResponseJson(
            ApiResponse::error_with_data(PushError::ForcePushRequired),
        )),
        Err(e) => Err(ApiError::GitService(e)),
    }
}

pub async fn force_push_task_attempt_branch(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<PushTaskAttemptRequest>,
) -> Result<ResponseJson<ApiResponse<(), PushError>>, ApiError> {
    let pool = &deployment.db().pool;

    let workspace_repo =
        WorkspaceRepo::find_by_workspace_and_repo_id(pool, workspace.id, request.repo_id)
            .await?
            .ok_or(RepoError::NotFound)?;

    let repo = Repo::find_by_id(pool, workspace_repo.repo_id)
        .await?
        .ok_or(RepoError::NotFound)?;

    let container_ref = deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;
    let workspace_path = Path::new(&container_ref);
    let worktree_path = workspace_path.join(&repo.name);

    deployment
        .git()
        .push_to_remote(&worktree_path, &workspace.branch, true)?;
    Ok(ResponseJson(ApiResponse::success(())))
}

#[derive(Debug, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum PushError {
    ForcePushRequired,
}

#[derive(serde::Deserialize, TS)]
pub struct OpenEditorRequest {
    editor_type: Option<String>,
    file_path: Option<String>,
}

#[derive(Debug, Serialize, TS)]
pub struct OpenEditorResponse {
    pub url: Option<String>,
}

pub async fn open_task_attempt_in_editor(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<OpenEditorRequest>,
) -> Result<ResponseJson<ApiResponse<OpenEditorResponse>>, ApiError> {
    let container_ref = deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;

    Workspace::touch(&deployment.db().pool, workspace.id).await?;

    let workspace_path = Path::new(&container_ref);

    // For single-repo projects, open from the repo directory
    let workspace_repos =
        WorkspaceRepo::find_repos_for_workspace(&deployment.db().pool, workspace.id).await?;
    let workspace_path = if workspace_repos.len() == 1 && payload.file_path.is_none() {
        workspace_path.join(&workspace_repos[0].name)
    } else {
        workspace_path.to_path_buf()
    };

    // If a specific file path is provided, use it; otherwise use the base path
    let path = if let Some(file_path) = payload.file_path.as_ref() {
        workspace_path.join(file_path)
    } else {
        workspace_path
    };

    let editor_config = {
        let config = deployment.config().read().await;
        let editor_type_str = payload.editor_type.as_deref();
        config.editor.with_override(editor_type_str)
    };

    match editor_config.open_file(path.as_path()).await {
        Ok(url) => {
            tracing::info!(
                "Opened editor for task attempt {} at path: {}{}",
                workspace.id,
                path.display(),
                if url.is_some() { " (remote mode)" } else { "" }
            );

            deployment
                .track_if_analytics_allowed(
                    "task_attempt_editor_opened",
                    serde_json::json!({
                        "workspace_id": workspace.id.to_string(),
                        "editor_type": payload.editor_type.as_ref(),
                        "remote_mode": url.is_some(),
                    }),
                )
                .await;

            Ok(ResponseJson(ApiResponse::success(OpenEditorResponse {
                url,
            })))
        }
        Err(e) => {
            tracing::error!(
                "Failed to open editor for attempt {}: {:?}",
                workspace.id,
                e
            );
            Err(ApiError::EditorOpen(e))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct BranchStatus {
    pub commits_behind: Option<usize>,
    pub commits_ahead: Option<usize>,
    pub has_uncommitted_changes: Option<bool>,
    pub head_oid: Option<String>,
    pub uncommitted_count: Option<usize>,
    pub untracked_count: Option<usize>,
    pub target_branch_name: String,
    pub remote_commits_behind: Option<usize>,
    pub remote_commits_ahead: Option<usize>,
    pub merges: Vec<Merge>,
    /// True if a `git rebase` is currently in progress in this worktree
    pub is_rebase_in_progress: bool,
    /// Current conflict operation if any
    pub conflict_op: Option<ConflictOp>,
    /// List of files currently in conflicted (unmerged) state
    pub conflicted_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
pub struct RepoBranchStatus {
    pub repo_id: Uuid,
    pub repo_name: String,
    #[serde(flatten)]
    pub status: BranchStatus,
}

pub async fn get_task_attempt_branch_status(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<Vec<RepoBranchStatus>>>, ApiError> {
    let pool = &deployment.db().pool;

    let repositories = WorkspaceRepo::find_repos_for_workspace(pool, workspace.id).await?;
    let workspace_repos = WorkspaceRepo::find_by_workspace_id(pool, workspace.id).await?;
    let target_branches: HashMap<_, _> = workspace_repos
        .iter()
        .map(|wr| (wr.repo_id, wr.target_branch.clone()))
        .collect();

    let container_ref = deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;
    let workspace_dir = PathBuf::from(&container_ref);

    // Batch fetch all merges for the workspace to avoid N+1 queries
    let all_merges = Merge::find_by_workspace_id(pool, workspace.id).await?;
    let merges_by_repo: HashMap<Uuid, Vec<Merge>> =
        all_merges
            .into_iter()
            .fold(HashMap::new(), |mut acc, merge| {
                let repo_id = match &merge {
                    Merge::Direct(dm) => dm.repo_id,
                    Merge::Pr(pm) => pm.repo_id,
                };
                acc.entry(repo_id).or_insert_with(Vec::new).push(merge);
                acc
            });

    let mut results = Vec::with_capacity(repositories.len());

    for repo in repositories {
        let Some(target_branch) = target_branches.get(&repo.id).cloned() else {
            continue;
        };

        let repo_merges = merges_by_repo.get(&repo.id).cloned().unwrap_or_default();

        let worktree_path = workspace_dir.join(&repo.name);

        let head_oid = deployment
            .git()
            .get_head_info(&worktree_path)
            .ok()
            .map(|h| h.oid);

        let (is_rebase_in_progress, conflicted_files, conflict_op) = {
            let in_rebase = deployment
                .git()
                .is_rebase_in_progress(&worktree_path)
                .unwrap_or(false);
            let conflicts = deployment
                .git()
                .get_conflicted_files(&worktree_path)
                .unwrap_or_default();
            let op = if conflicts.is_empty() {
                None
            } else {
                deployment
                    .git()
                    .detect_conflict_op(&worktree_path)
                    .unwrap_or(None)
            };
            (in_rebase, conflicts, op)
        };

        let (uncommitted_count, untracked_count) =
            match deployment.git().get_worktree_change_counts(&worktree_path) {
                Ok((a, b)) => (Some(a), Some(b)),
                Err(_) => (None, None),
            };

        let has_uncommitted_changes = uncommitted_count.map(|c| c > 0);

        let target_branch_type = deployment
            .git()
            .find_branch_type(&repo.path, &target_branch)?;

        let (commits_ahead, commits_behind) = match target_branch_type {
            BranchType::Local => {
                let (a, b) = deployment.git().get_branch_status(
                    &repo.path,
                    &workspace.branch,
                    &target_branch,
                )?;
                (Some(a), Some(b))
            }
            BranchType::Remote => {
                let (ahead, behind) = deployment.git().get_remote_branch_status(
                    &repo.path,
                    &workspace.branch,
                    Some(&target_branch),
                )?;
                (Some(ahead), Some(behind))
            }
        };

        let (remote_ahead, remote_behind) = if let Some(Merge::Pr(PrMerge {
            pr_info:
                PullRequestInfo {
                    status: MergeStatus::Open,
                    ..
                },
            ..
        })) = repo_merges.first()
        {
            match deployment
                .git()
                .get_remote_branch_status(&repo.path, &workspace.branch, None)
            {
                Ok((ahead, behind)) => (Some(ahead), Some(behind)),
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        };

        results.push(RepoBranchStatus {
            repo_id: repo.id,
            repo_name: repo.name,
            status: BranchStatus {
                commits_ahead,
                commits_behind,
                has_uncommitted_changes,
                head_oid,
                uncommitted_count,
                untracked_count,
                remote_commits_ahead: remote_ahead,
                remote_commits_behind: remote_behind,
                merges: repo_merges,
                target_branch_name: target_branch,
                is_rebase_in_progress,
                conflict_op,
                conflicted_files,
            },
        });
    }

    Ok(ResponseJson(ApiResponse::success(results)))
}

#[derive(serde::Deserialize, Debug, TS)]
pub struct ChangeTargetBranchRequest {
    pub repo_id: Uuid,
    pub new_target_branch: String,
}

#[derive(serde::Serialize, Debug, TS)]
pub struct ChangeTargetBranchResponse {
    pub repo_id: Uuid,
    pub new_target_branch: String,
    pub status: (usize, usize),
}

#[derive(serde::Deserialize, Debug, TS)]
pub struct RenameBranchRequest {
    pub new_branch_name: String,
}

#[derive(serde::Serialize, Debug, TS)]
pub struct RenameBranchResponse {
    pub branch: String,
}

#[derive(Debug, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum RenameBranchError {
    EmptyBranchName,
    InvalidBranchNameFormat,
    OpenPullRequest,
    BranchAlreadyExists { repo_name: String },
    RebaseInProgress { repo_name: String },
    RenameFailed { repo_name: String, message: String },
}

#[axum::debug_handler]
pub async fn change_target_branch(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<ChangeTargetBranchRequest>,
) -> Result<ResponseJson<ApiResponse<ChangeTargetBranchResponse>>, ApiError> {
    let repo_id = payload.repo_id;
    let new_target_branch = payload.new_target_branch;
    let pool = &deployment.db().pool;

    let repo = Repo::find_by_id(pool, repo_id)
        .await?
        .ok_or(RepoError::NotFound)?;

    if !deployment
        .git()
        .check_branch_exists(&repo.path, &new_target_branch)?
    {
        return Ok(ResponseJson(ApiResponse::error(
            format!(
                "Branch '{}' does not exist in repository '{}'",
                new_target_branch, repo.name
            )
            .as_str(),
        )));
    };

    WorkspaceRepo::update_target_branch(pool, workspace.id, repo_id, &new_target_branch).await?;

    let status =
        deployment
            .git()
            .get_branch_status(&repo.path, &workspace.branch, &new_target_branch)?;

    deployment
        .track_if_analytics_allowed(
            "task_attempt_target_branch_changed",
            serde_json::json!({
                "repo_id": repo_id.to_string(),
                "workspace_id": workspace.id.to_string(),
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(
        ChangeTargetBranchResponse {
            repo_id,
            new_target_branch,
            status,
        },
    )))
}

#[axum::debug_handler]
pub async fn rename_branch(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<RenameBranchRequest>,
) -> Result<ResponseJson<ApiResponse<RenameBranchResponse, RenameBranchError>>, ApiError> {
    let new_branch_name = payload.new_branch_name.trim();

    if new_branch_name.is_empty() {
        return Ok(ResponseJson(ApiResponse::error_with_data(
            RenameBranchError::EmptyBranchName,
        )));
    }
    if !deployment.git().is_branch_name_valid(new_branch_name) {
        return Ok(ResponseJson(ApiResponse::error_with_data(
            RenameBranchError::InvalidBranchNameFormat,
        )));
    }
    if new_branch_name == workspace.branch {
        return Ok(ResponseJson(ApiResponse::success(RenameBranchResponse {
            branch: workspace.branch.clone(),
        })));
    }

    let pool = &deployment.db().pool;

    // Fail if workspace has an open PR in any repo
    let merges = Merge::find_by_workspace_id(pool, workspace.id).await?;
    let has_open_pr = merges.into_iter().any(|merge| {
        matches!(merge, Merge::Pr(pr_merge) if matches!(pr_merge.pr_info.status, MergeStatus::Open))
    });
    if has_open_pr {
        return Ok(ResponseJson(ApiResponse::error_with_data(
            RenameBranchError::OpenPullRequest,
        )));
    }

    let repos = WorkspaceRepo::find_repos_for_workspace(pool, workspace.id).await?;
    let container_ref = deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;
    let workspace_dir = PathBuf::from(&container_ref);

    for repo in &repos {
        let worktree_path = workspace_dir.join(&repo.name);

        if deployment
            .git()
            .check_branch_exists(&repo.path, new_branch_name)?
        {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                RenameBranchError::BranchAlreadyExists {
                    repo_name: repo.name.clone(),
                },
            )));
        }

        if deployment.git().is_rebase_in_progress(&worktree_path)? {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                RenameBranchError::RebaseInProgress {
                    repo_name: repo.name.clone(),
                },
            )));
        }
    }

    // Rename all repos with rollback
    let old_branch = workspace.branch.clone();
    let mut renamed_repos: Vec<&Repo> = Vec::new();

    for repo in &repos {
        let worktree_path = workspace_dir.join(&repo.name);

        match deployment.git().rename_local_branch(
            &worktree_path,
            &workspace.branch,
            new_branch_name,
        ) {
            Ok(()) => {
                renamed_repos.push(repo);
            }
            Err(e) => {
                // Rollback already renamed repos
                for renamed_repo in &renamed_repos {
                    let rollback_path = workspace_dir.join(&renamed_repo.name);
                    if let Err(rollback_err) = deployment.git().rename_local_branch(
                        &rollback_path,
                        new_branch_name,
                        &old_branch,
                    ) {
                        tracing::error!(
                            "Failed to rollback branch rename in '{}': {}",
                            renamed_repo.name,
                            rollback_err
                        );
                    }
                }
                return Ok(ResponseJson(ApiResponse::error_with_data(
                    RenameBranchError::RenameFailed {
                        repo_name: repo.name.clone(),
                        message: e.to_string(),
                    },
                )));
            }
        }
    }

    Workspace::update_branch_name(pool, workspace.id, new_branch_name).await?;
    // What will become of me?
    let updated_children_count = WorkspaceRepo::update_target_branch_for_children_of_workspace(
        pool,
        workspace.id,
        &old_branch,
        new_branch_name,
    )
    .await?;

    if updated_children_count > 0 {
        tracing::info!(
            "Updated {} child task attempts to target new branch '{}'",
            updated_children_count,
            new_branch_name
        );
    }

    deployment
        .track_if_analytics_allowed(
            "task_attempt_branch_renamed",
            serde_json::json!({
                "updated_children": updated_children_count,
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(RenameBranchResponse {
        branch: new_branch_name.to_string(),
    })))
}

#[axum::debug_handler]
pub async fn rebase_task_attempt(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<RebaseTaskAttemptRequest>,
) -> Result<ResponseJson<ApiResponse<(), GitOperationError>>, ApiError> {
    let pool = &deployment.db().pool;

    let workspace_repo =
        WorkspaceRepo::find_by_workspace_and_repo_id(pool, workspace.id, payload.repo_id)
            .await?
            .ok_or(RepoError::NotFound)?;

    let repo = Repo::find_by_id(pool, workspace_repo.repo_id)
        .await?
        .ok_or(RepoError::NotFound)?;

    let old_base_branch = payload
        .old_base_branch
        .unwrap_or_else(|| workspace_repo.target_branch.clone());
    let new_base_branch = payload
        .new_base_branch
        .unwrap_or_else(|| workspace_repo.target_branch.clone());

    match deployment
        .git()
        .check_branch_exists(&repo.path, &new_base_branch)?
    {
        true => {
            WorkspaceRepo::update_target_branch(
                pool,
                workspace.id,
                payload.repo_id,
                &new_base_branch,
            )
            .await?;
        }
        false => {
            return Ok(ResponseJson(ApiResponse::error(
                format!(
                    "Branch '{}' does not exist in the repository",
                    new_base_branch
                )
                .as_str(),
            )));
        }
    }

    let container_ref = deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;
    let workspace_path = Path::new(&container_ref);
    let worktree_path = workspace_path.join(&repo.name);

    let result = deployment.git().rebase_branch(
        &repo.path,
        &worktree_path,
        &new_base_branch,
        &old_base_branch,
        &workspace.branch.clone(),
    );
    if let Err(e) = result {
        use services::services::git::GitServiceError;
        return match e {
            GitServiceError::MergeConflicts(msg) => Ok(ResponseJson(ApiResponse::<
                (),
                GitOperationError,
            >::error_with_data(
                GitOperationError::MergeConflicts {
                    message: msg,
                    op: ConflictOp::Rebase,
                },
            ))),
            GitServiceError::RebaseInProgress => Ok(ResponseJson(ApiResponse::<
                (),
                GitOperationError,
            >::error_with_data(
                GitOperationError::RebaseInProgress,
            ))),
            other => Err(ApiError::GitService(other)),
        };
    }

    deployment
        .track_if_analytics_allowed(
            "task_attempt_rebased",
            serde_json::json!({
                "workspace_id": workspace.id.to_string(),
                "repo_id": payload.repo_id.to_string(),
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(())))
}

#[axum::debug_handler]
pub async fn abort_conflicts_task_attempt(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<AbortConflictsRequest>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    let pool = &deployment.db().pool;

    let repo = Repo::find_by_id(pool, payload.repo_id)
        .await?
        .ok_or(RepoError::NotFound)?;

    let container_ref = deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;
    let workspace_path = Path::new(&container_ref);
    let worktree_path = workspace_path.join(&repo.name);

    deployment.git().abort_conflicts(&worktree_path)?;

    Ok(ResponseJson(ApiResponse::success(())))
}

#[axum::debug_handler]
pub async fn start_dev_server(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    let pool = &deployment.db().pool;

    // Get parent task
    let task = workspace
        .parent_task(&deployment.db().pool)
        .await?
        .ok_or(SqlxError::RowNotFound)?;

    // Get parent project
    let project = task
        .parent_project(&deployment.db().pool)
        .await?
        .ok_or(SqlxError::RowNotFound)?;

    // Stop any existing dev servers for this project
    let existing_dev_servers =
        match ExecutionProcess::find_running_dev_servers_by_project(pool, project.id).await {
            Ok(servers) => servers,
            Err(e) => {
                tracing::error!(
                    "Failed to find running dev servers for project {}: {}",
                    project.id,
                    e
                );
                return Err(ApiError::Workspace(WorkspaceError::ValidationError(
                    e.to_string(),
                )));
            }
        };

    for dev_server in existing_dev_servers {
        tracing::info!(
            "Stopping existing dev server {} for project {}",
            dev_server.id,
            project.id
        );

        if let Err(e) = deployment
            .container()
            .stop_execution(&dev_server, ExecutionProcessStatus::Killed)
            .await
        {
            tracing::error!("Failed to stop dev server {}: {}", dev_server.id, e);
        }
    }

    let repos = WorkspaceRepo::find_repos_for_workspace(pool, workspace.id).await?;
    let repos_with_dev_script: Vec<_> = repos
        .iter()
        .filter(|r| r.dev_server_script.as_ref().is_some_and(|s| !s.is_empty()))
        .collect();

    if repos_with_dev_script.is_empty() {
        return Ok(ResponseJson(ApiResponse::error(
            "No dev server script configured for any repository in this workspace",
        )));
    }

    let session = match Session::find_latest_by_workspace_id(pool, workspace.id).await? {
        Some(s) => s,
        None => {
            Session::create(
                pool,
                &CreateSession {
                    executor: Some("dev-server".to_string()),
                },
                Uuid::new_v4(),
                workspace.id,
            )
            .await?
        }
    };

    for repo in repos_with_dev_script {
        let executor_action = ExecutorAction::new(
            ExecutorActionType::ScriptRequest(ScriptRequest {
                script: repo.dev_server_script.clone().unwrap(),
                language: ScriptRequestLanguage::Bash,
                context: ScriptContext::DevServer,
                working_dir: Some(repo.name.clone()),
            }),
            None,
        );

        deployment
            .container()
            .start_execution(
                &workspace,
                &session,
                &executor_action,
                &ExecutionProcessRunReason::DevServer,
            )
            .await?;
    }

    deployment
        .track_if_analytics_allowed(
            "dev_server_started",
            serde_json::json!({
                "task_id": task.id.to_string(),
                "project_id": project.id.to_string(),
                "workspace_id": workspace.id.to_string(),
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(())))
}

pub async fn get_task_attempt_children(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<TaskRelationships>>, StatusCode> {
    match Task::find_relationships_for_workspace(&deployment.db().pool, &workspace).await {
        Ok(relationships) => {
            deployment
                .track_if_analytics_allowed(
                    "task_attempt_children_viewed",
                    serde_json::json!({
                        "workspace_id": workspace.id.to_string(),
                        "children_count": relationships.children.len(),
                        "parent_count": if relationships.parent_task.is_some() { 1 } else { 0 },
                    }),
                )
                .await;

            Ok(ResponseJson(ApiResponse::success(relationships)))
        }
        Err(e) => {
            tracing::error!(
                "Failed to fetch relationships for task attempt {}: {}",
                workspace.id,
                e
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn stop_task_attempt_execution(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    deployment.container().try_stop(&workspace, false).await;

    deployment
        .track_if_analytics_allowed(
            "task_attempt_stopped",
            serde_json::json!({
                "workspace_id": workspace.id.to_string(),
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(())))
}

#[derive(Debug, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum RunScriptError {
    NoScriptConfigured,
    ProcessAlreadyRunning,
}

#[axum::debug_handler]
pub async fn run_setup_script(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<ExecutionProcess, RunScriptError>>, ApiError> {
    let pool = &deployment.db().pool;

    // Check if any non-dev-server processes are already running for this workspace
    if ExecutionProcess::has_running_non_dev_server_processes_for_workspace(pool, workspace.id)
        .await?
    {
        return Ok(ResponseJson(ApiResponse::error_with_data(
            RunScriptError::ProcessAlreadyRunning,
        )));
    }

    deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;

    let task = workspace
        .parent_task(pool)
        .await?
        .ok_or(SqlxError::RowNotFound)?;

    let project = task
        .parent_project(pool)
        .await?
        .ok_or(SqlxError::RowNotFound)?;

    let repos = WorkspaceRepo::find_repos_for_workspace(pool, workspace.id).await?;
    let executor_action = match deployment.container().setup_actions_for_repos(&repos) {
        Some(action) => action,
        None => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                RunScriptError::NoScriptConfigured,
            )));
        }
    };

    // Get or create a session for setup script
    let session = match Session::find_latest_by_workspace_id(pool, workspace.id).await? {
        Some(s) => s,
        None => {
            Session::create(
                pool,
                &CreateSession {
                    executor: Some("setup-script".to_string()),
                },
                Uuid::new_v4(),
                workspace.id,
            )
            .await?
        }
    };

    let execution_process = deployment
        .container()
        .start_execution(
            &workspace,
            &session,
            &executor_action,
            &ExecutionProcessRunReason::SetupScript,
        )
        .await?;

    deployment
        .track_if_analytics_allowed(
            "setup_script_executed",
            serde_json::json!({
                "task_id": task.id.to_string(),
                "project_id": project.id.to_string(),
                "workspace_id": workspace.id.to_string(),
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(execution_process)))
}

#[axum::debug_handler]
pub async fn run_cleanup_script(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<ExecutionProcess, RunScriptError>>, ApiError> {
    let pool = &deployment.db().pool;

    // Check if any non-dev-server processes are already running for this workspace
    if ExecutionProcess::has_running_non_dev_server_processes_for_workspace(pool, workspace.id)
        .await?
    {
        return Ok(ResponseJson(ApiResponse::error_with_data(
            RunScriptError::ProcessAlreadyRunning,
        )));
    }

    deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;

    let task = workspace
        .parent_task(pool)
        .await?
        .ok_or(SqlxError::RowNotFound)?;

    let project = task
        .parent_project(pool)
        .await?
        .ok_or(SqlxError::RowNotFound)?;

    let repos = WorkspaceRepo::find_repos_for_workspace(pool, workspace.id).await?;
    let executor_action = match deployment.container().cleanup_actions_for_repos(&repos) {
        Some(action) => action,
        None => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                RunScriptError::NoScriptConfigured,
            )));
        }
    };

    // Get or create a session for cleanup script
    let session = match Session::find_latest_by_workspace_id(pool, workspace.id).await? {
        Some(s) => s,
        None => {
            Session::create(
                pool,
                &CreateSession {
                    executor: Some("cleanup-script".to_string()),
                },
                Uuid::new_v4(),
                workspace.id,
            )
            .await?
        }
    };

    let execution_process = deployment
        .container()
        .start_execution(
            &workspace,
            &session,
            &executor_action,
            &ExecutionProcessRunReason::CleanupScript,
        )
        .await?;

    deployment
        .track_if_analytics_allowed(
            "cleanup_script_executed",
            serde_json::json!({
                "task_id": task.id.to_string(),
                "project_id": project.id.to_string(),
                "workspace_id": workspace.id.to_string(),
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(execution_process)))
}

#[axum::debug_handler]
pub async fn gh_cli_setup_handler(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<ExecutionProcess, GhCliSetupError>>, ApiError> {
    match gh_cli_setup::run_gh_cli_setup(&deployment, &workspace).await {
        Ok(execution_process) => {
            deployment
                .track_if_analytics_allowed(
                    "gh_cli_setup_executed",
                    serde_json::json!({
                        "workspace_id": workspace.id.to_string(),
                    }),
                )
                .await;

            Ok(ResponseJson(ApiResponse::success(execution_process)))
        }
        Err(ApiError::Executor(ExecutorError::ExecutableNotFound { program }))
            if program == "brew" =>
        {
            Ok(ResponseJson(ApiResponse::error_with_data(
                GhCliSetupError::BrewMissing,
            )))
        }
        Err(ApiError::Executor(ExecutorError::SetupHelperNotSupported)) => Ok(ResponseJson(
            ApiResponse::error_with_data(GhCliSetupError::SetupHelperNotSupported),
        )),
        Err(ApiError::Executor(err)) => Ok(ResponseJson(ApiResponse::error_with_data(
            GhCliSetupError::Other {
                message: err.to_string(),
            },
        ))),
        Err(err) => Err(err),
    }
}

pub async fn get_task_attempt_repos(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<Vec<RepoWithTargetBranch>>>, ApiError> {
    let pool = &deployment.db().pool;

    let repos =
        WorkspaceRepo::find_repos_with_target_branch_for_workspace(pool, workspace.id).await?;

    Ok(ResponseJson(ApiResponse::success(repos)))
}

pub async fn search_workspace_files(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Query(search_query): Query<SearchQuery>,
) -> Result<ResponseJson<ApiResponse<Vec<SearchResult>>>, StatusCode> {
    if search_query.q.trim().is_empty() {
        return Ok(ResponseJson(ApiResponse::error(
            "Query parameter 'q' is required and cannot be empty",
        )));
    }

    let repos =
        match WorkspaceRepo::find_repos_for_workspace(&deployment.db().pool, workspace.id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to get workspace repos: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        };

    match deployment
        .project()
        .search_files(
            deployment.file_search_cache().as_ref(),
            &repos,
            &search_query,
        )
        .await
    {
        Ok(results) => Ok(ResponseJson(ApiResponse::success(results))),
        Err(e) => {
            tracing::error!("Failed to search files: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn get_first_user_message(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<Option<String>>>, ApiError> {
    let pool = &deployment.db().pool;

    let message = Workspace::get_first_user_message(pool, workspace.id).await?;

    Ok(ResponseJson(ApiResponse::success(message)))
}

fn parse_markdown_pdf_with_fallback(markdown: String) -> Result<(Vec<u8>, bool), MdpError> {
    match markdown2pdf::parse_into_bytes(markdown.clone(), ConfigSource::Default, None) {
        Ok(bytes) => Ok((bytes, false)),
        Err(MdpError::ParseError { .. }) => {
            let fallback = wrap_markdown_as_text_code_block(&markdown);
            let bytes = markdown2pdf::parse_into_bytes(fallback, ConfigSource::Default, None)?;
            Ok((bytes, true))
        }
        Err(err) => Err(err),
    }
}

fn strip_leading_execution_marker_line(markdown: String) -> (String, bool) {
    let Some((first_line, rest)) = markdown.split_once('\n') else {
        return if is_execution_marker_line(&markdown) {
            (String::new(), true)
        } else {
            (markdown, false)
        };
    };

    if is_execution_marker_line(first_line) {
        (rest.to_string(), true)
    } else {
        (markdown, false)
    }
}

fn is_execution_marker_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("<!-- execution_id:") && trimmed.ends_with("-->")
}

fn wrap_markdown_as_text_code_block(markdown: &str) -> String {
    let fence_len = std::cmp::max(3, longest_backtick_run(markdown) + 1);
    let fence = "`".repeat(fence_len);
    format!("{fence}text\n{markdown}\n{fence}")
}

fn longest_backtick_run(markdown: &str) -> usize {
    let mut max_run = 0;
    let mut current_run = 0;

    for ch in markdown.chars() {
        if ch == '`' {
            current_run += 1;
            max_run = max_run.max(current_run);
        } else {
            current_run = 0;
        }
    }

    max_run
}

fn build_review_subtask_title(parent_task_title: &str) -> String {
    format!("Code review: {parent_task_title}")
}

fn build_review_subtask_description(
    parent_task: &Task,
    reviewed_branch: &str,
    archive_context: &str,
) -> String {
    let archived_context_block = wrap_markdown_as_text_code_block(archive_context);
    format!(
        "Review the implementation for \"{}\".\n\nFocus on correctness, regressions, edge cases, and missing tests. Group findings by severity and include concrete file-level references.\n\nBranch under review: `{}`\n\nArchived context from the parent attempt:\n{}",
        parent_task.title, reviewed_branch, archived_context_block
    )
}

pub async fn export_context(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<ExportContextQuery>,
) -> Result<Response, ApiError> {
    let pool = &deployment.db().pool;
    let archive_context = context_archive::load_archive_context_for_workspace(pool, &workspace)
        .await
        .map_err(|err| {
            tracing::error!(
                "Failed to load archived context for workspace {}: {err:#}",
                workspace.id
            );
            ApiError::BadRequest("Failed to load archived context".to_string())
        })?
        .ok_or_else(|| ApiError::BadRequest("No archived context available yet".to_string()))?;
    let (export_context, stripped_marker_line) =
        strip_leading_execution_marker_line(archive_context);

    let (content_type, bytes) = match query.format {
        ExportContextFormat::Md => ("text/markdown; charset=utf-8", export_context.into_bytes()),
        ExportContextFormat::Pdf => {
            let workspace_id = workspace.id;
            let (pdf_bytes, used_fallback) = tokio::task::spawn_blocking(move || {
                parse_markdown_pdf_with_fallback(export_context)
            })
            .await
            .map_err(|err| {
                tracing::error!(
                    "Failed to join PDF export task for workspace {}: {}",
                    workspace_id,
                    err
                );
                ApiError::BadRequest("Failed to convert archived context to PDF".to_string())
            })?
            .map_err(|err| {
                tracing::error!(
                    "Failed to convert archived context to PDF for workspace {}: {}",
                    workspace_id,
                    err
                );
                ApiError::BadRequest("Failed to convert archived context to PDF".to_string())
            })?;

            if used_fallback {
                tracing::warn!(
                    "Archived context markdown was invalid for workspace {}; exported PDF with plain-text fallback",
                    workspace_id
                );
            }
            ("application/pdf", pdf_bytes)
        }
    };

    if stripped_marker_line {
        tracing::info!(
            "Stripped archive execution marker comment from exported context for workspace {}",
            workspace.id
        );
    }

    Ok(([(header::CONTENT_TYPE, content_type)], bytes).into_response())
}

pub async fn delete_workspace(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<(StatusCode, ResponseJson<ApiResponse<()>>), ApiError> {
    let pool = &deployment.db().pool;

    // Check for running execution processes
    if ExecutionProcess::has_running_non_dev_server_processes_for_workspace(pool, workspace.id)
        .await?
    {
        return Err(ApiError::Conflict(
            "Cannot delete workspace while processes are running. Stop all processes first."
                .to_string(),
        ));
    }

    // Stop any running dev servers for this workspace
    let dev_servers =
        ExecutionProcess::find_running_dev_servers_by_workspace(pool, workspace.id).await?;

    for dev_server in dev_servers {
        tracing::info!(
            "Stopping dev server {} before deleting workspace {}",
            dev_server.id,
            workspace.id
        );

        if let Err(e) = deployment
            .container()
            .stop_execution(&dev_server, ExecutionProcessStatus::Killed)
            .await
        {
            tracing::error!(
                "Failed to stop dev server {} for workspace {}: {}",
                dev_server.id,
                workspace.id,
                e
            );
        }
    }

    // Gather data needed for background cleanup
    let workspace_dir = workspace.container_ref.clone().map(PathBuf::from);
    let repositories = WorkspaceRepo::find_repos_for_workspace(pool, workspace.id).await?;

    if let Ok(Some(task)) = workspace.parent_task(pool).await
        && let Ok(Some(project)) = task.parent_project(pool).await
        && let Err(err) =
            context_archive::ensure_archive_files(pool, &project, &task, &workspace.branch).await
    {
        tracing::warn!(
            "Failed to ensure archive files while deleting workspace {}: {}",
            workspace.id,
            err
        );
    }

    // Nullify parent_workspace_id for any child tasks before deletion
    let children_affected = Task::nullify_children_by_workspace_id(pool, workspace.id).await?;
    if children_affected > 0 {
        tracing::info!(
            "Nullified {} child task references before deleting workspace {}",
            children_affected,
            workspace.id
        );
    }

    // Delete workspace from database (FK CASCADE will handle sessions, execution_processes, etc.)
    let rows_affected = Workspace::delete(pool, workspace.id).await?;

    if rows_affected == 0 {
        return Err(ApiError::Database(SqlxError::RowNotFound));
    }

    deployment
        .track_if_analytics_allowed(
            "workspace_deleted",
            serde_json::json!({
                "workspace_id": workspace.id.to_string(),
                "task_id": workspace.task_id.to_string(),
            }),
        )
        .await;

    // Spawn background cleanup task for filesystem resources
    if let Some(workspace_dir) = workspace_dir {
        let workspace_id = workspace.id;
        tokio::spawn(async move {
            tracing::info!(
                "Starting background cleanup for workspace {} at {}",
                workspace_id,
                workspace_dir.display()
            );

            if let Err(e) = WorkspaceManager::cleanup_workspace(&workspace_dir, &repositories).await
            {
                tracing::error!(
                    "Background workspace cleanup failed for {} at {}: {}",
                    workspace_id,
                    workspace_dir.display(),
                    e
                );
            } else {
                tracing::info!(
                    "Background cleanup completed for workspace {}",
                    workspace_id
                );
            }
        });
    }

    // Return 202 Accepted to indicate deletion was scheduled
    Ok((StatusCode::ACCEPTED, ResponseJson(ApiResponse::success(()))))
}

/// Mark all coding agent turns for a workspace as seen
#[axum::debug_handler]
pub async fn mark_seen(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    let pool = &deployment.db().pool;

    CodingAgentTurn::mark_seen_by_workspace_id(pool, workspace.id).await?;

    Ok(ResponseJson(ApiResponse::success(())))
}

/// Compute diff stats (additions, deletions) for a workspace before merging.
/// Returns None if stats cannot be computed (non-fatal).
fn compute_diff_stats_before_merge(
    deployment: &DeploymentImpl,
    workspace: &Workspace,
    repo_path: &Path,
    worktree_path: &Path,
    target_branch: &str,
) -> Option<(usize, usize)> {
    let base_commit = deployment
        .git()
        .get_base_commit(repo_path, &workspace.branch, target_branch)
        .ok()?;

    let diffs = deployment
        .git()
        .get_diffs(
            DiffTarget::Worktree {
                worktree_path,
                base_commit: &base_commit,
            },
            None,
        )
        .ok()?;

    let mut total_added = 0usize;
    let mut total_removed = 0usize;
    for diff in diffs {
        total_added += diff.additions.unwrap_or(0);
        total_removed += diff.deletions.unwrap_or(0);
    }
    Some((total_added, total_removed))
}

pub fn router(deployment: &DeploymentImpl) -> Router<DeploymentImpl> {
    let task_attempt_id_router = Router::new()
        .route(
            "/",
            get(get_task_attempt)
                .put(update_workspace)
                .delete(delete_workspace),
        )
        .route("/run-agent-setup", post(run_agent_setup))
        .route("/gh-cli-setup", post(gh_cli_setup_handler))
        .route("/start-dev-server", post(start_dev_server))
        .route("/run-setup-script", post(run_setup_script))
        .route("/run-cleanup-script", post(run_cleanup_script))
        .route("/branch-status", get(get_task_attempt_branch_status))
        .route("/diff/ws", get(stream_task_attempt_diff_ws))
        .route("/merge", post(merge_task_attempt))
        .route("/push", post(push_task_attempt_branch))
        .route("/push/force", post(force_push_task_attempt_branch))
        .route("/rebase", post(rebase_task_attempt))
        .route("/conflicts/abort", post(abort_conflicts_task_attempt))
        .route("/pr/attach", post(pr::attach_existing_pr))
        .route("/pr/comments", get(pr::get_pr_comments))
        .route("/open-editor", post(open_task_attempt_in_editor))
        .route("/children", get(get_task_attempt_children))
        .route("/stop", post(stop_task_attempt_execution))
        .route("/change-target-branch", post(change_target_branch))
        .route("/rename-branch", post(rename_branch))
        .route("/repos", get(get_task_attempt_repos))
        .route("/search", get(search_workspace_files))
        .route("/first-message", get(get_first_user_message))
        .route("/export-context", get(export_context))
        .route("/review-subtask/start", post(start_review_subtask))
        .route("/mark-seen", put(mark_seen))
        .route(
            "/review-command",
            get(get_review_command)
                .put(save_review_command)
                .delete(delete_review_command),
        )
        .route("/review-command/unsolved", get(get_unsolved_review_command))
        .route(
            "/review-command/status",
            patch(update_review_command_status),
        )
        .layer(from_fn_with_state(
            deployment.clone(),
            load_workspace_middleware,
        ));

    let task_attempts_router = Router::new()
        .route("/", get(get_task_attempts).post(create_task_attempt))
        .route("/count", get(get_workspace_count))
        .route("/stream/ws", get(stream_workspaces_ws))
        .route("/summary", post(workspace_summary::get_workspace_summaries))
        .nest("/{id}", task_attempt_id_router)
        .nest("/{id}/images", images::router(deployment));

    Router::new().nest("/task-attempts", task_attempts_router)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_leading_execution_marker_line_removes_marker() {
        let input = "<!-- execution_id:abc -->\n## Execution";
        let (stripped, did_strip) = strip_leading_execution_marker_line(input.to_string());
        assert!(did_strip);
        assert_eq!(stripped, "## Execution");
    }

    #[test]
    fn strip_leading_execution_marker_line_keeps_normal_content() {
        let input = "## Execution\n- content";
        let (stripped, did_strip) = strip_leading_execution_marker_line(input.to_string());
        assert!(!did_strip);
        assert_eq!(stripped, input);
    }

    #[test]
    fn wrap_markdown_as_text_code_block_uses_safe_fence() {
        let wrapped = wrap_markdown_as_text_code_block("alpha ```` beta");

        let mut lines = wrapped.lines();
        let opening_fence = lines.next().expect("missing opening fence");
        assert_eq!(opening_fence, "`````text");
        assert_eq!(lines.next(), Some("alpha ```` beta"));
        assert_eq!(lines.last(), Some("`````"));
    }

    #[test]
    fn parse_markdown_pdf_with_fallback_handles_invalid_markdown() {
        let markdown = "*unclosed emphasis".to_string();

        let direct = markdown2pdf::parse_into_bytes(markdown.clone(), ConfigSource::Default, None);
        assert!(matches!(direct, Err(MdpError::ParseError { .. })));

        let (bytes, used_fallback) = parse_markdown_pdf_with_fallback(markdown).unwrap();
        assert!(used_fallback);
        assert!(!bytes.is_empty());
    }
}
