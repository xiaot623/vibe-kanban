import { useTranslation } from 'react-i18next';
import { useProject } from '@/contexts/ProjectContext';
import { useTaskAttemptsWithSessions } from '@/hooks/useTaskAttempts';
import { useNavigateWithSearch } from '@/hooks';
import { paths } from '@/lib/paths';
import type { TaskWithAttemptStatus } from 'shared/types';
import type { WorkspaceWithSession } from '@/types/attempt';
import { NewCardContent } from '../ui/new-card';
import { Button } from '../ui/button';
import { PlusIcon } from 'lucide-react';
import { CreateAttemptDialog } from '@/components/dialogs/tasks/CreateAttemptDialog';
import { DataTable, type ColumnDef } from '@/components/ui/table';
import MarkdownEditor from '@/components/ui/markdown-editor';

interface TaskPanelProps {
  task: TaskWithAttemptStatus | null;
}

const TaskPanel = ({ task }: TaskPanelProps) => {
  const { t } = useTranslation('tasks');
  const navigate = useNavigateWithSearch();
  const { projectId } = useProject();

  const {
    data: attempts = [],
    isLoading: isAttemptsLoading,
    isError: isAttemptsError,
  } = useTaskAttemptsWithSessions(task?.id);

  const formatTimeAgo = (iso: string) => {
    const d = new Date(iso);
    const diffMs = Date.now() - d.getTime();
    const absSec = Math.round(Math.abs(diffMs) / 1000);

    const rtf =
      typeof Intl !== 'undefined' &&
      typeof Intl.RelativeTimeFormat === 'function'
        ? new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' })
        : null;

    const to = (value: number, unit: Intl.RelativeTimeFormatUnit) =>
      rtf
        ? rtf.format(-value, unit)
        : `${value} ${unit}${value !== 1 ? 's' : ''} ago`;

    if (absSec < 60) return to(Math.round(absSec), 'second');
    const mins = Math.round(absSec / 60);
    if (mins < 60) return to(mins, 'minute');
    const hours = Math.round(mins / 60);
    if (hours < 24) return to(hours, 'hour');
    const days = Math.round(hours / 24);
    if (days < 30) return to(days, 'day');
    const months = Math.round(days / 30);
    if (months < 12) return to(months, 'month');
    const years = Math.round(months / 12);
    return to(years, 'year');
  };

  const displayedAttempts = [...attempts].sort(
    (a, b) =>
      new Date(b.created_at).getTime() - new Date(a.created_at).getTime()
  );

  if (!task) {
    return (
      <div className="text-muted-foreground">
        {t('taskPanel.noTaskSelected')}
      </div>
    );
  }

  const descriptionContent = task.description || '';

  const attemptColumns: ColumnDef<WorkspaceWithSession>[] = [
    {
      id: 'executor',
      header: '',
      accessor: (attempt) => attempt.session?.executor || 'Base Agent',
      className: 'pr-4 text-muted-foreground',
    },
    {
      id: 'branch',
      header: '',
      accessor: (attempt) => attempt.branch || '—',
      className: 'pr-4',
    },
    {
      id: 'time',
      header: '',
      accessor: (attempt) => formatTimeAgo(attempt.created_at),
      className: 'pr-0 text-right text-muted-foreground whitespace-nowrap',
    },
  ];

  const attemptsData = isAttemptsError ? [] : displayedAttempts;
  const attemptsEmptyState = isAttemptsError ? (
    <span className="text-destructive">
      {t('taskPanel.errorLoadingAttempts')}
    </span>
  ) : (
    t('taskPanel.noAttempts')
  );

  return (
    <NewCardContent>
      <div className="h-full max-h-[calc(100vh-8rem)] overflow-y-auto p-6">
        <div className="mx-auto w-full max-w-[50rem] space-y-10">
          <div className="space-y-1">
            <h1 className="text-2xl font-semibold">{task.title || 'Task'}</h1>
            {descriptionContent && (
              <MarkdownEditor
                value={descriptionContent}
                disabled
                taskId={task.id}
              />
            )}
          </div>

          <div>
            <DataTable
              data={attemptsData}
              columns={attemptColumns}
              keyExtractor={(attempt) => attempt.id}
              onRowClick={(attempt) => {
                if (projectId && task.id) {
                  navigate(paths.attempt(projectId, task.id, attempt.id));
                }
              }}
              isLoading={isAttemptsLoading && !isAttemptsError}
              emptyState={attemptsEmptyState}
              headerContent={
                <div className="w-full flex items-center gap-2 text-left text-foreground">
                  <span className="flex-1 text-lg font-semibold tracking-wide">
                    {t('taskPanel.attemptsCount', {
                      count: displayedAttempts.length,
                    })}
                  </span>
                  <Button
                    variant="icon"
                    onClick={() =>
                      CreateAttemptDialog.show({
                        taskId: task.id,
                      })
                    }
                  >
                    <PlusIcon size={18} />
                  </Button>
                </div>
              }
            />
          </div>
        </div>
      </div>
    </NewCardContent>
  );
};

export default TaskPanel;
