use std::{path::PathBuf, str::FromStr};

use db::models::{
    execution_process::{ExecutionProcess, ExecutionProcessRunReason, ExecutionProcessStatus},
    project::Project,
    project_repo::ProjectRepo,
    session::Session,
    task::{CreateTask, Task, TaskStatus, TaskWithAttemptStatus},
    workspace::Workspace,
    workspace_repo::WorkspaceRepo,
};
use executors::{
    executors::BaseCodingAgent,
    profile::{ExecutorProfileId, canonical_variant_key},
};
use serde::Serialize;
use utils::response::ApiResponse;
use uuid::Uuid;

use super::TelegramBotService;
use crate::services::{git::GitBranch, telegram::EXIT_PLAN_MODE_NAME};

// ─── Shared command logic (interactive Telegram bot) ─────────────────

impl TelegramBotService {
    pub(super) async fn create_daily_task_from_message(
        &self,
        message: &str,
    ) -> Result<Task, String> {
        let Some((title, description)) = parse_message_as_task(message) else {
            return Err("Message is empty. Please send a task title.".to_string());
        };

        let daily_project_id_raw = {
            let config = self.config.read().await;
            config.daily_mode.project_id.clone()
        };

        let Some(daily_project_id_raw) = daily_project_id_raw else {
            return Err(
                "Daily Mode is not configured. Open the app and enter Daily Mode once first."
                    .to_string(),
            );
        };

        let daily_project_id = Uuid::parse_str(&daily_project_id_raw).map_err(|_| {
            "Daily Mode project id is invalid. Please reconfigure Daily Mode in the app."
                .to_string()
        })?;

        let daily_project = Project::find_by_id(&self.db.pool, daily_project_id)
            .await
            .map_err(|e| format!("Failed to load Daily project: {e}"))?;
        if daily_project.is_none() {
            return Err(
                "Daily project was not found. Please reconfigure Daily Mode in the app."
                    .to_string(),
            );
        }

        let task_id = Uuid::new_v4();
        Task::create(
            &self.db.pool,
            &CreateTask::from_title_description(daily_project_id, title, description),
            task_id,
        )
        .await
        .map_err(|e| format!("Failed to create Daily task: {e}"))
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

    /// Shared run logic used by interactive command and callback flows.
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
                                "No branches found for {}. Configure a target branch and try again.",
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

    pub(super) async fn create_review_task_from_workspace(
        &self,
        source_workspace_id: Uuid,
    ) -> Result<CreateReviewTaskResult, String> {
        tracing::info!(
            "Creating review task using source workspace {}",
            source_workspace_id
        );

        let executor_profile_id = {
            let config = self.config.read().await;
            config
                .review_executor_profile
                .clone()
                .unwrap_or_else(|| config.executor_profile.clone())
        };

        let base_url = api_base_url()
            .await
            .map_err(|e| format!("Failed to locate API server: {e}"))?;

        let request = StartReviewSubtaskBody {
            executor_profile_id,
        };

        let client = reqwest::Client::new();
        let response = client
            .post(format!(
                "{base_url}/task-attempts/{source_workspace_id}/review-subtask/start"
            ))
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("Failed to create review task: {e}"))?;

        let api_response: ApiResponse<TaskWithAttemptStatus> = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse review task response: {e}"))?;

        let error_message = api_response.message().map(String::from);
        match api_response.into_data() {
            Some(task) => Ok(CreateReviewTaskResult { task }),
            None => {
                Err(error_message.unwrap_or_else(|| "Failed to create review task.".to_string()))
            }
        }
    }

    pub(super) async fn send_follow_up_reply(
        &self,
        session_id: Uuid,
        prompt: &str,
    ) -> Result<Option<ExecutorProfileId>, String> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err("Reply cannot be empty.".to_string());
        }

        let latest_profile =
            ExecutionProcess::latest_executor_profile_for_session(&self.db.pool, session_id)
                .await
                .map_err(|e| format!("Failed to load follow-up executor profile: {e}"))?;
        let (variant, executor) = match latest_profile.as_ref() {
            Some(profile) => (profile.variant.clone(), Some(profile.executor.clone())),
            None => (None, None),
        };
        let base_url = api_base_url()
            .await
            .map_err(|e| format!("Failed to locate API server: {e}"))?;

        let request = CreateFollowUpAttemptBody {
            prompt: prompt.to_string(),
            variant,
            executor,
            retry_process_id: None,
            force_when_dirty: None,
            perform_git_reset: None,
        };

        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base_url}/sessions/{session_id}/follow-up"))
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("Failed to send follow-up reply: {e}"))?;

        let api_response: ApiResponse<ExecutionProcess> = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse follow-up response: {e}"))?;

        let error_message = api_response.message().map(String::from);
        match api_response.into_data() {
            Some(_) => Ok(latest_profile),
            None => {
                Err(error_message.unwrap_or_else(|| "Failed to send follow-up reply.".to_string()))
            }
        }
    }

    pub(super) async fn find_exit_plan_approval_for_flow(
        &self,
        session_id: Uuid,
        execution_process_id: Option<Uuid>,
    ) -> Option<PlanApproval> {
        for approval in self.approvals.list_pending() {
            if approval.tool_name != EXIT_PLAN_MODE_NAME {
                continue;
            }

            if let Some(expected_execution_process_id) = execution_process_id
                && approval.execution_process_id != expected_execution_process_id
            {
                continue;
            }

            let ctx =
                ExecutionProcess::load_context(&self.db.pool, approval.execution_process_id).await;
            if let Ok(ctx) = ctx
                && ctx.session.id == session_id
            {
                return Some(PlanApproval {
                    approval_id: approval.id,
                    execution_process_id: approval.execution_process_id,
                });
            }
        }

        None
    }

    pub(super) async fn find_exit_plan_approval_by_task(
        &self,
        task_id: Uuid,
    ) -> Option<PlanApproval> {
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

    pub(super) async fn has_running_sibling_flow(
        &self,
        task_id: Uuid,
        current_session_id: Uuid,
    ) -> Result<bool, String> {
        let workspaces = Workspace::fetch_all(&self.db.pool, Some(task_id))
            .await
            .map_err(|e| format!("Failed to load task attempts: {e}"))?;

        for workspace in workspaces {
            let sessions = Session::find_by_workspace_id(&self.db.pool, workspace.id)
                .await
                .map_err(|e| {
                    format!(
                        "Failed to load sessions for workspace {}: {e}",
                        workspace.id
                    )
                })?;
            for session in sessions {
                if session.id == current_session_id {
                    continue;
                }
                if let Some(process) = ExecutionProcess::find_latest_by_session_and_run_reason(
                    &self.db.pool,
                    session.id,
                    &ExecutionProcessRunReason::CodingAgent,
                )
                .await
                .map_err(|e| format!("Failed to inspect execution process: {e}"))?
                    && process.status == ExecutionProcessStatus::Running
                {
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }
}

// ─── Internal types ──────────────────────────────────────────────────

pub(super) struct PlanApproval {
    pub(super) approval_id: String,
    pub(super) execution_process_id: Uuid,
}

pub(super) struct CreateReviewTaskResult {
    pub(super) task: TaskWithAttemptStatus,
}

#[derive(Debug, Serialize)]
struct CreateTaskAttemptBody {
    task_id: Uuid,
    executor_profile_id: ExecutorProfileId,
    repos: Vec<WorkspaceRepoInput>,
}

#[derive(Debug, Serialize)]
struct CreateFollowUpAttemptBody {
    prompt: String,
    variant: Option<String>,
    executor: Option<BaseCodingAgent>,
    retry_process_id: Option<Uuid>,
    force_when_dirty: Option<bool>,
    perform_git_reset: Option<bool>,
}

#[derive(Debug, Serialize)]
struct StartReviewSubtaskBody {
    executor_profile_id: ExecutorProfileId,
}

#[derive(Debug, Serialize)]
struct WorkspaceRepoInput {
    repo_id: Uuid,
    target_branch: String,
}

// ─── Free functions ──────────────────────────────────────────────────

pub(super) fn parse_task_status(raw: &str) -> Option<TaskStatus> {
    let normalized = raw.trim().to_lowercase();
    TaskStatus::from_str(&normalized).ok()
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
        "PI" => Some(BaseCodingAgent::Pi),
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

pub(super) fn parse_message_as_task(message: &str) -> Option<(String, Option<String>)> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut lines = trimmed.lines();
    let title = lines.next()?.trim();
    if title.is_empty() {
        return None;
    }

    let description_raw = lines.collect::<Vec<_>>().join("\n");
    let description = if description_raw.trim().is_empty() {
        None
    } else {
        Some(description_raw.trim().to_string())
    };

    Some((title.to_string(), description))
}

/// Format the user-facing success message for a newly-created review task.
pub(super) fn format_review_task_created_message(
    short_id: &str,
    has_in_progress_attempt: bool,
) -> String {
    let mut message = format!("✅ Review task created: [{short_id}]");
    if !has_in_progress_attempt {
        message.push_str("\n⚠️ Created but not auto-started. Open the task and run it manually.");
    }
    message
}

#[cfg(test)]
mod tests {
    use super::{format_review_task_created_message, parse_message_as_task};

    #[test]
    fn parse_message_as_task_uses_first_line_as_title() {
        let parsed = parse_message_as_task("Finish report\nInclude metrics\nand summary").unwrap();

        assert_eq!(parsed.0, "Finish report");
        assert_eq!(parsed.1.as_deref(), Some("Include metrics\nand summary"));
    }

    #[test]
    fn parse_message_as_task_handles_single_line() {
        let parsed = parse_message_as_task("Quick follow-up").unwrap();

        assert_eq!(parsed.0, "Quick follow-up");
        assert_eq!(parsed.1, None);
    }

    #[test]
    fn parse_message_as_task_rejects_empty_input() {
        assert!(parse_message_as_task("   \n  ").is_none());
    }

    // ── format_review_task_created_message ───────────────────────────

    #[test]
    fn format_review_task_created_message_with_auto_started_attempt() {
        let msg = format_review_task_created_message("ab12", true);
        assert_eq!(msg, "✅ Review task created: [ab12]");
        assert!(!msg.contains("not auto-started"));
    }

    #[test]
    fn format_review_task_created_message_without_auto_started_attempt() {
        let msg = format_review_task_created_message("zz99", false);
        assert!(msg.starts_with("✅ Review task created: [zz99]"));
        assert!(msg.contains("not auto-started"));
    }
}
