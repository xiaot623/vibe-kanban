use std::future::Future;

use db::models::{
    tag::Tag,
    task::{CreateTask, Task, TaskStatus, TaskWithAttemptStatus, UpdateTask},
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddTaskRequest {
    #[schemars(
        description = "The ID of the project to create the task in. In agent runtimes this is usually available in VK_PROJECT_ID."
    )]
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
    #[schemars(
        description = "The ID of the project to list TODO tasks from. In agent runtimes this is usually available in VK_PROJECT_ID."
    )]
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

#[derive(Debug, Clone)]
pub struct TaskServer {
    client: reqwest::Client,
    base_url: String,
    tool_router: ToolRouter<TaskServer>,
}

impl TaskServer {
    pub fn new(base_url: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.to_string(),
            tool_router: Self::tool_router(),
        }
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
        let instruction = "A TODO-task management MCP server for Vibe Kanban. Use 'list_tasks' to read TODO tasks for a project (project_id is usually available as VK_PROJECT_ID in the runtime environment). Use 'add_task', 'edit_task', and 'delete_task' to manage tasks. Editing and deleting are restricted to tasks currently in TODO state.".to_string();

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
