//! Handler registry for collecting and dispatching to state handlers.

use std::sync::{Arc, OnceLock};

use tokio::sync::RwLock;

use super::TaskStateTransition;
use super::handler::{HandlerContext, TaskStateHandler};

/// Runtime handler registry.
pub struct HandlerRegistry {
    handlers: Vec<Arc<dyn TaskStateHandler>>,
}

impl Default for HandlerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HandlerRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    /// Register a handler at runtime.
    pub fn register(&mut self, handler: Arc<dyn TaskStateHandler>) {
        tracing::debug!(handler = handler.name(), "Registered runtime handler");
        self.handlers.push(handler);
    }

    /// Dispatch a transition to all matching handlers.
    pub async fn dispatch(&self, ctx: &HandlerContext, transition: &TaskStateTransition) {
        for handler in &self.handlers {
            if handler.filter().matches(transition) {
                tracing::debug!(
                    handler = handler.name(),
                    task_id = %transition.task_id(),
                    from = ?transition.from_status(),
                    to = ?transition.to_status(),
                    "Dispatching task state transition"
                );

                handler.handle(ctx, transition).await;
            }
        }
    }

    /// Get the number of registered handlers.
    pub fn handler_count(&self) -> usize {
        self.handlers.len()
    }
}

static GLOBAL_REGISTRY: OnceLock<Arc<RwLock<HandlerRegistry>>> = OnceLock::new();

/// Shared handler registry for all dispatchers.
pub fn shared_registry() -> Arc<RwLock<HandlerRegistry>> {
    GLOBAL_REGISTRY
        .get_or_init(|| Arc::new(RwLock::new(HandlerRegistry::new())))
        .clone()
}
