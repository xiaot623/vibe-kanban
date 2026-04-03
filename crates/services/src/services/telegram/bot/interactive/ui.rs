use db::models::{project::Project, task::Task};
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButtonKind, InlineKeyboardMarkup, MessageId},
};
use uuid::Uuid;

use super::{BotDialogue, CardRenderContext};
use crate::services::telegram::{callback::CallbackAction, format, notifier};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CompletionCleanupPlan {
    pub(super) delete_plan_review_cards: bool,
    pub(super) delete_plan_message_id: Option<MessageId>,
    pub(super) delete_interaction_message_id: Option<MessageId>,
    pub(super) send_result_as_new_message: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompletionFailurePolicy {
    Reset,
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DialogueCompletionDecision {
    should_reset_dialogue: bool,
    should_delete_prompt: bool,
}

fn dialogue_completion_decision(
    success: bool,
    failure_policy: CompletionFailurePolicy,
) -> DialogueCompletionDecision {
    match (success, failure_policy) {
        (true, _) => DialogueCompletionDecision {
            should_reset_dialogue: true,
            should_delete_prompt: true,
        },
        (false, CompletionFailurePolicy::Reset) => DialogueCompletionDecision {
            should_reset_dialogue: true,
            should_delete_prompt: false,
        },
        (false, CompletionFailurePolicy::Keep) => DialogueCompletionDecision {
            should_reset_dialogue: false,
            should_delete_prompt: false,
        },
    }
}

pub(super) async fn complete_dialogue_step(
    bot: &Bot,
    chat_id: ChatId,
    dialogue: &BotDialogue,
    prompt_message_id: MessageId,
    success: bool,
    failure_policy: CompletionFailurePolicy,
) {
    let decision = dialogue_completion_decision(success, failure_policy);

    if decision.should_reset_dialogue {
        dialogue.reset().await.ok();
    }

    if decision.should_delete_prompt {
        delete_message_best_effort(bot, chat_id, prompt_message_id).await;
    }
}

pub(super) async fn render_or_send_card(
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

pub(super) async fn delete_message_best_effort(bot: &Bot, chat_id: ChatId, message_id: MessageId) {
    if let Err(err) = bot.delete_message(chat_id, message_id).await {
        tracing::debug!(
            "Failed to delete Telegram message {} in chat {}: {}",
            message_id.0,
            chat_id.0,
            err
        );
    }
}

pub(super) async fn load_project_or_render_error(
    bot: &Bot,
    chat_id: ChatId,
    pool: &sqlx::SqlitePool,
    project_id: Uuid,
    card_context: Option<CardRenderContext>,
    not_found_text: &str,
    error_prefix: &str,
) -> ResponseResult<Option<Project>> {
    match Project::find_by_id(pool, project_id).await {
        Ok(Some(project)) => Ok(Some(project)),
        Ok(None) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                not_found_text,
                Some(empty_inline_keyboard()),
            )
            .await?;
            Ok(None)
        }
        Err(err) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("{error_prefix}: {err}"),
                Some(empty_inline_keyboard()),
            )
            .await?;
            Ok(None)
        }
    }
}

pub(super) async fn load_task_or_render_error(
    bot: &Bot,
    chat_id: ChatId,
    pool: &sqlx::SqlitePool,
    task_id: Uuid,
    card_context: Option<CardRenderContext>,
    not_found_text: &str,
    error_prefix: &str,
) -> ResponseResult<Option<Task>> {
    match Task::find_by_id(pool, task_id).await {
        Ok(Some(task)) => Ok(Some(task)),
        Ok(None) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                not_found_text,
                Some(empty_inline_keyboard()),
            )
            .await?;
            Ok(None)
        }
        Err(err) => {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("{error_prefix}: {err}"),
                Some(empty_inline_keyboard()),
            )
            .await?;
            Ok(None)
        }
    }
}

pub(super) async fn finalize_completion_result(
    bot: &Bot,
    chat_id: ChatId,
    task_id: Uuid,
    cleanup: CompletionCleanupPlan,
    card_context: Option<CardRenderContext>,
    completion_text: Option<&str>,
) -> ResponseResult<()> {
    apply_completion_cleanup(bot, chat_id, task_id, cleanup).await;

    if let Some(text) = completion_text {
        if cleanup.send_result_as_new_message {
            format::send_rich_then_plain(bot, chat_id, text, Some(empty_inline_keyboard()))
                .await?;
        } else {
            render_or_send_card(
                bot,
                chat_id,
                card_context,
                text,
                Some(empty_inline_keyboard()),
            )
            .await?;
        }
    }

    Ok(())
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

pub(super) fn extract_stage_summary_task_id_from_reply(message: &Message) -> Option<Uuid> {
    let reply = message.reply_to_message()?;
    let from = reply.from.as_ref()?;
    if !from.is_bot {
        return None;
    }

    let markup = reply.reply_markup()?;
    extract_task_id_from_inline_keyboard(markup)
}

pub(super) fn extract_task_id_from_inline_keyboard(markup: &InlineKeyboardMarkup) -> Option<Uuid> {
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

pub(super) fn empty_inline_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(Vec::<Vec<teloxide::types::InlineKeyboardButton>>::new())
}

#[cfg(test)]
mod tests {
    use teloxide::types::{
        Chat, ChatFullInfo, ChatId, ChatKind, ChatPrivate, InlineKeyboardButton,
        InlineKeyboardMarkup, MediaKind, MediaText, Message, MessageCommon, MessageId, MessageKind,
        User, UserId,
    };
    use uuid::Uuid;

    use super::{
        CompletionFailurePolicy, dialogue_completion_decision,
        extract_stage_summary_task_id_from_reply, extract_task_id_from_inline_keyboard,
    };
    use crate::services::telegram::{callback::CallbackAction, keyboard};

    #[test]
    fn dialogue_completion_resets_and_deletes_prompt_on_success() {
        let decision = dialogue_completion_decision(true, CompletionFailurePolicy::Keep);

        assert!(decision.should_reset_dialogue);
        assert!(decision.should_delete_prompt);
    }

    #[test]
    fn dialogue_completion_resets_without_delete_when_failure_requires_reset() {
        let decision = dialogue_completion_decision(false, CompletionFailurePolicy::Reset);

        assert!(decision.should_reset_dialogue);
        assert!(!decision.should_delete_prompt);
    }

    #[test]
    fn dialogue_completion_keeps_state_on_failure_when_requested() {
        let decision = dialogue_completion_decision(false, CompletionFailurePolicy::Keep);

        assert!(!decision.should_reset_dialogue);
        assert!(!decision.should_delete_prompt);
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
