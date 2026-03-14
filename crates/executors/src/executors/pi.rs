pub mod normalize_logs;
pub mod types;

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use command_group::AsyncCommandGroup;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    process::{ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use ts_rs::TS;
use workspace_utils::{
    approvals::{APPROVAL_TIMEOUT_SECONDS, ApprovalStatus},
    msg_store::MsgStore,
};

use self::{
    normalize_logs::normalize_logs,
    types::{
        ExtensionUiRequest, PiExecutorEvent, PiRpcMessage,
        approval_status_to_extension_ui_response, extract_agent_end_error,
        extract_session_file_from_state, parse_extension_ui_request, parse_rpc_message,
    },
};
use crate::{
    approvals::{ExecutorApprovalError, ExecutorApprovalService},
    command::{
        CmdOverrides, CommandBuildError, CommandBuilder, CommandParts, apply_overrides,
        env_command_or_default, format_command_for_log,
    },
    env::ExecutionEnv,
    executors::{
        AppendPrompt, ExecutorError, ExecutorExitResult, SpawnedChild, StandardCodingAgentExecutor,
    },
    stdout_dup::create_stdout_pipe_writer,
};

static PI_COMMAND: LazyLock<String> = LazyLock::new(|| env_command_or_default("VK_PI", "pi"));

pub fn base_command() -> &'static str {
    PI_COMMAND.as_str()
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Pi {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approve: Option<bool>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,

    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PiMode {
    Default,
    Approve,
}

impl Pi {
    fn mode_from_flags(approve: bool) -> PiMode {
        if approve {
            PiMode::Approve
        } else {
            PiMode::Default
        }
    }

    fn mode(&self) -> PiMode {
        Self::mode_from_flags(self.approve.unwrap_or(false))
    }

    fn build_command_builder_with_base(
        &self,
        base: &str,
    ) -> Result<CommandBuilder, CommandBuildError> {
        let mut builder = CommandBuilder::new(base).extend_params(["--mode", "rpc"]);

        if let Some(model) = &self.model {
            builder = builder.extend_params(["--model", model.as_str()]);
        }
        if let Some(thinking) = &self.thinking {
            builder = builder.extend_params(["--thinking", thinking.as_str()]);
        }

        match self.mode() {
            PiMode::Approve => {
                builder = builder.extend_params(["--extension-policy", "balanced"]);
            }
            PiMode::Default => {}
        }

        apply_overrides(builder, &self.cmd)
    }

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        tracing::info!(
            "build_command_builder using system pi command {}",
            base_command()
        );
        self.build_command_builder_with_base(base_command())
    }

    async fn spawn_inner(
        &self,
        current_dir: &Path,
        prompt: &str,
        resume_session_file: Option<String>,
        command_parts: CommandParts,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let (program_path, args) = command_parts.into_resolved().await?;
        tracing::debug!(
            command = %format_command_for_log(&program_path, &args),
            "Spawning Pi command"
        );

        let mut command = Command::new(program_path);
        command
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(current_dir)
            .args(&args)
            .env("NO_COLOR", "1");

        env.clone()
            .with_profile(&self.cmd)
            .apply_to_command(&mut command);

        let mut child = command.group_spawn()?;
        let child_stdout = child.inner().stdout.take().ok_or_else(|| {
            ExecutorError::Io(io::Error::other("Pi process missing stdout for RPC"))
        })?;
        let child_stdin = child.inner().stdin.take().ok_or_else(|| {
            ExecutorError::Io(io::Error::other("Pi process missing stdin for RPC"))
        })?;

        let stdout = create_stdout_pipe_writer(&mut child)?;
        let log_writer = PiLogWriter::new(stdout);

        let (exit_signal_tx, exit_signal_rx) = tokio::sync::oneshot::channel();
        let mode = self.mode();
        let approvals = if matches!(mode, PiMode::Approve) {
            self.approvals.clone()
        } else {
            None
        };
        let model = self.model.clone();
        let reasoning_effort = self.thinking.clone();

        tokio::spawn(async move {
            let run_result = run_pi_rpc_session(
                child_stdin,
                child_stdout,
                combined_prompt,
                resume_session_file,
                mode,
                approvals,
                model,
                reasoning_effort,
                log_writer.clone(),
            )
            .await;

            let exit_result = match run_result {
                Ok(result) => result,
                Err(err) => {
                    let _ = log_writer
                        .log_event(&PiExecutorEvent::ProtocolError {
                            message: format!("Pi executor error: {err}"),
                        })
                        .await;
                    ExecutorExitResult::Failure
                }
            };

            let _ = exit_signal_tx.send(exit_result);
        });

        Ok(SpawnedChild {
            child,
            exit_signal: Some(exit_signal_rx),
            interrupt_sender: None,
        })
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for Pi {
    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let command_parts = self.build_command_builder()?.build_initial()?;
        self.spawn_inner(current_dir, prompt, None, command_parts, env)
            .await
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let command_parts = self.build_command_builder()?.build_initial()?;
        self.spawn_inner(
            current_dir,
            prompt,
            Some(session_id.to_string()),
            command_parts,
            env,
        )
        .await
    }

    fn normalize_logs(
        &self,
        msg_store: Arc<MsgStore>,
        worktree_path: &Path,
    ) {
        normalize_logs(msg_store, worktree_path);
    }

    fn default_mcp_config_path(&self) -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(".pi").join("config.json"))
    }
}

#[derive(Clone)]
struct PiLogWriter {
    writer: Arc<Mutex<BufWriter<Box<dyn AsyncWrite + Send + Unpin>>>>,
}

impl PiLogWriter {
    fn new(writer: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Self {
            writer: Arc::new(Mutex::new(BufWriter::new(Box::new(writer)))),
        }
    }

    async fn log_event(&self, event: &PiExecutorEvent) -> Result<(), ExecutorError> {
        let raw =
            serde_json::to_string(event).map_err(|err| ExecutorError::Io(io::Error::other(err)))?;
        let mut writer = self.writer.lock().await;
        writer
            .write_all(raw.as_bytes())
            .await
            .map_err(ExecutorError::Io)?;
        writer.write_all(b"\n").await.map_err(ExecutorError::Io)?;
        writer.flush().await.map_err(ExecutorError::Io)?;
        Ok(())
    }
}

struct PiRpcSession {
    stdin: BufWriter<ChildStdin>,
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
}

impl PiRpcSession {
    fn new(stdin: ChildStdin, stdout: ChildStdout) -> Self {
        Self {
            stdin: BufWriter::new(stdin),
            lines: BufReader::new(stdout).lines(),
        }
    }

    async fn send_command(&mut self, command: &str, params: Value) -> Result<(), ExecutorError> {
        self.send_json(&build_pi_command(command, params)).await
    }

    async fn call(
        &mut self,
        method: &str,
        params: Value,
        context: &mut PiRunContext,
    ) -> Result<Value, ExecutorError> {
        let request_id = method.to_string();
        self.send_command(method, params).await?;

        loop {
            let Some(message) = self.next_message().await? else {
                return Err(ExecutorError::Io(io::Error::other(format!(
                    "Pi RPC closed while waiting for `{method}` response"
                ))));
            };

            match message {
                PiRpcMessage::Response { id, result } if id == request_id => return Ok(result),
                PiRpcMessage::ErrorResponse { id, message, .. }
                    if id.as_deref() == Some(request_id.as_str()) || id.is_none() =>
                {
                    return Err(ExecutorError::Io(io::Error::other(format!(
                        "Pi `{method}` request failed: {message}"
                    ))));
                }
                other => {
                    let _ = context.handle_message(self, other).await?;
                }
            }
        }
    }

    async fn next_message(&mut self) -> Result<Option<PiRpcMessage>, ExecutorError> {
        loop {
            let line = match self.lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => return Ok(None),
                Err(err) => return Err(ExecutorError::Io(err)),
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match parse_rpc_message(trimmed) {
                Ok(message) => return Ok(Some(message)),
                Err(_) => {
                    return Ok(Some(PiRpcMessage::Other(Value::String(
                        trimmed.to_string(),
                    ))));
                }
            }
        }
    }

    async fn send_json(&mut self, message: &Value) -> Result<(), ExecutorError> {
        let raw = serde_json::to_string(message)
            .map_err(|err| ExecutorError::Io(io::Error::other(err.to_string())))?;
        self.stdin
            .write_all(raw.as_bytes())
            .await
            .map_err(ExecutorError::Io)?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(ExecutorError::Io)?;
        self.stdin.flush().await.map_err(ExecutorError::Io)?;
        Ok(())
    }
}

fn build_pi_command(command: &str, params: Value) -> Value {
    let mut payload = serde_json::Map::new();
    match params {
        Value::Object(map) => payload.extend(map),
        Value::Null => {}
        value => {
            payload.insert("payload".to_string(), value);
        }
    }
    payload.insert("type".to_string(), Value::String(command.to_string()));
    payload
        .entry("id".to_string())
        .or_insert_with(|| Value::String(command.to_string()));
    Value::Object(payload)
}

struct PiRunContext {
    mode: PiMode,
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
    log_writer: PiLogWriter,
    agent_end_error: Option<String>,
}

impl PiRunContext {
    fn new(
        mode: PiMode,
        approvals: Option<Arc<dyn ExecutorApprovalService>>,
        log_writer: PiLogWriter,
    ) -> Self {
        Self {
            mode,
            approvals,
            log_writer,
            agent_end_error: None,
        }
    }

    async fn wait_for_agent_end(&mut self, rpc: &mut PiRpcSession) -> Result<(), ExecutorError> {
        loop {
            let Some(message) = rpc.next_message().await? else {
                return Err(ExecutorError::Io(io::Error::other(
                    "Pi RPC closed before `agent_end` was emitted",
                )));
            };
            if self.handle_message(rpc, message).await? {
                return Ok(());
            }
        }
    }

    async fn handle_message(
        &mut self,
        rpc: &mut PiRpcSession,
        message: PiRpcMessage,
    ) -> Result<bool, ExecutorError> {
        match message {
            PiRpcMessage::Notification {
                method,
                payload,
                raw,
            } => {
                self.log_writer
                    .log_event(&PiExecutorEvent::PiEvent {
                        method: method.clone(),
                        payload: payload.clone(),
                        raw,
                    })
                    .await?;

                if matches!(self.mode, PiMode::Approve)
                    && let Some(request) = parse_extension_ui_request(&method, &payload)
                {
                    self.handle_extension_ui_request(rpc, request).await?;
                }

                if method == "agent_end" {
                    self.agent_end_error = extract_agent_end_error(&payload);
                    return Ok(true);
                }
            }
            PiRpcMessage::ErrorResponse { message, raw, .. } => {
                self.log_writer
                    .log_event(&PiExecutorEvent::ProtocolError {
                        message: format!("Pi RPC error response: {message} ({raw})"),
                    })
                    .await?;
            }
            PiRpcMessage::Other(raw) => {
                self.log_writer
                    .log_event(&PiExecutorEvent::ProtocolError {
                        message: format!("Unrecognized Pi RPC output: {raw}"),
                    })
                    .await?;
            }
            PiRpcMessage::Response { .. } => {}
        }

        Ok(false)
    }

    async fn handle_extension_ui_request(
        &mut self,
        rpc: &mut PiRpcSession,
        request: ExtensionUiRequest,
    ) -> Result<(), ExecutorError> {
        let requested_at = Utc::now();
        let timeout_at = requested_at + Duration::seconds(APPROVAL_TIMEOUT_SECONDS);

        self.log_writer
            .log_event(&PiExecutorEvent::ApprovalPending {
                tool_call_id: request.tool_call_id.clone(),
                tool_name: request.tool_name.clone(),
                tool_input: request.payload.clone(),
                approval_id: request.tool_call_id.clone(),
                requested_at,
                timeout_at,
            })
            .await?;

        let status = request_ui_approval(self.approvals.clone(), &request).await;
        self.log_writer
            .log_event(&PiExecutorEvent::ApprovalResult {
                tool_call_id: request.tool_call_id.clone(),
                status: status.clone(),
            })
            .await?;

        let response = approval_status_to_extension_ui_response(&request, &status);
        let mut params = match response {
            Value::Object(map) => map,
            value => {
                let mut map = serde_json::Map::new();
                map.insert("value".to_string(), value);
                map
            }
        };
        params.insert(
            "requestId".to_string(),
            Value::String(request.request_id.clone()),
        );
        rpc.send_command("extension_ui_response", Value::Object(params))
            .await?;

        Ok(())
    }
}

async fn request_ui_approval(
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
    request: &ExtensionUiRequest,
) -> ApprovalStatus {
    let Some(approvals) = approvals else {
        return ApprovalStatus::Approved;
    };

    match approvals
        .request_tool_approval(
            &request.tool_name,
            request.payload.clone(),
            &request.tool_call_id,
        )
        .await
    {
        Ok(status) => status,
        Err(
            ExecutorApprovalError::ServiceUnavailable | ExecutorApprovalError::SessionNotRegistered,
        ) => ApprovalStatus::Approved,
        Err(err) => ApprovalStatus::Denied {
            reason: Some(format!("Approval request failed: {err}")),
        },
    }
}

async fn run_pi_rpc_session(
    stdin: ChildStdin,
    stdout: ChildStdout,
    prompt: String,
    resume_session_file: Option<String>,
    mode: PiMode,
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    log_writer: PiLogWriter,
) -> Result<ExecutorExitResult, ExecutorError> {
    let mut rpc = PiRpcSession::new(stdin, stdout);
    let mut context = PiRunContext::new(mode, approvals, log_writer.clone());

    if model.is_some() || reasoning_effort.is_some() {
        log_writer
            .log_event(&PiExecutorEvent::ModelMetadata {
                model,
                reasoning_effort,
            })
            .await?;
    }

    if let Some(session_file) = resume_session_file {
        rpc.call(
            "switch_session",
            json!({
                "sessionPath": session_file.clone(),
                "sessionFile": session_file.clone(),
                "session_file": session_file.clone(),
                "sessionId": session_file.clone(),
                "session_id": session_file,
            }),
            &mut context,
        )
        .await?;
    }

    rpc.call("prompt", json!({ "message": prompt }), &mut context)
        .await?;
    context.wait_for_agent_end(&mut rpc).await?;

    let state = rpc.call("get_state", json!({}), &mut context).await?;
    if let Some(session_id) = extract_session_file_from_state(&state) {
        log_writer
            .log_event(&PiExecutorEvent::SessionStart { session_id })
            .await?;
    }

    log_writer.log_event(&PiExecutorEvent::Done).await?;
    if context.agent_end_error.is_some() {
        Ok(ExecutorExitResult::Failure)
    } else {
        Ok(ExecutorExitResult::Success)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pi_with_mode(approve: Option<bool>) -> Pi {
        Pi {
            append_prompt: AppendPrompt::default(),
            model: None,
            thinking: None,
            approve,
            cmd: CmdOverrides::default(),
            approvals: None,
        }
    }

    fn command_params(executor: &Pi) -> Vec<String> {
        executor
            .build_command_builder_with_base("pi")
            .expect("command builder should be created")
            .params
            .unwrap_or_default()
    }

    fn has_arg_pair(args: &[String], first: &str, second: &str) -> bool {
        args.windows(2)
            .any(|window| window[0] == first && window[1] == second)
    }

    #[test]
    fn default_mode_has_no_approve_flags() {
        let params = command_params(&pi_with_mode(Some(false)));
        assert!(!has_arg_pair(&params, "--extension-policy", "balanced"));
        assert!(!params.iter().any(|arg| arg == "--no-tools"));
    }

    #[test]
    fn approve_mode_adds_extension_policy_flag() {
        let params = command_params(&pi_with_mode(Some(true)));
        assert!(has_arg_pair(&params, "--extension-policy", "balanced"));
        assert!(!params.iter().any(|arg| arg == "--no-tools"));
    }

    #[test]
    fn build_pi_command_includes_default_request_id() {
        let command = build_pi_command("get_state", json!({}));
        assert_eq!(
            command.pointer("/id").and_then(Value::as_str),
            Some("get_state")
        );
    }

    #[test]
    fn build_pi_command_keeps_existing_request_id() {
        let command = build_pi_command(
            "extension_ui_response",
            json!({
                "id": "req-1",
                "requestId": "req-1",
                "confirmed": true
            }),
        );
        assert_eq!(
            command.pointer("/id").and_then(Value::as_str),
            Some("req-1")
        );
    }
}
