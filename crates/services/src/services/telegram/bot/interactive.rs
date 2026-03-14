use std::{str::FromStr, sync::Arc, time::Duration};

use db::models::{
    execution_process::ExecutionProcess,
    project::Project,
    short_id_mapping::ShortIdMapping,
    task::{CreateTask, Task, TaskStatus, UpdateTask},
    workspace::Workspace,
};
use executors::{executors::BaseCodingAgent, profile::ExecutorConfigs};
use serde::{Deserialize, Serialize};
use teloxide::{
    dispatching::{Dispatcher, UpdateFilterExt, dialogue::InMemStorage},
    dptree,
    error_handlers::LoggingErrorHandler,
    prelude::*,
    types::{InlineKeyboardButtonKind, InlineKeyboardMarkup, MessageId},
    update_listeners::Polling,
    utils::command::BotCommands,
};
use utils::{
    approvals::{ApprovalResponse, ApprovalStatus},
    response::ApiResponse,
};
use uuid::Uuid;

use super::{
    TelegramBotService,
    shared::{api_base_url, format_review_task_created_message, parse_task_status, truncate_text},
};
use crate::services::{
    approvals::{ApprovalError, PendingApprovalInfo},
    telegram::{
        EXIT_PLAN_MODE_NAME, callback::CallbackAction, format, keyboard, notifier,
        state::DialogueState,
    },
};

/// Type alias for the dialogue handle used in handlers.
type BotDialogue =
    teloxide::dispatching::dialogue::Dialogue<DialogueState, InMemStorage<DialogueState>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CardRenderContext {
    source_message_id: MessageId,
}

impl CardRenderContext {
    fn from_callback(query: &CallbackQuery) -> Option<Self> {
        let message = query.message.as_ref()?;
        Some(Self {
            source_message_id: message.id(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct DoneRepoBranchStatus {
    repo_id: Uuid,
    commits_ahead: Option<usize>,
}

#[derive(Debug, Serialize)]
struct MergeTaskAttemptBody {
    repo_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompletionCleanupPlan {
    delete_plan_review_cards: bool,
    delete_plan_message_id: Option<MessageId>,
    delete_interaction_message_id: Option<MessageId>,
    send_result_as_new_message: bool,
}

// ─── Commands ────────────────────────────────────────────────────────

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
}

pub(super) async fn run_dispatcher(service: TelegramBotService, bot: Bot, chat_id: ChatId) {
    let storage: Arc<InMemStorage<DialogueState>> = InMemStorage::new();

    // Build the update handler tree
    let handler = dptree::entry()
        // Inject dialogue state for all branches
        .enter_dialogue::<Update, InMemStorage<DialogueState>, DialogueState>()
        // Branch 1: callback queries (button presses)
        .branch(Update::filter_callback_query().endpoint(handle_callback))
        // Branch 2: messages with commands
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(handle_command),
        )
        // Branch 3: plain text messages (dialogue input)
        .branch(Update::filter_message().endpoint(handle_dialogue_text));

    let service = Arc::new(service);

    let deps = dptree::deps![storage, service, chat_id];

    if let Err(err) = bot.delete_webhook().send().await {
        tracing::warn!(
            "Failed to delete Telegram webhook before polling startup: {:?}",
            err
        );
    }

    if let Err(err) = bot.set_my_commands(Command::bot_commands()).send().await {
        tracing::warn!("Failed to register Telegram slash commands: {:?}", err);
    }

    let listener = Polling::builder(bot.clone())
        .timeout(Duration::from_secs(10))
        .build();

    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(deps)
        .default_handler(|upd: Arc<Update>| async move {
            tracing::trace!("Unhandled update: {:?}", upd.id);
        })
        .build();

    dispatcher
        .dispatch_with_listener(
            listener,
            LoggingErrorHandler::with_custom_text("An error from the update listener"),
        )
        .await;
}

// ─── Command handler ─────────────────────────────────────────────────

async fn handle_command(
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
            let text = "Welcome to Vibe Kanban! Choose an action:";
            format::send_rich_then_plain(&bot, msg.chat.id, text, Some(keyboard::home_keyboard()))
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
            show_projects_for_browsing(&bot, msg.chat.id, &service, None).await?;
        }
        Command::New => {
            show_projects_for_new_task(&bot, msg.chat.id, &service, None).await?;
        }
        Command::Pending => {
            show_pending_approvals(&bot, msg.chat.id, &service, None).await?;
        }
        Command::Cancel => {
            if let Ok(Some(state)) = dialogue.get().await
                && let Some(prompt_message_id) = state.prompt_message_id()
            {
                delete_message_best_effort(&bot, msg.chat.id, MessageId(prompt_message_id)).await;
            }
            dialogue.reset().await.ok();
            format::send_rich_then_plain(
                &bot,
                msg.chat.id,
                "Cancelled.",
                Some(keyboard::home_keyboard()),
            )
            .await?;
        }
    }

    Ok(())
}

// ─── Callback handler ────────────────────────────────────────────────

async fn handle_callback(
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
            render_or_send_card(
                &bot,
                chat_id,
                card_context,
                "Invalid action. Please try again.",
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(());
        }
    };

    // Handle actions that complete immediately.
    match &action {
        CallbackAction::DismissInteraction => {
            let current_message_id = q.message.as_ref().map(|message| message.id());
            let should_reset = match (dialogue.get().await.ok().flatten(), current_message_id) {
                (Some(state), Some(current_message_id)) => {
                    state.prompt_message_id() == Some(current_message_id.0)
                }
                _ => false,
            };

            if should_reset {
                dialogue.reset().await.ok();
            }

            if let Some(current_message_id) = current_message_id {
                delete_message_best_effort(&bot, chat_id, current_message_id).await;
            }

            return Ok(());
        }
        CallbackAction::Cancel => {
            let prompt_message_id = dialogue
                .get()
                .await
                .ok()
                .flatten()
                .and_then(|state| state.prompt_message_id());
            dialogue.reset().await.ok();
            if let Some(prompt_message_id) = prompt_message_id {
                delete_message_best_effort(&bot, chat_id, MessageId(prompt_message_id)).await;
                render_or_send_card(
                    &bot,
                    chat_id,
                    None,
                    "Cancelled.",
                    Some(keyboard::home_keyboard()),
                )
                .await?;
            } else {
                render_or_send_card(
                    &bot,
                    chat_id,
                    card_context,
                    "Cancelled.",
                    Some(keyboard::home_keyboard()),
                )
                .await?;
            }
            return Ok(());
        }
        CallbackAction::Skip => {
            let state = match dialogue.get().await {
                Ok(Some(state)) => state,
                _ => return Ok(()),
            };

            match state {
                DialogueState::CreatingTaskDescription {
                    project_id,
                    project_name: _,
                    title,
                    prompt_message_id,
                } => {
                    let success = handle_create_task_finish(
                        &bot,
                        chat_id,
                        &service,
                        project_id,
                        &title,
                        None,
                        card_context,
                    )
                    .await?;
                    if success {
                        dialogue.reset().await.ok();
                        delete_message_best_effort(&bot, chat_id, MessageId(prompt_message_id))
                            .await;
                    }
                    // On failure leave dialogue state intact so the user can retry or /cancel.
                }
                DialogueState::EditingTaskDescription {
                    task_id,
                    title,
                    current_description,
                    prompt_message_id,
                } => {
                    let success = handle_edit_task_finish(
                        &bot,
                        chat_id,
                        &service,
                        task_id,
                        &title,
                        current_description.as_deref(),
                        card_context,
                    )
                    .await?;
                    if success {
                        dialogue.reset().await.ok();
                        delete_message_best_effort(&bot, chat_id, MessageId(prompt_message_id))
                            .await;
                    }
                    // On failure leave dialogue state intact so the user can retry or /cancel.
                }
                _ => {}
            }
            return Ok(());
        }
        CallbackAction::Noop => {
            return Ok(());
        }
        _ => {}
    }

    ShortIdMapping::cleanup_expired(&service.db.pool).await;

    match action {
        CallbackAction::Home => {
            dialogue.reset().await.ok();
            render_or_send_card(
                &bot,
                chat_id,
                card_context,
                "Choose an action:",
                Some(keyboard::home_keyboard()),
            )
            .await?;
        }
        CallbackAction::Projects => {
            show_projects_for_browsing(&bot, chat_id, &service, card_context).await?;
        }
        CallbackAction::NewTask => {
            show_projects_for_new_task(&bot, chat_id, &service, card_context).await?;
        }
        CallbackAction::Pending => {
            show_pending_approvals(&bot, chat_id, &service, card_context).await?;
        }
        CallbackAction::Tasks { project_id, status } => {
            show_task_list(
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
            show_task_list(
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
            show_task_detail(&bot, chat_id, &service, task_id, card_context).await?;
        }
        CallbackAction::RunDefault { task_id } => {
            handle_run_default(&bot, chat_id, &service, task_id, card_context).await?;
        }
        CallbackAction::RunPick { task_id } => {
            render_or_send_card(
                &bot,
                chat_id,
                card_context,
                "Select an executor:",
                Some(keyboard::executor_pick_keyboard(task_id)),
            )
            .await?;
        }
        CallbackAction::RunWith { task_id, executor } => {
            let run_modes = available_run_modes_for_executor(&executor);
            render_or_send_card(
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
            handle_run_with_executor_mode(
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
            handle_edit_start(&bot, chat_id, &service, task_id, &dialogue, card_context).await?;
        }
        CallbackAction::ApproveConfirm { task_id } => {
            handle_approve_confirm(&bot, chat_id, task_id).await?;
        }
        CallbackAction::ApproveYes { task_id } => {
            handle_approve(&bot, chat_id, &service, task_id, card_context).await?;
        }
        CallbackAction::RejectInput { task_id } => {
            handle_reject_start(&bot, chat_id, task_id, &dialogue, card_context).await?;
        }
        CallbackAction::FollowUpReply { task_id } => {
            handle_follow_up_reply_start(&bot, chat_id, task_id, &dialogue, card_context).await?;
        }
        CallbackAction::CreateReviewTask { task_id } => {
            handle_create_review_task_confirm(&bot, chat_id, task_id).await?;
        }
        CallbackAction::CreateReviewTaskConfirm { task_id } => {
            handle_create_review_task(&bot, chat_id, &service, task_id, card_context).await?;
        }
        CallbackAction::DoneTask { task_id } => {
            handle_done_task(&bot, chat_id, &service, task_id, card_context).await?;
        }
        CallbackAction::ToolApprove { approval_id } => {
            handle_tool_approval_callback(
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
            handle_tool_approval_callback(
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
            show_task_detail(&bot, chat_id, &service, task_id, card_context).await?;
        }
        CallbackAction::NewTaskProject { project_id } => {
            handle_new_task_project(&bot, chat_id, &service, project_id, &dialogue, card_context)
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

async fn handle_tool_approval_callback(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    approval_id: &str,
    approve: bool,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let Some(pending_approval) = service.approvals.pending_by_id(approval_id) else {
        render_or_send_card(
            bot,
            chat_id,
            card_context,
            "This approval is no longer pending.",
            Some(empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    };

    if approval_requires_structured_input(&pending_approval) {
        render_or_send_card(
            bot,
            chat_id,
            card_context,
            "This approval requires structured input. Please continue in the Web UI.",
            Some(empty_inline_keyboard()),
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
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                message,
                Some(empty_inline_keyboard()),
            )
            .await?;
        }
        Err(ApprovalError::NotFound | ApprovalError::AlreadyCompleted) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                "This approval has already been handled.",
                Some(empty_inline_keyboard()),
            )
            .await?;
        }
        Err(e) => {
            render_or_send_card(
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

fn approval_requires_structured_input(approval: &PendingApprovalInfo) -> bool {
    approval
        .tool_name
        .eq_ignore_ascii_case("request_user_input")
}

// ─── Dialogue text handler ───────────────────────────────────────────

async fn handle_dialogue_text(
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
            if let Some(task_id) = extract_stage_summary_task_id_from_reply(&msg) {
                handle_follow_up_reply_finish(&bot, msg.chat.id, &service, task_id, text, None)
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
                let updated_prompt_id = render_or_send_card(
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
            let updated_prompt_id = render_or_send_card(
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
            let success = handle_create_task_finish(
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
            if success {
                dialogue.reset().await.ok();
                delete_message_best_effort(&bot, msg.chat.id, MessageId(prompt_message_id)).await;
            } else {
                dialogue.reset().await.ok();
            }
        }
        DialogueState::EditingTaskTitle {
            task_id,
            current_title,
            prompt_message_id,
        } => {
            if text.is_empty() {
                let updated_prompt_id = render_or_send_card(
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
            let updated_prompt_id = render_or_send_card(
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
            let success = handle_edit_task_finish(
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
            if success {
                dialogue.reset().await.ok();
                delete_message_best_effort(&bot, msg.chat.id, MessageId(prompt_message_id)).await;
            } else {
                dialogue.reset().await.ok();
            }
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
            let success = handle_reject_finish(
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
            if success {
                dialogue.reset().await.ok();
                delete_message_best_effort(&bot, msg.chat.id, MessageId(prompt_message_id)).await;
            } else {
                dialogue.reset().await.ok();
            }
        }
        DialogueState::ReplyingFollowUp {
            task_id,
            prompt_message_id,
        } => {
            if text.is_empty() {
                let updated_prompt_id = render_or_send_card(
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

            let success = handle_follow_up_reply_finish(
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
            if success {
                dialogue.reset().await.ok();
                delete_message_best_effort(&bot, msg.chat.id, MessageId(prompt_message_id)).await;
            } else {
                dialogue.reset().await.ok();
            }
        }
    }

    Ok(())
}

// ─── Screen builders ─────────────────────────────────────────────────

async fn show_projects_for_browsing(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    match Project::find_all(&service.db.pool).await {
        Ok(projects) if projects.is_empty() => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                "No projects found.",
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
        }
        Ok(projects) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                "Select a project:",
                Some(keyboard::project_list_keyboard(&projects, false)),
            )
            .await?;
        }
        Err(e) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to load projects: {e}"),
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
        }
    }
    Ok(())
}

async fn show_projects_for_new_task(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    match Project::find_all(&service.db.pool).await {
        Ok(projects) if projects.is_empty() => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                "No projects found. Create a project first.",
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
        }
        Ok(projects) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                "➕ Select a project for the new task:",
                Some(keyboard::project_list_keyboard(&projects, true)),
            )
            .await?;
        }
        Err(e) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to load projects: {e}"),
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
        }
    }
    Ok(())
}

async fn show_pending_approvals(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let pending = service.approvals.list_pending();
    let plan_approvals: Vec<_> = pending
        .iter()
        .filter(|a| a.tool_name == EXIT_PLAN_MODE_NAME)
        .collect();

    if plan_approvals.is_empty() {
        render_or_send_card(
            bot,
            chat_id,
            card_context,
            "No pending approvals.",
            Some(keyboard::home_only_keyboard()),
        )
        .await?;
        return Ok(());
    }

    let mut rows = Vec::new();
    let mut text = format!("⏳ Pending approvals ({}):\n", plan_approvals.len());

    for approval in &plan_approvals {
        let ctx =
            ExecutionProcess::load_context(&service.db.pool, approval.execution_process_id).await;
        if let Ok(ctx) = ctx {
            let short_id = ShortIdMapping::get_or_create(&service.db.pool, ctx.task.id)
                .await
                .unwrap_or_else(|_| "????".to_string());
            text.push_str(&format!(
                "\n[{}] {}",
                short_id,
                truncate_text(&ctx.task.title, 40)
            ));
            rows.push(vec![
                teloxide::types::InlineKeyboardButton::callback(
                    format!("✅ [{}]", short_id),
                    CallbackAction::ApproveConfirm {
                        task_id: ctx.task.id,
                    }
                    .encode(),
                ),
                teloxide::types::InlineKeyboardButton::callback(
                    format!("📝 [{}]", short_id),
                    CallbackAction::RejectInput {
                        task_id: ctx.task.id,
                    }
                    .encode(),
                ),
            ]);
        }
    }

    rows.push(vec![teloxide::types::InlineKeyboardButton::callback(
        "🏠 Home".to_string(),
        CallbackAction::Home.encode(),
    )]);

    render_or_send_card(
        bot,
        chat_id,
        card_context,
        text,
        Some(InlineKeyboardMarkup::new(rows)),
    )
    .await?;
    Ok(())
}

const PAGE_SIZE: usize = 10;

async fn show_task_list(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    project_id: Uuid,
    status_filter: Option<&str>,
    page: u16,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let project = match Project::find_by_id(&service.db.pool, project_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                "Project not found.",
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(());
        }
        Err(e) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to load project: {e}"),
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(());
        }
    };

    // If no status filter, show filter buttons first
    if status_filter.is_none() && page == 0 {
        render_or_send_card(
            bot,
            chat_id,
            card_context,
            format!("📋 {} — filter by status:", project.name),
            Some(keyboard::status_filter_keyboard(project_id)),
        )
        .await?;
        return Ok(());
    }

    let tasks =
        match Task::find_by_project_id_with_attempt_status(&service.db.pool, project.id).await {
            Ok(tasks) => tasks,
            Err(e) => {
                render_or_send_card(
                    bot,
                    chat_id,
                    card_context,
                    format!("Failed to load tasks: {e}"),
                    Some(keyboard::home_only_keyboard()),
                )
                .await?;
                return Ok(());
            }
        };

    let parsed_status = status_filter.and_then(parse_task_status);
    let filtered: Vec<_> = tasks
        .into_iter()
        .filter(|t| match &parsed_status {
            Some(s) => &t.status == s,
            None => true,
        })
        .collect();

    if filtered.is_empty() {
        let status_label = status_filter.unwrap_or("all");
        render_or_send_card(
            bot,
            chat_id,
            card_context,
            format!("No {} tasks in {}.", status_label, project.name),
            Some(keyboard::home_only_keyboard()),
        )
        .await?;
        return Ok(());
    }

    let offset = page as usize * PAGE_SIZE;
    let page_tasks = &filtered[offset.min(filtered.len())..];
    let has_more = page_tasks.len() > PAGE_SIZE;
    let page_tasks = &page_tasks[..page_tasks.len().min(PAGE_SIZE)];

    let mut task_buttons = Vec::new();
    for task in page_tasks {
        let short_id = ShortIdMapping::get_or_create(&service.db.pool, task.id)
            .await
            .unwrap_or_else(|_| "????".to_string());
        task_buttons.push((task.id, short_id, task.title.clone()));
    }

    let status_label = status_filter.unwrap_or("all");
    let header = format!(
        "📋 {} — {} tasks ({}):",
        project.name,
        status_label,
        filtered.len()
    );

    render_or_send_card(
        bot,
        chat_id,
        card_context,
        header,
        Some(keyboard::task_list_keyboard(
            &task_buttons,
            project_id,
            page,
            has_more,
        )),
    )
    .await?;
    Ok(())
}

async fn show_task_detail(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let task = match Task::find_by_id(&service.db.pool, task_id).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                "Task not found. It may have been deleted.",
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(());
        }
        Err(e) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to load task: {e}"),
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(());
        }
    };

    let project_name = match Project::find_by_id(&service.db.pool, task.project_id).await {
        Ok(Some(project)) => project.name,
        _ => "Unknown project".to_string(),
    };

    let short_id = ShortIdMapping::get_or_create(&service.db.pool, task.id)
        .await
        .unwrap_or_else(|_| "????".to_string());

    let description = task
        .description
        .clone()
        .unwrap_or_else(|| "No description.".to_string());

    let mut message = format!(
        "[{}] {:?}\nProject: {}\nTitle: {}\n\n{}",
        short_id,
        task.status,
        project_name,
        task.title,
        truncate_text(&description, 500)
    );

    if let Some(attempts_info) = service.fetch_attempts_info(task.id).await {
        if !attempts_info.is_empty() {
            message.push_str(&format!("\n\nAttempts ({}):", attempts_info.len()));
            for attempt in attempts_info {
                message.push_str(&format!("\n  • {attempt}"));
            }
        }
    }

    render_or_send_card(
        bot,
        chat_id,
        card_context,
        message,
        Some(keyboard::task_detail_keyboard(task.id, &task.status)),
    )
    .await?;
    Ok(())
}

// ─── Action handlers ─────────────────────────────────────────────────

async fn handle_run_default(
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
                delete_message_best_effort(bot, chat_id, context.source_message_id).await;
            }
        }
        Err(msg) => {
            render_or_send_card(
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

fn available_run_modes_for_executor(executor: &str) -> Vec<String> {
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

async fn handle_run_with_executor_mode(
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
        render_or_send_card(
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
                delete_message_best_effort(bot, chat_id, context.source_message_id).await;
            }

            let text = match Task::find_by_id(&service.db.pool, task_id).await {
                Ok(Some(task)) => format_run_started_message(&task, executor, selected_mode),
                Ok(None) => format!("▶️ Task started\nExecutor: {executor}\nMode: {selected_mode}"),
                Err(err) => {
                    tracing::warn!(
                        "Failed to load task {} after Telegram run start: {}",
                        task_id,
                        err
                    );
                    format!("▶️ Task started\nExecutor: {executor}\nMode: {selected_mode}")
                }
            };

            format::send_rich_then_plain(bot, chat_id, &text, None).await?;
        }
        Err(msg) => {
            render_or_send_card(
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

fn format_run_started_message(task: &Task, executor: &str, mode: &str) -> String {
    format!(
        "▶️ Running task\nTitle: {}\nExecutor: {}\nMode: {}",
        task.title, executor, mode
    )
}

async fn run_failure_keyboard(service: &TelegramBotService, task_id: Uuid) -> InlineKeyboardMarkup {
    match Task::find_by_id(&service.db.pool, task_id).await {
        Ok(Some(task)) => keyboard::task_detail_keyboard(task_id, &task.status),
        _ => keyboard::home_only_keyboard(),
    }
}

async fn handle_approve_confirm(bot: &Bot, chat_id: ChatId, task_id: Uuid) -> ResponseResult<()> {
    format::send_rich_then_plain(
        bot,
        chat_id,
        "⚠️ Confirm plan approval?",
        Some(keyboard::approve_confirm_keyboard(task_id)),
    )
    .await?;
    Ok(())
}

fn approval_completion_cleanup_plan(
    card_context: Option<CardRenderContext>,
) -> CompletionCleanupPlan {
    let interaction_message_id = card_context.map(|ctx| ctx.source_message_id);

    CompletionCleanupPlan {
        delete_plan_review_cards: true,
        delete_plan_message_id: None,
        delete_interaction_message_id: interaction_message_id,
        send_result_as_new_message: true,
    }
}

fn reject_completion_cleanup_plan(
    plan_message_id: Option<MessageId>,
    card_context: Option<CardRenderContext>,
) -> CompletionCleanupPlan {
    CompletionCleanupPlan {
        delete_plan_review_cards: true,
        delete_plan_message_id: plan_message_id,
        delete_interaction_message_id: card_context.map(|ctx| ctx.source_message_id),
        send_result_as_new_message: true,
    }
}

fn review_completion_cleanup_plan(
    card_context: Option<CardRenderContext>,
) -> CompletionCleanupPlan {
    CompletionCleanupPlan {
        delete_plan_review_cards: false,
        delete_plan_message_id: None,
        delete_interaction_message_id: card_context.map(|ctx| ctx.source_message_id),
        send_result_as_new_message: true,
    }
}

async fn handle_approve(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let Some(plan_approval) = service.find_exit_plan_approval(task_id).await else {
        render_or_send_card(
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
            apply_completion_cleanup(bot, chat_id, task_id, cleanup).await;
            send_completion_message(bot, chat_id, cleanup, card_context, "✅ Plan approved!")
                .await?;
        }
        Err(e) => {
            render_or_send_card(
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

async fn handle_reject_start(
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
        .update(DialogueState::RejectingPlan {
            task_id,
            prompt_message_id: prompt_message.id.0,
        })
        .await
        .ok();
    Ok(())
}

async fn handle_reject_finish(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    plan_message_id: Option<MessageId>,
    reason: Option<&str>,
    _card_context: Option<CardRenderContext>,
) -> ResponseResult<bool> {
    let Some(plan_approval) = service.find_exit_plan_approval(task_id).await else {
        render_or_send_card(
            bot,
            chat_id,
            _card_context,
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
            let cleanup = reject_completion_cleanup_plan(plan_message_id, _card_context);
            apply_completion_cleanup(bot, chat_id, task_id, cleanup).await;
            send_completion_message(bot, chat_id, cleanup, _card_context, "📝 Plan rejected.")
                .await?;
            return Ok(true);
        }
        Err(e) => {
            render_or_send_card(
                bot,
                chat_id,
                _card_context,
                format!("Failed to reject plan: {e}"),
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
        }
    }
    Ok(false)
}

async fn handle_follow_up_reply_start(
    bot: &Bot,
    chat_id: ChatId,
    task_id: Uuid,
    dialogue: &BotDialogue,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let prompt_message_id = render_or_send_card(
        bot,
        chat_id,
        card_context,
        "Type your reply here:",
        Some(keyboard::cancel_keyboard()),
    )
    .await?;
    dialogue
        .update(DialogueState::ReplyingFollowUp {
            task_id,
            prompt_message_id: prompt_message_id.0,
        })
        .await
        .ok();
    Ok(())
}

async fn handle_follow_up_reply_finish(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    prompt: &str,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<bool> {
    match service.send_follow_up_reply(task_id, prompt).await {
        Ok(()) => {
            render_or_send_card(
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
            render_or_send_card(
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

async fn handle_done_task(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let task = match Task::find_by_id(&service.db.pool, task_id).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                "Task not found. It may have been deleted.",
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(());
        }
        Err(e) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to load task: {e}"),
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(());
        }
    };

    let daily_project_id = {
        let config = service.config.read().await;
        config.daily_mode.project_id.clone()
    };
    let Some(daily_project_id) = daily_project_id else {
        render_or_send_card(
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
            render_or_send_card(
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
        render_or_send_card(
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

    render_or_send_card(
        bot,
        chat_id,
        card_context,
        message,
        Some(keyboard::home_only_keyboard()),
    )
    .await?;
    Ok(())
}

async fn handle_create_review_task(
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
            apply_completion_cleanup(bot, chat_id, task_id, cleanup).await;

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
            render_or_send_card(
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

async fn handle_create_review_task_confirm(
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

async fn handle_edit_start(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    dialogue: &BotDialogue,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let task = match Task::find_by_id(&service.db.pool, task_id).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                "Task not found.",
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(());
        }
        Err(e) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to load task: {e}"),
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(());
        }
    };

    if task.status != TaskStatus::Todo {
        render_or_send_card(
            bot,
            chat_id,
            card_context,
            format!("Task is {:?}. Only Todo tasks can be edited.", task.status),
            Some(keyboard::home_only_keyboard()),
        )
        .await?;
        return Ok(());
    }

    let prompt_message_id = render_or_send_card(
        bot,
        chat_id,
        card_context,
        format!("Current title: {}\n\nEnter a new title:", task.title),
        Some(keyboard::cancel_keyboard()),
    )
    .await?;

    dialogue
        .update(DialogueState::EditingTaskTitle {
            task_id,
            current_title: task.title.clone(),
            prompt_message_id: prompt_message_id.0,
        })
        .await
        .ok();

    Ok(())
}

async fn handle_edit_task_finish(
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
            render_or_send_card(
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
            render_or_send_card(
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
            render_or_send_card(
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
            render_or_send_card(
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
            render_or_send_card(
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

async fn handle_new_task_project(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    project_id: Uuid,
    dialogue: &BotDialogue,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let project = match Project::find_by_id(&service.db.pool, project_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                "Project not found.",
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(());
        }
        Err(e) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to load project: {e}"),
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(());
        }
    };

    let prompt_message_id = render_or_send_card(
        bot,
        chat_id,
        card_context,
        format!("➕ New task in {}\n\nEnter the task title:", project.name),
        Some(keyboard::cancel_keyboard()),
    )
    .await?;

    dialogue
        .update(DialogueState::CreatingTaskTitle {
            project_id,
            project_name: project.name.clone(),
            prompt_message_id: prompt_message_id.0,
        })
        .await
        .ok();
    Ok(())
}

async fn handle_create_task_finish(
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
            render_or_send_card(
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

// ─── Helper: edit or send ────────────────────────────────────────────

async fn render_or_send_card(
    bot: &Bot,
    chat_id: ChatId,
    card_context: Option<CardRenderContext>,
    text: impl Into<String>,
    markup: Option<InlineKeyboardMarkup>,
) -> ResponseResult<MessageId> {
    let text = text.into();
    let source_message_id = card_context.map(|context| context.source_message_id);
    let message =
        format::edit_or_send_rich_then_plain(bot, chat_id, source_message_id, &text, markup)
            .await?;
    Ok(message.id)
}

async fn delete_message_best_effort(bot: &Bot, chat_id: ChatId, message_id: MessageId) {
    if let Err(err) = bot.delete_message(chat_id, message_id).await {
        tracing::debug!(
            "Failed to delete Telegram message {} in chat {}: {}",
            message_id.0,
            chat_id.0,
            err
        );
    }
}

async fn cleanup_plan_review_cards(bot: &Bot, chat_id: ChatId, task_id: Uuid) -> Vec<MessageId> {
    let message_ids = notifier::take_plan_review_message_ids(task_id).await;
    for message_id in &message_ids {
        delete_message_best_effort(bot, chat_id, *message_id).await;
    }
    message_ids
}

async fn apply_completion_cleanup(
    bot: &Bot,
    chat_id: ChatId,
    task_id: Uuid,
    cleanup: CompletionCleanupPlan,
) {
    if cleanup.delete_plan_review_cards {
        cleanup_plan_review_cards(bot, chat_id, task_id).await;
    }

    if let Some(message_id) = cleanup.delete_plan_message_id {
        delete_message_best_effort(bot, chat_id, message_id).await;
    }

    if let Some(message_id) = cleanup.delete_interaction_message_id {
        delete_message_best_effort(bot, chat_id, message_id).await;
    }
}

async fn send_completion_message(
    bot: &Bot,
    chat_id: ChatId,
    cleanup: CompletionCleanupPlan,
    card_context: Option<CardRenderContext>,
    text: &str,
) -> ResponseResult<()> {
    if cleanup.send_result_as_new_message {
        format::send_rich_then_plain(bot, chat_id, text, Some(keyboard::home_only_keyboard()))
            .await?;
    } else {
        render_or_send_card(
            bot,
            chat_id,
            card_context,
            text,
            Some(keyboard::home_only_keyboard()),
        )
        .await?;
    }

    Ok(())
}

fn extract_stage_summary_task_id_from_reply(message: &Message) -> Option<Uuid> {
    let reply = message.reply_to_message()?;
    let from = reply.from.as_ref()?;
    if !from.is_bot {
        return None;
    }

    let markup = reply.reply_markup()?;
    extract_task_id_from_inline_keyboard(markup)
}

fn extract_task_id_from_inline_keyboard(markup: &InlineKeyboardMarkup) -> Option<Uuid> {
    for row in &markup.inline_keyboard {
        for button in row {
            let InlineKeyboardButtonKind::CallbackData(data) = &button.kind else {
                continue;
            };

            match CallbackAction::decode(data) {
                Some(CallbackAction::CreateReviewTask { task_id })
                | Some(CallbackAction::CreateReviewTaskConfirm { task_id })
                | Some(CallbackAction::DoneTask { task_id })
                | Some(CallbackAction::FollowUpReply { task_id }) => return Some(task_id),
                _ => {}
            }
        }
    }

    None
}

fn empty_inline_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(Vec::<Vec<teloxide::types::InlineKeyboardButton>>::new())
}

#[cfg(test)]
mod tests {
    use executors::logs::{ActionType, NormalizedEntry, NormalizedEntryType, ToolStatus};
    use teloxide::{
        types::{
            Chat, ChatFullInfo, ChatId, ChatKind, ChatPrivate, InlineKeyboardButton,
            InlineKeyboardMarkup, MediaKind, MediaText, Message, MessageCommon, MessageId,
            MessageKind, User, UserId,
        },
        utils::command::BotCommands,
    };
    use uuid::Uuid;

    use super::{
        CardRenderContext, Command, approval_completion_cleanup_plan,
        approval_requires_structured_input, extract_stage_summary_task_id_from_reply,
        extract_task_id_from_inline_keyboard, reject_completion_cleanup_plan,
        review_completion_cleanup_plan,
    };
    use crate::services::{
        approvals::PendingApprovalInfo,
        telegram::{callback::CallbackAction, keyboard},
    };

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
            vec!["/start", "/help", "/tasks", "/new", "/pending", "/cancel"]
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
    fn extracts_task_id_from_stage_summary_reply_message() {
        let task_id = Uuid::new_v4();
        let replied_message =
            bot_message_with_keyboard(keyboard::stage_summary_reply_keyboard(task_id));
        let reply_message = user_reply_message(replied_message);

        assert_eq!(
            extract_stage_summary_task_id_from_reply(&reply_message),
            Some(task_id)
        );
    }

    #[test]
    fn extracts_task_id_from_legacy_follow_up_button() {
        let task_id = Uuid::new_v4();
        let markup = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            "Reply",
            CallbackAction::FollowUpReply { task_id }.encode(),
        )]]);

        assert_eq!(extract_task_id_from_inline_keyboard(&markup), Some(task_id));
    }

    #[test]
    fn ignores_replies_without_stage_summary_callback_data() {
        let replied_message = bot_message_with_keyboard(InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("Home", CallbackAction::Home.encode()),
        ]]));
        let reply_message = user_reply_message(replied_message);

        assert_eq!(
            extract_stage_summary_task_id_from_reply(&reply_message),
            None
        );
    }

    #[test]
    fn ignores_replies_to_non_bot_messages() {
        let task_id = Uuid::new_v4();
        let replied_message =
            user_message_with_keyboard(keyboard::stage_summary_reply_keyboard(task_id));
        let reply_message = user_reply_message(replied_message);

        assert_eq!(
            extract_stage_summary_task_id_from_reply(&reply_message),
            None
        );
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

    fn bot_message_with_keyboard(markup: InlineKeyboardMarkup) -> Message {
        message_with_keyboard(true, markup)
    }

    fn user_message_with_keyboard(markup: InlineKeyboardMarkup) -> Message {
        message_with_keyboard(false, markup)
    }

    fn message_with_keyboard(is_bot: bool, markup: InlineKeyboardMarkup) -> Message {
        Message {
            id: MessageId(11),
            thread_id: None,
            from: Some(User {
                id: UserId(1),
                is_bot,
                first_name: if is_bot { "Bot" } else { "User" }.to_string(),
                last_name: None,
                username: None,
                language_code: None,
                is_premium: false,
                added_to_attachment_menu: false,
            }),
            sender_chat: None,
            date: chrono::Utc::now(),
            chat: Chat {
                id: ChatId(1),
                kind: ChatKind::Private(ChatPrivate {
                    username: None,
                    first_name: Some("Tester".to_string()),
                    last_name: None,
                    bio: None,
                    has_private_forwards: None,
                    has_restricted_voice_and_video_messages: None,
                }),
                photo: None,
                available_reactions: None,
                pinned_message: None,
                message_auto_delete_time: None,
                has_hidden_members: false,
                has_aggressive_anti_spam_enabled: false,
                chat_full_info: ChatFullInfo::default(),
            },
            is_topic_message: false,
            via_bot: None,
            kind: MessageKind::Common(MessageCommon {
                author_signature: None,
                forward_origin: None,
                reply_to_message: None,
                external_reply: None,
                quote: None,
                edit_date: None,
                media_kind: MediaKind::Text(MediaText {
                    text: "stage summary".to_string(),
                    entities: Vec::new(),
                    link_preview_options: None,
                }),
                reply_markup: Some(markup),
                is_automatic_forward: false,
                has_protected_content: false,
            }),
        }
    }

    fn user_reply_message(reply_to_message: Message) -> Message {
        let mut message = user_message_with_keyboard(InlineKeyboardMarkup::default());
        if let MessageKind::Common(common) = &mut message.kind {
            common.reply_to_message = Some(Box::new(reply_to_message));
            common.reply_markup = None;
            common.media_kind = MediaKind::Text(MediaText {
                text: "reply".to_string(),
                entities: Vec::new(),
                link_preview_options: None,
            });
        }
        message
    }
}
