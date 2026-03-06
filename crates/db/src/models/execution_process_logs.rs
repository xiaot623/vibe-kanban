use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool};
use ts_rs::TS;
use utils::log_msg::LogMsg;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, TS)]
pub struct ExecutionProcessLogs {
    pub execution_id: Uuid,
    pub logs: String, // JSONL format
    pub msg_type: Option<String>,
    pub byte_size: i64,
    pub inserted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct SequencedExecutionProcessLog {
    pub seq: i64,
    pub execution_id: Uuid,
    pub logs: String, // JSONL format
    pub msg_type: Option<String>,
    pub byte_size: i64,
    pub inserted_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewExecutionProcessLog {
    pub logs: String, // JSONL line (with trailing newline)
    pub msg_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SequencedLogMsg {
    pub seq: i64,
    pub msg: LogMsg,
}

impl ExecutionProcessLogs {
    /// Find logs by execution process ID
    pub async fn find_by_execution_id(
        pool: &SqlitePool,
        execution_id: Uuid,
    ) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, ExecutionProcessLogs>(
            r#"SELECT
                execution_id,
                logs,
                msg_type,
                byte_size,
                inserted_at
               FROM execution_process_logs
               WHERE execution_id = ?
               ORDER BY rowid ASC"#,
        )
        .bind(execution_id)
        .fetch_all(pool)
        .await
    }

    /// Find logs by execution process ID, with optional rowid cursor.
    pub async fn find_by_execution_id_with_cursor(
        pool: &SqlitePool,
        execution_id: Uuid,
        after_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<SequencedExecutionProcessLog>, sqlx::Error> {
        Self::find_by_execution_id_with_cursor_and_type(pool, execution_id, after_seq, limit, None)
            .await
    }

    /// Find logs for one message type with optional rowid cursor.
    pub async fn find_by_execution_id_with_cursor_and_type(
        pool: &SqlitePool,
        execution_id: Uuid,
        after_seq: Option<i64>,
        limit: u32,
        msg_type: Option<&str>,
    ) -> Result<Vec<SequencedExecutionProcessLog>, sqlx::Error> {
        let mut qb = QueryBuilder::<Sqlite>::new(
            r#"SELECT
                rowid as seq,
                execution_id,
                logs,
                msg_type,
                byte_size,
                inserted_at
               FROM execution_process_logs
               WHERE execution_id = "#,
        );
        qb.push_bind(execution_id);

        if let Some(msg_type) = msg_type {
            qb.push(" AND msg_type = ");
            qb.push_bind(msg_type);
        }
        if let Some(after_seq) = after_seq {
            qb.push(" AND rowid > ");
            qb.push_bind(after_seq);
        }
        qb.push(" ORDER BY rowid ASC LIMIT ");
        qb.push_bind(i64::from(limit.max(1)));

        qb.build_query_as::<SequencedExecutionProcessLog>()
            .fetch_all(pool)
            .await
    }

    /// Checks whether there are logs for one message type.
    pub async fn has_logs_for_type(
        pool: &SqlitePool,
        execution_id: Uuid,
        msg_type: &str,
    ) -> Result<bool, sqlx::Error> {
        let exists = sqlx::query_scalar::<_, i64>(
            r#"SELECT EXISTS(
                   SELECT 1
                   FROM execution_process_logs
                   WHERE execution_id = ?
                     AND msg_type = ?
                   LIMIT 1
               )"#,
        )
        .bind(execution_id)
        .bind(msg_type)
        .fetch_one(pool)
        .await?;

        Ok(exists != 0)
    }

    /// Parse JSONL logs back into Vec<LogMsg>
    pub fn parse_logs(records: &[Self]) -> Result<Vec<LogMsg>, serde_json::Error> {
        let mut messages = Vec::new();
        for line in records.iter().flat_map(|record| record.logs.lines()) {
            if !line.trim().is_empty() {
                let msg: LogMsg = serde_json::from_str(line)?;
                messages.push(msg);
            }
        }
        Ok(messages)
    }

    /// Parse JSONL logs and preserve rowid sequence.
    pub fn parse_logs_with_seq(
        records: &[SequencedExecutionProcessLog],
    ) -> Result<Vec<SequencedLogMsg>, serde_json::Error> {
        let mut messages = Vec::new();
        for record in records {
            for line in record.logs.lines() {
                if !line.trim().is_empty() {
                    let msg: LogMsg = serde_json::from_str(line)?;
                    messages.push(SequencedLogMsg {
                        seq: record.seq,
                        msg,
                    });
                }
            }
        }
        Ok(messages)
    }

    /// Append JSONL lines to the logs for an execution process in one batch insert.
    pub async fn append_log_lines(
        pool: &SqlitePool,
        execution_id: Uuid,
        logs: &[NewExecutionProcessLog],
    ) -> Result<(), sqlx::Error> {
        if logs.is_empty() {
            return Ok(());
        }

        let mut qb = QueryBuilder::<Sqlite>::new(
            r#"INSERT INTO execution_process_logs (execution_id, logs, msg_type, byte_size) "#,
        );
        qb.push_values(logs, |mut b, log| {
            b.push_bind(execution_id)
                .push_bind(&log.logs)
                .push_bind(&log.msg_type)
                .push_bind(log.logs.len() as i64);
        });

        qb.build().execute(pool).await?;
        Ok(())
    }

    /// Append one JSONL line to the logs for an execution process.
    pub async fn append_log_line(
        pool: &SqlitePool,
        execution_id: Uuid,
        jsonl_line: &str,
        msg_type: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        Self::append_log_lines(
            pool,
            execution_id,
            &[NewExecutionProcessLog {
                logs: jsonl_line.to_string(),
                msg_type: msg_type.map(str::to_string),
            }],
        )
        .await
    }
}
