use db::models::{task::Task, telegram_task_topic::TelegramTaskTopic};
use serde::Deserialize;
use sqlx::SqlitePool;
use teloxide::{
    prelude::*,
    types::{ForumTopic, MessageId, ThreadId},
};
use uuid::Uuid;

const TELEGRAM_TOPIC_TITLE_LIMIT: usize = 128;
const FALLBACK_TOPIC_TITLE: &str = "Untitled task";
const DEFAULT_TOPIC_ICON_COLOR: u32 = 0x6FB9F0;

#[derive(Debug, Clone)]
pub struct TaskTopic {
    pub task_id: Uuid,
    pub thread_id: ThreadId,
    pub topic_name: String,
}

impl TaskTopic {
    fn from_binding(binding: TelegramTaskTopic) -> Option<Self> {
        let message_thread_id = binding.message_thread_id?;
        let message_thread_id = i32::try_from(message_thread_id).ok()?;
        Some(Self {
            task_id: binding.task_id,
            thread_id: ThreadId(MessageId(message_thread_id)),
            topic_name: binding
                .topic_name
                .unwrap_or_else(|| FALLBACK_TOPIC_TITLE.to_string()),
        })
    }
}

pub async fn find_task_topic(
    pool: &SqlitePool,
    task_id: Uuid,
) -> Result<Option<TaskTopic>, sqlx::Error> {
    Ok(TelegramTaskTopic::find_by_task_id(pool, task_id)
        .await?
        .and_then(TaskTopic::from_binding))
}

pub async fn find_task_topic_by_thread_id(
    pool: &SqlitePool,
    thread_id: ThreadId,
) -> Result<Option<TaskTopic>, sqlx::Error> {
    Ok(
        TelegramTaskTopic::find_by_message_thread_id(pool, i64::from(thread_id.0.0))
            .await?
            .and_then(TaskTopic::from_binding),
    )
}

pub async fn ensure_task_topic(
    pool: &SqlitePool,
    bot: &Bot,
    chat_id: ChatId,
    task: &Task,
) -> Option<TaskTopic> {
    match find_task_topic(pool, task.id).await {
        Ok(Some(existing)) => return Some(existing),
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(task_id = %task.id, "Failed to look up Telegram task topic: {err}");
            return None;
        }
    }

    let topic_name = topic_title(&task.title);
    match create_forum_topic(bot, chat_id, &topic_name).await {
        Ok(topic) => {
            let thread_id = topic.thread_id;
            match TelegramTaskTopic::bind(pool, task.id, i64::from(thread_id.0.0), &topic_name)
                .await
            {
                Ok(binding) => TaskTopic::from_binding(binding),
                Err(err) => {
                    tracing::warn!(
                        task_id = %task.id,
                        thread_id = thread_id.0.0,
                        "Created Telegram topic but failed to store task binding: {err}"
                    );
                    Some(TaskTopic {
                        task_id: task.id,
                        thread_id,
                        topic_name,
                    })
                }
            }
        }
        Err(err) => {
            tracing::warn!(task_id = %task.id, "Failed to create Telegram task topic: {err}");
            if let Err(db_err) = TelegramTaskTopic::create_empty(pool, task.id).await {
                tracing::debug!(
                    task_id = %task.id,
                    "Failed to store empty Telegram task topic binding after create failure: {db_err}"
                );
            }
            None
        }
    }
}

async fn create_forum_topic(
    bot: &Bot,
    chat_id: ChatId,
    topic_name: &str,
) -> Result<ForumTopic, String> {
    let api_url = bot
        .api_url()
        .join(&format!("/bot{}/createForumTopic", bot.token()))
        .map_err(|err| format!("failed to build Telegram API URL: {err}"))?;
    let payload = serde_json::json!({
        "chat_id": chat_id.0,
        "name": topic_name,
        "icon_color": DEFAULT_TOPIC_ICON_COLOR,
    });
    let response = bot
        .client()
        .post(api_url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?
        .text()
        .await
        .map_err(|err| format!("failed to read response: {err}"))?;
    let response: TelegramApiResponse<ForumTopic> = serde_json::from_str(&response)
        .map_err(|err| format!("failed to parse response: {err}"))?;
    response.into_result()
}

#[derive(Debug, Deserialize)]
struct TelegramApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

impl<T> TelegramApiResponse<T> {
    fn into_result(self) -> Result<T, String> {
        match (self.ok, self.result, self.description) {
            (true, Some(result), _) => Ok(result),
            (true, None, _) => Err("Telegram API returned ok without result".to_string()),
            (false, _, Some(description)) => Err(description),
            (false, _, None) => Err("Telegram API request failed".to_string()),
        }
    }
}

pub async fn delete_task_topic(
    pool: &SqlitePool,
    bot: &Bot,
    chat_id: ChatId,
    task_id: Uuid,
) -> Result<(), sqlx::Error> {
    let topic = find_task_topic(pool, task_id).await?;
    if let Some(topic) = topic
        && let Err(err) = bot.delete_forum_topic(chat_id, topic.thread_id).await
    {
        tracing::warn!(
            task_id = %task_id,
            thread_id = topic.thread_id.0.0,
            "Failed to delete Telegram task topic: {err}"
        );
    }

    TelegramTaskTopic::clear(pool, task_id).await
}

pub fn topic_title(title: &str) -> String {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return FALLBACK_TOPIC_TITLE.to_string();
    }

    let title: String = trimmed.chars().take(TELEGRAM_TOPIC_TITLE_LIMIT).collect();
    if title.is_empty() {
        FALLBACK_TOPIC_TITLE.to_string()
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::{FALLBACK_TOPIC_TITLE, TELEGRAM_TOPIC_TITLE_LIMIT, topic_title};

    #[test]
    fn topic_title_uses_fallback_for_blank_title() {
        assert_eq!(topic_title(""), FALLBACK_TOPIC_TITLE);
        assert_eq!(topic_title(" \n\t "), FALLBACK_TOPIC_TITLE);
    }

    #[test]
    fn topic_title_truncates_ascii_to_telegram_limit() {
        let title = "a".repeat(TELEGRAM_TOPIC_TITLE_LIMIT + 10);
        let truncated = topic_title(&title);
        assert_eq!(truncated.chars().count(), TELEGRAM_TOPIC_TITLE_LIMIT);
        assert!(truncated.chars().all(|ch| ch == 'a'));
    }

    #[test]
    fn topic_title_truncates_unicode_on_char_boundaries() {
        let title = "任务".repeat(80);
        let truncated = topic_title(&title);
        assert_eq!(truncated.chars().count(), TELEGRAM_TOPIC_TITLE_LIMIT);
        assert!(truncated.ends_with('务') || truncated.ends_with('任'));
    }
}
