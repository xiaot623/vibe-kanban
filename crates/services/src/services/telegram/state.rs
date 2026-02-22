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
    },

    /// Creating a new task — waiting for the user to type a description (or skip).
    CreatingTaskDescription {
        project_id: Uuid,
        project_name: String,
        title: String,
    },

    /// Editing a task — waiting for the user to type a new title.
    EditingTaskTitle {
        task_id: Uuid,
        current_title: String,
    },

    /// Editing a task — waiting for the user to type a new description.
    EditingTaskDescription {
        task_id: Uuid,
        title: String,
        current_description: Option<String>,
    },

    /// Rejecting a plan — waiting for the user to type a reason.
    RejectingPlan {
        task_id: Uuid,
    },
}
