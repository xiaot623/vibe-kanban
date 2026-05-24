use db::models::{
    execution_process::ExecutionProcess,
    session::Session,
    task::Task,
    telegram_flow_binding::{CreateTelegramFlowBinding, TelegramFlowBinding},
    workspace::Workspace,
};
use sqlx::SqlitePool;
use teloxide::types::{MessageId, ThreadId};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct TelegramFlowContext {
    pub flow_token: String,
    pub task_id: Uuid,
    pub workspace_id: Uuid,
    pub session_id: Uuid,
    pub latest_execution_process_id: Option<Uuid>,
    pub executor_label: String,
    pub variant_label: String,
    pub topic_thread_id: Option<ThreadId>,
}

pub async fn resolve_flow_context(
    pool: &SqlitePool,
    flow_token: &str,
) -> Result<Option<TelegramFlowContext>, sqlx::Error> {
    let Some(binding) = TelegramFlowBinding::resolve(pool, flow_token).await else {
        return Ok(None);
    };

    let session_exists = Session::find_by_id(pool, binding.session_id)
        .await?
        .is_some();
    if !session_exists {
        return Ok(None);
    }

    Ok(Some(binding_to_context(pool, binding).await))
}

pub async fn ensure_flow_context_for_session(
    pool: &SqlitePool,
    session_id: Uuid,
    latest_execution_process_id: Option<Uuid>,
) -> Result<TelegramFlowContext, sqlx::Error> {
    let session = Session::find_by_id(pool, session_id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    let workspace = Workspace::find_by_id(pool, session.workspace_id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    let task = Task::find_by_id(pool, workspace.task_id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;

    let executor_label = session
        .executor
        .as_deref()
        .map(normalize_executor_label)
        .unwrap_or_else(|| "UNKNOWN".to_string());

    let binding = TelegramFlowBinding::get_or_create(
        pool,
        &CreateTelegramFlowBinding {
            task_id: task.id,
            workspace_id: workspace.id,
            session_id: session.id,
            latest_execution_process_id,
            executor_label,
        },
    )
    .await?;

    Ok(binding_to_context(pool, binding).await)
}

pub async fn update_flow_latest_process(
    pool: &SqlitePool,
    session_id: Uuid,
    execution_process_id: Uuid,
) -> Result<Option<TelegramFlowContext>, sqlx::Error> {
    let Some(existing) = TelegramFlowBinding::find_by_session_id(pool, session_id).await? else {
        return Ok(None);
    };

    let updated = db::models::telegram_flow_binding::UpdateTelegramFlowBinding {
        workspace_id: existing.workspace_id,
        latest_execution_process_id: Some(execution_process_id),
        executor_label: existing.executor_label.clone(),
    };
    let binding =
        TelegramFlowBinding::update_existing(pool, &existing.flow_token, &updated).await?;
    Ok(Some(binding_to_context(pool, binding).await))
}

pub async fn resolve_flow_context_from_execution(
    pool: &SqlitePool,
    execution_process_id: Uuid,
) -> Result<Option<TelegramFlowContext>, sqlx::Error> {
    let process = ExecutionProcess::find_by_id(pool, execution_process_id).await?;
    let Some(process) = process else {
        return Ok(None);
    };
    let ctx = ensure_flow_context_for_session(pool, process.session_id, Some(process.id)).await?;
    Ok(Some(ctx))
}

async fn binding_to_context(
    pool: &SqlitePool,
    binding: TelegramFlowBinding,
) -> TelegramFlowContext {
    TelegramFlowContext {
        flow_token: binding.flow_token,
        task_id: binding.task_id,
        workspace_id: binding.workspace_id,
        session_id: binding.session_id,
        latest_execution_process_id: binding.latest_execution_process_id,
        executor_label: binding.executor_label,
        variant_label: resolve_variant_label(pool, binding.session_id).await,
        topic_thread_id: resolve_topic_thread_id(pool, binding.task_id).await,
    }
}

async fn resolve_variant_label(pool: &SqlitePool, session_id: Uuid) -> String {
    ExecutionProcess::latest_executor_profile_for_session(pool, session_id)
        .await
        .ok()
        .flatten()
        .and_then(|profile| profile.variant)
        .unwrap_or_else(|| "DEFAULT".to_string())
}

fn normalize_executor_label(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "UNKNOWN".to_string();
    }
    trimmed.to_ascii_uppercase()
}

async fn resolve_topic_thread_id(pool: &SqlitePool, task_id: Uuid) -> Option<ThreadId> {
    let binding =
        db::models::telegram_task_topic::TelegramTaskTopic::find_by_task_id(pool, task_id)
            .await
            .ok()
            .flatten()?;
    let message_thread_id = i32::try_from(binding.message_thread_id?).ok()?;
    Some(ThreadId(MessageId(message_thread_id)))
}
