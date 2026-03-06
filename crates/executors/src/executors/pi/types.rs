use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use workspace_utils::approvals::ApprovalStatus;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PiExecutorEvent {
    ModelMetadata {
        model: Option<String>,
        reasoning_effort: Option<String>,
    },
    SessionStart {
        session_id: String,
    },
    PiEvent {
        method: String,
        payload: Value,
        raw: Value,
    },
    ApprovalPending {
        tool_call_id: String,
        tool_name: String,
        tool_input: Value,
        approval_id: String,
        requested_at: chrono::DateTime<chrono::Utc>,
        timeout_at: chrono::DateTime<chrono::Utc>,
    },
    ApprovalResult {
        tool_call_id: String,
        status: ApprovalStatus,
    },
    ProtocolError {
        message: String,
    },
    Done,
}

#[derive(Debug, Clone)]
pub enum PiRpcMessage {
    Response {
        id: String,
        result: Value,
    },
    ErrorResponse {
        id: Option<String>,
        message: String,
        raw: Value,
    },
    Notification {
        method: String,
        payload: Value,
        raw: Value,
    },
    Other(Value),
}

pub fn parse_rpc_message(line: &str) -> Result<PiRpcMessage, serde_json::Error> {
    let raw = serde_json::from_str::<Value>(line)?;

    if raw.get("type").and_then(Value::as_str) == Some("response")
        && let Some(command) = raw.get("command").and_then(Value::as_str)
    {
        let response_id = raw
            .get("id")
            .and_then(value_to_id)
            .unwrap_or_else(|| command.to_string());
        let success = raw.get("success").and_then(Value::as_bool).unwrap_or(false);
        if success {
            return Ok(PiRpcMessage::Response {
                id: response_id.clone(),
                result: raw.get("data").cloned().unwrap_or(Value::Null),
            });
        }

        let message = raw
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "Unknown Pi command response error".to_string());
        return Ok(PiRpcMessage::ErrorResponse {
            id: Some(response_id),
            message,
            raw,
        });
    }

    if let Some(error) = raw.get("error")
        && !error.is_null()
        && raw.get("id").is_some()
    {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| error.as_str().map(str::to_string))
            .unwrap_or_else(|| error.to_string());
        return Ok(PiRpcMessage::ErrorResponse {
            id: raw.get("id").and_then(value_to_id),
            message,
            raw,
        });
    }

    if let Some(id) = raw.get("id").and_then(value_to_id)
        && let Some(result) = raw.get("result")
    {
        return Ok(PiRpcMessage::Response {
            id,
            result: result.clone(),
        });
    }

    if let Some((method, payload)) = extract_notification(&raw) {
        return Ok(PiRpcMessage::Notification {
            method,
            payload,
            raw,
        });
    }

    Ok(PiRpcMessage::Other(raw))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionUiRequestKind {
    Confirm,
    Select,
    Input,
    Editor,
    Custom,
}

impl ExtensionUiRequestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirm => "confirm",
            Self::Select => "select",
            Self::Input => "input",
            Self::Editor => "editor",
            Self::Custom => "custom",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "confirm" => Some(Self::Confirm),
            "select" => Some(Self::Select),
            "input" => Some(Self::Input),
            "editor" => Some(Self::Editor),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExtensionUiRequest {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub kind: ExtensionUiRequestKind,
    pub payload: Value,
}

pub fn parse_extension_ui_request(method: &str, payload: &Value) -> Option<ExtensionUiRequest> {
    if method != "extension_ui_request" {
        return None;
    }

    let kind = extract_first_string(
        payload,
        &[
            "/method",
            "/kind",
            "/type",
            "/request/type",
            "/requestType",
            "/ui_type",
            "/uiType",
            "/prompt/type",
        ],
    )
    .and_then(|raw| ExtensionUiRequestKind::parse(&raw))?;

    let request_id = extract_first_string(
        payload,
        &[
            "/request_id",
            "/requestId",
            "/id",
            "/tool_call_id",
            "/toolCallId",
            "/call_id",
            "/callId",
        ],
    )
    .unwrap_or_else(|| "extension_ui_request".to_string());

    let tool_call_id = extract_first_string(
        payload,
        &[
            "/tool_call_id",
            "/toolCallId",
            "/call_id",
            "/callId",
            "/id",
            "/request_id",
            "/requestId",
        ],
    )
    .unwrap_or_else(|| request_id.clone());

    let tool_name = extract_first_string(
        payload,
        &[
            "/tool_name",
            "/toolName",
            "/tool/name",
            "/request/tool_name",
            "/request/toolName",
        ],
    )
    .unwrap_or_else(|| format!("extension_ui_{}", kind.as_str()));

    Some(ExtensionUiRequest {
        request_id,
        tool_call_id,
        tool_name,
        kind,
        payload: payload.clone(),
    })
}

pub fn approval_status_to_extension_ui_response(
    request: &ExtensionUiRequest,
    status: &ApprovalStatus,
) -> Value {
    match status {
        ApprovalStatus::ProvidedInput { input } => {
            if input.is_object() {
                input.clone()
            } else {
                json!({ "value": input })
            }
        }
        ApprovalStatus::Approved => match request.kind {
            ExtensionUiRequestKind::Confirm => json!({ "confirmed": true }),
            ExtensionUiRequestKind::Select => {
                if let Some(value) = first_select_option_value(&request.payload) {
                    json!({ "value": value })
                } else {
                    json!({ "cancelled": true })
                }
            }
            ExtensionUiRequestKind::Input | ExtensionUiRequestKind::Editor => {
                json!({ "value": "" })
            }
            ExtensionUiRequestKind::Custom => json!({ "cancelled": true }),
        },
        ApprovalStatus::Denied { .. } | ApprovalStatus::TimedOut | ApprovalStatus::Pending => {
            json!({ "cancelled": true })
        }
    }
}

pub fn extract_session_file_from_state(value: &Value) -> Option<String> {
    extract_first_string(
        value,
        &[
            "/sessionFile",
            "/session_file",
            "/state/sessionFile",
            "/state/session_file",
            "/sessionId",
            "/session_id",
            "/state/sessionId",
            "/state/session_id",
        ],
    )
}

pub fn extract_agent_end_error(payload: &Value) -> Option<String> {
    let error = payload
        .pointer("/error")
        .or_else(|| payload.pointer("/agent_end/error"))?;

    if error.is_null() {
        return None;
    }

    if let Some(message) = error.as_str() {
        let message = message.trim();
        return (!message.is_empty()).then(|| message.to_string());
    }

    if let Some(message) = error.get("message").and_then(Value::as_str) {
        let message = message.trim();
        return (!message.is_empty()).then(|| message.to_string());
    }

    Some(error.to_string())
}

fn extract_notification(raw: &Value) -> Option<(String, Value)> {
    if let Some(method) = raw
        .get("type")
        .and_then(Value::as_str)
        .filter(|method| *method != "response")
        .or_else(|| raw.get("event").and_then(Value::as_str))
    {
        let payload = raw
            .get("payload")
            .cloned()
            .or_else(|| raw.get("params").cloned())
            .or_else(|| raw.get("data").cloned())
            .unwrap_or_else(|| {
                if method == "extension_ui_request" {
                    raw.clone()
                } else {
                    stripped_object_payload(raw)
                }
            });
        return Some((method.to_string(), payload));
    }

    if let Some(method) = raw.get("method").and_then(Value::as_str) {
        let payload = raw
            .get("params")
            .cloned()
            .or_else(|| raw.get("payload").cloned())
            .or_else(|| raw.get("data").cloned())
            .unwrap_or(Value::Null);
        return Some((method.to_string(), payload));
    }

    None
}

fn first_select_option_value(payload: &Value) -> Option<Value> {
    let options = payload.pointer("/options")?.as_array()?;

    for option in options {
        match option {
            Value::String(value) => return Some(Value::String(value.clone())),
            Value::Object(map) => {
                if let Some(value) = map.get("value") {
                    return Some(value.clone());
                }
                if let Some(label) = map
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                {
                    return Some(Value::String(label.to_string()));
                }
            }
            _ => {}
        }
    }

    None
}

fn stripped_object_payload(raw: &Value) -> Value {
    let mut payload = raw.clone();
    if let Value::Object(map) = &mut payload {
        map.remove("jsonrpc");
        map.remove("id");
        map.remove("method");
        map.remove("type");
        map.remove("event");
    }
    payload
}

fn value_to_id(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn extract_first_string(value: &Value, pointers: &[&str]) -> Option<String> {
    pointers.iter().find_map(|pointer| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_rpc_response_message() {
        let message = parse_rpc_message(r#"{"jsonrpc":"2.0","id":2,"result":{"ok":true}}"#)
            .expect("response should parse");

        let PiRpcMessage::Response { id, result } = message else {
            panic!("expected response");
        };

        assert_eq!(id, "2");
        assert_eq!(result.pointer("/ok").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn parse_rpc_notification_message() {
        let message = parse_rpc_message(
            r#"{"jsonrpc":"2.0","method":"message_update","params":{"assistantMessageEvent":{"text_delta":"Hi"}}}"#,
        )
        .expect("notification should parse");

        let PiRpcMessage::Notification {
            method, payload, ..
        } = message
        else {
            panic!("expected notification");
        };

        assert_eq!(method, "message_update");
        assert_eq!(
            payload.pointer("/assistantMessageEvent/text_delta"),
            Some(&Value::String("Hi".to_string()))
        );
    }

    #[test]
    fn parse_pi_command_response_message() {
        let message = parse_rpc_message(
            r#"{"type":"response","command":"get_state","success":true,"data":{"ok":true}}"#,
        )
        .expect("response should parse");

        let PiRpcMessage::Response { id, result } = message else {
            panic!("expected response");
        };

        assert_eq!(id, "get_state");
        assert_eq!(result.pointer("/ok").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn parse_pi_command_response_prefers_response_id() {
        let message = parse_rpc_message(
            r#"{"id":"rpc-1","type":"response","command":"get_state","success":true,"data":{"ok":true}}"#,
        )
        .expect("response should parse");

        let PiRpcMessage::Response { id, .. } = message else {
            panic!("expected response");
        };

        assert_eq!(id, "rpc-1");
    }

    #[test]
    fn parse_agent_end_with_error_as_notification() {
        let message = parse_rpc_message(
            r#"{"type":"agent_end","error":{"message":"tool failed"},"messages":[]}"#,
        )
        .expect("agent_end should parse");

        let PiRpcMessage::Notification {
            method, payload, ..
        } = message
        else {
            panic!("expected notification");
        };

        assert_eq!(method, "agent_end");
        assert_eq!(
            payload.pointer("/error/message").and_then(Value::as_str),
            Some("tool failed")
        );
    }

    #[test]
    fn parse_extension_ui_request_confirm() {
        let payload = json!({
            "id": "req-1",
            "tool_name": "bash",
            "type": "confirm",
        });

        let parsed = parse_extension_ui_request("extension_ui_request", &payload)
            .expect("should parse extension ui request");
        assert_eq!(parsed.request_id, "req-1");
        assert_eq!(parsed.tool_call_id, "req-1");
        assert_eq!(parsed.tool_name, "bash");
        assert_eq!(parsed.kind, ExtensionUiRequestKind::Confirm);
    }

    #[test]
    fn parse_extension_ui_request_notification_payload() {
        let message = parse_rpc_message(
            r#"{"type":"extension_ui_request","id":"req-1","method":"confirm","title":"Allow?"}"#,
        )
        .expect("extension ui request should parse");

        let PiRpcMessage::Notification {
            method, payload, ..
        } = message
        else {
            panic!("expected notification");
        };

        assert_eq!(method, "extension_ui_request");

        let parsed = parse_extension_ui_request(&method, &payload)
            .expect("should parse extension ui request payload");
        assert_eq!(parsed.request_id, "req-1");
        assert_eq!(parsed.kind, ExtensionUiRequestKind::Confirm);
    }

    #[test]
    fn approval_mapping_for_denied_status() {
        let request = ExtensionUiRequest {
            request_id: "req-1".to_string(),
            tool_call_id: "tool-1".to_string(),
            tool_name: "bash".to_string(),
            kind: ExtensionUiRequestKind::Confirm,
            payload: json!({}),
        };

        let response = approval_status_to_extension_ui_response(
            &request,
            &ApprovalStatus::Denied { reason: None },
        );
        assert_eq!(response.pointer("/cancelled"), Some(&Value::Bool(true)));
    }

    #[test]
    fn approval_mapping_for_confirm_approved_status() {
        let request = ExtensionUiRequest {
            request_id: "req-1".to_string(),
            tool_call_id: "tool-1".to_string(),
            tool_name: "bash".to_string(),
            kind: ExtensionUiRequestKind::Confirm,
            payload: json!({}),
        };

        let response =
            approval_status_to_extension_ui_response(&request, &ApprovalStatus::Approved);
        assert_eq!(response.pointer("/confirmed"), Some(&Value::Bool(true)));
    }

    #[test]
    fn approval_mapping_for_select_approved_uses_first_option() {
        let request = ExtensionUiRequest {
            request_id: "req-1".to_string(),
            tool_call_id: "tool-1".to_string(),
            tool_name: "bash".to_string(),
            kind: ExtensionUiRequestKind::Select,
            payload: json!({
                "options": [
                    {"label": "A", "value": "alpha"},
                    {"label": "B", "value": "beta"}
                ]
            }),
        };

        let response =
            approval_status_to_extension_ui_response(&request, &ApprovalStatus::Approved);
        assert_eq!(
            response.pointer("/value").and_then(Value::as_str),
            Some("alpha")
        );
    }

    #[test]
    fn extract_session_file_prefers_session_file() {
        let state = json!({
            "sessionFile": "/tmp/pi-session.json",
            "sessionId": "session-1"
        });
        assert_eq!(
            extract_session_file_from_state(&state),
            Some("/tmp/pi-session.json".to_string())
        );
    }
}
