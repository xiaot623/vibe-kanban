//! Central dispatcher for task state transitions.

use std::sync::{Arc, OnceLock};

use tokio::sync::{RwLock, broadcast};

use super::{
    TaskStateTransition,
    handler::{HandlerContext, TaskStateHandler},
    registry,
    registry::HandlerRegistry,
};
use crate::models::task::{Task, TaskStatus};

static GLOBAL_DISPATCHER: OnceLock<Arc<TaskStateDispatcher>> = OnceLock::new();

/// Get the shared global dispatcher instance.
///
/// This dispatcher is shared across the application and can be used
/// to dispatch transitions from the db layer without requiring explicit
/// dispatcher passing.
pub fn shared_dispatcher() -> Arc<TaskStateDispatcher> {
    GLOBAL_DISPATCHER
        .get_or_init(|| Arc::new(TaskStateDispatcher::new()))
        .clone()
}

/// Dispatch a task state transition using the global dispatcher.
///
/// This is a convenience function for dispatching transitions from the db layer.
/// It creates a HandlerContext from the pool and schedules handler execution.
pub async fn dispatch_task_transition(
    pool: &sqlx::SqlitePool,
    task: Task,
    old_status: Option<TaskStatus>,
) {
    let ctx = HandlerContext::new(pool.clone());
    shared_dispatcher().transition(&ctx, task, old_status).await;
}

/// Central dispatcher for task state transitions.
///
/// The dispatcher maintains a registry of handlers and a broadcast channel
/// for external subscribers. When a transition occurs, it notifies all
/// matching handlers and broadcasts to subscribers.
pub struct TaskStateDispatcher {
    registry: Arc<RwLock<HandlerRegistry>>,
    /// Broadcast channel for external subscribers
    tx: broadcast::Sender<TaskStateTransition>,
}

impl Default for TaskStateDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskStateDispatcher {
    /// Create a new dispatcher with shared registry.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            registry: registry::shared_registry(),
            tx,
        }
    }

    /// Subscribe to state transitions.
    ///
    /// Returns a receiver that will receive all state transitions.
    pub fn subscribe(&self) -> broadcast::Receiver<TaskStateTransition> {
        self.tx.subscribe()
    }

    /// Record and dispatch a state transition.
    ///
    /// This method:
    /// 1. Creates a TaskStateTransition from the task and old status
    /// 2. Broadcasts to all subscribers
    /// 3. Dispatches all matching handlers in a detached async task
    pub async fn transition(
        &self,
        ctx: &HandlerContext,
        task: Task,
        old_status: Option<TaskStatus>,
    ) {
        let transition = TaskStateTransition::new(task, old_status);

        // Skip if no actual change
        if !transition.transition.is_change() {
            tracing::trace!(
                task_id = %transition.task_id(),
                status = ?transition.to_status(),
                "Skipping no-op state transition"
            );
            return;
        }

        tracing::info!(
            task_id = %transition.task_id(),
            from = ?transition.from_status(),
            to = ?transition.to_status(),
            "Task state transition"
        );

        // Broadcast to subscribers (ignore errors if no receivers)
        let _ = self.tx.send(transition.clone());

        // Snapshot handlers quickly, then execute them in the background so
        // status updates are not blocked by slow handlers.
        let handlers = {
            let registry = self.registry.read().await;
            registry.matching_handlers(&transition)
        };

        if handlers.is_empty() {
            tracing::trace!(
                task_id = %transition.task_id(),
                "No task state handlers matched transition"
            );
            return;
        }

        tracing::trace!(
            task_id = %transition.task_id(),
            handler_count = handlers.len(),
            "Scheduling async task state handler dispatch"
        );

        let ctx = ctx.clone();
        let _dispatch_task = tokio::spawn(async move {
            for handler in handlers {
                tracing::debug!(
                    handler = handler.name(),
                    task_id = %transition.task_id(),
                    from = ?transition.from_status(),
                    to = ?transition.to_status(),
                    "Dispatching task state transition asynchronously"
                );

                handler.handle(&ctx, &transition).await;
            }
        });
    }

    /// Register a runtime handler.
    pub async fn register_handler(&self, handler: Arc<dyn TaskStateHandler>) {
        self.registry.write().await.register(handler);
    }

    /// Get the number of registered handlers.
    pub async fn handler_count(&self) -> usize {
        self.registry.read().await.handler_count()
    }
}
