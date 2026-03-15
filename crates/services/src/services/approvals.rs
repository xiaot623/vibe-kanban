pub mod executor_approvals;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration as StdDuration,
};

use dashmap::DashMap;
use db::models::{
    execution_process::ExecutionProcess,
    task::{CreateTask, Task, TaskStatus},
};
use executors::{
    approvals::ToolCallMetadata,
    executors::opencode::EXIT_PLAN_MODE_NAME,
    logs::{
        ActionType, NormalizedEntry, NormalizedEntryType, ToolStatus,
        utils::patch::{ConversationPatch, extract_normalized_entry_from_patch},
    },
};
use futures::future::{BoxFuture, FutureExt, Shared};
use sqlx::{Error as SqlxError, SqlitePool};
use thiserror::Error;
use tokio::sync::{RwLock, oneshot};
use utils::{
    approvals::{ApprovalRequest, ApprovalResponse, ApprovalStatus},
    log_msg::LogMsg,
    msg_store::MsgStore,
};
use uuid::Uuid;

const TOOL_USE_MATCH_WAIT_TIMEOUT: StdDuration = StdDuration::from_millis(400);
const TOOL_USE_MATCH_POLL_INTERVAL: StdDuration = StdDuration::from_millis(10);
#[derive(Debug)]
struct PendingApproval {
    entry_index: Option<usize>,
    entry: NormalizedEntry,
    execution_process_id: Uuid,
    tool_name: String,
    response_tx: oneshot::Sender<ApprovalStatus>,
}

type ApprovalWaiter = Shared<BoxFuture<'static, ApprovalStatus>>;

#[derive(Debug)]
pub struct ToolContext {
    pub tool_name: String,
    pub execution_process_id: Uuid,
}

#[derive(Debug, Clone)]
pub struct PendingApprovalInfo {
    pub id: String,
    pub tool_name: String,
    pub execution_process_id: Uuid,
    pub entry: NormalizedEntry,
}

#[derive(Clone)]
pub struct Approvals {
    pending: Arc<DashMap<String, PendingApproval>>,
    completed: Arc<DashMap<String, ApprovalStatus>>,
    msg_stores: Arc<RwLock<HashMap<Uuid, Arc<MsgStore>>>>,
}

#[derive(Debug, Error)]
pub enum ApprovalError {
    #[error("approval request not found")]
    NotFound,
    #[error("approval request already completed")]
    AlreadyCompleted,
    #[error("no executor session found for session_id: {0}")]
    NoExecutorSession(String),
    #[error("corresponding tool use entry not found for approval request")]
    NoToolUseEntry,
    #[error(transparent)]
    Custom(#[from] anyhow::Error),
    #[error(transparent)]
    Sqlx(#[from] SqlxError),
}

impl Approvals {
    pub fn new(msg_stores: Arc<RwLock<HashMap<Uuid, Arc<MsgStore>>>>) -> Self {
        Self {
            pending: Arc::new(DashMap::new()),
            completed: Arc::new(DashMap::new()),
            msg_stores,
        }
    }

    pub async fn create_with_waiter(
        &self,
        request: ApprovalRequest,
    ) -> Result<(ApprovalRequest, ApprovalWaiter), ApprovalError> {
        let (tx, rx) = oneshot::channel();
        let waiter: ApprovalWaiter = rx
            .map(|result| result.unwrap_or(ApprovalStatus::TimedOut))
            .boxed()
            .shared();
        let req_id = request.id.clone();

        let (entry_index, entry) = if let Some(store) =
            self.msg_store_by_id(&request.execution_process_id).await
        {
            if let Some((idx, matching_tool)) = wait_for_matching_tool_use(
                store.clone(),
                &request.tool_call_id,
                TOOL_USE_MATCH_WAIT_TIMEOUT,
            )
            .await
            {
                let matching_tool = ensure_plan_presentation_entry(&request, matching_tool);

                let approval_entry = matching_tool
                    .with_tool_status(ToolStatus::PendingApproval {
                        approval_id: req_id.clone(),
                        requested_at: request.created_at,
                        timeout_at: request.timeout_at,
                    })
                    .ok_or(ApprovalError::NoToolUseEntry)?;
                store.push_patch(ConversationPatch::replace(idx, approval_entry));

                tracing::debug!(
                    "Created approval {} for tool '{}' at entry index {}",
                    req_id,
                    request.tool_name,
                    idx
                );
                (Some(idx), matching_tool)
            } else {
                tracing::warn!(
                    "No matching tool use entry found for approval request after waiting: tool='{}', execution_process_id={}; using fallback entry",
                    request.tool_name,
                    request.execution_process_id
                );
                (None, build_fallback_tool_use_entry(&request))
            }
        } else {
            tracing::warn!(
                "No msg_store found for execution_process_id: {}; using fallback entry",
                request.execution_process_id
            );
            (None, build_fallback_tool_use_entry(&request))
        };

        self.pending.insert(
            req_id.clone(),
            PendingApproval {
                entry_index,
                entry,
                execution_process_id: request.execution_process_id,
                tool_name: request.tool_name.clone(),
                response_tx: tx,
            },
        );

        self.spawn_timeout_watcher(req_id.clone(), request.timeout_at, waiter.clone());
        Ok((request, waiter))
    }

    #[tracing::instrument(skip(self, id, req))]
    pub async fn respond(
        &self,
        pool: &SqlitePool,
        id: &str,
        req: ApprovalResponse,
    ) -> Result<(ApprovalStatus, ToolContext), ApprovalError> {
        if let Some((_, p)) = self.pending.remove(id) {
            self.completed.insert(id.to_string(), req.status.clone());
            let _ = p.response_tx.send(req.status.clone());

            if let Some(store) = self.msg_store_by_id(&p.execution_process_id).await {
                if let Some(entry_index) = p.entry_index {
                    let status = ToolStatus::from_approval_status(&req.status).ok_or(
                        ApprovalError::Custom(anyhow::anyhow!("Invalid approval status")),
                    )?;
                    let updated_entry = p
                        .entry
                        .with_tool_status(status)
                        .ok_or(ApprovalError::NoToolUseEntry)?;

                    store.push_patch(ConversationPatch::replace(entry_index, updated_entry));
                }
            } else {
                tracing::warn!(
                    "No msg_store found for execution_process_id: {}",
                    p.execution_process_id
                );
            }

            let tool_ctx = ToolContext {
                tool_name: p.tool_name,
                execution_process_id: p.execution_process_id,
            };

            // If responded, and task is still InReview, move back to InProgress
            if matches!(
                req.status,
                ApprovalStatus::Approved
                    | ApprovalStatus::ProvidedInput { .. }
                    | ApprovalStatus::Denied { .. }
            ) {
                if let Ok(ctx) =
                    ExecutionProcess::load_context(pool, tool_ctx.execution_process_id).await
                {
                    if ctx.task.status == TaskStatus::InReview {
                        // State transition is auto-dispatched by Task::update_status
                        if let Err(e) =
                            Task::update_status(pool, ctx.task.id, TaskStatus::InProgress).await
                        {
                            tracing::warn!(
                                "Failed to update task status to InProgress after approval response: {}",
                                e
                            );
                        }
                    }
                }
            }

            if matches!(req.status, ApprovalStatus::Approved)
                && tool_ctx.tool_name == EXIT_PLAN_MODE_NAME
            {
                let plan_content = p.entry.content.clone();
                match ExecutionProcess::load_context(pool, tool_ctx.execution_process_id).await {
                    Ok(ctx) => {
                        let create_task = CreateTask {
                            project_id: ctx.task.project_id,
                            title: format!("Implement the Plan ({})", ctx.task.title),
                            description: Some(plan_content),
                            status: Some(TaskStatus::Todo),
                            parent_workspace_id: Some(ctx.workspace.id),
                            source_cron_task_id: None,
                            image_ids: None,
                        };
                        let task_id = Uuid::new_v4();
                        // State transition is auto-dispatched by Task::create
                        if let Err(e) = Task::create(pool, &create_task, task_id).await {
                            tracing::warn!(
                                "Failed to create plan implementation task for execution_process_id {}: {}",
                                tool_ctx.execution_process_id,
                                e
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to load execution context for plan approval: {}", e);
                    }
                }
            }

            Ok((req.status, tool_ctx))
        } else if self.completed.contains_key(id) {
            Err(ApprovalError::AlreadyCompleted)
        } else {
            Err(ApprovalError::NotFound)
        }
    }

    #[tracing::instrument(skip(self, id, timeout_at, waiter))]
    fn spawn_timeout_watcher(
        &self,
        id: String,
        timeout_at: chrono::DateTime<chrono::Utc>,
        waiter: ApprovalWaiter,
    ) {
        let pending = self.pending.clone();
        let completed = self.completed.clone();
        let msg_stores = self.msg_stores.clone();

        let now = chrono::Utc::now();
        let to_wait = (timeout_at - now)
            .to_std()
            .unwrap_or_else(|_| StdDuration::from_secs(0));
        let deadline = tokio::time::Instant::now() + to_wait;

        tokio::spawn(async move {
            let status = tokio::select! {
                biased;

                resolved = waiter.clone() => resolved,
                _ = tokio::time::sleep_until(deadline) => ApprovalStatus::TimedOut,
            };

            let is_timeout = matches!(&status, ApprovalStatus::TimedOut);
            completed.insert(id.clone(), status.clone());

            if is_timeout && let Some((_, pending_approval)) = pending.remove(&id) {
                if pending_approval.response_tx.send(status.clone()).is_err() {
                    tracing::debug!("approval '{}' timeout notification receiver dropped", id);
                }

                let store = {
                    let map = msg_stores.read().await;
                    map.get(&pending_approval.execution_process_id).cloned()
                };

                if let Some(store) = store {
                    if let Some(entry_index) = pending_approval.entry_index {
                        if let Some(updated_entry) = pending_approval
                            .entry
                            .with_tool_status(ToolStatus::TimedOut)
                        {
                            store
                                .push_patch(ConversationPatch::replace(entry_index, updated_entry));
                        } else {
                            tracing::warn!(
                                "Timed out approval '{}' but couldn't update tool status (no tool-use entry).",
                                id
                            );
                        }
                    }
                } else {
                    tracing::warn!(
                        "No msg_store found for execution_process_id: {}",
                        pending_approval.execution_process_id
                    );
                }
            }
        });
    }

    async fn msg_store_by_id(&self, execution_process_id: &Uuid) -> Option<Arc<MsgStore>> {
        let map = self.msg_stores.read().await;
        map.get(execution_process_id).cloned()
    }

    pub(crate) async fn get_msg_store_for_execution_process(
        &self,
        execution_process_id: &Uuid,
    ) -> Option<Arc<MsgStore>> {
        self.msg_store_by_id(execution_process_id).await
    }

    /// Check which execution processes have pending approvals.
    /// Returns a set of execution_process_ids that have at least one pending approval.
    pub fn get_pending_execution_process_ids(
        &self,
        execution_process_ids: &[Uuid],
    ) -> HashSet<Uuid> {
        let id_set: HashSet<_> = execution_process_ids.iter().collect();
        self.pending
            .iter()
            .filter_map(|entry| {
                let ep_id = entry.value().execution_process_id;
                if id_set.contains(&ep_id) {
                    Some(ep_id)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn list_pending(&self) -> Vec<PendingApprovalInfo> {
        self.pending
            .iter()
            .map(|entry| PendingApprovalInfo {
                id: entry.key().clone(),
                tool_name: entry.value().tool_name.clone(),
                execution_process_id: entry.value().execution_process_id,
                entry: entry.value().entry.clone(),
            })
            .collect()
    }

    pub(crate) fn pending_by_id(&self, approval_id: &str) -> Option<PendingApprovalInfo> {
        self.pending
            .get(approval_id)
            .map(|entry| PendingApprovalInfo {
                id: entry.key().clone(),
                tool_name: entry.value().tool_name.clone(),
                execution_process_id: entry.value().execution_process_id,
                entry: entry.value().entry.clone(),
            })
    }
}

pub(crate) async fn ensure_task_in_review(pool: &SqlitePool, execution_process_id: Uuid) {
    if let Ok(ctx) = ExecutionProcess::load_context(pool, execution_process_id).await
        && ctx.task.status == TaskStatus::InProgress
        && let Err(e) = Task::update_status(pool, ctx.task.id, TaskStatus::InReview).await
    {
        tracing::warn!(
            "Failed to update task status to InReview for approval request: {}",
            e
        );
    }
}

fn ensure_plan_presentation_entry(
    request: &ApprovalRequest,
    entry: NormalizedEntry,
) -> NormalizedEntry {
    if request.tool_name != EXIT_PLAN_MODE_NAME {
        return entry;
    }

    let Some(plan) = request
        .tool_input
        .get("plan")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
    else {
        return entry;
    };

    let already_plan_entry = matches!(
        entry.entry_type,
        NormalizedEntryType::ToolUse {
            action_type: executors::logs::ActionType::PlanPresentation { .. },
            ..
        }
    );
    if already_plan_entry {
        return entry;
    }

    NormalizedEntry {
        timestamp: entry.timestamp.clone(),
        entry_type: NormalizedEntryType::ToolUse {
            tool_name: EXIT_PLAN_MODE_NAME.to_string(),
            action_type: executors::logs::ActionType::PlanPresentation {
                plan: plan.to_string(),
            },
            status: ToolStatus::Created,
        },
        content: plan.to_string(),
        metadata: entry.metadata.clone(),
    }
}

fn build_fallback_tool_use_entry(request: &ApprovalRequest) -> NormalizedEntry {
    let metadata = serde_json::to_value(ToolCallMetadata {
        tool_call_id: request.tool_call_id.clone(),
    })
    .ok();
    let plan = request
        .tool_input
        .get("plan")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
        .map(str::to_string);

    if request.tool_name == EXIT_PLAN_MODE_NAME {
        let plan = plan.unwrap_or_else(|| "Plan content unavailable".to_string());
        return NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: EXIT_PLAN_MODE_NAME.to_string(),
                action_type: ActionType::PlanPresentation { plan: plan.clone() },
                status: ToolStatus::Created,
            },
            content: plan,
            metadata,
        };
    }

    NormalizedEntry {
        timestamp: None,
        entry_type: NormalizedEntryType::ToolUse {
            tool_name: request.tool_name.clone(),
            action_type: ActionType::Tool {
                tool_name: request.tool_name.clone(),
                arguments: Some(request.tool_input.clone()),
                result: None,
            },
            status: ToolStatus::Created,
        },
        content: request.tool_name.clone(),
        metadata,
    }
}

/// Find a matching tool use entry that hasn't been assigned to an approval yet
/// Matches by tool call id from tool metadata
fn find_matching_tool_use(
    store: &MsgStore,
    tool_call_id: &str,
) -> Option<(usize, NormalizedEntry)> {
    let history = store.get_history();

    // Single loop through history
    for msg in history.iter().rev() {
        if let LogMsg::JsonPatch(patch) = msg
            && let Some((idx, entry)) = extract_normalized_entry_from_patch(patch)
            && let NormalizedEntryType::ToolUse { status, .. } = &entry.entry_type
        {
            // Only match tools that are in Created state
            if !matches!(status, ToolStatus::Created) {
                continue;
            }

            // Match by tool call id from metadata
            if let Some(metadata) = &entry.metadata
                && let Ok(ToolCallMetadata {
                    tool_call_id: entry_call_id,
                    ..
                }) = serde_json::from_value::<ToolCallMetadata>(metadata.clone())
                && entry_call_id == tool_call_id
            {
                tracing::debug!(
                    "Matched tool use entry at index {idx} for tool call id '{tool_call_id}'"
                );
                return Some((idx, entry));
            }
        }
    }

    None
}

async fn wait_for_matching_tool_use(
    store: Arc<MsgStore>,
    tool_call_id: &str,
    timeout: StdDuration,
) -> Option<(usize, NormalizedEntry)> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if let Some(entry) = find_matching_tool_use(store.as_ref(), tool_call_id) {
            return Some(entry);
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return None;
        }

        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(std::cmp::min(remaining, TOOL_USE_MATCH_POLL_INTERVAL)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use executors::logs::{ActionType, NormalizedEntry, NormalizedEntryType, ToolStatus};
    use tokio::sync::RwLock;
    use utils::{
        approvals::{ApprovalRequest, CreateApprovalRequest},
        log_msg::LogMsg,
        msg_store::MsgStore,
    };

    use super::*;

    fn create_tool_use_entry(
        tool_name: &str,
        file_path: &str,
        id: &str,
        status: ToolStatus,
    ) -> NormalizedEntry {
        NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: tool_name.to_string(),
                action_type: ActionType::FileRead {
                    path: file_path.to_string(),
                },
                status,
            },
            content: format!("Reading {file_path}"),
            metadata: Some(
                serde_json::to_value(ToolCallMetadata {
                    tool_call_id: id.to_string(),
                })
                .unwrap(),
            ),
        }
    }

    #[test]
    fn test_parallel_tool_call_approval_matching() {
        let store = Arc::new(MsgStore::new());

        // Setup: Simulate 3 parallel Read tool calls with different files
        let read_foo = create_tool_use_entry("Read", "foo.rs", "foo-id", ToolStatus::Created);
        let read_bar = create_tool_use_entry("Read", "bar.rs", "bar-id", ToolStatus::Created);
        let read_baz = create_tool_use_entry("Read", "baz.rs", "baz-id", ToolStatus::Created);

        store.push_patch(
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(0, read_foo),
        );
        store.push_patch(
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(1, read_bar),
        );
        store.push_patch(
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(2, read_baz),
        );

        let (idx_foo, _) =
            find_matching_tool_use(store.as_ref(), "foo-id").expect("Should match foo.rs");
        let (idx_bar, _) =
            find_matching_tool_use(store.as_ref(), "bar-id").expect("Should match bar.rs");
        let (idx_baz, _) =
            find_matching_tool_use(store.as_ref(), "baz-id").expect("Should match baz.rs");

        assert_eq!(idx_foo, 0, "foo.rs should match first entry");
        assert_eq!(idx_bar, 1, "bar.rs should match second entry");
        assert_eq!(idx_baz, 2, "baz.rs should match third entry");

        // Test 2: Already pending tools are skipped
        let read_pending = create_tool_use_entry(
            "Read",
            "pending.rs",
            "pending-id",
            ToolStatus::PendingApproval {
                approval_id: "test-id".to_string(),
                requested_at: chrono::Utc::now(),
                timeout_at: chrono::Utc::now(),
            },
        );
        store.push_patch(
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(3, read_pending),
        );

        assert!(
            find_matching_tool_use(store.as_ref(), "pending-id").is_none(),
            "Should not match tools in PendingApproval state"
        );

        // Test 3: Wrong tool id returns None
        assert!(
            find_matching_tool_use(store.as_ref(), "wrong-id").is_none(),
            "Should not match different tool ids"
        );
    }

    fn latest_entry(store: &MsgStore) -> NormalizedEntry {
        store
            .get_history()
            .iter()
            .filter_map(|msg| match msg {
                LogMsg::JsonPatch(patch) => extract_normalized_entry_from_patch(patch),
                _ => None,
            })
            .max_by_key(|(idx, _)| *idx)
            .map(|(_, entry)| entry)
            .expect("expected at least one normalized entry")
    }

    #[tokio::test]
    async fn create_with_waiter_rewrites_exit_plan_mode_entry_to_plan_presentation() {
        let store = Arc::new(MsgStore::new());
        let execution_process_id = Uuid::new_v4();
        let plan_text = "# Plan\n- Step 1";
        let call_id = "plan-call-id";

        let raw_entry = create_tool_use_entry("plan_exit", "plan.md", call_id, ToolStatus::Created);
        store.push_patch(
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(0, raw_entry),
        );

        let mut map = HashMap::new();
        map.insert(execution_process_id, store.clone());
        let approvals = Approvals::new(Arc::new(RwLock::new(map)));

        let request = ApprovalRequest::from_create(
            CreateApprovalRequest {
                tool_name: EXIT_PLAN_MODE_NAME.to_string(),
                tool_input: serde_json::json!({ "plan": plan_text }),
                tool_call_id: call_id.to_string(),
            },
            execution_process_id,
        );

        let (_created_request, _waiter) = approvals
            .create_with_waiter(request)
            .await
            .expect("approval request should be created");

        let pending = approvals.list_pending();
        assert_eq!(pending.len(), 1);

        let pending_entry = &pending[0].entry;
        assert_eq!(pending_entry.content, plan_text);
        match &pending_entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                status,
            } => {
                assert_eq!(tool_name, EXIT_PLAN_MODE_NAME);
                assert!(matches!(status, ToolStatus::Created));
                assert!(matches!(
                    action_type,
                    ActionType::PlanPresentation { plan } if plan == plan_text
                ));
            }
            _ => panic!("expected tool entry"),
        }

        let latest = latest_entry(&store);
        match latest.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type,
                status,
            } => {
                assert_eq!(tool_name, EXIT_PLAN_MODE_NAME);
                assert!(matches!(status, ToolStatus::PendingApproval { .. }));
                assert!(matches!(
                    action_type,
                    ActionType::PlanPresentation { plan } if plan == plan_text
                ));
            }
            _ => panic!("expected pending tool entry"),
        }
    }

    #[tokio::test]
    async fn create_with_waiter_waits_for_delayed_tool_entry() {
        let store = Arc::new(MsgStore::new());
        let execution_process_id = Uuid::new_v4();
        let call_id = "delayed-tool-id";

        let mut map = HashMap::new();
        map.insert(execution_process_id, store.clone());
        let approvals = Approvals::new(Arc::new(RwLock::new(map)));

        let delayed_store = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            delayed_store.push_patch(
                executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                    0,
                    create_tool_use_entry("Read", "delayed.rs", call_id, ToolStatus::Created),
                ),
            );
        });

        let request = ApprovalRequest::from_create(
            CreateApprovalRequest {
                tool_name: "Read".to_string(),
                tool_input: serde_json::json!({ "tool_call": { "id": call_id } }),
                tool_call_id: call_id.to_string(),
            },
            execution_process_id,
        );

        let (_created_request, _waiter) = approvals
            .create_with_waiter(request)
            .await
            .expect("approval request should be created");

        let latest = latest_entry(&store);
        match latest.entry_type {
            NormalizedEntryType::ToolUse { status, .. } => {
                assert!(matches!(status, ToolStatus::PendingApproval { .. }));
            }
            _ => panic!("expected pending tool entry"),
        }
    }

    #[tokio::test]
    async fn create_with_waiter_keeps_pending_when_tool_entry_missing() {
        let store = Arc::new(MsgStore::new());
        let execution_process_id = Uuid::new_v4();
        let plan_text = "# Plan\n- Fallback";

        let mut map = HashMap::new();
        map.insert(execution_process_id, store);
        let approvals = Approvals::new(Arc::new(RwLock::new(map)));

        let request = ApprovalRequest::from_create(
            CreateApprovalRequest {
                tool_name: EXIT_PLAN_MODE_NAME.to_string(),
                tool_input: serde_json::json!({ "plan": plan_text }),
                tool_call_id: "missing-tool-entry".to_string(),
            },
            execution_process_id,
        );

        let (_created_request, waiter) = approvals
            .create_with_waiter(request)
            .await
            .expect("approval request should be created");

        let wait_result = tokio::time::timeout(std::time::Duration::from_millis(30), waiter).await;
        assert!(
            wait_result.is_err(),
            "waiter should remain pending instead of resolving immediately"
        );

        let pending = approvals.list_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].tool_name, EXIT_PLAN_MODE_NAME);
        assert_eq!(pending[0].entry.content, plan_text);
        match &pending[0].entry.entry_type {
            NormalizedEntryType::ToolUse { action_type, .. } => {
                assert!(matches!(
                    action_type,
                    ActionType::PlanPresentation { plan } if plan == plan_text
                ));
            }
            _ => panic!("expected tool entry"),
        }
    }
}
