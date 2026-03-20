use std::ffi::OsString;

use clap::{
    builder::Styles, error::ErrorKind, Args, Command, CommandFactory, Parser, Subcommand, ValueEnum,
};
use db::models::project_repo::CreateProjectRepo;

const BINARY_NAME: &str = "kanban";
pub const KANBAN_SERVER_URL_ENV: &str = "KANBAN_SERVER_URL";
pub const KANBAN_PASSWORD_ENV: &str = "KANBAN_PASSWORD";

#[derive(Debug, Clone, Parser)]
#[command(
    name = BINARY_NAME,
    bin_name = BINARY_NAME,
    disable_help_subcommand = true,
    styles = plain_styles()
)]
pub struct CliArgs {
    #[command(subcommand)]
    pub command: Option<CliCommand>,
}

impl CliArgs {
    pub fn parse_env() -> Result<Self, CliError> {
        Self::parse_from(std::env::args_os().skip(1))
    }

    pub fn parse_from<I, S>(raw_args: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let filtered_args = filtered_args(raw_args);
        let argv = std::iter::once(OsString::from(BINARY_NAME))
            .chain(filtered_args.iter().cloned())
            .collect::<Vec<_>>();

        Self::try_parse_from(argv).map_err(|err| CliError::from_clap(err, &filtered_args))
    }

    pub fn render_help(target: HelpTarget) -> String {
        let mut command = Self::command();
        let mut selected = target.select(&mut command).clone();
        let mut buffer = Vec::new();
        selected
            .write_long_help(&mut buffer)
            .expect("help rendering should succeed");
        String::from_utf8(buffer).expect("help should be valid UTF-8")
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum CliCommand {
    /// Start the backend server in headless mode.
    Server(ServerArgs),
    /// Show command help.
    Help(HelpArgs),
    /// Manage projects through the HTTP API.
    Project(ProjectArgs),
    /// Manage tasks through the HTTP API.
    Task(TaskArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ServerArgs {
    #[arg(long, value_name = "u16")]
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Args)]
pub struct HelpArgs {
    pub topic: Option<HelpTopic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HelpTopic {
    Server,
    Project,
    Task,
}

#[derive(Debug, Clone, Args)]
pub struct ProjectArgs {
    #[command(subcommand)]
    pub command: ProjectCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ProjectCommand {
    List(ProjectListArgs),
    Get(ProjectGetArgs),
    Add(ProjectAddArgs),
    Modify(ProjectModifyArgs),
    Delete(ProjectDeleteArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ProjectListArgs {
    #[command(flatten)]
    pub connection: ConnectionArgs,
}

#[derive(Debug, Clone, Args)]
pub struct ProjectGetArgs {
    pub selector: String,
    #[command(flatten)]
    pub connection: ConnectionArgs,
}

#[derive(Debug, Clone, Args)]
pub struct ProjectAddArgs {
    #[arg(long)]
    pub name: String,
    #[arg(long = "repo", value_parser = parse_repo_argument)]
    pub repositories: Vec<CreateProjectRepo>,
    #[command(flatten)]
    pub connection: ConnectionArgs,
}

#[derive(Debug, Clone, Args)]
pub struct ProjectModifyArgs {
    pub selector: String,
    #[arg(long)]
    pub name: String,
    #[command(flatten)]
    pub connection: ConnectionArgs,
}

#[derive(Debug, Clone, Args)]
pub struct ProjectDeleteArgs {
    pub selector: String,
    #[command(flatten)]
    pub connection: ConnectionArgs,
}

#[derive(Debug, Clone, Args)]
pub struct TaskArgs {
    #[command(subcommand)]
    pub command: TaskCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum TaskCommand {
    List(TaskListArgs),
    Get(TaskGetArgs),
    Add(TaskAddArgs),
    Modify(TaskModifyArgs),
    Delete(TaskDeleteArgs),
    Query(TaskQueryArgs),
}

#[derive(Debug, Clone, Args)]
pub struct TaskListArgs {
    #[arg(long)]
    pub project: String,
    #[command(flatten)]
    pub connection: ConnectionArgs,
}

#[derive(Debug, Clone, Args)]
pub struct TaskGetArgs {
    pub task_id: String,
    #[command(flatten)]
    pub connection: ConnectionArgs,
}

#[derive(Debug, Clone, Args)]
pub struct TaskAddArgs {
    #[arg(long)]
    pub project: String,
    #[arg(long)]
    pub title: String,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long, value_enum)]
    pub status: Option<TaskStatusArg>,
    #[command(flatten)]
    pub connection: ConnectionArgs,
}

#[derive(Debug, Clone, Args)]
#[command(group(
    clap::ArgGroup::new("description_mode")
        .args(["description", "clear_description"])
        .multiple(false)
))]
pub struct TaskModifyArgs {
    pub task_id: String,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long)]
    pub clear_description: bool,
    #[arg(long, value_enum)]
    pub status: Option<TaskStatusArg>,
    #[command(flatten)]
    pub connection: ConnectionArgs,
}

#[derive(Debug, Clone, Args)]
pub struct TaskDeleteArgs {
    pub task_id: String,
    #[command(flatten)]
    pub connection: ConnectionArgs,
}

#[derive(Debug, Clone, Args)]
pub struct TaskQueryArgs {
    #[arg(long)]
    pub project: String,
    #[arg(long)]
    pub q: String,
    #[arg(long, value_enum)]
    pub status: Option<TaskStatusArg>,
    #[command(flatten)]
    pub connection: ConnectionArgs,
}

#[derive(Debug, Clone, Args)]
pub struct ConnectionArgs {
    /// Optional server URL. If omitted, the CLI discovers the local server from
    /// KANBAN_SERVER_URL, the port file, BACKEND_PORT, or PORT.
    #[arg(long, env = KANBAN_SERVER_URL_ENV, value_name = "url")]
    pub server_url: Option<String>,
    /// Print API results as JSON.
    #[arg(long)]
    pub json: bool,
    /// Optional Basic auth password. Only needed when talking to a LAN server
    /// with local network access enabled.
    #[arg(long, env = KANBAN_PASSWORD_ENV, value_name = "password")]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TaskStatusArg {
    Todo,
    Inprogress,
    Inreview,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTarget {
    Root,
    Server,
    Project,
    ProjectList,
    ProjectGet,
    ProjectAdd,
    ProjectModify,
    ProjectDelete,
    Task,
    TaskList,
    TaskGet,
    TaskAdd,
    TaskModify,
    TaskDelete,
    TaskQuery,
}

impl HelpTarget {
    fn select(self, command: &mut Command) -> &mut Command {
        match self {
            Self::Root => command,
            Self::Server => command
                .find_subcommand_mut("server")
                .expect("server help should exist"),
            Self::Project => command
                .find_subcommand_mut("project")
                .expect("project help should exist"),
            Self::ProjectList => command
                .find_subcommand_mut("project")
                .and_then(|cmd| cmd.find_subcommand_mut("list"))
                .expect("project list help should exist"),
            Self::ProjectGet => command
                .find_subcommand_mut("project")
                .and_then(|cmd| cmd.find_subcommand_mut("get"))
                .expect("project get help should exist"),
            Self::ProjectAdd => command
                .find_subcommand_mut("project")
                .and_then(|cmd| cmd.find_subcommand_mut("add"))
                .expect("project add help should exist"),
            Self::ProjectModify => command
                .find_subcommand_mut("project")
                .and_then(|cmd| cmd.find_subcommand_mut("modify"))
                .expect("project modify help should exist"),
            Self::ProjectDelete => command
                .find_subcommand_mut("project")
                .and_then(|cmd| cmd.find_subcommand_mut("delete"))
                .expect("project delete help should exist"),
            Self::Task => command
                .find_subcommand_mut("task")
                .expect("task help should exist"),
            Self::TaskList => command
                .find_subcommand_mut("task")
                .and_then(|cmd| cmd.find_subcommand_mut("list"))
                .expect("task list help should exist"),
            Self::TaskGet => command
                .find_subcommand_mut("task")
                .and_then(|cmd| cmd.find_subcommand_mut("get"))
                .expect("task get help should exist"),
            Self::TaskAdd => command
                .find_subcommand_mut("task")
                .and_then(|cmd| cmd.find_subcommand_mut("add"))
                .expect("task add help should exist"),
            Self::TaskModify => command
                .find_subcommand_mut("task")
                .and_then(|cmd| cmd.find_subcommand_mut("modify"))
                .expect("task modify help should exist"),
            Self::TaskDelete => command
                .find_subcommand_mut("task")
                .and_then(|cmd| cmd.find_subcommand_mut("delete"))
                .expect("task delete help should exist"),
            Self::TaskQuery => command
                .find_subcommand_mut("task")
                .and_then(|cmd| cmd.find_subcommand_mut("query"))
                .expect("task query help should exist"),
        }
    }

    fn from_tokens(tokens: &[OsString]) -> Self {
        let normalized = tokens
            .iter()
            .map(|token| token.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        match normalized.first().map(String::as_str) {
            Some("help") => match normalized.get(1).map(String::as_str) {
                Some("server") => Self::Server,
                Some("project") => Self::Project,
                Some("task") => Self::Task,
                _ => Self::Root,
            },
            Some("server") => Self::Server,
            Some("project") => match normalized.get(1).map(String::as_str) {
                Some("list") => Self::ProjectList,
                Some("get") => Self::ProjectGet,
                Some("add") => Self::ProjectAdd,
                Some("modify") => Self::ProjectModify,
                Some("delete") => Self::ProjectDelete,
                _ => Self::Project,
            },
            Some("task") => match normalized.get(1).map(String::as_str) {
                Some("list") => Self::TaskList,
                Some("get") => Self::TaskGet,
                Some("add") => Self::TaskAdd,
                Some("modify") => Self::TaskModify,
                Some("delete") => Self::TaskDelete,
                Some("query") => Self::TaskQuery,
                _ => Self::Task,
            },
            _ => Self::Root,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CliError {
    output: String,
    exit_code: i32,
    use_stderr: bool,
}

impl CliError {
    fn from_clap(error: clap::Error, raw_args: &[OsString]) -> Self {
        let is_help = matches!(
            error.kind(),
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
        );
        let mut output = error.to_string();

        if !is_help {
            let help = CliArgs::render_help(HelpTarget::from_tokens(raw_args));
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push('\n');
            output.push_str(&help);
        }

        Self {
            output,
            exit_code: error.exit_code(),
            use_stderr: !is_help,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub fn print(&self) {
        if self.use_stderr {
            eprint!("{}", self.output);
        } else {
            print!("{}", self.output);
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.output)
    }
}

impl std::error::Error for CliError {}

fn filtered_args<I, S>(raw_args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    raw_args
        .into_iter()
        .map(Into::into)
        .filter(|arg| !arg.to_string_lossy().starts_with("-psn_"))
        .collect()
}

fn parse_repo_argument(raw: &str) -> Result<CreateProjectRepo, String> {
    let (display_name, git_repo_path) = raw
        .split_once('=')
        .ok_or_else(|| "expected <display_name>=<git_repo_path>".to_string())?;

    if display_name.trim().is_empty() {
        return Err("repository display name cannot be empty".to_string());
    }

    if git_repo_path.trim().is_empty() {
        return Err("repository path cannot be empty".to_string());
    }

    Ok(CreateProjectRepo {
        display_name: display_name.to_string(),
        git_repo_path: git_repo_path.to_string(),
    })
}

const fn plain_styles() -> Styles {
    Styles::plain()
}

#[cfg(test)]
mod tests {
    use super::{CliArgs, CliCommand, HelpTarget, ProjectCommand, TaskCommand, TaskStatusArg};

    #[test]
    fn keeps_desktop_launch_when_no_args() {
        let parsed = CliArgs::parse_from([] as [&str; 0]).expect("expected parse success");
        assert!(parsed.command.is_none());
    }

    #[test]
    fn parses_server_subcommand() {
        let parsed = CliArgs::parse_from(["server"]).expect("expected parse success");
        match parsed.command {
            Some(CliCommand::Server(args)) => assert_eq!(args.port, None),
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn parses_server_subcommand_with_port() {
        let parsed =
            CliArgs::parse_from(["server", "--port", "8080"]).expect("expected parse success");
        match parsed.command {
            Some(CliCommand::Server(args)) => assert_eq!(args.port, Some(8080)),
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn parses_help_subcommand() {
        let parsed = CliArgs::parse_from(["help", "task"]).expect("expected parse success");
        match parsed.command {
            Some(CliCommand::Help(args)) => {
                assert_eq!(args.topic, Some(super::HelpTopic::Task));
            }
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn parses_project_add_with_multiple_repositories() {
        let parsed = CliArgs::parse_from([
            "project",
            "add",
            "--name",
            "Demo",
            "--repo",
            "frontend=/tmp/frontend",
            "--repo",
            "backend=/tmp/backend",
        ])
        .expect("expected parse success");

        match parsed.command {
            Some(CliCommand::Project(project)) => match project.command {
                ProjectCommand::Add(args) => {
                    assert_eq!(args.name, "Demo");
                    assert_eq!(args.repositories.len(), 2);
                    assert_eq!(args.repositories[0].display_name, "frontend");
                    assert_eq!(args.repositories[0].git_repo_path, "/tmp/frontend");
                    assert_eq!(args.repositories[1].display_name, "backend");
                    assert_eq!(args.repositories[1].git_repo_path, "/tmp/backend");
                }
                other => panic!("unexpected project command: {other:?}"),
            },
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn parses_task_modify_clear_description() {
        let parsed = CliArgs::parse_from([
            "task",
            "modify",
            "task-1",
            "--clear-description",
            "--status",
            "done",
        ])
        .expect("expected parse success");

        match parsed.command {
            Some(CliCommand::Task(task)) => match task.command {
                TaskCommand::Modify(args) => {
                    assert_eq!(args.task_id, "task-1");
                    assert!(args.clear_description);
                    assert_eq!(args.status, Some(TaskStatusArg::Done));
                }
                other => panic!("unexpected task command: {other:?}"),
            },
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn rejects_legacy_server_flag_with_help() {
        let err = CliArgs::parse_from(["--server"]).expect_err("expected parse failure");
        assert_ne!(err.exit_code(), 0);
        let output = err.to_string();
        assert!(output.contains("--server"));
        assert!(output.contains("Usage: kanban"));
    }

    #[test]
    fn rejects_unknown_subcommand_with_contextual_help() {
        let err = CliArgs::parse_from(["task", "unknown"]).expect_err("expected parse failure");
        assert_ne!(err.exit_code(), 0);
        let output = err.to_string();
        assert!(output.contains("task"));
        assert!(output.contains("Usage: kanban task"));
    }

    #[test]
    fn rejects_missing_required_arguments_with_contextual_help() {
        let err = CliArgs::parse_from(["task", "list"]).expect_err("expected parse failure");
        assert_ne!(err.exit_code(), 0);
        let output = err.to_string();
        assert!(output.contains("--project"));
        assert!(output.contains("Usage: kanban task list"));
    }

    #[test]
    fn ignores_macos_finder_psn_argument() {
        let parsed =
            CliArgs::parse_from(["-psn_0_12345", "server"]).expect("expected parse success");
        match parsed.command {
            Some(CliCommand::Server(args)) => assert_eq!(args.port, None),
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn renders_explicit_help_targets() {
        let help = CliArgs::render_help(HelpTarget::Task);
        assert!(help.contains("Manage tasks through the HTTP API"));
        assert!(help.contains("list"));
        assert!(help.contains("query"));
    }
}
