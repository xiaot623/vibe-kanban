//! In-memory registry of running audio (TTS) synthesis jobs.
//!
//! Keyed by `(flow_token, source_message_id)` — the identity of the
//! Stage Summary card that launched the job.
//!
//! The registry is runtime-only: it is reset when the bot restarts.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use teloxide::types::MessageId;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Identity key for an audio job — tied to the originating Stage Summary card.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AudioJobKey {
    pub flow_token: String,
    pub source_message_id: MessageId,
}

/// State of a running audio job.
#[derive(Debug, Clone)]
pub struct AudioJob {
    /// Token that the handler can cancel.
    pub cancel: CancellationToken,
    /// Replicate prediction ID, filled in once the prediction is created.
    pub prediction_id: Option<String>,
    /// Path to the downloaded audio file (filled in on success before sending).
    pub audio_path: Option<PathBuf>,
}

#[derive(Default)]
struct Registry {
    jobs: HashMap<AudioJobKey, AudioJob>,
}

static AUDIO_JOB_REGISTRY: OnceLock<Arc<RwLock<Registry>>> = OnceLock::new();

fn registry() -> &'static Arc<RwLock<Registry>> {
    AUDIO_JOB_REGISTRY.get_or_init(|| Arc::new(RwLock::new(Registry::default())))
}

/// Insert a new job. Returns the cancellation token.
pub async fn insert(key: AudioJobKey) -> CancellationToken {
    let cancel = CancellationToken::new();
    let job = AudioJob {
        cancel: cancel.clone(),
        prediction_id: None,
        audio_path: None,
    };
    registry().write().await.jobs.insert(key, job);
    cancel
}

/// Remove and return the job for `key`, if it exists.
pub async fn remove(key: &AudioJobKey) -> Option<AudioJob> {
    registry().write().await.jobs.remove(key)
}

/// Check whether a job is currently registered for `key`.
pub async fn contains(key: &AudioJobKey) -> bool {
    registry().read().await.jobs.contains_key(key)
}

/// Cancel and remove all registered jobs (called on bot shutdown).
pub async fn cancel_all() {
    let mut guard = registry().write().await;
    for (_, job) in guard.jobs.drain() {
        job.cancel.cancel();
    }
}
