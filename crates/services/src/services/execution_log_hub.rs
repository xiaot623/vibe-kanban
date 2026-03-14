use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
};

use executors::logs::{
    NormalizedEventSink, NormalizedEventStream, NormalizedLogEvent,
    utils::patch::extract_normalized_events_from_patch,
};
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use utils::msg_store::MsgStore;

const NORMALIZED_HISTORY_CAPACITY: usize = 20_000;

pub struct ExecutionLogHub {
    raw_store: Arc<MsgStore>,
    normalized_history: RwLock<VecDeque<NormalizedLogEvent>>,
    normalized_sender: broadcast::Sender<NormalizedLogEvent>,
}

#[derive(Clone)]
struct HubNormalizedEventSink {
    hub: Arc<ExecutionLogHub>,
}

impl NormalizedEventSink for HubNormalizedEventSink {
    fn emit(&self, event: NormalizedLogEvent) {
        self.hub.push_normalized_event(event);
    }
}

impl ExecutionLogHub {
    pub fn new(raw_store: Arc<MsgStore>) -> Arc<Self> {
        let (normalized_sender, _) = broadcast::channel(NORMALIZED_HISTORY_CAPACITY);
        let hub = Arc::new(Self {
            raw_store: raw_store.clone(),
            normalized_history: RwLock::new(VecDeque::with_capacity(512)),
            normalized_sender,
        });

        let weak_hub = Arc::downgrade(&hub);
        raw_store.set_patch_interceptor(Some(Arc::new(move |patch| {
            let Some(hub) = weak_hub.upgrade() else {
                return false;
            };
            let Some(events) = extract_normalized_events_from_patch(patch) else {
                return false;
            };
            for event in events {
                hub.push_normalized_event(event);
            }
            false
        })));

        hub
    }

    pub fn raw_store(&self) -> Arc<MsgStore> {
        self.raw_store.clone()
    }

    pub fn normalized_sink(self: &Arc<Self>) -> Arc<dyn NormalizedEventSink> {
        Arc::new(HubNormalizedEventSink {
            hub: Arc::clone(self),
        })
    }

    pub fn push_normalized_event(&self, event: NormalizedLogEvent) {
        let _ = self.normalized_sender.send(event.clone());

        let mut history = self.normalized_history.write().unwrap();
        while history.len() >= NORMALIZED_HISTORY_CAPACITY {
            let _ = history.pop_front();
        }
        history.push_back(event);
    }

    pub fn push_finished(&self) {
        self.push_normalized_event(NormalizedLogEvent::Finished);
        self.raw_store.push_finished();
    }

    pub fn normalized_history(&self) -> Vec<NormalizedLogEvent> {
        self.normalized_history
            .read()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }

    pub fn normalized_history_plus_stream(&self) -> NormalizedEventStream {
        // Subscribe BEFORE snapshotting history to close the TOCTOU gap:
        // any event pushed between subscribe and snapshot ends up in history
        // (captured by the snapshot) but NOT in the live stream (broadcast only
        // delivers events sent after subscription).  The reverse order would
        // silently drop events pushed in that window.
        let rx = self.normalized_sender.subscribe();
        let history = self.normalized_history();

        let hist = futures::stream::iter(history.into_iter().map(Ok::<_, std::io::Error>));
        let live = BroadcastStream::new(rx)
            .filter_map(|res| async move { res.ok().map(Ok::<_, std::io::Error>) });

        Box::pin(hist.chain(live))
    }
}
