//! Task state machine framework for task status transitions.

pub mod dispatcher;
pub mod handler;
pub mod registry;
pub mod transition;

use transition::StateTransition;

use crate::models::task::{Task, TaskStatus};

/// Task-specific state transition with full task context.
#[derive(Debug, Clone)]
pub struct TaskStateTransition {
    /// The underlying state transition
    pub transition: StateTransition<TaskStatus>,
    /// The full task after transition
    pub task: Task,
}

impl TaskStateTransition {
    /// Create a new task state transition.
    pub fn new(task: Task, old_status: Option<TaskStatus>) -> Self {
        Self {
            transition: StateTransition::new(task.id, old_status, task.status.clone()),
            task,
        }
    }

    /// Get the task ID.
    pub fn task_id(&self) -> uuid::Uuid {
        self.task.id
    }

    /// Get the previous status (None if task was just created).
    pub fn from_status(&self) -> Option<&TaskStatus> {
        self.transition.from_state.as_ref()
    }

    /// Get the new status.
    pub fn to_status(&self) -> &TaskStatus {
        &self.transition.to_state
    }

    /// Check if this is a creation event.
    pub fn is_creation(&self) -> bool {
        self.transition.is_creation()
    }

    /// Check if the status actually changed.
    pub fn is_change(&self) -> bool {
        self.transition.is_change()
    }
}

/// Prelude module for convenient imports.
pub mod prelude {
    pub use crate::task_state::{
        TaskStateTransition,
        dispatcher::{TaskStateDispatcher, dispatch_task_transition, shared_dispatcher},
        handler::{HandlerContext, TaskStateHandler, TransitionFilter, fn_handler},
        registry::HandlerRegistry,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transition_filter() {
        use handler::TransitionFilter;

        let filter = TransitionFilter::new().to(vec![TaskStatus::InReview]);

        // Create a mock task
        let task = Task {
            id: uuid::Uuid::new_v4(),
            project_id: uuid::Uuid::new_v4(),
            title: "Test".to_string(),
            description: None,
            status: TaskStatus::InReview,
            parent_workspace_id: None,
            diff_additions: None,
            diff_deletions: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let transition = TaskStateTransition::new(task, Some(TaskStatus::InProgress));
        assert!(filter.matches(&transition));

        // Create another transition that shouldn't match
        let task2 = Task {
            id: uuid::Uuid::new_v4(),
            project_id: uuid::Uuid::new_v4(),
            title: "Test".to_string(),
            description: None,
            status: TaskStatus::Done,
            parent_workspace_id: None,
            diff_additions: None,
            diff_deletions: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let transition2 = TaskStateTransition::new(task2, Some(TaskStatus::InProgress));
        assert!(!filter.matches(&transition2));
    }

    #[test]
    fn test_state_transition() {
        use transition::StateTransition;

        let transition: StateTransition<TaskStatus> = StateTransition::new(
            uuid::Uuid::new_v4(),
            Some(TaskStatus::Todo),
            TaskStatus::InProgress,
        );

        assert!(transition.is_change());
        assert!(!transition.is_creation());
        assert!(transition.from(&TaskStatus::Todo));
        assert!(transition.to(&TaskStatus::InProgress));
        assert!(transition.is_transition(&TaskStatus::Todo, &TaskStatus::InProgress));
    }
}
