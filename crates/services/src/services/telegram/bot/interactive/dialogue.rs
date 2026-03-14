use std::sync::Arc;

use db::models::task::Task;
use teloxide::{prelude::*, types::MessageId};

use super::{BotDialogue, CardRenderContext, TelegramBotService};
use crate::services::telegram::{format, keyboard, state::DialogueState};

pub(super) async fn handle_dialogue_text(
    bot: Bot,
    msg: Message,
    service: Arc<TelegramBotService>,
    dialogue: BotDialogue,
    allowed_chat_id: ChatId,
) -> ResponseResult<()> {
    if msg.chat.id != allowed_chat_id {
        return Ok(());
    }

    let text = msg.text().unwrap_or("").trim();
    if text.is_empty() {
        return Ok(());
    }

    // Unknown slash commands should not auto-create tasks.
    if text.starts_with('/') {
        return Ok(());
    }

    let state = match dialogue.get().await {
        Ok(Some(state)) => state,
        _ => DialogueState::Idle,
    };

    match state {
        DialogueState::Idle => {
            if let Some(task_id) = super::ui::extract_stage_summary_task_id_from_reply(&msg) {
                super::actions::handle_follow_up_reply_finish(
                    &bot,
                    msg.chat.id,
                    &service,
                    task_id,
                    text,
                    None,
                )
                .await?;
            } else if let Err(err) = service.create_daily_task_from_message(text).await {
                format::send_rich_then_plain(
                    &bot,
                    msg.chat.id,
                    &err,
                    Some(keyboard::home_only_keyboard()),
                )
                .await?;
            }
        }
        DialogueState::CreatingTaskTitle {
            project_id,
            project_name,
            prompt_message_id,
        } => {
            if text.is_empty() {
                let updated_prompt_id = super::ui::render_or_send_card(
                    &bot,
                    msg.chat.id,
                    Some(CardRenderContext {
                        source_message_id: MessageId(prompt_message_id),
                    }),
                    "Title cannot be empty. Please enter a title:",
                    Some(keyboard::cancel_keyboard()),
                )
                .await?;
                dialogue
                    .update(DialogueState::CreatingTaskTitle {
                        project_id,
                        project_name,
                        prompt_message_id: updated_prompt_id.0,
                    })
                    .await
                    .ok();
                return Ok(());
            }
            let updated_prompt_id = super::ui::render_or_send_card(
                &bot,
                msg.chat.id,
                Some(CardRenderContext {
                    source_message_id: MessageId(prompt_message_id),
                }),
                format!(
                    "Title: {}\n\nNow enter a description (or press Skip):",
                    text
                ),
                Some(keyboard::skip_cancel_keyboard()),
            )
            .await?;
            dialogue
                .update(DialogueState::CreatingTaskDescription {
                    project_id,
                    project_name: project_name.clone(),
                    title: text.to_string(),
                    prompt_message_id: updated_prompt_id.0,
                })
                .await
                .ok();
        }
        DialogueState::CreatingTaskDescription {
            project_id,
            project_name: _,
            title,
            prompt_message_id,
        } => {
            let description = if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            };
            let success = super::actions::handle_create_task_finish(
                &bot,
                msg.chat.id,
                &service,
                project_id,
                &title,
                description.as_deref(),
                Some(CardRenderContext {
                    source_message_id: MessageId(prompt_message_id),
                }),
            )
            .await?;

            super::ui::complete_dialogue_step(
                &bot,
                msg.chat.id,
                &dialogue,
                MessageId(prompt_message_id),
                success,
                super::ui::CompletionFailurePolicy::Reset,
            )
            .await;
        }
        DialogueState::EditingTaskTitle {
            task_id,
            current_title,
            prompt_message_id,
        } => {
            if text.is_empty() {
                let updated_prompt_id = super::ui::render_or_send_card(
                    &bot,
                    msg.chat.id,
                    Some(CardRenderContext {
                        source_message_id: MessageId(prompt_message_id),
                    }),
                    "Title cannot be empty. Please enter a new title:",
                    Some(keyboard::cancel_keyboard()),
                )
                .await?;
                dialogue
                    .update(DialogueState::EditingTaskTitle {
                        task_id,
                        current_title,
                        prompt_message_id: updated_prompt_id.0,
                    })
                    .await
                    .ok();
                return Ok(());
            }
            // Load current description for next step
            let current_description = match Task::find_by_id(&service.db.pool, task_id).await {
                Ok(Some(task)) => task.description,
                _ => None,
            };
            let updated_prompt_id = super::ui::render_or_send_card(
                &bot,
                msg.chat.id,
                Some(CardRenderContext {
                    source_message_id: MessageId(prompt_message_id),
                }),
                format!(
                    "New title: {}\n\nNow enter a new description (or press Skip to keep current):",
                    text
                ),
                Some(keyboard::skip_cancel_keyboard()),
            )
            .await?;
            dialogue
                .update(DialogueState::EditingTaskDescription {
                    task_id,
                    title: text.to_string(),
                    current_description,
                    prompt_message_id: updated_prompt_id.0,
                })
                .await
                .ok();
        }
        DialogueState::EditingTaskDescription {
            task_id,
            title,
            current_description,
            prompt_message_id,
        } => {
            let description = if text.is_empty() {
                current_description
            } else {
                Some(text.to_string())
            };
            let success = super::actions::handle_edit_task_finish(
                &bot,
                msg.chat.id,
                &service,
                task_id,
                &title,
                description.as_deref(),
                Some(CardRenderContext {
                    source_message_id: MessageId(prompt_message_id),
                }),
            )
            .await?;

            super::ui::complete_dialogue_step(
                &bot,
                msg.chat.id,
                &dialogue,
                MessageId(prompt_message_id),
                success,
                super::ui::CompletionFailurePolicy::Reset,
            )
            .await;
        }
        DialogueState::RejectingPlan {
            task_id,
            prompt_message_id,
        } => {
            let reason = if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            };
            let success = super::actions::handle_reject_finish(
                &bot,
                msg.chat.id,
                &service,
                task_id,
                msg.reply_to_message().map(|reply| reply.id),
                reason.as_deref(),
                Some(CardRenderContext {
                    source_message_id: MessageId(prompt_message_id),
                }),
            )
            .await?;

            super::ui::complete_dialogue_step(
                &bot,
                msg.chat.id,
                &dialogue,
                MessageId(prompt_message_id),
                success,
                super::ui::CompletionFailurePolicy::Reset,
            )
            .await;
        }
        DialogueState::ReplyingFollowUp {
            task_id,
            prompt_message_id,
        } => {
            if text.is_empty() {
                let updated_prompt_id = super::ui::render_or_send_card(
                    &bot,
                    msg.chat.id,
                    Some(CardRenderContext {
                        source_message_id: MessageId(prompt_message_id),
                    }),
                    "Reply cannot be empty. Please enter follow-up text:",
                    Some(keyboard::cancel_keyboard()),
                )
                .await?;
                dialogue
                    .update(DialogueState::ReplyingFollowUp {
                        task_id,
                        prompt_message_id: updated_prompt_id.0,
                    })
                    .await
                    .ok();
                return Ok(());
            }

            let success = super::actions::handle_follow_up_reply_finish(
                &bot,
                msg.chat.id,
                &service,
                task_id,
                text,
                Some(CardRenderContext {
                    source_message_id: MessageId(prompt_message_id),
                }),
            )
            .await?;

            super::ui::complete_dialogue_step(
                &bot,
                msg.chat.id,
                &dialogue,
                MessageId(prompt_message_id),
                success,
                super::ui::CompletionFailurePolicy::Reset,
            )
            .await;
        }
    }

    Ok(())
}
