use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::logs::NormalizedEntry;

pub const MSG_TYPE_NORMALIZED_UPSERT: &str = "normalized_upsert";
pub const MSG_TYPE_NORMALIZED_REMOVE: &str = "normalized_remove";
pub const MSG_TYPE_NORMALIZED_FINISHED: &str = "normalized_finished";

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum NormalizedLogEvent {
    UpsertEntry {
        index: usize,
        entry: NormalizedEntry,
    },
    RemoveEntry {
        index: usize,
    },
    Finished,
}

impl NormalizedLogEvent {
    pub fn msg_type(&self) -> &'static str {
        match self {
            Self::UpsertEntry { .. } => MSG_TYPE_NORMALIZED_UPSERT,
            Self::RemoveEntry { .. } => MSG_TYPE_NORMALIZED_REMOVE,
            Self::Finished => MSG_TYPE_NORMALIZED_FINISHED,
        }
    }
}

pub trait NormalizedEventSink: Send + Sync {
    fn emit(&self, event: NormalizedLogEvent);
}

pub type NormalizedEventStream = BoxStream<'static, Result<NormalizedLogEvent, std::io::Error>>;
