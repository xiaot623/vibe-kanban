use std::sync::Arc;

use db::models::short_id_mapping::ShortIdMapping;
use teloxide::{prelude::*, types::MessageId, utils::command::BotCommands};

use super::{BotDialogue, CardRenderContext, TelegramBotService};
use crate::services::telegram::{callback::CallbackAction, format, keyboard, state::DialogueState};

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Commands:")]
pub enum Command {
    #[command(description = "home screen with buttons")]
    Start,
    #[command(description = "show this help")]
    Help,
    #[command(description = "browse tasks")]
    Tasks,
    #[command(description = "create a new task")]
    New,
    #[command(description = "list pending approvals")]
    Pending,
    #[command(description = "cancel current operation")]
    Cancel,
    #[command(description = "pin a project for quick task creation")]
    Pin,
    #[command(description = "unpin the current project")]
    Unpin,
}

pub(super) async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    service: Arc<TelegramBotService>,
    dialogue: BotDialogue,
    allowed_chat_id: ChatId,
) -> ResponseResult<()> {
    if msg.chat.id != allowed_chat_id {
        tracing::warn!(
            "Ignoring telegram command from unexpected chat: {}",
            msg.chat.id.0
        );
        return Ok(());
    }

    // Lazily clean up expired short ID mappings
    ShortIdMapping::cleanup_expired(&service.db.pool).await;

    match cmd {
        Command::Start => {
            let pin = service.resolve_pin().await;
            let text = if let Some(ref pin) = pin {
                format!(
                    "Welcome to Vibe Kanban! 📍 Pinned: {}\nChoose an action:",
                    pin.project_name
                )
            } else {
                "Welcome to Vibe Kanban! Choose an action:".to_string()
            };
            format::send_rich_then_plain(
                &bot,
                msg.chat.id,
                &text,
                Some(keyboard::home_keyboard(pin.is_some())),
            )
            .await?;
        }
        Command::Help => {
            let help = format!(
                "🏠 **Quick actions** — use the buttons from /start\n\n{}",
                Command::descriptions()
            );
            format::send_rich_then_plain(&bot, msg.chat.id, &help, None).await?;
        }
        Command::Tasks => {
            super::screens::show_projects_for_browsing(&bot, msg.chat.id, &service, None).await?;
        }
        Command::New => {
            super::screens::show_new_task_or_pinned(&bot, msg.chat.id, &service, &dialogue, None)
                .await?;
        }
        Command::Pending => {
            super::screens::show_pending_approvals(&bot, msg.chat.id, &service, None).await?;
        }
        Command::Cancel => {
            if let Ok(Some(state)) = dialogue.get().await
                && let Some(prompt_message_id) = state.prompt_message_id()
            {
                super::ui::delete_message_best_effort(
                    &bot,
                    msg.chat.id,
                    MessageId(prompt_message_id),
                )
                .await;
            }
            dialogue.reset().await.ok();
            let pin = service.resolve_pin().await;
            format::send_rich_then_plain(
                &bot,
                msg.chat.id,
                "Cancelled.",
                Some(keyboard::home_keyboard(pin.is_some())),
            )
            .await?;
        }
        Command::Pin => {
            super::screens::show_projects_for_pinning(&bot, msg.chat.id, &service, None).await?;
        }
        Command::Unpin => {
            super::screens::handle_unpin(&bot, msg.chat.id, &service, None).await?;
        }
    }

    Ok(())
}

pub(super) async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    service: Arc<TelegramBotService>,
    dialogue: BotDialogue,
    allowed_chat_id: ChatId,
) -> ResponseResult<()> {
    // Always answer the callback to dismiss the spinner
    bot.answer_callback_query(&q.id).await?;

    let chat_id = match q.message.as_ref().map(|m| m.chat().id) {
        Some(id) if id == allowed_chat_id => id,
        Some(id) => {
            tracing::warn!("Ignoring callback from unexpected chat: {}", id.0);
            return Ok(());
        }
        None => return Ok(()),
    };
    let card_context = CardRenderContext::from_callback(&q);

    let data = match q.data.as_deref() {
        Some(d) => d,
        None => return Ok(()),
    };

    let action = match CallbackAction::decode(data) {
        Some(a) => a,
        None => {
            super::ui::render_or_send_card(
                &bot,
                chat_id,
                card_context,
                "Invalid action. Please try again.",
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
            return Ok(());
        }
    };

    // Handle actions that complete immediately.
    if handle_immediate_callback_action(
        &bot,
        &q,
        &action,
        chat_id,
        &service,
        &dialogue,
        card_context,
    )
    .await?
    {
        return Ok(());
    }

    ShortIdMapping::cleanup_expired(&service.db.pool).await;

    match action {
        CallbackAction::Home => {
            dialogue.reset().await.ok();
            let pin = service.resolve_pin().await;
            let text = if let Some(ref pin) = pin {
                format!("Choose an action: 📍 {}", pin.project_name)
            } else {
                "Choose an action:".to_string()
            };
            super::ui::render_or_send_card(
                &bot,
                chat_id,
                card_context,
                text,
                Some(keyboard::home_keyboard(pin.is_some())),
            )
            .await?;
        }
        CallbackAction::Projects => {
            super::screens::show_projects_for_browsing(&bot, chat_id, &service, card_context)
                .await?;
        }
        CallbackAction::NewTask => {
            super::screens::show_new_task_or_pinned(
                &bot,
                chat_id,
                &service,
                &dialogue,
                card_context,
            )
            .await?;
        }
        CallbackAction::Pending => {
            super::screens::show_pending_approvals(&bot, chat_id, &service, card_context).await?;
        }
        CallbackAction::Tasks { project_id, status } => {
            super::screens::show_task_list(
                &bot,
                chat_id,
                &service,
                project_id,
                status.as_deref(),
                0,
                card_context,
            )
            .await?;
        }
        CallbackAction::TaskPage { project_id, page } => {
            super::screens::show_task_list(
                &bot,
                chat_id,
                &service,
                project_id,
                None,
                page,
                card_context,
            )
            .await?;
        }
        CallbackAction::TaskDetail { task_id } => {
            super::screens::show_task_detail(&bot, chat_id, &service, task_id, card_context)
                .await?;
        }
        CallbackAction::RunDefault { task_id } => {
            super::actions::handle_run_default(&bot, chat_id, &service, task_id, card_context)
                .await?;
        }
        CallbackAction::RunPick { task_id } => {
            super::ui::render_or_send_card(
                &bot,
                chat_id,
                card_context,
                "Select an executor:",
                Some(keyboard::executor_pick_keyboard(task_id)),
            )
            .await?;
        }
        CallbackAction::RunWith { task_id, executor } => {
            let run_modes = super::actions::available_run_modes_for_executor(&executor);
            super::ui::render_or_send_card(
                &bot,
                chat_id,
                card_context,
                format!("Select run mode for {executor}:"),
                Some(keyboard::run_mode_pick_keyboard(
                    task_id, &executor, &run_modes,
                )),
            )
            .await?;
        }
        CallbackAction::RunWithMode {
            task_id,
            executor,
            mode_index,
        } => {
            super::actions::handle_run_with_executor_mode(
                &bot,
                chat_id,
                &service,
                task_id,
                &executor,
                mode_index,
                card_context,
            )
            .await?;
        }
        CallbackAction::EditTask { task_id } => {
            super::actions::handle_edit_start(
                &bot,
                chat_id,
                &service,
                task_id,
                &dialogue,
                card_context,
            )
            .await?;
        }
        CallbackAction::ApproveConfirm { task_id } => {
            super::actions::handle_approve_confirm(&bot, chat_id, task_id).await?;
        }
        CallbackAction::LegacyFlowApproveConfirmTask { task_id } => {
            super::actions::handle_approve_confirm(&bot, chat_id, task_id).await?;
        }
        CallbackAction::FlowApproveConfirm { flow_token } => {
            super::actions::handle_flow_approve_confirm(&bot, chat_id, &flow_token).await?;
        }
        CallbackAction::ApproveYes { task_id } => {
            super::actions::handle_approve(
                &bot,
                chat_id,
                &service,
                &format!("legacy-task:{task_id}"),
                card_context,
            )
            .await?;
        }
        CallbackAction::LegacyFlowApproveYesTask { task_id } => {
            super::actions::handle_approve(
                &bot,
                chat_id,
                &service,
                &format!("legacy-task:{task_id}"),
                card_context,
            )
            .await?;
        }
        CallbackAction::FlowApproveYes { flow_token } => {
            super::actions::handle_approve(&bot, chat_id, &service, &flow_token, card_context)
                .await?;
        }
        CallbackAction::RejectInput { task_id } => {
            super::actions::handle_reject_start(
                &bot,
                chat_id,
                &format!("legacy-task:{task_id}"),
                &dialogue,
                card_context,
            )
            .await?;
        }
        CallbackAction::FlowRejectInput { flow_token } => {
            super::actions::handle_reject_start(
                &bot,
                chat_id,
                &flow_token,
                &dialogue,
                card_context,
            )
            .await?;
        }
        CallbackAction::LegacyFlowRejectInputTask { task_id } => {
            super::actions::handle_reject_start(
                &bot,
                chat_id,
                &format!("legacy-task:{task_id}"),
                &dialogue,
                card_context,
            )
            .await?;
        }
        CallbackAction::FollowUpReply { flow_token } => {
            super::actions::handle_follow_up_reply_start(
                &bot,
                chat_id,
                &service,
                &flow_token,
                &dialogue,
                card_context,
            )
            .await?;
        }
        CallbackAction::LegacyFollowUpReplyTask { task_id } => {
            super::actions::handle_follow_up_reply_start_legacy_task(
                &bot,
                chat_id,
                task_id,
                &dialogue,
                card_context,
            )
            .await?;
        }
        CallbackAction::CreateReviewTask { flow_token } => {
            super::actions::handle_create_review_task_confirm(&bot, chat_id, &flow_token).await?;
        }
        CallbackAction::LegacyCreateReviewTask { .. }
        | CallbackAction::LegacyCreateReviewTaskConfirm { .. }
        | CallbackAction::LegacyDoneTask { .. } => {
            super::ui::render_or_send_card(
                &bot,
                chat_id,
                card_context,
                "This older card is not flow-aware anymore; use a newer summary/run card.",
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
        CallbackAction::CreateReviewTaskConfirm { flow_token } => {
            super::actions::handle_create_review_task(
                &bot,
                chat_id,
                &service,
                &flow_token,
                card_context,
            )
            .await?;
        }
        CallbackAction::DoneTask { flow_token } => {
            super::actions::handle_done_task(&bot, chat_id, &service, &flow_token, card_context)
                .await?;
        }
        CallbackAction::ToolApprove { approval_id } => {
            super::actions::handle_tool_approval_callback(
                &bot,
                chat_id,
                &service,
                &approval_id,
                true,
                card_context,
            )
            .await?;
        }
        CallbackAction::ToolReject { approval_id } => {
            super::actions::handle_tool_approval_callback(
                &bot,
                chat_id,
                &service,
                &approval_id,
                false,
                card_context,
            )
            .await?;
        }
        CallbackAction::Refresh { task_id } => {
            super::screens::show_task_detail(&bot, chat_id, &service, task_id, card_context)
                .await?;
        }
        CallbackAction::NewTaskProject { project_id } => {
            super::actions::handle_new_task_project(
                &bot,
                chat_id,
                &service,
                project_id,
                &dialogue,
                card_context,
            )
            .await?;
        }
        CallbackAction::PinMenu => {
            super::screens::show_projects_for_pinning(&bot, chat_id, &service, card_context)
                .await?;
        }
        CallbackAction::Unpin => {
            super::screens::handle_unpin(&bot, chat_id, &service, card_context).await?;
        }
        CallbackAction::PinProject { project_id } => {
            super::actions::handle_pin_project_select(
                &bot,
                chat_id,
                &service,
                project_id,
                card_context,
            )
            .await?;
        }
        CallbackAction::PinDuration {
            project_id,
            minutes,
        } => {
            super::actions::handle_pin_duration_select(
                &bot,
                chat_id,
                &service,
                project_id,
                minutes,
                card_context,
            )
            .await?;
        }
        CallbackAction::DismissInteraction
        | CallbackAction::Cancel
        | CallbackAction::Skip
        | CallbackAction::Noop => {
            // Already handled above
        }
    }

    Ok(())
}

async fn handle_immediate_callback_action(
    bot: &Bot,
    q: &CallbackQuery,
    action: &CallbackAction,
    chat_id: ChatId,
    service: &TelegramBotService,
    dialogue: &BotDialogue,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<bool> {
    match action {
        CallbackAction::DismissInteraction => {
            let current_message_id = q.message.as_ref().map(|message| message.id());
            let should_reset = should_reset_dialogue_on_dismiss(
                dialogue.get().await.ok().flatten(),
                current_message_id,
            );

            if should_reset {
                dialogue.reset().await.ok();
            }

            if let Some(current_message_id) = current_message_id {
                super::ui::delete_message_best_effort(bot, chat_id, current_message_id).await;
            }

            Ok(true)
        }
        CallbackAction::Cancel => {
            let prompt_message_id = dialogue
                .get()
                .await
                .ok()
                .flatten()
                .and_then(|state| state.prompt_message_id());
            dialogue.reset().await.ok();
            let pin = service.resolve_pin().await;
            if let Some(prompt_message_id) = prompt_message_id {
                super::ui::delete_message_best_effort(bot, chat_id, MessageId(prompt_message_id))
                    .await;
                super::ui::render_or_send_card(
                    bot,
                    chat_id,
                    None,
                    "Cancelled.",
                    Some(keyboard::home_keyboard(pin.is_some())),
                )
                .await?;
            } else {
                super::ui::render_or_send_card(
                    bot,
                    chat_id,
                    card_context,
                    "Cancelled.",
                    Some(keyboard::home_keyboard(pin.is_some())),
                )
                .await?;
            }
            Ok(true)
        }
        CallbackAction::Skip => {
            let state = match dialogue.get().await {
                Ok(Some(state)) => state,
                _ => return Ok(true),
            };

            match state {
                DialogueState::EditingTaskDescription {
                    task_id,
                    title,
                    current_description,
                    prompt_message_id,
                } => {
                    let success = super::actions::handle_edit_task_finish(
                        bot,
                        chat_id,
                        service,
                        task_id,
                        &title,
                        current_description.as_deref(),
                        card_context,
                    )
                    .await?;
                    super::ui::complete_dialogue_step(
                        bot,
                        chat_id,
                        dialogue,
                        MessageId(prompt_message_id),
                        success,
                        super::ui::CompletionFailurePolicy::Keep,
                    )
                    .await;
                    // On failure leave dialogue state intact so the user can retry or /cancel.
                }
                _ => {}
            }
            Ok(true)
        }
        CallbackAction::Noop => Ok(true),
        _ => Ok(false),
    }
}

fn should_reset_dialogue_on_dismiss(
    state: Option<DialogueState>,
    current_message_id: Option<MessageId>,
) -> bool {
    match (state, current_message_id) {
        (Some(state), Some(current_message_id)) => {
            state.prompt_message_id() == Some(current_message_id.0)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use teloxide::{types::MessageId, utils::command::BotCommands};
    use uuid::Uuid;

    use super::{Command, should_reset_dialogue_on_dismiss};
    use crate::services::telegram::state::DialogueState;

    #[test]
    fn interactive_commands_parse_supported_slash_commands() {
        let bot_name = "vibe_kanban_bot";

        assert!(matches!(
            Command::parse("/start", bot_name),
            Ok(Command::Start)
        ));
        assert!(matches!(
            Command::parse("/help", bot_name),
            Ok(Command::Help)
        ));
        assert!(matches!(
            Command::parse("/tasks", bot_name),
            Ok(Command::Tasks)
        ));
        assert!(matches!(Command::parse("/new", bot_name), Ok(Command::New)));
        assert!(matches!(
            Command::parse("/pending", bot_name),
            Ok(Command::Pending)
        ));
        assert!(matches!(
            Command::parse("/cancel", bot_name),
            Ok(Command::Cancel)
        ));
        assert!(matches!(Command::parse("/pin", bot_name), Ok(Command::Pin)));
        assert!(matches!(
            Command::parse("/unpin", bot_name),
            Ok(Command::Unpin)
        ));
    }

    #[test]
    fn interactive_commands_publish_supported_slash_commands_in_expected_order() {
        let commands = Command::bot_commands();
        let names = commands
            .iter()
            .map(|command| command.command.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "/start", "/help", "/tasks", "/new", "/pending", "/cancel", "/pin", "/unpin"
            ]
        );
    }

    #[test]
    fn interactive_commands_reject_legacy_slash_commands() {
        let bot_name = "vibe_kanban_bot";
        for legacy in [
            "/project",
            "/list demo",
            "/task 1234",
            "/add demo",
            "/edit task1234",
            "/run task1234",
            "/approve task1234",
            "/reject task1234",
        ] {
            assert!(
                Command::parse(legacy, bot_name).is_err(),
                "{legacy} should fail"
            );
        }
    }

    #[test]
    fn dismiss_resets_only_when_dismissing_active_prompt_message() {
        let task_id = Uuid::new_v4();
        let state = Some(DialogueState::ReplyingFollowUp {
            flow_token: format!("f-{}", task_id.simple()),
            session_id: task_id,
            prompt_message_id: 33,
        });

        assert!(should_reset_dialogue_on_dismiss(
            state.clone(),
            Some(MessageId(33))
        ));
        assert!(!should_reset_dialogue_on_dismiss(
            state,
            Some(MessageId(99))
        ));
        assert!(!should_reset_dialogue_on_dismiss(None, Some(MessageId(33))));
        assert!(!should_reset_dialogue_on_dismiss(
            Some(DialogueState::Idle),
            Some(MessageId(33))
        ));
    }
}
