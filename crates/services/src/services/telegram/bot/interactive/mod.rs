use std::{sync::Arc, time::Duration};

use teloxide::{
    dispatching::{Dispatcher, UpdateFilterExt, dialogue::InMemStorage},
    dptree,
    error_handlers::LoggingErrorHandler,
    prelude::*,
    types::MessageId,
    update_listeners::Polling,
    utils::command::BotCommands,
};

use super::TelegramBotService;
use crate::services::telegram::state::DialogueState;

mod actions;
mod dialogue;
mod router;
mod screens;
mod ui;

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

pub(super) async fn run_dispatcher(service: TelegramBotService, bot: Bot, chat_id: ChatId) {
    let storage: Arc<InMemStorage<DialogueState>> = InMemStorage::new();

    // Build the update handler tree
    let handler = dptree::entry()
        // Inject dialogue state for all branches
        .enter_dialogue::<Update, InMemStorage<DialogueState>, DialogueState>()
        // Branch 1: callback queries (button presses)
        .branch(Update::filter_callback_query().endpoint(router::handle_callback))
        // Branch 2: messages with commands
        .branch(
            Update::filter_message()
                .filter_command::<router::Command>()
                .endpoint(router::handle_command),
        )
        // Branch 3: plain text messages (dialogue input)
        .branch(Update::filter_message().endpoint(dialogue::handle_dialogue_text));

    let service = Arc::new(service);

    let deps = dptree::deps![storage, service, chat_id];

    if let Err(err) = bot.delete_webhook().send().await {
        tracing::warn!(
            "Failed to delete Telegram webhook before polling startup: {:?}",
            err
        );
    }

    if let Err(err) = bot
        .set_my_commands(router::Command::bot_commands())
        .send()
        .await
    {
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
