use std::{sync::Arc, time::Duration};

use db::models::short_id_mapping::ShortIdMapping;
use teloxide::{
    dispatching::{Dispatcher, UpdateFilterExt},
    dptree,
    error_handlers::LoggingErrorHandler,
    prelude::*,
    update_listeners::Polling,
    utils::command::BotCommands,
};

use super::TelegramBotService;
use crate::services::telegram::format;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Commands:")]
pub enum LegacyCommand {
    #[command(description = "show this help")]
    Help,
    #[command(description = "list all projects")]
    Project,
    #[command(description = "/list <project> [status]")]
    List(String),
    #[command(description = "/task <uuid>")]
    Task(String),
    #[command(description = "/add <project>\\n<title>\\n<description>")]
    Add(String),
    #[command(description = "/edit <task_id>\\n<title>\\n<description>")]
    Edit(String),
    #[command(description = "/run <uuid> [executor] [mode] [branch]")]
    Run(String),
    #[command(description = "/approve <task_id>")]
    Approve(String),
    #[command(description = "/reject <task_id> [reason]")]
    Reject(String),
}

pub(super) async fn run_dispatcher(service: TelegramBotService, bot: Bot, chat_id: ChatId) {
    let service = Arc::new(service);
    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<LegacyCommand>()
                .endpoint(handle_command),
        )
        .branch(Update::filter_message().endpoint(handle_plain_text));

    let deps = dptree::deps![service, chat_id];

    if let Err(err) = bot.delete_webhook().send().await {
        tracing::warn!(
            "Failed to delete Telegram webhook before polling startup: {:?}",
            err
        );
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

pub(super) async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: LegacyCommand,
    service: Arc<TelegramBotService>,
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

    let response = match cmd {
        LegacyCommand::Help => Some(LegacyCommand::descriptions().to_string()),
        LegacyCommand::Project => Some(service.list_projects().await),
        LegacyCommand::List(args) => Some(service.list_tasks_for_project(args).await),
        LegacyCommand::Task(task_id_raw) => Some(service.describe_task(&task_id_raw).await),
        LegacyCommand::Add(payload) => service.create_task_from_command(&payload).await,
        LegacyCommand::Edit(payload) => Some(service.edit_task_from_command(&payload).await),
        LegacyCommand::Run(payload) => service.run_task_from_command(&payload).await,
        LegacyCommand::Approve(payload) => service.approve_plan_from_command(&payload).await,
        LegacyCommand::Reject(payload) => service.reject_plan_from_command(&payload).await,
    };

    if let Some(text) = response {
        format::send_rich_then_plain(&bot, msg.chat.id, &text, None).await?;
    }

    Ok(())
}

async fn handle_plain_text(
    bot: Bot,
    msg: Message,
    service: Arc<TelegramBotService>,
    allowed_chat_id: ChatId,
) -> ResponseResult<()> {
    if msg.chat.id != allowed_chat_id {
        tracing::warn!(
            "Ignoring telegram message from unexpected chat: {}",
            msg.chat.id.0
        );
        return Ok(());
    }

    let text = msg.text().unwrap_or("").trim();
    if text.is_empty() || text.starts_with('/') {
        return Ok(());
    }

    // Lazily clean up expired short ID mappings
    ShortIdMapping::cleanup_expired(&service.db.pool).await;

    if let Err(err) = service.create_daily_task_from_message(text).await {
        format::send_rich_then_plain(&bot, msg.chat.id, &err, None).await?;
    }

    Ok(())
}
