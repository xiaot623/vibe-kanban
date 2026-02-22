use std::{path::PathBuf, str::FromStr};

use db::models::{
    execution_process::ExecutionProcess,
    project::Project,
    project_repo::ProjectRepo,
    session::Session,
    short_id_mapping::ShortIdMapping,
    task::{CreateTask, Task, TaskStatus, TaskWithAttemptStatus, UpdateTask},
    workspace::Workspace,
    workspace_repo::WorkspaceRepo,
};
use executors::{
    executors::BaseCodingAgent,
    profile::{ExecutorProfileId, canonical_variant_key},
};
use serde::Serialize;
use utils::{
    approvals::{ApprovalResponse, ApprovalStatus},
    response::ApiResponse,
};
use uuid::Uuid;

use super::TelegramBotService;
use crate::services::{git::GitBranch, telegram::EXIT_PLAN_MODE_NAME};

// ─── Shared command logic (used by both modes) ───────────────────────

impl TelegramBotService {
    pub(super) async fn list_projects(&self) -> String {
        match Project::find_all(&self.db.pool).await {
            Ok(projects) if projects.is_empty() => "No projects found.".to_string(),
            Ok(projects) => {
                let mut message = format!("Projects ({}):", projects.len());
                for project in projects {
                    message.push_str(&format!("\n- {} (project{})", project.name, project.id));
                }
                message
            }
            Err(e) => format!("Failed to load projects: {e}"),
        }
    }

    pub(super) async fn list_tasks_for_project(&self, args: String) -> String {
        let args = args.trim();
        let parts: Vec<&str> = if args.is_empty() {
            Vec::new()
        } else {
            args.split_whitespace().collect()
        };

        if parts.is_empty() {
            return "Usage: /list <project> [status]".to_string();
        }

        let project_name = parts[0];
        let status_filter = if parts.len() > 1 {
            match parse_task_status(parts[1]) {
                Some(status) => Some(status),
                None => {
                    return format!(
                        "Invalid status: {} (expected todo/inprogress/inreview/done/cancelled)",
                        parts[1]
                    );
                }
            }
        } else {
            None
        };

        let (project, warning) = match Project::find_by_name(&self.db.pool, project_name).await {
            Ok(projects) if projects.is_empty() => {
                match Uuid::parse_str(project_name.trim_start_matches("project")) {
                    Ok(id) => match Project::find_by_id(&self.db.pool, id).await {
                        Ok(Some(p)) => (p, None),
                        Ok(None) => return format!("Project not found: {project_name}"),
                        Err(e) => return format!("Failed to load project: {e}"),
                    },
                    Err(_) => return format!("Project not found: {project_name}"),
                }
            }
            Ok(projects) if projects.len() > 1 => {
                let mut msg = format!(
                    "Warning: Multiple projects match '{project_name}', using first match:"
                );
                for p in &projects {
                    msg.push_str(&format!("\n- {} (project{})", p.name, p.id));
                }
                (projects.into_iter().next().unwrap(), Some(msg))
            }
            Ok(mut projects) => (projects.remove(0), None),
            Err(e) => return format!("Failed to load project: {e}"),
        };

        let tasks =
            match Task::find_by_project_id_with_attempt_status(&self.db.pool, project.id).await {
                Ok(tasks) => tasks,
                Err(e) => return format!("Failed to load tasks: {e}"),
            };

        let tasks: Vec<_> = tasks
            .into_iter()
            .filter(|task| {
                if let Some(ref s) = status_filter {
                    &task.status == s
                } else {
                    matches!(task.status, TaskStatus::InReview | TaskStatus::Done)
                }
            })
            .collect();

        let mut result = format_task_list(&self.db.pool, &tasks, Some(&project.name)).await;
        if let Some(warn) = warning {
            result = format!("{warn}\n\n{result}");
        }
        result
    }

    pub(super) async fn describe_task(&self, raw_id: &str) -> String {
        let Some(task_id) = self.resolve_task_id(raw_id).await else {
            return "Invalid task id. Expected short code, task<uuid>, or uuid.".to_string();
        };

        let task = match Task::find_by_id(&self.db.pool, task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => return format!("Task not found: task{task_id}"),
            Err(e) => return format!("Failed to load task: {e}"),
        };

        if !matches!(task.status, TaskStatus::InReview | TaskStatus::Done) {
            return format!(
                "Task is not available for review (status: {:?}). Only InReview and Done tasks can be viewed.",
                task.status
            );
        }

        let project_name = match Project::find_by_id(&self.db.pool, task.project_id).await {
            Ok(Some(project)) => project.name,
            _ => "Unknown project".to_string(),
        };

        let description = task
            .description
            .clone()
            .unwrap_or_else(|| "No description.".to_string());

        let short_id = ShortIdMapping::get_or_create(&self.db.pool, task.id)
            .await
            .unwrap_or_else(|_| "????".to_string());

        let mut message = format!(
            "[{}] task{}\nProject: {}\nStatus: {:?}\nTitle: {}\nDescription: {}",
            short_id,
            task.id,
            project_name,
            task.status,
            task.title,
            truncate_text(&description, 500)
        );

        if let Some(attempts_info) = self.fetch_attempts_info(task_id).await {
            if !attempts_info.is_empty() {
                message.push_str(&format!("\n\nAttempts ({}):", attempts_info.len()));
                for attempt in attempts_info {
                    message.push_str(&format!("\n  • {}", attempt));
                }
            }
        }

        message
    }

    pub(super) async fn fetch_attempts_info(&self, task_id: Uuid) -> Option<Vec<String>> {
        let workspaces = Workspace::fetch_all(&self.db.pool, Some(task_id))
            .await
            .ok()?;

        if workspaces.is_empty() {
            return Some(vec![]);
        }

        let mut attempts_info = Vec::new();
        for workspace in workspaces {
            let mut status_parts = Vec::new();

            if let Ok(Some(session)) =
                Session::find_latest_by_workspace_id(&self.db.pool, workspace.id).await
            {
                if let Some(executor) = session.executor {
                    status_parts.push(executor);
                }
            }

            if let Ok(Some(ws_with_status)) =
                Workspace::find_by_id_with_status(&self.db.pool, workspace.id).await
            {
                if ws_with_status.is_running {
                    status_parts.push("running".to_string());
                } else if ws_with_status.is_errored {
                    status_parts.push("errored".to_string());
                } else {
                    status_parts.push("idle".to_string());
                }
            }

            if workspace.archived {
                status_parts.push("archived".to_string());
            }

            if let Some((added, removed)) = self.compute_workspace_diff_stats(&workspace).await {
                status_parts.push(format!("+{} / -{}", added, removed));
            }

            attempts_info.push(format!("[{}]", status_parts.join(", ")));
        }

        Some(attempts_info)
    }

    async fn compute_workspace_diff_stats(&self, workspace: &Workspace) -> Option<(usize, usize)> {
        let container_ref = workspace.container_ref.as_ref()?;

        let workspace_repos =
            WorkspaceRepo::find_repos_with_target_branch_for_workspace(&self.db.pool, workspace.id)
                .await
                .ok()?;

        let mut total_added = 0usize;
        let mut total_removed = 0usize;

        for repo_with_branch in workspace_repos {
            let worktree_path = PathBuf::from(container_ref).join(&repo_with_branch.repo.name);
            let repo_path = repo_with_branch.repo.path.clone();
            let workspace_branch = workspace.branch.clone();
            let target_branch = repo_with_branch.target_branch.clone();

            let base_commit = self
                .git
                .get_base_commit(&repo_path, &workspace_branch, &target_branch)
                .ok()?;

            let diffs = self
                .git
                .get_diffs(
                    crate::services::git::DiffTarget::Worktree {
                        worktree_path: &worktree_path,
                        base_commit: &base_commit,
                    },
                    None,
                )
                .ok()?;

            for diff in diffs {
                total_added += diff.additions.unwrap_or(0);
                total_removed += diff.deletions.unwrap_or(0);
            }
        }

        Some((total_added, total_removed))
    }

    pub(super) async fn create_task_from_command(&self, payload: &str) -> Option<String> {
        let lines: Vec<&str> = payload.lines().collect();
        if lines.len() < 2 {
            return Some("Usage: /add [project_name]\\n[title]\\n[description]".to_string());
        }

        let project_name = lines[0].trim();
        let title = lines[1].trim();

        if project_name.is_empty() || title.is_empty() {
            return Some("Project name and title are required.".to_string());
        }

        let description = if lines.len() > 2 {
            let rest = lines[2..].join("\n");
            let trimmed = rest.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        } else {
            None
        };

        let (project, warning) = match Project::find_by_name(&self.db.pool, project_name).await {
            Ok(projects) if projects.is_empty() => {
                match Uuid::parse_str(project_name.trim_start_matches("project")) {
                    Ok(id) => match Project::find_by_id(&self.db.pool, id).await {
                        Ok(Some(p)) => (p, None),
                        Ok(None) => {
                            return Some(format!("Project not found: {project_name}"));
                        }
                        Err(e) => {
                            return Some(format!("Failed to load project: {e}"));
                        }
                    },
                    Err(_) => {
                        return Some(format!("Project not found: {project_name}"));
                    }
                }
            }
            Ok(projects) if projects.len() > 1 => {
                let mut msg = format!(
                    "Warning: Multiple projects match '{project_name}', using first match:"
                );
                for p in &projects {
                    msg.push_str(&format!("\n- {} (project{})", p.name, p.id));
                }
                (projects.into_iter().next().unwrap(), Some(msg))
            }
            Ok(mut projects) => (projects.remove(0), None),
            Err(e) => return Some(format!("Failed to load project: {e}")),
        };

        let task_id = Uuid::new_v4();
        match Task::create(
            &self.db.pool,
            &CreateTask::from_title_description(project.id, title.to_string(), description),
            task_id,
        )
        .await
        {
            Ok(_task) => {
                if let Some(warn) = warning {
                    tracing::warn!("{warn}");
                }
                return None;
            }
            Err(e) => {
                return Some(format!("Failed to create task: {e}"));
            }
        }
    }

    /// Shared run logic used by both button and legacy command paths.
    pub(super) async fn run_task_impl(
        &self,
        task_id: Uuid,
        executor_raw: &str,
        mode_raw: Option<&str>,
        branch_override: Option<String>,
    ) -> Result<(), String> {
        let executor = parse_executor(executor_raw)?;

        let config = self.config.read().await.telegram.clone();
        let use_default_mode = executor_raw.eq_ignore_ascii_case(&config.default_executor);
        let mode_raw = mode_raw.or_else(|| {
            if !use_default_mode {
                return None;
            }
            let trimmed = config.default_mode.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });

        let variant = mode_raw.map(|raw| canonical_variant_key(raw));
        let executor_profile_id = if let Some(variant) = variant {
            if variant == "DEFAULT" {
                ExecutorProfileId::new(executor)
            } else {
                ExecutorProfileId::with_variant(executor, variant)
            }
        } else {
            ExecutorProfileId::new(executor)
        };

        let task = match Task::find_by_id(&self.db.pool, task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => return Err(format!("Task not found: task{task_id}")),
            Err(e) => return Err(format!("Failed to load task: {e}")),
        };

        let repos = match ProjectRepo::find_repos_for_project(&self.db.pool, task.project_id).await
        {
            Ok(repos) => repos,
            Err(e) => return Err(format!("Failed to load project repos: {e}")),
        };

        if repos.is_empty() {
            return Err("Project has no repositories configured.".to_string());
        }

        let base_branch = if branch_override.is_none() {
            if let Some(workspace_id) = task.parent_workspace_id {
                match Workspace::find_by_id(&self.db.pool, workspace_id).await {
                    Ok(Some(workspace)) => Some(workspace.branch),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };

        let mut repo_inputs = Vec::with_capacity(repos.len());
        for repo in repos {
            let target_branch = match &branch_override {
                Some(branch) => branch.clone(),
                None => match self.git.get_all_branches(&repo.path) {
                    Ok(branches) => match select_target_branch(&branches, base_branch.as_deref()) {
                        Some(branch) => branch,
                        None => {
                            return Err(format!(
                                "No branches found for {}. Provide a branch with /run.",
                                repo.display_name
                            ));
                        }
                    },
                    Err(e) => {
                        return Err(format!(
                            "Failed to load branches for {}: {e}",
                            repo.display_name
                        ));
                    }
                },
            };

            repo_inputs.push(WorkspaceRepoInput {
                repo_id: repo.id,
                target_branch,
            });
        }

        let base_url = api_base_url()
            .await
            .map_err(|e| format!("Failed to locate API server: {e}"))?;

        let request = CreateTaskAttemptBody {
            task_id,
            executor_profile_id,
            repos: repo_inputs,
        };

        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base_url}/task-attempts"))
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("Failed to start task: {e}"))?;

        let api_response: ApiResponse<Workspace> = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse task attempt response: {e}"))?;

        let error_message = api_response.message().map(String::from);
        match api_response.into_data() {
            Some(_) => Ok(()),
            None => {
                Err(error_message.unwrap_or_else(|| "Failed to start task attempt.".to_string()))
            }
        }
    }

    pub(super) async fn run_task_from_command(&self, payload: &str) -> Option<String> {
        let parts: Vec<&str> = payload.split_whitespace().collect();
        if parts.is_empty() {
            return Some("Usage: /run task<uuid> [executor] [mode] [branch]".to_string());
        }

        let Some(task_id) = self.resolve_task_id(parts[0]).await else {
            return Some("Invalid task id. Expected short code, task<uuid>, or uuid.".to_string());
        };

        let config = self.config.read().await.telegram.clone();
        let executor_raw = parts.get(1).copied().unwrap_or(&config.default_executor);
        let mode_raw = parts.get(2).copied();
        let branch_override = parts.get(3).map(|v| v.to_string());

        match self
            .run_task_impl(task_id, executor_raw, mode_raw, branch_override)
            .await
        {
            Ok(()) => None,
            Err(msg) => Some(msg),
        }
    }

    pub(super) async fn edit_task_from_command(&self, payload: &str) -> String {
        let lines: Vec<&str> = payload.lines().collect();
        if lines.len() < 3 {
            return "Usage: /edit [task_id]\\n[title]\\n[description]".to_string();
        }

        let raw_task_id = lines[0].trim();
        let title = lines[1].trim();
        let description = lines[2..].join("\n").trim().to_string();

        if raw_task_id.is_empty() || title.is_empty() {
            return "Task id and title are required.".to_string();
        }

        let Some(task_id) = self.resolve_task_id(raw_task_id).await else {
            return "Invalid task id. Expected short code, task<uuid>, or uuid.".to_string();
        };

        let task = match Task::find_by_id(&self.db.pool, task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => return format!("Task not found: task{task_id}"),
            Err(e) => return format!("Failed to load task: {e}"),
        };

        if task.status != TaskStatus::Todo {
            return format!(
                "Task task{} is {:?}. Only Todo tasks can be edited with /edit.",
                task.id, task.status
            );
        }

        let payload = UpdateTask {
            title: Some(title.to_string()),
            description: Some(description),
            status: None,
            parent_workspace_id: None,
            image_ids: None,
        };

        let base_url = match api_base_url().await {
            Ok(base_url) => base_url,
            Err(e) => return format!("Failed to locate API server: {e}"),
        };

        let client = reqwest::Client::new();
        let response = match client
            .put(format!("{base_url}/tasks/{task_id}"))
            .json(&payload)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => return format!("Failed to edit task: {e}"),
        };

        let api_response: ApiResponse<Task> = match response.json().await {
            Ok(response) => response,
            Err(e) => return format!("Failed to parse task update response: {e}"),
        };

        let error_message = api_response.message().map(String::from);
        match api_response.into_data() {
            Some(updated_task) => {
                let short_id = ShortIdMapping::get_or_create(&self.db.pool, updated_task.id)
                    .await
                    .unwrap_or_else(|_| "????".to_string());
                format!(
                    "Updated task [{}] task{}\\nTitle: {}",
                    short_id, updated_task.id, updated_task.title
                )
            }
            None => error_message.unwrap_or_else(|| "Failed to edit task.".to_string()),
        }
    }

    pub(super) async fn approve_plan_from_command(&self, payload: &str) -> Option<String> {
        let parts: Vec<&str> = payload.split_whitespace().collect();
        if parts.is_empty() {
            return Some("Usage: /approve <task_id>".to_string());
        }

        let Some(task_id) = self.resolve_task_id(parts[0]).await else {
            return Some("Invalid task id. Expected short code, task<uuid>, or uuid.".to_string());
        };

        let Some(plan_approval) = self.find_exit_plan_approval(task_id).await else {
            return Some(format!("No pending plan approval found for task{task_id}."));
        };

        let response = self
            .approvals
            .respond(
                &self.db.pool,
                &plan_approval.approval_id,
                ApprovalResponse {
                    execution_process_id: plan_approval.execution_process_id,
                    status: ApprovalStatus::Approved,
                },
            )
            .await;

        match response {
            Ok(_) => None,
            Err(e) => Some(format!("Failed to approve plan: {e}")),
        }
    }

    pub(super) async fn reject_plan_from_command(&self, payload: &str) -> Option<String> {
        let parts: Vec<&str> = payload.split_whitespace().collect();
        if parts.is_empty() {
            return Some("Usage: /reject <task_id> [reason]".to_string());
        }

        let Some(task_id) = self.resolve_task_id(parts[0]).await else {
            return Some("Invalid task id. Expected short code, task<uuid>, or uuid.".to_string());
        };

        let Some(plan_approval) = self.find_exit_plan_approval(task_id).await else {
            return Some(format!("No pending plan approval found for task{task_id}."));
        };

        let reason = parts.get(1..).map(|rest| rest.join(" ")).and_then(|text| {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

        let response = self
            .approvals
            .respond(
                &self.db.pool,
                &plan_approval.approval_id,
                ApprovalResponse {
                    execution_process_id: plan_approval.execution_process_id,
                    status: ApprovalStatus::Denied {
                        reason: reason.clone(),
                    },
                },
            )
            .await;

        match response {
            Ok(_) => None,
            Err(e) => Some(format!("Failed to reject plan: {e}")),
        }
    }

    pub(super) async fn find_exit_plan_approval(&self, task_id: Uuid) -> Option<PlanApproval> {
        for approval in self.approvals.list_pending() {
            if approval.tool_name != EXIT_PLAN_MODE_NAME {
                continue;
            }

            let ctx =
                ExecutionProcess::load_context(&self.db.pool, approval.execution_process_id).await;
            if let Ok(ctx) = ctx
                && ctx.task.id == task_id
            {
                return Some(PlanApproval {
                    approval_id: approval.id,
                    execution_process_id: approval.execution_process_id,
                });
            }
        }

        None
    }

    async fn resolve_task_id(&self, raw: &str) -> Option<Uuid> {
        let trimmed = raw.trim();
        if let Some(uuid) = parse_task_id(trimmed) {
            return Some(uuid);
        }
        ShortIdMapping::resolve(&self.db.pool, trimmed).await
    }
}

// ─── Internal types ──────────────────────────────────────────────────

pub(super) struct PlanApproval {
    pub(super) approval_id: String,
    pub(super) execution_process_id: Uuid,
}

#[derive(Debug, Serialize)]
struct CreateTaskAttemptBody {
    task_id: Uuid,
    executor_profile_id: ExecutorProfileId,
    repos: Vec<WorkspaceRepoInput>,
}

#[derive(Debug, Serialize)]
struct WorkspaceRepoInput {
    repo_id: Uuid,
    target_branch: String,
}

// ─── Free functions ──────────────────────────────────────────────────

async fn format_task_list(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    tasks: &[TaskWithAttemptStatus],
    project_name: Option<&str>,
) -> String {
    if tasks.is_empty() {
        return match project_name {
            Some(name) => format!("No tasks found for project {name}."),
            None => "No tasks found.".to_string(),
        };
    }

    let mut message = match project_name {
        Some(name) => format!("Tasks for {name} ({}):", tasks.len()),
        None => format!("Tasks ({}):", tasks.len()),
    };

    for task in tasks.iter().take(50) {
        let short_id = ShortIdMapping::get_or_create(pool, task.id)
            .await
            .unwrap_or_else(|_| "????".to_string());
        message.push_str(&format!(
            "\n- [{}] [{}] {}",
            short_id,
            format!("{:?}", &task.status),
            &task.title
        ));
    }

    if tasks.len() > 50 {
        message.push_str(&format!("\n... and {} more", tasks.len() - 50));
    }

    message
}

pub(super) fn parse_task_status(raw: &str) -> Option<TaskStatus> {
    let normalized = raw.trim().to_lowercase();
    TaskStatus::from_str(&normalized).ok()
}

fn parse_task_id(raw: &str) -> Option<Uuid> {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix("task").unwrap_or(trimmed);
    Uuid::parse_str(trimmed).ok()
}

fn parse_executor(raw: &str) -> Result<BaseCodingAgent, String> {
    let normalized = raw.trim().replace('-', "_").to_ascii_uppercase();

    // Try exact match first
    if let Ok(executor) = BaseCodingAgent::from_str(&normalized) {
        return Ok(executor);
    }

    // Try shorthand aliases
    let alias_match = match normalized.as_str() {
        "CLAUDE" | "CLAUDECODE" => Some(BaseCodingAgent::ClaudeCode),
        "GEMINI" => Some(BaseCodingAgent::Gemini),
        "CODEX" => Some(BaseCodingAgent::Codex),
        "OPENCODE" => Some(BaseCodingAgent::Opencode),
        "DROID" => Some(BaseCodingAgent::Droid),
        _ => None,
    };

    alias_match.ok_or_else(|| format!("Unknown executor: {raw}"))
}

fn select_target_branch(branches: &[GitBranch], base_branch: Option<&str>) -> Option<String> {
    if let Some(base) = base_branch {
        if branches.iter().any(|branch| branch.name == base) {
            return Some(base.to_string());
        }
    }

    if let Some(current) = branches.iter().find(|branch| branch.is_current) {
        return Some(current.name.clone());
    }

    branches.first().map(|branch| branch.name.clone())
}

pub(super) fn truncate_text(input: &str, limit: usize) -> String {
    let mut chars = input.chars();
    let truncated: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

pub(super) async fn api_base_url() -> Result<String, std::io::Error> {
    let port = utils::port_file::get_shared_port().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Server port not yet available",
        )
    })?;
    Ok(format!("http://127.0.0.1:{port}/api"))
}
