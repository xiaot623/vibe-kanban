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

const FALLBACK_GEMINI_COMMAND: &str = "npx -y @google/gemini-cli@0.34.0";

pub fn base_command() -> &'static str {
    GEMINI_COMMAND.as_str()
}

pub fn fallback_command() -> &'static str {
    FALLBACK_GEMINI_COMMAND
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Gemini {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeminiMode {
    Default,
    Plan,
    Yolo,
}

impl Gemini {
    fn mode_from_flags(plan_mode: bool, yolo_mode: bool) -> GeminiMode {
        if plan_mode {
            GeminiMode::Plan
        } else if yolo_mode {
            GeminiMode::Yolo
        } else {
            GeminiMode::Default
        }
    }

    fn mode(&self) -> GeminiMode {
        Self::mode_from_flags(self.plan.unwrap_or(false), self.yolo.unwrap_or(false))
    }

    fn mode_with_warning(&self) -> GeminiMode {
        let plan_mode = self.plan.unwrap_or(false);
        let yolo_mode = self.yolo.unwrap_or(false);
        if plan_mode && yolo_mode {
            tracing::warn!("Both plan and yolo are enabled. Plan mode will take precedence.");
        }
        Self::mode_from_flags(plan_mode, yolo_mode)
    }

    fn build_command_builder_with_base(
        &self,
        base: &str,
    ) -> Result<CommandBuilder, CommandBuildError> {
        let mut builder = CommandBuilder::new(base);

        if let Some(model) = &self.model {
            builder = builder.extend_params(["--model", model.as_str()]);
        }

        match self.mode_with_warning() {
            GeminiMode::Plan => {
                builder = builder.extend_params(["--approval-mode", "plan"]);
            }
            GeminiMode::Yolo => {
                builder = builder.extend_params(["--yolo"]);
            }
            GeminiMode::Default => {}
        }

        builder = builder.extend_params(["--experimental-acp"]);

        apply_overrides(builder, &self.cmd)
    }

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        tracing::info!(
            "build_command_builder using system gemini command {}",
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
        let approvals = if matches!(self.mode(), GeminiMode::Yolo) {
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
        let approvals = if matches!(self.mode(), GeminiMode::Yolo) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn gemini_with_mode(plan: Option<bool>, yolo: Option<bool>) -> Gemini {
        Gemini {
            append_prompt: AppendPrompt::default(),
            model: None,
            plan,
            yolo,
            cmd: CmdOverrides::default(),
            approvals: None,
        }
    }

    fn command_params(executor: &Gemini) -> Vec<String> {
        executor
            .build_command_builder_with_base("gemini")
            .expect("command builder should be created")
            .params
            .unwrap_or_default()
    }

    fn has_arg_pair(args: &[String], first: &str, second: &str) -> bool {
        args.windows(2)
            .any(|window| window[0] == first && window[1] == second)
    }

    #[test]
    fn plan_mode_adds_plan_approval_flags_without_yolo_or_allowlist() {
        let params = command_params(&gemini_with_mode(Some(true), Some(false)));

        assert!(has_arg_pair(&params, "--approval-mode", "plan"));
        assert!(!params.iter().any(|arg| arg == "--yolo"));
        assert!(!params.iter().any(|arg| arg == "--allowed-tools"));
        assert!(params.iter().any(|arg| arg == "--experimental-acp"));
    }

    #[test]
    fn yolo_mode_adds_yolo_flags_without_plan_approval_or_allowlist() {
        let params = command_params(&gemini_with_mode(Some(false), Some(true)));

        assert!(params.iter().any(|arg| arg == "--yolo"));
        assert!(!params.iter().any(|arg| arg == "--allowed-tools"));
        assert!(!has_arg_pair(&params, "--approval-mode", "plan"));
        assert!(params.iter().any(|arg| arg == "--experimental-acp"));
    }

    #[test]
    fn plan_mode_takes_precedence_over_yolo_when_both_enabled_without_allowlist() {
        let params = command_params(&gemini_with_mode(Some(true), Some(true)));

        assert!(has_arg_pair(&params, "--approval-mode", "plan"));
        assert!(!params.iter().any(|arg| arg == "--yolo"));
        assert!(!params.iter().any(|arg| arg == "--allowed-tools"));
    }

    #[test]
    fn default_mode_has_no_plan_yolo_or_allowlist_flags() {
        let params = command_params(&gemini_with_mode(Some(false), Some(false)));

        assert!(!has_arg_pair(&params, "--approval-mode", "plan"));
        assert!(!params.iter().any(|arg| arg == "--yolo"));
        assert!(!params.iter().any(|arg| arg == "--allowed-tools"));
        assert!(params.iter().any(|arg| arg == "--experimental-acp"));
    }

    #[test]
    fn serde_roundtrip_preserves_optional_plan_field() {
        let executor = gemini_with_mode(Some(true), Some(false));

        let value = serde_json::to_value(&executor).expect("executor should serialize");
        assert_eq!(value.get("plan"), Some(&serde_json::Value::Bool(true)));

        let deserialized: Gemini = serde_json::from_value(value).expect("executor should parse");
        assert_eq!(deserialized, executor);
    }
}
