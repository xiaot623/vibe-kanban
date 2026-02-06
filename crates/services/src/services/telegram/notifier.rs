//! Task state handlers for Telegram notifications.
//!
//! triggered when task status changes occur.

use std::sync::{Arc, OnceLock};

use db::{
    DBService,
    models::{
        execution_process::ExecutionProcess,
        project::Project,
        short_id_mapping::ShortIdMapping,
        task::{Task, TaskStatus},
    },
    task_state::{
        TaskStateTransition,
        dispatcher::TaskStateDispatcher,
        handler::{HandlerContext, TransitionFilter, fn_handler},
    },
};
use teloxide::{prelude::*, types::ParseMode};
use tokio::sync::{OnceCell, RwLock};

use super::EXIT_PLAN_MODE_NAME;
use crate::services::approvals::Approvals;

const TELEGRAM_MESSAGE_SAFE_LIMIT: usize = 3800;

/// Telegram context for event handlers.
#[derive(Clone)]
pub struct TelegramContext {
    pub db: DBService,
    pub bot: Bot,
    pub chat_id: ChatId,
    pub approvals: Approvals,
}

/// Global telegram context, set during bot initialization.
static TELEGRAM_CONTEXT: OnceLock<Arc<RwLock<Option<TelegramContext>>>> = OnceLock::new();

static TELEGRAM_HANDLER_REGISTRATION: OnceCell<()> = OnceCell::const_new();

/// Initialize the telegram context for event handlers.
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

/// Register Telegram handlers with the task state dispatcher.
pub async fn register_handlers(dispatcher: &TaskStateDispatcher) {
    TELEGRAM_HANDLER_REGISTRATION
        .get_or_init(|| async {
            let handlers = vec![
                fn_handler(
                    "on_task_created",
                    TransitionFilter::new().to(vec![TaskStatus::Todo]),
                    |ctx, transition| Box::pin(on_task_created(ctx, transition)),
                ),
                fn_handler(
                    "on_task_in_review",
                    TransitionFilter::new().to(vec![TaskStatus::InReview]),
                    |ctx, transition| Box::pin(on_task_in_review(ctx, transition)),
                ),
                fn_handler(
                    "on_task_status_change",
                    TransitionFilter::new().to(vec![
                        TaskStatus::Done,
                        TaskStatus::Cancelled,
                        TaskStatus::InProgress,
                    ]),
                    |ctx, transition| Box::pin(on_task_status_change(ctx, transition)),
                ),
                fn_handler(
                    "on_task_reopened",
                    TransitionFilter::new()
                        .from(vec![
                            TaskStatus::InProgress,
                            TaskStatus::InReview,
                            TaskStatus::Done,
                            TaskStatus::Cancelled,
                        ])
                        .to(vec![TaskStatus::Todo]),
                    |ctx, transition| Box::pin(on_task_reopened(ctx, transition)),
                ),
            ];

            for handler in handlers {
                dispatcher.register_handler(handler).await;
            }
        })
        .await;
}

async fn get_context() -> Option<TelegramContext> {
    let lock = TELEGRAM_CONTEXT.get()?;
    lock.read().await.clone()
}

/// Handler for newly created tasks (Todo status with no previous state).
pub async fn on_task_created(_ctx: &HandlerContext, transition: &TaskStateTransition) {
    if transition.from_status().is_some() {
        return;
    }

    let Some(tg) = get_context().await else {
        return;
    };

    let task = &transition.task;
    let short_id = ShortIdMapping::get_or_create(&tg.db.pool, task.id)
        .await
        .unwrap_or_else(|_| "????".to_string());

    let project_name = match Project::find_by_id(&tg.db.pool, task.project_id).await {
        Ok(Some(project)) => project.name,
        _ => "Unknown".to_string(),
    };

    let message = format!(
        "Task Created\n\n[{}] task{}\nProject: {}\nTitle: {}",
        short_id, task.id, project_name, task.title
    );

    if let Err(err) = tg.bot.send_message(tg.chat_id, message).await {
        tracing::warn!("Failed to send telegram notification: {}", err);
    }
}

/// Handler for tasks transitioning to InReview status.
pub async fn on_task_in_review(_ctx: &HandlerContext, transition: &TaskStateTransition) {
    if transition.from_status().is_none() {
        return;
    }

    let Some(tg) = get_context().await else {
        return;
    };

    let task = &transition.task;

    // Check for pending plan approval
    if let Some(plan_approval) = find_exit_plan_approval(&tg, task.id).await {
        send_plan_notification(&tg, task, &plan_approval).await;
    } else {
        let message = describe_task_for_notification(&tg, task).await;
        if let Err(err) = tg.bot.send_message(tg.chat_id, message).await {
            tracing::warn!("Failed to send telegram notification: {}", err);
        }
    }
}

/// Handler for task status changes (excluding creation).
pub async fn on_task_status_change(_ctx: &HandlerContext, transition: &TaskStateTransition) {
    let Some(old_status) = transition.from_status() else {
        return;
    };

    if old_status == transition.to_status() {
        return;
    }

    let Some(tg) = get_context().await else {
        return;
    };

    send_status_change_notification(&tg, &transition.task, old_status, transition.to_status())
        .await;
}

/// Handler for task moved back to Todo (re-opened).
pub async fn on_task_reopened(_ctx: &HandlerContext, transition: &TaskStateTransition) {
    let Some(old_status) = transition.from_status() else {
        return;
    };

    if matches!(old_status, TaskStatus::Todo) {
        return;
    }

    let Some(tg) = get_context().await else {
        return;
    };

    send_status_change_notification(&tg, &transition.task, old_status, transition.to_status())
        .await;
}

// Helper functions for notifications

async fn send_status_change_notification(
    tg: &TelegramContext,
    task: &Task,
    old_status: &TaskStatus,
    new_status: &TaskStatus,
) {
    let message = format!(
        "*Task Status Changed*\n\n*{}*\n{} -> {}",
        escape_markdown_v2(&task.title),
        escape_markdown_v2(&format!("{:?}", old_status)),
        escape_markdown_v2(&format!("{:?}", new_status))
    );

    if let Err(err) = tg
        .bot
        .send_message(tg.chat_id, message)
        .parse_mode(ParseMode::MarkdownV2)
        .await
    {
        tracing::warn!("Failed to send telegram notification: {}", err);
    }
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

async fn send_plan_notification(tg: &TelegramContext, task: &Task, plan_approval: &PlanApproval) {
    let short_id = ShortIdMapping::get_or_create(&tg.db.pool, task.id)
        .await
        .unwrap_or_else(|_| "????".to_string());

    let header = format!(
        "Plan ready for review\n\n[{}] task{}\nTitle: {}\n\nCommands:\n/approve task{}\n/reject task{} [reason]",
        short_id, task.id, task.title, task.id, task.id
    );

    if let Err(err) = tg.bot.send_message(tg.chat_id, header).await {
        tracing::warn!("Failed to send telegram notification: {}", err);
        return;
    }

    let chunks = split_text_chunks(&plan_approval.plan, TELEGRAM_MESSAGE_SAFE_LIMIT);
    if chunks.is_empty() {
        if let Err(err) = tg.bot.send_message(tg.chat_id, "Plan\n".to_string()).await {
            tracing::warn!("Failed to send telegram plan notification: {}", err);
        }
        return;
    }

    let total_parts = chunks.len();
    for (index, chunk) in chunks.into_iter().enumerate() {
        let prefix = if total_parts > 1 {
            format!("Plan (part {}/{})\n", index + 1, total_parts)
        } else {
            "Plan\n".to_string()
        };
        let message = format!("{prefix}{chunk}");
        if let Err(err) = tg.bot.send_message(tg.chat_id, message).await {
            tracing::warn!("Failed to send telegram plan notification: {}", err);
            break;
        }
    }
}

async fn describe_task_for_notification(tg: &TelegramContext, task: &Task) -> String {
    let project_name = match Project::find_by_id(&tg.db.pool, task.project_id).await {
        Ok(Some(project)) => project.name,
        _ => "Unknown project".to_string(),
    };

    let description = task
        .description
        .clone()
        .unwrap_or_else(|| "No description.".to_string());

    let short_id = ShortIdMapping::get_or_create(&tg.db.pool, task.id)
        .await
        .unwrap_or_else(|_| "????".to_string());

    format!(
        "[{}] task{}\nProject: {}\nStatus: {:?}\nTitle: {}\nDescription: {}",
        short_id,
        task.id,
        project_name,
        task.status,
        task.title,
        truncate_text(&description, 500)
    )
}

fn escape_markdown_v2(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '_' | '*' | '[' | ']' | '(' | ')' | '~' | '`' | '>' | '#' | '+' | '-' | '='
            | '|' | '{' | '}' | '.' | '!' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn truncate_text(input: &str, limit: usize) -> String {
    let mut chars = input.chars();
    let truncated: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn split_text_chunks(input: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return vec![input.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;

    for ch in input.chars() {
        if count >= limit {
            chunks.push(current);
            current = String::new();
            count = 0;
        }
        current.push(ch);
        count += 1;
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}
