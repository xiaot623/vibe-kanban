use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use chrono::Local;
use cron::Schedule;
use db::{
    DBService,
    models::{
        project::Project,
        project_repo::ProjectRepo,
        task::{CreateTask, Task, TaskStatus},
        workspace::{CreateWorkspace, Workspace, WorkspaceError},
        workspace_repo::{CreateWorkspaceRepo, WorkspaceRepo},
    },
};
use executors::{
    executors::BaseCodingAgent,
    profile::{ExecutorConfigs, ExecutorProfileId, canonical_variant_key},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{sync::RwLock, task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;
use uuid::Uuid;

use crate::services::{
    container::ContainerService,
    git::{GitBranch, GitService},
};

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct CronTaskConfig {
    pub project: CronProject,
    pub tasks: Vec<CronTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct CronProject {
    pub id: Uuid,
    pub name: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct CronTask {
    pub id: Uuid,
    pub enabled: bool,
    pub cron: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub executor: BaseCodingAgent,
    pub mode: String,
}

#[derive(Debug, Error)]
pub enum CronTaskError {
    #[error("Project has no repositories configured.")]
    ProjectHasNoRepositories,
    #[error("Cron task IDs must be unique.")]
    DuplicateTaskIds,
    #[error("Cron task {task_id} has an empty title.")]
    EmptyTitle { task_id: Uuid },
    #[error("Cron task {task_id} has an empty cron expression.")]
    EmptyCron { task_id: Uuid },
    #[error("Cron task {task_id} has invalid cron expression: {error}")]
    InvalidCron { task_id: Uuid, error: String },
    #[error("Cron task {task_id} has invalid executor/mode: {reason}")]
    InvalidExecutorMode { task_id: Uuid, reason: String },
    #[error("Cron task {task_id} has no selectable git branches.")]
    NoBranches { task_id: Uuid },
    #[error("Failed to resolve branches for repository '{repo}': {error}")]
    BranchLookup { repo: String, error: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct CronScheduler<C: ContainerService + Clone + Send + Sync + 'static> {
    db: DBService,
    git: GitService,
    container: C,
    runners: Arc<RwLock<HashMap<Uuid, CronRunnerHandle>>>,
}

struct CronRunnerHandle {
    project_id: Uuid,
    cancel: CancellationToken,
    _join: JoinHandle<()>,
}

impl<C: ContainerService + Clone + Send + Sync + 'static> CronScheduler<C> {
    pub fn new(db: DBService, git: GitService, container: C) -> Self {
        Self {
            db,
            git,
            container,
            runners: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn sync_all_projects(&self) -> Result<(), CronTaskError> {
        let projects = Project::find_all(&self.db.pool).await?;
        for project in projects {
            match load_project_cron_config(&self.db.pool, &project).await {
                Ok(config) => {
                    if let Err(e) = self.replace_project_tasks(&project, config.tasks).await {
                        tracing::warn!(
                            "Failed to start cron tasks for project {}: {}",
                            project.id,
                            e
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load cron config for project {}: {}",
                        project.id,
                        e
                    );
                }
            }
        }

        Ok(())
    }

    pub async fn replace_project_tasks(
        &self,
        project: &Project,
        tasks: Vec<CronTask>,
    ) -> Result<(), CronTaskError> {
        self.stop_project_tasks(project.id).await;

        for task in tasks.into_iter().filter(|task| task.enabled) {
            if let Err(e) = self.spawn_task_runner(project.clone(), task).await {
                tracing::warn!(
                    "Failed to start cron runner for project {}: {}",
                    project.id,
                    e
                );
            }
        }

        Ok(())
    }

    async fn stop_project_tasks(&self, project_id: Uuid) {
        let mut runners = self.runners.write().await;
        let existing_ids: Vec<Uuid> = runners
            .iter()
            .filter_map(|(id, handle)| {
                if handle.project_id == project_id {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();

        for id in existing_ids {
            if let Some(handle) = runners.remove(&id) {
                handle.cancel.cancel();
            }
        }
    }

    async fn spawn_task_runner(
        &self,
        project: Project,
        mut task: CronTask,
    ) -> Result<(), CronTaskError> {
        let profiles = ExecutorConfigs::get_cached();
        let profile_id = resolve_executor_profile(&task, &profiles)?;
        task.mode = canonicalize_mode(&task.mode);
        let schedule = parse_schedule(&task, &task.cron)?;

        let db = self.db.clone();
        let git = self.git.clone();
        let container = self.container.clone();
        let cancel = CancellationToken::new();
        let cancel_signal = cancel.clone();

        let project_id = project.id;
        let task_id = task.id;
        let project_for_run = project.clone();
        let task_for_run = task.clone();

        let handle = tokio::spawn(async move {
            loop {
                let now = Local::now();
                let next = match schedule.after(&now).next() {
                    Some(next) => next,
                    None => {
                        tracing::warn!(
                            "Cron schedule exhausted for task {} (project {})",
                            task.id,
                            project.id
                        );
                        break;
                    }
                };

                let duration = match (next - now).to_std() {
                    Ok(duration) => duration,
                    Err(_) => Duration::from_secs(0),
                };

                tokio::select! {
                    _ = cancel_signal.cancelled() => {
                        break;
                    }
                    _ = tokio::time::sleep_until(Instant::now() + duration) => {
                        if let Err(e) = trigger_cron_task(&db, &git, &container, &project_for_run, &task_for_run, profile_id.clone()).await {
                            tracing::warn!(
                                "Failed to trigger cron task {} for project {}: {}",
                                task_for_run.id,
                                project_for_run.id,
                                e
                            );
                        }
                    }
                }
            }
        });

        let mut runners = self.runners.write().await;
        runners.insert(
            task_id,
            CronRunnerHandle {
                project_id,
                cancel,
                _join: handle,
            },
        );

        Ok(())
    }
}

fn canonicalize_mode(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        "DEFAULT".to_string()
    } else {
        canonical_variant_key(trimmed)
    }
}

fn normalize_cron_expression(raw: &str) -> Result<String, CronTaskError> {
    let trimmed = raw.trim();
    let parts = trimmed.split_whitespace().count();
    match parts {
        5 => Ok(format!("0 {trimmed}")),
        6 | 7 => Ok(trimmed.to_string()),
        _ => Err(CronTaskError::InvalidCron {
            task_id: Uuid::nil(),
            error: format!("expected 5-7 fields, got {parts}"),
        }),
    }
}

fn parse_schedule(task: &CronTask, raw: &str) -> Result<Schedule, CronTaskError> {
    let normalized = normalize_cron_expression(raw).map_err(|err| match err {
        CronTaskError::InvalidCron { error, .. } => CronTaskError::InvalidCron {
            task_id: task.id,
            error,
        },
        other => other,
    })?;

    Schedule::from_str(&normalized).map_err(|e| CronTaskError::InvalidCron {
        task_id: task.id,
        error: e.to_string(),
    })
}

fn resolve_executor_profile(
    task: &CronTask,
    profiles: &ExecutorConfigs,
) -> Result<ExecutorProfileId, CronTaskError> {
    let executor_config = profiles.executors.get(&task.executor).ok_or_else(|| {
        CronTaskError::InvalidExecutorMode {
            task_id: task.id,
            reason: format!("executor '{}' is not available", task.executor),
        }
    })?;

    let mode = canonicalize_mode(&task.mode);

    if mode == "DEFAULT" {
        if executor_config.get_default().is_none() {
            return Err(CronTaskError::InvalidExecutorMode {
                task_id: task.id,
                reason: format!("executor '{}' has no DEFAULT mode", task.executor),
            });
        }
        Ok(ExecutorProfileId::new(task.executor.clone()))
    } else if executor_config.get_variant(&mode).is_some() {
        Ok(ExecutorProfileId::with_variant(task.executor.clone(), mode))
    } else {
        Err(CronTaskError::InvalidExecutorMode {
            task_id: task.id,
            reason: format!("mode '{mode}' is not configured for '{}'", task.executor),
        })
    }
}

pub async fn load_project_cron_config(
    pool: &sqlx::SqlitePool,
    project: &Project,
) -> Result<CronTaskConfig, CronTaskError> {
    let path = cron_config_path_for_project(pool, project.id).await?;
    let default_config = CronTaskConfig {
        project: CronProject {
            id: project.id,
            name: project.name.clone(),
            updated_at: Local::now().to_rfc3339(),
        },
        tasks: Vec::new(),
    };

    if !path.exists() {
        write_cron_store(&path, &HashMap::new())?;
        return Ok(default_config);
    }

    let store = read_cron_store(&path)?;
    let tasks = store.get(&project.id).cloned().unwrap_or_default();

    Ok(CronTaskConfig {
        project: CronProject {
            id: project.id,
            name: project.name.clone(),
            updated_at: Local::now().to_rfc3339(),
        },
        tasks,
    })
}

pub async fn save_project_cron_config(
    pool: &sqlx::SqlitePool,
    project: &Project,
    mut config: CronTaskConfig,
) -> Result<CronTaskConfig, CronTaskError> {
    config.project = CronProject {
        id: project.id,
        name: project.name.clone(),
        updated_at: Local::now().to_rfc3339(),
    };

    normalize_cron_config(&mut config)?;

    let path = cron_config_path_for_project(pool, project.id).await?;
    let mut store = if path.exists() {
        read_cron_store(&path)?
    } else {
        HashMap::new()
    };
    store.insert(project.id, config.tasks.clone());
    write_cron_store(&path, &store)?;

    Ok(config)
}

fn normalize_cron_config(config: &mut CronTaskConfig) -> Result<(), CronTaskError> {
    let mut seen_ids = HashSet::new();
    let profiles = ExecutorConfigs::get_cached();

    for task in &mut config.tasks {
        if !seen_ids.insert(task.id) {
            return Err(CronTaskError::DuplicateTaskIds);
        }

        task.title = task.title.trim().to_string();
        if task.title.is_empty() {
            return Err(CronTaskError::EmptyTitle { task_id: task.id });
        }

        task.cron = task.cron.trim().to_string();
        if task.cron.is_empty() {
            return Err(CronTaskError::EmptyCron { task_id: task.id });
        }

        if let Some(desc) = task.description.as_mut() {
            let trimmed = desc.trim();
            if trimmed.is_empty() {
                task.description = None;
            } else {
                *desc = trimmed.to_string();
            }
        }

        task.mode = canonicalize_mode(&task.mode);

        parse_schedule(task, &task.cron)?;
        resolve_executor_profile(task, &profiles)?;
    }

    Ok(())
}

fn write_cron_store(
    path: &PathBuf,
    store: &HashMap<Uuid, Vec<CronTask>>,
) -> Result<(), CronTaskError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let raw = serde_json::to_string_pretty(store)?;
    std::fs::write(path, raw)?;
    Ok(())
}

fn read_cron_store(path: &PathBuf) -> Result<HashMap<Uuid, Vec<CronTask>>, CronTaskError> {
    let raw = std::fs::read_to_string(path)?;

    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }

    if let Ok(store) = serde_json::from_str::<HashMap<Uuid, Vec<CronTask>>>(&raw) {
        return Ok(store);
    }

    // Backward compatibility: old format stored a single project config.
    if let Ok(config) = serde_json::from_str::<CronTaskConfig>(&raw) {
        return Ok(HashMap::from([(config.project.id, config.tasks)]));
    }

    // Backward compatibility: transitional format stored project -> full config.
    if let Ok(config_store) = serde_json::from_str::<HashMap<Uuid, CronTaskConfig>>(&raw) {
        let tasks_store = config_store
            .into_iter()
            .map(|(project_id, config)| (project_id, config.tasks))
            .collect();
        return Ok(tasks_store);
    }

    Err(serde_json::from_str::<HashMap<Uuid, Vec<CronTask>>>(&raw)
        .unwrap_err()
        .into())
}

async fn cron_config_path_for_project(
    _pool: &sqlx::SqlitePool,
    _project_id: Uuid,
) -> Result<PathBuf, CronTaskError> {
    Ok(utils::assets::asset_dir().join("cron.json"))
}

async fn trigger_cron_task<C: ContainerService + Clone + Send + Sync + 'static>(
    db: &DBService,
    git: &GitService,
    container: &C,
    project: &Project,
    task: &CronTask,
    executor_profile_id: ExecutorProfileId,
) -> Result<(), CronTaskError> {
    let repos = ProjectRepo::find_repos_for_project(&db.pool, project.id).await?;
    if repos.is_empty() {
        return Err(CronTaskError::ProjectHasNoRepositories);
    }

    let task_id = Uuid::new_v4();
    let create_task = CreateTask {
        project_id: project.id,
        title: task.title.clone(),
        description: task.description.clone(),
        status: Some(TaskStatus::Todo),
        parent_workspace_id: None,
        source_cron_task_id: Some(task.id),
        image_ids: None,
    };

    let task = Task::create(&db.pool, &create_task, task_id).await?;

    let attempt_id = Uuid::new_v4();
    let git_branch_name = container
        .git_branch_from_workspace(&attempt_id, &task.title)
        .await;

    let agent_working_dir = if repos.len() == 1 {
        Some(repos[0].name.clone())
    } else {
        None
    };

    let workspace = Workspace::create(
        &db.pool,
        &CreateWorkspace {
            branch: git_branch_name,
            agent_working_dir,
        },
        attempt_id,
        task.id,
    )
    .await?;

    let mut workspace_repos = Vec::with_capacity(repos.len());
    for repo in repos {
        let branches =
            git.get_all_branches(&repo.path)
                .map_err(|e| CronTaskError::BranchLookup {
                    repo: repo.name.clone(),
                    error: e.to_string(),
                })?;
        let target_branch = select_target_branch(&branches)
            .ok_or(CronTaskError::NoBranches { task_id: task.id })?;
        workspace_repos.push(CreateWorkspaceRepo {
            repo_id: repo.id,
            target_branch,
        });
    }

    WorkspaceRepo::create_many(&db.pool, workspace.id, &workspace_repos).await?;

    if let Err(e) = container
        .start_workspace(&workspace, executor_profile_id)
        .await
    {
        tracing::warn!("Failed to start cron task {} attempt: {}", task.id, e);
    }

    Ok(())
}

fn select_target_branch(branches: &[GitBranch]) -> Option<String> {
    if let Some(current) = branches.iter().find(|branch| branch.is_current) {
        return Some(current.name.clone());
    }

    branches.first().map(|branch| branch.name.clone())
}
