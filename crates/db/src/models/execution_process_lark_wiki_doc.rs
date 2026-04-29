use chrono::{DateTime, Utc};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

const PENDING_LARK_WIKI_DOC_ID: &str = "__vibe_kanban_lark_wiki_pending__";

#[derive(Debug, Clone, FromRow)]
pub struct ExecutionProcessLarkWikiDoc {
    pub execution_process_id: Uuid,
    pub doc_id: String,
    pub url: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct UpsertExecutionProcessLarkWikiDoc<'a> {
    pub doc_id: &'a str,
    pub url: &'a str,
    pub title: &'a str,
}

impl ExecutionProcessLarkWikiDoc {
    pub fn is_pending_claim(&self) -> bool {
        self.doc_id == PENDING_LARK_WIKI_DOC_ID && self.url.is_empty()
    }

    pub async fn try_claim_append(
        pool: &SqlitePool,
        execution_process_id: Uuid,
        title: &str,
    ) -> Result<bool, sqlx::Error> {
        let now = Utc::now();

        let result = sqlx::query(
            r#"INSERT INTO execution_process_lark_wiki_docs (
                    execution_process_id, doc_id, url, title, created_at, updated_at
                ) VALUES (?, ?, '', ?, ?, ?)
                ON CONFLICT(execution_process_id) DO NOTHING"#,
        )
        .bind(execution_process_id)
        .bind(PENDING_LARK_WIKI_DOC_ID)
        .bind(title)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;

        Ok(result.rows_affected() == 1)
    }

    pub async fn release_pending_claim(
        pool: &SqlitePool,
        execution_process_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"DELETE FROM execution_process_lark_wiki_docs
               WHERE execution_process_id = ?
                 AND doc_id = ?
                 AND url = ''"#,
        )
        .bind(execution_process_id)
        .bind(PENDING_LARK_WIKI_DOC_ID)
        .execute(pool)
        .await?;

        Ok(())
    }

    pub async fn upsert(
        pool: &SqlitePool,
        execution_process_id: Uuid,
        doc: &UpsertExecutionProcessLarkWikiDoc<'_>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"INSERT INTO execution_process_lark_wiki_docs (
                    execution_process_id, doc_id, url, title, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?)
                ON CONFLICT(execution_process_id) DO UPDATE SET
                    doc_id = excluded.doc_id,
                    url = excluded.url,
                    title = excluded.title,
                    updated_at = excluded.updated_at"#,
        )
        .bind(execution_process_id)
        .bind(doc.doc_id)
        .bind(doc.url)
        .bind(doc.title)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;

        Ok(())
    }

    pub async fn find_url_by_execution_process_id(
        pool: &SqlitePool,
        execution_process_id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar(
            r#"SELECT url
               FROM execution_process_lark_wiki_docs
               WHERE execution_process_id = ?"#,
        )
        .bind(execution_process_id)
        .fetch_optional(pool)
        .await
    }

    pub async fn find_by_execution_process_id(
        pool: &SqlitePool,
        execution_process_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as(
            r#"SELECT execution_process_id, doc_id, url, title, created_at, updated_at
               FROM execution_process_lark_wiki_docs
               WHERE execution_process_id = ?"#,
        )
        .bind(execution_process_id)
        .fetch_optional(pool)
        .await
    }
}
