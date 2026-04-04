use chrono::{DateTime, Duration, Utc};
use rand::Rng;
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

const FLOW_TOKEN_PREFIX: &str = "f-";
const FLOW_TOKEN_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const FLOW_TOKEN_LEN: usize = 5;
const FLOW_BINDING_EXPIRY_DAYS: i64 = 30;

#[derive(Debug, Clone, FromRow)]
pub struct TelegramFlowBinding {
    pub flow_token: String,
    pub task_id: Uuid,
    pub workspace_id: Uuid,
    pub session_id: Uuid,
    pub latest_execution_process_id: Option<Uuid>,
    pub executor_label: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct CreateTelegramFlowBinding {
    pub task_id: Uuid,
    pub workspace_id: Uuid,
    pub session_id: Uuid,
    pub latest_execution_process_id: Option<Uuid>,
    pub executor_label: String,
}

pub struct UpdateTelegramFlowBinding {
    pub workspace_id: Uuid,
    pub latest_execution_process_id: Option<Uuid>,
    pub executor_label: String,
}

impl TelegramFlowBinding {
    pub async fn resolve(pool: &SqlitePool, flow_token: &str) -> Option<Self> {
        let row: Option<Self> = sqlx::query_as(
            r#"SELECT flow_token,
                      task_id,
                      workspace_id,
                      session_id,
                      latest_execution_process_id,
                      executor_label,
                      expires_at,
                      created_at,
                      updated_at
               FROM telegram_flow_bindings
               WHERE flow_token = ?"#,
        )
        .bind(flow_token)
        .fetch_optional(pool)
        .await
        .ok()?;

        let binding = row?;
        if binding.expires_at < Utc::now() {
            let _ = sqlx::query("DELETE FROM telegram_flow_bindings WHERE flow_token = ?")
                .bind(flow_token)
                .execute(pool)
                .await;
            return None;
        }

        if Self::refresh_expiry(pool, flow_token).await.is_err() {
            tracing::debug!(flow_token, "Failed to refresh telegram flow token expiry");
        }

        Some(binding)
    }

    pub async fn find_by_session_id(
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as(
            r#"SELECT flow_token,
                      task_id,
                      workspace_id,
                      session_id,
                      latest_execution_process_id,
                      executor_label,
                      expires_at,
                      created_at,
                      updated_at
               FROM telegram_flow_bindings
               WHERE session_id = ?"#,
        )
        .bind(session_id)
        .fetch_optional(pool)
        .await
    }

    pub async fn get_or_create(
        pool: &SqlitePool,
        data: &CreateTelegramFlowBinding,
    ) -> Result<Self, sqlx::Error> {
        if let Some(existing) = Self::find_by_session_id(pool, data.session_id).await? {
            return Self::update_existing(
                pool,
                &existing.flow_token,
                &UpdateTelegramFlowBinding {
                    workspace_id: data.workspace_id,
                    latest_execution_process_id: data.latest_execution_process_id,
                    executor_label: data.executor_label.clone(),
                },
            )
            .await;
        }

        let now = Utc::now();
        let expires_at = now + Duration::days(FLOW_BINDING_EXPIRY_DAYS);

        loop {
            let flow_token = generate_flow_token();
            let result = sqlx::query(
                r#"INSERT INTO telegram_flow_bindings (
                        flow_token,
                        task_id,
                        workspace_id,
                        session_id,
                        latest_execution_process_id,
                        executor_label,
                        expires_at,
                        created_at,
                        updated_at
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(&flow_token)
            .bind(data.task_id)
            .bind(data.workspace_id)
            .bind(data.session_id)
            .bind(data.latest_execution_process_id)
            .bind(&data.executor_label)
            .bind(expires_at)
            .bind(now)
            .bind(now)
            .execute(pool)
            .await;

            match result {
                Ok(_) => {
                    return Self::find_by_session_id(pool, data.session_id)
                        .await?
                        .ok_or(sqlx::Error::RowNotFound);
                }
                Err(sqlx::Error::Database(ref e)) if e.message().contains("UNIQUE") => {
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub async fn update_existing(
        pool: &SqlitePool,
        flow_token: &str,
        update: &UpdateTelegramFlowBinding,
    ) -> Result<Self, sqlx::Error> {
        let now = Utc::now();
        let expires_at = now + Duration::days(FLOW_BINDING_EXPIRY_DAYS);
        sqlx::query(
            r#"UPDATE telegram_flow_bindings
               SET workspace_id = ?,
                   latest_execution_process_id = ?,
                   executor_label = ?,
                   expires_at = ?,
                   updated_at = ?
               WHERE flow_token = ?"#,
        )
        .bind(update.workspace_id)
        .bind(update.latest_execution_process_id)
        .bind(&update.executor_label)
        .bind(expires_at)
        .bind(now)
        .bind(flow_token)
        .execute(pool)
        .await?;

        Self::resolve(pool, flow_token)
            .await
            .ok_or(sqlx::Error::RowNotFound)
    }

    pub async fn refresh_expiry(pool: &SqlitePool, flow_token: &str) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        let expires_at = now + Duration::days(FLOW_BINDING_EXPIRY_DAYS);
        sqlx::query(
            r#"UPDATE telegram_flow_bindings
               SET expires_at = ?,
                   updated_at = ?
               WHERE flow_token = ?"#,
        )
        .bind(expires_at)
        .bind(now)
        .bind(flow_token)
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn cleanup_expired(pool: &SqlitePool) {
        let _ = sqlx::query("DELETE FROM telegram_flow_bindings WHERE expires_at < ?")
            .bind(Utc::now())
            .execute(pool)
            .await;
    }

    pub fn display_short(&self) -> &str {
        self.flow_token
            .strip_prefix(FLOW_TOKEN_PREFIX)
            .unwrap_or(self.flow_token.as_str())
    }
}

fn generate_flow_token() -> String {
    let mut rng = rand::thread_rng();
    let suffix: String = (0..FLOW_TOKEN_LEN)
        .map(|_| FLOW_TOKEN_CHARS[rng.gen_range(0..FLOW_TOKEN_CHARS.len())] as char)
        .collect();
    format!("{FLOW_TOKEN_PREFIX}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::generate_flow_token;

    #[test]
    fn generated_token_has_expected_prefix_and_length() {
        let token = generate_flow_token();
        assert!(token.starts_with("f-"));
        assert_eq!(token.len(), 7);
    }
}
