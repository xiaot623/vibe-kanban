use std::future::Future;

use db::models::{
    project::Project,
    repo::Repo,
    tag::Tag,
    task::{CreateTask, Task, TaskStatus, TaskWithAttemptStatus, UpdateTask},
    workspace::WorkspaceContext,
};
use regex::Regex;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::tool::{Parameters, ToolRouter},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::routes::containers::ContainerQuery;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddTaskRequest {
    #[schemars(description = "The ID of the project to create the task in.")]
    pub project_id: Uuid,
    #[schemars(description = "The title of the task")]
    pub title: String,
    #[schemars(description = "Optional description of the task")]
    pub description: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AddTaskResponse {
    pub task: TaskDetails,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListTasksRequest {
    #[schemars(description = "The ID of the project to list TODO tasks from.")]
    pub project_id: Uuid,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TaskSummary {
    #[schemars(description = "The unique identifier of the task")]
    pub id: String,
    #[schemars(description = "The title of the task")]
    pub title: String,
    #[schemars(description = "Current status of the task")]
    pub status: String,
    #[schemars(description = "When the task was created")]
    pub created_at: String,
    #[schemars(description = "When the task was last updated")]
    pub updated_at: String,
    #[schemars(description = "Whether the task has an in-progress execution attempt")]
    pub has_in_progress_attempt: bool,
    #[schemars(description = "Whether the last execution attempt failed")]
    pub last_attempt_failed: bool,
}

impl TaskSummary {
    fn from_task_with_status(task: TaskWithAttemptStatus) -> Self {
        Self {
            id: task.id.to_string(),
            title: task.title.clone(),
            status: task.status.to_string(),
            created_at: task.created_at.to_rfc3339(),
            updated_at: task.updated_at.to_rfc3339(),
            has_in_progress_attempt: task.has_in_progress_attempt,
            last_attempt_failed: task.last_attempt_failed,
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TaskDetails {
    #[schemars(description = "The unique identifier of the task")]
    pub id: String,
    #[schemars(description = "The project ID this task belongs to")]
    pub project_id: String,
    #[schemars(description = "The title of the task")]
    pub title: String,
    #[schemars(description = "Optional description of the task")]
    pub description: Option<String>,
    #[schemars(description = "Current status of the task")]
    pub status: String,
    #[schemars(description = "When the task was created")]
    pub created_at: String,
    #[schemars(description = "When the task was last updated")]
    pub updated_at: String,
}

impl TaskDetails {
    fn from_task(task: Task) -> Self {
        Self {
            id: task.id.to_string(),
            project_id: task.project_id.to_string(),
            title: task.title,
            description: task.description,
            status: task.status.to_string(),
            created_at: task.created_at.to_rfc3339(),
            updated_at: task.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListTasksResponse {
    pub tasks: Vec<TaskSummary>,
    pub count: usize,
    pub project_id: String,
    pub applied_filters: ListTasksFilters,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListTasksFilters {
    #[schemars(description = "Hardcoded status filter for MCP task listing.")]
    pub status: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EditTaskRequest {
    #[schemars(description = "The ID of the task to edit.")]
    pub task_id: Uuid,
    #[schemars(description = "New title for the task")]
    pub title: Option<String>,
    #[schemars(
        description = "New description for the task. Pass an empty string to clear the description."
    )]
    pub description: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EditTaskResponse {
    pub task: TaskDetails,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteTaskRequest {
    #[schemars(description = "The ID of the task to delete")]
    pub task_id: Uuid,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DeleteTaskResponse {
    pub deleted_task_id: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ProjectRepositoryMeta {
    #[schemars(description = "The unique identifier of the repository")]
    pub id: String,
    #[schemars(description = "The internal repository name")]
    pub name: String,
    #[schemars(description = "The display name of the repository")]
    pub display_name: String,
    #[schemars(description = "Repository absolute path")]
    pub path: String,
    #[schemars(description = "When the repository was created")]
    pub created_at: String,
    #[schemars(description = "When the repository was last updated")]
    pub updated_at: String,
}

impl ProjectRepositoryMeta {
    fn from_repo(repo: Repo) -> Self {
        Self {
            id: repo.id.to_string(),
            name: repo.name,
            display_name: repo.display_name,
            path: repo.path.to_string_lossy().to_string(),
            created_at: repo.created_at.to_rfc3339(),
            updated_at: repo.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ProjectMeta {
    #[schemars(description = "The unique identifier of the project")]
    pub id: String,
    #[schemars(description = "The name of the project")]
    pub name: String,
    #[schemars(description = "Default working dir for single-repo tasks, if set")]
    pub default_agent_working_dir: Option<String>,
    #[schemars(description = "When the project was created")]
    pub created_at: String,
    #[schemars(description = "When the project was last updated")]
    pub updated_at: String,
    #[schemars(description = "Repositories linked to the project")]
    pub repositories: Vec<ProjectRepositoryMeta>,
}

impl ProjectMeta {
    fn from_project_with_repositories(project: Project, repositories: Vec<Repo>) -> Self {
        Self {
            id: project.id.to_string(),
            name: project.name,
            default_agent_working_dir: project.default_agent_working_dir,
            created_at: project.created_at.to_rfc3339(),
            updated_at: project.updated_at.to_rfc3339(),
            repositories: repositories
                .into_iter()
                .map(ProjectRepositoryMeta::from_repo)
                .collect(),
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GetContextResponse {
    #[schemars(description = "All known projects and their metadata.")]
    pub projects: Vec<ProjectMeta>,
    #[schemars(description = "Total number of projects in this response.")]
    pub count: usize,
    #[schemars(
        description = "Active workspace context resolved from the current working directory, if available."
    )]
    pub active_workspace: Option<McpContext>,
}

#[derive(Debug, Clone)]
pub struct TaskServer {
    client: reqwest::Client,
    base_url: String,
    tool_router: ToolRouter<TaskServer>,
    context: Option<McpContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct McpRepoContext {
    #[schemars(description = "The unique identifier of the repository")]
    pub repo_id: Uuid,
    #[schemars(description = "The name of the repository")]
    pub repo_name: String,
    #[schemars(description = "The target branch for this repository in this workspace")]
    pub target_branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct McpContext {
    #[schemars(description = "The project ID for the active workspace session")]
    pub project_id: Uuid,
    #[schemars(description = "The task ID linked to the active workspace session")]
    pub task_id: Uuid,
    #[schemars(description = "The task title linked to the active workspace session")]
    pub task_title: String,
    #[schemars(description = "The workspace/attempt ID for the active session")]
    pub workspace_id: Uuid,
    #[schemars(description = "The workspace branch name")]
    pub workspace_branch: String,
    #[schemars(
        description = "Repository info and target branches for each repo in this workspace"
    )]
    pub workspace_repos: Vec<McpRepoContext>,
}

impl TaskServer {
    pub fn new(base_url: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.to_string(),
            tool_router: Self::tool_router(),
            context: None,
        }
    }

    pub async fn init(mut self) -> Self {
        self.context = self.fetch_workspace_context().await;
        self
    }

    async fn fetch_workspace_context(&self) -> Option<McpContext> {
        let current_dir = std::env::current_dir().ok()?;
        let canonical_path = current_dir.canonicalize().unwrap_or(current_dir);
        let normalized_path = utils::path::normalize_macos_private_alias(&canonical_path);

        let url = self.url("/api/containers/attempt-context");
        let query = ContainerQuery {
            container_ref: normalized_path.to_string_lossy().to_string(),
        };

        let response = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            self.client.get(&url).query(&query).send(),
        )
        .await
        .ok()?
        .ok()?;

        if !response.status().is_success() {
            return None;
        }

        let api_response: ApiResponseEnvelope<WorkspaceContext> = response.json().await.ok()?;
        if !api_response.success {
            return None;
        }

        let ctx = api_response.data?;
        let workspace_repos = ctx
            .workspace_repos
            .into_iter()
            .map(|repo_with_branch| McpRepoContext {
                repo_id: repo_with_branch.repo.id,
                repo_name: repo_with_branch.repo.name,
                target_branch: repo_with_branch.target_branch,
            })
            .collect();

        Some(McpContext {
            project_id: ctx.project.id,
            task_id: ctx.task.id,
            task_title: ctx.task.title,
            workspace_id: ctx.workspace.id,
            workspace_branch: ctx.workspace.branch,
            workspace_repos,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ApiResponseEnvelope<T> {
    success: bool,
    data: Option<T>,
    message: Option<String>,
}

impl TaskServer {
    fn success<T: Serialize>(data: &T) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(data)
                .unwrap_or_else(|_| "Failed to serialize response".to_string()),
        )]))
    }

    fn err_value(v: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::error(vec![Content::text(
            serde_json::to_string_pretty(&v)
                .unwrap_or_else(|_| "Failed to serialize error".to_string()),
        )]))
    }

    fn err<M: Into<String>, D: Into<String>>(
        msg: M,
        details: Option<D>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut v = serde_json::json!({ "success": false, "error": msg.into() });
        if let Some(d) = details {
            v["details"] = serde_json::json!(d.into());
        }
        Self::err_value(v)
    }

    async fn send_json<T: DeserializeOwned>(
        &self,
        rb: reqwest::RequestBuilder,
    ) -> Result<T, CallToolResult> {
        let resp = rb
            .send()
            .await
            .map_err(|e| Self::err("Failed to connect to VK API", Some(e.to_string())).unwrap())?;

        if !resp.status().is_success() {
            return Err(Self::err(
                format!("VK API returned error status: {}", resp.status()),
                None::<String>,
            )
            .unwrap());
        }

        let api_response = resp.json::<ApiResponseEnvelope<T>>().await.map_err(|e| {
            Self::err("Failed to parse VK API response", Some(e.to_string())).unwrap()
        })?;

        if !api_response.success {
            let msg = api_response
                .message
                .unwrap_or_else(|| "Unknown error".to_string());
            return Err(Self::err("VK API returned error", Some(msg)).unwrap());
        }

        api_response
            .data
            .ok_or_else(|| Self::err("VK API response missing data field", None::<String>).unwrap())
    }

    async fn send_empty_json(&self, rb: reqwest::RequestBuilder) -> Result<(), CallToolResult> {
        let resp = rb
            .send()
            .await
            .map_err(|e| Self::err("Failed to connect to VK API", Some(e.to_string())).unwrap())?;

        if !resp.status().is_success() {
            return Err(Self::err(
                format!("VK API returned error status: {}", resp.status()),
                None::<String>,
            )
            .unwrap());
        }

        #[derive(Deserialize)]
        struct EmptyApiResponse {
            success: bool,
            message: Option<String>,
        }

        let api_response = resp.json::<EmptyApiResponse>().await.map_err(|e| {
            Self::err("Failed to parse VK API response", Some(e.to_string())).unwrap()
        })?;

        if !api_response.success {
            let msg = api_response
                .message
                .unwrap_or_else(|| "Unknown error".to_string());
            return Err(Self::err("VK API returned error", Some(msg)).unwrap());
        }

        Ok(())
    }

    fn url(&self, path: &str) -> String {
        let base_url = if self.base_url.ends_with(":0") {
            utils::port_file::get_shared_port()
                .or_else(|| std::env::var("BACKEND_PORT").ok()?.parse::<u16>().ok())
                .or_else(|| std::env::var("PORT").ok()?.parse::<u16>().ok())
                .map(|port| format!("http://127.0.0.1:{port}"))
                .unwrap_or_else(|| self.base_url.clone())
        } else {
            self.base_url.clone()
        };

        format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    async fn fetch_project_meta(&self) -> Result<Vec<ProjectMeta>, CallToolResult> {
        let projects_url = self.url("/api/projects");
        let projects: Vec<Project> = self.send_json(self.client.get(&projects_url)).await?;

        let mut project_meta = Vec::with_capacity(projects.len());
        for project in projects {
            let repos_url = self.url(&format!("/api/projects/{}/repositories", project.id));
            let repos: Vec<Repo> = self.send_json(self.client.get(&repos_url)).await?;
            project_meta.push(ProjectMeta::from_project_with_repositories(project, repos));
        }

        Ok(project_meta)
    }

    /// Expand @tag references in task descriptions.
    async fn expand_tags(&self, text: &str) -> String {
        let tag_pattern = match Regex::new(r"@([^\s@]+)") {
            Ok(re) => re,
            Err(_) => return text.to_string(),
        };

        let tag_names: Vec<String> = tag_pattern
            .captures_iter(text)
            .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        if tag_names.is_empty() {
            return text.to_string();
        }

        let url = self.url("/api/tags");
        let tags: Vec<Tag> = match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<ApiResponseEnvelope<Vec<Tag>>>().await {
                    Ok(envelope) if envelope.success => envelope.data.unwrap_or_default(),
                    _ => return text.to_string(),
                }
            }
            _ => return text.to_string(),
        };

        let tag_map: std::collections::HashMap<&str, &str> = tags
            .iter()
            .map(|tag| (tag.tag_name.as_str(), tag.content.as_str()))
            .collect();

        tag_pattern
            .replace_all(text, |caps: &regex::Captures| {
                let tag_name = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
                match tag_map.get(tag_name) {
                    Some(content) => (*content).to_string(),
                    None => caps
                        .get(0)
                        .map(|m| m.as_str())
                        .unwrap_or_default()
                        .to_string(),
                }
            })
            .into_owned()
    }
}

#[tool_router]
impl TaskServer {
    #[tool(
        description = "Return all project metadata for this Vibe Kanban instance and include active workspace context when available."
    )]
    async fn get_context(&self) -> Result<CallToolResult, ErrorData> {
        let projects = match self.fetch_project_meta().await {
            Ok(projects) => projects,
            Err(err) => return Ok(err),
        };

        let active_workspace = match self.context.clone() {
            Some(context) => Some(context),
            None => self.fetch_workspace_context().await,
        };

        TaskServer::success(&GetContextResponse {
            count: projects.len(),
            projects,
            active_workspace,
        })
    }

    #[tool(
        description = "List TODO tasks for a project. This MCP tool only returns tasks in `todo` status."
    )]
    async fn list_tasks(
        &self,
        Parameters(ListTasksRequest { project_id }): Parameters<ListTasksRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let url = self.url(&format!("/api/tasks?project_id={project_id}"));
        let tasks: Vec<TaskWithAttemptStatus> = match self.send_json(self.client.get(&url)).await {
            Ok(tasks) => tasks,
            Err(err) => return Ok(err),
        };

        let task_summaries = tasks
            .into_iter()
            .filter(|task| task.status == TaskStatus::Todo)
            .map(TaskSummary::from_task_with_status)
            .collect::<Vec<_>>();

        TaskServer::success(&ListTasksResponse {
            count: task_summaries.len(),
            tasks: task_summaries,
            project_id: project_id.to_string(),
            applied_filters: ListTasksFilters {
                status: "todo".to_string(),
            },
        })
    }

    #[tool(description = "Add a new task in TODO state to a project.")]
    async fn add_task(
        &self,
        Parameters(AddTaskRequest {
            project_id,
            title,
            description,
        }): Parameters<AddTaskRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let expanded_description = match description {
            Some(desc) => Some(self.expand_tags(&desc).await),
            None => None,
        };

        let url = self.url("/api/tasks");
        let task: Task = match self
            .send_json(
                self.client
                    .post(&url)
                    .json(&CreateTask::from_title_description(
                        project_id,
                        title,
                        expanded_description,
                    )),
            )
            .await
        {
            Ok(task) => task,
            Err(err) => return Ok(err),
        };

        TaskServer::success(&AddTaskResponse {
            task: TaskDetails::from_task(task),
        })
    }

    #[tool(
        description = "Edit title/description for an existing task. This only works if the task is currently in `todo` status."
    )]
    async fn edit_task(
        &self,
        Parameters(EditTaskRequest {
            task_id,
            title,
            description,
        }): Parameters<EditTaskRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        if title.is_none() && description.is_none() {
            return Self::err(
                "Nothing to edit. Provide at least one of `title` or `description`.".to_string(),
                None::<String>,
            );
        }

        let expanded_description = match description {
            Some(desc) => Some(self.expand_tags(&desc).await),
            None => None,
        };

        let payload = UpdateTask {
            title,
            description: expanded_description,
            status: None,
            parent_workspace_id: None,
            image_ids: None,
        };

        let url = self.url(&format!("/api/tasks/{task_id}"));
        let task: Task = match self
            .send_json(
                self.client
                    .put(&url)
                    .query(&[("expected_status", "todo")])
                    .json(&payload),
            )
            .await
        {
            Ok(task) => task,
            Err(err) => return Ok(err),
        };

        TaskServer::success(&EditTaskResponse {
            task: TaskDetails::from_task(task),
        })
    }

    #[tool(
        description = "Delete a task. This only works if the task is currently in `todo` status."
    )]
    async fn delete_task(
        &self,
        Parameters(DeleteTaskRequest { task_id }): Parameters<DeleteTaskRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let url = self.url(&format!("/api/tasks/{task_id}"));
        if let Err(err) = self
            .send_empty_json(
                self.client
                    .delete(&url)
                    .query(&[("expected_status", "todo")]),
            )
            .await
        {
            return Ok(err);
        }

        TaskServer::success(&DeleteTaskResponse {
            deleted_task_id: task_id.to_string(),
        })
    }
}

#[tool_handler]
impl ServerHandler for TaskServer {
    fn get_info(&self) -> ServerInfo {
        let instruction = "A TODO-task management MCP server for Vibe Kanban. Use 'get_context' to fetch all project metadata (including project IDs and repositories). Use 'list_tasks' to read TODO tasks for a project. Use 'add_task', 'edit_task', and 'delete_task' to manage tasks. Editing and deleting are restricted to tasks currently in TODO state.".to_string();

        ServerInfo {
            protocol_version: ProtocolVersion::V_2025_03_26,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "vibe-kanban".to_string(),
                version: "1.0.0".to_string(),
            },
            instructions: Some(instruction),
        }
    }
}
