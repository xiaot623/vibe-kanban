use std::sync::Arc;

use db::models::short_id_mapping::ShortIdMapping;
use teloxide::{prelude::*, utils::command::BotCommands};

use super::TelegramBotService;

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
    LegacyCommand::repl(bot, move |bot: Bot, msg: Message, cmd: LegacyCommand| {
        let service = Arc::clone(&service);
        async move { handle_command(bot, msg, cmd, service, chat_id).await }
    })
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
        bot.send_message(msg.chat.id, text).await?;
    }

    Ok(())
}
