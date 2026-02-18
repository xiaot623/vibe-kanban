import { useEffect, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Loader2, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { attemptsApi } from '@/lib/api';

export const reviewCommandQueryKey = (attemptId: string) =>
  ['review-command', attemptId] as const;

interface ReviewCommandCardProps {
  attemptId: string;
}

const toDisplayTime = (time: string) => {
  const date = new Date(time);
  if (Number.isNaN(date.getTime())) return time;
  return date.toLocaleString();
};

export function ReviewCommandCard({ attemptId }: ReviewCommandCardProps) {
  const queryClient = useQueryClient();
  const { data: command, isLoading } = useQuery({
    queryKey: reviewCommandQueryKey(attemptId),
    queryFn: () => attemptsApi.getReviewCommand(attemptId),
    enabled: !!attemptId,
  });

  const [reasonDraft, setReasonDraft] = useState('');

  useEffect(() => {
    setReasonDraft(command?.reason ?? '');
  }, [command?.reason, command?.updated_at]);

  const normalizedReason = reasonDraft.trim();
  const normalizedServerReason = (command?.reason ?? '').trim();
  const reasonChanged = normalizedReason !== normalizedServerReason;

  const updateStatusMutation = useMutation({
    mutationFn: (data: { solved: boolean; reason: string | null }) =>
      attemptsApi.updateReviewCommandStatus(attemptId, data),
    onSuccess: () => {
      queryClient.invalidateQueries({
        queryKey: reviewCommandQueryKey(attemptId),
      });
    },
  });

  const deleteCommandMutation = useMutation({
    mutationFn: () => attemptsApi.deleteReviewCommand(attemptId),
    onSuccess: () => {
      queryClient.invalidateQueries({
        queryKey: reviewCommandQueryKey(attemptId),
      });
    },
  });

  const isPending =
    updateStatusMutation.isPending || deleteCommandMutation.isPending;

  if (!attemptId) return null;
  if (isLoading) {
    return (
      <div className="border rounded-md p-3 text-sm text-muted-foreground">
        <Loader2 className="h-4 w-4 animate-spin" />
      </div>
    );
  }
  if (!command) return null;

  return (
    <div className="mb-2 overflow-hidden rounded-md border bg-card">
      <div className="border-b px-3 py-2">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="text-sm font-medium leading-none">Review Command</div>
          <div
            className={
              command.solved
                ? 'rounded bg-green-500/10 px-2 py-0.5 text-xs font-medium text-green-700'
                : 'rounded bg-amber-500/10 px-2 py-0.5 text-xs font-medium text-amber-700'
            }
          >
            {command.solved ? 'Solved' : 'Unsolved'}
          </div>
        </div>
        <div className="mt-1 text-xs text-muted-foreground">
          Created: {toDisplayTime(command.created_at)} | Updated:{' '}
          {toDisplayTime(command.updated_at)}
        </div>
      </div>

      <div className="border-t px-3 py-2">
        <div>
          <label className="mb-1 block text-xs text-muted-foreground">
            Reason
          </label>
          <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
            <input
              value={reasonDraft}
              onChange={(e) => setReasonDraft(e.target.value)}
              disabled={isPending}
              className="h-8 w-full rounded-md border bg-background px-2 text-sm sm:flex-1"
              placeholder="Optional reason"
            />

            <div className="flex items-center gap-2">
              <Button
                size="sm"
                variant="outline"
                disabled={isPending || !reasonChanged}
                onClick={() =>
                  updateStatusMutation.mutate({
                    solved: command.solved,
                    reason: normalizedReason || null,
                  })
                }
              >
                {isPending ? (
                  <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                ) : null}
                Save Reason
              </Button>

              <Button
                size="sm"
                disabled={isPending}
                onClick={() =>
                  updateStatusMutation.mutate({
                    solved: !command.solved,
                    reason: normalizedReason || null,
                  })
                }
              >
                {isPending ? (
                  <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                ) : null}
                {command.solved ? 'Mark Unsolved' : 'Mark Solved'}
              </Button>

              <Button
                size="sm"
                variant="destructive"
                disabled={isPending}
                onClick={() => deleteCommandMutation.mutate()}
              >
                {isPending ? (
                  <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                ) : (
                  <Trash2 className="mr-2 h-4 w-4" />
                )}
                Delete
              </Button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
