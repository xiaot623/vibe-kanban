use std::str::FromStr;

use db::models::{
    short_id_mapping::ShortIdMapping,
    task::{CreateTask, Task, TaskStatus, UpdateTask},
    workspace::Workspace,
};
use executors::{executors::BaseCodingAgent, profile::ExecutorConfigs};
use serde::{Deserialize, Serialize};
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
    telegram::{format, keyboard},
};

#[derive(Debug, Deserialize)]
struct DoneRepoBranchStatus {
    repo_id: Uuid,
    commits_ahead: Option<usize>,
}

#[derive(Debug, Serialize)]
struct MergeTaskAttemptBody {
    repo_id: Uuid,
}

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
                Some(keyboard::home_only_keyboard()),
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
        _ => keyboard::home_only_keyboard(),
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
    card_context: Option<CardRenderContext>,
) -> super::ui::CompletionCleanupPlan {
    let interaction_message_id = card_context.map(|ctx| ctx.source_message_id);

    super::ui::CompletionCleanupPlan {
        delete_plan_review_cards: true,
        delete_plan_message_id: None,
        delete_interaction_message_id: interaction_message_id,
        send_result_as_new_message: true,
    }
}

pub(super) fn reject_completion_cleanup_plan(
    plan_message_id: Option<MessageId>,
    card_context: Option<CardRenderContext>,
) -> super::ui::CompletionCleanupPlan {
    super::ui::CompletionCleanupPlan {
        delete_plan_review_cards: true,
        delete_plan_message_id: plan_message_id,
        delete_interaction_message_id: card_context.map(|ctx| ctx.source_message_id),
        send_result_as_new_message: true,
    }
}

pub(super) fn review_completion_cleanup_plan(
    card_context: Option<CardRenderContext>,
) -> super::ui::CompletionCleanupPlan {
    super::ui::CompletionCleanupPlan {
        delete_plan_review_cards: false,
        delete_plan_message_id: None,
        delete_interaction_message_id: card_context.map(|ctx| ctx.source_message_id),
        send_result_as_new_message: true,
    }
}

pub(super) async fn handle_approve(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let Some(plan_approval) = service.find_exit_plan_approval(task_id).await else {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "No pending plan approval found. It may have expired.",
            Some(keyboard::home_only_keyboard()),
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
            let cleanup = approval_completion_cleanup_plan(card_context);
            super::ui::finalize_completion_result(
                bot,
                chat_id,
                task_id,
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
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
        }
    }
    Ok(())
}

pub(super) async fn handle_reject_start(
    bot: &Bot,
    chat_id: ChatId,
    task_id: Uuid,
    dialogue: &BotDialogue,
    _card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
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
                task_id,
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
    task_id: Uuid,
    plan_message_id: Option<MessageId>,
    reason: Option<&str>,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<bool> {
    let Some(plan_approval) = service.find_exit_plan_approval(task_id).await else {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "No pending plan approval found. It may have expired.",
            Some(keyboard::home_only_keyboard()),
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
            let cleanup = reject_completion_cleanup_plan(plan_message_id, card_context);
            super::ui::finalize_completion_result(
                bot,
                chat_id,
                task_id,
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
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
        }
    }
    Ok(false)
}

pub(super) async fn handle_follow_up_reply_start(
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
        "Type your reply here:",
        Some(keyboard::cancel_keyboard()),
    )
    .await?;
    dialogue
        .update(
            crate::services::telegram::state::DialogueState::ReplyingFollowUp {
                task_id,
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
    task_id: Uuid,
    prompt: &str,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<bool> {
    match service.send_follow_up_reply(task_id, prompt).await {
        Ok(()) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                None,
                "✅ Reply sent. The task is running again.",
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(true);
        }
        Err(err) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to send reply: {err}"),
                Some(keyboard::home_only_keyboard()),
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
    task_id: Uuid,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let Some(task) = super::ui::load_task_or_render_error(
        bot,
        chat_id,
        &service.db.pool,
        task_id,
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
            Some(keyboard::home_only_keyboard()),
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
                Some(keyboard::home_only_keyboard()),
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
            Some(keyboard::home_only_keyboard()),
        )
        .await?;
        return Ok(());
    }

    let mut merge_attempted = 0usize;
    let mut merge_succeeded = 0usize;
    let mut merge_failures: Vec<String> = Vec::new();

    // Workspace::fetch_all already returns newest-first, so we take the first as the latest attempt.
    let latest_workspace = match Workspace::fetch_all(&service.db.pool, Some(task.id)).await {
        Ok(workspaces) => workspaces.into_iter().next(),
        Err(e) => {
            merge_failures.push(format!("Failed to load task attempts: {e}"));
            None
        }
    };

    if let Some(workspace) = latest_workspace {
        let base_url = match api_base_url().await {
            Ok(url) => Some(url),
            Err(e) => {
                merge_failures.push(format!("Failed to locate API server: {e}"));
                None
            }
        };

        if let Some(base_url) = base_url {
            let client = reqwest::Client::new();
            match fetch_mergeable_repo_ids(&client, &base_url, workspace.id).await {
                Ok(repo_ids) => {
                    merge_attempted = repo_ids.len();
                    for repo_id in repo_ids {
                        match merge_repo_from_workspace(&client, &base_url, workspace.id, repo_id)
                            .await
                        {
                            Ok(()) => {
                                merge_succeeded = merge_succeeded.saturating_add(1);
                            }
                            Err(err) => {
                                merge_failures
                                    .push(format!("Merge failed for repo {}: {}", repo_id, err));
                            }
                        }
                    }
                }
                Err(err) => {
                    merge_failures.push(format!("Failed to inspect merge status: {err}"));
                }
            }
        }
    }

    let marked_done = if task.status == TaskStatus::Done {
        true
    } else {
        match Task::update_status(&service.db.pool, task.id, TaskStatus::Done).await {
            Ok(_) => true,
            Err(e) => {
                merge_failures.push(format!("Failed to mark task Done: {e}"));
                false
            }
        }
    };

    let summary = if merge_attempted == 0 {
        "No mergeable commits found.".to_string()
    } else if merge_succeeded == merge_attempted {
        format!("Merged {merge_succeeded}/{merge_attempted} repo(s).")
    } else {
        format!("Merged {merge_succeeded}/{merge_attempted} repo(s) with warnings.")
    };

    let status_line = if marked_done {
        "✅ Task status is now Done."
    } else {
        "❌ Failed to set task status to Done."
    };

    let mut message = format!("{summary}\n{status_line}");
    if !merge_failures.is_empty() {
        let details = merge_failures
            .iter()
            .map(|item| format!("- {}", truncate_text(item, 220)))
            .collect::<Vec<_>>()
            .join("\n");
        message.push_str("\n\nWarnings:\n");
        message.push_str(&details);
    }

    super::ui::render_or_send_card(
        bot,
        chat_id,
        card_context,
        message,
        Some(keyboard::home_only_keyboard()),
    )
    .await?;
    Ok(())
}

pub(super) async fn handle_create_review_task(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    match service.create_review_task(task_id).await {
        Ok(result) => {
            tracing::info!("Created review task {}", result.task.id);

            let short_id = ShortIdMapping::get_or_create(&service.db.pool, result.task.id)
                .await
                .unwrap_or_else(|_| "????".to_string());

            let message =
                format_review_task_created_message(&short_id, result.task.has_in_progress_attempt);
            let cleanup = review_completion_cleanup_plan(card_context);
            super::ui::finalize_completion_result(
                bot,
                chat_id,
                task_id,
                cleanup,
                card_context,
                None,
            )
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
                Some(keyboard::review_confirm_keyboard(task_id)),
            )
            .await?;
        }
    }

    Ok(())
}

pub(super) async fn handle_create_review_task_confirm(
    bot: &Bot,
    chat_id: ChatId,
    task_id: Uuid,
) -> ResponseResult<()> {
    format::send_rich_then_plain(
        bot,
        chat_id,
        "Confirm creating a review task?",
        Some(keyboard::review_confirm_keyboard(task_id)),
    )
    .await?;
    Ok(())
}

async fn fetch_mergeable_repo_ids(
    client: &reqwest::Client,
    base_url: &str,
    workspace_id: Uuid,
) -> Result<Vec<Uuid>, String> {
    let response = client
        .get(format!(
            "{base_url}/task-attempts/{workspace_id}/branch-status"
        ))
        .send()
        .await
        .map_err(|e| format!("Branch status request failed: {e}"))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("Failed to read branch status response: {e}"))?;
    let api_response: ApiResponse<Vec<DoneRepoBranchStatus>> = serde_json::from_str(&body)
        .map_err(|e| format!("Failed to parse branch status response ({status}): {e}"))?;

    let error_message = api_response.message().map(String::from);
    let statuses = api_response.into_data().ok_or_else(|| {
        error_message.unwrap_or_else(|| "Branch status request was unsuccessful.".to_string())
    })?;

    Ok(statuses
        .into_iter()
        .filter(|status| status.commits_ahead.unwrap_or(0) > 0)
        .map(|status| status.repo_id)
        .collect())
}

async fn merge_repo_from_workspace(
    client: &reqwest::Client,
    base_url: &str,
    workspace_id: Uuid,
    repo_id: Uuid,
) -> Result<(), String> {
    let response = client
        .post(format!("{base_url}/task-attempts/{workspace_id}/merge"))
        .json(&MergeTaskAttemptBody { repo_id })
        .send()
        .await
        .map_err(|e| format!("Merge request failed: {e}"))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("Failed to read merge response: {e}"))?;
    let api_response: ApiResponse<()> = serde_json::from_str(&body)
        .map_err(|e| format!("Failed to parse merge response ({status}): {e}"))?;

    let error_message = api_response.message().map(String::from);
    match api_response.into_data() {
        Some(_) => Ok(()),
        None => Err(error_message.unwrap_or_else(|| "Merge request failed.".to_string())),
    }
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
            Some(keyboard::home_only_keyboard()),
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
                Some(keyboard::home_only_keyboard()),
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
                Some(keyboard::home_only_keyboard()),
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
                Some(keyboard::home_only_keyboard()),
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
                Some(keyboard::home_only_keyboard()),
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
        format!("➕ New task in {}\n\nEnter the task title:", project.name),
        Some(keyboard::cancel_keyboard()),
    )
    .await?;

    dialogue
        .update(
            crate::services::telegram::state::DialogueState::CreatingTaskTitle {
                project_id,
                project_name: project.name.clone(),
                prompt_message_id: prompt_message_id.0,
            },
        )
        .await
        .ok();
    Ok(())
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
                Some(keyboard::home_only_keyboard()),
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
        approval_requires_structured_input, reject_completion_cleanup_plan,
        review_completion_cleanup_plan,
    };
    use crate::services::approvals::PendingApprovalInfo;

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

        let cleanup = approval_completion_cleanup_plan(context);

        assert!(cleanup.delete_plan_review_cards);
        assert_eq!(cleanup.delete_plan_message_id, None);
        assert_eq!(cleanup.delete_interaction_message_id, Some(MessageId(42)));
        assert!(cleanup.send_result_as_new_message);
    }

    #[test]
    fn reject_cleanup_plan_deletes_plan_and_interaction_messages() {
        let cleanup = reject_completion_cleanup_plan(
            Some(MessageId(7)),
            Some(CardRenderContext {
                source_message_id: MessageId(8),
            }),
        );

        assert!(cleanup.delete_plan_review_cards);
        assert_eq!(cleanup.delete_plan_message_id, Some(MessageId(7)));
        assert_eq!(cleanup.delete_interaction_message_id, Some(MessageId(8)));
        assert!(cleanup.send_result_as_new_message);
    }

    #[test]
    fn review_cleanup_plan_only_deletes_interaction_message() {
        let cleanup = review_completion_cleanup_plan(Some(CardRenderContext {
            source_message_id: MessageId(99),
        }));

        assert!(!cleanup.delete_plan_review_cards);
        assert_eq!(cleanup.delete_plan_message_id, None);
        assert_eq!(cleanup.delete_interaction_message_id, Some(MessageId(99)));
        assert!(cleanup.send_result_as_new_message);
    }
}
