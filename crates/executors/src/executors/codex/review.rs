use std::sync::Arc;

use codex_app_server_protocol::ReviewTarget;

use super::{
    client::{AppServerClient, LogWriter, SessionConfigParams},
    jsonrpc::{ExitSignalSender, JsonRpcPeer},
    session::SessionHandler,
};
use crate::{approvals::ExecutorApprovalService, executors::ExecutorError};

#[allow(clippy::too_many_arguments)]
pub async fn launch_codex_review(
    session_config: SessionConfigParams,
    resume_session: Option<String>,
    review_target: ReviewTarget,
    child_stdout: tokio::process::ChildStdout,
    child_stdin: tokio::process::ChildStdin,
    log_writer: LogWriter,
    exit_signal_tx: ExitSignalSender,
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
    auto_approve: bool,
) -> Result<(), ExecutorError> {
    let client = AppServerClient::new(log_writer, approvals, auto_approve);
    let rpc_peer = JsonRpcPeer::spawn(child_stdin, child_stdout, client.clone(), exit_signal_tx);
    client.connect(rpc_peer);
    client.initialize().await?;
    let account = client.get_account().await?;
    if account.requires_openai_auth && account.account.is_none() {
        return Err(ExecutorError::AuthRequired(
            "Codex authentication required".to_string(),
        ));
    }

    let thread_id = match resume_session {
        Some(session_id) => {
            let (rollout_path, forked_session_id) = SessionHandler::fork_rollout_file(&session_id)
                .map_err(|e| ExecutorError::FollowUpNotSupported(e.to_string()))?;
            let response = client
                .thread_resume(
                    forked_session_id,
                    Some(rollout_path.clone()),
                    session_config,
                )
                .await?;
            tracing::debug!(
                "resuming session for review using rollout file {}, response {:?}",
                rollout_path.display(),
                response
            );
            response.thread.id
        }
        None => {
            let response = client.thread_start(session_config).await?;
            response.thread.id
        }
    };

    client.register_session(&thread_id).await?;
    client.start_review(thread_id, review_target).await?;

    Ok(())
}
