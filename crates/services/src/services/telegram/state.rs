//! Dialogue state for multi-step Telegram interactions.

use uuid::Uuid;

/// States for the Telegram dialogue FSM.
///
/// Each variant represents a point in a multi-step user interaction.
/// The dialogue resets to `Idle` on `/cancel`, timeout, or completion.
#[derive(Clone, Debug, Default)]
pub enum DialogueState {
    /// No active dialogue. Default state.
    #[default]
    Idle,

    /// Creating a new task — waiting for the user to send the task message.
    ///
    /// Message parsing follows Daily mode behavior:
    /// first line is title, remaining lines are description.
    CreatingTaskMessage {
        project_id: Uuid,
        prompt_message_id: i32,
    },

    /// Editing a task — waiting for the user to type a new title.
    EditingTaskTitle {
        task_id: Uuid,
        current_title: String,
        prompt_message_id: i32,
    },

    /// Editing a task — waiting for the user to type a new description.
    EditingTaskDescription {
        task_id: Uuid,
        title: String,
        current_description: Option<String>,
        prompt_message_id: i32,
    },

    /// Rejecting a plan — waiting for the user to type a reason.
    RejectingPlan {
        flow_token: String,
        prompt_message_id: i32,
    },

    /// Sending a follow-up reply — waiting for the user input text.
    ReplyingFollowUp {
        flow_token: String,
        session_id: Uuid,
        prompt_message_id: i32,
    },
}

impl DialogueState {
    pub fn prompt_message_id(&self) -> Option<i32> {
        match self {
            Self::CreatingTaskMessage {
                prompt_message_id, ..
            }
            | Self::EditingTaskTitle {
                prompt_message_id, ..
            }
            | Self::EditingTaskDescription {
                prompt_message_id, ..
            }
            | Self::RejectingPlan {
                prompt_message_id, ..
            }
            | Self::ReplyingFollowUp {
                prompt_message_id, ..
            } => Some(*prompt_message_id),
            Self::Idle => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::DialogueState;

    #[test]
    fn prompt_message_id_lifecycle_for_create_flow() {
        let project_id = Uuid::new_v4();
        let entering = DialogueState::CreatingTaskMessage {
            project_id,
            prompt_message_id: 101,
        };
        assert_eq!(entering.prompt_message_id(), Some(101));
        assert_eq!(DialogueState::Idle.prompt_message_id(), None);
    }

    #[test]
    fn prompt_message_id_available_for_single_step_input_states() {
        let task_id = Uuid::new_v4();
        let reject = DialogueState::RejectingPlan {
            flow_token: format!("f-{}", task_id.simple()),
            prompt_message_id: 55,
        };
        assert_eq!(reject.prompt_message_id(), Some(55));

        let reply = DialogueState::ReplyingFollowUp {
            flow_token: "f-abc12".to_string(),
            session_id: task_id,
            prompt_message_id: 77,
        };
        assert_eq!(reply.prompt_message_id(), Some(77));
    }
}
