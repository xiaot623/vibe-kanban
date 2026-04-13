use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use db::models::scratch::DraftFollowUpData;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use ts_rs::TS;
use uuid::Uuid;

/// Represents a queued follow-up message for a session
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct QueuedMessage {
    /// The session this message is queued for
    pub session_id: Uuid,
    /// The follow-up data (message + variant)
    pub data: DraftFollowUpData,
    /// Timestamp when the message was queued
    pub queued_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueWaitError {
    Overwritten,
    Cancelled,
    Discarded,
    StartFailed(String),
}

#[derive(Debug)]
pub struct QueuedMessageWaiter {
    receiver: oneshot::Receiver<Result<Uuid, QueueWaitError>>,
}

impl QueuedMessageWaiter {
    pub async fn wait_for_start(self) -> Result<Uuid, QueueWaitError> {
        self.receiver
            .await
            .unwrap_or_else(|_| Err(QueueWaitError::Discarded))
    }
}

#[derive(Debug, Clone)]
pub struct ConsumedQueuedMessage {
    pub queued_message: QueuedMessage,
    queue_id: Uuid,
}

impl ConsumedQueuedMessage {
    pub fn queue_id(&self) -> Uuid {
        self.queue_id
    }
}

#[derive(Debug, Clone)]
struct QueuedMessageEntry {
    queued_message: QueuedMessage,
    queue_id: Uuid,
}

/// Status of the queue for a session (for frontend display)
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "status", rename_all = "snake_case")]
#[ts(export)]
pub enum QueueStatus {
    /// No message queued
    Empty,
    /// Message is queued and waiting for execution to complete
    Queued { message: QueuedMessage },
}

/// In-memory service for managing queued follow-up messages.
/// One queued message per session.
#[derive(Clone)]
pub struct QueuedMessageService {
    queue: Arc<DashMap<Uuid, QueuedMessageEntry>>,
    waiters: Arc<DashMap<Uuid, oneshot::Sender<Result<Uuid, QueueWaitError>>>>,
}

impl QueuedMessageService {
    pub fn new() -> Self {
        Self {
            queue: Arc::new(DashMap::new()),
            waiters: Arc::new(DashMap::new()),
        }
    }

    /// Queue a message for a session. Replaces any existing queued message.
    pub fn queue_message(&self, session_id: Uuid, data: DraftFollowUpData) -> QueuedMessage {
        self.queue_message_internal(session_id, data, None).0
    }

    /// Queue a message for a session and wait until it is started or superseded.
    pub fn queue_message_with_waiter(
        &self,
        session_id: Uuid,
        data: DraftFollowUpData,
    ) -> (QueuedMessage, QueuedMessageWaiter) {
        let (tx, rx) = oneshot::channel();
        let (queued_message, _) = self.queue_message_internal(session_id, data, Some(tx));
        (queued_message, QueuedMessageWaiter { receiver: rx })
    }

    fn queue_message_internal(
        &self,
        session_id: Uuid,
        data: DraftFollowUpData,
        waiter: Option<oneshot::Sender<Result<Uuid, QueueWaitError>>>,
    ) -> (QueuedMessage, Uuid) {
        let queued = QueuedMessage {
            session_id,
            data,
            queued_at: Utc::now(),
        };

        let queue_id = Uuid::new_v4();
        let entry = QueuedMessageEntry {
            queued_message: queued.clone(),
            queue_id,
        };

        if let Some(waiter) = waiter {
            self.waiters.insert(queue_id, waiter);
        }

        let replaced = self.queue.insert(session_id, entry);
        if let Some(old_entry) = replaced {
            self.resolve_waiter(old_entry.queue_id, Err(QueueWaitError::Overwritten));
        }

        (queued, queue_id)
    }

    /// Cancel/remove a queued message for a session
    pub fn cancel_queued(&self, session_id: Uuid) -> Option<QueuedMessage> {
        self.queue.remove(&session_id).map(|(_, entry)| {
            self.resolve_waiter(entry.queue_id, Err(QueueWaitError::Cancelled));
            entry.queued_message
        })
    }

    /// Get the queued message for a session (if any)
    pub fn get_queued(&self, session_id: Uuid) -> Option<QueuedMessage> {
        self.queue.get(&session_id).map(|r| r.queued_message.clone())
    }

    /// Take (remove and return) the queued message for a session.
    /// Used by finalization flow to consume the queued message.
    pub fn take_queued(&self, session_id: Uuid) -> Option<ConsumedQueuedMessage> {
        self.queue.remove(&session_id).map(|(_, entry)| ConsumedQueuedMessage {
            queued_message: entry.queued_message,
            queue_id: entry.queue_id,
        })
    }

    pub fn notify_started(&self, queue_id: Uuid, execution_id: Uuid) {
        self.resolve_waiter(queue_id, Ok(execution_id));
    }

    pub fn notify_discarded(&self, queue_id: Uuid) {
        self.resolve_waiter(queue_id, Err(QueueWaitError::Discarded));
    }

    pub fn notify_start_failed(&self, queue_id: Uuid, error: impl Into<String>) {
        self.resolve_waiter(queue_id, Err(QueueWaitError::StartFailed(error.into())));
    }

    /// Check if a session has a queued message
    pub fn has_queued(&self, session_id: Uuid) -> bool {
        self.queue.contains_key(&session_id)
    }

    /// Get queue status for frontend display
    pub fn get_status(&self, session_id: Uuid) -> QueueStatus {
        match self.get_queued(session_id) {
            Some(msg) => QueueStatus::Queued { message: msg },
            None => QueueStatus::Empty,
        }
    }

    fn resolve_waiter(&self, queue_id: Uuid, result: Result<Uuid, QueueWaitError>) {
        if let Some((_, sender)) = self.waiters.remove(&queue_id) {
            let _ = sender.send(result);
        }
    }
}

impl Default for QueuedMessageService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use db::models::scratch::DraftFollowUpData;

    use super::{QueueWaitError, QueuedMessageService};

    fn draft(message: &str) -> DraftFollowUpData {
        DraftFollowUpData {
            message: message.to_string(),
            variant: None,
            executor: None,
        }
    }

    #[tokio::test]
    async fn new_queue_overwrites_old_waiter() {
        let service = QueuedMessageService::new();
        let session_id = uuid::Uuid::new_v4();

        let (_, old_waiter) = service.queue_message_with_waiter(session_id, draft("first"));
        let (_, new_waiter) = service.queue_message_with_waiter(session_id, draft("second"));

        assert_eq!(old_waiter.wait_for_start().await, Err(QueueWaitError::Overwritten));

        let consumed = service.take_queued(session_id).expect("queued item");
        let execution_id = uuid::Uuid::new_v4();
        service.notify_started(consumed.queue_id(), execution_id);

        assert_eq!(new_waiter.wait_for_start().await, Ok(execution_id));
    }

    #[tokio::test]
    async fn consuming_queue_notifies_execution_id() {
        let service = QueuedMessageService::new();
        let session_id = uuid::Uuid::new_v4();

        let (_, waiter) = service.queue_message_with_waiter(session_id, draft("hello"));
        let consumed = service.take_queued(session_id).expect("queued item");
        let execution_id = uuid::Uuid::new_v4();

        service.notify_started(consumed.queue_id(), execution_id);

        assert_eq!(waiter.wait_for_start().await, Ok(execution_id));
    }

    #[tokio::test]
    async fn cancel_and_discard_and_failure_notify_waiters() {
        let service = QueuedMessageService::new();
        let session_id = uuid::Uuid::new_v4();

        let (_, cancelled_waiter) = service.queue_message_with_waiter(session_id, draft("a"));
        service.cancel_queued(session_id);
        assert_eq!(
            cancelled_waiter.wait_for_start().await,
            Err(QueueWaitError::Cancelled)
        );

        let (_, discarded_waiter) = service.queue_message_with_waiter(session_id, draft("b"));
        let consumed = service.take_queued(session_id).expect("queued item");
        service.notify_discarded(consumed.queue_id());
        assert_eq!(
            discarded_waiter.wait_for_start().await,
            Err(QueueWaitError::Discarded)
        );

        let (_, failed_waiter) = service.queue_message_with_waiter(session_id, draft("c"));
        let consumed = service.take_queued(session_id).expect("queued item");
        service.notify_start_failed(consumed.queue_id(), "boom");
        assert_eq!(
            failed_waiter.wait_for_start().await,
            Err(QueueWaitError::StartFailed("boom".to_string()))
        );
    }
}
