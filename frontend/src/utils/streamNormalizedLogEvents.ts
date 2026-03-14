import type { NormalizedEntry } from 'shared/types';

export interface IndexedNormalizedEntry {
  index: number;
  entry: NormalizedEntry;
}

type ApplyMode = 'replay' | 'last_write_wins';

interface StreamUpdateMeta {
  lastSeq?: number;
  indexedEntries: IndexedNormalizedEntry[];
  resolvedIndexes: number[];
}

interface StreamFinishedMeta {
  lastSeq?: number;
  indexedEntries: IndexedNormalizedEntry[];
  resolvedIndexes: number[];
}

export interface StreamNormalizedEventOptions {
  initialIndexedEntries?: IndexedNormalizedEntry[];
  initialResolvedIndexes?: number[];
  applyMode?: ApplyMode;
  onEntries?: (entries: NormalizedEntry[], meta: StreamUpdateMeta) => void;
  onConnect?: () => void;
  onError?: (err: unknown) => void;
  onClose?: () => void;
  onFinished?: (entries: NormalizedEntry[], meta: StreamFinishedMeta) => void;
}

interface StreamController {
  getEntries(): NormalizedEntry[];
  getLastSeq(): number | undefined;
  close(): void;
}

type WsNormalizedLogEvent =
  | { type: 'upsert_entry'; index: number; entry: NormalizedEntry }
  | { type: 'remove_entry'; index: number }
  | { type: 'finished' };

export function streamNormalizedLogEvents(
  url: string,
  opts: StreamNormalizedEventOptions = {}
): StreamController {
  const wsUrl = url.replace(/^http/, 'ws');
  const ws = new WebSocket(wsUrl);
  // Use a Map keyed by index so that gaps and out-of-order arrivals are
  // handled correctly.  The dense array is derived on each emit.
  const entryMap = new Map<number, NormalizedEntry>(
    (opts.initialIndexedEntries ?? []).map(({ index, entry }) => [index, entry])
  );
  const resolvedIndexes = new Set<number>(opts.initialResolvedIndexes ?? []);
  const applyMode = opts.applyMode ?? 'replay';
  let lastSeq: number | undefined;
  let closed = false;

  const toIndexedEntries = (): IndexedNormalizedEntry[] => {
    const keys = Array.from(entryMap.keys()).sort((a, b) => a - b);
    return keys.map((index) => ({ index, entry: entryMap.get(index)! }));
  };

  const toArray = (): NormalizedEntry[] => {
    return toIndexedEntries().map(({ entry }) => entry);
  };

  const emitEntries = () => {
    opts.onEntries?.(toArray(), {
      lastSeq,
      indexedEntries: toIndexedEntries(),
      resolvedIndexes: Array.from(resolvedIndexes.values()),
    });
  };

  ws.addEventListener('open', () => {
    opts.onConnect?.();
  });

  ws.addEventListener('message', (event) => {
    try {
      const msg = JSON.parse(event.data);

      const normalizedEvent: WsNormalizedLogEvent | undefined = msg.event;
      if (normalizedEvent) {
        if (
          normalizedEvent.type !== 'finished' &&
          typeof msg.seq === 'number' &&
          Number.isFinite(msg.seq)
        ) {
          lastSeq = msg.seq;
        }

        switch (normalizedEvent.type) {
          case 'upsert_entry': {
            const index = Math.max(0, normalizedEvent.index);
            if (applyMode === 'last_write_wins' && resolvedIndexes.has(index)) {
              break;
            }
            resolvedIndexes.add(index);
            entryMap.set(index, normalizedEvent.entry);
            emitEntries();
            break;
          }
          case 'remove_entry': {
            const index = normalizedEvent.index;
            if (applyMode === 'last_write_wins' && resolvedIndexes.has(index)) {
              break;
            }
            resolvedIndexes.add(index);
            entryMap.delete(index);
            emitEntries();
            break;
          }
          case 'finished': {
            opts.onFinished?.(toArray(), {
              lastSeq,
              indexedEntries: toIndexedEntries(),
              resolvedIndexes: Array.from(resolvedIndexes.values()),
            });
            ws.close();
            break;
          }
          default:
            break;
        }
        return;
      }

      if (msg.finished !== undefined) {
        opts.onFinished?.(toArray(), {
          lastSeq,
          indexedEntries: toIndexedEntries(),
          resolvedIndexes: Array.from(resolvedIndexes.values()),
        });
        ws.close();
      }
    } catch (err) {
      opts.onError?.(err);
    }
  });

  ws.addEventListener('error', (err) => {
    opts.onError?.(err);
  });

  ws.addEventListener('close', () => {
    if (closed) return;
    closed = true;
    opts.onClose?.();
  });

  return {
    getEntries() {
      return toArray();
    },
    getLastSeq() {
      return lastSeq;
    },
    close() {
      if (closed) return;
      closed = true;
      ws.close();
    },
  };
}
