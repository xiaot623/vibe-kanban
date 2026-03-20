use std::env;

use anyhow::{anyhow, bail, Context};
use db::models::{
    project::{CreateProject, Project, UpdateProject},
    task::{CreateTask, Task, TaskStatus, TaskWithAttemptStatus, UpdateTask},
};
use reqwest::{Method, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::cli::{
    CliCommand, ConnectionArgs, ProjectArgs, ProjectCommand, TaskAddArgs, TaskArgs, TaskCommand,
    TaskModifyArgs, TaskStatusArg,
};

pub async fn execute_cli_command(command: CliCommand) -> anyhow::Result<String> {
    match command {
        CliCommand::Project(project) => execute_project_command(project).await,
        CliCommand::Task(task) => execute_task_command(task).await,
        other => bail!("Unsupported CLI command for HTTP execution: {other:?}"),
    }
}

pub async fn execute_project_command(args: ProjectArgs) -> anyhow::Result<String> {
    match args.command {
        ProjectCommand::List(command) => {
            let client = KanbanApiClient::from_connection(&command.connection).await?;
            let projects = client.list_projects().await?;
            render_projects(&projects, command.connection.json)
        }
        ProjectCommand::Get(command) => {
            let client = KanbanApiClient::from_connection(&command.connection).await?;
            let project = client.resolve_project_selector(&command.selector).await?;
            render_project(&project, command.connection.json)
        }
        ProjectCommand::Add(command) => {
            let client = KanbanApiClient::from_connection(&command.connection).await?;
            let project = client
                .create_project(CreateProject {
                    name: command.name,
                    repositories: command.repositories,
                })
                .await?;
            render_project(&project, command.connection.json)
        }
        ProjectCommand::Modify(command) => {
            let client = KanbanApiClient::from_connection(&command.connection).await?;
            let project = client.resolve_project_selector(&command.selector).await?;
            let updated = client
                .update_project(
                    project.id,
                    UpdateProject {
                        name: Some(command.name),
                    },
                )
                .await?;
            render_project(&updated, command.connection.json)
        }
        ProjectCommand::Delete(command) => {
            let client = KanbanApiClient::from_connection(&command.connection).await?;
            let project = client.resolve_project_selector(&command.selector).await?;
            client.delete_project(project.id).await?;
            render_deleted_project(&project, command.connection.json)
        }
    }
}

pub async fn execute_task_command(args: TaskArgs) -> anyhow::Result<String> {
    match args.command {
        TaskCommand::List(command) => {
            let client = KanbanApiClient::from_connection(&command.connection).await?;
            let project = client.resolve_project_selector(&command.project).await?;
            let tasks = client.list_tasks(project.id).await?;
            render_tasks(&tasks, command.connection.json)
        }
        TaskCommand::Get(command) => {
            let client = KanbanApiClient::from_connection(&command.connection).await?;
            let task = client.get_task(command.task_id).await?;
            render_task(&task, command.connection.json)
        }
        TaskCommand::Add(command) => execute_task_add(command).await,
        TaskCommand::Modify(command) => execute_task_modify(command).await,
        TaskCommand::Delete(command) => {
            let client = KanbanApiClient::from_connection(&command.connection).await?;
            client.delete_task(command.task_id.clone()).await?;
            render_deleted_task(&command.task_id, command.connection.json)
        }
        TaskCommand::Query(command) => {
            let client = KanbanApiClient::from_connection(&command.connection).await?;
            let project = client.resolve_project_selector(&command.project).await?;
            let tasks = client.list_tasks(project.id).await?;
            let filtered = filter_tasks_for_query(tasks, &command.q, command.status);
            render_tasks(&filtered, command.connection.json)
        }
    }
}

async fn execute_task_add(command: TaskAddArgs) -> anyhow::Result<String> {
    let client = KanbanApiClient::from_connection(&command.connection).await?;
    let project = client.resolve_project_selector(&command.project).await?;
    let task = client
        .create_task(CreateTask {
            project_id: project.id,
            title: command.title,
            description: command.description,
            status: command.status.map(to_task_status),
            parent_workspace_id: None,
            source_cron_task_id: None,
            image_ids: None,
        })
        .await?;

    render_task(&task, command.connection.json)
}

async fn execute_task_modify(command: TaskModifyArgs) -> anyhow::Result<String> {
    let client = KanbanApiClient::from_connection(&command.connection).await?;
    let description = if command.clear_description {
        Some(String::new())
    } else {
        command.description
    };

    let task = client
        .update_task(
            command.task_id,
            UpdateTask {
                title: command.title,
                description,
                status: command.status.map(to_task_status),
                parent_workspace_id: None,
                image_ids: None,
            },
        )
        .await?;

    render_task(&task, command.connection.json)
}

#[derive(Debug, Clone)]
struct KanbanApiClient {
    client: reqwest::Client,
    base_url: String,
    password: Option<String>,
}

impl KanbanApiClient {
    async fn from_connection(connection: &ConnectionArgs) -> anyhow::Result<Self> {
        let base_url = resolve_server_base_url(connection).await?;
        let client = reqwest::Client::builder()
            .build()
            .context("Failed to create CLI HTTP client")?;

        Ok(Self {
            client,
            base_url,
            password: connection.password.clone(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self.client.request(method, self.url(path));
        if let Some(password) = &self.password {
            builder.basic_auth("", Some(password))
        } else {
            builder
        }
    }

    async fn list_projects(&self) -> anyhow::Result<Vec<Project>> {
        self.send(self.request(Method::GET, "/api/projects")).await
    }

    async fn get_project(&self, project_id: Uuid) -> anyhow::Result<Project> {
        self.send(self.request(Method::GET, &format!("/api/projects/{project_id}")))
            .await
    }

    async fn create_project(&self, payload: CreateProject) -> anyhow::Result<Project> {
        self.send(self.request(Method::POST, "/api/projects").json(&payload))
            .await
    }

    async fn update_project(
        &self,
        project_id: Uuid,
        payload: UpdateProject,
    ) -> anyhow::Result<Project> {
        self.send(
            self.request(Method::PUT, &format!("/api/projects/{project_id}"))
                .json(&payload),
        )
        .await
    }

    async fn delete_project(&self, project_id: Uuid) -> anyhow::Result<()> {
        self.send_empty(self.request(Method::DELETE, &format!("/api/projects/{project_id}")))
            .await
    }

    async fn get_task(&self, task_id: String) -> anyhow::Result<Task> {
        self.send(self.request(Method::GET, &format!("/api/tasks/{task_id}")))
            .await
    }

    async fn list_tasks(&self, project_id: Uuid) -> anyhow::Result<Vec<TaskWithAttemptStatus>> {
        self.send(
            self.request(Method::GET, "/api/tasks")
                .query(&[("project_id", project_id.to_string())]),
        )
        .await
    }

    async fn create_task(&self, payload: CreateTask) -> anyhow::Result<Task> {
        self.send(self.request(Method::POST, "/api/tasks").json(&payload))
            .await
    }

    async fn update_task(&self, task_id: String, payload: UpdateTask) -> anyhow::Result<Task> {
        self.send(
            self.request(Method::PUT, &format!("/api/tasks/{task_id}"))
                .json(&payload),
        )
        .await
    }

    async fn delete_task(&self, task_id: String) -> anyhow::Result<()> {
        self.send_empty(self.request(Method::DELETE, &format!("/api/tasks/{task_id}")))
            .await
    }

    async fn resolve_project_selector(&self, selector: &str) -> anyhow::Result<Project> {
        if let Ok(project_id) = Uuid::parse_str(selector) {
            return self
                .get_project(project_id)
                .await
                .with_context(|| format!("Failed to load project '{selector}'"));
        }

        let normalized_selector = selector.trim().to_lowercase();
        let matches = self
            .list_projects()
            .await?
            .into_iter()
            .filter(|project| project.name.to_lowercase() == normalized_selector)
            .collect::<Vec<_>>();

        match matches.as_slice() {
            [project] => Ok(project.clone()),
            [] => bail!(
                "Project '{selector}' not found. Start a server with `kanban server` or run `kanban project list`."
            ),
            _ => {
                let names = matches
                    .iter()
                    .map(|project| format!("{} ({})", project.name, project.id))
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("Project selector '{selector}' is ambiguous: {names}");
            }
        }
    }

    async fn send<T>(&self, request: reqwest::RequestBuilder) -> anyhow::Result<T>
    where
        T: DeserializeOwned,
    {
        let response = request
            .send()
            .await
            .context("Failed to connect to Vibe Kanban server")?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .context("Failed to read API response body")?;

        if !status.is_success() {
            bail!("{}", http_error_message(status, &body));
        }

        let envelope: ApiEnvelope<T> = serde_json::from_slice(&body).with_context(|| {
            format!(
                "Failed to parse API response from {}",
                self.base_url.trim_end_matches('/')
            )
        })?;

        if !envelope.success {
            bail!(
                "{}",
                api_error_message(envelope.message, envelope.error_data)
            );
        }

        envelope
            .data
            .ok_or_else(|| anyhow!("API response did not include data"))
    }

    async fn send_empty(&self, request: reqwest::RequestBuilder) -> anyhow::Result<()> {
        let response = request
            .send()
            .await
            .context("Failed to connect to Vibe Kanban server")?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .context("Failed to read API response body")?;

        if !status.is_success() {
            bail!("{}", http_error_message(status, &body));
        }

        let envelope: ApiEnvelope<Value> = serde_json::from_slice(&body).with_context(|| {
            format!(
                "Failed to parse API response from {}",
                self.base_url.trim_end_matches('/')
            )
        })?;

        if !envelope.success {
            bail!(
                "{}",
                api_error_message(envelope.message, envelope.error_data)
            );
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    success: bool,
    data: Option<T>,
    error_data: Option<Value>,
    message: Option<String>,
}

async fn resolve_server_base_url(connection: &ConnectionArgs) -> anyhow::Result<String> {
    let port_from_file = utils::port_file::read_port_file("vibe-kanban").await.ok();
    resolve_server_base_url_from_sources(
        connection.server_url.as_deref(),
        port_from_file,
        env::var("BACKEND_PORT").ok().as_deref(),
        env::var("PORT").ok().as_deref(),
    )
}

fn resolve_server_base_url_from_sources(
    explicit: Option<&str>,
    port_from_file: Option<u16>,
    backend_port: Option<&str>,
    port_env: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(url) = explicit.and_then(normalize_optional_string) {
        return Ok(url.to_string());
    }

    if let Some(port) = port_from_file {
        return Ok(loopback_url(port));
    }

    if let Some(port) = parse_port_source(backend_port) {
        return Ok(loopback_url(port));
    }

    if let Some(port) = parse_port_source(port_env) {
        return Ok(loopback_url(port));
    }

    bail!(
        "Could not determine a Vibe Kanban server URL. Pass --server-url, set KANBAN_SERVER_URL, or start one with `kanban server`."
    )
}

fn parse_port_source(raw: Option<&str>) -> Option<u16> {
    raw.and_then(normalize_optional_string)
        .and_then(|value| value.parse::<u16>().ok())
}

fn normalize_optional_string(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn loopback_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn http_error_message(status: StatusCode, body: &[u8]) -> String {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        if let Some(message) = value.get("message").and_then(Value::as_str) {
            if !message.trim().is_empty() {
                return message.to_string();
            }
        }
    }

    if let Ok(text) = std::str::from_utf8(body) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    format!("Request failed with status {status}")
}

fn api_error_message(message: Option<String>, error_data: Option<Value>) -> String {
    if let Some(message) = message.filter(|value| !value.trim().is_empty()) {
        return message;
    }

    if let Some(error_data) = error_data {
        return serde_json::to_string_pretty(&error_data)
            .unwrap_or_else(|_| "API request failed".to_string());
    }

    "API request failed".to_string()
}

fn filter_tasks_for_query(
    tasks: Vec<TaskWithAttemptStatus>,
    query: &str,
    status: Option<TaskStatusArg>,
) -> Vec<TaskWithAttemptStatus> {
    let normalized_query = query.trim().to_lowercase();
    let status_filter = status.map(to_task_status);

    tasks
        .into_iter()
        .filter(|task| {
            if let Some(status_filter) = &status_filter {
                if &task.status != status_filter {
                    return false;
                }
            }

            if normalized_query.is_empty() {
                return true;
            }

            let title_matches = task.title.to_lowercase().contains(&normalized_query);
            let description_matches = task
                .description
                .as_deref()
                .unwrap_or_default()
                .to_lowercase()
                .contains(&normalized_query);

            title_matches || description_matches
        })
        .collect()
}

fn to_task_status(status: TaskStatusArg) -> TaskStatus {
    match status {
        TaskStatusArg::Todo => TaskStatus::Todo,
        TaskStatusArg::Inprogress => TaskStatus::InProgress,
        TaskStatusArg::Inreview => TaskStatus::InReview,
        TaskStatusArg::Done => TaskStatus::Done,
        TaskStatusArg::Cancelled => TaskStatus::Cancelled,
    }
}

#[derive(Debug, Serialize)]
struct DeletedProjectOutput {
    deleted_project_id: Uuid,
    deleted_project_name: String,
}

#[derive(Debug, Serialize)]
struct DeletedTaskOutput<'a> {
    deleted_task_id: &'a str,
}

fn render_projects(projects: &[Project], json: bool) -> anyhow::Result<String> {
    if json {
        return render_json(projects);
    }

    if projects.is_empty() {
        return Ok("No projects found.\n".to_string());
    }

    Ok(projects
        .iter()
        .map(|project| format!("{}\t{}", project.id, project.name))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
}

fn render_project(project: &Project, json: bool) -> anyhow::Result<String> {
    if json {
        return render_json(project);
    }

    Ok(format!(
        "id: {}\nname: {}\ndefault_agent_working_dir: {}\ncreated_at: {}\nupdated_at: {}\n",
        project.id,
        project.name,
        project
            .default_agent_working_dir
            .as_deref()
            .unwrap_or("(none)"),
        project.created_at.to_rfc3339(),
        project.updated_at.to_rfc3339()
    ))
}

fn render_deleted_project(project: &Project, json: bool) -> anyhow::Result<String> {
    let output = DeletedProjectOutput {
        deleted_project_id: project.id,
        deleted_project_name: project.name.clone(),
    };

    if json {
        return render_json(&output);
    }

    Ok(format!(
        "Deleted project {} ({})\n",
        project.name, project.id
    ))
}

fn render_tasks(tasks: &[TaskWithAttemptStatus], json: bool) -> anyhow::Result<String> {
    if json {
        return render_json(tasks);
    }

    if tasks.is_empty() {
        return Ok("No tasks found.\n".to_string());
    }

    Ok(tasks
        .iter()
        .map(|task| format!("{}\t{}\t{}", task.id, task.status, task.title))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
}

fn render_task(task: &Task, json: bool) -> anyhow::Result<String> {
    if json {
        return render_json(task);
    }

    Ok(format!(
        "id: {}\nproject_id: {}\nstatus: {}\ntitle: {}\ndescription: {}\ncreated_at: {}\nupdated_at: {}\n",
        task.id,
        task.project_id,
        task.status,
        task.title,
        task.description.as_deref().unwrap_or("(none)"),
        task.created_at.to_rfc3339(),
        task.updated_at.to_rfc3339()
    ))
}

fn render_deleted_task(task_id: &str, json: bool) -> anyhow::Result<String> {
    let output = DeletedTaskOutput {
        deleted_task_id: task_id,
    };

    if json {
        return render_json(&output);
    }

    Ok(format!("Deleted task {task_id}\n"))
}

fn render_json<T>(value: &T) -> anyhow::Result<String>
where
    T: Serialize + ?Sized,
{
    Ok(format!(
        "{}\n",
        serde_json::to_string_pretty(value).context("Failed to serialize CLI output")?
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex, OnceLock},
    };

    use axum::{
        body::to_bytes,
        extract::{Request, State},
        response::{IntoResponse, Response},
        routing::any,
        Router,
    };
    use reqwest::StatusCode;
    use serde_json::{json, Value};
    use uuid::Uuid;

    use super::{
        filter_tasks_for_query, resolve_server_base_url_from_sources, ConnectionArgs,
        CreateProject, KanbanApiClient, TaskArgs, TaskCommand, TaskStatusArg,
        TaskWithAttemptStatus, UpdateProject,
    };
    use crate::cli::{TaskModifyArgs, TaskQueryArgs};

    #[test]
    fn server_url_resolution_uses_expected_priority() {
        assert_eq!(
            resolve_server_base_url_from_sources(
                Some(" http://example.test/api "),
                Some(3001),
                Some("3002"),
                Some("3003"),
            )
            .unwrap(),
            "http://example.test/api"
        );
        assert_eq!(
            resolve_server_base_url_from_sources(None, Some(3001), Some("3002"), Some("3003"))
                .unwrap(),
            "http://127.0.0.1:3001"
        );
        assert_eq!(
            resolve_server_base_url_from_sources(None, None, Some("3002"), Some("3003")).unwrap(),
            "http://127.0.0.1:3002"
        );
        assert_eq!(
            resolve_server_base_url_from_sources(None, None, None, Some("3003")).unwrap(),
            "http://127.0.0.1:3003"
        );

        let err = resolve_server_base_url_from_sources(None, None, Some("bad"), Some("also-bad"))
            .expect_err("expected resolution to fail");
        assert!(err.to_string().contains("kanban server"));
    }

    #[tokio::test]
    async fn client_injects_basic_auth_header() {
        let server =
            TestServer::spawn(vec![MockReply::success(json!([project_json("Alpha")]))]).await;
        let client = server.client(Some("secret".to_string()));

        let projects = client.list_projects().await.expect("expected success");
        assert_eq!(projects.len(), 1);

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/api/projects");
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Basic OnNlY3JldA==")
        );
    }

    #[tokio::test]
    async fn project_requests_use_expected_methods_paths_and_payloads() {
        let created = project_json("Alpha");
        let renamed = project_json("Beta");
        let created_id = created["id"].as_str().unwrap().to_string();

        let server = TestServer::spawn(vec![
            MockReply::success(json!([created.clone()])),
            MockReply::success(created.clone()),
            MockReply::success(created.clone()),
            MockReply::success(renamed.clone()),
            MockReply::accepted_success(),
        ])
        .await;
        let client = server.client(None);

        let listed = client.list_projects().await.expect("list should succeed");
        assert_eq!(listed.len(), 1);

        let created_project = client
            .create_project(CreateProject {
                name: "Alpha".to_string(),
                repositories: vec![
                    db::models::project_repo::CreateProjectRepo {
                        display_name: "frontend".to_string(),
                        git_repo_path: "/tmp/frontend".to_string(),
                    },
                    db::models::project_repo::CreateProjectRepo {
                        display_name: "backend".to_string(),
                        git_repo_path: "/tmp/backend".to_string(),
                    },
                ],
            })
            .await
            .expect("create should succeed");
        assert_eq!(created_project.name, "Alpha");

        let project_id = uuid::Uuid::parse_str(&created_id).unwrap();
        let fetched = client
            .get_project(project_id)
            .await
            .expect("get should succeed");
        assert_eq!(fetched.id, project_id);

        let updated = client
            .update_project(
                project_id,
                UpdateProject {
                    name: Some("Beta".to_string()),
                },
            )
            .await
            .expect("update should succeed");
        assert_eq!(updated.name, "Beta");

        client
            .delete_project(project_id)
            .await
            .expect("delete should succeed");

        let requests = server.requests();
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/api/projects");
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/api/projects");
        assert_eq!(
            requests[1].body.as_ref().unwrap()["repositories"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(requests[2].method, "GET");
        assert_eq!(requests[2].path, format!("/api/projects/{created_id}"));
        assert_eq!(requests[3].method, "PUT");
        assert_eq!(requests[3].body.as_ref().unwrap()["name"], json!("Beta"));
        assert_eq!(requests[4].method, "DELETE");
        assert_eq!(requests[4].path, format!("/api/projects/{created_id}"));
    }

    #[tokio::test]
    async fn project_selector_reports_conflicts_and_missing_names() {
        let server = TestServer::spawn(vec![MockReply::success(json!([
            project_json("Alpha"),
            project_json("alpha"),
        ]))])
        .await;
        let client = server.client(None);

        let err = client
            .resolve_project_selector("ALPHA")
            .await
            .expect_err("expected conflict");
        assert!(err.to_string().contains("ambiguous"));

        let server = TestServer::spawn(vec![MockReply::success(json!([]))]).await;
        let client = server.client(None);
        let err = client
            .resolve_project_selector("missing")
            .await
            .expect_err("expected not found");
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn task_modify_clear_description_maps_to_empty_string() {
        let task = task_json(task_project_id(), "Fix bug", Some("old"), "done");
        let task_id = task["id"].as_str().unwrap().to_string();
        let server = TestServer::spawn(vec![MockReply::success(task.clone())]).await;

        super::execute_task_command(TaskArgs {
            command: TaskCommand::Modify(TaskModifyArgs {
                task_id: task_id.clone(),
                title: None,
                description: None,
                clear_description: true,
                status: Some(TaskStatusArg::Done),
                connection: connection_args(&server.base_url, false),
            }),
        })
        .await
        .expect("modify should succeed");

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "PUT");
        assert_eq!(requests[0].path, format!("/api/tasks/{task_id}"));
        assert_eq!(requests[0].body.as_ref().unwrap()["description"], json!(""));
        assert_eq!(requests[0].body.as_ref().unwrap()["status"], json!("done"));
    }

    #[tokio::test]
    async fn task_query_uses_resolved_project_and_frontend_filtering() {
        let alpha = project_json("Alpha");
        let beta = project_json("Beta");
        let alpha_id = alpha["id"].as_str().unwrap().to_string();
        let tasks = json!([
            task_with_status_json(&alpha_id, "Needle title", Some("desc"), "done"),
            task_with_status_json(&alpha_id, "Ignore", Some("Needle in description"), "todo"),
            task_with_status_json(&alpha_id, "Other", Some("other"), "done"),
        ]);

        let server = TestServer::spawn(vec![
            MockReply::success(json!([alpha.clone(), beta])),
            MockReply::success(tasks),
        ])
        .await;

        let output = super::execute_task_command(TaskArgs {
            command: TaskCommand::Query(TaskQueryArgs {
                project: "Alpha".to_string(),
                q: "needle".to_string(),
                status: Some(TaskStatusArg::Done),
                connection: connection_args(&server.base_url, true),
            }),
        })
        .await
        .expect("query should succeed");

        let parsed: Vec<TaskWithAttemptStatus> =
            serde_json::from_str(&output).expect("expected JSON output");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "Needle title");

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].path, "/api/projects");
        assert_eq!(requests[1].path, "/api/tasks");
        let expected_query = format!("project_id={alpha_id}");
        assert_eq!(requests[1].query.as_deref(), Some(expected_query.as_str()));
    }

    #[tokio::test]
    async fn api_response_message_is_propagated() {
        let server = TestServer::spawn(vec![MockReply::api_error("boom")]).await;
        let client = server.client(None);

        let err = client
            .list_projects()
            .await
            .expect_err("expected API envelope error");
        assert_eq!(err.to_string(), "boom");
    }

    #[test]
    fn query_filter_matches_frontend_rules() {
        let project_id = task_project_id().to_string();
        let tasks = vec![
            deserialize_task_with_status(task_with_status_json(
                &project_id,
                "Needle",
                Some("other"),
                "todo",
            )),
            deserialize_task_with_status(task_with_status_json(
                &project_id,
                "Other",
                Some("needle in description"),
                "done",
            )),
            deserialize_task_with_status(task_with_status_json(
                &project_id,
                "Other",
                Some("other"),
                "done",
            )),
        ];

        let filtered = filter_tasks_for_query(tasks, "needle", Some(TaskStatusArg::Done));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].status.to_string(), "done");
        assert_eq!(
            filtered[0].description.as_deref(),
            Some("needle in description")
        );
    }

    #[derive(Debug, Clone)]
    struct RecordedRequest {
        method: String,
        path: String,
        query: Option<String>,
        authorization: Option<String>,
        body: Option<Value>,
    }

    #[derive(Debug, Clone)]
    struct MockReply {
        status: StatusCode,
        body: Value,
    }

    impl MockReply {
        fn success(data: Value) -> Self {
            Self {
                status: StatusCode::OK,
                body: json!({
                    "success": true,
                    "data": data,
                    "error_data": null,
                    "message": null,
                }),
            }
        }

        fn accepted_success() -> Self {
            Self {
                status: StatusCode::ACCEPTED,
                body: json!({
                    "success": true,
                    "data": null,
                    "error_data": null,
                    "message": null,
                }),
            }
        }

        fn api_error(message: &str) -> Self {
            Self {
                status: StatusCode::OK,
                body: json!({
                    "success": false,
                    "data": null,
                    "error_data": null,
                    "message": message,
                }),
            }
        }
    }

    #[derive(Clone)]
    struct TestState {
        replies: Arc<Mutex<VecDeque<MockReply>>>,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    struct TestServer {
        base_url: String,
        state: TestState,
        handle: tokio::task::JoinHandle<()>,
    }

    impl TestServer {
        async fn spawn(replies: Vec<MockReply>) -> Self {
            ensure_rustls_provider();

            let state = TestState {
                replies: Arc::new(Mutex::new(VecDeque::from(replies))),
                requests: Arc::new(Mutex::new(Vec::new())),
            };

            let app = Router::new()
                .fallback(any(test_handler))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("listener should bind");
            let addr = listener.local_addr().expect("local addr should exist");
            let handle = tokio::spawn(async move {
                axum::serve(listener, app).await.expect("server should run");
            });

            Self {
                base_url: format!("http://{addr}"),
                state,
                handle,
            }
        }

        fn client(&self, password: Option<String>) -> KanbanApiClient {
            KanbanApiClient {
                client: reqwest::Client::new(),
                base_url: self.base_url.clone(),
                password,
            }
        }

        fn requests(&self) -> Vec<RecordedRequest> {
            self.state.requests.lock().unwrap().clone()
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    fn ensure_rustls_provider() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        });
    }

    async fn test_handler(State(state): State<TestState>, request: Request) -> Response {
        let method = request.method().as_str().to_string();
        let path = request.uri().path().to_string();
        let query = request.uri().query().map(ToString::to_string);
        let authorization = request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let body = if body.is_empty() {
            None
        } else {
            Some(serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| {
                Value::String(String::from_utf8(body.to_vec()).expect("body should be UTF-8"))
            }))
        };

        state.requests.lock().unwrap().push(RecordedRequest {
            method,
            path,
            query,
            authorization,
            body,
        });

        let reply = state
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("missing mock reply");

        (reply.status, axum::Json(reply.body)).into_response()
    }

    fn connection_args(server_url: &str, json: bool) -> ConnectionArgs {
        ConnectionArgs {
            server_url: Some(server_url.to_string()),
            json,
            password: None,
        }
    }

    fn project_json(name: &str) -> Value {
        json!({
            "id": Uuid::new_v4(),
            "name": name,
            "default_agent_working_dir": null,
            "created_at": "2026-03-21T00:00:00Z",
            "updated_at": "2026-03-21T00:00:00Z",
        })
    }

    fn task_project_id() -> Uuid {
        Uuid::new_v4()
    }

    fn task_json(project_id: Uuid, title: &str, description: Option<&str>, status: &str) -> Value {
        json!({
            "id": Uuid::new_v4(),
            "project_id": project_id,
            "title": title,
            "description": description,
            "status": status,
            "parent_workspace_id": null,
            "source_cron_task_id": null,
            "diff_additions": null,
            "diff_deletions": null,
            "created_at": "2026-03-21T00:00:00Z",
            "updated_at": "2026-03-21T00:00:00Z",
        })
    }

    fn task_with_status_json(
        project_id: &str,
        title: &str,
        description: Option<&str>,
        status: &str,
    ) -> Value {
        json!({
            "id": Uuid::new_v4(),
            "project_id": project_id,
            "title": title,
            "description": description,
            "status": status,
            "parent_workspace_id": null,
            "source_cron_task_id": null,
            "diff_additions": null,
            "diff_deletions": null,
            "created_at": "2026-03-21T00:00:00Z",
            "updated_at": "2026-03-21T00:00:00Z",
            "has_in_progress_attempt": false,
            "last_attempt_failed": false,
            "executor": "codex",
        })
    }

    fn deserialize_task_with_status(value: Value) -> TaskWithAttemptStatus {
        serde_json::from_value(value).expect("task with status should deserialize")
    }
}
