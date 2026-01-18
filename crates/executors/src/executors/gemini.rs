use std::{
    path::Path,
    sync::{Arc, LazyLock},
};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

pub use super::acp::AcpAgentHarness;
use crate::{
    approvals::ExecutorApprovalService,
    command::{
        CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides, env_command_or_default,
    },
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, SpawnedChild, StandardCodingAgentExecutor,
    },
};

static GEMINI_COMMAND: LazyLock<String> =
    LazyLock::new(|| env_command_or_default("VK_GEMINI", "gemini"));

const FALLLBACK_GEMINI_COMMAND: &str = "npx -y @google/gemini-cli@0.23.0";

pub fn base_command() -> &'static str {
    GEMINI_COMMAND.as_str()
}

pub fn fallback_command() -> &'static str {
    FALLLBACK_GEMINI_COMMAND
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Gemini {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

impl Gemini {
    fn build_command_builder_with_base(
        &self,
        base: &str,
    ) -> Result<CommandBuilder, CommandBuildError> {
        let mut builder = CommandBuilder::new(base);

        if let Some(model) = &self.model {
            builder = builder.extend_params(["--model", model.as_str()]);
        }

        if self.yolo.unwrap_or(false) {
            builder = builder.extend_params(["--yolo"]);
            builder = builder.extend_params(["--allowed-tools", "run_shell_command"]);
        }

        builder = builder.extend_params(["--experimental-acp"]);

        apply_overrides(builder, &self.cmd)
    }

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        tracing::info!(
            "build_command_builder using system codex command {}",
            base_command()
        );
        self.build_command_builder_with_base(base_command())
    }

    fn build_fallback_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        tracing::info!(
            "build_fallback_builder using fallback npx gemini command {}",
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
}

#[async_trait]
impl StandardCodingAgentExecutor for Gemini {
    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let harness = AcpAgentHarness::new();
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let approvals = if self.yolo.unwrap_or(false) {
            None
        } else {
            self.approvals.clone()
        };
        let gemini_command = self.build_command_builder()?.build_initial()?;
        match harness
            .spawn_with_command(
                current_dir,
                combined_prompt.clone(),
                gemini_command,
                env,
                &self.cmd,
                approvals.clone(),
            )
            .await
        {
            Ok(child) => Ok(child),
            Err(err) => {
                if self.should_fallback_to_npx(&err) {
                    let fallback_command =
                        self.build_fallback_command_builder()?.build_initial()?;
                    return harness
                        .spawn_with_command(
                            current_dir,
                            combined_prompt,
                            fallback_command,
                            env,
                            &self.cmd,
                            approvals,
                        )
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
        let harness = AcpAgentHarness::new();
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let approvals = if self.yolo.unwrap_or(false) {
            None
        } else {
            self.approvals.clone()
        };
        let gemini_command = self.build_command_builder()?.build_follow_up(&[])?;
        match harness
            .spawn_follow_up_with_command(
                current_dir,
                combined_prompt.clone(),
                session_id,
                gemini_command,
                env,
                &self.cmd,
                approvals.clone(),
            )
            .await
        {
            Ok(child) => Ok(child),
            Err(err) => {
                if self.should_fallback_to_npx(&err) {
                    let fallback_command = self
                        .build_fallback_command_builder()?
                        .build_follow_up(&[])?;
                    return harness
                        .spawn_follow_up_with_command(
                            current_dir,
                            combined_prompt,
                            session_id,
                            fallback_command,
                            env,
                            &self.cmd,
                            approvals,
                        )
                        .await;
                }
                Err(err)
            }
        }
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, worktree_path: &Path) {
        super::acp::normalize_logs(msg_store, worktree_path);
    }

    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|home| home.join(".gemini").join("settings.json"))
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        if let Some(timestamp) = dirs::home_dir()
            .and_then(|home| std::fs::metadata(home.join(".gemini").join("oauth_creds.json")).ok())
            .and_then(|m| m.modified().ok())
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
        {
            return AvailabilityInfo::LoginDetected {
                last_auth_timestamp: timestamp,
            };
        }

        let mcp_config_found = self
            .default_mcp_config_path()
            .map(|p| p.exists())
            .unwrap_or(false);

        let installation_indicator_found = dirs::home_dir()
            .map(|home| home.join(".gemini").join("installation_id").exists())
            .unwrap_or(false);

        if mcp_config_found || installation_indicator_found {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }
}
