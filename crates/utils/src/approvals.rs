use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

pub const APPROVAL_TIMEOUT_SECONDS: i64 = 3600; // 1 hour

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ApprovalRequest {
    pub id: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_call_id: String,
    pub execution_process_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub timeout_at: DateTime<Utc>,
}

impl ApprovalRequest {
    pub fn from_create(request: CreateApprovalRequest, execution_process_id: Uuid) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            tool_name: request.tool_name,
            tool_input: request.tool_input,
            tool_call_id: request.tool_call_id,
            execution_process_id,
            created_at: now,
            timeout_at: now + Duration::seconds(APPROVAL_TIMEOUT_SECONDS),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateApprovalRequest {
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    ProvidedInput {
        input: serde_json::Value,
    },
    Denied {
        #[ts(optional)]
        reason: Option<String>,
    },
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ApprovalResponse {
    pub execution_process_id: Uuid,
    pub status: ApprovalStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provided_input_status_serializes_and_deserializes() {
        let status = ApprovalStatus::ProvidedInput {
            input: serde_json::json!({
                "answers": {
                    "question_1": ["option_a"]
                }
            }),
        };

        let json = serde_json::to_value(&status).expect("serialize approval status");
        assert_eq!(
            json,
            serde_json::json!({
                "status": "provided_input",
                "input": {
                    "answers": {
                        "question_1": ["option_a"]
                    }
                }
            })
        );

        let decoded: ApprovalStatus =
            serde_json::from_value(json).expect("deserialize approval status");
        match decoded {
            ApprovalStatus::ProvidedInput { input } => {
                assert_eq!(
                    input,
                    serde_json::json!({
                        "answers": {
                            "question_1": ["option_a"]
                        }
                    })
                );
            }
            other => panic!("expected provided_input status, got {other:?}"),
        }
    }
}
