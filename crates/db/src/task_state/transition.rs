//! State transition types for tracking entity state changes.

use std::fmt::Debug;

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Represents a state transition event with before/after states.
///
/// Generic over the state type `S` to allow reuse across different entity types.
#[derive(Debug, Clone)]
pub struct StateTransition<S: Clone + PartialEq + Debug> {
    /// The entity that transitioned
    pub entity_id: Uuid,
    /// The previous state (None for newly created entities)
    pub from_state: Option<S>,
    /// The new state
    pub to_state: S,
    /// When the transition occurred
    pub timestamp: DateTime<Utc>,
}

impl<S: Clone + PartialEq + Debug> StateTransition<S> {
    /// Create a new state transition
    pub fn new(entity_id: Uuid, from_state: Option<S>, to_state: S) -> Self {
        Self {
            entity_id,
            from_state,
            to_state,
            timestamp: Utc::now(),
        }
    }

    /// Check if this is a transition from a specific state to another
    pub fn is_transition(&self, from: &S, to: &S) -> bool {
        self.from_state.as_ref() == Some(from) && &self.to_state == to
    }

    /// Check if the target state matches
    pub fn to(&self, state: &S) -> bool {
        &self.to_state == state
    }

    /// Check if the source state matches (returns false for creation events)
    pub fn from(&self, state: &S) -> bool {
        self.from_state.as_ref() == Some(state)
    }

    /// Returns true if this is a creation event (no previous state)
    pub fn is_creation(&self) -> bool {
        self.from_state.is_none()
    }

    /// Returns true if the state actually changed
    pub fn is_change(&self) -> bool {
        self.from_state.as_ref() != Some(&self.to_state)
    }
}
