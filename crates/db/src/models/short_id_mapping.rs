use chrono::{DateTime, Duration, Utc};
use rand::Rng;
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

const SHORT_ID_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const SHORT_ID_LEN: usize = 5;
const EXPIRY_HOURS: i64 = 3;

#[derive(Debug, Clone, FromRow)]
pub struct ShortIdMapping {
    pub short_id: String,
    pub task_id: Uuid,
    pub expires_at: DateTime<Utc>,
}

impl ShortIdMapping {
    /// Resolve a short_id to a task UUID. Refreshes expiry on hit, deletes if expired.
    pub async fn resolve(pool: &SqlitePool, short_id: &str) -> Option<Uuid> {
        let row: Option<ShortIdMapping> = sqlx::query_as(
            "SELECT short_id, task_id, expires_at FROM short_id_mappings WHERE short_id = ?",
        )
        .bind(short_id)
        .fetch_optional(pool)
        .await
        .ok()?;

        let mapping = row?;
        let now = Utc::now();

        if mapping.expires_at < now {
            // Expired — delete it
            let _ = sqlx::query("DELETE FROM short_id_mappings WHERE short_id = ?")
                .bind(short_id)
                .execute(pool)
                .await;
            return None;
        }

        // Refresh expiry
        let new_expiry = now + Duration::hours(EXPIRY_HOURS);
        let _ = sqlx::query("UPDATE short_id_mappings SET expires_at = ? WHERE short_id = ?")
            .bind(new_expiry)
            .bind(short_id)
            .execute(pool)
            .await;

        Some(mapping.task_id)
    }

    /// Get existing short_id for a task, or create a new one.
    pub async fn get_or_create(pool: &SqlitePool, task_id: Uuid) -> Result<String, sqlx::Error> {
        let now = Utc::now();

        // Check for existing non-expired mapping
        let existing: Option<ShortIdMapping> = sqlx::query_as(
            "SELECT short_id, task_id, expires_at FROM short_id_mappings WHERE task_id = ? AND expires_at > ?",
        )
        .bind(task_id)
        .bind(now)
        .fetch_optional(pool)
        .await?;

        if let Some(mapping) = existing {
            // Refresh expiry
            let new_expiry = now + Duration::hours(EXPIRY_HOURS);
            sqlx::query("UPDATE short_id_mappings SET expires_at = ? WHERE short_id = ?")
                .bind(new_expiry)
                .bind(&mapping.short_id)
                .execute(pool)
                .await?;
            return Ok(mapping.short_id);
        }

        // Delete any expired mapping for this task
        sqlx::query("DELETE FROM short_id_mappings WHERE task_id = ?")
            .bind(task_id)
            .execute(pool)
            .await?;

        // Generate a new unique short_id
        let expires_at = now + Duration::hours(EXPIRY_HOURS);
        loop {
            let short_id = generate_short_id();
            let result = sqlx::query(
                "INSERT INTO short_id_mappings (short_id, task_id, expires_at) VALUES (?, ?, ?)",
            )
            .bind(&short_id)
            .bind(task_id)
            .bind(expires_at)
            .execute(pool)
            .await;

            match result {
                Ok(_) => return Ok(short_id),
                Err(sqlx::Error::Database(ref e)) if e.message().contains("UNIQUE") => {
                    // Collision on short_id, retry
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Delete all expired mappings.
    pub async fn cleanup_expired(pool: &SqlitePool) {
        let now = Utc::now();
        let _ = sqlx::query("DELETE FROM short_id_mappings WHERE expires_at < ?")
            .bind(now)
            .execute(pool)
            .await;
    }
}

fn generate_short_id() -> String {
    let mut rng = rand::thread_rng();
    (0..SHORT_ID_LEN)
        .map(|_| SHORT_ID_CHARS[rng.gen_range(0..SHORT_ID_CHARS.len())] as char)
        .collect()
}
