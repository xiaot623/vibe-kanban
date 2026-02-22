use std::{str::FromStr, sync::Arc};

use db::models::{
    execution_process::ExecutionProcess,
    project::Project,
    short_id_mapping::ShortIdMapping,
    task::{CreateTask, Task, TaskStatus, UpdateTask},
};
use executors::{executors::BaseCodingAgent, profile::ExecutorConfigs};
use teloxide::{
    dispatching::{Dispatcher, UpdateFilterExt, dialogue::InMemStorage},
    dptree,
    prelude::*,
    types::{InlineKeyboardMarkup, ParseMode},
    utils::command::BotCommands,
};
use utils::{
    approvals::{ApprovalResponse, ApprovalStatus},
    response::ApiResponse,
};
use uuid::Uuid;

use super::{
    TelegramBotService,
    legacy::{self, LegacyCommand},
    shared::{api_base_url, parse_task_status, truncate_text},
};
use crate::services::telegram::{
    EXIT_PLAN_MODE_NAME, callback::CallbackAction, keyboard, state::DialogueState,
};

/// Type alias for the dialogue handle used in handlers.
type BotDialogue =
    teloxide::dispatching::dialogue::Dialogue<DialogueState, InMemStorage<DialogueState>>;

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
        // Branch 3: legacy slash commands remain available in interactive mode.
        .branch(
            Update::filter_message()
                .filter_command::<LegacyCommand>()
                .endpoint(handle_legacy_command),
        )
        // Branch 4: plain text messages (dialogue input)
        .branch(Update::filter_message().endpoint(handle_dialogue_text));

    let service = Arc::new(service);

    let deps = dptree::deps![storage, service, chat_id];

    Dispatcher::builder(bot, handler)
        .dependencies(deps)
        .default_handler(|upd: Arc<Update>| async move {
            tracing::trace!("Unhandled update: {:?}", upd.id);
        })
        .build()
        .dispatch()
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
            bot.send_message(msg.chat.id, text)
                .reply_markup(keyboard::home_keyboard())
                .await?;
        }
        Command::Help => {
            let help = format!(
                "🏠 *Quick actions* — use the buttons from /start\n\n\
                 *Legacy commands:*\n{}",
                LegacyCommand::descriptions()
            );
            bot.send_message(msg.chat.id, help)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
        }
        Command::Tasks => {
            show_projects_for_browsing(&bot, msg.chat.id, &service).await?;
        }
        Command::New => {
            show_projects_for_new_task(&bot, msg.chat.id, &service).await?;
        }
        Command::Pending => {
            show_pending_approvals(&bot, msg.chat.id, &service).await?;
        }
        Command::Cancel => {
            dialogue.reset().await.ok();
            bot.send_message(msg.chat.id, "Cancelled.")
                .reply_markup(keyboard::home_keyboard())
                .await?;
        }
    }

    Ok(())
}

async fn handle_legacy_command(
    bot: Bot,
    msg: Message,
    cmd: LegacyCommand,
    service: Arc<TelegramBotService>,
    allowed_chat_id: ChatId,
) -> ResponseResult<()> {
    legacy::handle_command(bot, msg, cmd, service, allowed_chat_id).await
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

    let data = match q.data.as_deref() {
        Some(d) => d,
        None => return Ok(()),
    };

    let action = match CallbackAction::decode(data) {
        Some(a) => a,
        None => {
            send_or_edit(
                &bot,
                &q,
                chat_id,
                "Invalid action. Please try again.",
                Some(keyboard::home_only_keyboard()),
            )
            .await?;
            return Ok(());
        }
    };

    // Reset dialogue state for non-dialogue actions
    match &action {
        CallbackAction::Cancel => {
            dialogue.reset().await.ok();
            send_or_edit(
                &bot,
                &q,
                chat_id,
                "Cancelled.",
                Some(keyboard::home_keyboard()),
            )
            .await?;
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
            send_or_edit(
                &bot,
                &q,
                chat_id,
                "Choose an action:",
                Some(keyboard::home_keyboard()),
            )
            .await?;
        }
        CallbackAction::Projects => {
            show_projects_for_browsing(&bot, chat_id, &service).await?;
        }
        CallbackAction::NewTask => {
            show_projects_for_new_task(&bot, chat_id, &service).await?;
        }
        CallbackAction::Pending => {
            show_pending_approvals(&bot, chat_id, &service).await?;
        }
        CallbackAction::Tasks { project_id, status } => {
            show_task_list(&bot, chat_id, &service, project_id, status.as_deref(), 0).await?;
        }
        CallbackAction::TaskPage { project_id, page } => {
            show_task_list(&bot, chat_id, &service, project_id, None, page).await?;
        }
        CallbackAction::TaskDetail { task_id } => {
            show_task_detail(&bot, chat_id, &service, task_id).await?;
        }
        CallbackAction::RunDefault { task_id } => {
            handle_run_default(&bot, chat_id, &service, task_id).await?;
        }
        CallbackAction::RunPick { task_id } => {
            bot.send_message(chat_id, "Select an executor:")
                .reply_markup(keyboard::executor_pick_keyboard(task_id))
                .await?;
        }
        CallbackAction::RunWith { task_id, executor } => {
            let run_modes = available_run_modes_for_executor(&executor);
            bot.send_message(chat_id, format!("Select run mode for {executor}:"))
                .reply_markup(keyboard::run_mode_pick_keyboard(
                    task_id, &executor, &run_modes,
                ))
                .await?;
        }
        CallbackAction::RunWithMode {
            task_id,
            executor,
            mode_index,
        } => {
            handle_run_with_executor_mode(&bot, chat_id, &service, task_id, &executor, mode_index)
                .await?;
        }
        CallbackAction::EditTask { task_id } => {
            handle_edit_start(&bot, chat_id, &service, task_id, &dialogue).await?;
        }
        CallbackAction::ApproveConfirm { task_id } => {
            bot.send_message(chat_id, "⚠️ Confirm plan approval?")
                .reply_markup(keyboard::approve_confirm_keyboard(task_id))
                .await?;
        }
        CallbackAction::ApproveYes { task_id } => {
            handle_approve(&bot, chat_id, &service, task_id).await?;
        }
        CallbackAction::RejectInput { task_id } => {
            handle_reject_start(&bot, chat_id, task_id, &dialogue).await?;
        }
        CallbackAction::Refresh { task_id } => {
            show_task_detail(&bot, chat_id, &service, task_id).await?;
        }
        CallbackAction::NewTaskProject { project_id } => {
            handle_new_task_project(&bot, chat_id, &service, project_id, &dialogue).await?;
        }
        CallbackAction::Cancel | CallbackAction::Noop => {
            // Already handled above
        }
    }

    Ok(())
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

    let state = match dialogue.get().await {
        Ok(Some(state)) => state,
        _ => return Ok(()), // No active dialogue — ignore plain text
    };

    let text = msg.text().unwrap_or("").trim();

    match state {
        DialogueState::Idle => {
            // Not in a dialogue — ignore
        }
        DialogueState::CreatingTaskTitle {
            project_id,
            project_name,
        } => {
            if text.is_empty() {
                bot.send_message(msg.chat.id, "Title cannot be empty. Please enter a title:")
                    .reply_markup(keyboard::cancel_keyboard())
                    .await?;
                return Ok(());
            }
            dialogue
                .update(DialogueState::CreatingTaskDescription {
                    project_id,
                    project_name: project_name.clone(),
                    title: text.to_string(),
                })
                .await
                .ok();
            bot.send_message(
                msg.chat.id,
                format!(
                    "Title: {}\n\nNow enter a description (or press Skip):",
                    text
                ),
            )
            .reply_markup(keyboard::skip_cancel_keyboard())
            .await?;
        }
        DialogueState::CreatingTaskDescription {
            project_id,
            project_name: _,
            title,
        } => {
            let description = if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            };
            dialogue.reset().await.ok();
            handle_create_task_finish(
                &bot,
                msg.chat.id,
                &service,
                project_id,
                &title,
                description.as_deref(),
            )
            .await?;
        }
        DialogueState::EditingTaskTitle {
            task_id,
            current_title: _,
        } => {
            if text.is_empty() {
                bot.send_message(
                    msg.chat.id,
                    "Title cannot be empty. Please enter a new title:",
                )
                .reply_markup(keyboard::cancel_keyboard())
                .await?;
                return Ok(());
            }
            // Load current description for next step
            let current_description = match Task::find_by_id(&service.db.pool, task_id).await {
                Ok(Some(task)) => task.description,
                _ => None,
            };
            dialogue
                .update(DialogueState::EditingTaskDescription {
                    task_id,
                    title: text.to_string(),
                    current_description,
                })
                .await
                .ok();
            bot.send_message(
                msg.chat.id,
                format!(
                    "New title: {}\n\nNow enter a new description (or press Skip to keep current):",
                    text
                ),
            )
            .reply_markup(keyboard::skip_cancel_keyboard())
            .await?;
        }
        DialogueState::EditingTaskDescription {
            task_id,
            title,
            current_description,
        } => {
            let description = if text.is_empty() {
                current_description
            } else {
                Some(text.to_string())
            };
            dialogue.reset().await.ok();
            handle_edit_task_finish(
                &bot,
                msg.chat.id,
                &service,
                task_id,
                &title,
                description.as_deref(),
            )
            .await?;
        }
        DialogueState::RejectingPlan { task_id } => {
            let reason = if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            };
            dialogue.reset().await.ok();
            handle_reject_finish(&bot, msg.chat.id, &service, task_id, reason.as_deref()).await?;
        }
    }

    Ok(())
}

// ─── Screen builders ─────────────────────────────────────────────────

async fn show_projects_for_browsing(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
) -> ResponseResult<()> {
    match Project::find_all(&service.db.pool).await {
        Ok(projects) if projects.is_empty() => {
            bot.send_message(chat_id, "No projects found.")
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
        }
        Ok(projects) => {
            bot.send_message(chat_id, "Select a project:")
                .reply_markup(keyboard::project_list_keyboard(&projects, false))
                .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("Failed to load projects: {e}"))
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
        }
    }
    Ok(())
}

async fn show_projects_for_new_task(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
) -> ResponseResult<()> {
    match Project::find_all(&service.db.pool).await {
        Ok(projects) if projects.is_empty() => {
            bot.send_message(chat_id, "No projects found. Create a project first.")
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
        }
        Ok(projects) => {
            bot.send_message(chat_id, "➕ Select a project for the new task:")
                .reply_markup(keyboard::project_list_keyboard(&projects, true))
                .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("Failed to load projects: {e}"))
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
        }
    }
    Ok(())
}

async fn show_pending_approvals(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
) -> ResponseResult<()> {
    let pending = service.approvals.list_pending();
    let plan_approvals: Vec<_> = pending
        .iter()
        .filter(|a| a.tool_name == EXIT_PLAN_MODE_NAME)
        .collect();

    if plan_approvals.is_empty() {
        bot.send_message(chat_id, "No pending approvals.")
            .reply_markup(keyboard::home_only_keyboard())
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

    bot.send_message(chat_id, text)
        .reply_markup(InlineKeyboardMarkup::new(rows))
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
) -> ResponseResult<()> {
    let project = match Project::find_by_id(&service.db.pool, project_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            bot.send_message(chat_id, "Project not found.")
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
            return Ok(());
        }
        Err(e) => {
            bot.send_message(chat_id, format!("Failed to load project: {e}"))
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
            return Ok(());
        }
    };

    // If no status filter, show filter buttons first
    if status_filter.is_none() && page == 0 {
        bot.send_message(chat_id, format!("📋 {} — filter by status:", project.name))
            .reply_markup(keyboard::status_filter_keyboard(project_id))
            .await?;
        return Ok(());
    }

    let tasks =
        match Task::find_by_project_id_with_attempt_status(&service.db.pool, project.id).await {
            Ok(tasks) => tasks,
            Err(e) => {
                bot.send_message(chat_id, format!("Failed to load tasks: {e}"))
                    .reply_markup(keyboard::home_only_keyboard())
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
        bot.send_message(
            chat_id,
            format!("No {} tasks in {}.", status_label, project.name),
        )
        .reply_markup(keyboard::home_only_keyboard())
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

    bot.send_message(chat_id, header)
        .reply_markup(keyboard::task_list_keyboard(
            &task_buttons,
            project_id,
            page,
            has_more,
        ))
        .await?;
    Ok(())
}

async fn show_task_detail(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
) -> ResponseResult<()> {
    let task = match Task::find_by_id(&service.db.pool, task_id).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            bot.send_message(chat_id, "Task not found. It may have been deleted.")
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
            return Ok(());
        }
        Err(e) => {
            bot.send_message(chat_id, format!("Failed to load task: {e}"))
                .reply_markup(keyboard::home_only_keyboard())
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

    bot.send_message(chat_id, message)
        .reply_markup(keyboard::task_detail_keyboard(task.id, &task.status))
        .await?;
    Ok(())
}

// ─── Action handlers ─────────────────────────────────────────────────

async fn handle_run_default(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
) -> ResponseResult<()> {
    let config = service.config.read().await.telegram.clone();
    let result = service
        .run_task_impl(task_id, &config.default_executor, None, None)
        .await;
    match result {
        Ok(()) => {
            // Notification will arrive via TaskInProgressHandler
        }
        Err(msg) => {
            bot.send_message(chat_id, msg)
                .reply_markup(keyboard::home_only_keyboard())
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
) -> ResponseResult<()> {
    let run_modes = available_run_modes_for_executor(executor);
    let Some(selected_mode) = run_modes.get(mode_index as usize) else {
        bot.send_message(chat_id, "Invalid run mode selection. Please try again.")
            .reply_markup(keyboard::executor_pick_keyboard(task_id))
            .await?;
        return Ok(());
    };

    let result = service
        .run_task_impl(task_id, executor, Some(selected_mode.as_str()), None)
        .await;
    match result {
        Ok(()) => {}
        Err(msg) => {
            bot.send_message(chat_id, msg)
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
        }
    }
    Ok(())
}

async fn handle_approve(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
) -> ResponseResult<()> {
    let Some(plan_approval) = service.find_exit_plan_approval(task_id).await else {
        bot.send_message(
            chat_id,
            "No pending plan approval found. It may have expired.",
        )
        .reply_markup(keyboard::home_only_keyboard())
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
            bot.send_message(chat_id, "✅ Plan approved!")
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("Failed to approve plan: {e}"))
                .reply_markup(keyboard::home_only_keyboard())
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
) -> ResponseResult<()> {
    dialogue
        .update(DialogueState::RejectingPlan { task_id })
        .await
        .ok();
    bot.send_message(
        chat_id,
        "Enter a rejection reason (or send any text to reject without reason):",
    )
    .reply_markup(keyboard::cancel_keyboard())
    .await?;
    Ok(())
}

async fn handle_reject_finish(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    reason: Option<&str>,
) -> ResponseResult<()> {
    let Some(plan_approval) = service.find_exit_plan_approval(task_id).await else {
        bot.send_message(
            chat_id,
            "No pending plan approval found. It may have expired.",
        )
        .reply_markup(keyboard::home_only_keyboard())
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
                status: ApprovalStatus::Denied {
                    reason: reason.map(String::from),
                },
            },
        )
        .await
    {
        Ok(_) => {
            bot.send_message(chat_id, "📝 Plan rejected.")
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("Failed to reject plan: {e}"))
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
        }
    }
    Ok(())
}

async fn handle_edit_start(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    dialogue: &BotDialogue,
) -> ResponseResult<()> {
    let task = match Task::find_by_id(&service.db.pool, task_id).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            bot.send_message(chat_id, "Task not found.")
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
            return Ok(());
        }
        Err(e) => {
            bot.send_message(chat_id, format!("Failed to load task: {e}"))
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
            return Ok(());
        }
    };

    if task.status != TaskStatus::Todo {
        bot.send_message(
            chat_id,
            format!("Task is {:?}. Only Todo tasks can be edited.", task.status),
        )
        .reply_markup(keyboard::home_only_keyboard())
        .await?;
        return Ok(());
    }

    dialogue
        .update(DialogueState::EditingTaskTitle {
            task_id,
            current_title: task.title.clone(),
        })
        .await
        .ok();

    bot.send_message(
        chat_id,
        format!("Current title: {}\n\nEnter a new title:", task.title),
    )
    .reply_markup(keyboard::cancel_keyboard())
    .await?;
    Ok(())
}

async fn handle_edit_task_finish(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    title: &str,
    description: Option<&str>,
) -> ResponseResult<()> {
    let payload = UpdateTask {
        title: Some(title.to_string()),
        description: Some(description.unwrap_or("").to_string()),
        status: None,
        parent_workspace_id: None,
        image_ids: None,
    };

    let base_url = match api_base_url().await {
        Ok(url) => url,
        Err(e) => {
            bot.send_message(chat_id, format!("Failed to locate API server: {e}"))
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
            return Ok(());
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
            bot.send_message(chat_id, format!("Failed to edit task: {e}"))
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
            return Ok(());
        }
    };

    let api_response: ApiResponse<Task> = match response.json().await {
        Ok(r) => r,
        Err(e) => {
            bot.send_message(chat_id, format!("Failed to parse response: {e}"))
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
            return Ok(());
        }
    };

    let error_message = api_response.message().map(String::from);
    match api_response.into_data() {
        Some(updated_task) => {
            let short_id = ShortIdMapping::get_or_create(&service.db.pool, updated_task.id)
                .await
                .unwrap_or_else(|_| "????".to_string());
            bot.send_message(
                chat_id,
                format!("✏️ Updated [{}]\nTitle: {}", short_id, updated_task.title),
            )
            .reply_markup(keyboard::task_detail_keyboard(
                updated_task.id,
                &updated_task.status,
            ))
            .await?;
        }
        None => {
            let msg = error_message.unwrap_or_else(|| "Failed to edit task.".to_string());
            bot.send_message(chat_id, msg)
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
        }
    }
    Ok(())
}

async fn handle_new_task_project(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    project_id: Uuid,
    dialogue: &BotDialogue,
) -> ResponseResult<()> {
    let project = match Project::find_by_id(&service.db.pool, project_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            bot.send_message(chat_id, "Project not found.")
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
            return Ok(());
        }
        Err(e) => {
            bot.send_message(chat_id, format!("Failed to load project: {e}"))
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
            return Ok(());
        }
    };

    dialogue
        .update(DialogueState::CreatingTaskTitle {
            project_id,
            project_name: project.name.clone(),
        })
        .await
        .ok();

    bot.send_message(
        chat_id,
        format!("➕ New task in *{}*\n\nEnter the task title:", project.name),
    )
    .parse_mode(ParseMode::MarkdownV2)
    .reply_markup(keyboard::cancel_keyboard())
    .await?;
    Ok(())
}

async fn handle_create_task_finish(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    project_id: Uuid,
    title: &str,
    description: Option<&str>,
) -> ResponseResult<()> {
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
        }
        Err(e) => {
            bot.send_message(chat_id, format!("Failed to create task: {e}"))
                .reply_markup(keyboard::home_only_keyboard())
                .await?;
        }
    }
    Ok(())
}

// ─── Helper: edit or send ────────────────────────────────────────────

/// Try to edit the callback message; fall back to sending a new one.
async fn send_or_edit(
    bot: &Bot,
    q: &CallbackQuery,
    chat_id: ChatId,
    text: &str,
    markup: Option<InlineKeyboardMarkup>,
) -> ResponseResult<()> {
    if let Some(msg) = q.message.as_ref() {
        let mut req = bot.edit_message_text(chat_id, msg.id(), text);
        if let Some(ref kb) = markup {
            req = req.reply_markup(kb.clone());
        }
        match req.await {
            Ok(_) => return Ok(()),
            Err(e) => {
                tracing::trace!("Failed to edit message, sending new one: {e}");
            }
        }
    }

    let mut req = bot.send_message(chat_id, text);
    if let Some(kb) = markup {
        req = req.reply_markup(kb);
    }
    req.await?;
    Ok(())
}
