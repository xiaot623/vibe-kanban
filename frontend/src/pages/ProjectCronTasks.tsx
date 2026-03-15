import { useCallback, useEffect, useMemo, useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import {
  BaseCodingAgent,
  type CronTask,
  type CronTaskConfig,
  type ExecutorProfileId,
} from 'shared/types';
import { AgentSelector } from '@/components/tasks/AgentSelector';
import { ConfigSelector } from '@/components/tasks/ConfigSelector';
import { useUserSystem } from '@/components/ConfigProvider';
import { useProject } from '@/contexts/ProjectContext';
import { cronTasksApi } from '@/lib/api';
import { Button } from '@/components/ui/button';
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { Switch } from '@/components/ui/switch';
import { Label } from '@/components/ui/label';
import { Loader } from '@/components/ui/loader';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { ArrowLeft, Plus, Save, Trash2 } from 'lucide-react';

const buildProfileId = (task: CronTask): ExecutorProfileId => ({
  executor: task.executor,
  variant: task.mode === 'DEFAULT' ? null : task.mode,
});

const normalizeMode = (profile: ExecutorProfileId | null): string =>
  profile?.variant ? profile.variant : 'DEFAULT';

const createTaskId = () => {
  if (typeof crypto !== 'undefined' && crypto.randomUUID) {
    return crypto.randomUUID();
  }
  return `${Date.now()}-${Math.random().toString(16).slice(2)}`;
};

export function ProjectCronTasks() {
  const { projectId } = useParams();
  const { project } = useProject();
  const { profiles: executors, config } = useUserSystem();
  const [cronConfig, setCronConfig] = useState<CronTaskConfig | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [isSaving, setIsSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);

  const defaultProfile = useMemo<ExecutorProfileId | null>(() => {
    if (config?.executor_profile) {
      return config.executor_profile;
    }

    const executorKeys = executors ? Object.keys(executors) : [];
    if (executorKeys.length === 0) return null;
    const fallbackExecutor = executorKeys.sort()[0] as BaseCodingAgent;
    return {
      executor: fallbackExecutor,
      variant: null,
    };
  }, [config?.executor_profile, executors]);

  const loadCronTasks = useCallback(async () => {
    if (!projectId) return;
    setIsLoading(true);
    setError(null);
    try {
      const config = await cronTasksApi.get(projectId);
      setCronConfig(config);
    } catch (err) {
      console.error('Failed to load cron tasks:', err);
      setError(
        err instanceof Error ? err.message : 'Failed to load cron tasks.'
      );
    } finally {
      setIsLoading(false);
    }
  }, [projectId]);

  useEffect(() => {
    void loadCronTasks();
  }, [loadCronTasks]);

  const updateTask = useCallback(
    (taskId: string, updater: (task: CronTask) => CronTask) => {
      setCronConfig((prev) => {
        if (!prev) return prev;
        return {
          ...prev,
          tasks: prev.tasks.map((task) =>
            task.id === taskId ? updater(task) : task
          ),
        };
      });
    },
    []
  );

  const handleAddTask = useCallback(() => {
    if (!defaultProfile) {
      setError('No executor profiles available. Configure an agent first.');
      return;
    }

    const newTask: CronTask = {
      id: createTaskId(),
      enabled: true,
      cron: '',
      title: '',
      description: null,
      executor: defaultProfile.executor,
      mode: normalizeMode(defaultProfile),
    };

    setCronConfig((prev) => {
      if (!prev) return prev;
      return {
        ...prev,
        tasks: [...prev.tasks, newTask],
      };
    });
  }, [defaultProfile]);

  const handleDeleteTask = useCallback((taskId: string) => {
    setCronConfig((prev) => {
      if (!prev) return prev;
      return {
        ...prev,
        tasks: prev.tasks.filter((task) => task.id !== taskId),
      };
    });
  }, []);

  const handleSave = useCallback(async () => {
    if (!projectId || !cronConfig) return;
    setIsSaving(true);
    setError(null);
    setSuccess(null);

    const payload: CronTaskConfig = {
      ...cronConfig,
      tasks: cronConfig.tasks.map((task) => ({
        ...task,
        description: task.description?.trim() ? task.description : null,
        mode: task.mode?.trim() || 'DEFAULT',
      })),
    };

    try {
      const saved = await cronTasksApi.update(projectId, payload);
      setCronConfig(saved);
      setSuccess('Cron tasks saved.');
    } catch (err) {
      console.error('Failed to save cron tasks:', err);
      setError(
        err instanceof Error ? err.message : 'Failed to save cron tasks.'
      );
    } finally {
      setIsSaving(false);
    }
  }, [cronConfig, projectId]);

  if (isLoading) {
    return (
      <div className="flex h-[60vh] items-center justify-center">
        <Loader message="Loading cron tasks..." size={32} />
      </div>
    );
  }

  if (!cronConfig || !projectId) {
    return (
      <div className="mx-auto max-w-3xl p-6">
        <Alert variant="destructive">
          <AlertTitle>Unable to load cron tasks</AlertTitle>
          <AlertDescription>
            {error || 'Cron tasks configuration is unavailable.'}
          </AlertDescription>
        </Alert>
      </div>
    );
  }

  return (
    <div className="mx-auto w-full max-w-4xl p-6 space-y-6">
      <div className="flex flex-col gap-4 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <h1 className="text-2xl font-semibold">Cron Tasks</h1>
          <p className="text-sm text-muted-foreground">
            Schedule tasks for {project?.name ?? 'this project'} using the system
            timezone.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Button variant="secondary" asChild>
            <Link to={`/projects/${projectId}/tasks`}>
              <ArrowLeft className="mr-2 h-4 w-4" />
              Back to Tasks
            </Link>
          </Button>
          <Button onClick={handleAddTask}>
            <Plus className="mr-2 h-4 w-4" />
            Add Task
          </Button>
        </div>
      </div>

      {error && (
        <Alert variant="destructive">
          <AlertTitle>Something went wrong</AlertTitle>
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {success && (
        <Alert>
          <AlertTitle>Saved</AlertTitle>
          <AlertDescription>{success}</AlertDescription>
        </Alert>
      )}

      <div className="space-y-4">
        {cronConfig.tasks.length === 0 ? (
          <Card className="border-dashed">
            <CardHeader>
              <CardTitle className="text-lg">No cron tasks yet</CardTitle>
              <CardDescription>
                Create a scheduled task to automatically start new work.
              </CardDescription>
            </CardHeader>
            <CardContent>
              <Button onClick={handleAddTask}>
                <Plus className="mr-2 h-4 w-4" />
                Add your first cron task
              </Button>
            </CardContent>
          </Card>
        ) : (
          cronConfig.tasks.map((task) => (
            <Card key={task.id} className="border-border/60">
              <CardHeader className="space-y-2">
                <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
                  <CardTitle className="text-lg">Cron Task</CardTitle>
                  <div className="flex items-center gap-2">
                    <div className="flex items-center gap-2">
                      <Switch
                        checked={task.enabled}
                        onCheckedChange={(value) =>
                          updateTask(task.id, (current) => ({
                            ...current,
                            enabled: value,
                          }))
                        }
                      />
                      <span className="text-sm text-muted-foreground">
                        {task.enabled ? 'Enabled' : 'Disabled'}
                      </span>
                    </div>
                    <Button
                      variant="ghost"
                      size="icon"
                      onClick={() => handleDeleteTask(task.id)}
                      aria-label="Delete cron task"
                    >
                      <Trash2 className="h-4 w-4" />
                    </Button>
                  </div>
                </div>
              </CardHeader>
              <CardContent className="space-y-4">
                <div className="grid gap-4 md:grid-cols-2">
                  <div className="space-y-2">
                    <Label htmlFor={`cron-title-${task.id}`}>Title</Label>
                    <Input
                      id={`cron-title-${task.id}`}
                      value={task.title}
                      onChange={(event) =>
                        updateTask(task.id, (current) => ({
                          ...current,
                          title: event.target.value,
                        }))
                      }
                      placeholder="Daily status summary"
                    />
                  </div>
                  <div className="space-y-2">
                    <Label htmlFor={`cron-expression-${task.id}`}>Schedule</Label>
                    <Input
                      id={`cron-expression-${task.id}`}
                      value={task.cron}
                      onChange={(event) =>
                        updateTask(task.id, (current) => ({
                          ...current,
                          cron: event.target.value,
                        }))
                      }
                      placeholder="0 9 * * *"
                    />
                    <p className="text-xs text-muted-foreground">
                      Use 5-7 field cron syntax. Runs in the system timezone.
                    </p>
                  </div>
                </div>

                <div className="space-y-2">
                  <Label htmlFor={`cron-description-${task.id}`}>
                    Description
                  </Label>
                  <Textarea
                    id={`cron-description-${task.id}`}
                    value={task.description ?? ''}
                    onChange={(event) =>
                      updateTask(task.id, (current) => ({
                        ...current,
                        description: event.target.value,
                      }))
                    }
                    placeholder="Optional details passed to the agent"
                  />
                </div>

                <div className="grid gap-4 md:grid-cols-2">
                  <AgentSelector
                    profiles={executors ?? null}
                    selectedExecutorProfile={buildProfileId(task)}
                    onChange={(profile) =>
                      updateTask(task.id, (current) => ({
                        ...current,
                        executor: profile.executor,
                        mode: normalizeMode(profile),
                      }))
                    }
                    showLabel
                  />
                  <ConfigSelector
                    profiles={executors ?? null}
                    selectedExecutorProfile={buildProfileId(task)}
                    onChange={(profile) =>
                      updateTask(task.id, (current) => ({
                        ...current,
                        executor: profile.executor,
                        mode: normalizeMode(profile),
                      }))
                    }
                    showLabel
                  />
                </div>
              </CardContent>
            </Card>
          ))
        )}
      </div>

      <div className="flex justify-end">
        <Button onClick={handleSave} disabled={isSaving}>
          <Save className="mr-2 h-4 w-4" />
          {isSaving ? 'Saving...' : 'Save Cron Tasks'}
        </Button>
      </div>
    </div>
  );
}
