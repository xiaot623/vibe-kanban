import {
  Loader2,
  Send,
  StopCircle,
  AlertCircle,
  Clock,
  X,
  Paperclip,
  Terminal,
  MessageSquare,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Alert, AlertDescription } from '@/components/ui/alert';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from '@/components/ui/tooltip';
//
import { useEffect, useMemo, useRef, useState, useCallback } from 'react';
import { ScratchType, type TaskWithAttemptStatus } from 'shared/types';
import { useBranchStatus } from '@/hooks';
import { useAttemptRepo } from '@/hooks/useAttemptRepo';
import { useAttemptExecution } from '@/hooks/useAttemptExecution';
import { useUserSystem } from '@/components/ConfigProvider';
import { cn } from '@/lib/utils';
//
import { useReview } from '@/contexts/ReviewProvider';
import { useClickedElements } from '@/contexts/ClickedElementsProvider';
import { useEntries } from '@/contexts/EntriesContext';
import { useKeySubmitFollowUp, Scope } from '@/keyboard';
import { useHotkeysContext } from 'react-hotkeys-hook';
import { useProject } from '@/contexts/ProjectContext';
//
import { VariantSelector } from '@/components/tasks/VariantSelector';
import { useAttemptBranch } from '@/hooks/useAttemptBranch';
import { FollowUpConflictSection } from '@/components/tasks/follow-up/FollowUpConflictSection';
import { ClickedElementsBanner } from '@/components/tasks/ClickedElementsBanner';
import WYSIWYGEditor from '@/components/ui/wysiwyg';
import { useRetryUi } from '@/contexts/RetryUiContext';
import { useFollowUpSend } from '@/hooks/useFollowUpSend';
import { AgentSelector } from '@/components/tasks/AgentSelector';
import type {
  BaseCodingAgent,
  DraftFollowUpData,
  ExecutorProfileId,
} from 'shared/types';
import { extractProfileFromAction, getVariantOptions } from '@/utils/executor';
import { buildResolveConflictsInstructions } from '@/lib/conflicts';
import { useTranslation } from 'react-i18next';
import { useScratch } from '@/hooks/useScratch';
import { useDebouncedCallback } from '@/hooks/useDebouncedCallback';
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { queueApi } from '@/lib/api';
import type { QueueStatus } from 'shared/types';
import { imagesApi, attemptsApi } from '@/lib/api';
import { PrCommentsDialog } from '@/components/dialogs/tasks/PrCommentsDialog';
import type { NormalizedComment } from '@/components/ui/wysiwyg/nodes/pr-comment-node';
import type { Session } from 'shared/types';

interface TaskFollowUpSectionProps {
  task: TaskWithAttemptStatus;
  session?: Session;
}

export function TaskFollowUpSection({
  task,
  session,
}: TaskFollowUpSectionProps) {
  const { t } = useTranslation('tasks');
  const { projectId } = useProject();

  // Derive IDs from session
  const workspaceId = session?.workspace_id;
  const sessionId = session?.id;

  const { isAttemptRunning, stopExecution, isStopping, processes } =
    useAttemptExecution(workspaceId, task.id);

  const { data: branchStatus, refetch: refetchBranchStatus } =
    useBranchStatus(workspaceId);
  const { repos, selectedRepoId } = useAttemptRepo(workspaceId);

  const getSelectedRepoId = useCallback(() => {
    return selectedRepoId ?? repos[0]?.id;
  }, [selectedRepoId, repos]);

  const repoWithConflicts = useMemo(
    () =>
      branchStatus?.find(
        (r) => r.is_rebase_in_progress || (r.conflicted_files?.length ?? 0) > 0
      ),
    [branchStatus]
  );
  const { branch: attemptBranch, refetch: refetchAttemptBranch } =
    useAttemptBranch(workspaceId);
  const { profiles } = useUserSystem();
  const { comments, generateReviewMarkdown, clearComments } = useReview();
  const {
    generateMarkdown: generateClickedMarkdown,
    clearElements: clearClickedElements,
  } = useClickedElements();
  const { enableScope, disableScope } = useHotkeysContext();

  const reviewMarkdown = useMemo(
    () => generateReviewMarkdown(),
    [generateReviewMarkdown]
  );

  const clickedMarkdown = useMemo(
    () => generateClickedMarkdown(),
    [generateClickedMarkdown]
  );

  // Non-editable conflict resolution instructions (derived, like review comments)
  const conflictResolutionInstructions = useMemo(() => {
    if (!repoWithConflicts?.conflicted_files?.length) return null;
    return buildResolveConflictsInstructions(
      attemptBranch,
      repoWithConflicts.target_branch_name,
      repoWithConflicts.conflicted_files,
      repoWithConflicts.conflict_op ?? null,
      repoWithConflicts.repo_name
    );
  }, [attemptBranch, repoWithConflicts]);

  // Editor state (persisted via scratch)
  const {
    scratch,
    updateScratch,
    isLoading: isScratchLoading,
  } = useScratch(ScratchType.DRAFT_FOLLOW_UP, sessionId ?? '');

  // Derive the message and variant from scratch
  const scratchData: DraftFollowUpData | undefined =
    scratch?.payload?.type === 'DRAFT_FOLLOW_UP'
      ? scratch.payload.data
      : undefined;

  // Track whether the follow-up textarea is focused
  const [isTextareaFocused, setIsTextareaFocused] = useState(false);

  // Local message state for immediate UI feedback (before debounced save)
  const [localMessage, setLocalMessage] = useState('');
  const lastSavedReviewMarkdownRef = useRef<string | null>(null);

  // Executor/variant selection - derive defaults from latest process
  const latestProfileId = useMemo<ExecutorProfileId | null>(() => {
    if (!processes?.length) return null;
    return (
      processes
        .slice()
        .reverse()
        .map((p) => extractProfileFromAction(p.executor_action ?? null))
        .find((pid) => pid !== null) ?? null
    );
  }, [processes]);

  const processExecutor = latestProfileId?.executor ?? null;
  const processVariant = latestProfileId?.variant ?? null;
  const [userSelectedExecutor, setUserSelectedExecutor] = useState<
    BaseCodingAgent | null | undefined
  >(undefined);
  const [userSelectedVariant, setUserSelectedVariant] = useState<
    string | null | undefined
  >(undefined);
  const selectedExecutor =
    userSelectedExecutor ?? scratchData?.executor ?? processExecutor ?? null;

  const currentProfile = useMemo(() => {
    if (!selectedExecutor) return null;
    return profiles?.[selectedExecutor] ?? null;
  }, [selectedExecutor, profiles]);

  const variantOptions = useMemo(
    () => getVariantOptions(selectedExecutor, profiles),
    [selectedExecutor, profiles]
  );

  const normalizeVariantForOptions = useCallback(
    (variant: string | null | undefined, options: string[]) => {
      if (variant == null) return null;
      if (options.includes(variant)) return variant;
      if (options.includes('DEFAULT')) return 'DEFAULT';
      return options[0] ?? null;
    },
    []
  );

  const scratchVariant = scratchData ? scratchData.variant : undefined;
  const selectedVariant = useMemo(() => {
    const preferredVariant =
      userSelectedVariant !== undefined
        ? userSelectedVariant
        : scratchVariant !== undefined
          ? scratchVariant
          : processVariant;

    return normalizeVariantForOptions(preferredVariant, variantOptions);
  }, [
    userSelectedVariant,
    scratchVariant,
    processVariant,
    normalizeVariantForOptions,
    variantOptions,
  ]);

  // Refs to track current variant/executor for message save callback
  const variantRef = useRef<string | null>(selectedVariant);
  useEffect(() => {
    variantRef.current = selectedVariant;
  }, [selectedVariant]);
  const executorRef = useRef<BaseCodingAgent | null>(selectedExecutor);
  useEffect(() => {
    executorRef.current = selectedExecutor;
  }, [selectedExecutor]);

  // Refs to stabilize callbacks - avoid re-creating callbacks when these values change
  const scratchRef = useRef(scratch);
  useEffect(() => {
    scratchRef.current = scratch;
  }, [scratch]);

  // Save scratch helper (used for both message and variant changes)
  // Uses scratchRef to avoid callback invalidation when scratch updates
  const saveToScratch = useCallback(
    async (
      message: string,
      variant: string | null,
      executor: BaseCodingAgent | null
    ) => {
      if (!workspaceId) return;
      // Don't create empty scratch entries - only save if there's actual content,
      // a profile override is selected, or scratch already exists (to allow clearing a draft)
      if (!message.trim() && !variant && !executor && !scratchRef.current)
        return;
      try {
        await updateScratch({
          payload: {
            type: 'DRAFT_FOLLOW_UP',
            data: { message, variant, executor },
          },
        });
      } catch (e) {
        console.error('Failed to save follow-up draft', e);
      }
    },
    [workspaceId, updateScratch]
  );

  // Wrapper to update variant and save to scratch immediately
  const setSelectedVariant = useCallback(
    (variant: string | null) => {
      const normalizedVariant = normalizeVariantForOptions(
        variant,
        variantOptions
      );
      setUserSelectedVariant(normalizedVariant);
      // Save immediately when user changes variant
      saveToScratch(localMessage, normalizedVariant, selectedExecutor);
    },
    [
      normalizeVariantForOptions,
      variantOptions,
      saveToScratch,
      localMessage,
      selectedExecutor,
    ]
  );

  const setSelectedExecutor = useCallback(
    (profile: ExecutorProfileId) => {
      const nextExecutor = profile.executor;
      const nextVariantOptions = getVariantOptions(nextExecutor, profiles);
      const nextVariant = normalizeVariantForOptions(
        selectedVariant,
        nextVariantOptions
      );

      setUserSelectedExecutor(nextExecutor);
      setUserSelectedVariant(nextVariant);
      saveToScratch(localMessage, nextVariant, nextExecutor);
    },
    [
      profiles,
      normalizeVariantForOptions,
      selectedVariant,
      saveToScratch,
      localMessage,
    ]
  );

  // Debounced save for message changes (uses current profile state from refs)
  const { debounced: setFollowUpMessage, cancel: cancelDebouncedSave } =
    useDebouncedCallback(
      useCallback(
        (value: string) =>
          saveToScratch(value, variantRef.current, executorRef.current),
        [saveToScratch]
      ),
      500
    );

  // Sync local message from scratch when it loads (but not while user is typing)
  useEffect(() => {
    if (isScratchLoading) return;
    if (isTextareaFocused) return; // Don't overwrite while user is typing
    setLocalMessage(scratchData?.message ?? '');
  }, [isScratchLoading, scratchData?.message, isTextareaFocused]);

  // During retry, follow-up box is greyed/disabled (not hidden)
  // Use RetryUi context so optimistic retry immediately disables this box
  const { activeRetryProcessId } = useRetryUi();
  const isRetryActive = !!activeRetryProcessId;

  // Queue status for queuing follow-up messages while agent is running
  const queryClient = useQueryClient();
  const QUEUE_STATUS_KEY = 'queue-status';

  const {
    data: queueStatus = { status: 'empty' as const },
    refetch: refreshQueueStatus,
  } = useQuery<QueueStatus>({
    queryKey: [QUEUE_STATUS_KEY, sessionId],
    queryFn: () => queueApi.getStatus(sessionId!),
    enabled: !!sessionId,
  });

  const { data: persistedReviewCommand } = useQuery({
    queryKey: ['review-command', workspaceId],
    queryFn: () => attemptsApi.getReviewCommand(workspaceId!),
    enabled: !!workspaceId,
  });

  const hasPersistedReviewCommand = Boolean(
    persistedReviewCommand &&
      !persistedReviewCommand.solved &&
      persistedReviewCommand.markdown_text.trim()
  );

  useEffect(() => {
    if (!workspaceId) {
      lastSavedReviewMarkdownRef.current = null;
      return;
    }
    const persistedMarkdown = persistedReviewCommand?.markdown_text?.trim();
    if (persistedMarkdown) {
      lastSavedReviewMarkdownRef.current = persistedMarkdown;
    }
  }, [
    workspaceId,
    persistedReviewCommand?.markdown_text,
    persistedReviewCommand?.updated_at,
  ]);

  const isQueued = queueStatus.status === 'queued';
  const queuedMessage = isQueued
    ? (queueStatus as Extract<QueueStatus, { status: 'queued' }>).message
    : null;

  const queueMutation = useMutation({
    mutationFn: ({
      message,
      variant,
      executor,
    }: {
      message: string;
      variant: string | null;
      executor: BaseCodingAgent | null;
    }) => queueApi.queue(sessionId!, { message, variant, executor }),
    onSuccess: (status) => {
      queryClient.setQueryData([QUEUE_STATUS_KEY, sessionId], status);
    },
  });

  const cancelMutation = useMutation({
    mutationFn: () => queueApi.cancel(sessionId!),
    onSuccess: (status) => {
      queryClient.setQueryData([QUEUE_STATUS_KEY, sessionId], status);
    },
  });

  const queueMessage = useCallback(
    async (
      message: string,
      variant: string | null,
      executor: BaseCodingAgent | null
    ) => {
      if (!sessionId) return;
      await queueMutation.mutateAsync({ message, variant, executor });
    },
    [sessionId, queueMutation]
  );

  const cancelQueue = useCallback(async () => {
    if (!sessionId) return;
    await cancelMutation.mutateAsync();
  }, [sessionId, cancelMutation]);

  const isQueueLoading = queueMutation.isPending || cancelMutation.isPending;

  // Track previous process count to detect new processes
  const prevProcessCountRef = useRef(processes.length);

  // Refresh queue status when execution stops OR when a new process starts
  useEffect(() => {
    const prevCount = prevProcessCountRef.current;
    prevProcessCountRef.current = processes.length;

    if (!workspaceId) return;

    // Refresh when execution stops
    if (!isAttemptRunning) {
      refreshQueueStatus();
      return;
    }

    // Refresh when a new process starts (could be queued message consumption or follow-up)
    if (processes.length > prevCount) {
      refreshQueueStatus();
      // Re-sync local message from current scratch state
      // If scratch was deleted, scratchData will be undefined, so localMessage becomes ''
      setLocalMessage(scratchData?.message ?? '');
    }
  }, [
    isAttemptRunning,
    workspaceId,
    processes.length,
    refreshQueueStatus,
    scratchData?.message,
  ]);

  // When queued, display the queued message content so user can edit it
  const displayMessage =
    isQueued && queuedMessage ? queuedMessage.data.message : localMessage;

  // Check if there's a pending approval - users shouldn't be able to type during approvals
  const { entries } = useEntries();
  const hasPendingApproval = useMemo(() => {
    return entries.some((entry) => {
      if (entry.type !== 'NORMALIZED_ENTRY') return false;
      const entryType = entry.content.entry_type;
      return (
        entryType.type === 'tool_use' &&
        entryType.status.status === 'pending_approval'
      );
    });
  }, [entries]);

  // Send follow-up action
  const { isSendingFollowUp, followUpError, setFollowUpError, onSendFollowUp } =
    useFollowUpSend({
      sessionId,
      workspaceId,
      message: localMessage,
      conflictMarkdown: conflictResolutionInstructions,
      clickedMarkdown,
      selectedVariant,
      selectedExecutor,
      clearComments,
      clearClickedElements,
      onAfterSendCleanup: () => {
        cancelDebouncedSave(); // Cancel any pending debounced save to avoid race condition
        setLocalMessage(''); // Clear local state immediately
        // Scratch deletion is handled by the backend when the queued message is consumed
      },
    });

  // Separate logic for when textarea should be disabled vs when send button should be disabled
  const canTypeFollowUp = useMemo(() => {
    if (!workspaceId || processes.length === 0 || isSendingFollowUp) {
      return false;
    }

    if (isRetryActive) return false; // disable typing while retry editor is active
    if (hasPendingApproval) return false; // disable typing during approval
    // Note: isQueued no longer blocks typing - editing auto-cancels the queue
    return true;
  }, [
    workspaceId,
    processes.length,
    isSendingFollowUp,
    isRetryActive,
    hasPendingApproval,
  ]);

  const canSendFollowUp = useMemo(() => {
    if (!canTypeFollowUp) {
      return false;
    }

    // Allow sending if conflict instructions, review comments, clicked elements, or message is present
    return Boolean(
      conflictResolutionInstructions ||
        reviewMarkdown ||
        hasPersistedReviewCommand ||
        clickedMarkdown ||
        localMessage.trim()
    );
  }, [
    canTypeFollowUp,
    conflictResolutionInstructions,
    reviewMarkdown,
    hasPersistedReviewCommand,
    clickedMarkdown,
    localMessage,
  ]);
  const isEditable = !isRetryActive && !hasPendingApproval;

  const hasAnyScript = true;

  const handleRunSetupScript = useCallback(async () => {
    if (!workspaceId || isAttemptRunning) return;
    try {
      await attemptsApi.runSetupScript(workspaceId);
    } catch (error) {
      console.error('Failed to run setup script:', error);
    }
  }, [workspaceId, isAttemptRunning]);

  const handleRunCleanupScript = useCallback(async () => {
    if (!workspaceId || isAttemptRunning) return;
    try {
      await attemptsApi.runCleanupScript(workspaceId);
    } catch (error) {
      console.error('Failed to run cleanup script:', error);
    }
  }, [workspaceId, isAttemptRunning]);

  const persistReviewCommand = useCallback(
    async (markdown: string) => {
      if (!workspaceId) return true;
      if (!markdown) {
        if (comments.length > 0 && hasPersistedReviewCommand) {
          try {
            await attemptsApi.deleteReviewCommand(workspaceId);
            lastSavedReviewMarkdownRef.current = null;
            queryClient.invalidateQueries({
              queryKey: ['review-command', workspaceId],
            });
          } catch (error) {
            console.error('Failed to delete review command:', error);
            setFollowUpError('Failed to delete review command');
            return false;
          }
        }
        return true;
      }
      if (markdown === lastSavedReviewMarkdownRef.current) return true;

      try {
        await attemptsApi.saveReviewCommand(workspaceId, {
          markdown_text: markdown,
          reason: null,
        });
        lastSavedReviewMarkdownRef.current = markdown;
        queryClient.invalidateQueries({
          queryKey: ['review-command', workspaceId],
        });
        return true;
      } catch (error) {
        console.error('Failed to save review command:', error);
        setFollowUpError('Failed to save review command');
        return false;
      }
    },
    [
      workspaceId,
      comments.length,
      hasPersistedReviewCommand,
      queryClient,
      setFollowUpError,
    ]
  );

  const persistReviewCommandIfNeeded = useCallback(
    () => persistReviewCommand(reviewMarkdown.trim()),
    [persistReviewCommand, reviewMarkdown]
  );

  const {
    debounced: debouncedPersistReviewCommand,
    cancel: cancelPersistReviewCommand,
  } = useDebouncedCallback(
    useCallback(
      (markdown: string) => {
        void persistReviewCommand(markdown);
      },
      [persistReviewCommand]
    ),
    400
  );

  useEffect(() => {
    const markdown = reviewMarkdown.trim();
    if (!workspaceId) {
      cancelPersistReviewCommand();
      return;
    }
    if (!markdown) {
      cancelPersistReviewCommand();
      if (comments.length > 0 && hasPersistedReviewCommand) {
        void persistReviewCommand('');
      }
      return;
    }
    if (markdown === lastSavedReviewMarkdownRef.current) return;
    debouncedPersistReviewCommand(markdown);
  }, [
    workspaceId,
    comments.length,
    hasPersistedReviewCommand,
    persistReviewCommand,
    reviewMarkdown,
    debouncedPersistReviewCommand,
    cancelPersistReviewCommand,
  ]);

  useEffect(() => {
    return () => {
      cancelPersistReviewCommand();
    };
  }, [cancelPersistReviewCommand]);

  // Handler to queue the current message for execution after agent finishes
  const handleQueueMessage = useCallback(async () => {
    try {
      if (
        !localMessage.trim() &&
        !conflictResolutionInstructions &&
        !reviewMarkdown &&
        !hasPersistedReviewCommand &&
        !clickedMarkdown
      ) {
        return;
      }

      cancelPersistReviewCommand();
      const saved = await persistReviewCommandIfNeeded();
      if (!saved) return;

      // Cancel any pending debounced save and save immediately before queueing
      // This prevents the race condition where the debounce fires after queueing
      cancelDebouncedSave();
      await saveToScratch(localMessage, selectedVariant, selectedExecutor);

      const baseParts = [
        conflictResolutionInstructions,
        clickedMarkdown,
        localMessage.trim(),
      ].filter(Boolean) as string[];

      const unsolvedReviewCommandMarkdowns = workspaceId
        ? (await attemptsApi.getUnsolvedReviewCommands(workspaceId)).map(
            (command) => command.markdown_text.trim()
          )
        : [];

      const commandParts = unsolvedReviewCommandMarkdowns.filter(
        (commandMarkdown) =>
          commandMarkdown &&
          !baseParts.some((part) => part.includes(commandMarkdown))
      );

      const combinedMessage = [...baseParts, ...commandParts].join('\n\n');
      if (!combinedMessage) return;
      await queueMessage(combinedMessage, selectedVariant, selectedExecutor);
    } catch (error) {
      console.error('Failed to queue follow-up message:', error);
      setFollowUpError('Failed to queue follow-up message');
    }
  }, [
    localMessage,
    conflictResolutionInstructions,
    reviewMarkdown,
    clickedMarkdown,
    hasPersistedReviewCommand,
    selectedVariant,
    selectedExecutor,
    persistReviewCommandIfNeeded,
    workspaceId,
    queueMessage,
    cancelDebouncedSave,
    cancelPersistReviewCommand,
    saveToScratch,
    setFollowUpError,
  ]);

  // Ref to access setFollowUpMessage without adding it as a dependency
  const setFollowUpMessageRef = useRef(setFollowUpMessage);
  useEffect(() => {
    setFollowUpMessageRef.current = setFollowUpMessage;
  }, [setFollowUpMessage]);

  // Ref for followUpError to use in stable onChange handler
  const followUpErrorRef = useRef(followUpError);
  useEffect(() => {
    followUpErrorRef.current = followUpError;
  }, [followUpError]);

  // Helper to get current queue state from cache (avoids ref-sync pattern)
  const getQueueState = useCallback(() => {
    const status = queryClient.getQueryData<QueueStatus>([
      QUEUE_STATUS_KEY,
      sessionId,
    ]);
    const queued = status?.status === 'queued';
    const message = queued
      ? (status as Extract<QueueStatus, { status: 'queued' }>).message
      : null;
    return { isQueued: queued, queuedMessage: message };
  }, [queryClient, sessionId]);

  const handleSendFollowUp = useCallback(async () => {
    cancelPersistReviewCommand();
    const saved = await persistReviewCommandIfNeeded();
    if (!saved) return;
    await onSendFollowUp();
  }, [
    cancelPersistReviewCommand,
    persistReviewCommandIfNeeded,
    onSendFollowUp,
  ]);

  // Keyboard shortcut handler - send follow-up or queue depending on state
  const handleSubmitShortcut = useCallback(
    (e?: KeyboardEvent) => {
      e?.preventDefault();
      if (isAttemptRunning) {
        // When running, CMD+Enter queues the message (if not already queued)
        if (!isQueued) {
          handleQueueMessage();
        }
      } else {
        handleSendFollowUp();
      }
    },
    [isAttemptRunning, isQueued, handleQueueMessage, handleSendFollowUp]
  );

  // Handle image paste - upload to container and insert markdown
  const handlePasteFiles = useCallback(
    async (files: File[]) => {
      if (!workspaceId) return;

      for (const file of files) {
        try {
          const response = await imagesApi.uploadForAttempt(workspaceId, file);
          // Append markdown image to current message
          const imageMarkdown = `![${response.original_name}](${response.file_path})`;

          // If queued, cancel queue and use queued message as base (same as editor change behavior)
          const {
            isQueued: currentlyQueued,
            queuedMessage: currentQueuedMessage,
          } = getQueueState();
          if (currentlyQueued && currentQueuedMessage) {
            cancelMutation.mutate();
            const base = currentQueuedMessage.data.message;
            const newMessage = base
              ? `${base}\n\n${imageMarkdown}`
              : imageMarkdown;
            setLocalMessage(newMessage);
            setFollowUpMessageRef.current(newMessage);
          } else {
            setLocalMessage((prev) => {
              const newMessage = prev
                ? `${prev}\n\n${imageMarkdown}`
                : imageMarkdown;
              setFollowUpMessageRef.current(newMessage); // Debounced save to scratch
              return newMessage;
            });
          }
        } catch (error) {
          console.error('Failed to upload image:', error);
        }
      }
    },
    [workspaceId, getQueueState, cancelMutation]
  );

  // Attachment button - file input ref and handlers
  const fileInputRef = useRef<HTMLInputElement>(null);
  const handleAttachClick = useCallback(() => {
    fileInputRef.current?.click();
  }, []);
  const handleFileInputChange = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      const files = Array.from(e.target.files || []).filter((f) =>
        f.type.startsWith('image/')
      );
      if (files.length > 0) {
        handlePasteFiles(files);
      }
      // Reset input so same file can be selected again
      e.target.value = '';
    },
    [handlePasteFiles]
  );

  // Handler for PR comments insertion
  const handlePrCommentClick = useCallback(async () => {
    if (!workspaceId) return;
    const repoId = getSelectedRepoId();
    if (!repoId) return;

    const result = await PrCommentsDialog.show({
      attemptId: workspaceId,
      repoId,
    });
    if (result.comments.length > 0) {
      // Build markdown for all selected comments
      const markdownBlocks = result.comments.map((comment) => {
        const payload: NormalizedComment = {
          id:
            comment.comment_type === 'general'
              ? comment.id
              : comment.id.toString(),
          comment_type: comment.comment_type,
          author: comment.author,
          body: comment.body,
          created_at: comment.created_at,
          url: comment.url,
          // Include review-specific fields when available
          ...(comment.comment_type === 'review' && {
            path: comment.path,
            line: comment.line != null ? Number(comment.line) : null,
            diff_hunk: comment.diff_hunk,
          }),
        };
        return '```gh-comment\n' + JSON.stringify(payload, null, 2) + '\n```';
      });

      const markdown = markdownBlocks.join('\n\n');

      // Same pattern as image paste
      const { isQueued: currentlyQueued, queuedMessage: currentQueuedMessage } =
        getQueueState();
      if (currentlyQueued && currentQueuedMessage) {
        cancelMutation.mutate();
        const base = currentQueuedMessage.data.message;
        const newMessage = base ? `${base}\n\n${markdown}` : markdown;
        setLocalMessage(newMessage);
        setFollowUpMessageRef.current(newMessage);
      } else {
        setLocalMessage((prev) => {
          const newMessage = prev ? `${prev}\n\n${markdown}` : markdown;
          setFollowUpMessageRef.current(newMessage);
          return newMessage;
        });
      }
    }
  }, [workspaceId, getSelectedRepoId, getQueueState, cancelMutation]);

  // Stable onChange handler for WYSIWYGEditor
  const handleEditorChange = useCallback(
    (value: string) => {
      // Auto-cancel queue when user starts editing
      const { isQueued: currentlyQueued } = getQueueState();
      if (currentlyQueued) {
        cancelMutation.mutate();
      }
      setLocalMessage(value); // Immediate update for UI responsiveness
      setFollowUpMessageRef.current(value); // Debounced save to scratch
      if (followUpErrorRef.current) setFollowUpError(null);
    },
    [setFollowUpError, getQueueState, cancelMutation]
  );

  // Memoize placeholder to avoid re-renders
  const hasExtraContext = !!(
    reviewMarkdown ||
    hasPersistedReviewCommand ||
    conflictResolutionInstructions
  );
  const editorPlaceholder = useMemo(
    () =>
      hasExtraContext
        ? '(Optional) Add additional instructions... Type @ to insert tags, search files, or use @review.'
        : 'Continue working on this task attempt... Type @ to insert tags, search files, or use @review.',
    [hasExtraContext]
  );

  // Register keyboard shortcuts
  useKeySubmitFollowUp(handleSubmitShortcut, {
    scope: Scope.FOLLOW_UP_READY,
    enableOnFormTags: ['textarea', 'TEXTAREA'],
    when: canSendFollowUp && isEditable,
  });

  // Enable FOLLOW_UP scope when textarea is focused AND editable
  useEffect(() => {
    if (isEditable && isTextareaFocused) {
      enableScope(Scope.FOLLOW_UP);
    } else {
      disableScope(Scope.FOLLOW_UP);
    }
    return () => {
      disableScope(Scope.FOLLOW_UP);
    };
  }, [isEditable, isTextareaFocused, enableScope, disableScope]);

  // Enable FOLLOW_UP_READY scope when ready to send
  useEffect(() => {
    const isReady = isTextareaFocused && isEditable;

    if (isReady) {
      enableScope(Scope.FOLLOW_UP_READY);
    } else {
      disableScope(Scope.FOLLOW_UP_READY);
    }
    return () => {
      disableScope(Scope.FOLLOW_UP_READY);
    };
  }, [isTextareaFocused, isEditable, enableScope, disableScope]);

  // When a process completes (e.g., agent resolved conflicts), refresh branch status promptly
  const prevRunningRef = useRef<boolean>(isAttemptRunning);
  useEffect(() => {
    if (prevRunningRef.current && !isAttemptRunning && workspaceId) {
      refetchBranchStatus();
      refetchAttemptBranch();
    }
    prevRunningRef.current = isAttemptRunning;
  }, [
    isAttemptRunning,
    workspaceId,
    refetchBranchStatus,
    refetchAttemptBranch,
  ]);

  if (!workspaceId) return null;

  if (isScratchLoading) {
    return (
      <div className="flex items-center justify-center h-full">
        <Loader2 className="animate-spin h-6 w-6" />
      </div>
    );
  }

  return (
    <div
      className={cn(
        'grid h-full min-h-0 grid-rows-[minmax(0,1fr)_auto] overflow-hidden',
        isRetryActive && 'opacity-50'
      )}
    >
      {/* Scrollable content area */}
      <div className="overflow-y-auto min-h-0 p-4">
        <div className="space-y-2">
          {followUpError && (
            <Alert variant="destructive">
              <AlertCircle className="h-4 w-4" />
              <AlertDescription>{followUpError}</AlertDescription>
            </Alert>
          )}
          <div className="space-y-2">
            {/* Conflict notice and actions (optional UI) */}
            {branchStatus && (
              <FollowUpConflictSection
                workspaceId={workspaceId}
                attemptBranch={attemptBranch}
                branchStatus={branchStatus}
                isEditable={isEditable}
                onResolve={handleSendFollowUp}
                enableResolve={
                  canSendFollowUp && !isAttemptRunning && isEditable
                }
                enableAbort={canSendFollowUp && !isAttemptRunning}
                conflictResolutionInstructions={conflictResolutionInstructions}
              />
            )}

            {/* Clicked elements notice and actions */}
            <ClickedElementsBanner />

            {/* Queued message indicator */}
            {isQueued && queuedMessage && (
              <div className="flex items-center gap-2 text-sm text-muted-foreground bg-muted p-3 rounded-md border">
                <Clock className="h-4 w-4 flex-shrink-0" />
                <div className="font-medium">
                  {t(
                    'followUp.queuedMessage',
                    'Message queued - will execute when current run finishes'
                  )}
                </div>
              </div>
            )}

            <div
              className="flex flex-col gap-2"
              onFocus={() => {
                if (isTextareaFocused) return;
                setIsTextareaFocused(true);
              }}
              onBlur={(e) => {
                // Only blur if focus is leaving the container entirely
                if (!e.currentTarget.contains(e.relatedTarget)) {
                  setIsTextareaFocused(false);
                }
              }}
            >
              <WYSIWYGEditor
                placeholder={editorPlaceholder}
                value={displayMessage}
                onChange={handleEditorChange}
                disabled={!isEditable}
                onPasteFiles={handlePasteFiles}
                workspaceId={workspaceId}
                projectId={projectId}
                taskAttemptId={workspaceId}
                onCmdEnter={handleSubmitShortcut}
                className="min-h-[40px]"
              />
            </div>
          </div>
        </div>
      </div>

      {/* Always-visible action bar */}
      <div className="p-4">
        <div className="flex flex-row gap-2 items-center">
          <div className="flex-1 flex gap-2">
            <AgentSelector
              profiles={profiles}
              selectedExecutorProfile={
                selectedExecutor
                  ? {
                      executor: selectedExecutor,
                      variant: selectedVariant,
                    }
                  : null
              }
              onChange={setSelectedExecutor}
              disabled={!isEditable}
            />
            <VariantSelector
              currentProfile={currentProfile}
              selectedVariant={selectedVariant}
              onChange={setSelectedVariant}
              disabled={!isEditable}
            />
          </div>

          {/* Hidden file input for attachment - always present */}
          <input
            ref={fileInputRef}
            type="file"
            accept="image/*"
            multiple
            className="hidden"
            onChange={handleFileInputChange}
          />

          {/* Attach button - always visible */}
          <Button
            onClick={handleAttachClick}
            disabled={!isEditable}
            size="sm"
            variant="outline"
            title="Attach image"
            aria-label="Attach image"
          >
            <Paperclip className="h-4 w-4" />
          </Button>

          {/* PR Comments button */}
          <Button
            onClick={handlePrCommentClick}
            disabled={!isEditable}
            size="sm"
            variant="outline"
            title="Insert PR comment"
            aria-label="Insert PR comment"
          >
            <MessageSquare className="h-4 w-4" />
          </Button>

          {/* Scripts dropdown - only show if project has any scripts */}
          {hasAnyScript && (
            <DropdownMenu>
              <TooltipProvider>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <DropdownMenuTrigger asChild>
                      <Button
                        size="sm"
                        variant="outline"
                        disabled={isAttemptRunning}
                        aria-label="Run scripts"
                      >
                        <Terminal className="h-4 w-4" />
                      </Button>
                    </DropdownMenuTrigger>
                  </TooltipTrigger>
                  {isAttemptRunning && (
                    <TooltipContent side="bottom">
                      {t('followUp.scriptsDisabledWhileRunning')}
                    </TooltipContent>
                  )}
                </Tooltip>
              </TooltipProvider>
              <DropdownMenuContent align="end">
                <DropdownMenuItem onClick={handleRunSetupScript}>
                  {t('followUp.runSetupScript')}
                </DropdownMenuItem>
                <DropdownMenuItem onClick={handleRunCleanupScript}>
                  {t('followUp.runCleanupScript')}
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          )}

          {isAttemptRunning ? (
            <div className="flex items-center gap-2">
              {/* Queue/Cancel Queue button when running */}
              {isQueued ? (
                <Button
                  onClick={cancelQueue}
                  disabled={isQueueLoading}
                  size="sm"
                  variant="outline"
                >
                  {isQueueLoading ? (
                    <Loader2 className="animate-spin h-4 w-4 mr-2" />
                  ) : (
                    <>
                      <X className="h-4 w-4 mr-2" />
                      {t('followUp.cancelQueue', 'Cancel Queue')}
                    </>
                  )}
                </Button>
              ) : (
                <Button
                  onClick={handleQueueMessage}
                  disabled={
                    isQueueLoading ||
                    (!localMessage.trim() &&
                      !conflictResolutionInstructions &&
                      !reviewMarkdown &&
                      !hasPersistedReviewCommand &&
                      !clickedMarkdown)
                  }
                  size="sm"
                >
                  {isQueueLoading ? (
                    <Loader2 className="animate-spin h-4 w-4 mr-2" />
                  ) : (
                    <>
                      <Clock className="h-4 w-4 mr-2" />
                      {t('followUp.queue', 'Queue')}
                    </>
                  )}
                </Button>
              )}
              <Button
                onClick={stopExecution}
                disabled={isStopping}
                size="sm"
                variant="destructive"
              >
                {isStopping ? (
                  <Loader2 className="animate-spin h-4 w-4 mr-2" />
                ) : (
                  <>
                    <StopCircle className="h-4 w-4 mr-2" />
                    {t('followUp.stop')}
                  </>
                )}
              </Button>
            </div>
          ) : (
            <div className="flex items-center gap-2">
              {comments.length > 0 && (
                <Button
                  onClick={clearComments}
                  size="sm"
                  variant="destructive"
                  disabled={!isEditable}
                >
                  {t('followUp.clearReviewComments')}
                </Button>
              )}
              <Button
                onClick={handleSendFollowUp}
                disabled={!canSendFollowUp || !isEditable}
                size="sm"
              >
                {isSendingFollowUp ? (
                  <Loader2 className="animate-spin h-4 w-4 mr-2" />
                ) : (
                  <>
                    <Send className="h-4 w-4 mr-2" />
                    {conflictResolutionInstructions
                      ? t('followUp.resolveConflicts')
                      : t('followUp.send')}
                  </>
                )}
              </Button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
