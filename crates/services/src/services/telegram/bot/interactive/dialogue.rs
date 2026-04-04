use std::sync::Arc;

use db::models::task::Task;
use teloxide::{prelude::*, types::MessageId};

use super::{BotDialogue, CardRenderContext, TelegramBotService};
use crate::services::telegram::{
    bot::shared::parse_message_as_task, format, keyboard, state::DialogueState,
};

const CREATE_TASK_MESSAGE_PARSE_ERROR: &str = "Could not parse task from message.\n\nPlease send one message:\n- First line: title\n- Remaining lines: detail\n- Single line: empty detail";

fn parse_interactive_new_task_message(message: &str) -> Option<(String, Option<String>)> {
    parse_message_as_task(message)
}

fn retry_creating_task_message_state(
    project_id: uuid::Uuid,
    prompt_message_id: i32,
) -> DialogueState {
    DialogueState::CreatingTaskMessage {
        project_id,
        prompt_message_id,
    }
}

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
            if text.is_empty() {
                return Ok(());
            }
            if let Some(flow_token) = super::ui::extract_stage_summary_flow_token_from_reply(&msg) {
                let Some(flow_ctx) = crate::services::telegram::flow::resolve_flow_context(
                    &service.db.pool,
                    &flow_token,
                )
                .await
                .map_err(|e| teloxide::RequestError::Io(std::io::Error::other(e.to_string())))?
                else {
                    format::send_rich_then_plain(
                        &bot,
                        msg.chat.id,
                        super::actions::unavailable_flow_message(),
                        Some(super::ui::empty_inline_keyboard()),
                    )
                    .await?;
                    return Ok(());
                };

                super::actions::handle_follow_up_reply_finish(
                    &bot,
                    msg.chat.id,
                    &service,
                    &flow_token,
                    flow_ctx.session_id,
                    text,
                    None,
                )
                .await?;
            } else if let Err(err) = service.create_daily_task_from_message(text).await {
                format::send_rich_then_plain(
                    &bot,
                    msg.chat.id,
                    &err,
                    Some(super::ui::empty_inline_keyboard()),
                )
                .await?;
            }
        }
        DialogueState::CreatingTaskMessage {
            project_id,
            prompt_message_id,
            ..
        } => {
            let Some((title, description)) = parse_interactive_new_task_message(text) else {
                let updated_prompt_id = super::ui::render_or_send_card(
                    &bot,
                    msg.chat.id,
                    Some(CardRenderContext {
                        source_message_id: MessageId(prompt_message_id),
                    }),
                    CREATE_TASK_MESSAGE_PARSE_ERROR,
                    Some(keyboard::cancel_keyboard()),
                )
                .await?;
                dialogue
                    .update(retry_creating_task_message_state(
                        project_id,
                        updated_prompt_id.0,
                    ))
                    .await
                    .ok();
                return Ok(());
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
            flow_token,
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
                &flow_token,
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
            flow_token,
            session_id,
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
                        flow_token,
                        session_id,
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
                &flow_token,
                session_id,
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

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{
        CREATE_TASK_MESSAGE_PARSE_ERROR, parse_interactive_new_task_message,
        retry_creating_task_message_state,
    };
    use crate::services::telegram::state::DialogueState;

    #[test]
    fn interactive_create_parse_single_line_sets_empty_description() {
        let (title, description) = parse_interactive_new_task_message("Quick follow-up").unwrap();

        assert_eq!(title, "Quick follow-up");
        assert_eq!(description, None);
    }

    #[test]
    fn interactive_create_parse_multiline_uses_rest_as_description() {
        let (title, description) =
            parse_interactive_new_task_message("Finish report\nInclude metrics\nand summary")
                .unwrap();

        assert_eq!(title, "Finish report");
        assert_eq!(description.as_deref(), Some("Include metrics\nand summary"));
    }

    #[test]
    fn interactive_create_parse_blank_message_fails() {
        assert!(parse_interactive_new_task_message("   \n  ").is_none());
    }

    #[test]
    fn interactive_create_parse_failure_keeps_single_step_state() {
        let project_id = Uuid::new_v4();
        let state = retry_creating_task_message_state(project_id, 77);

        assert!(matches!(
            state,
            DialogueState::CreatingTaskMessage {
                project_id: id,
                prompt_message_id: 77,
            } if id == project_id
        ));
    }

    #[test]
    fn interactive_create_parse_failure_message_mentions_single_message_rules() {
        assert!(CREATE_TASK_MESSAGE_PARSE_ERROR.contains("First line: title"));
        assert!(CREATE_TASK_MESSAGE_PARSE_ERROR.contains("Remaining lines: detail"));
        assert!(CREATE_TASK_MESSAGE_PARSE_ERROR.contains("Single line: empty detail"));
    }
}
