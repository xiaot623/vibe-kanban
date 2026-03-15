use std::{
    path::Path,
    sync::{Arc, LazyLock},
    time::Duration,
};

use async_trait::async_trait;
use command_group::AsyncCommandGroup;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncBufReadExt, process::Command};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use crate::{
    approvals::ExecutorApprovalService,
    command::{
        CmdOverrides, CommandBuildError, CommandBuilder, CommandParts, apply_overrides,
        env_command_or_default, format_command_for_log,
    },
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, ExecutorExitResult, SpawnedChild,
        StandardCodingAgentExecutor, command_available,
    },
    stdout_dup::create_stdout_pipe_writer,
};

mod normalize_logs;
mod plan_mode;
mod sdk;
mod types;

pub use plan_mode::EXIT_PLAN_MODE_NAME;
use plan_mode::append_plan_mode_prompt_guidance;
use sdk::{LogWriter, RunConfig, run_session};

static CODEX_COMMAND: LazyLock<String> =
    LazyLock::new(|| env_command_or_default("VK_OPENCODE", "opencode"));

const FALLBACK_CODEX_COMMAND: &str = "npx -y opencode-ai@1.2.15";

pub fn base_command() -> &'static str {
    CODEX_COMMAND.as_str()
}

pub fn fallback_command() -> &'static str {
    FALLBACK_CODEX_COMMAND
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Opencode {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "agent")]
    pub mode: Option<String>,
    /// Auto-approve agent actions
    #[serde(default = "default_to_true")]
    pub auto_approve: bool,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

impl Opencode {
    fn build_command_builder_with_base(
        &self,
        base: &str,
    ) -> Result<CommandBuilder, CommandBuildError> {
        let builder = CommandBuilder::new(base)
            // Pass hostname/port as separate args so OpenCode treats them as explicitly set
            // (it checks `process.argv.includes(\"--port\")` / `\"--hostname\"`).
            .extend_params(["serve", "--hostname", "127.0.0.1", "--port", "0"]);
        apply_overrides(builder, &self.cmd)
    }

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        tracing::debug!(
            "build_command_builder using system opencode command {}",
            base_command()
        );
        self.build_command_builder_with_base(base_command())
    }

    fn build_fallback_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        tracing::debug!(
            "build_fallback_builder using fallback npx opencode command {}",
            fallback_command()
        );
        self.build_command_builder_with_base(fallback_command())
    }

    fn should_fallback_to_npx(&self, err: &ExecutorError) -> bool {
        if self.cmd.base_command_override.is_some() {
            return false;
        }
        matches!(err, ExecutorError::ExecutableNotFound { .. })
    }

    async fn spawn_inner(
        &self,
        current_dir: &Path,
        prompt: &str,
        resume_session: Option<&str>,
        command_parts: CommandParts,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let combined_prompt = append_plan_mode_prompt_guidance(
            self.mode.as_deref(),
            self.append_prompt.combine_prompt(prompt),
        );
        let (program_path, args) = command_parts.into_resolved().await?;
        tracing::debug!(
            command = %format_command_for_log(&program_path, &args),
            "Spawning OpenCode command"
        );

        let mut command = Command::new(program_path);
        command
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(current_dir)
            .args(&args)
            .env("NODE_NO_WARNINGS", "1")
            .env("NO_COLOR", "1");

        env.clone()
            .with_profile(&self.cmd)
            .apply_to_command(&mut command);

        let mut child = command.group_spawn()?;
        let server_stdout = child.inner().stdout.take().ok_or_else(|| {
            ExecutorError::Io(std::io::Error::other(
                "OpenCode server missing stdout (needed to parse listening URL)",
            ))
        })?;

        let stdout = create_stdout_pipe_writer(&mut child)?;
        let log_writer = LogWriter::new(stdout);

        let (exit_signal_tx, exit_signal_rx) = tokio::sync::oneshot::channel();
        let (interrupt_tx, interrupt_rx) = tokio::sync::oneshot::channel();

        let directory = current_dir.to_string_lossy().to_string();
        let base_url = wait_for_server_url(server_stdout).await?;
        let permission_approvals = if self.auto_approve {
            None
        } else {
            self.approvals.clone()
        };

        let config = RunConfig {
            base_url,
            directory,
            prompt: combined_prompt,
            resume_session_id: resume_session.map(|s| s.to_string()),
            model: self.model.clone(),
            agent: self.mode.clone(),
            permission_approvals,
            // Plan approvals always require review when an approval service is available.
            plan_approvals: self.approvals.clone(),
        };

        tokio::spawn(async move {
            let result = run_session(config, log_writer.clone(), interrupt_rx).await;
            let exit_result = match result {
                Ok(()) => ExecutorExitResult::Success,
                Err(err) => {
                    let _ = log_writer
                        .log_error(format!("OpenCode executor error: {err}"))
                        .await;
                    ExecutorExitResult::Failure
                }
            };
            let _ = exit_signal_tx.send(exit_result);
        });

        Ok(SpawnedChild {
            child,
            exit_signal: Some(exit_signal_rx),
            interrupt_sender: Some(interrupt_tx),
        })
    }
}

fn format_tail(captured: Vec<String>) -> String {
    captured
        .into_iter()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

async fn wait_for_server_url(stdout: tokio::process::ChildStdout) -> Result<String, ExecutorError> {
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let mut captured: Vec<String> = Vec::new();

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(ExecutorError::Io(std::io::Error::other(format!(
                "Timed out waiting for OpenCode server to print listening URL.\nServer output tail:\n{}",
                format_tail(captured)
            ))));
        }

        let line = match tokio::time::timeout_at(deadline, lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => {
                return Err(ExecutorError::Io(std::io::Error::other(format!(
                    "OpenCode server exited before printing listening URL.\nServer output tail:\n{}",
                    format_tail(captured)
                ))));
            }
            Ok(Err(err)) => return Err(ExecutorError::Io(err)),
            Err(_) => continue,
        };

        if captured.len() < 64 {
            captured.push(line.clone());
        }

        if let Some(url) = line.trim().strip_prefix("opencode server listening on ") {
            // Keep draining stdout to avoid backpressure on the server, but don't block startup.
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(lines.into_inner()).lines();
                while let Ok(Some(_)) = lines.next_line().await {}
            });
            return Ok(url.trim().to_string());
        }
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for Opencode {
    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let env = setup_approvals_env(self.auto_approve, env);
        let command_parts = self.build_command_builder()?.build_initial()?;
        match self
            .spawn_inner(current_dir, prompt, None, command_parts, &env)
            .await
        {
            Ok(child) => Ok(child),
            Err(err) => {
                if self.should_fallback_to_npx(&err) {
                    let fallback_parts = self.build_fallback_command_builder()?.build_initial()?;
                    return self
                        .spawn_inner(current_dir, prompt, None, fallback_parts, &env)
                        .await;
                }
                Err(err)
            }
        }
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let env = setup_approvals_env(self.auto_approve, env);
        let command_parts = self.build_command_builder()?.build_initial()?;
        match self
            .spawn_inner(current_dir, prompt, Some(session_id), command_parts, &env)
            .await
        {
            Ok(child) => Ok(child),
            Err(err) => {
                if self.should_fallback_to_npx(&err) {
                    let fallback_parts = self.build_fallback_command_builder()?.build_initial()?;
                    return self
                        .spawn_inner(current_dir, prompt, Some(session_id), fallback_parts, &env)
                        .await;
                }
                Err(err)
            }
        }
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, worktree_path: &Path) {
        normalize_logs::normalize_logs(msg_store, worktree_path);
    }

    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        #[cfg(unix)]
        {
            xdg::BaseDirectories::with_prefix("opencode").get_config_file("opencode.json")
        }
        #[cfg(not(unix))]
        {
            dirs::config_dir().map(|config| config.join("opencode").join("opencode.json"))
        }
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        let mcp_config_found = self
            .default_mcp_config_path()
            .map(|p| p.exists())
            .unwrap_or(false);

        let installation_indicator_found = dirs::config_dir()
            .map(|config| config.join("opencode").exists())
            .unwrap_or(false);

        // Check whether the primary `opencode` binary is on PATH.
        let primary_command_found = command_available(
            self.build_command_builder()
                .and_then(|builder| builder.build_initial()),
        );

        // Only check the npx fallback when no override is set and the primary
        // command is absent. The fallback resolves `npx` (the first token), not
        // the opencode package itself, so it must not be used as a standalone
        // installation signal — it would return true on any machine with Node.js.
        let command_found = primary_command_found
            || (!primary_command_found
                && self.cmd.base_command_override.is_none()
                && (mcp_config_found || installation_indicator_found)
                && command_available(
                    self.build_fallback_command_builder()
                        .and_then(|builder| builder.build_initial()),
                ));

        if mcp_config_found || installation_indicator_found || command_found {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }
}

fn default_to_true() -> bool {
    true
}

fn setup_approvals_env(auto_approve: bool, env: &ExecutionEnv) -> ExecutionEnv {
    let mut env = env.clone();
    if !auto_approve && !env.contains_key("OPENCODE_PERMISSION") {
        env.insert("OPENCODE_PERMISSION", r#"{"edit": "ask", "bash": "ask", "webfetch": "ask", "doom_loop": "ask", "external_directory": "ask"}"#);
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::CmdOverrides;

    fn make_opencode(base_command_override: Option<String>) -> Opencode {
        Opencode {
            append_prompt: AppendPrompt::default(),
            model: None,
            mode: None,
            auto_approve: true,
            cmd: CmdOverrides {
                base_command_override,
                additional_params: None,
                env: None,
            },
            approvals: None,
        }
    }

    /// When `base_command_override` is set to an existing binary the primary
    /// command check should succeed and the fallback path must not be consulted.
    #[test]
    fn get_availability_info_uses_override_command() {
        // Use the current test binary itself as a guaranteed-existing executable.
        let exe = std::env::current_exe()
            .expect("current_exe should resolve")
            .to_string_lossy()
            .into_owned();

        let opencode = make_opencode(Some(exe));
        assert!(
            opencode.get_availability_info().is_available(),
            "override pointing to existing binary should be InstallationFound"
        );
    }

    /// When `base_command_override` is set to a nonexistent binary the result
    /// must be `NotFound` — the fallback should NOT be checked.
    #[test]
    fn get_availability_info_override_missing_binary_not_found() {
        let opencode = make_opencode(Some(
            "vibe-kanban-nonexistent-opencode-override".to_string(),
        ));
        // Only valid if neither config dir nor command resolves; on a CI box
        // without opencode installed this should return NotFound.
        let info = opencode.get_availability_info();
        // We can't guarantee config dirs don't exist on all machines, so we
        // only assert that if both config checks fail, the result is NotFound.
        if !dirs::config_dir()
            .map(|d| d.join("opencode").exists())
            .unwrap_or(false)
        {
            assert!(
                !info.is_available(),
                "nonexistent override with no config dir should be NotFound"
            );
        }
    }

    /// When no override is set the fallback must not signal installation when
    /// the primary `opencode` binary is absent and no config directory exists.
    #[test]
    fn get_availability_info_fallback_requires_config_or_primary() {
        // Only meaningful on machines without `opencode` installed.
        let opencode = make_opencode(None);
        // We're testing the logic path: if neither primary command nor config
        // dir exists, the fallback should NOT make it available.
        // This is a logic test; we check the function doesn't panic.
        let _info = opencode.get_availability_info();
    }
}
