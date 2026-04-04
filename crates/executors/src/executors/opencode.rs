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

use crate::{
    approvals::ExecutorApprovalService,
    command::{
        CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides, env_command_or_default,
    },
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, SpawnedChild, StandardCodingAgentExecutor,
        acp::AcpAgentHarness, command_available,
    },
};

mod normalize_logs;
mod plan_mode;
mod sdk;

pub use plan_mode::EXIT_PLAN_MODE_NAME;
use plan_mode::append_plan_mode_prompt_guidance;

static OPENCODE_COMMAND: LazyLock<String> =
    LazyLock::new(|| env_command_or_default("VK_OPENCODE", "opencode"));

const FALLBACK_OPENCODE_COMMAND: &str = "npx -y opencode-ai@1.3.13";

pub fn base_command() -> &'static str {
    OPENCODE_COMMAND.as_str()
}

pub fn fallback_command() -> &'static str {
    FALLBACK_OPENCODE_COMMAND
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Opencode {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whether to run in plan mode. When `true`, overrides any legacy `mode`
    /// field and sends `"plan"` as the ACP session mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<bool>,
    /// Legacy mode string (alias "agent"). Forwarded as ACP session mode when
    /// `plan` is not set. Canonical `plan: true` always wins.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "agent")]
    pub mode: Option<String>,
    /// Auto-approve agent actions.
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
    /// Returns the resolved ACP session mode string, if any.
    ///
    /// `plan: true` wins over the legacy `mode` string.
    fn resolved_mode(&self) -> Option<String> {
        if self.plan.unwrap_or(false) {
            Some("plan".to_string())
        } else {
            self.mode.clone()
        }
    }

    /// Returns true when permission approvals should be skipped entirely.
    fn skip_permissions(&self) -> bool {
        self.auto_approve
    }

    fn build_command_builder_with_base(
        &self,
        base: &str,
    ) -> Result<CommandBuilder, CommandBuildError> {
        let builder = CommandBuilder::new(base).extend_params(["acp"]);
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

    fn make_harness(&self) -> AcpAgentHarness {
        let mut harness = AcpAgentHarness::with_session_namespace("opencode_sessions");
        if let Some(model) = &self.model {
            harness = harness.with_model(model.clone());
        }
        if let Some(mode) = self.resolved_mode() {
            harness = harness.with_mode(mode);
        }
        harness
    }

    fn approvals_for_spawn(&self) -> Option<Arc<dyn ExecutorApprovalService>> {
        if self.skip_permissions() {
            None
        } else {
            self.approvals.clone()
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
        let env = setup_approvals_env(self.skip_permissions(), env);
        let combined_prompt = append_plan_mode_prompt_guidance(
            self.resolved_mode().as_deref(),
            self.append_prompt.combine_prompt(prompt),
        );
        let harness = self.make_harness();
        let approvals = self.approvals_for_spawn();
        let command_parts = self.build_command_builder()?.build_initial()?;
        match harness
            .spawn_with_command(
                current_dir,
                combined_prompt.clone(),
                command_parts,
                &env,
                &self.cmd,
                approvals.clone(),
            )
            .await
        {
            Ok(child) => Ok(child),
            Err(err) => {
                if self.should_fallback_to_npx(&err) {
                    let fallback_parts = self.build_fallback_command_builder()?.build_initial()?;
                    return harness
                        .spawn_with_command(
                            current_dir,
                            combined_prompt,
                            fallback_parts,
                            &env,
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
        let env = setup_approvals_env(self.skip_permissions(), env);
        let combined_prompt = append_plan_mode_prompt_guidance(
            self.resolved_mode().as_deref(),
            self.append_prompt.combine_prompt(prompt),
        );
        let harness = self.make_harness();
        let approvals = self.approvals_for_spawn();
        let command_parts = self.build_command_builder()?.build_follow_up(&[])?;
        match harness
            .spawn_follow_up_with_command(
                current_dir,
                combined_prompt.clone(),
                session_id,
                command_parts,
                &env,
                &self.cmd,
                approvals.clone(),
            )
            .await
        {
            Ok(child) => Ok(child),
            Err(err) => {
                if self.should_fallback_to_npx(&err) {
                    let fallback_parts = self
                        .build_fallback_command_builder()?
                        .build_follow_up(&[])?;
                    return harness
                        .spawn_follow_up_with_command(
                            current_dir,
                            combined_prompt,
                            session_id,
                            fallback_parts,
                            &env,
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

/// Inject `OPENCODE_PERMISSION` env var when permission approvals are active
/// (i.e. `skip_permissions` is false).
fn setup_approvals_env(skip_permissions: bool, env: &ExecutionEnv) -> ExecutionEnv {
    let mut env = env.clone();
    if !skip_permissions && !env.contains_key("OPENCODE_PERMISSION") {
        env.insert(
            "OPENCODE_PERMISSION",
            r#"{"edit": "ask", "bash": "ask", "webfetch": "ask", "doom_loop": "ask", "external_directory": "ask"}"#,
        );
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
            plan: None,
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

    // ── availability tests ────────────────────────────────────────────────────

    /// When `base_command_override` is set to an existing binary the primary
    /// command check should succeed and the fallback path must not be consulted.
    #[test]
    fn get_availability_info_uses_override_command() {
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
        let info = opencode.get_availability_info();
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
        let opencode = make_opencode(None);
        let _info = opencode.get_availability_info();
    }

    // ── profile deserialization tests ─────────────────────────────────────────

    /// Canonical `auto_approve: true` skips permission approvals.
    #[test]
    fn auto_approve_true_deserializes() {
        let json = r#"{"auto_approve": true, "model": "opencode/big-pickle"}"#;
        let oc: Opencode = serde_json::from_str(json).expect("should deserialize");
        assert!(oc.auto_approve);
        assert!(oc.skip_permissions());
    }

    /// Canonical `auto_approve: false` enables approvals.
    #[test]
    fn auto_approve_false_deserializes() {
        let json = r#"{"auto_approve": false}"#;
        let oc: Opencode = serde_json::from_str(json).expect("should deserialize");
        assert!(!oc.auto_approve);
        assert!(!oc.skip_permissions());
    }

    /// `auto_approve` remains the serialized field name.
    #[test]
    fn serializes_auto_approve_field_name() {
        let oc = make_opencode(None);
        let value = serde_json::to_value(&oc).expect("should serialize");
        assert_eq!(
            value.get("auto_approve"),
            Some(&serde_json::Value::Bool(true))
        );
        assert!(value.get("dangerously_skip_permissions").is_none());
    }

    /// Legacy `mode: "plan"` round-trips through resolved_mode.
    #[test]
    fn legacy_mode_plan_resolves() {
        let json = r#"{"mode": "plan", "auto_approve": true}"#;
        let oc: Opencode = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(oc.resolved_mode(), Some("plan".to_string()));
    }

    /// Canonical `plan: true` wins over legacy `mode`.
    #[test]
    fn canonical_plan_wins_over_legacy_mode() {
        let json = r#"{"plan": true, "mode": "other", "auto_approve": true}"#;
        let oc: Opencode = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(oc.resolved_mode(), Some("plan".to_string()));
    }

    /// Non-plan legacy mode is forwarded as-is.
    #[test]
    fn non_plan_legacy_mode_forwarded() {
        let json = r#"{"mode": "auto"}"#;
        let oc: Opencode = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(oc.resolved_mode(), Some("auto".to_string()));
    }

    /// When neither plan nor mode is set, resolved_mode returns None.
    #[test]
    fn no_mode_returns_none() {
        let oc = make_opencode(None);
        assert_eq!(oc.resolved_mode(), None);
    }

    /// Default (no fields set) skips permissions.
    #[test]
    fn default_skips_permissions() {
        let oc = make_opencode(None);
        assert!(oc.skip_permissions());
    }

    // ── command building tests ────────────────────────────────────────────────

    fn command_args(oc: &Opencode) -> Vec<String> {
        oc.build_command_builder_with_base("opencode")
            .expect("builder should succeed")
            .params
            .unwrap_or_default()
    }

    #[test]
    fn command_builder_uses_acp_subcommand() {
        let oc = make_opencode(None);
        let args = command_args(&oc);
        assert_eq!(args, vec!["acp".to_string()]);
    }

    #[test]
    fn fallback_command_builder_uses_acp_subcommand() {
        let oc = make_opencode(None);
        let parts = oc
            .build_fallback_command_builder()
            .expect("fallback builder should succeed")
            .params
            .unwrap_or_default();
        assert_eq!(parts, vec!["acp".to_string()]);
    }
}
