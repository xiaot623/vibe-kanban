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

    /// Creating a new task — waiting for the user to type a title.
    CreatingTaskTitle {
        project_id: Uuid,
        project_name: String,
        prompt_message_id: i32,
    },

    /// Creating a new task — waiting for the user to type a description (or skip).
    CreatingTaskDescription {
        project_id: Uuid,
        project_name: String,
        title: String,
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
        task_id: Uuid,
        prompt_message_id: i32,
    },

    /// Sending a follow-up reply — waiting for the user input text.
    ReplyingFollowUp {
        task_id: Uuid,
        prompt_message_id: i32,
    },
}

impl DialogueState {
    pub fn prompt_message_id(&self) -> Option<i32> {
        match self {
            Self::CreatingTaskTitle {
                prompt_message_id, ..
            }
            | Self::CreatingTaskDescription {
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
        let entering = DialogueState::CreatingTaskTitle {
            project_id,
            project_name: "Daily".to_string(),
            prompt_message_id: 101,
        };
        assert_eq!(entering.prompt_message_id(), Some(101));

        let next = DialogueState::CreatingTaskDescription {
            project_id,
            project_name: "Daily".to_string(),
            title: "Ship it".to_string(),
            prompt_message_id: entering.prompt_message_id().unwrap_or_default(),
        };
        assert_eq!(next.prompt_message_id(), Some(101));
        assert_eq!(DialogueState::Idle.prompt_message_id(), None);
    }

    #[test]
    fn prompt_message_id_available_for_single_step_input_states() {
        let task_id = Uuid::new_v4();
        let reject = DialogueState::RejectingPlan {
            task_id,
            prompt_message_id: 55,
        };
        assert_eq!(reject.prompt_message_id(), Some(55));

        let reply = DialogueState::ReplyingFollowUp {
            task_id,
            prompt_message_id: 77,
        };
        assert_eq!(reply.prompt_message_id(), Some(77));
    }
}
