// useConversationHistory.ts
import {
  CommandExitStatus,
  ExecutionProcess,
  ExecutionProcessStatus,
  ExecutorAction,
  NormalizedEntry,
  PatchType,
  ToolStatus,
  Workspace,
} from 'shared/types';
import { useExecutionProcessesContext } from '@/contexts/ExecutionProcessesContext';
import { useCallback, useEffect, useMemo, useRef } from 'react';
import { streamJsonPatchEntries } from '@/utils/streamJsonPatchEntries';
import {
  type IndexedNormalizedEntry,
  streamNormalizedLogEvents,
} from '@/utils/streamNormalizedLogEvents';

export type PatchTypeWithKey = PatchType & {
  patchKey: string;
  executionProcessId: string;
};

export type AddEntryType = 'initial' | 'running' | 'historic' | 'plan';

export type OnEntriesUpdated = (
  newEntries: PatchTypeWithKey[],
  addType: AddEntryType,
  loading: boolean
) => void;

type ExecutionProcessStaticInfo = {
  id: string;
  created_at: string;
  updated_at: string;
  executor_action: ExecutorAction;
};

type ExecutionProcessState = {
  executionProcess: ExecutionProcessStaticInfo;
  entries: PatchTypeWithKey[];
};

type ExecutionProcessStateStore = Record<string, ExecutionProcessState>;

interface UseConversationHistoryParams {
  attempt: Workspace;
  onEntriesUpdated: OnEntriesUpdated;
}

interface UseConversationHistoryResult {}

const MIN_INITIAL_ENTRIES = 10;
const REMAINING_BATCH_SIZE = 50;
const HISTORIC_LOG_PAGE_LIMIT = 400;
const MAX_HISTORIC_LOG_PAGES = 200;
const MAX_SAFE_CURSOR = Number.MAX_SAFE_INTEGER;
class StreamCancelledError extends Error {
  constructor() {
    super('stream-cancelled');
    this.name = 'StreamCancelledError';
  }
}

const isStreamCancelledError = (error: unknown): boolean =>
  error instanceof StreamCancelledError;

const makeLoadingPatch = (executionProcessId: string): PatchTypeWithKey => ({
  type: 'NORMALIZED_ENTRY',
  content: {
    entry_type: {
      type: 'loading',
    },
    content: '',
    timestamp: null,
  },
  patchKey: `${executionProcessId}:loading`,
  executionProcessId,
});

const nextActionPatch: (
  failed: boolean,
  execution_processes: number,
  needs_setup: boolean,
  setup_help_text?: string
) => PatchTypeWithKey = (
  failed,
  execution_processes,
  needs_setup,
  setup_help_text
) => ({
  type: 'NORMALIZED_ENTRY',
  content: {
    entry_type: {
      type: 'next_action',
      failed: failed,
      execution_processes: execution_processes,
      needs_setup: needs_setup,
      setup_help_text: setup_help_text ?? null,
    },
    content: '',
    timestamp: null,
  },
  patchKey: 'next_action',
  executionProcessId: '',
});

const normalizedEntriesToPatches = (entries: NormalizedEntry[]): PatchType[] =>
  entries.map((entry) => ({
    type: 'NORMALIZED_ENTRY',
    content: entry,
  }));

export const useConversationHistory = ({
  attempt,
  onEntriesUpdated,
}: UseConversationHistoryParams): UseConversationHistoryResult => {
  const {
    executionProcessesVisible: executionProcessesRaw,
    isLoading: isExecutionProcessesLoading,
    isConnected: isExecutionProcessesConnected,
  } = useExecutionProcessesContext();
  const executionProcesses = useRef<ExecutionProcess[]>(executionProcessesRaw);
  const displayedExecutionProcesses = useRef<ExecutionProcessStateStore>({});
  const loadedInitialEntries = useRef(false);
  const streamingProcessIdsRef = useRef<Set<string>>(new Set());
  const lifecycleGenerationRef = useRef(0);
  const activeStreamCancelsRef = useRef<Map<string, Set<() => void>>>(
    new Map()
  );
  const onEntriesUpdatedRef = useRef<OnEntriesUpdated | null>(null);

  const mergeIntoDisplayed = (
    mutator: (state: ExecutionProcessStateStore) => void
  ) => {
    const state = displayedExecutionProcesses.current;
    mutator(state);
  };

  const isGenerationCurrent = useCallback(
    (generation: number) => lifecycleGenerationRef.current === generation,
    []
  );

  const registerActiveStreamCancel = useCallback(
    (executionProcessId: string, cancel: () => void) => {
      const cancelsForProcess =
        activeStreamCancelsRef.current.get(executionProcessId) ?? new Set();
      cancelsForProcess.add(cancel);
      activeStreamCancelsRef.current.set(executionProcessId, cancelsForProcess);

      return () => {
        const currentCancels =
          activeStreamCancelsRef.current.get(executionProcessId);
        if (!currentCancels) return;

        currentCancels.delete(cancel);
        if (currentCancels.size === 0) {
          activeStreamCancelsRef.current.delete(executionProcessId);
        }
      };
    },
    []
  );

  const cancelAllActiveStreams = useCallback(() => {
    const allCancels = [...activeStreamCancelsRef.current.values()].flatMap(
      (cancelsForProcess) => [...cancelsForProcess]
    );
    activeStreamCancelsRef.current.clear();
    allCancels.forEach((cancel) => {
      cancel();
    });
    streamingProcessIdsRef.current.clear();
  }, []);

  const clearDisplayedState = useCallback(() => {
    displayedExecutionProcesses.current = {};
    loadedInitialEntries.current = false;
  }, []);

  useEffect(() => {
    onEntriesUpdatedRef.current = onEntriesUpdated;
  }, [onEntriesUpdated]);

  const isConversationExecutorAction = useCallback((action: ExecutorAction) => {
    return (
      action.typ.type === 'CodingAgentFollowUpRequest' ||
      action.typ.type === 'CodingAgentInitialRequest' ||
      action.typ.type === 'ReviewRequest'
    );
  }, []);

  const buildLogStreamUrl = useCallback(
    (
      executionProcess: ExecutionProcess,
      opts: { afterSeq?: number; beforeSeq?: number } = {}
    ): { url: string; supportsCursor: boolean } => {
      const { afterSeq, beforeSeq } = opts;
      if (executionProcess.executor_action.typ.type === 'ScriptRequest') {
        const params = new URLSearchParams();
        params.set('limit', String(HISTORIC_LOG_PAGE_LIMIT));
        if (afterSeq !== undefined) {
          params.set('after_seq', String(afterSeq));
        }
        return {
          url: `/api/execution-processes/${executionProcess.id}/raw-logs/ws?${params.toString()}`,
          supportsCursor: false,
        };
      }

      const params = new URLSearchParams();
      params.set('limit', String(HISTORIC_LOG_PAGE_LIMIT));
      if (afterSeq !== undefined) {
        params.set('after_seq', String(afterSeq));
      }
      if (beforeSeq !== undefined) {
        params.set('before_seq', String(beforeSeq));
      }

      return {
        url: `/api/execution-processes/${executionProcess.id}/normalized-logs/ws?${params.toString()}`,
        supportsCursor: true,
      };
    },
    []
  );

  // Keep executionProcesses up to date
  useEffect(() => {
    executionProcesses.current = executionProcessesRaw.filter(
      (ep) =>
        ep.run_reason === 'setupscript' ||
        ep.run_reason === 'cleanupscript' ||
        ep.run_reason === 'codingagent'
    );
  }, [executionProcessesRaw]);

  const loadEntriesForHistoricExecutionProcess = useCallback(
    async (executionProcess: ExecutionProcess, generation: number) => {
      if (!isGenerationCurrent(generation)) {
        return [];
      }

      let accumulatedEntries: PatchType[] = [];
      let accumulatedIndexedEntries: IndexedNormalizedEntry[] = [];
      let accumulatedResolvedIndexes: number[] = [];
      let afterSeq: number | undefined;
      let beforeSeq: number | undefined;

      for (let page = 0; page < MAX_HISTORIC_LOG_PAGES; page++) {
        if (!isGenerationCurrent(generation)) {
          return [];
        }

        const { url, supportsCursor } = buildLogStreamUrl(
          executionProcess,
          executionProcess.executor_action.typ.type === 'ScriptRequest'
            ? { afterSeq }
            : { beforeSeq: beforeSeq ?? MAX_SAFE_CURSOR }
        );
        const isScriptStream =
          executionProcess.executor_action.typ.type === 'ScriptRequest';
        const initialEntries = accumulatedEntries;
        const initialIndexedEntries = accumulatedIndexedEntries;
        const initialResolvedIndexes = accumulatedResolvedIndexes;
        const pageResult = await new Promise<{
          entries: PatchType[];
          lastSeq?: number;
          indexedEntries?: IndexedNormalizedEntry[];
          resolvedIndexes?: number[];
        }>((resolve) => {
          let settled = false;
          let unregisterCancel: () => void = () => {};

          const settle = (
            entries: PatchType[],
            lastSeq?: number,
            indexedEntries?: IndexedNormalizedEntry[],
            resolvedIndexes?: number[]
          ) => {
            if (settled) return;
            settled = true;
            unregisterCancel();
            resolve({ entries, lastSeq, indexedEntries, resolvedIndexes });
          };

          if (isScriptStream) {
            const controller = streamJsonPatchEntries<PatchType>(url, {
              initial: {
                entries: initialEntries,
              },
              onFinished: (allEntries, meta) => {
                if (!isGenerationCurrent(generation)) {
                  settle([], meta.lastSeq);
                  return;
                }
                settle(allEntries, meta.lastSeq);
              },
              onError: (err) => {
                if (!isGenerationCurrent(generation)) {
                  controller.close();
                  settle([], controller.getLastSeq());
                  return;
                }
                console.warn(
                  `Error loading entries for historic execution process ${executionProcess.id}`,
                  err
                );
                controller.close();
                settle(initialEntries, controller.getLastSeq());
              },
              onClose: () => {
                settle(initialEntries);
              },
            });

            unregisterCancel = registerActiveStreamCancel(
              executionProcess.id,
              () => controller.close()
            );
            return;
          }

          const controller = streamNormalizedLogEvents(url, {
            initialIndexedEntries,
            onFinished: (normalizedEntries, meta) => {
              if (!isGenerationCurrent(generation)) {
                settle([], meta.lastSeq);
                return;
              }
              settle(
                normalizedEntriesToPatches(normalizedEntries),
                meta.lastSeq,
                meta.indexedEntries,
                meta.resolvedIndexes
              );
            },
            onError: (err) => {
              if (!isGenerationCurrent(generation)) {
                controller.close();
                settle([], controller.getLastSeq());
                return;
              }
              console.warn(
                `Error loading entries for historic execution process ${executionProcess.id}`,
                err
              );
              controller.close();
              settle(
                initialEntries,
                controller.getLastSeq(),
                initialIndexedEntries,
                initialResolvedIndexes
              );
            },
            onClose: () => {
              settle(
                initialEntries,
                undefined,
                initialIndexedEntries,
                initialResolvedIndexes
              );
            },
            initialResolvedIndexes,
            applyMode: 'last_write_wins',
          });

          unregisterCancel = registerActiveStreamCancel(
            executionProcess.id,
            () => controller.close()
          );
        });

        if (!isGenerationCurrent(generation)) {
          return [];
        }

        accumulatedEntries = pageResult.entries;
        accumulatedIndexedEntries =
          pageResult.indexedEntries ?? accumulatedIndexedEntries;
        accumulatedResolvedIndexes =
          pageResult.resolvedIndexes ?? accumulatedResolvedIndexes;

        if (!supportsCursor || pageResult.lastSeq === undefined) {
          break;
        }
        if (isScriptStream) {
          if (afterSeq !== undefined && pageResult.lastSeq <= afterSeq) {
            break;
          }
          afterSeq = pageResult.lastSeq;
          continue;
        }
        if (beforeSeq !== undefined && pageResult.lastSeq >= beforeSeq) {
          break;
        }
        beforeSeq = pageResult.lastSeq;
      }

      return accumulatedEntries;
    },
    [buildLogStreamUrl, isGenerationCurrent, registerActiveStreamCancel]
  );

  const getLiveExecutionProcess = (
    executionProcessId: string
  ): ExecutionProcess | undefined => {
    return executionProcesses?.current.find(
      (executionProcess) => executionProcess.id === executionProcessId
    );
  };

  const patchWithKey = (
    patch: PatchType,
    executionProcessId: string,
    index: number | 'user'
  ) => {
    return {
      ...patch,
      patchKey: `${executionProcessId}:${index}`,
      executionProcessId,
    };
  };

  const countConversationEntries = useCallback(
    (executionProcessState: ExecutionProcessStateStore): number => {
      let count = 0;
      for (const processState of Object.values(executionProcessState)) {
        if (
          isConversationExecutorAction(
            processState.executionProcess.executor_action
          )
        ) {
          count += processState.entries.length;
        }
      }
      return count;
    },
    [isConversationExecutorAction]
  );

  const getActiveAgentProcesses = (): ExecutionProcess[] => {
    return (
      executionProcesses?.current.filter(
        (p) =>
          p.status === ExecutionProcessStatus.running &&
          p.run_reason !== 'devserver'
      ) ?? []
    );
  };

  const flattenEntriesForEmit = useCallback(
    (executionProcessState: ExecutionProcessStateStore): PatchTypeWithKey[] => {
      // Flags to control Next Action bar emit
      let hasPendingApproval = false;
      let hasRunningProcess = false;
      let lastProcessFailedOrKilled = false;
      let needsSetup = false;
      let setupHelpText: string | undefined;

      const orderedProcesses = Object.values(executionProcessState).sort(
        (a, b) =>
          new Date(
            a.executionProcess.created_at as unknown as string
          ).getTime() -
          new Date(b.executionProcess.created_at as unknown as string).getTime()
      );
      const processCount = orderedProcesses.length;

      // Create user messages + tool calls for setup/cleanup scripts
      const allEntries = orderedProcesses.flatMap((p, index) => {
        const entries: PatchTypeWithKey[] = [];
        if (
          p.executionProcess.executor_action.typ.type ===
            'CodingAgentInitialRequest' ||
          p.executionProcess.executor_action.typ.type ===
            'CodingAgentFollowUpRequest' ||
          p.executionProcess.executor_action.typ.type === 'ReviewRequest'
        ) {
          // New user message
          const actionType = p.executionProcess.executor_action.typ;
          const userNormalizedEntry: NormalizedEntry = {
            entry_type: {
              type: 'user_message',
            },
            content: actionType.prompt,
            timestamp: null,
          };
          const userPatch: PatchType = {
            type: 'NORMALIZED_ENTRY',
            content: userNormalizedEntry,
          };
          const userPatchTypeWithKey = patchWithKey(
            userPatch,
            p.executionProcess.id,
            'user'
          );
          entries.push(userPatchTypeWithKey);

          // Remove all coding agent added user messages, replace with our custom one
          const entriesExcludingUser = p.entries.filter(
            (e) =>
              e.type !== 'NORMALIZED_ENTRY' ||
              e.content.entry_type.type !== 'user_message'
          );

          const hasPendingApprovalEntry = entriesExcludingUser.some((entry) => {
            if (entry.type !== 'NORMALIZED_ENTRY') return false;
            const entryType = entry.content.entry_type;
            return (
              entryType.type === 'tool_use' &&
              entryType.status.status === 'pending_approval'
            );
          });

          if (hasPendingApprovalEntry) {
            hasPendingApproval = true;
          }

          entries.push(...entriesExcludingUser);

          const liveProcessStatus = getLiveExecutionProcess(
            p.executionProcess.id
          )?.status;
          const isProcessRunning =
            liveProcessStatus === ExecutionProcessStatus.running;
          const processFailedOrKilled =
            liveProcessStatus === ExecutionProcessStatus.failed ||
            liveProcessStatus === ExecutionProcessStatus.killed;

          if (isProcessRunning) {
            hasRunningProcess = true;
          }

          if (processFailedOrKilled && index === processCount - 1) {
            lastProcessFailedOrKilled = true;

            // Check if this failed process has a SetupRequired entry
            const hasSetupRequired = entriesExcludingUser.some((entry) => {
              if (entry.type !== 'NORMALIZED_ENTRY') return false;
              if (
                entry.content.entry_type.type === 'error_message' &&
                entry.content.entry_type.error_type.type === 'setup_required'
              ) {
                setupHelpText = entry.content.content;
                return true;
              }
              return false;
            });

            if (hasSetupRequired) {
              needsSetup = true;
            }
          }

          if (isProcessRunning && !hasPendingApprovalEntry) {
            entries.push(makeLoadingPatch(p.executionProcess.id));
          }
        } else if (
          p.executionProcess.executor_action.typ.type === 'ScriptRequest'
        ) {
          // Add setup and cleanup script as a tool call
          let toolName = '';
          switch (p.executionProcess.executor_action.typ.context) {
            case 'SetupScript':
              toolName = 'Setup Script';
              break;
            case 'CleanupScript':
              toolName = 'Cleanup Script';
              break;
            case 'ToolInstallScript':
              toolName = 'Tool Install Script';
              break;
            default:
              return [];
          }

          const executionProcess = getLiveExecutionProcess(
            p.executionProcess.id
          );

          if (executionProcess?.status === ExecutionProcessStatus.running) {
            hasRunningProcess = true;
          }

          if (
            (executionProcess?.status === ExecutionProcessStatus.failed ||
              executionProcess?.status === ExecutionProcessStatus.killed) &&
            index === processCount - 1
          ) {
            lastProcessFailedOrKilled = true;
          }

          const exitCode = Number(executionProcess?.exit_code) || 0;
          const exit_status: CommandExitStatus | null =
            executionProcess?.status === 'running'
              ? null
              : {
                  type: 'exit_code',
                  code: exitCode,
                };

          const toolStatus: ToolStatus =
            executionProcess?.status === ExecutionProcessStatus.running
              ? { status: 'created' }
              : exitCode === 0
                ? { status: 'success' }
                : { status: 'failed' };

          const output = p.entries.map((line) => line.content).join('\n');

          const toolNormalizedEntry: NormalizedEntry = {
            entry_type: {
              type: 'tool_use',
              tool_name: toolName,
              action_type: {
                action: 'command_run',
                command: p.executionProcess.executor_action.typ.script,
                result: {
                  output,
                  exit_status,
                },
              },
              status: toolStatus,
            },
            content: toolName,
            timestamp: null,
          };
          const toolPatch: PatchType = {
            type: 'NORMALIZED_ENTRY',
            content: toolNormalizedEntry,
          };
          const toolPatchWithKey: PatchTypeWithKey = patchWithKey(
            toolPatch,
            p.executionProcess.id,
            0
          );

          entries.push(toolPatchWithKey);
        }

        return entries;
      });

      // Emit the next action bar if no process running
      if (!hasRunningProcess && !hasPendingApproval) {
        allEntries.push(
          nextActionPatch(
            lastProcessFailedOrKilled,
            processCount,
            needsSetup,
            setupHelpText
          )
        );
      }

      return allEntries;
    },
    []
  );

  const emitEntries = useCallback(
    (
      executionProcessState: ExecutionProcessStateStore,
      addEntryType: AddEntryType,
      loading: boolean
    ) => {
      const entries = flattenEntriesForEmit(executionProcessState);
      let modifiedAddEntryType = addEntryType;

      // If this is a live-running emit and the last entry is a plan, emit special plan type.
      if (addEntryType === 'running' && entries.length > 0) {
        const lastEntry = entries[entries.length - 1];
        if (
          lastEntry.type === 'NORMALIZED_ENTRY' &&
          lastEntry.content.entry_type.type === 'tool_use' &&
          (lastEntry.content.entry_type.tool_name === 'ExitPlanMode' ||
            lastEntry.content.entry_type.action_type.action ===
              'plan_presentation')
        ) {
          modifiedAddEntryType = 'plan';
        }
      }

      onEntriesUpdatedRef.current?.(entries, modifiedAddEntryType, loading);
    },
    [flattenEntriesForEmit]
  );

  // This emits its own events as they are streamed
  const loadRunningAndEmit = useCallback(
    (executionProcess: ExecutionProcess, generation: number): Promise<void> => {
      return new Promise((resolve, reject) => {
        if (!isGenerationCurrent(generation)) {
          resolve();
          return;
        }

        let url = '';
        if (executionProcess.executor_action.typ.type === 'ScriptRequest') {
          url = `/api/execution-processes/${executionProcess.id}/raw-logs/ws`;
        } else {
          url = `/api/execution-processes/${executionProcess.id}/normalized-logs/ws`;
        }

        let settled = false;
        let unregisterCancel: () => void = () => {};

        const settleResolve = () => {
          if (settled) return;
          settled = true;
          unregisterCancel();
          resolve();
        };

        const settleReject = (error: unknown) => {
          if (settled) return;
          settled = true;
          unregisterCancel();
          reject(error);
        };

        const applyEntries = (entries: PatchType[]) => {
          const patchesWithKey = entries.map((entry, index) =>
            patchWithKey(entry, executionProcess.id, index)
          );
          let shouldEmit = true;
          mergeIntoDisplayed((state) => {
            const previousEntries = state[executionProcess.id]?.entries ?? [];
            if (patchesWithKey.length === 0 && previousEntries.length > 0) {
              shouldEmit = false;
              return;
            }
            state[executionProcess.id] = {
              executionProcess,
              entries: patchesWithKey,
            };
          });
          if (shouldEmit) {
            emitEntries(displayedExecutionProcesses.current, 'running', false);
          }
        };

        if (executionProcess.executor_action.typ.type === 'ScriptRequest') {
          const controller = streamJsonPatchEntries<PatchType>(url, {
            onEntries(entries) {
              if (!isGenerationCurrent(generation)) {
                controller.close();
                return;
              }
              applyEntries(entries);
            },
            onFinished: (entries) => {
              if (!isGenerationCurrent(generation)) {
                settleReject(new StreamCancelledError());
                return;
              }
              applyEntries(entries);
              settleResolve();
            },
            onError: (error) => {
              if (!isGenerationCurrent(generation)) {
                controller.close();
                settleReject(new StreamCancelledError());
                return;
              }
              controller.close();
              settleReject(error);
            },
            onClose: () => {
              if (settled) return;
              if (!isGenerationCurrent(generation)) {
                settleReject(new StreamCancelledError());
                return;
              }
              settleReject(
                new Error(
                  `Stream closed before completion for execution process ${executionProcess.id}`
                )
              );
            },
          });

          unregisterCancel = registerActiveStreamCancel(
            executionProcess.id,
            () => controller.close()
          );
          return;
        }

        const controller = streamNormalizedLogEvents(url, {
          onEntries(normalizedEntries) {
            if (!isGenerationCurrent(generation)) {
              controller.close();
              return;
            }
            applyEntries(normalizedEntriesToPatches(normalizedEntries));
          },
          onFinished: (normalizedEntries) => {
            if (!isGenerationCurrent(generation)) {
              settleReject(new StreamCancelledError());
              return;
            }
            applyEntries(normalizedEntriesToPatches(normalizedEntries));
            settleResolve();
          },
          onError: (error) => {
            if (!isGenerationCurrent(generation)) {
              controller.close();
              settleReject(new StreamCancelledError());
              return;
            }
            controller.close();
            settleReject(error);
          },
          onClose: () => {
            if (settled) return;
            if (!isGenerationCurrent(generation)) {
              settleReject(new StreamCancelledError());
              return;
            }
            settleReject(
              new Error(
                `Stream closed before completion for execution process ${executionProcess.id}`
              )
            );
          },
        });

        unregisterCancel = registerActiveStreamCancel(executionProcess.id, () =>
          controller.close()
        );
      });
    },
    [emitEntries, isGenerationCurrent, registerActiveStreamCancel]
  );

  // Sometimes it can take a few seconds for the stream to start, wrap the loadRunningAndEmit method
  const loadRunningAndEmitWithBackoff = useCallback(
    async (executionProcess: ExecutionProcess, generation: number) => {
      if (!isGenerationCurrent(generation)) return;

      for (let i = 0; i < 20; i++) {
        if (!isGenerationCurrent(generation)) return;

        try {
          await loadRunningAndEmit(executionProcess, generation);
          return;
        } catch (error) {
          if (
            !isGenerationCurrent(generation) ||
            isStreamCancelledError(error)
          ) {
            return;
          }
          await new Promise((resolve) => setTimeout(resolve, 500));
        }
      }
    },
    [isGenerationCurrent, loadRunningAndEmit]
  );

  const loadInitialEntries = useCallback(
    async (generation: number): Promise<ExecutionProcessStateStore> => {
      const localDisplayedExecutionProcesses: ExecutionProcessStateStore = {};
      let conversationEntryCount = 0;

      if (!executionProcesses?.current || !isGenerationCurrent(generation)) {
        return localDisplayedExecutionProcesses;
      }

      for (const executionProcess of [
        ...executionProcesses.current,
      ].reverse()) {
        if (!isGenerationCurrent(generation)) break;
        if (executionProcess.status === ExecutionProcessStatus.running)
          continue;

        const entries = await loadEntriesForHistoricExecutionProcess(
          executionProcess,
          generation
        );
        if (!isGenerationCurrent(generation)) break;
        const entriesWithKey = entries.map((e, idx) =>
          patchWithKey(e, executionProcess.id, idx)
        );

        localDisplayedExecutionProcesses[executionProcess.id] = {
          executionProcess,
          entries: entriesWithKey,
        };

        if (isConversationExecutorAction(executionProcess.executor_action)) {
          conversationEntryCount += entriesWithKey.length;
        }

        if (conversationEntryCount > MIN_INITIAL_ENTRIES) {
          break;
        }
      }

      return localDisplayedExecutionProcesses;
    },
    [
      executionProcesses,
      isGenerationCurrent,
      isConversationExecutorAction,
      loadEntriesForHistoricExecutionProcess,
    ]
  );

  const loadRemainingEntriesInBatches = useCallback(
    async (batchSize: number, generation: number): Promise<boolean> => {
      if (!executionProcesses?.current || !isGenerationCurrent(generation)) {
        return false;
      }

      let anyUpdated = false;
      let conversationEntryCount = countConversationEntries(
        displayedExecutionProcesses.current
      );
      for (const executionProcess of [
        ...executionProcesses.current,
      ].reverse()) {
        if (!isGenerationCurrent(generation)) return false;
        const current = displayedExecutionProcesses.current;
        if (
          current[executionProcess.id] ||
          executionProcess.status === ExecutionProcessStatus.running
        )
          continue;

        const entries = await loadEntriesForHistoricExecutionProcess(
          executionProcess,
          generation
        );
        if (!isGenerationCurrent(generation)) return false;
        const entriesWithKey = entries.map((e, idx) =>
          patchWithKey(e, executionProcess.id, idx)
        );

        mergeIntoDisplayed((state) => {
          state[executionProcess.id] = {
            executionProcess,
            entries: entriesWithKey,
          };
        });

        if (isConversationExecutorAction(executionProcess.executor_action)) {
          conversationEntryCount += entriesWithKey.length;
        }

        if (conversationEntryCount > batchSize) {
          anyUpdated = true;
          break;
        }
        anyUpdated = true;
      }
      return anyUpdated;
    },
    [
      countConversationEntries,
      executionProcesses,
      isGenerationCurrent,
      isConversationExecutorAction,
      loadEntriesForHistoricExecutionProcess,
    ]
  );

  const ensureProcessVisible = useCallback((p: ExecutionProcess) => {
    mergeIntoDisplayed((state) => {
      if (!state[p.id]) {
        state[p.id] = {
          executionProcess: {
            id: p.id,
            created_at: p.created_at,
            updated_at: p.updated_at,
            executor_action: p.executor_action,
          },
          entries: [],
        };
      }
    });
  }, []);

  const idListKey = useMemo(
    () => executionProcessesRaw?.map((p) => p.id).join(','),
    [executionProcessesRaw]
  );

  const idStatusKey = useMemo(
    () => executionProcessesRaw?.map((p) => `${p.id}:${p.status}`).join(','),
    [executionProcessesRaw]
  );

  // Reset stream lifecycle when attempt changes and on unmount.
  useEffect(() => {
    lifecycleGenerationRef.current += 1;
    cancelAllActiveStreams();
    clearDisplayedState();
    emitEntries(displayedExecutionProcesses.current, 'initial', true);

    return () => {
      lifecycleGenerationRef.current += 1;
      cancelAllActiveStreams();
      clearDisplayedState();
    };
  }, [attempt.id, cancelAllActiveStreams, clearDisplayedState, emitEntries]);

  // Initial load when attempt changes
  useEffect(() => {
    let cancelled = false;
    const generation = lifecycleGenerationRef.current;

    (async () => {
      if (!isGenerationCurrent(generation)) return;

      // Waiting for execution processes to load
      if (
        executionProcesses?.current.length === 0 ||
        isExecutionProcessesLoading ||
        loadedInitialEntries.current
      )
        return;

      // Initial entries
      const allInitialEntries = await loadInitialEntries(generation);
      if (cancelled || !isGenerationCurrent(generation)) return;
      mergeIntoDisplayed((state) => {
        Object.assign(state, allInitialEntries);
      });
      emitEntries(displayedExecutionProcesses.current, 'initial', false);
      loadedInitialEntries.current = true;

      // Then load the remaining in batches
      while (
        !cancelled &&
        isGenerationCurrent(generation) &&
        (await loadRemainingEntriesInBatches(REMAINING_BATCH_SIZE, generation))
      ) {
        if (cancelled || !isGenerationCurrent(generation)) return;
      }

      if (cancelled || !isGenerationCurrent(generation)) return;
      await new Promise((resolve) => setTimeout(resolve, 100));
      if (cancelled || !isGenerationCurrent(generation)) return;
      emitEntries(displayedExecutionProcesses.current, 'historic', false);
    })();

    return () => {
      cancelled = true;
    };
  }, [
    attempt.id,
    idListKey,
    loadInitialEntries,
    loadRemainingEntriesInBatches,
    emitEntries,
    isExecutionProcessesLoading,
    isGenerationCurrent,
  ]); // include idListKey so new processes trigger reload

  useEffect(() => {
    const generation = lifecycleGenerationRef.current;
    if (!isGenerationCurrent(generation)) return;

    const activeProcesses = getActiveAgentProcesses();
    if (activeProcesses.length === 0) return;

    for (const activeProcess of activeProcesses) {
      if (!isGenerationCurrent(generation)) return;

      if (!displayedExecutionProcesses.current[activeProcess.id]) {
        const runningOrInitial =
          Object.keys(displayedExecutionProcesses.current).length > 1
            ? 'running'
            : 'initial';
        ensureProcessVisible(activeProcess);
        emitEntries(
          displayedExecutionProcesses.current,
          runningOrInitial,
          false
        );
      }

      if (
        activeProcess.status === ExecutionProcessStatus.running &&
        !streamingProcessIdsRef.current.has(activeProcess.id)
      ) {
        streamingProcessIdsRef.current.add(activeProcess.id);
        loadRunningAndEmitWithBackoff(activeProcess, generation).finally(() => {
          if (!isGenerationCurrent(generation)) return;
          streamingProcessIdsRef.current.delete(activeProcess.id);
        });
      }
    }
  }, [
    attempt.id,
    idStatusKey,
    emitEntries,
    ensureProcessVisible,
    isGenerationCurrent,
    loadRunningAndEmitWithBackoff,
  ]);

  // If an execution process is removed, remove it from the state
  useEffect(() => {
    const generation = lifecycleGenerationRef.current;
    if (!isGenerationCurrent(generation)) return;

    if (
      !executionProcessesRaw ||
      isExecutionProcessesLoading ||
      !isExecutionProcessesConnected
    ) {
      return;
    }

    const removedProcessIds = Object.keys(
      displayedExecutionProcesses.current
    ).filter((id) => !executionProcessesRaw.some((p) => p.id === id));

    if (removedProcessIds.length > 0) {
      if (!isGenerationCurrent(generation)) return;
      mergeIntoDisplayed((state) => {
        removedProcessIds.forEach((id) => {
          delete state[id];
        });
      });
      emitEntries(displayedExecutionProcesses.current, 'historic', false);
    }
  }, [
    attempt.id,
    idListKey,
    executionProcessesRaw,
    isExecutionProcessesLoading,
    isExecutionProcessesConnected,
    emitEntries,
    isGenerationCurrent,
  ]);

  return {};
};
