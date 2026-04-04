//! Telegram bot service entrypoint.
//!
//! Interactive runtime and shared command logic.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use db::DBService;
use teloxide::prelude::{Bot, ChatId};
use tokio::sync::RwLock;
use uuid::Uuid;

use super::notifier::TelegramContext;
use crate::services::{
    approvals::Approvals, config::Config, git::GitService, task_state::TaskStateService,
};

mod interactive;
mod shared;

/// Runtime-only pinned project state. Cleared on bot restart.
#[derive(Debug, Clone)]
pub struct PinnedProjectState {
    pub project_id: Uuid,
    pub project_name: String,
    pub expires_at: DateTime<Utc>,
}

/// Telegram bot service for handling commands and notifications.
pub struct TelegramBotService {
    db: DBService,
    git: GitService,
    config: Arc<RwLock<Config>>,
    approvals: Approvals,
    task_state: TaskStateService,
    /// Runtime-only pin state; `None` means no active pin.
    pin_state: Arc<RwLock<Option<PinnedProjectState>>>,
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
        let task_state = TaskStateService::new(db.pool.clone());
        let service = Self {
            db,
            git,
            config,
            approvals,
            task_state,
            pin_state: Arc::new(RwLock::new(None)),
        };

        Some(tokio::spawn(async move {
            service.start(bot_token, chat_id).await;
        }))
    }

    async fn start(self, bot_token: String, chat_id: i64) {
        let bot = Bot::new(bot_token);
        let chat_id = ChatId(chat_id);

        tracing::info!("Starting Telegram bot service");

        // Register telegram context for event handlers
        let tg_context = TelegramContext {
            db: self.db.clone(),
            bot: bot.clone(),
            chat_id,
            config: self.config.clone(),
            approvals: self.approvals.clone(),
            git: self.git.clone(),
        };
        super::notifier::set_telegram_context(tg_context).await;
        super::notifier::register_handlers(self.task_state.dispatcher()).await;

        interactive::run_dispatcher(self, bot, chat_id).await;

        super::notifier::clear_telegram_context().await;
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
            pin_state: self.pin_state.clone(),
        }
    }
}

impl TelegramBotService {
    /// Set the active pin, overwriting any existing one.
    pub async fn set_pin(&self, state: PinnedProjectState) {
        *self.pin_state.write().await = Some(state);
    }

    /// Clear the active pin.
    pub async fn clear_pin(&self) {
        *self.pin_state.write().await = None;
    }

    /// Resolve the active pin, auto-clearing it if it has expired.
    /// Returns `None` when there is no pin or the pin has expired.
    pub async fn resolve_pin(&self) -> Option<PinnedProjectState> {
        let mut guard = self.pin_state.write().await;
        if let Some(ref pin) = *guard {
            if Utc::now() >= pin.expires_at {
                *guard = None;
                return None;
            }
        }
        guard.clone()
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    /// Build a minimal pin-state holder for unit testing helpers.
    fn pin_state(initial: Option<PinnedProjectState>) -> Arc<RwLock<Option<PinnedProjectState>>> {
        Arc::new(RwLock::new(initial))
    }

    async fn resolve(state: &Arc<RwLock<Option<PinnedProjectState>>>) -> Option<PinnedProjectState> {
        let mut guard = state.write().await;
        if let Some(ref pin) = *guard {
            if Utc::now() >= pin.expires_at {
                *guard = None;
                return None;
            }
        }
        guard.clone()
    }

    #[tokio::test]
    async fn resolve_pin_returns_none_when_no_pin() {
        let state = pin_state(None);
        assert!(resolve(&state).await.is_none());
    }

    #[tokio::test]
    async fn resolve_pin_returns_valid_pin() {
        let state = pin_state(Some(PinnedProjectState {
            project_id: Uuid::new_v4(),
            project_name: "Demo".to_string(),
            expires_at: Utc::now() + Duration::hours(1),
        }));
        assert!(resolve(&state).await.is_some());
    }

    #[tokio::test]
    async fn resolve_pin_auto_clears_expired_pin() {
        let state = pin_state(Some(PinnedProjectState {
            project_id: Uuid::new_v4(),
            project_name: "Expired".to_string(),
            expires_at: Utc::now() - Duration::seconds(1),
        }));
        assert!(resolve(&state).await.is_none());
        assert!(state.read().await.is_none());
    }

    #[tokio::test]
    async fn clearing_pin_removes_active_pin() {
        let state = pin_state(Some(PinnedProjectState {
            project_id: Uuid::new_v4(),
            project_name: "Active".to_string(),
            expires_at: Utc::now() + Duration::hours(1),
        }));
        *state.write().await = None;
        assert!(state.read().await.is_none());
    }
}
