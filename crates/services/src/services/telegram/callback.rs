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
    TaskDetail {
        task_id: Uuid,
    },
    /// Run a task with default executor
    RunDefault {
        task_id: Uuid,
    },
    /// Show executor selection for a task
    RunPick {
        task_id: Uuid,
    },
    /// Run a task with a specific executor
    RunWith {
        task_id: Uuid,
        executor: String,
    },
    /// Run a task with specific executor + mode index
    RunWithMode {
        task_id: Uuid,
        executor: String,
        mode_index: u16,
    },
    /// Start editing a task (enters dialogue)
    EditTask {
        task_id: Uuid,
    },
    /// Approve plan — first click (confirmation prompt) from task detail
    ApproveConfirm {
        task_id: Uuid,
    },
    /// Approve plan — confirmed from task detail
    ApproveYes {
        task_id: Uuid,
    },
    /// Reject plan — enter reason input (enters dialogue) from task detail
    RejectInput {
        task_id: Uuid,
    },
    /// Approve plan — first click (confirmation prompt) for a flow-aware card
    FlowApproveConfirm {
        flow_token: String,
    },
    /// Approve plan — confirmed for a flow-aware card
    FlowApproveYes {
        flow_token: String,
    },
    /// Reject plan — enter reason input for a flow-aware card
    FlowRejectInput {
        flow_token: String,
    },
    /// Dismiss a temporary interaction message
    DismissInteraction,
    /// Approve tool execution request by approval id
    ToolApprove {
        approval_id: String,
    },
    /// Reject tool execution request by approval id
    ToolReject {
        approval_id: String,
    },
    /// Show pending approvals list
    Pending,
    /// Start new task creation — pick project (enters dialogue)
    NewTask,
    /// Select project for new task creation (dialogue step)
    NewTaskProject {
        project_id: Uuid,
    },
    /// Refresh / reload current view
    Refresh {
        task_id: Uuid,
    },
    /// Send a follow-up reply for a task (opens text input dialogue)
    FollowUpReply {
        flow_token: String,
    },
    /// Create a review subtask for this task
    CreateReviewTask {
        flow_token: String,
    },
    /// Create a review subtask after explicit confirmation
    CreateReviewTaskConfirm {
        flow_token: String,
    },
    /// Mark a Daily task as Done (attempt merge first when needed)
    DoneTask {
        flow_token: String,
    },
    /// Legacy flow-unaware follow-up action encoded with task id.
    LegacyFollowUpReplyTask {
        task_id: Uuid,
    },
    /// Legacy flow-unaware review action encoded with task id.
    LegacyCreateReviewTask {
        task_id: Uuid,
    },
    /// Legacy flow-unaware review confirmation encoded with task id.
    LegacyCreateReviewTaskConfirm {
        task_id: Uuid,
    },
    /// Legacy flow-unaware done action encoded with task id.
    LegacyDoneTask {
        task_id: Uuid,
    },
    /// Legacy flow-unaware plan approval actions encoded with task id.
    LegacyFlowApproveConfirmTask {
        task_id: Uuid,
    },
    LegacyFlowApproveYesTask {
        task_id: Uuid,
    },
    LegacyFlowRejectInputTask {
        task_id: Uuid,
    },
    /// Pagination for task lists
    TaskPage {
        project_id: Uuid,
        page: u16,
    },
    /// Cancel current dialogue and go home
    Cancel,
    /// Skip current optional dialogue input
    Skip,
    /// No-op acknowledgement (for already-handled buttons)
    Noop,
    /// Show project picker for pinning
    PinMenu,
    /// Unpin the currently pinned project
    Unpin,
    /// Select a project to pin
    PinProject {
        project_id: Uuid,
    },
    /// Select a duration (in minutes) for the pinned project
    PinDuration {
        project_id: Uuid,
        minutes: u32,
    },
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
            Self::FlowApproveConfirm { .. } => "af",
            Self::FlowApproveYes { .. } => "ag",
            Self::FlowRejectInput { .. } => "ah",
            Self::DismissInteraction => "di",
            Self::ToolApprove { .. } => "ta",
            Self::ToolReject { .. } => "tr",
            Self::Pending => "pe",
            Self::NewTask => "nt",
            Self::NewTaskProject { .. } => "np",
            Self::Refresh { .. } => "rf",
            Self::FollowUpReply { .. } => "fr",
            Self::CreateReviewTask { .. } => "rv",
            Self::CreateReviewTaskConfirm { .. } => "rc",
            Self::DoneTask { .. } => "fd",
            Self::LegacyFollowUpReplyTask { .. } => "frl",
            Self::LegacyCreateReviewTask { .. } => "rvl",
            Self::LegacyCreateReviewTaskConfirm { .. } => "rcl",
            Self::LegacyDoneTask { .. } => "fdl",
            Self::LegacyFlowApproveConfirmTask { .. } => "acl",
            Self::LegacyFlowApproveYesTask { .. } => "ayl",
            Self::LegacyFlowRejectInputTask { .. } => "ril",
            Self::TaskPage { .. } => "tp",
            Self::Cancel => "ca",
            Self::Skip => "sk",
            Self::Noop => "no",
            Self::PinMenu => "pm",
            Self::Unpin => "pu",
            Self::PinProject { .. } => "pp",
            Self::PinDuration { .. } => "pd",
        }
    }

    /// Encode to callback_data string (must be <= 64 bytes).
    pub fn encode(&self) -> String {
        match self {
            Self::Home
            | Self::Projects
            | Self::Pending
            | Self::NewTask
            | Self::DismissInteraction
            | Self::Cancel
            | Self::Skip
            | Self::Noop
            | Self::PinMenu
            | Self::Unpin => {
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
            | Self::LegacyFollowUpReplyTask { task_id }
            | Self::LegacyCreateReviewTask { task_id }
            | Self::LegacyCreateReviewTaskConfirm { task_id }
            | Self::LegacyDoneTask { task_id }
            | Self::LegacyFlowApproveConfirmTask { task_id }
            | Self::LegacyFlowApproveYesTask { task_id }
            | Self::LegacyFlowRejectInputTask { task_id } => {
                format!("v1|{}|{}", self.tag(), short_uuid(task_id))
            }
            Self::FlowApproveConfirm { flow_token }
            | Self::FlowApproveYes { flow_token }
            | Self::FlowRejectInput { flow_token }
            | Self::FollowUpReply { flow_token }
            | Self::CreateReviewTask { flow_token }
            | Self::CreateReviewTaskConfirm { flow_token }
            | Self::DoneTask { flow_token } => {
                format!("v1|{}|{}", self.tag(), flow_token)
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
            Self::PinProject { project_id } => {
                format!("v1|{}|{}", self.tag(), short_uuid(project_id))
            }
            Self::PinDuration {
                project_id,
                minutes,
            } => {
                format!("v1|{}|{}|{}", self.tag(), short_uuid(project_id), minutes)
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
            "di" => Some(Self::DismissInteraction),
            "ca" => Some(Self::Cancel),
            "sk" => Some(Self::Skip),
            "no" => Some(Self::Noop),
            "pm" => Some(Self::PinMenu),
            "pu" => Some(Self::Unpin),
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
            "af" => Some(Self::FlowApproveConfirm {
                flow_token: parts.get(2)?.to_string(),
            }),
            "ag" => Some(Self::FlowApproveYes {
                flow_token: parts.get(2)?.to_string(),
            }),
            "ah" => Some(Self::FlowRejectInput {
                flow_token: parts.get(2)?.to_string(),
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
                flow_token: parts.get(2)?.to_string(),
            }),
            "rv" => Some(Self::CreateReviewTask {
                flow_token: parts.get(2)?.to_string(),
            }),
            "rc" => Some(Self::CreateReviewTaskConfirm {
                flow_token: parts.get(2)?.to_string(),
            }),
            "fd" => Some(Self::DoneTask {
                flow_token: parts.get(2)?.to_string(),
            }),
            "frl" => Some(Self::LegacyFollowUpReplyTask {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "rvl" => Some(Self::LegacyCreateReviewTask {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "rcl" => Some(Self::LegacyCreateReviewTaskConfirm {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "fdl" => Some(Self::LegacyDoneTask {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "acl" => Some(Self::LegacyFlowApproveConfirmTask {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "ayl" => Some(Self::LegacyFlowApproveYesTask {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "ril" => Some(Self::LegacyFlowRejectInputTask {
                task_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "tp" => {
                let project_id = parse_short_uuid(parts.get(2)?)?;
                let page: u16 = parts.get(3)?.parse().ok()?;
                Some(Self::TaskPage { project_id, page })
            }
            "pp" => Some(Self::PinProject {
                project_id: parse_short_uuid(parts.get(2)?)?,
            }),
            "pd" => {
                let project_id = parse_short_uuid(parts.get(2)?)?;
                let minutes: u32 = parts.get(3)?.parse().ok()?;
                Some(Self::PinDuration {
                    project_id,
                    minutes,
                })
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
            CallbackAction::DismissInteraction,
            CallbackAction::Cancel,
            CallbackAction::Skip,
            CallbackAction::Noop,
            CallbackAction::PinMenu,
            CallbackAction::Unpin,
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
        let flow_token = "f-ab12c".to_string();
        let actions = vec![
            CallbackAction::TaskDetail { task_id: id },
            CallbackAction::RunDefault { task_id: id },
            CallbackAction::RunPick { task_id: id },
            CallbackAction::EditTask { task_id: id },
            CallbackAction::ApproveConfirm { task_id: id },
            CallbackAction::ApproveYes { task_id: id },
            CallbackAction::RejectInput { task_id: id },
            CallbackAction::FlowApproveConfirm {
                flow_token: flow_token.clone(),
            },
            CallbackAction::FlowApproveYes {
                flow_token: flow_token.clone(),
            },
            CallbackAction::FlowRejectInput {
                flow_token: flow_token.clone(),
            },
            CallbackAction::ToolApprove {
                approval_id: Uuid::new_v4().to_string(),
            },
            CallbackAction::ToolReject {
                approval_id: Uuid::new_v4().to_string(),
            },
            CallbackAction::Refresh { task_id: id },
            CallbackAction::FollowUpReply {
                flow_token: flow_token.clone(),
            },
            CallbackAction::CreateReviewTask {
                flow_token: flow_token.clone(),
            },
            CallbackAction::CreateReviewTaskConfirm {
                flow_token: flow_token.clone(),
            },
            CallbackAction::DoneTask {
                flow_token: flow_token.clone(),
            },
            CallbackAction::NewTaskProject { project_id: id },
            CallbackAction::LegacyFollowUpReplyTask { task_id: id },
            CallbackAction::LegacyCreateReviewTask { task_id: id },
            CallbackAction::LegacyCreateReviewTaskConfirm { task_id: id },
            CallbackAction::LegacyDoneTask { task_id: id },
            CallbackAction::LegacyFlowApproveConfirmTask { task_id: id },
            CallbackAction::LegacyFlowApproveYesTask { task_id: id },
            CallbackAction::LegacyFlowRejectInputTask { task_id: id },
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
    fn roundtrip_create_review_task() {
        let action = CallbackAction::CreateReviewTask {
            flow_token: "f-ab12c".to_string(),
        };
        let encoded = action.encode();
        assert!(encoded.len() <= 64);
        assert_eq!(CallbackAction::decode(&encoded).unwrap(), action);
    }

    #[test]
    fn roundtrip_create_review_task_confirm() {
        let action = CallbackAction::CreateReviewTaskConfirm {
            flow_token: "f-ab12c".to_string(),
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
        let flow_token = "f-ab12c".to_string();
        let all_actions = vec![
            CallbackAction::Home,
            CallbackAction::Projects,
            CallbackAction::Pending,
            CallbackAction::NewTask,
            CallbackAction::DismissInteraction,
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
            CallbackAction::FlowApproveConfirm {
                flow_token: flow_token.clone(),
            },
            CallbackAction::FlowApproveYes {
                flow_token: flow_token.clone(),
            },
            CallbackAction::FlowRejectInput {
                flow_token: flow_token.clone(),
            },
            CallbackAction::ToolApprove {
                approval_id: id.to_string(),
            },
            CallbackAction::ToolReject {
                approval_id: id.to_string(),
            },
            CallbackAction::Refresh { task_id: id },
            CallbackAction::FollowUpReply {
                flow_token: flow_token.clone(),
            },
            CallbackAction::CreateReviewTask {
                flow_token: flow_token.clone(),
            },
            CallbackAction::CreateReviewTaskConfirm {
                flow_token: flow_token.clone(),
            },
            CallbackAction::DoneTask {
                flow_token: flow_token.clone(),
            },
            CallbackAction::NewTaskProject { project_id: id },
            CallbackAction::LegacyFollowUpReplyTask { task_id: id },
            CallbackAction::LegacyCreateReviewTask { task_id: id },
            CallbackAction::LegacyCreateReviewTaskConfirm { task_id: id },
            CallbackAction::LegacyDoneTask { task_id: id },
            CallbackAction::LegacyFlowApproveConfirmTask { task_id: id },
            CallbackAction::LegacyFlowApproveYesTask { task_id: id },
            CallbackAction::LegacyFlowRejectInputTask { task_id: id },
            CallbackAction::TaskPage {
                project_id: id,
                page: 999,
            },
            CallbackAction::PinMenu,
            CallbackAction::Unpin,
            CallbackAction::PinProject { project_id: id },
            CallbackAction::PinDuration {
                project_id: id,
                minutes: 720,
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

    #[test]
    fn roundtrip_pin_project() {
        let id = Uuid::new_v4();
        let action = CallbackAction::PinProject { project_id: id };
        let encoded = action.encode();
        assert!(encoded.len() <= 64);
        assert_eq!(CallbackAction::decode(&encoded).unwrap(), action);
    }

    #[test]
    fn roundtrip_pin_duration() {
        let id = Uuid::new_v4();
        for minutes in [30u32, 60, 180, 360, 720] {
            let action = CallbackAction::PinDuration {
                project_id: id,
                minutes,
            };
            let encoded = action.encode();
            assert!(
                encoded.len() <= 64,
                "encoded too long for {minutes}m: {encoded}"
            );
            assert_eq!(CallbackAction::decode(&encoded).unwrap(), action);
        }
    }
}
