import { Link, useLocation, useNavigate } from 'react-router-dom';
import { useCallback, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  FolderOpen,
  Home,
  Settings,
  BookOpen,
  MessageCircleQuestion,
  MessageCircle,
  Menu,
  Plus,
} from 'lucide-react';
import { Logo } from '@/components/Logo';
import { SearchBar } from '@/components/SearchBar';
import { useSearch } from '@/contexts/SearchContext';
import { openTaskForm } from '@/lib/openTaskForm';
import { useProject } from '@/contexts/ProjectContext';
import { useOpenProjectInEditor } from '@/hooks/useOpenProjectInEditor';
import { OpenInIdeButton } from '@/components/ide/OpenInIdeButton';
import { useProjectRepos } from '@/hooks';
import { RepoPickerDialog } from '@/components/dialogs/shared/RepoPickerDialog';
import { projectsApi, tasksApi } from '@/lib/api';
import { useUserSystem } from '@/components/ConfigProvider';

const INTERNAL_NAV = [{ label: 'Projects', icon: FolderOpen, to: '/projects' }];

const EXTERNAL_LINKS = [
  {
    label: 'Docs',
    icon: BookOpen,
    href: 'https://vibekanban.com/docs',
  },
  {
    label: 'Support',
    icon: MessageCircleQuestion,
    href: 'https://github.com/BloopAI/vibe-kanban/issues',
  },
  {
    label: 'Discord',
    icon: MessageCircle,
    href: 'https://discord.gg/AC4nwVtJM3',
  },
];

function NavDivider() {
  return (
    <div
      className="mx-2 h-6 w-px bg-border/60"
      role="separator"
      aria-orientation="vertical"
    />
  );
}

export function Navbar() {
  const location = useLocation();
  const navigate = useNavigate();
  const { projectId, project } = useProject();
  const { query, setQuery, active, clear, registerInputRef } = useSearch();
  const handleOpenInEditor = useOpenProjectInEditor(project || null);
  const { config, updateAndSaveConfig } = useUserSystem();
  const [isEnteringDailyMode, setIsEnteringDailyMode] = useState(false);
  const enterDailyModeInFlightRef = useRef(false);

  const { data: repos } = useProjectRepos(projectId);
  const isSingleRepoProject = repos?.length === 1;

  const setSearchBarRef = useCallback(
    (node: HTMLInputElement | null) => {
      registerInputRef(node);
    },
    [registerInputRef]
  );

  const handleCreateTask = () => {
    if (projectId) {
      openTaskForm({ mode: 'create', projectId });
    }
  };

  const handleOpenInIDE = () => {
    handleOpenInEditor();
  };

  const clearDailyModeProject = useCallback(async () => {
    if (!config?.daily_mode?.project_id) {
      return true;
    }

    return updateAndSaveConfig({
      daily_mode: {
        project_id: null,
      },
    });
  }, [config?.daily_mode?.project_id, updateAndSaveConfig]);

  const runDailyModeOnboarding = useCallback(async () => {
    const repo = await RepoPickerDialog.show({
      title: 'Set Up Daily Project',
      description: 'Select or create the repository for Daily Mode',
    });

    if (!repo) {
      return;
    }

    const projectName = repo.display_name || repo.name;
    const dailyProject = await projectsApi.create({
      name: projectName,
      repositories: [
        {
          display_name: projectName,
          git_repo_path: repo.path,
        },
      ],
    });

    const saved = await updateAndSaveConfig({
      daily_mode: {
        project_id: dailyProject.id,
      },
    });

    if (!saved) {
      return;
    }

    navigate(`/projects/${dailyProject.id}/tasks`);
  }, [navigate, updateAndSaveConfig]);

  const cleanupStaleDailyInReviewTasks = useCallback(
    async (dailyProjectId: string) => {
      try {
        const tasks = await tasksApi.getByProject(dailyProjectId);
        const now = new Date();
        const minAgeMs = 12 * 60 * 60 * 1000;

        const staleInReviewTasks = tasks.filter((task) => {
          if (task.status !== 'inreview') {
            return false;
          }

          const createdAt = new Date(task.created_at);
          if (Number.isNaN(createdAt.getTime())) {
            return false;
          }

          const ageMs = now.getTime() - createdAt.getTime();
          if (ageMs < minAgeMs) {
            return false;
          }

          const createdToday =
            createdAt.getFullYear() === now.getFullYear() &&
            createdAt.getMonth() === now.getMonth() &&
            createdAt.getDate() === now.getDate();

          return !createdToday;
        });

        if (staleInReviewTasks.length === 0) {
          return;
        }

        const cleanupResults = await Promise.allSettled(
          staleInReviewTasks.map((task) =>
            tasksApi.delete(task.id, { expectedStatus: 'inreview' })
          )
        );

        const failedCount = cleanupResults.filter(
          (result) => result.status === 'rejected'
        ).length;
        if (failedCount > 0) {
          console.warn(
            `Daily cleanup skipped ${failedCount}/${staleInReviewTasks.length} tasks due to delete failures.`
          );
        }
      } catch (error) {
        console.warn('Failed to cleanup stale Daily InReview tasks.', error);
      }
    },
    []
  );

  const handleEnterDailyMode = useCallback(async () => {
    if (!config || enterDailyModeInFlightRef.current) {
      return;
    }

    enterDailyModeInFlightRef.current = true;
    setIsEnteringDailyMode(true);

    try {
      const dailyProjectId = config.daily_mode?.project_id || null;
      if (dailyProjectId) {
        try {
          await projectsApi.getById(dailyProjectId);
          await cleanupStaleDailyInReviewTasks(dailyProjectId);
          navigate(`/projects/${dailyProjectId}/tasks`);
          return;
        } catch (error) {
          console.warn(
            'Configured daily project is unavailable, resetting Daily Mode.',
            error
          );
        }
      }

      const cleared = await clearDailyModeProject();
      if (!cleared) {
        return;
      }

      await runDailyModeOnboarding();
    } catch (error) {
      console.error('Failed to enter Daily Mode:', error);
    } finally {
      enterDailyModeInFlightRef.current = false;
      setIsEnteringDailyMode(false);
    }
  }, [
    cleanupStaleDailyInReviewTasks,
    clearDailyModeProject,
    config,
    navigate,
    runDailyModeOnboarding,
  ]);

  return (
    <div className="border-b bg-background">
      <div className="w-full px-3">
        <div className="flex items-center h-12 py-2">
          <div className="flex-1 flex items-center">
            <button
              type="button"
              onClick={() => {
                void handleEnterDailyMode();
              }}
              disabled={isEnteringDailyMode || !config}
              aria-label="Enter Daily Mode"
              aria-busy={isEnteringDailyMode}
              className="disabled:opacity-70"
            >
              <Logo />
            </button>
          </div>

          <div className="hidden sm:flex items-center gap-2">
            <SearchBar
              ref={setSearchBarRef}
              className="shrink-0"
              value={query}
              onChange={setQuery}
              disabled={!active}
              onClear={clear}
              project={project || null}
            />
          </div>

          <div className="flex flex-1 items-center justify-end gap-1">
            {projectId ? (
              <>
                <div className="flex items-center gap-1">
                  {isSingleRepoProject && (
                    <OpenInIdeButton
                      onClick={handleOpenInIDE}
                      className="h-9 w-9"
                    />
                  )}
                  <Button
                    variant="ghost"
                    size="icon"
                    className="h-9 w-9"
                    onClick={handleCreateTask}
                    aria-label="Create new task"
                  >
                    <Plus className="h-4 w-4" />
                  </Button>
                </div>
                <NavDivider />
              </>
            ) : null}

            {/* <Button variant="ghost" size="sm" className="h-9 gap-1.5" asChild>
              <Link to="/workspaces">
                <Sparkles className="h-4 w-4" />
                {t('common:navbar.tryNewUI')}
              </Link>
            </Button>
            <NavDivider /> */}

            <div className="flex items-center gap-1">
              <Button
                variant="ghost"
                size="icon"
                className="h-9 w-9"
                asChild
                aria-label="Home"
              >
                <Link to="/projects">
                  <Home className="h-4 w-4" />
                </Link>
              </Button>

              <Button
                variant="ghost"
                size="icon"
                className="h-9 w-9"
                asChild
                aria-label="Settings"
              >
                <Link
                  to={
                    projectId
                      ? `/settings/projects?projectId=${projectId}`
                      : '/settings'
                  }
                >
                  <Settings className="h-4 w-4" />
                </Link>
              </Button>

              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="h-9 w-9"
                    aria-label="Main navigation"
                  >
                    <Menu className="h-4 w-4" />
                  </Button>
                </DropdownMenuTrigger>

                <DropdownMenuContent align="end">
                  {INTERNAL_NAV.map((item) => {
                    const active = location.pathname.startsWith(item.to);
                    const Icon = item.icon;
                    return (
                      <DropdownMenuItem
                        key={item.to}
                        asChild
                        className={active ? 'bg-accent' : ''}
                      >
                        <Link to={item.to}>
                          <Icon className="mr-2 h-4 w-4" />
                          {item.label}
                        </Link>
                      </DropdownMenuItem>
                    );
                  })}

                  <DropdownMenuSeparator />

                  {EXTERNAL_LINKS.map((item) => {
                    const Icon = item.icon;
                    return (
                      <DropdownMenuItem key={item.href} asChild>
                        <a
                          href={item.href}
                          target="_blank"
                          rel="noopener noreferrer"
                        >
                          <Icon className="mr-2 h-4 w-4" />
                          {item.label}
                        </a>
                      </DropdownMenuItem>
                    );
                  })}
                </DropdownMenuContent>
              </DropdownMenu>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
