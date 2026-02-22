//! Telegram bot service entrypoint.
//!
//! Interactive/legacy runtimes and shared command logic are split
//! into dedicated submodules.

use std::sync::Arc;

use db::DBService;
use teloxide::prelude::{Bot, ChatId};
use tokio::sync::RwLock;

use super::notifier::TelegramContext;
use crate::services::{
    approvals::Approvals, config::Config, git::GitService, task_state::TaskStateService,
};

mod interactive;
mod legacy;
mod shared;

/// Telegram bot service for handling commands and notifications.
pub struct TelegramBotService {
    db: DBService,
    git: GitService,
    config: Arc<RwLock<Config>>,
    approvals: Approvals,
    task_state: TaskStateService,
}

impl TelegramBotService {
    pub async fn spawn(
        db: DBService,
        git: GitService,
        config: Arc<RwLock<Config>>,
        approvals: Approvals,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let telegram_config = config.read().await.telegram.clone();

        if !telegram_config.enabled {
            tracing::info!("Telegram bot disabled");
            return None;
        }

        let Some(bot_token) = telegram_config.bot_token else {
            tracing::warn!("Telegram bot enabled but bot_token is missing");
            return None;
        };

        let Some(chat_id) = telegram_config.chat_id else {
            tracing::warn!("Telegram bot enabled but chat_id is missing");
            return None;
        };
        let interactive_bot = telegram_config.interactive_bot;

        let task_state = TaskStateService::new(db.pool.clone());
        let service = Self {
            db,
            git,
            config,
            approvals,
            task_state,
        };

        Some(tokio::spawn(async move {
            service.start(bot_token, chat_id, interactive_bot).await;
        }))
    }

    async fn start(self, bot_token: String, chat_id: i64, interactive_bot: bool) {
        let bot = Bot::new(bot_token);
        let chat_id = ChatId(chat_id);

        tracing::info!(
            "Starting Telegram bot service (interactive_bot={})",
            interactive_bot
        );

        // Register telegram context for event handlers
        let tg_context = TelegramContext {
            db: self.db.clone(),
            bot: bot.clone(),
            chat_id,
            interactive_bot,
            approvals: self.approvals.clone(),
            git: self.git.clone(),
        };
        super::notifier::set_telegram_context(tg_context);
        super::notifier::register_handlers(self.task_state.dispatcher()).await;

        if interactive_bot {
            interactive::run_dispatcher(self, bot, chat_id).await;
        } else {
            legacy::run_dispatcher(self, bot, chat_id).await;
        }

        super::notifier::clear_telegram_context();
    }
}

impl Clone for TelegramBotService {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            git: self.git.clone(),
            config: self.config.clone(),
            approvals: self.approvals.clone(),
            task_state: self.task_state.clone(),
        }
    }
}
