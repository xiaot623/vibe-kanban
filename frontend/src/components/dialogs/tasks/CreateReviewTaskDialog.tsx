import { useEffect, useMemo, useState } from 'react';
import { useMutation, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { ExecutorProfileSelector } from '@/components/settings';
import { useUserSystem } from '@/components/ConfigProvider';
import { attemptsApi } from '@/lib/api';
import { defineModal } from '@/lib/modals';
import { paths } from '@/lib/paths';
import { useNavigateWithSearch } from '@/hooks';
import { taskRelationshipsKeys } from '@/hooks/useTaskRelationships';
import { taskKeys } from '@/hooks/useTask';
import { Scope, useKeySubmitTask } from '@/keyboard';
import NiceModal, { useModal } from '@ebay/nice-modal-react';
import type { ExecutorProfileId } from 'shared/types';

export interface CreateReviewTaskDialogProps {
  attemptId: string;
  projectId: string;
}

const CreateReviewTaskDialogImpl =
  NiceModal.create<CreateReviewTaskDialogProps>(({ attemptId, projectId }) => {
    const modal = useModal();
    const navigate = useNavigateWithSearch();
    const queryClient = useQueryClient();
    const { t } = useTranslation('tasks');
    const { profiles, config } = useUserSystem();

    const [userSelectedProfile, setUserSelectedProfile] =
      useState<ExecutorProfileId | null>(null);

    const defaultProfile = useMemo(
      () => config?.review_executor_profile ?? config?.executor_profile ?? null,
      [config?.review_executor_profile, config?.executor_profile]
    );

    const effectiveProfile = userSelectedProfile ?? defaultProfile;

    const createReviewTaskMutation = useMutation({
      mutationFn: (executorProfileId: ExecutorProfileId) =>
        attemptsApi.startReviewSubtask(attemptId, {
          executor_profile_id: executorProfileId,
        }),
      onSuccess: (task) => {
        queryClient.invalidateQueries({ queryKey: taskKeys.all });
        queryClient.invalidateQueries({
          queryKey: taskRelationshipsKeys.byAttempt(attemptId),
        });
        navigate(`${paths.task(projectId, task.id)}/attempts/latest`);
        modal.hide();
      },
    });

    useEffect(() => {
      if (!modal.visible) {
        setUserSelectedProfile(null);
      }
    }, [modal.visible]);

    const canCreate = Boolean(
      effectiveProfile && !createReviewTaskMutation.isPending
    );

    const handleCreate = async () => {
      if (!effectiveProfile) return;
      try {
        await createReviewTaskMutation.mutateAsync(effectiveProfile);
      } catch (err) {
        console.error('Failed to create review task:', err);
      }
    };

    const handleOpenChange = (open: boolean) => {
      if (!open) modal.hide();
    };

    useKeySubmitTask(handleCreate, {
      enabled: modal.visible && canCreate,
      scope: Scope.DIALOG,
      preventDefault: true,
    });

    const errorMessage =
      createReviewTaskMutation.error instanceof Error
        ? createReviewTaskMutation.error.message
        : createReviewTaskMutation.error
          ? t('createReviewTaskDialog.error')
          : null;

    return (
      <Dialog open={modal.visible} onOpenChange={handleOpenChange}>
        <DialogContent className="sm:max-w-[500px]">
          <DialogHeader>
            <DialogTitle>{t('createReviewTaskDialog.title')}</DialogTitle>
            <DialogDescription>
              {t('createReviewTaskDialog.description')}
            </DialogDescription>
          </DialogHeader>

          <div className="space-y-4 py-4">
            {profiles && (
              <ExecutorProfileSelector
                profiles={profiles}
                selectedProfile={effectiveProfile}
                onProfileSelect={setUserSelectedProfile}
                showLabel={true}
              />
            )}

            {errorMessage && (
              <div className="text-sm text-destructive">{errorMessage}</div>
            )}
          </div>

          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => modal.hide()}
              disabled={createReviewTaskMutation.isPending}
            >
              {t('common:buttons.cancel')}
            </Button>
            <Button onClick={handleCreate} disabled={!canCreate}>
              {createReviewTaskMutation.isPending
                ? t('createReviewTaskDialog.creating')
                : t('createReviewTaskDialog.start')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    );
  });

export const CreateReviewTaskDialog = defineModal<
  CreateReviewTaskDialogProps,
  void
>(CreateReviewTaskDialogImpl);
