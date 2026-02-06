//! Task state handlers for Telegram notifications.
//!
//! triggered when task status changes occur.

use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    vec,
};

use db::{
    DBService,
    models::{
        execution_process::ExecutionProcess, project::Project, session::Session,
        short_id_mapping::ShortIdMapping, task::TaskStatus, workspace::Workspace,
        workspace_repo::WorkspaceRepo,
    },
    task_state::{
        TaskStateTransition,
        dispatcher::TaskStateDispatcher,
        handler::{TransitionFilter, fn_handler},
    },
};
use teloxide::prelude::*;
use tokio::sync::{OnceCell, RwLock};

use super::EXIT_PLAN_MODE_NAME;
use crate::services::{approvals::Approvals, git::GitService};

/// Telegram context for event handlers.
#[derive(Clone)]
pub struct TelegramContext {
    pub db: DBService,
    pub bot: Bot,
    pub chat_id: ChatId,
    pub approvals: Approvals,
    pub git: GitService,
}

/// Global telegram context, set during bot initialization.
static TELEGRAM_CONTEXT: OnceLock<Arc<RwLock<Option<TelegramContext>>>> = OnceLock::new();

static TELEGRAM_HANDLER_REGISTRATION: OnceCell<()> = OnceCell::const_new();

pub fn set_telegram_context(ctx: TelegramContext) {
    let lock = TELEGRAM_CONTEXT.get_or_init(|| Arc::new(RwLock::new(None)));
    if let Ok(mut guard) = lock.try_write() {
        *guard = Some(ctx);
    }
}

/// Clear the telegram context (for shutdown).
pub fn clear_telegram_context() {
    if let Some(lock) = TELEGRAM_CONTEXT.get() {
        if let Ok(mut guard) = lock.try_write() {
            *guard = None;
        }
    }
}

async fn get_context() -> Option<TelegramContext> {
    let lock = TELEGRAM_CONTEXT.get()?;
    lock.read().await.clone()
}

/// Register Telegram handlers with the task state dispatcher
pub async fn register_handlers(dispatcher: &TaskStateDispatcher) {
    TELEGRAM_HANDLER_REGISTRATION
        .get_or_init(|| async {
            TelegramHandlerWrapper::<TaskCreatedHandler>::register(dispatcher).await;
            TelegramHandlerWrapper::<TaskInProgressHandler>::register(dispatcher).await;
            TelegramHandlerWrapper::<TaskInReviewHandler>::register(dispatcher).await;
            TelegramHandlerWrapper::<TaskFinishedHandler>::register(dispatcher).await;
        })
        .await;
}

#[async_trait::async_trait]
pub trait TelegramHandler: Send + Sync + 'static {
    /// The filter for which transitions this handler should receive
    fn filter() -> TransitionFilter
    where
        Self: Sized;

    /// Handler name for logging/debugging
    fn name() -> &'static str
    where
        Self: Sized;

    /// The actual handler logic, called only when Telegram context is available
    async fn handle_with_context(&self, tg: &TelegramContext, transition: &TaskStateTransition);
}

/// Wrapper to convert a TelegramHandler into a TaskStateHandler.
pub struct TelegramHandlerWrapper<H: TelegramHandler> {
    _phantom: std::marker::PhantomData<H>,
}

impl<H: TelegramHandler> TelegramHandlerWrapper<H> {
    /// Register this handler with the dispatcher
    pub async fn register(dispatcher: &TaskStateDispatcher)
    where
        H: Default,
    {
        let handler = fn_handler(H::name(), H::filter(), |_, transition| {
            Box::pin(async move {
                let Some(tg) = get_context().await else {
                    return;
                };

                let handler = H::default();
                handler.handle_with_context(&tg, transition).await;
            })
        });
        dispatcher.register_handler(handler).await;
    }
}

/// Handler for newly created tasks (add a task)
#[derive(Default)]
pub struct TaskCreatedHandler;

#[async_trait::async_trait]
impl TelegramHandler for TaskCreatedHandler {
    fn filter() -> TransitionFilter {
        TransitionFilter::new().to(vec![TaskStatus::Todo])
    }

    fn name() -> &'static str {
        "on_task_created"
    }

    async fn handle_with_context(&self, tg: &TelegramContext, transition: &TaskStateTransition) {
        let task = &transition.task;
        let short_id = ShortIdMapping::get_or_create(&tg.db.pool, task.id)
            .await
            .unwrap_or_else(|_| "????".to_string());

        let project_name = match Project::find_by_id(&tg.db.pool, task.project_id).await {
            Ok(Some(project)) => project.name,
            _ => "Unknown".to_string(),
        };

        let message = format!(
            "[{}]Task Created\nProject: {}; Task: {}",
            short_id, project_name, task.title
        );
        tracing::info!("Sending telegram notification: {}", message);

        if let Err(err) = tg.bot.send_message(tg.chat_id, message).await {
            tracing::warn!("Failed to send telegram notification: {}", err);
        }
    }
}

/// Handler for Todo tasks transitioning to InProgress status (run a task)
#[derive(Default)]
pub struct TaskInProgressHandler;

#[async_trait::async_trait]
impl TelegramHandler for TaskInProgressHandler {
    fn filter() -> TransitionFilter {
        TransitionFilter::new()
            .from(vec![TaskStatus::Todo])
            .to(vec![TaskStatus::InProgress])
    }

    fn name() -> &'static str {
        "on_task_in_progress"
    }

    async fn handle_with_context(&self, tg: &TelegramContext, transition: &TaskStateTransition) {
        let task = &transition.task;
        let short_id = ShortIdMapping::get_or_create(&tg.db.pool, task.id)
            .await
            .unwrap_or_else(|_| "????".to_string());

        let workspace = match Workspace::fetch_all(&tg.db.pool, Some(task.id)).await {
            Ok(workspaces) => match workspaces.into_iter().next() {
                Some(workspace) => workspace,
                None => {
                    tracing::warn!("No workspace found for task {}", task.id);
                    return;
                }
            },
            Err(_) => {
                tracing::warn!("Failed to find workspace for task {}", task.id);
                return;
            }
        };

        let executor = match Session::find_latest_by_workspace_id(&tg.db.pool, workspace.id).await {
            Ok(Some(session)) => session.executor.unwrap_or_else(|| "Unknown".to_string()),
            _ => "Unknown".to_string(),
        };

        let project_name = match Project::find_by_id(&tg.db.pool, task.project_id).await {
            Ok(Some(project)) => project.name,
            _ => "Unknown".to_string(),
        };

        let message = format!(
            "[{}]Task Running on {} {}\nProject: {}; Task: {}",
            short_id, executor, workspace.branch, project_name, task.title
        );

        if let Err(err) = tg.bot.send_message(tg.chat_id, message).await {
            tracing::warn!("Failed to send telegram notification: {}", err);
        }
    }
}

/// Handler for tasks transitioning to InReview status
/// if the task is planned, include the plan in the message
#[derive(Default)]
pub struct TaskInReviewHandler;

#[async_trait::async_trait]
impl TelegramHandler for TaskInReviewHandler {
    fn filter() -> TransitionFilter {
        TransitionFilter::new().to(vec![TaskStatus::InReview])
    }

    fn name() -> &'static str {
        "on_task_in_review"
    }

    async fn handle_with_context(&self, tg: &TelegramContext, transition: &TaskStateTransition) {
        let task = &transition.task;

        let short_id = ShortIdMapping::get_or_create(&tg.db.pool, task.id)
            .await
            .unwrap_or_else(|_| "????".to_string());

        let mut message = format!("[{}] task{} InReview", short_id, task.title,);

        // override the message if the task is pending plan
        if let Some(plan) = find_exit_plan_approval(tg, task.id).await {
            tracing::debug!("Found exit plan approval: {:?}", plan.plan);
            message = format!("Plan: {}", plan.plan);
        }
        if let Err(err) = tg.bot.send_message(tg.chat_id, message).await {
            tracing::warn!("Failed to send telegram notification: {}", err);
        }
    }
}

/// Handler for tasks transitioning to Done status.
#[derive(Default)]
pub struct TaskFinishedHandler;

#[async_trait::async_trait]
impl TelegramHandler for TaskFinishedHandler {
    fn filter() -> TransitionFilter {
        TransitionFilter::new().to(vec![TaskStatus::Done])
    }

    fn name() -> &'static str {
        "on_task_done"
    }

    async fn handle_with_context(&self, tg: &TelegramContext, transition: &TaskStateTransition) {
        let task = &transition.task;
        let short_id = ShortIdMapping::get_or_create(&tg.db.pool, task.id)
            .await
            .unwrap_or_else(|_| "????".to_string());

        let message = format!(
            "[{}] 🎉🎉🎉 Task {} Finished\ndiff: {}",
            short_id,
            task.title,
            match compute_workspace_diff_stats(tg, task.id).await {
                Some((added, removed)) => format!("+{} / -{}", added, removed),
                None => format!("+{0} / -{0}", 0),
            }
        );

        if let Err(err) = tg.bot.send_message(tg.chat_id, message).await {
            tracing::warn!("Failed to send telegram notification: {}", err);
        }
    }
}

/// helper function

async fn compute_workspace_diff_stats(
    tg: &TelegramContext,
    task_id: uuid::Uuid,
) -> Option<(usize, usize)> {
    let workspaces = Workspace::fetch_all(&tg.db.pool, Some(task_id))
        .await
        .ok()?;
    let workspace = workspaces.into_iter().next()?;
    let container_ref = workspace.container_ref.as_ref()?;

    let workspace_repos =
        WorkspaceRepo::find_repos_with_target_branch_for_workspace(&tg.db.pool, workspace.id)
            .await
            .ok()?;

    let mut total_added = 0usize;
    let mut total_removed = 0usize;

    for repo_with_branch in workspace_repos {
        let worktree_path = PathBuf::from(container_ref).join(&repo_with_branch.repo.name);
        let workspace_branch = workspace.branch.clone();
        let target_branch = repo_with_branch.target_branch.clone();

        let base_commit = tg
            .git
            .get_base_commit(
                &repo_with_branch.repo.path,
                &workspace_branch,
                &target_branch,
            )
            .ok()?;

        let diffs = tg
            .git
            .get_diffs(
                crate::services::git::DiffTarget::Worktree {
                    worktree_path: &worktree_path,
                    base_commit: &base_commit,
                },
                None,
            )
            .ok()?;

        for diff in diffs {
            total_added += diff.additions.unwrap_or(0);
            total_removed += diff.deletions.unwrap_or(0);
        }
    }

    Some((total_added, total_removed))
}

struct PlanApproval {
    plan: String,
}

async fn find_exit_plan_approval(
    tg: &TelegramContext,
    task_id: uuid::Uuid,
) -> Option<PlanApproval> {
    for approval in tg.approvals.list_pending() {
        if approval.tool_name != EXIT_PLAN_MODE_NAME {
            continue;
        }

        let ctx = ExecutionProcess::load_context(&tg.db.pool, approval.execution_process_id).await;
        if let Ok(ctx) = ctx
            && ctx.task.id == task_id
        {
            return Some(PlanApproval {
                plan: approval.entry.content,
            });
        }
    }

    None
}
