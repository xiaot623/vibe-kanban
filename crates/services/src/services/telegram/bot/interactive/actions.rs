use std::str::FromStr;

use chrono::Utc;
use db::models::{
    short_id_mapping::ShortIdMapping,
    task::{CreateTask, Task, TaskStatus, UpdateTask},
};
use executors::{executors::BaseCodingAgent, profile::ExecutorConfigs};
use teloxide::{
    prelude::*,
    types::{InlineKeyboardMarkup, MessageId},
};
use utils::{
    approvals::{ApprovalResponse, ApprovalStatus},
    response::ApiResponse,
};
use uuid::Uuid;

use super::{
    super::shared::{api_base_url, format_review_task_created_message, truncate_text},
    BotDialogue, CardRenderContext, TelegramBotService,
};
use crate::services::{
    approvals::{ApprovalError, PendingApprovalInfo},
    telegram::{bot::PinnedProjectState, flow, format, keyboard, notifier, state::DialogueState},
};

pub(super) async fn handle_tool_approval_callback(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    approval_id: &str,
    approve: bool,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let Some(pending_approval) = service.approvals.pending_by_id(approval_id) else {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "This approval is no longer pending.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    };

    if approval_requires_structured_input(&pending_approval) {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "This approval requires structured input. Please continue in the Web UI.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    }

    let response_status = if approve {
        ApprovalStatus::Approved
    } else {
        ApprovalStatus::Denied {
            reason: Some("Rejected from Telegram".to_string()),
        }
    };

    match service
        .approvals
        .respond(
            &service.db.pool,
            approval_id,
            ApprovalResponse {
                execution_process_id: pending_approval.execution_process_id,
                status: response_status,
            },
        )
        .await
    {
        Ok(_) => {
            let message = if approve {
                "✅ Tool request approved."
            } else {
                "🛑 Tool request rejected."
            };
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                message,
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
        Err(ApprovalError::NotFound | ApprovalError::AlreadyCompleted) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                "This approval has already been handled.",
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
        Err(e) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                &format!("Failed to process approval: {e}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
    }

    Ok(())
}

pub(super) fn approval_requires_structured_input(approval: &PendingApprovalInfo) -> bool {
    approval
        .tool_name
        .eq_ignore_ascii_case("request_user_input")
}

pub(super) async fn handle_run_default(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let config = service.config.read().await.telegram.clone();
    let result = service
        .run_task_impl(task_id, &config.default_executor, None, None)
        .await;
    match result {
        Ok(()) => {
            if let Some(context) = card_context {
                // Keep run notifications, but remove the source interaction card.
                super::ui::delete_message_best_effort(bot, chat_id, context.source_message_id)
                    .await;
            }
        }
        Err(msg) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to start run: {msg}"),
                Some(run_failure_keyboard(service, task_id).await),
            )
            .await?;
        }
    }
    Ok(())
}

fn normalize_running_label(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

async fn send_running_message(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    session_id: Option<Uuid>,
    flow_token: Option<&str>,
    executor: &str,
    mode: &str,
) {
    let task_title = match Task::find_by_id(&service.db.pool, task_id).await {
        Ok(Some(task)) => task.title,
        _ => "Unknown task".to_string(),
    };
    let executor_label = normalize_running_label(executor, "UNKNOWN");
    let mode_label = normalize_running_label(mode, "DEFAULT");
    let flow_label = flow_token
        .map(|token| format!("[{executor_label} · {token}]"))
        .unwrap_or_else(|| format!("[{executor_label}]"));
    let message = format!(
        "🏃 {flow_label} Running\nTask: {task_title}\nExecutor: {executor_label}\nMode: {mode_label}"
    );

    match format::send_rich_then_plain(
        bot,
        chat_id,
        &message,
        Some(super::ui::empty_inline_keyboard()),
    )
    .await
    {
        Ok(sent) => {
            if let Some(session_id) = session_id {
                notifier::record_running_message_id(session_id, sent.id).await;
            }
        }
        Err(err) => {
            tracing::warn!("Failed to send Telegram running message: {err}");
        }
    }
}

pub(super) fn available_run_modes_for_executor(executor: &str) -> Vec<String> {
    let executor = match BaseCodingAgent::from_str(executor) {
        Ok(executor) => executor,
        Err(_) => return vec!["DEFAULT".to_string()],
    };

    let profiles = ExecutorConfigs::get_cached();
    let Some(executor_config) = profiles.executors.get(&executor) else {
        return vec!["DEFAULT".to_string()];
    };

    let mut modes: Vec<String> = executor_config.configurations.keys().cloned().collect();
    if modes.is_empty() {
        return vec!["DEFAULT".to_string()];
    }

    modes.sort();
    if let Some(default_index) = modes.iter().position(|mode| mode == "DEFAULT") {
        let default_mode = modes.remove(default_index);
        modes.insert(0, default_mode);
    }

    modes
}

pub(super) async fn handle_run_with_executor_mode(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    executor: &str,
    mode_index: u16,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let run_modes = available_run_modes_for_executor(executor);
    let Some(selected_mode) = run_modes.get(mode_index as usize) else {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "Invalid run mode selection. Please try again.",
            Some(keyboard::executor_pick_keyboard(task_id)),
        )
        .await?;
        return Ok(());
    };

    let result = service
        .run_task_impl(task_id, executor, Some(selected_mode.as_str()), None)
        .await;
    match result {
        Ok(()) => {
            if let Some(context) = card_context {
                super::ui::delete_message_best_effort(bot, chat_id, context.source_message_id)
                    .await;
            }
            send_running_message(
                bot,
                chat_id,
                service,
                task_id,
                None,
                None,
                executor,
                selected_mode,
            )
            .await;
        }
        Err(msg) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to start run with {executor}/{selected_mode}: {msg}"),
                Some(run_failure_keyboard(service, task_id).await),
            )
            .await?;
        }
    }
    Ok(())
}

async fn run_failure_keyboard(service: &TelegramBotService, task_id: Uuid) -> InlineKeyboardMarkup {
    match Task::find_by_id(&service.db.pool, task_id).await {
        Ok(Some(task)) => keyboard::task_detail_keyboard(task_id, &task.status),
        _ => super::ui::empty_inline_keyboard(),
    }
}

pub(super) async fn handle_approve_confirm(
    bot: &Bot,
    chat_id: ChatId,
    task_id: Uuid,
) -> ResponseResult<()> {
    format::send_rich_then_plain(
        bot,
        chat_id,
        "⚠️ Confirm plan approval?",
        Some(keyboard::approve_confirm_keyboard(task_id)),
    )
    .await?;
    Ok(())
}

pub(super) fn approval_completion_cleanup_plan(
    execution_process_id: Option<Uuid>,
    card_context: Option<CardRenderContext>,
) -> super::ui::CompletionCleanupPlan {
    let interaction_message_id = card_context.map(|ctx| ctx.source_message_id);

    super::ui::CompletionCleanupPlan {
        delete_plan_review_execution_process_id: execution_process_id,
        delete_plan_message_id: None,
        delete_interaction_message_id: interaction_message_id,
        send_result_as_new_message: true,
    }
}

pub(super) fn reject_completion_cleanup_plan(
    execution_process_id: Option<Uuid>,
    plan_message_id: Option<MessageId>,
    card_context: Option<CardRenderContext>,
) -> super::ui::CompletionCleanupPlan {
    super::ui::CompletionCleanupPlan {
        delete_plan_review_execution_process_id: execution_process_id,
        delete_plan_message_id: plan_message_id,
        delete_interaction_message_id: card_context.map(|ctx| ctx.source_message_id),
        send_result_as_new_message: true,
    }
}

pub(super) fn review_completion_cleanup_plan(
    card_context: Option<CardRenderContext>,
) -> super::ui::CompletionCleanupPlan {
    super::ui::CompletionCleanupPlan {
        delete_plan_review_execution_process_id: None,
        delete_plan_message_id: None,
        delete_interaction_message_id: card_context.map(|ctx| ctx.source_message_id),
        send_result_as_new_message: true,
    }
}

pub(super) fn unavailable_flow_message() -> &'static str {
    "This executor flow is no longer available."
}

fn parse_legacy_task_token(flow_token: &str) -> Option<Uuid> {
    flow_token
        .strip_prefix("legacy-task:")
        .and_then(|raw| Uuid::parse_str(raw).ok())
}

fn to_request_error<E: std::fmt::Display>(err: E) -> teloxide::RequestError {
    teloxide::RequestError::Io(std::io::Error::other(err.to_string()))
}

async fn resolve_flow_context_or_render_unavailable(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    flow_token: &str,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<Option<crate::services::telegram::flow::TelegramFlowContext>> {
    let flow_ctx = flow::resolve_flow_context(&service.db.pool, flow_token)
        .await
        .map_err(to_request_error)?;
    if flow_ctx.is_none() {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            unavailable_flow_message(),
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
    }
    Ok(flow_ctx)
}

pub(super) async fn handle_approve(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    flow_token: &str,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    if let Some(task_id) = parse_legacy_task_token(flow_token) {
        return handle_approve_legacy_task(bot, chat_id, service, task_id, card_context).await;
    }

    let Some(flow_ctx) =
        resolve_flow_context_or_render_unavailable(bot, chat_id, service, flow_token, card_context)
            .await?
    else {
        return Ok(());
    };

    let Some(plan_approval) = service
        .find_exit_plan_approval_for_flow(flow_ctx.session_id, flow_ctx.latest_execution_process_id)
        .await
    else {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "No pending plan approval found. It may have expired.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    };

    match service
        .approvals
        .respond(
            &service.db.pool,
            &plan_approval.approval_id,
            ApprovalResponse {
                execution_process_id: plan_approval.execution_process_id,
                status: ApprovalStatus::Approved,
            },
        )
        .await
    {
        Ok(_) => {
            let cleanup = approval_completion_cleanup_plan(
                Some(plan_approval.execution_process_id),
                card_context,
            );
            super::ui::finalize_completion_result(
                bot,
                chat_id,
                cleanup,
                card_context,
                Some("✅ Plan approved!"),
            )
            .await?;
        }
        Err(e) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to approve plan: {e}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
    }
    Ok(())
}

pub(super) async fn handle_approve_legacy_task(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let Some(plan_approval) = service.find_exit_plan_approval_by_task(task_id).await else {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "No pending plan approval found. It may have expired.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    };

    match service
        .approvals
        .respond(
            &service.db.pool,
            &plan_approval.approval_id,
            ApprovalResponse {
                execution_process_id: plan_approval.execution_process_id,
                status: ApprovalStatus::Approved,
            },
        )
        .await
    {
        Ok(_) => {
            let cleanup = approval_completion_cleanup_plan(
                Some(plan_approval.execution_process_id),
                card_context,
            );
            super::ui::finalize_completion_result(
                bot,
                chat_id,
                cleanup,
                card_context,
                Some("✅ Plan approved!"),
            )
            .await?;
        }
        Err(e) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to approve plan: {e}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
    }
    Ok(())
}

pub(super) async fn handle_flow_approve_confirm(
    bot: &Bot,
    chat_id: ChatId,
    flow_token: &str,
) -> ResponseResult<()> {
    format::send_rich_then_plain(
        bot,
        chat_id,
        "⚠️ Confirm plan approval?",
        Some(keyboard::flow_approve_confirm_keyboard(flow_token)),
    )
    .await?;
    Ok(())
}

pub(super) async fn handle_reject_start(
    bot: &Bot,
    chat_id: ChatId,
    flow_token: &str,
    dialogue: &BotDialogue,
    _card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    if parse_legacy_task_token(flow_token).is_some() {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            _card_context,
            "This older card is not flow-aware anymore; use a newer summary/run card.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    }

    let prompt_message = format::send_rich_then_plain(
        bot,
        chat_id,
        "Enter a rejection reason (or send any text to reject without reason):",
        Some(keyboard::interaction_cancel_keyboard()),
    )
    .await?;
    dialogue
        .update(
            crate::services::telegram::state::DialogueState::RejectingPlan {
                flow_token: flow_token.to_string(),
                prompt_message_id: prompt_message.id.0,
            },
        )
        .await
        .ok();
    Ok(())
}

pub(super) async fn handle_reject_finish(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    flow_token: &str,
    plan_message_id: Option<MessageId>,
    reason: Option<&str>,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<bool> {
    if let Some(task_id) = parse_legacy_task_token(flow_token) {
        return handle_reject_finish_legacy_task(
            bot,
            chat_id,
            service,
            task_id,
            plan_message_id,
            reason,
            card_context,
        )
        .await;
    }

    let Some(flow_ctx) =
        resolve_flow_context_or_render_unavailable(bot, chat_id, service, flow_token, card_context)
            .await?
    else {
        return Ok(false);
    };

    let Some(plan_approval) = service
        .find_exit_plan_approval_for_flow(flow_ctx.session_id, flow_ctx.latest_execution_process_id)
        .await
    else {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "No pending plan approval found. It may have expired.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(false);
    };

    match service
        .approvals
        .respond(
            &service.db.pool,
            &plan_approval.approval_id,
            ApprovalResponse {
                execution_process_id: plan_approval.execution_process_id,
                status: ApprovalStatus::Denied {
                    reason: reason.map(String::from),
                },
            },
        )
        .await
    {
        Ok(_) => {
            let cleanup = reject_completion_cleanup_plan(
                Some(plan_approval.execution_process_id),
                plan_message_id,
                card_context,
            );
            super::ui::finalize_completion_result(
                bot,
                chat_id,
                cleanup,
                card_context,
                Some("📝 Plan rejected."),
            )
            .await?;
            return Ok(true);
        }
        Err(e) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to reject plan: {e}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
    }
    Ok(false)
}

pub(super) async fn handle_reject_finish_legacy_task(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    plan_message_id: Option<MessageId>,
    reason: Option<&str>,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<bool> {
    let Some(plan_approval) = service.find_exit_plan_approval_by_task(task_id).await else {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "No pending plan approval found. It may have expired.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(false);
    };

    match service
        .approvals
        .respond(
            &service.db.pool,
            &plan_approval.approval_id,
            ApprovalResponse {
                execution_process_id: plan_approval.execution_process_id,
                status: ApprovalStatus::Denied {
                    reason: reason.map(String::from),
                },
            },
        )
        .await
    {
        Ok(_) => {
            let cleanup = reject_completion_cleanup_plan(
                Some(plan_approval.execution_process_id),
                plan_message_id,
                card_context,
            );
            super::ui::finalize_completion_result(
                bot,
                chat_id,
                cleanup,
                card_context,
                Some("📝 Plan rejected."),
            )
            .await?;
            return Ok(true);
        }
        Err(e) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to reject plan: {e}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
    }
    Ok(false)
}

pub(super) async fn handle_follow_up_reply_start(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    flow_token: &str,
    dialogue: &BotDialogue,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    if parse_legacy_task_token(flow_token).is_some() {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "This older card is not flow-aware anymore; use a newer summary/run card.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    }

    let Some(flow_ctx) =
        resolve_flow_context_or_render_unavailable(bot, chat_id, service, flow_token, card_context)
            .await?
    else {
        return Ok(());
    };
    let prompt_message_id = super::ui::render_or_send_card(
        bot,
        chat_id,
        card_context,
        "Type your reply here:",
        Some(keyboard::cancel_keyboard()),
    )
    .await?;
    dialogue
        .update(
            crate::services::telegram::state::DialogueState::ReplyingFollowUp {
                flow_token: flow_ctx.flow_token,
                session_id: flow_ctx.session_id,
                prompt_message_id: prompt_message_id.0,
            },
        )
        .await
        .ok();
    Ok(())
}

pub(super) async fn handle_follow_up_reply_start_legacy_task(
    bot: &Bot,
    chat_id: ChatId,
    task_id: Uuid,
    dialogue: &BotDialogue,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let prompt_message_id = super::ui::render_or_send_card(
        bot,
        chat_id,
        card_context,
        "This older card is not flow-aware anymore; use a newer summary/run card.",
        Some(super::ui::empty_inline_keyboard()),
    )
    .await?;
    dialogue
        .update(
            crate::services::telegram::state::DialogueState::ReplyingFollowUp {
                flow_token: format!("legacy-task:{task_id}"),
                session_id: task_id,
                prompt_message_id: prompt_message_id.0,
            },
        )
        .await
        .ok();
    Ok(())
}

pub(super) async fn handle_follow_up_reply_finish(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    flow_token: &str,
    session_id: Uuid,
    prompt: &str,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<bool> {
    if parse_legacy_task_token(flow_token).is_some() {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "This older card is not flow-aware anymore; use a newer summary/run card.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(false);
    }

    let Some(flow_ctx) =
        resolve_flow_context_or_render_unavailable(bot, chat_id, service, flow_token, card_context)
            .await?
    else {
        return Ok(false);
    };

    if flow_ctx.session_id != session_id {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "This executor flow context is out of date. Please open a newer flow summary card.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(false);
    }

    match service.send_follow_up_reply(session_id, prompt).await {
        Ok(latest_profile) => {
            let (executor_label, mode_label) = if let Some(profile) = latest_profile {
                let mode = profile
                    .variant
                    .clone()
                    .unwrap_or_else(|| "DEFAULT".to_string());
                (profile.executor.to_string(), mode)
            } else {
                let config = service.config.read().await.telegram.clone();
                let mode = normalize_running_label(&config.default_mode, "DEFAULT");
                (config.default_executor, mode)
            };

            send_running_message(
                bot,
                chat_id,
                service,
                flow_ctx.task_id,
                Some(session_id),
                Some(flow_token),
                &executor_label,
                &mode_label,
            )
            .await;
            return Ok(true);
        }
        Err(err) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to send reply: {err}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
    }
    Ok(false)
}

pub(super) async fn handle_done_task(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    flow_token: &str,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    if parse_legacy_task_token(flow_token).is_some() {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "This older card is not flow-aware anymore; use a newer summary/run card.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    }

    let Some(flow_ctx) =
        resolve_flow_context_or_render_unavailable(bot, chat_id, service, flow_token, card_context)
            .await?
    else {
        return Ok(());
    };

    let Some(task) = super::ui::load_task_or_render_error(
        bot,
        chat_id,
        &service.db.pool,
        flow_ctx.task_id,
        card_context,
        "Task not found. It may have been deleted.",
        "Failed to load task",
    )
    .await?
    else {
        return Ok(());
    };

    let daily_project_id = {
        let config = service.config.read().await;
        config.daily_mode.project_id.clone()
    };
    let Some(daily_project_id) = daily_project_id else {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "Daily Mode is not configured.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    };

    let daily_project_id = match Uuid::parse_str(&daily_project_id) {
        Ok(id) => id,
        Err(e) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Daily Mode project id is invalid. Please reconfigure it: {e}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
            return Ok(());
        }
    };

    if task.project_id != daily_project_id {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "Done is only available for Daily Project tasks from this card.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    }

    match service
        .has_running_sibling_flow(task.id, flow_ctx.session_id)
        .await
    {
        Ok(true) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                "Cannot mark Done while another executor flow for this task is still running.",
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
            return Ok(());
        }
        Ok(false) => {}
        Err(err) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to validate concurrent flows: {err}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
            return Ok(());
        }
    }

    let mut status_errors = Vec::new();
    let status_updated = if task.status == TaskStatus::Done {
        true
    } else {
        match Task::update_status(&service.db.pool, task.id, TaskStatus::Done).await {
            Ok(_) => true,
            Err(e) => {
                status_errors.push(format!("Failed to mark task Done: {e}"));
                false
            }
        }
    };

    super::ui::render_or_send_card(
        bot,
        chat_id,
        card_context,
        format_done_task_status_message(&task.title, status_updated, &status_errors),
        Some(super::ui::empty_inline_keyboard()),
    )
    .await?;
    Ok(())
}

fn format_done_task_status_message(
    task_title: &str,
    status_updated: bool,
    status_errors: &[String],
) -> String {
    let status_line = if status_updated {
        format!("✅ Task {task_title} has Finished")
    } else {
        format!("❌ Failed to update task {task_title} state")
    };

    if status_errors.is_empty() {
        return status_line;
    }

    let details = status_errors
        .iter()
        .map(|item| format!("- {}", truncate_text(item, 220)))
        .collect::<Vec<_>>()
        .join("\n");

    format!("{status_line}\n\nDetails:\n{details}")
}

pub(super) async fn handle_create_review_task(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    flow_token: &str,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    if parse_legacy_task_token(flow_token).is_some() {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "This older card is not flow-aware anymore; use a newer summary/run card.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    }

    let Some(flow_ctx) =
        resolve_flow_context_or_render_unavailable(bot, chat_id, service, flow_token, card_context)
            .await?
    else {
        return Ok(());
    };

    match service
        .create_review_task_from_workspace(flow_ctx.workspace_id)
        .await
    {
        Ok(result) => {
            tracing::info!("Created review task {}", result.task.id);

            let short_id = ShortIdMapping::get_or_create(&service.db.pool, result.task.id)
                .await
                .unwrap_or_else(|_| "????".to_string());

            let message =
                format_review_task_created_message(&short_id, result.task.has_in_progress_attempt);
            let cleanup = review_completion_cleanup_plan(card_context);
            super::ui::finalize_completion_result(bot, chat_id, cleanup, card_context, None)
                .await?;

            format::send_rich_then_plain(
                bot,
                chat_id,
                &message,
                Some(keyboard::task_detail_keyboard(
                    result.task.id,
                    &result.task.status,
                )),
            )
            .await?;
        }
        Err(err) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to create review task: {err}\n\nYou can retry or cancel."),
                Some(keyboard::review_confirm_keyboard(flow_ctx.task_id)),
            )
            .await?;
        }
    }

    Ok(())
}

pub(super) async fn handle_create_review_task_confirm(
    bot: &Bot,
    chat_id: ChatId,
    flow_token: &str,
) -> ResponseResult<()> {
    if parse_legacy_task_token(flow_token).is_some() {
        format::send_rich_then_plain(
            bot,
            chat_id,
            "This older card is not flow-aware anymore; use a newer summary/run card.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    }

    format::send_rich_then_plain(
        bot,
        chat_id,
        "Confirm creating a review task?",
        Some(keyboard::review_confirm_flow_keyboard(flow_token)),
    )
    .await?;
    Ok(())
}

/// Show a duration picker for the selected project during pinning.
pub(super) async fn handle_pin_project_select(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    project_id: Uuid,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let project_name =
        match db::models::project::Project::find_by_id(&service.db.pool, project_id).await {
            Ok(Some(p)) => p.name,
            _ => {
                super::ui::render_or_send_card(
                    bot,
                    chat_id,
                    card_context,
                    "Project not found.",
                    Some(super::ui::empty_inline_keyboard()),
                )
                .await?;
                return Ok(());
            }
        };

    super::ui::render_or_send_card(
        bot,
        chat_id,
        card_context,
        format!("📌 Pin **{}** — select duration:", project_name),
        Some(keyboard::pin_duration_keyboard(project_id)),
    )
    .await?;
    Ok(())
}

/// Apply the selected pin (project + duration) and show confirmation.
pub(super) async fn handle_pin_duration_select(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    project_id: Uuid,
    minutes: u32,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let project_name =
        match db::models::project::Project::find_by_id(&service.db.pool, project_id).await {
            Ok(Some(p)) => p.name,
            _ => {
                super::ui::render_or_send_card(
                    bot,
                    chat_id,
                    card_context,
                    "Project not found. Pin not set.",
                    Some(super::ui::empty_inline_keyboard()),
                )
                .await?;
                return Ok(());
            }
        };

    let expires_at = Utc::now() + chrono::Duration::minutes(i64::from(minutes));
    let duration_label = format_duration_label(minutes);
    service
        .set_pin(PinnedProjectState {
            project_id,
            project_name: project_name.clone(),
            expires_at,
        })
        .await;

    let confirmation = format!(
        "📍 Pinned **{project_name}** for {duration_label}.\n\n/new and ➕ New will now skip project selection.",
    );
    super::ui::render_or_send_card(
        bot,
        chat_id,
        card_context,
        confirmation,
        Some(keyboard::home_keyboard(true)),
    )
    .await?;
    Ok(())
}

fn format_duration_label(minutes: u32) -> String {
    if minutes < 60 {
        format!("{minutes}min")
    } else {
        format!("{}h", minutes / 60)
    }
}

/// Enter task-creation dialogue for a pinned project without showing the
/// project picker. Requires the dialogue handle for state update.
pub(super) async fn handle_new_task_project_for_pin_full(
    bot: &Bot,
    chat_id: ChatId,
    _service: &TelegramBotService,
    project_id: Uuid,
    project_name: &str,
    dialogue: &BotDialogue,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let prompt_message_id = super::ui::render_or_send_card(
        bot,
        chat_id,
        card_context,
        format_new_task_message_prompt(project_name),
        Some(keyboard::cancel_keyboard()),
    )
    .await?;

    dialogue
        .update(build_creating_task_message_state(
            project_id,
            prompt_message_id.0,
        ))
        .await
        .ok();
    Ok(())
}

pub(super) async fn handle_edit_start(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    dialogue: &BotDialogue,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let Some(task) = super::ui::load_task_or_render_error(
        bot,
        chat_id,
        &service.db.pool,
        task_id,
        card_context,
        "Task not found.",
        "Failed to load task",
    )
    .await?
    else {
        return Ok(());
    };

    if task.status != TaskStatus::Todo {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            format!("Task is {:?}. Only Todo tasks can be edited.", task.status),
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    }

    let prompt_message_id = super::ui::render_or_send_card(
        bot,
        chat_id,
        card_context,
        format!("Current title: {}\n\nEnter a new title:", task.title),
        Some(keyboard::cancel_keyboard()),
    )
    .await?;

    dialogue
        .update(
            crate::services::telegram::state::DialogueState::EditingTaskTitle {
                task_id,
                current_title: task.title.clone(),
                prompt_message_id: prompt_message_id.0,
            },
        )
        .await
        .ok();

    Ok(())
}

pub(super) async fn handle_edit_task_finish(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    title: &str,
    description: Option<&str>,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<bool> {
    let payload = UpdateTask {
        title: Some(title.to_string()),
        description: description.map(str::to_string),
        status: None,
        parent_workspace_id: None,
        image_ids: None,
    };

    let base_url = match api_base_url().await {
        Ok(url) => url,
        Err(e) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to locate API server: {e}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
            return Ok(false);
        }
    };

    let client = reqwest::Client::new();
    let response = match client
        .put(format!("{base_url}/tasks/{task_id}"))
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to edit task: {e}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
            return Ok(false);
        }
    };

    let api_response: ApiResponse<Task> = match response.json().await {
        Ok(r) => r,
        Err(e) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to parse response: {e}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
            return Ok(false);
        }
    };

    let error_message = api_response.message().map(String::from);
    match api_response.into_data() {
        Some(updated_task) => {
            let short_id = ShortIdMapping::get_or_create(&service.db.pool, updated_task.id)
                .await
                .unwrap_or_else(|_| "????".to_string());
            super::ui::render_or_send_card(
                bot,
                chat_id,
                None,
                format!("✏️ Updated [{}]\nTitle: {}", short_id, updated_task.title),
                Some(keyboard::task_detail_keyboard(
                    updated_task.id,
                    &updated_task.status,
                )),
            )
            .await?;
            return Ok(true);
        }
        None => {
            let msg = error_message.unwrap_or_else(|| "Failed to edit task.".to_string());
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                msg,
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
    }
    Ok(false)
}

pub(super) async fn handle_new_task_project(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    project_id: Uuid,
    dialogue: &BotDialogue,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let Some(project) = super::ui::load_project_or_render_error(
        bot,
        chat_id,
        &service.db.pool,
        project_id,
        card_context,
        "Project not found.",
        "Failed to load project",
    )
    .await?
    else {
        return Ok(());
    };

    let prompt_message_id = super::ui::render_or_send_card(
        bot,
        chat_id,
        card_context,
        format_new_task_message_prompt(&project.name),
        Some(keyboard::cancel_keyboard()),
    )
    .await?;

    dialogue
        .update(build_creating_task_message_state(
            project_id,
            prompt_message_id.0,
        ))
        .await
        .ok();
    Ok(())
}

fn format_new_task_message_prompt(project_name: &str) -> String {
    format!(
        "➕ New task in {project_name}\n\nSend one message to create the task:\n- First line: title\n- Remaining lines: detail\n- Single line: empty detail"
    )
}

fn build_creating_task_message_state(project_id: Uuid, prompt_message_id: i32) -> DialogueState {
    DialogueState::CreatingTaskMessage {
        project_id,
        prompt_message_id,
    }
}

pub(super) async fn handle_create_task_finish(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    project_id: Uuid,
    title: &str,
    description: Option<&str>,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<bool> {
    let task_id = Uuid::new_v4();
    match Task::create(
        &service.db.pool,
        &CreateTask::from_title_description(
            project_id,
            title.to_string(),
            description.map(String::from),
        ),
        task_id,
    )
    .await
    {
        Ok(_task) => {
            // TaskCreatedHandler sends the create confirmation with action buttons.
            return Ok(true);
        }
        Err(e) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to create task: {e}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use executors::logs::{ActionType, NormalizedEntry, NormalizedEntryType, ToolStatus};
    use teloxide::types::MessageId;
    use uuid::Uuid;

    use super::{
        super::CardRenderContext, approval_completion_cleanup_plan,
        approval_requires_structured_input, build_creating_task_message_state,
        format_done_task_status_message, format_new_task_message_prompt,
        reject_completion_cleanup_plan, review_completion_cleanup_plan,
    };
    use crate::services::{approvals::PendingApprovalInfo, telegram::state::DialogueState};

    #[test]
    fn request_user_input_requires_structured_handling() {
        let approval = PendingApprovalInfo {
            id: Uuid::new_v4().to_string(),
            tool_name: "request_user_input".to_string(),
            execution_process_id: Uuid::new_v4(),
            entry: NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::ToolUse {
                    tool_name: "request_user_input".to_string(),
                    action_type: ActionType::Tool {
                        tool_name: "request_user_input".to_string(),
                        arguments: None,
                        result: None,
                    },
                    status: ToolStatus::PendingApproval {
                        approval_id: Uuid::new_v4().to_string(),
                        requested_at: chrono::Utc::now(),
                        timeout_at: chrono::Utc::now(),
                    },
                },
                content: "request user input".to_string(),
                metadata: None,
            },
        };

        assert!(approval_requires_structured_input(&approval));
    }

    #[test]
    fn approve_cleanup_plan_deletes_interaction_and_sends_new_message() {
        let context = Some(CardRenderContext {
            source_message_id: MessageId(42),
        });

        let cleanup = approval_completion_cleanup_plan(None, context);

        assert_eq!(cleanup.delete_plan_review_execution_process_id, None);
        assert_eq!(cleanup.delete_plan_message_id, None);
        assert_eq!(cleanup.delete_interaction_message_id, Some(MessageId(42)));
        assert!(cleanup.send_result_as_new_message);
    }

    #[test]
    fn reject_cleanup_plan_deletes_plan_and_interaction_messages() {
        let cleanup = reject_completion_cleanup_plan(
            None,
            Some(MessageId(7)),
            Some(CardRenderContext {
                source_message_id: MessageId(8),
            }),
        );

        assert_eq!(cleanup.delete_plan_review_execution_process_id, None);
        assert_eq!(cleanup.delete_plan_message_id, Some(MessageId(7)));
        assert_eq!(cleanup.delete_interaction_message_id, Some(MessageId(8)));
        assert!(cleanup.send_result_as_new_message);
    }

    #[test]
    fn review_cleanup_plan_only_deletes_interaction_message() {
        let cleanup = review_completion_cleanup_plan(Some(CardRenderContext {
            source_message_id: MessageId(99),
        }));

        assert_eq!(cleanup.delete_plan_review_execution_process_id, None);
        assert_eq!(cleanup.delete_plan_message_id, None);
        assert_eq!(cleanup.delete_interaction_message_id, Some(MessageId(99)));
        assert!(cleanup.send_result_as_new_message);
    }

    #[test]
    fn done_task_success_message_contains_only_status() {
        let message = format_done_task_status_message("Ship release", true, &[]);

        assert_eq!(message, "✅ Task Ship release has Finished");
        assert!(!message.contains("Merged"));
        assert!(!message.contains("mergeable"));
    }

    #[test]
    fn done_task_failure_message_contains_error_details_without_merge_summary() {
        let message = format_done_task_status_message(
            "Ship release",
            false,
            &[String::from(
                "Failed to mark task Done: database unavailable",
            )],
        );

        assert!(message.starts_with("❌ Failed to update task Ship release state"));
        assert!(message.contains("Details:\n- Failed to mark task Done: database unavailable"));
        assert!(!message.contains("Merged"));
        assert!(!message.contains("No mergeable commits found."));
    }

    #[test]
    fn new_task_project_uses_single_message_prompt_copy() {
        let prompt = format_new_task_message_prompt("Daily");

        assert!(prompt.contains("First line: title"));
        assert!(prompt.contains("Remaining lines: detail"));
        assert!(prompt.contains("Single line: empty detail"));
        assert!(!prompt.contains("Enter the task title"));
    }

    #[test]
    fn new_task_project_enters_single_step_message_state() {
        let project_id = Uuid::new_v4();
        let state = build_creating_task_message_state(project_id, 42);

        assert!(matches!(
            state,
            DialogueState::CreatingTaskMessage {
                project_id: id,
                prompt_message_id: 42,
            } if id == project_id
        ));
    }
}
