use chrono::{DateTime, Utc};
use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, FromRow)]
pub struct ExecutionProcessLarkWikiMonthNode {
    pub space_id: String,
    pub month: String,
    pub node_token: String,
    pub obj_token: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct UpsertExecutionProcessLarkWikiMonthNode<'a> {
    pub node_token: &'a str,
    pub obj_token: &'a str,
    pub title: &'a str,
}

impl ExecutionProcessLarkWikiMonthNode {
    pub async fn upsert(
        pool: &SqlitePool,
        space_id: &str,
        month: &str,
        node: &UpsertExecutionProcessLarkWikiMonthNode<'_>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"INSERT INTO execution_process_lark_wiki_month_nodes (
                    space_id, month, node_token, obj_token, title, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(space_id, month) DO UPDATE SET
                    node_token = excluded.node_token,
                    obj_token = excluded.obj_token,
                    title = excluded.title,
                    updated_at = excluded.updated_at"#,
        )
        .bind(space_id)
        .bind(month)
        .bind(node.node_token)
        .bind(node.obj_token)
        .bind(node.title)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;

        Ok(())
    }

    pub async fn find_by_space_and_month(
        pool: &SqlitePool,
        space_id: &str,
        month: &str,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as(
            r#"SELECT space_id, month, node_token, obj_token, title, created_at, updated_at
               FROM execution_process_lark_wiki_month_nodes
               WHERE space_id = ? AND month = ?"#,
        )
        .bind(space_id)
        .bind(month)
        .fetch_optional(pool)
        .await
    }
}
