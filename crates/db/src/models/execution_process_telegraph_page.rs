use chrono::{DateTime, Utc};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct ExecutionProcessTelegraphPage {
    pub execution_process_id: Uuid,
    pub page_index: i64,
    pub url: String,
    pub path: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct NewExecutionProcessTelegraphPage<'a> {
    pub page_index: i64,
    pub url: &'a str,
    pub path: &'a str,
    pub title: &'a str,
}

impl ExecutionProcessTelegraphPage {
    pub async fn replace_all(
        pool: &SqlitePool,
        execution_process_id: Uuid,
        pages: &[NewExecutionProcessTelegraphPage<'_>],
    ) -> Result<(), sqlx::Error> {
        let mut tx = pool.begin().await?;
        let now = Utc::now();

        sqlx::query("DELETE FROM execution_process_telegraph_pages WHERE execution_process_id = ?")
            .bind(execution_process_id)
            .execute(&mut *tx)
            .await?;

        for page in pages {
            sqlx::query(
                r#"INSERT INTO execution_process_telegraph_pages (
                        execution_process_id, page_index, url, path, title, created_at, updated_at
                    ) VALUES (?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(execution_process_id)
            .bind(page.page_index)
            .bind(page.url)
            .bind(page.path)
            .bind(page.title)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn find_urls_by_execution_process_id(
        pool: &SqlitePool,
        execution_process_id: Uuid,
    ) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query_scalar(
            r#"SELECT url
               FROM execution_process_telegraph_pages
               WHERE execution_process_id = ?
               ORDER BY page_index ASC"#,
        )
        .bind(execution_process_id)
        .fetch_all(pool)
        .await?;

        Ok(rows)
    }
}
