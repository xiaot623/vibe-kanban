import { useCallback, useState } from 'react';
import { attemptsApi, sessionsApi } from '@/lib/api';
import type { CreateFollowUpAttempt } from 'shared/types';

type Args = {
  sessionId?: string;
  workspaceId?: string;
  message: string;
  conflictMarkdown: string | null;
  clickedMarkdown?: string;
  selectedVariant: string | null;
  clearComments: () => void;
  clearClickedElements?: () => void;
  onAfterSendCleanup: () => void;
};

export function useFollowUpSend({
  sessionId,
  workspaceId,
  message,
  conflictMarkdown,
  clickedMarkdown,
  selectedVariant,
  clearComments,
  clearClickedElements,
  onAfterSendCleanup,
}: Args) {
  const [isSendingFollowUp, setIsSendingFollowUp] = useState(false);
  const [followUpError, setFollowUpError] = useState<string | null>(null);

  const onSendFollowUp = useCallback(async () => {
    if (!sessionId) return;
    try {
      setIsSendingFollowUp(true);
      setFollowUpError(null);
      const unsolvedReviewCommandMarkdowns = workspaceId
        ? (await attemptsApi.getUnsolvedReviewCommands(workspaceId)).map(
            (command) => command.markdown_text.trim()
          )
        : [];
      const extraMessage = message.trim();
      const baseParts = [
        conflictMarkdown,
        clickedMarkdown?.trim(),
        extraMessage,
      ].filter(Boolean) as string[];
      const commandParts = unsolvedReviewCommandMarkdowns.filter(
        (commandMarkdown) =>
          commandMarkdown &&
          !baseParts.some((part) => part.includes(commandMarkdown))
      );
      const finalPrompt = [...baseParts, ...commandParts].join('\n\n');
      if (!finalPrompt) return;
      const body: CreateFollowUpAttempt = {
        prompt: finalPrompt,
        variant: selectedVariant,
        retry_process_id: null,
        force_when_dirty: null,
        perform_git_reset: null,
      };
      await sessionsApi.followUp(sessionId, body);
      clearComments();
      clearClickedElements?.();
      onAfterSendCleanup();
      // Don't call jumpToLogsTab() - preserves focus on the follow-up editor
    } catch (error: unknown) {
      const err = error as { message?: string };
      setFollowUpError(
        `Failed to start follow-up execution: ${err.message ?? 'Unknown error'}`
      );
    } finally {
      setIsSendingFollowUp(false);
    }
  }, [
    sessionId,
    workspaceId,
    message,
    conflictMarkdown,
    clickedMarkdown,
    selectedVariant,
    clearComments,
    clearClickedElements,
    onAfterSendCleanup,
  ]);

  return {
    isSendingFollowUp,
    followUpError,
    setFollowUpError,
    onSendFollowUp,
  } as const;
}
