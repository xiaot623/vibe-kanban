//! Handler trait and filter types for task state transitions.

use async_trait::async_trait;
use sqlx::SqlitePool;
use std::{future::Future, pin::Pin, sync::Arc};

use crate::models::task::TaskStatus;

use super::TaskStateTransition;

/// Filter specification for which transitions a handler should receive.
#[derive(Debug, Clone, Default)]
pub struct TransitionFilter {
    /// If Some, only match transitions FROM these states
    pub from_states: Option<Vec<TaskStatus>>,
    /// If Some, only match transitions TO these states
    pub to_states: Option<Vec<TaskStatus>>,
}

impl TransitionFilter {
    /// Create a new empty filter (matches all transitions)
    pub fn new() -> Self {
        Self::default()
    }

    /// Filter to only match transitions FROM the specified states
    pub fn from(mut self, states: Vec<TaskStatus>) -> Self {
        self.from_states = Some(states);
        self
    }

    /// Filter to only match transitions TO the specified states
    pub fn to(mut self, states: Vec<TaskStatus>) -> Self {
        self.to_states = Some(states);
        self
    }

    /// Check if a transition matches this filter
    pub fn matches(&self, transition: &TaskStateTransition) -> bool {
        let from_matches = match &self.from_states {
            None => true,
            Some(states) => transition
                .from_status()
                .map(|s| states.contains(s))
                .unwrap_or(false),
        };

        let to_matches = match &self.to_states {
            None => true,
            Some(states) => states.contains(transition.to_status()),
        };

        from_matches && to_matches
    }
}

/// Context provided to event handlers.
#[derive(Clone)]
pub struct HandlerContext {
    /// Database connection pool
    pub pool: SqlitePool,
}

impl HandlerContext {
    /// Create a new handler context
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// Trait for task state transition handlers.
///
/// Implement this trait to handle task state transitions.
#[async_trait]
pub trait TaskStateHandler: Send + Sync + 'static {
    /// Returns the filter for which transitions this handler should receive
    fn filter(&self) -> TransitionFilter;

    /// Handle a state transition
    async fn handle(&self, ctx: &HandlerContext, transition: &TaskStateTransition);

    /// Handler name for logging/debugging
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

/// Boxed future type for handler functions.
pub type HandlerFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Function signature for handler callbacks.
pub type HandlerFn = Arc<
    dyn for<'a> Fn(&'a HandlerContext, &'a TaskStateTransition) -> HandlerFuture<'a>
        + Send
        + Sync,
>;

/// Handler implementation backed by a function callback.
pub struct FnTaskStateHandler {
    name: &'static str,
    filter: TransitionFilter,
    handler: HandlerFn,
}

impl FnTaskStateHandler {
    pub fn new(name: &'static str, filter: TransitionFilter, handler: HandlerFn) -> Self {
        Self {
            name,
            filter,
            handler,
        }
    }
}

#[async_trait]
impl TaskStateHandler for FnTaskStateHandler {
    fn filter(&self) -> TransitionFilter {
        self.filter.clone()
    }

    async fn handle(&self, ctx: &HandlerContext, transition: &TaskStateTransition) {
        (self.handler)(ctx, transition).await
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

/// Helper to build a handler from an async function.
pub fn fn_handler(
    name: &'static str,
    filter: TransitionFilter,
    handler: impl for<'a> Fn(&'a HandlerContext, &'a TaskStateTransition) -> HandlerFuture<'a>
        + Send
        + Sync
        + 'static,
) -> Arc<dyn TaskStateHandler> {
    Arc::new(FnTaskStateHandler::new(name, filter, Arc::new(handler)))
}
