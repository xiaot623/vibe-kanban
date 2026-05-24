use chrono::{DateTime, Utc};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct TelegramTaskTopic {
    pub task_id: Uuid,
    pub message_thread_id: Option<i64>,
    pub topic_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TelegramTaskTopic {
    pub async fn find_by_task_id(
        pool: &SqlitePool,
        task_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as(
            r#"SELECT task_id,
                      message_thread_id,
                      topic_name,
                      created_at,
                      updated_at
               FROM telegram_task_topics
               WHERE task_id = ?"#,
        )
        .bind(task_id)
        .fetch_optional(pool)
        .await
    }

    pub async fn find_by_message_thread_id(
        pool: &SqlitePool,
        message_thread_id: i64,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as(
            r#"SELECT task_id,
                      message_thread_id,
                      topic_name,
                      created_at,
                      updated_at
               FROM telegram_task_topics
               WHERE message_thread_id = ?"#,
        )
        .bind(message_thread_id)
        .fetch_optional(pool)
        .await
    }

    pub async fn create_empty(pool: &SqlitePool, task_id: Uuid) -> Result<Self, sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r#"INSERT INTO telegram_task_topics (
                    task_id,
                    message_thread_id,
                    topic_name,
                    created_at,
                    updated_at
               ) VALUES (?, NULL, NULL, ?, ?)
               ON CONFLICT(task_id) DO UPDATE SET updated_at = excluded.updated_at"#,
        )
        .bind(task_id)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;

        Self::find_by_task_id(pool, task_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    pub async fn bind(
        pool: &SqlitePool,
        task_id: Uuid,
        message_thread_id: i64,
        topic_name: &str,
    ) -> Result<Self, sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r#"INSERT INTO telegram_task_topics (
                    task_id,
                    message_thread_id,
                    topic_name,
                    created_at,
                    updated_at
               ) VALUES (?, ?, ?, ?, ?)
               ON CONFLICT(task_id) DO UPDATE SET
                    message_thread_id = excluded.message_thread_id,
                    topic_name = excluded.topic_name,
                    updated_at = excluded.updated_at"#,
        )
        .bind(task_id)
        .bind(message_thread_id)
        .bind(topic_name)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;

        Self::find_by_task_id(pool, task_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    pub async fn clear(pool: &SqlitePool, task_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM telegram_task_topics WHERE task_id = ?")
            .bind(task_id)
            .execute(pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;
    use uuid::Uuid;

    use super::TelegramTaskTopic;

    async fn setup_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("connect sqlite memory");
        sqlx::query(
            r#"CREATE TABLE telegram_task_topics (
                task_id BLOB PRIMARY KEY,
                message_thread_id INTEGER,
                topic_name TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
            )"#,
        )
        .execute(&pool)
        .await
        .expect("create telegram_task_topics");
        sqlx::query(
            r#"CREATE INDEX idx_telegram_task_topics_message_thread_id
               ON telegram_task_topics(message_thread_id)"#,
        )
        .execute(&pool)
        .await
        .expect("create thread index");
        pool
    }

    #[tokio::test]
    async fn task_topic_helpers_create_reuse_find_and_delete() {
        let pool = setup_pool().await;
        let task_id = Uuid::new_v4();

        let empty = TelegramTaskTopic::create_empty(&pool, task_id)
            .await
            .expect("create empty binding");
        assert_eq!(empty.task_id, task_id);
        assert_eq!(empty.message_thread_id, None);

        let bound = TelegramTaskTopic::bind(&pool, task_id, 42, "Task topic")
            .await
            .expect("bind topic");
        assert_eq!(bound.message_thread_id, Some(42));
        assert_eq!(bound.topic_name.as_deref(), Some("Task topic"));

        let reused = TelegramTaskTopic::find_by_task_id(&pool, task_id)
            .await
            .expect("find by task")
            .expect("topic exists");
        assert_eq!(reused.message_thread_id, Some(42));

        let by_thread = TelegramTaskTopic::find_by_message_thread_id(&pool, 42)
            .await
            .expect("find by thread")
            .expect("topic exists");
        assert_eq!(by_thread.task_id, task_id);

        TelegramTaskTopic::clear(&pool, task_id)
            .await
            .expect("clear topic");
        assert!(
            TelegramTaskTopic::find_by_task_id(&pool, task_id)
                .await
                .expect("find after clear")
                .is_none()
        );
    }
}
