//! Callback action encoding/decoding for Telegram inline keyboard buttons.
//!
//! Format: `v1|<action>|<arg1>|<arg2>`
//! Telegram callback_data limit is 64 bytes, so we keep payloads compact.

use std::fmt;

use uuid::Uuid;

/// Actions that can be triggered by inline keyboard buttons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackAction {
    /// Show the home/start screen
    Home,
    /// List projects (for browsing tasks)
    Projects,
    /// List tasks for a project, optionally filtered by status
    Tasks {
        project_id: Uuid,
        status: Option<String>,
    },
    /// Show task details
    TaskDetail { task_id: Uuid },
    /// Run a task with default executor
    RunDefault { task_id: Uuid },
    /// Show executor selection for a task
    RunPick { task_id: Uuid },
    /// Run a task with a specific executor
    RunWith { task_id: Uuid, executor: String },
    /// Run a task with specific executor + mode index
    RunWithMode {
        task_id: Uuid,
        executor: String,
        mode_index: u16,
    },
    /// Start editing a task (enters dialogue)
    EditTask { task_id: Uuid },
    /// Approve plan — first click (confirmation prompt)
    ApproveConfirm { task_id: Uuid },
    /// Approve plan — confirmed
    ApproveYes { task_id: Uuid },
    /// Reject plan — enter reason input (enters dialogue)
    RejectInput { task_id: Uuid },
    /// Approve tool execution request by approval id
    ToolApprove { approval_id: String },
    /// Reject tool execution request by approval id
    ToolReject { approval_id: String },
    /// Show pending approvals list
    Pending,
    /// Start new task creation — pick project (enters dialogue)
    NewTask,
    /// Select project for new task creation (dialogue step)
    NewTaskProject { project_id: Uuid },
    /// Refresh / reload current view
    Refresh { task_id: Uuid },
    /// Send a follow-up reply for a task (opens text input dialogue)
    FollowUpReply { task_id: Uuid },
    /// Mark a Daily task as Done (attempt merge first when needed)
    DoneTask { task_id: Uuid },
    /// Pagination for task lists
    TaskPage { project_id: Uuid, page: u16 },
    /// Cancel current dialogue and go home
    Cancel,
    /// Skip current optional dialogue input
    Skip,
    /// No-op acknowledgement (for already-handled buttons)
    Noop,
}

/// Short action tags used in the wire format.
impl CallbackAction {
    fn tag(&self) -> &'static str {
        match self {
            Self::Home => "h",
            Self::Projects => "p",
            Self::Tasks { .. } => "ts",
            Self::TaskDetail { .. } => "td",
            Self::RunDefault { .. } => "rd",
            Self::RunPick { .. } => "rp",
            Self::RunWith { .. } => "rw",
            Self::RunWithMode { .. } => "rm",
            Self::EditTask { .. } => "et",
            Self::ApproveConfirm { .. } => "ac",
            Self::ApproveYes { .. } => "ay",
            Self::RejectInput { .. } => "ri",
            Self::ToolApprove { .. } => "ta",
            Self::ToolReject { .. } => "tr",
            Self::Pending => "pe",
            Self::NewTask => "nt",
            Self::NewTaskProject { .. } => "np",
            Self::Refresh { .. } => "rf",
            Self::FollowUpReply { .. } => "fr",
            Self::DoneTask { .. } => "fd",
            Self::TaskPage { .. } => "tp",
            Self::Cancel => "ca",
            Self::Skip => "sk",
            Self::Noop => "no",
        }
    }

    /// Encode to callback_data string (must be <= 64 bytes).
    pub fn encode(&self) -> String {
        match self {
            Self::Home
            | Self::Projects
            | Self::Pending
            | Self::NewTask
            | Self::Cancel
            | Self::Skip
            | Self::Noop => {
                format!("v1|{}", self.tag())
            }
            Self::Tasks { project_id, status } => {
                let id = short_uuid(project_id);
                match status {
                    Some(s) => format!("v1|{}|{}|{}", self.tag(), id, s),
                    None => format!("v1|{}|{}", self.tag(), id),
                }
            }
            Self::TaskDetail { task_id }
            | Self::RunDefault { task_id }
            | Self::RunPick { task_id }
            | Self::EditTask { task_id }
            | Self::ApproveConfirm { task_id }
            | Self::ApproveYes { task_id }
            | Self::RejectInput { task_id }
            | Self::Refresh { task_id }
            | Self::FollowUpReply { task_id }
            | Self::DoneTask { task_id } => {
                format!("v1|{}|{}", self.tag(), short_uuid(task_id))
            }
            Self::ToolApprove { approval_id } | Self::ToolReject { approval_id } => {
                format!("v1|{}|{}", self.tag(), approval_id)
            }
            Self::RunWith { task_id, executor } => {
                format!("v1|{}|{}|{}", self.tag(), short_uuid(task_id), executor)
            }
            Self::RunWithMode {
                task_id,
                executor,
                mode_index,
            } => {
                let executor_code = encode_executor_code(executor).unwrap_or(executor);
                format!(
                    "v1|{}|{}|{}|{}",
                    self.tag(),
                    short_uuid(task_id),
                    executor_code,
                    mode_index
                )
            }
            Self::NewTaskProject { project_id } => {
                format!("v1|{}|{}", self.tag(), short_uuid(project_id))
            }
            Self::TaskPage { project_id, page } => {
                format!("v1|{}|{}|{}", self.tag(), short_uuid(project_id), page)
            }
        }
    }

    /// Decode from callback_data string.
    pub fn decode(data: &str) -> Option<Self> {
        let parts: Vec<&str> = data.split('|').collect();
        if parts.len() < 2 || parts[0] != "v1" {
            return None;
        }

        let tag = parts[1];
        match tag {
            "h" => Some(Self::Home),
            "p" => Some(Self::Projects),
            "pe" => Some(Self::Pending),
            "nt" => Some(Self::NewTask),
            "ca" => Some(Self::Cancel),
            "sk" => Some(Self::Skip),
            "no" => Some(Self::Noop),
            "ts" => {
                let project_id = parse_short_uuid(parts.get(2)?)?;
                let status = parts.get(3).map(|s| s.to_string());
                Some(Self::Tasks { project_id, status })
            }
            "td" => Some(Self::TaskDetail {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "rd" => Some(Self::RunDefault {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "rp" => Some(Self::RunPick {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "rw" => {
                let task_id = parse_short_uuid(parts.get(2)?)?;
                let executor = parts.get(3)?.to_string();
                Some(Self::RunWith { task_id, executor })
            }
            "rm" => {
                let task_id = parse_short_uuid(parts.get(2)?)?;
                let executor_raw = parts.get(3)?;
                let executor = decode_executor_code(executor_raw)
                    .unwrap_or(*executor_raw)
                    .to_string();
                let mode_index: u16 = parts.get(4)?.parse().ok()?;
                Some(Self::RunWithMode {
                    task_id,
                    executor,
                    mode_index,
                })
            }
            "et" => Some(Self::EditTask {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "ac" => Some(Self::ApproveConfirm {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "ay" => Some(Self::ApproveYes {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "ri" => Some(Self::RejectInput {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "ta" => Some(Self::ToolApprove {
                approval_id: parts.get(2)?.to_string(),
            }),
            "tr" => Some(Self::ToolReject {
                approval_id: parts.get(2)?.to_string(),
            }),
            "np" => Some(Self::NewTaskProject {
                project_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "rf" => Some(Self::Refresh {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "fr" => Some(Self::FollowUpReply {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "fd" => Some(Self::DoneTask {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "tp" => {
                let project_id = parse_short_uuid(parts.get(2)?)?;
                let page: u16 = parts.get(3)?.parse().ok()?;
                Some(Self::TaskPage { project_id, page })
            }
            _ => None,
        }
    }
}

impl fmt::Display for CallbackAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.encode())
    }
}

/// Encode a UUID to a compact hex string (no hyphens).
fn short_uuid(id: &Uuid) -> String {
    id.simple().to_string()
}

/// Parse a compact hex UUID string back to a Uuid.
fn parse_short_uuid(s: &str) -> Option<Uuid> {
    Uuid::parse_str(s).ok()
}

fn encode_executor_code(executor: &str) -> Option<&'static str> {
    match executor {
        "CLAUDE_CODE" => Some("c"),
        "CODEX" => Some("x"),
        "GEMINI" => Some("g"),
        "OPENCODE" => Some("o"),
        "DROID" => Some("d"),
        "PI" => Some("i"),
        _ => None,
    }
}

fn decode_executor_code(code: &str) -> Option<&'static str> {
    match code {
        "c" => Some("CLAUDE_CODE"),
        "x" => Some("CODEX"),
        "g" => Some("GEMINI"),
        "o" => Some("OPENCODE"),
        "d" => Some("DROID"),
        "i" => Some("PI"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_simple_actions() {
        for action in [
            CallbackAction::Home,
            CallbackAction::Projects,
            CallbackAction::Pending,
            CallbackAction::NewTask,
            CallbackAction::Cancel,
            CallbackAction::Skip,
            CallbackAction::Noop,
        ] {
            let encoded = action.encode();
            assert!(encoded.len() <= 64, "encoded too long: {encoded}");
            let decoded = CallbackAction::decode(&encoded).expect("decode failed");
            assert_eq!(decoded, action);
        }
    }

    #[test]
    fn roundtrip_uuid_actions() {
        let id = Uuid::new_v4();
        let actions = vec![
            CallbackAction::TaskDetail { task_id: id },
            CallbackAction::RunDefault { task_id: id },
            CallbackAction::RunPick { task_id: id },
            CallbackAction::EditTask { task_id: id },
            CallbackAction::ApproveConfirm { task_id: id },
            CallbackAction::ApproveYes { task_id: id },
            CallbackAction::RejectInput { task_id: id },
            CallbackAction::ToolApprove {
                approval_id: Uuid::new_v4().to_string(),
            },
            CallbackAction::ToolReject {
                approval_id: Uuid::new_v4().to_string(),
            },
            CallbackAction::Refresh { task_id: id },
            CallbackAction::FollowUpReply { task_id: id },
            CallbackAction::DoneTask { task_id: id },
            CallbackAction::NewTaskProject { project_id: id },
        ];
        for action in actions {
            let encoded = action.encode();
            assert!(encoded.len() <= 64, "encoded too long: {encoded}");
            let decoded = CallbackAction::decode(&encoded).expect("decode failed");
            assert_eq!(decoded, action);
        }
    }

    #[test]
    fn roundtrip_tasks_with_status() {
        let id = Uuid::new_v4();
        let action = CallbackAction::Tasks {
            project_id: id,
            status: Some("todo".to_string()),
        };
        let encoded = action.encode();
        assert!(encoded.len() <= 64);
        assert_eq!(CallbackAction::decode(&encoded).unwrap(), action);
    }

    #[test]
    fn roundtrip_tasks_without_status() {
        let id = Uuid::new_v4();
        let action = CallbackAction::Tasks {
            project_id: id,
            status: None,
        };
        let encoded = action.encode();
        assert!(encoded.len() <= 64);
        assert_eq!(CallbackAction::decode(&encoded).unwrap(), action);
    }

    #[test]
    fn roundtrip_run_with_executor() {
        let id = Uuid::new_v4();
        let action = CallbackAction::RunWith {
            task_id: id,
            executor: "CLAUDE_CODE".to_string(),
        };
        let encoded = action.encode();
        assert!(encoded.len() <= 64);
        assert_eq!(CallbackAction::decode(&encoded).unwrap(), action);
    }

    #[test]
    fn roundtrip_run_with_mode() {
        let id = Uuid::new_v4();
        let action = CallbackAction::RunWithMode {
            task_id: id,
            executor: "CLAUDE_CODE".to_string(),
            mode_index: 12,
        };
        let encoded = action.encode();
        assert!(encoded.len() <= 64);
        assert_eq!(CallbackAction::decode(&encoded).unwrap(), action);
    }

    #[test]
    fn roundtrip_task_page() {
        let id = Uuid::new_v4();
        let action = CallbackAction::TaskPage {
            project_id: id,
            page: 3,
        };
        let encoded = action.encode();
        assert!(encoded.len() <= 64);
        assert_eq!(CallbackAction::decode(&encoded).unwrap(), action);
    }

    #[test]
    fn decode_invalid_returns_none() {
        assert!(CallbackAction::decode("").is_none());
        assert!(CallbackAction::decode("v2|h").is_none());
        assert!(CallbackAction::decode("v1|unknown_action").is_none());
        assert!(CallbackAction::decode("garbage").is_none());
        assert!(CallbackAction::decode("v1|td|not-a-uuid").is_none());
    }

    #[test]
    fn all_encodings_within_64_bytes() {
        let id = Uuid::new_v4();
        let all_actions = vec![
            CallbackAction::Home,
            CallbackAction::Projects,
            CallbackAction::Pending,
            CallbackAction::NewTask,
            CallbackAction::Cancel,
            CallbackAction::Skip,
            CallbackAction::Noop,
            CallbackAction::Tasks {
                project_id: id,
                status: Some("inprogress".to_string()),
            },
            CallbackAction::TaskDetail { task_id: id },
            CallbackAction::RunDefault { task_id: id },
            CallbackAction::RunPick { task_id: id },
            CallbackAction::RunWith {
                task_id: id,
                executor: "CLAUDE_CODE".to_string(),
            },
            CallbackAction::RunWithMode {
                task_id: id,
                executor: "CLAUDE_CODE".to_string(),
                mode_index: 7,
            },
            CallbackAction::EditTask { task_id: id },
            CallbackAction::ApproveConfirm { task_id: id },
            CallbackAction::ApproveYes { task_id: id },
            CallbackAction::RejectInput { task_id: id },
            CallbackAction::ToolApprove {
                approval_id: id.to_string(),
            },
            CallbackAction::ToolReject {
                approval_id: id.to_string(),
            },
            CallbackAction::Refresh { task_id: id },
            CallbackAction::FollowUpReply { task_id: id },
            CallbackAction::DoneTask { task_id: id },
            CallbackAction::NewTaskProject { project_id: id },
            CallbackAction::TaskPage {
                project_id: id,
                page: 999,
            },
        ];
        for action in all_actions {
            let encoded = action.encode();
            assert!(
                encoded.len() <= 64,
                "Action {:?} encoded to {} bytes: {encoded}",
                action,
                encoded.len()
            );
        }
    }
}
