//! Task state machine service for unified state transition handling.
//!
//! This service provides access to the shared global dispatcher for
//! registering handlers and subscribing to state transitions.
//!
//! Note: State transitions are now automatically dispatched by the db layer
//! when Task::create, Task::update, or Task::update_status are called.
//! This service is primarily used for accessing the dispatcher to register
//! handlers or subscribe to events.

use std::sync::Arc;

use db::models::task::{Task, TaskStatus};
use sqlx::SqlitePool;
use db::task_state::dispatcher::{TaskStateDispatcher, shared_dispatcher};

/// Service for managing task state transitions with event dispatch.
///
/// This service provides access to the shared global dispatcher for
/// registering handlers and subscribing to state transitions.
///
/// Note: State transitions are now automatically dispatched by Task methods
/// in the db layer. This service is primarily used to access the dispatcher
/// for registering handlers or subscribing to events.
pub struct TaskStateService {
    dispatcher: Arc<TaskStateDispatcher>,
    pool: SqlitePool,
}

impl TaskStateService {
    /// Create a new task state service using the shared global dispatcher.
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            dispatcher: shared_dispatcher(),
            pool,
        }
    }

    /// Create a new task state service with a custom dispatcher.
    /// Prefer using `new()` which uses the shared global dispatcher.
    pub fn with_dispatcher(pool: SqlitePool, dispatcher: Arc<TaskStateDispatcher>) -> Self {
        Self { dispatcher, pool }
    }

    /// Get the dispatcher for subscribing to events or registering handlers.
    pub fn dispatcher(&self) -> &Arc<TaskStateDispatcher> {
        &self.dispatcher
    }

    /// Get the database pool.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Update task status.
    ///
    /// Note: State transition is automatically dispatched by Task::update_status.
    pub async fn update_status(
        &self,
        task_id: uuid::Uuid,
        new_status: TaskStatus,
    ) -> Result<Task, sqlx::Error> {
        Task::update_status(&self.pool, task_id, new_status).await
    }

    /// Subscribe to state transitions.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<db::task_state::TaskStateTransition> {
        self.dispatcher.subscribe()
    }
}

impl Clone for TaskStateService {
    fn clone(&self) -> Self {
        Self {
            dispatcher: Arc::clone(&self.dispatcher),
            pool: self.pool.clone(),
        }
    }
}
