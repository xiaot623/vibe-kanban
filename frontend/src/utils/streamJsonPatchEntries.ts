// streamJsonPatchEntries.ts - WebSocket JSON patch streaming utility
import { applyPatch, type Operation } from 'rfc6902';

type PatchContainer<E = unknown> = { entries: E[] };

interface StreamUpdateMeta {
  lastSeq?: number;
  appliedOps: number;
}

interface StreamFinishedMeta {
  lastSeq?: number;
}

export interface StreamOptions<E = unknown> {
  initial?: PatchContainer<E>;
  /** batch window in ms (defaults to 24ms) */
  batchWindowMs?: number;
  /** called after each successful patch application */
  onEntries?: (entries: E[], meta: StreamUpdateMeta) => void;
  onConnect?: () => void;
  onError?: (err: unknown) => void;
  /** called once when the socket closes */
  onClose?: () => void;
  /** called once when a "finished" event is received */
  onFinished?: (entries: E[], meta: StreamFinishedMeta) => void;
}

interface StreamController<E = unknown> {
  /** Current entries array (immutable snapshot) */
  getEntries(): E[];
  /** Full { entries } snapshot */
  getSnapshot(): PatchContainer<E>;
  /** Last rowid cursor observed from the server, if present */
  getLastSeq(): number | undefined;
  /** Best-effort connection state */
  isConnected(): boolean;
  /** Subscribe to updates; returns an unsubscribe function */
  onChange(cb: (entries: E[]) => void): () => void;
  /** Close the stream */
  close(): void;
}

const DEFAULT_BATCH_WINDOW_MS = 24;

/**
 * Connect to a WebSocket endpoint that emits JSON messages containing:
 *   {"JsonPatch": [{"op": "add", "path": "/entries/0", "value": {...}}, ...]}
 *   {"Finished": ""}
 *
 * Maintains an in-memory { entries: [] } snapshot and returns a controller.
 */
export function streamJsonPatchEntries<E = unknown>(
  url: string,
  opts: StreamOptions<E> = {}
): StreamController<E> {
  const batchWindowMs = Math.max(
    1,
    opts.batchWindowMs ?? DEFAULT_BATCH_WINDOW_MS
  );
  let connected = false;
  let snapshot: PatchContainer<E> = structuredClone(
    opts.initial ?? ({ entries: [] } as PatchContainer<E>)
  );
  let lastSeq: number | undefined;
  let pendingOps: Operation[] = [];
  let flushScheduled = false;
  let rafId: number | null = null;
  let timerId: number | null = null;

  const subscribers = new Set<(entries: E[]) => void>();
  let closed = false;
  let closeNotified = false;

  // Convert HTTP endpoint to WebSocket endpoint
  const wsUrl = url.replace(/^http/, 'ws');
  const ws = new WebSocket(wsUrl);

  const notifyClose = () => {
    if (closeNotified) return;
    closeNotified = true;
    subscribers.clear();
    opts.onClose?.();
  };

  const cancelScheduledFlush = () => {
    if (rafId !== null && typeof window !== 'undefined') {
      window.cancelAnimationFrame(rafId);
    }
    if (timerId !== null && typeof window !== 'undefined') {
      window.clearTimeout(timerId);
    }
    rafId = null;
    timerId = null;
    flushScheduled = false;
  };

  const flushPendingOps = () => {
    if (pendingOps.length === 0) return;

    const ops = pendingOps;
    pendingOps = [];

    try {
      const next = structuredClone(snapshot);
      applyPatch(next as unknown as object, ops);
      snapshot = next;

      opts.onEntries?.(snapshot.entries, {
        lastSeq,
        appliedOps: ops.length,
      });
      for (const cb of subscribers) {
        try {
          cb(snapshot.entries);
        } catch {
          /* swallow subscriber errors */
        }
      }
    } catch (err) {
      opts.onError?.(err);
    }
  };

  const scheduleFlush = () => {
    if (flushScheduled) return;
    flushScheduled = true;

    if (
      typeof window !== 'undefined' &&
      typeof window.requestAnimationFrame === 'function'
    ) {
      rafId = window.requestAnimationFrame(() => {
        rafId = null;
        if (!flushScheduled) return;
        flushScheduled = false;
        if (timerId !== null) {
          window.clearTimeout(timerId);
          timerId = null;
        }
        flushPendingOps();
      });

      timerId = window.setTimeout(() => {
        if (!flushScheduled) return;
        flushScheduled = false;
        if (rafId !== null) {
          window.cancelAnimationFrame(rafId);
          rafId = null;
        }
        timerId = null;
        flushPendingOps();
      }, batchWindowMs);
      return;
    }

    if (typeof window !== 'undefined') {
      timerId = window.setTimeout(() => {
        flushScheduled = false;
        timerId = null;
        flushPendingOps();
      }, batchWindowMs);
    }
  };

  const handleMessage = (event: MessageEvent) => {
    try {
      const msg = JSON.parse(event.data);

      if (typeof msg.seq === 'number' && Number.isFinite(msg.seq)) {
        lastSeq = msg.seq;
      }

      // Handle JsonPatch messages (from LogMsg::to_ws_message)
      if (msg.JsonPatch) {
        const raw = msg.JsonPatch as Operation[];
        const ops = dedupeOps(raw);
        if (ops.length > 0) {
          pendingOps.push(...ops);
          scheduleFlush();
        }
      }

      // Handle Finished messages
      if (msg.finished !== undefined) {
        cancelScheduledFlush();
        flushPendingOps();
        opts.onFinished?.(snapshot.entries, { lastSeq });
        ws.close();
      }
    } catch (err) {
      opts.onError?.(err);
    }
  };

  ws.addEventListener('open', () => {
    connected = true;
    opts.onConnect?.();
  });

  ws.addEventListener('message', handleMessage);

  ws.addEventListener('error', (err) => {
    connected = false;
    cancelScheduledFlush();
    opts.onError?.(err);
  });

  ws.addEventListener('close', () => {
    connected = false;
    closed = true;
    cancelScheduledFlush();
    flushPendingOps();
    notifyClose();
  });

  return {
    getEntries(): E[] {
      return snapshot.entries;
    },
    getSnapshot(): PatchContainer<E> {
      return snapshot;
    },
    getLastSeq(): number | undefined {
      return lastSeq;
    },
    isConnected(): boolean {
      return connected;
    },
    onChange(cb: (entries: E[]) => void): () => void {
      subscribers.add(cb);
      // push current state immediately
      cb(snapshot.entries);
      return () => subscribers.delete(cb);
    },
    close(): void {
      if (closed) {
        notifyClose();
        return;
      }
      closed = true;
      connected = false;
      cancelScheduledFlush();
      pendingOps = [];
      if (
        ws.readyState === WebSocket.CLOSING ||
        ws.readyState === WebSocket.CLOSED
      ) {
        notifyClose();
        return;
      }
      ws.close();
    },
  };
}

/**
 * Dedupe multiple ops that touch the same path within a single event.
 * Last write for a path wins, while preserving the overall left-to-right
 * order of the *kept* final operations.
 *
 * Example:
 *   add /entries/4, replace /entries/4  -> keep only the final replace
 */
function dedupeOps(ops: Operation[]): Operation[] {
  const lastIndexByPath = new Map<string, number>();
  ops.forEach((op, i) => lastIndexByPath.set(op.path, i));

  // Keep only the last op for each path, in ascending order of their final index
  const keptIndices = [...lastIndexByPath.values()].sort((a, b) => a - b);
  return keptIndices.map((i) => ops[i]!);
}
