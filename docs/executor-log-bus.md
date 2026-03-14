# Executor Log Bus — Refactor Design & Integration Guide

## Background

Before this refactor, agent execution logs were transported as opaque
**JSON Patch** operations applied against a mutable conversation document.
Every consumer (WebSocket handler, DB writer, test assertions) had to
re-parse the patch to understand what had actually changed. This made it
hard to add new consumers (Telegram bot notifications, analytics pipelines,
alternative UIs) without touching the executor layer.

The refactor replaces JSON Patch as the *intermediate* format with a typed
event stream: **`NormalizedLogEvent`**. Patches are still written by
executors (they're a convenient wire format), but the hub automatically
projects them into typed events before any downstream code sees them.

---

## Core Types

### `NormalizedLogEvent` — `crates/executors/src/logs/normalized_event.rs`

```rust
pub enum NormalizedLogEvent {
    UpsertEntry { index: usize, entry: NormalizedEntry },
    RemoveEntry { index: usize },
    Finished,
}
```

Three variants cover the full lifecycle of a conversation entry list:

| Variant | Meaning |
|---|---|
| `UpsertEntry` | Insert or replace the entry at `index` |
| `RemoveEntry` | Delete the entry at `index` |
| `Finished` | Execution is complete; no more events will follow |

Each variant maps to a DB `msg_type` string constant (`normalized_upsert`,
`normalized_remove`, `normalized_finished`) stored in
`execution_process_logs.msg_type`.

### `NormalizedEntry` — `crates/executors/src/logs/mod.rs`

The payload inside `UpsertEntry`. Carries:

- `entry_type: NormalizedEntryType` — discriminated union covering
  `UserMessage`, `AssistantMessage`, `ToolUse`, `Thinking`, `Loading`,
  `NextAction`, `TokenUsageInfo`, `ErrorMessage`, `SystemMessage`,
  `UserFeedback`
- `content: String` — rendered text of the entry
- `timestamp: Option<String>` — ISO-8601 timestamp, if available
- `metadata: Option<serde_json::Value>` — executor-specific extension data
  (opaque to the bus layer)

### `NormalizedEventSink` / `NormalizedEventStream`

```rust
pub trait NormalizedEventSink: Send + Sync {
    fn emit(&self, event: NormalizedLogEvent);
}

pub type NormalizedEventStream =
    BoxStream<'static, Result<NormalizedLogEvent, std::io::Error>>;
```

`NormalizedEventSink` is the *push* interface; `NormalizedEventStream` is
the *pull* interface. Both are defined in `normalized_event.rs` and
re-exported from `executors::logs`.

---

## Architecture

```
Executor (e.g. claude.rs)
  │
  │  push_patch(ConversationPatch::add_normalized_entry(...))
  ▼
MsgStore::push_patch()                   crates/utils/src/msg_store.rs
  │
  │  calls patch_interceptor (if set)
  ▼
ExecutionLogHub::patch_interceptor       crates/services/src/services/execution_log_hub.rs
  │
  │  extract_normalized_events_from_patch()
  │
  ├─► broadcast::Sender<NormalizedLogEvent>   (live subscribers)
  └─► normalized_history: VecDeque            (ring buffer, 20 000 cap)
                 │
                 ▼
  ┌──────────────────────────────────────────────────────────────────┐
  │              Consumers                                           │
  │                                                                  │
  │  1. ContainerService::spawn_stream_raw_logs_to_db()             │
  │     Drains normalized_history_plus_stream() and writes          │
  │     JSONL rows (msg_type = normalized_upsert/remove/finished)   │
  │     to execution_process_logs in SQLite.                        │
  │                                                                  │
  │  2. ContainerService::stream_normalized_logs() (live path)      │
  │     Returns hub.normalized_history_plus_stream() wrapped in     │
  │     StreamedNormalizedLogEvent to the WS handler.               │
  │                                                                  │
  │  3. WS handler handle_normalized_logs_ws()                      │
  │     Serialises each event as { "event": ..., "seq": ... }       │
  │     and sends over WebSocket to the browser/mobile client.      │
  │                                                                  │
  │  4. Frontend streamNormalizedLogEvents.ts                        │
  │     Maintains a Map<index, NormalizedEntry>; renders entries    │
  │     in index order via toArray().                               │
  └──────────────────────────────────────────────────────────────────┘
```

---

## Component Deep-Dive

### `MsgStore` — `crates/utils/src/msg_store.rs`

Central raw-log ring buffer for one execution. Holds up to ~100 MB of
`LogMsg` history and a broadcast channel for live streaming.

The `patch_interceptor` slot (type `Arc<dyn Fn(&Patch) -> bool + Send + Sync>`)
is the extension point wired by `ExecutionLogHub`. When
`push_patch()` is called:

1. The interceptor is invoked with the patch.
2. If it returns `true`, the patch is **consumed** (not stored in raw history).
3. If it returns `false`, the patch is stored normally as `LogMsg::JsonPatch`.

Currently the interceptor always returns `false` so patches are stored in
both raw history and projected into typed events. This is intentional for
backward compatibility with old log consumers.

### `ExecutionLogHub` — `crates/services/src/services/execution_log_hub.rs`

Per-execution hub wiring `MsgStore` to the typed event bus.

```
ExecutionLogHub {
    raw_store: Arc<MsgStore>,
    normalized_history: RwLock<VecDeque<NormalizedLogEvent>>,  // cap 20 000
    normalized_sender: broadcast::Sender<NormalizedLogEvent>,  // cap 20 000
}
```

Constructed via `ExecutionLogHub::new(store)`, which:

1. Creates the hub `Arc`.
2. Registers a `patch_interceptor` on the store using a **weak** back-reference
   to prevent reference cycles.
3. The interceptor calls `extract_normalized_events_from_patch()` and for
   each resulting event calls `hub.push_normalized_event()`.

Key methods:

| Method | Purpose |
|---|---|
| `push_normalized_event(event)` | Broadcasts to live subscribers and appends to history ring buffer |
| `push_finished()` | Emits `Finished` event then calls `raw_store.push_finished()` |
| `normalized_history()` | Returns a snapshot of the history deque |
| `normalized_history_plus_stream()` | Subscribe-then-snapshot: live stream starting from subscription point, preceded by history; no event gap |
| `normalized_sink()` | Returns an `Arc<dyn NormalizedEventSink>` backed by this hub (direct push path, bypasses patch layer) |

### `extract_normalized_events_from_patch` — `crates/executors/src/logs/utils/patch.rs`

Converts a `json_patch::Patch` into `Vec<NormalizedLogEvent>`:

- `Add` at `/entries/{n}` with value type `NORMALIZED_ENTRY` → `UpsertEntry`
- `Replace` at `/entries/{n}` with value type `NORMALIZED_ENTRY` → `UpsertEntry`
- `Remove` at `/entries/{n}` → `RemoveEntry`
- Non-normalized ops (`Stdout`, `Stderr`, `Diff` adds) are **skipped** (not
  an error); this allows mixed patches to coexist.
- Returns `None` only if the patch contained zero normalizable operations.

### `stream_normalized_logs` — `crates/services/src/services/container.rs`

Four-path dispatch:

```
1. Live execution   → hub.normalized_history_plus_stream()
2. DB typed rows    → ExecutionProcessLogs filtered by msg_type IN (normalized_*)
3. DB json_patch    → parse JsonPatch rows, project via extract_normalized_events_from_patch,
                      lazily backfill typed rows for future requests
4. No data          → None (404 to caller)
```

Path 3 (legacy backfill) also writes the projected events back to
`execution_process_logs` with proper `msg_type` values so that path 2 is
hit on all subsequent requests, making path 3 a one-time migration.

### DB Schema — `execution_process_logs`

```
execution_id  UUID
logs          TEXT    JSONL — one event per line
msg_type      TEXT?   NULL | "json_patch" | "normalized_upsert"
                               | "normalized_remove" | "normalized_finished"
                               | "stdout" | "stderr" | ...
byte_size     INT
inserted_at   DATETIME
rowid         (implicit, used as seq cursor)
```

`msg_type` enables fast `WHERE msg_type IN (...)` queries without scanning
all rows for an execution. The `rowid` doubles as a monotonic sequence
number (`seq`) exposed to clients for cursor-based pagination via
`?after_seq=<n>`.

### WS Wire Protocol — `GET /execution-processes/:id/normalized-logs/ws`

Each message is a JSON object:

```jsonc
// Normal event
{ "event": { "type": "upsert_entry", "index": 3, "entry": { ... } }, "seq": 42 }
{ "event": { "type": "remove_entry", "index": 3 }, "seq": 43 }

// Terminal event — server closes connection after sending
{ "event": { "type": "finished" }, "seq": 44 }
```

`seq` is:
- A real SQLite `rowid` for DB-backed streams (historical playback).
- A synthetic monotonic counter (starting at 0) for live in-memory streams.

Clients use `?after_seq=<last_seen_seq>` to resume a stream after reconnect.

### Frontend — `frontend/src/utils/streamNormalizedLogEvents.ts`

Maintains a `Map<number, NormalizedEntry>` (keyed by `index`) and produces
a dense sorted array on each emit via `toArray()`. This correctly handles:

- **Index gaps** — missing indices from out-of-order arrivals are held in
  the map and filled when the missing events arrive.
- **Removes** — `entryMap.delete(index)` followed by re-emit.
- **Upserts** — overwrite any existing entry at the same index.

---

## Data Flow Diagram (live execution)

```
Executor process
  │
  │  calls push_patch(ConversationPatch::add_normalized_entry(3, entry))
  ▼
MsgStore::push_patch
  ├─ interceptor → extract_normalized_events_from_patch
  │                  → NormalizedLogEvent::UpsertEntry { index: 3, entry }
  │                  → hub.push_normalized_event(event)
  │                       ├─ broadcast::send(event)  ──────────────────────┐
  │                       └─ history.push_back(event)                      │
  │                                                                         │
  └─ push(LogMsg::JsonPatch(patch)) → raw history + broadcast              │
                                                                            │
spawn_stream_raw_logs_to_db                                                 │
  select! {                                                                  │
    normalized_stream.next() ◄──────────────────────────────────────────────┘
      → pending_logs.push(JSONL line, msg_type="normalized_upsert")
    raw_stream.next()
      → pending_logs.push(JSONL line, msg_type="json_patch")  // skipped for normalized patches
    flush_interval.tick()
      → ExecutionProcessLogs::append_log_lines(batch)
  }

WS connection (handle_normalized_logs_ws)
  hub.normalized_history_plus_stream()
  → StreamedNormalizedLogEvent { event, seq: None }
  → seq assigned as synthetic counter
  → { "event": {...}, "seq": 0 }  sent over WebSocket

Frontend
  streamNormalizedLogEvents(url)
  → entryMap.set(3, entry)
  → toArray() → [entry0, entry1, entry2, entry3]
  → onEntries([...])
```

---

## Integrating a New Log Consumer

The event bus is open for extension. There are two integration points
depending on what you need.

### Option A — Subscribe to the live hub (in-process Rust)

Use this when you need real-time delivery and are running inside the server
process (e.g. Telegram bot notification, analytics event, webhook trigger).

```rust
// Obtain the hub for a running execution
let hub: Arc<ExecutionLogHub> = container
    .get_execution_log_hub_by_id(&exec_id)
    .await
    .expect("execution is live");

// Get history + live stream
let mut stream = hub.normalized_history_plus_stream();

tokio::spawn(async move {
    while let Some(Ok(event)) = stream.next().await {
        match event {
            NormalizedLogEvent::UpsertEntry { index, entry } => {
                // e.g. send Telegram message when agent posts an AssistantMessage
                if matches!(entry.entry_type, NormalizedEntryType::AssistantMessage) {
                    telegram_bot.send(&entry.content).await;
                }
            }
            NormalizedLogEvent::RemoveEntry { .. } => {}
            NormalizedLogEvent::Finished => break,
        }
    }
});
```

The stream is backed by the broadcast channel (20 000-event capacity) and
pre-seeded with history, so you won't miss events that happened before you
subscribed as long as history hasn't been evicted.

If you need to receive events from *all* executions, subscribe inside
`spawn_exit_monitor` or hook into the `ContainerService` layer where hubs
are created.

### Option B — Read typed rows from the DB (any language / process)

Use this for asynchronous consumers that can tolerate some delay, external
services, or anything running outside the Rust process.

```sql
-- Poll for new normalized events after the last seen seq
SELECT rowid as seq, logs, msg_type
FROM execution_process_logs
WHERE execution_id = ?
  AND msg_type IN ('normalized_upsert', 'normalized_remove', 'normalized_finished')
  AND rowid > ?   -- your last seen seq
ORDER BY rowid ASC
LIMIT 200;
```

Each row's `logs` column is a JSONL block. Parse each line as
`NormalizedLogEvent` (the same JSON schema exported to TypeScript via
`shared/types.ts`).

The TypeScript shape:

```ts
type NormalizedLogEvent =
  | { type: 'upsert_entry'; index: number; entry: NormalizedEntry }
  | { type: 'remove_entry'; index: number }
  | { type: 'finished' };
```

### Option C — Subscribe via WebSocket (external process, any language)

Connect to `ws://<host>/execution-processes/<exec_id>/normalized-logs/ws`
with optional `?after_seq=<n>`. The protocol is described in the
[WS Wire Protocol](#ws-wire-protocol) section above.

This is the same endpoint the frontend uses. It is suitable for:
- Mobile clients
- External dashboard tools
- CI/CD status reporters
- Any IM bot that runs outside the main server process

Example reconnect loop in pseudo-code:

```
lastSeq = 0
loop:
  ws = connect("ws://host/execution-processes/{id}/normalized-logs/ws?after_seq={lastSeq}")
  for msg in ws:
    process(msg.event)
    lastSeq = msg.seq
  sleep(1s)  # reconnect
```

---

## Adding a New `NormalizedEntryType`

1. Add the variant to `NormalizedEntryType` in
   `crates/executors/src/logs/mod.rs`.
2. Populate it from whichever executor produces it (use
   `upsert_normalized_entry` / `add_normalized_entry` helpers in
   `crates/executors/src/logs/utils/patch.rs`).
3. Run `cargo test --workspace` to regenerate the `shared/types.ts` export
   (via `ts_rs`).
4. Update any frontend `switch` statements that exhaustively match
   `NormalizedEntryType`.

No changes are needed to the bus layer (`ExecutionLogHub`, `MsgStore`,
`stream_normalized_logs`, the WS handler, or the DB schema).

---

## Known Constraints & Future Work

| Area | Current state | Recommended next step |
|---|---|---|
| History eviction | Hub ring buffer caps at 20 000 events; older events are dropped on live streams | Add a DB-backed fallback for long-running executions that exceed the cap |
| Broadcast lag | `RecvError::Lagged` from the broadcast channel silently drops events in fast-burst scenarios | Emit a synthetic `gap` event or restart from DB history when lag is detected |
| Cross-execution fan-out | No built-in bus for "all executions" — consumers must subscribe per-hub | Add a global `ExecutionEventBus` at the `ContainerService` level |
| IM consumer lifecycle | No standard lifecycle hook for registering/unregistering subscribers | Introduce a `ConsumerRegistry` trait in `ContainerService` |
| Seq continuity on reconnect | Synthetic seqs (live) and real rowids (DB) share the same `after_seq` cursor but may not be contiguous | Document the gap; consider always using rowid-based seqs even for live events |
