//! Task state handlers for Telegram notifications.
//!
//! triggered when task status changes occur.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
    vec,
};

use dashmap::DashMap;
use db::{
    DBService,
    models::{
        execution_process::{ExecutionProcess, ExecutionProcessRunReason, ExecutionProcessStatus},
        task::{Task, TaskStatus},
        telegram_flow_binding::TelegramFlowBinding,
        workspace::Workspace,
        workspace_repo::WorkspaceRepo,
    },
    task_state::{
        TaskStateTransition,
        dispatcher::TaskStateDispatcher,
        handler::{TransitionFilter, fn_handler},
    },
};
use executors::logs::{
    ActionType, NormalizedEntry, NormalizedEntryType, TokenUsageInfo, ToolStatus,
    utils::patch::extract_normalized_entry_from_patch,
};
use teloxide::{
    prelude::*,
    types::{MessageId, ThreadId},
};
use tokio::sync::{OnceCell, RwLock};
use tokio_util::sync::CancellationToken;
use utils::{log_msg::LogMsg, msg_store::MsgStore};
use uuid::Uuid;

use super::{EXIT_PLAN_MODE_NAME, flow, format, keyboard, lark_wiki, telegraph, topic};
use crate::services::{approvals::Approvals, config::Config, git::GitService};

/// Telegram context for event handlers.
#[derive(Clone)]
pub struct TelegramContext {
    pub db: DBService,
    pub bot: Bot,
    pub chat_id: ChatId,
    pub config: Arc<RwLock<Config>>,
    pub approvals: Approvals,
    pub git: GitService,
}

/// Global telegram context, set during bot initialization.
static TELEGRAM_CONTEXT: OnceLock<Arc<RwLock<Option<TelegramContext>>>> = OnceLock::new();

static TELEGRAM_HANDLER_REGISTRATION: OnceCell<()> = OnceCell::const_new();
static RUN_FEED_WATCHERS: OnceLock<Arc<RwLock<HashMap<Uuid, RunFeedWatcherHandle>>>> =
    OnceLock::new();
static PLAN_REVIEW_MESSAGE_IDS: OnceLock<Arc<RwLock<PlanReviewMessageRegistry>>> = OnceLock::new();
static RUNNING_MESSAGE_IDS: OnceLock<Arc<RwLock<RunningMessageRegistry>>> = OnceLock::new();

/// Cache of TTS text keyed by (flow_token, msg_id) so the Audio handler can
/// retrieve the assistant text that was shown in the stage summary card.
static STAGE_SUMMARY_TTS_TEXT: OnceLock<DashMap<(String, i32), String>> = OnceLock::new();

fn tts_text_cache() -> &'static DashMap<(String, i32), String> {
    STAGE_SUMMARY_TTS_TEXT.get_or_init(DashMap::new)
}

#[derive(Debug, Default)]
struct PlanReviewMessageRegistry {
    by_execution_process: HashMap<Uuid, Vec<i32>>,
}

impl PlanReviewMessageRegistry {
    /// Store a non-empty set of message IDs for a task.
    /// To remove an entry use [`Self::clear`] or [`Self::take`].
    fn record(&mut self, execution_process_id: Uuid, message_ids: Vec<i32>) {
        debug_assert!(
            !message_ids.is_empty(),
            "record() called with empty ids; use clear() instead"
        );
        if !message_ids.is_empty() {
            self.by_execution_process
                .insert(execution_process_id, message_ids);
        }
    }

    /// Remove and return any recorded message IDs for a task (one-shot).
    fn take(&mut self, execution_process_id: Uuid) -> Vec<i32> {
        self.by_execution_process
            .remove(&execution_process_id)
            .unwrap_or_default()
    }
}

#[derive(Debug, Default)]
struct RunningMessageRegistry {
    by_session: HashMap<Uuid, Vec<i32>>,
}

impl RunningMessageRegistry {
    fn record(&mut self, session_id: Uuid, message_id: i32) {
        self.by_session
            .entry(session_id)
            .or_default()
            .push(message_id);
    }

    fn take(&mut self, session_id: Uuid) -> Vec<i32> {
        self.by_session.remove(&session_id).unwrap_or_default()
    }
}

pub async fn set_telegram_context(ctx: TelegramContext) {
    let lock = TELEGRAM_CONTEXT.get_or_init(|| Arc::new(RwLock::new(None)));
    let mut guard = lock.write().await;
    *guard = Some(ctx);
}

/// Clear the telegram context (for shutdown).
pub async fn clear_telegram_context() {
    if let Some(lock) = TELEGRAM_CONTEXT.get() {
        let mut guard = lock.write().await;
        *guard = None;
    }
    cancel_all_run_feed_watchers();
    clear_plan_review_message_registry();
    clear_running_message_registry();
}

pub async fn notify_coding_agent_execution_started(session_id: Uuid, execution_process_id: Uuid) {
    let Some(tg) = get_context().await else {
        return;
    };

    if let Ok(flow_ctx) =
        flow::ensure_flow_context_for_session(&tg.db.pool, session_id, Some(execution_process_id))
            .await
    {
        let _ = TelegramFlowBinding::refresh_expiry(&tg.db.pool, &flow_ctx.flow_token).await;
        let topic_enabled = tg.config.read().await.telegram.topic_enabled;
        if topic_enabled
            && let Ok(Some(task)) = Task::find_by_id(&tg.db.pool, flow_ctx.task_id).await
        {
            let _ = topic::ensure_task_topic(&tg.db.pool, &tg.bot, tg.chat_id, &task).await;
        }
    }

    start_or_refresh_run_feed_watcher(tg, session_id).await;
}

pub async fn notify_flow_deleted(session_id: Uuid) {
    stop_run_feed_watcher(session_id).await;
}

pub async fn get_context() -> Option<TelegramContext> {
    let lock = TELEGRAM_CONTEXT.get()?;
    lock.read().await.clone()
}

#[derive(Clone)]
struct RunFeedWatcherHandle {
    watcher_id: Uuid,
    cancel: CancellationToken,
}

fn run_feed_watchers() -> &'static Arc<RwLock<HashMap<Uuid, RunFeedWatcherHandle>>> {
    RUN_FEED_WATCHERS.get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
}

fn plan_review_message_registry() -> &'static Arc<RwLock<PlanReviewMessageRegistry>> {
    PLAN_REVIEW_MESSAGE_IDS
        .get_or_init(|| Arc::new(RwLock::new(PlanReviewMessageRegistry::default())))
}

fn running_message_registry() -> &'static Arc<RwLock<RunningMessageRegistry>> {
    RUNNING_MESSAGE_IDS.get_or_init(|| Arc::new(RwLock::new(RunningMessageRegistry::default())))
}

fn cancel_all_run_feed_watchers() {
    let Some(lock) = RUN_FEED_WATCHERS.get() else {
        return;
    };

    if let Ok(mut guard) = lock.try_write() {
        let handles: Vec<_> = guard.drain().map(|(_, handle)| handle).collect();
        for handle in handles {
            handle.cancel.cancel();
        }
    }
}

fn clear_plan_review_message_registry() {
    let Some(lock) = PLAN_REVIEW_MESSAGE_IDS.get() else {
        return;
    };

    if let Ok(mut guard) = lock.try_write() {
        guard.by_execution_process.clear();
    }
}

fn clear_running_message_registry() {
    let Some(lock) = RUNNING_MESSAGE_IDS.get() else {
        return;
    };

    if let Ok(mut guard) = lock.try_write() {
        guard.by_session.clear();
    }
}

async fn record_plan_review_message_ids(execution_process_id: Uuid, message_ids: Vec<MessageId>) {
    let ids: Vec<i32> = message_ids.into_iter().map(|id| id.0).collect();
    plan_review_message_registry()
        .write()
        .await
        .record(execution_process_id, ids);
}

pub(super) async fn take_plan_review_message_ids(execution_process_id: Uuid) -> Vec<MessageId> {
    plan_review_message_registry()
        .write()
        .await
        .take(execution_process_id)
        .into_iter()
        .map(MessageId)
        .collect()
}

pub(super) async fn record_running_message_id(session_id: Uuid, message_id: MessageId) {
    running_message_registry()
        .write()
        .await
        .record(session_id, message_id.0);
}

async fn take_running_message_ids(session_id: Uuid) -> Vec<MessageId> {
    running_message_registry()
        .write()
        .await
        .take(session_id)
        .into_iter()
        .map(MessageId)
        .collect()
}

/// Register Telegram handlers with the task state dispatcher
pub async fn register_handlers(dispatcher: &TaskStateDispatcher) {
    TELEGRAM_HANDLER_REGISTRATION
        .get_or_init(|| async {
            TelegramHandlerWrapper::<TaskCreatedHandler>::register(dispatcher).await;
            TelegramHandlerWrapper::<TaskInProgressHandler>::register(dispatcher).await;
            TelegramHandlerWrapper::<TaskOutOfInProgressHandler>::register(dispatcher).await;
            TelegramHandlerWrapper::<TaskInReviewHandler>::register(dispatcher).await;
            TelegramHandlerWrapper::<TaskFinishedHandler>::register(dispatcher).await;
        })
        .await;
}

#[async_trait::async_trait]
pub trait TelegramHandler: Send + Sync + 'static {
    /// The filter for which transitions this handler should receive
    fn filter() -> TransitionFilter
    where
        Self: Sized;

    /// Handler name for logging/debugging
    fn name() -> &'static str
    where
        Self: Sized;

    /// The actual handler logic, called only when Telegram context is available
    async fn handle_with_context(&self, tg: &TelegramContext, transition: &TaskStateTransition);
}

/// Wrapper to convert a TelegramHandler into a TaskStateHandler.
pub struct TelegramHandlerWrapper<H: TelegramHandler> {
    _phantom: std::marker::PhantomData<H>,
}

impl<H: TelegramHandler> TelegramHandlerWrapper<H> {
    /// Register this handler with the dispatcher
    pub async fn register(dispatcher: &TaskStateDispatcher)
    where
        H: Default,
    {
        let handler = fn_handler(H::name(), H::filter(), |_, transition| {
            Box::pin(async move {
                let Some(tg) = get_context().await else {
                    tracing::warn!(
                        handler = H::name(),
                        task_id = %transition.task_id(),
                        from = ?transition.from_status(),
                        to = ?transition.to_status(),
                        "Skipping telegram task-state notification because context is unavailable"
                    );
                    return;
                };

                let handler = H::default();
                handler.handle_with_context(&tg, transition).await;
            })
        });
        dispatcher.register_handler(handler).await;
    }
}

/// Handler for newly created tasks (add a task)
#[derive(Default)]
pub struct TaskCreatedHandler;

#[async_trait::async_trait]
impl TelegramHandler for TaskCreatedHandler {
    fn filter() -> TransitionFilter {
        TransitionFilter::new().to(vec![TaskStatus::Todo])
    }

    fn name() -> &'static str {
        "on_task_created"
    }

    async fn handle_with_context(&self, tg: &TelegramContext, transition: &TaskStateTransition) {
        let task = &transition.task;
        let result = format::send_rich_then_plain(
            &tg.bot,
            tg.chat_id,
            &format!("✅ Task created: {}\n\nRun it now?", task.title),
            Some(keyboard::task_detail_keyboard(task.id, &task.status)),
        )
        .await;

        if let Err(err) = result {
            tracing::warn!("Failed to send telegram notification: {}", err);
        }
    }
}

/// Handler for Todo tasks transitioning to InProgress status (run a task)
#[derive(Default)]
pub struct TaskInProgressHandler;

#[async_trait::async_trait]
impl TelegramHandler for TaskInProgressHandler {
    fn filter() -> TransitionFilter {
        TransitionFilter::new().to(vec![TaskStatus::InProgress])
    }

    fn name() -> &'static str {
        "on_task_in_progress"
    }

    async fn handle_with_context(&self, tg: &TelegramContext, transition: &TaskStateTransition) {
        let _ = (tg, transition);
    }
}

/// Handler for tasks leaving InProgress.
#[derive(Default)]
pub struct TaskOutOfInProgressHandler;

#[async_trait::async_trait]
impl TelegramHandler for TaskOutOfInProgressHandler {
    fn filter() -> TransitionFilter {
        TransitionFilter::new()
            .from(vec![TaskStatus::InProgress])
            .to(vec![
                TaskStatus::Todo,
                TaskStatus::InReview,
                TaskStatus::Done,
                TaskStatus::Cancelled,
            ])
    }

    fn name() -> &'static str {
        "on_task_out_of_in_progress"
    }

    async fn handle_with_context(&self, _tg: &TelegramContext, transition: &TaskStateTransition) {
        let _ = transition;
    }
}

/// Handler for tasks transitioning to InReview status
/// if the task is planned, include the plan in the message
#[derive(Default)]
pub struct TaskInReviewHandler;

#[async_trait::async_trait]
impl TelegramHandler for TaskInReviewHandler {
    fn filter() -> TransitionFilter {
        TransitionFilter::new().to(vec![TaskStatus::InReview])
    }

    fn name() -> &'static str {
        "on_task_in_review"
    }

    async fn handle_with_context(&self, tg: &TelegramContext, transition: &TaskStateTransition) {
        let task = &transition.task;

        // If the task has a pending plan, split and send as multiple messages.
        // This transition is task-scoped and can be ambiguous with concurrent flows,
        // so we keep this card flow-aware only when it can be resolved safely.
        if let Some(plan) = find_exit_plan_approval(tg, task.id).await {
            let chunks = split_plan(&plan.plan);
            let total = chunks.len();
            let mut sent_message_ids = Vec::new();
            for (i, chunk) in chunks.into_iter().enumerate() {
                // Attach approve/reject buttons to the last chunk.
                let markup = if i == total - 1 {
                    if let Ok(Some(flow_ctx)) = flow::resolve_flow_context_from_execution(
                        &tg.db.pool,
                        plan.execution_process_id,
                    )
                    .await
                    {
                        Some(keyboard::review_notification_flow_keyboard(
                            &flow_ctx.flow_token,
                        ))
                    } else {
                        Some(keyboard::review_notification_keyboard(task.id))
                    }
                } else {
                    None
                };
                if let Some(message_id) = send_task_telegram_card(tg, task.id, chunk, markup).await
                {
                    sent_message_ids.push(message_id);
                }
            }
            if !sent_message_ids.is_empty() {
                record_plan_review_message_ids(plan.execution_process_id, sent_message_ids).await;
            }
            return;
        }

        // No split review cards for this task in the current transition.
        // Explicitly clear any IDs that might have been recorded from a prior InReview
        // cycle so they are not stale-deleted on a future approve/reject.
        // no-op: flow-local clear requires execution process context.
    }
}

/// Handler for tasks transitioning to Done status.
#[derive(Default)]
pub struct TaskFinishedHandler;

#[async_trait::async_trait]
impl TelegramHandler for TaskFinishedHandler {
    fn filter() -> TransitionFilter {
        TransitionFilter::new().to(vec![TaskStatus::Done])
    }

    fn name() -> &'static str {
        "on_task_done"
    }

    async fn handle_with_context(&self, tg: &TelegramContext, transition: &TaskStateTransition) {
        let task = &transition.task;
        let configured_daily_project_id = {
            let config = tg.config.read().await;
            config.daily_mode.project_id.clone()
        };
        if !should_send_task_finished_notification(
            Some(task.project_id),
            configured_daily_project_id.as_deref(),
        ) {
            send_task_topic_close_prompt(tg, task.id, &task.title).await;
            return;
        }

        // Use stored diff stats from the task (computed before merge),
        // falling back to runtime computation if not available
        let diff_stats = match (task.diff_additions, task.diff_deletions) {
            (Some(added), Some(removed)) => Some((added as usize, removed as usize)),
            _ => compute_workspace_diff_stats(tg, task.id).await,
        };

        let message = format!(
            "🎉🎉🎉 Task {} Finished\ndiff: {}",
            task.title,
            match diff_stats {
                Some((added, removed)) => format!("+{} / -{}", added, removed),
                None => format!("+{0} / -{0}", 0),
            }
        );

        let _ = send_task_telegram_card(tg, task.id, message, None).await;
        send_task_topic_close_prompt(tg, task.id, &task.title).await;
    }
}

const RUN_FEED_POLL_INTERVAL: Duration = Duration::from_millis(800);
const RUN_FEED_STORE_WAIT_ATTEMPTS: usize = 16;
const REQUEST_USER_INPUT_TOOL_NAME: &str = "request_user_input";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryDeliveryBehavior {
    Ignore,
    RealtimeOnly,
    SummaryOnly,
    RealtimeAndSummary,
    RealtimeAndSummaryTrigger,
}

#[derive(Debug, Clone, Copy)]
enum SummaryTrigger {
    NextAction,
    ExecutionFinished,
    TaskLeftInProgress,
}

#[derive(Debug, Default, Clone)]
struct ToolSummaryStats {
    total: usize,
    created: usize,
    success: usize,
    failed: usize,
    denied: usize,
    timed_out: usize,
    pending_approval: usize,
}

#[derive(Debug, Default)]
struct StageSummaryData {
    user_first: Option<String>,
    user_latest: Option<String>,
    assistant_messages: Vec<String>,
    model_latest: Option<String>,
    tool_stats: ToolSummaryStats,
    system_count: usize,
    system_latest: Option<String>,
    token_usage: Option<TokenUsageInfo>,
}

#[derive(Debug)]
struct EntryUpdate {
    index: usize,
    previous: Option<NormalizedEntry>,
    current: NormalizedEntry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StableTextKind {
    Assistant,
    Thinking,
}

#[derive(Debug, Default)]
struct ToolTurnAggregation {
    current_user_turn_entry_index: Option<usize>,
    telegram_message_id: Option<MessageId>,
    appended_terminal_tool_indexes: HashSet<usize>,
    rendered_rows: Vec<String>,
}

#[derive(Default)]
struct RunFeedAccumulator {
    entries_by_index: BTreeMap<usize, NormalizedEntry>,
    update_seq_by_index: HashMap<usize, usize>,
    current_seq: usize,
    last_summary_seq: usize,
    task_id: Option<Uuid>,
    task_project_id: Option<Uuid>,
    task_title: Option<String>,
    session_id: Option<Uuid>,
    workspace_id: Option<Uuid>,
    execution_process_id: Option<Uuid>,
    flow_context: Option<flow::TelegramFlowContext>,
    sent_pending_approvals: HashSet<String>,
    active_stable_text_entry_index: Option<usize>,
    active_stable_text_message_id: Option<MessageId>,
    active_stable_text_kind: Option<StableTextKind>,
    tool_turn: ToolTurnAggregation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolTurnUpdatePlan {
    SkipDuplicate,
    EditCurrent {
        rows: Vec<String>,
        message: String,
    },
    ReplaceMessage {
        old_message_id: Option<MessageId>,
        rows: Vec<String>,
        message: String,
    },
}

impl RunFeedAccumulator {
    fn apply_patch(&mut self, patch: &json_patch::Patch) -> Option<EntryUpdate> {
        let (index, entry) = extract_normalized_entry_from_patch(patch)?;
        let previous = self.entries_by_index.insert(index, entry.clone());
        self.current_seq = self.current_seq.saturating_add(1);
        self.update_seq_by_index.insert(index, self.current_seq);
        Some(EntryUpdate {
            index,
            previous,
            current: entry,
        })
    }

    fn iter_entries_for_summary(&self, changed_only: bool) -> Vec<(usize, NormalizedEntry)> {
        self.entries_by_index
            .iter()
            .filter(|(idx, _)| {
                !changed_only
                    || self
                        .update_seq_by_index
                        .get(idx)
                        .is_some_and(|seq| *seq > self.last_summary_seq)
            })
            .map(|(idx, entry)| (*idx, entry.clone()))
            .collect()
    }

    fn collect_summary_data(&self, changed_only: bool) -> StageSummaryData {
        let mut data = StageSummaryData::default();
        for (_, entry) in self.iter_entries_for_summary(changed_only) {
            match entry.entry_type {
                NormalizedEntryType::UserMessage => {
                    if data.user_first.is_none() {
                        data.user_first = Some(entry.content.clone());
                    }
                    data.user_latest = Some(entry.content);
                }
                NormalizedEntryType::AssistantMessage => {
                    data.assistant_messages.push(entry.content);
                }
                NormalizedEntryType::ToolUse { status, .. } => {
                    data.tool_stats.total += 1;
                    match status {
                        ToolStatus::Created => data.tool_stats.created += 1,
                        ToolStatus::Success => data.tool_stats.success += 1,
                        ToolStatus::Failed => data.tool_stats.failed += 1,
                        ToolStatus::Denied { .. } => data.tool_stats.denied += 1,
                        ToolStatus::TimedOut => data.tool_stats.timed_out += 1,
                        ToolStatus::PendingApproval { .. } => data.tool_stats.pending_approval += 1,
                    }
                }
                NormalizedEntryType::SystemMessage => {
                    data.system_count += 1;
                    data.system_latest = Some(entry.content.clone());
                    if let Some(model) = extract_model_related_info(&entry.content) {
                        data.model_latest = Some(model);
                    }
                }
                NormalizedEntryType::TokenUsageInfo(usage) => {
                    data.token_usage = Some(usage);
                }
                _ => {}
            }
        }

        data
    }

    fn finalize_stage(&mut self) {
        self.last_summary_seq = self.current_seq;
    }

    fn remember_task(&mut self, task_id: Uuid, task_project_id: Uuid, title: &str) {
        self.task_id = Some(task_id);
        self.task_project_id = Some(task_project_id);
        self.task_title = Some(title.to_string());
    }

    fn remember_session(&mut self, session_id: Uuid, workspace_id: Uuid) {
        self.session_id = Some(session_id);
        self.workspace_id = Some(workspace_id);
    }

    fn remember_flow_context(&mut self, flow_ctx: flow::TelegramFlowContext) {
        self.flow_context = Some(flow_ctx);
    }

    fn topic_thread_id(&self) -> Option<ThreadId> {
        self.flow_context.as_ref()?.topic_thread_id
    }

    fn remember_execution_process(&mut self, execution_process_id: Uuid) {
        if self.execution_process_id != Some(execution_process_id) {
            self.reset_execution_process_state();
            self.execution_process_id = Some(execution_process_id);
        }
    }

    fn flow_prefix(&self) -> String {
        let (executor, variant) = self
            .flow_context
            .as_ref()
            .map(|ctx| (ctx.executor_label.clone(), ctx.variant_label.clone()))
            .unwrap_or_else(|| ("UNKNOWN".to_string(), "DEFAULT".to_string()));
        format!("[{executor} · {variant}]")
    }

    fn reset_execution_process_state(&mut self) {
        self.entries_by_index.clear();
        self.update_seq_by_index.clear();
        self.current_seq = 0;
        self.last_summary_seq = 0;
        self.sent_pending_approvals.clear();
        self.clear_active_stable_text();
        self.tool_turn = ToolTurnAggregation::default();
    }

    fn clear_active_stable_text(&mut self) {
        self.active_stable_text_entry_index = None;
        self.active_stable_text_message_id = None;
        self.active_stable_text_kind = None;
    }

    fn active_stable_text_message_id(&self) -> Option<MessageId> {
        self.active_stable_text_message_id
    }

    fn set_active_stable_text_message_id(&mut self, kind: StableTextKind, message_id: MessageId) {
        self.active_stable_text_entry_index = None;
        self.active_stable_text_kind = Some(kind);
        self.active_stable_text_message_id = Some(message_id);
    }

    fn track_active_stable_text_entry(&mut self, entry_index: usize, kind: StableTextKind) {
        self.active_stable_text_entry_index = Some(entry_index);
        self.active_stable_text_kind = Some(kind);
    }

    fn take_active_stable_text_message_id(&mut self) -> Option<MessageId> {
        let message_id = self.active_stable_text_message_id.take();
        self.active_stable_text_entry_index = None;
        self.active_stable_text_kind = None;
        message_id
    }

    fn flushable_active_stable_text(&self) -> Option<(StableTextKind, String)> {
        let entry_index = self.active_stable_text_entry_index?;
        let kind = self.active_stable_text_kind?;
        let entry = self.entries_by_index.get(&entry_index)?;
        if stable_text_kind_for_entry_type(&entry.entry_type) != Some(kind) {
            return None;
        }
        Some((kind, entry.content.clone()))
    }

    fn note_user_turn(&mut self, entry_index: usize) {
        self.tool_turn.current_user_turn_entry_index = Some(entry_index);
        self.reset_tool_turn_aggregation();
    }

    fn reset_tool_turn_aggregation(&mut self) {
        self.tool_turn.telegram_message_id = None;
        self.tool_turn.appended_terminal_tool_indexes.clear();
        self.tool_turn.rendered_rows.clear();
    }

    fn take_tool_turn_message_id(&mut self) -> Option<MessageId> {
        let message_id = self.tool_turn.telegram_message_id.take();
        self.tool_turn.appended_terminal_tool_indexes.clear();
        self.tool_turn.rendered_rows.clear();
        message_id
    }

    fn plan_terminal_tool_turn_update(
        &self,
        entry_index: usize,
        row: String,
        prefix: &str,
    ) -> ToolTurnUpdatePlan {
        if self
            .tool_turn
            .appended_terminal_tool_indexes
            .contains(&entry_index)
        {
            return ToolTurnUpdatePlan::SkipDuplicate;
        }

        let mut rows = self.tool_turn.rendered_rows.clone();
        rows.push(row.clone());
        let candidate = render_tool_turn_card(prefix, &rows);
        if candidate.chars().count() <= TELEGRAM_MESSAGE_LIMIT {
            return ToolTurnUpdatePlan::EditCurrent {
                rows,
                message: candidate,
            };
        }

        let replacement_rows = vec![row];
        ToolTurnUpdatePlan::ReplaceMessage {
            old_message_id: self.tool_turn.telegram_message_id,
            message: render_tool_turn_card(prefix, &replacement_rows),
            rows: replacement_rows,
        }
    }

    fn apply_terminal_tool_turn_update(
        &mut self,
        entry_index: usize,
        rows: Vec<String>,
        message_id: MessageId,
    ) {
        self.tool_turn
            .appended_terminal_tool_indexes
            .insert(entry_index);
        self.tool_turn.rendered_rows = rows;
        self.tool_turn.telegram_message_id = Some(message_id);
    }
}

fn classify_entry_behavior(entry: &NormalizedEntry) -> EntryDeliveryBehavior {
    match &entry.entry_type {
        NormalizedEntryType::Thinking => EntryDeliveryBehavior::RealtimeAndSummary,
        NormalizedEntryType::Loading => EntryDeliveryBehavior::Ignore,
        NormalizedEntryType::UserFeedback { .. } => EntryDeliveryBehavior::RealtimeOnly,
        NormalizedEntryType::ErrorMessage { .. } => EntryDeliveryBehavior::Ignore,
        NormalizedEntryType::NextAction { .. } => EntryDeliveryBehavior::RealtimeAndSummaryTrigger,
        NormalizedEntryType::ToolUse { status, .. } => match status {
            ToolStatus::PendingApproval { .. }
            | ToolStatus::Success
            | ToolStatus::Failed
            | ToolStatus::Denied { .. }
            | ToolStatus::TimedOut => EntryDeliveryBehavior::RealtimeAndSummary,
            ToolStatus::Created => EntryDeliveryBehavior::SummaryOnly,
        },
        NormalizedEntryType::UserMessage
        | NormalizedEntryType::SystemMessage
        | NormalizedEntryType::TokenUsageInfo(_) => EntryDeliveryBehavior::SummaryOnly,
        NormalizedEntryType::AssistantMessage => EntryDeliveryBehavior::RealtimeAndSummary,
    }
}

fn stable_text_kind_for_entry_type(entry_type: &NormalizedEntryType) -> Option<StableTextKind> {
    match entry_type {
        NormalizedEntryType::AssistantMessage => Some(StableTextKind::Assistant),
        NormalizedEntryType::Thinking => Some(StableTextKind::Thinking),
        _ => None,
    }
}

fn stable_text_kind_label(kind: StableTextKind) -> &'static str {
    match kind {
        StableTextKind::Assistant => "Assistant",
        StableTextKind::Thinking => "Thinking",
    }
}

fn stable_text_kind_emoji(kind: StableTextKind) -> &'static str {
    match kind {
        StableTextKind::Assistant => "🤖",
        StableTextKind::Thinking => "🤔",
    }
}

fn is_terminal_tool_status(status: &ToolStatus) -> bool {
    matches!(
        status,
        ToolStatus::Success | ToolStatus::Failed | ToolStatus::Denied { .. } | ToolStatus::TimedOut
    )
}

async fn start_or_refresh_run_feed_watcher(tg: TelegramContext, session_id: Uuid) {
    let watcher_id = Uuid::new_v4();
    let cancel = CancellationToken::new();

    {
        let mut watchers = run_feed_watchers().write().await;
        if let Some(existing) = watchers.insert(
            session_id,
            RunFeedWatcherHandle {
                watcher_id,
                cancel: cancel.clone(),
            },
        ) {
            existing.cancel.cancel();
        }
    }

    tokio::spawn(async move {
        run_feed_watcher_loop(tg, session_id, watcher_id, cancel.clone()).await;
        cleanup_run_feed_watcher(session_id, watcher_id).await;
    });
}

async fn stop_run_feed_watcher(session_id: Uuid) {
    let handle = { run_feed_watchers().write().await.remove(&session_id) };
    if let Some(handle) = handle {
        handle.cancel.cancel();
    }
}

async fn cleanup_run_feed_watcher(session_id: Uuid, watcher_id: Uuid) {
    let mut watchers = run_feed_watchers().write().await;
    if watchers
        .get(&session_id)
        .is_some_and(|handle| handle.watcher_id == watcher_id)
    {
        watchers.remove(&session_id);
    }
}

async fn run_feed_watcher_loop(
    tg: TelegramContext,
    session_id: Uuid,
    watcher_id: Uuid,
    cancel: CancellationToken,
) {
    let mut accumulator = RunFeedAccumulator::default();
    let mut watched_processes: HashSet<Uuid> = HashSet::new();

    loop {
        if cancel.is_cancelled() {
            emit_stage_summary(&tg, &mut accumulator, SummaryTrigger::TaskLeftInProgress).await;
            return;
        }

        let session = match db::models::session::Session::find_by_id(&tg.db.pool, session_id).await
        {
            Ok(Some(session)) => session,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!("run-feed watcher {watcher_id} failed to load session: {err}");
                return;
            }
        };

        let workspace = match Workspace::find_by_id(&tg.db.pool, session.workspace_id).await {
            Ok(Some(workspace)) => workspace,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!("run-feed watcher {watcher_id} failed to load workspace: {err}");
                return;
            }
        };

        let task = match Task::find_by_id(&tg.db.pool, workspace.task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!("run-feed watcher {watcher_id} failed to load task: {err}");
                return;
            }
        };

        accumulator.remember_session(session.id, workspace.id);
        accumulator.remember_task(task.id, task.project_id, &task.title);

        let mut flow_ctx =
            match flow::ensure_flow_context_for_session(&tg.db.pool, session.id, None).await {
                Ok(ctx) => ctx,
                Err(err) => {
                    tracing::warn!(
                        "run-feed watcher {watcher_id} failed to ensure flow context: {err}"
                    );
                    return;
                }
            };
        if !telegram_topics_enabled(&tg).await {
            flow_ctx.topic_thread_id = None;
        }
        accumulator.remember_flow_context(flow_ctx);

        let processes =
            match ExecutionProcess::find_by_session_id(&tg.db.pool, session_id, false).await {
                Ok(processes) => processes,
                Err(err) => {
                    tracing::warn!("run-feed watcher {watcher_id} failed to load processes: {err}");
                    return;
                }
            };
        let has_running = processes.iter().any(|p| {
            p.run_reason == ExecutionProcessRunReason::CodingAgent
                && p.status == ExecutionProcessStatus::Running
        });

        if !has_running {
            emit_stage_summary(&tg, &mut accumulator, SummaryTrigger::TaskLeftInProgress).await;
            return;
        }

        let Some(process) =
            find_running_coding_agent_process_for_session(&tg, session_id, &watched_processes)
                .await
        else {
            tokio::select! {
                _ = cancel.cancelled() => {
                    emit_stage_summary(&tg, &mut accumulator, SummaryTrigger::TaskLeftInProgress).await;
                    return;
                }
                _ = tokio::time::sleep(RUN_FEED_POLL_INTERVAL) => {}
            }
            continue;
        };

        watched_processes.insert(process.id);
        accumulator.remember_execution_process(process.id);
        if let Ok(mut flow_ctx) =
            flow::ensure_flow_context_for_session(&tg.db.pool, session_id, Some(process.id)).await
        {
            if !telegram_topics_enabled(&tg).await {
                flow_ctx.topic_thread_id = None;
            }
            accumulator.remember_flow_context(flow_ctx);
        }

        let Some(store) = wait_for_msg_store(&tg, process.id, &cancel).await else {
            tracing::warn!(
                "run-feed watcher {watcher_id}: no MsgStore for execution_process_id={}, skipping realtime feed notifications",
                process.id
            );
            return;
        };

        watch_execution_run_feed(&tg, &cancel, &mut accumulator, store).await;
        if cancel.is_cancelled() {
            return;
        }
        continue;
    }
}

async fn find_running_coding_agent_process_for_session(
    tg: &TelegramContext,
    session_id: Uuid,
    exclude: &HashSet<Uuid>,
) -> Option<ExecutionProcess> {
    let processes = ExecutionProcess::find_by_session_id(&tg.db.pool, session_id, false)
        .await
        .ok()?;
    processes
        .into_iter()
        .filter(|process| {
            process.run_reason == ExecutionProcessRunReason::CodingAgent
                && process.status == ExecutionProcessStatus::Running
                && !exclude.contains(&process.id)
        })
        .max_by_key(|process| process.created_at)
}

async fn wait_for_msg_store(
    tg: &TelegramContext,
    execution_process_id: Uuid,
    cancel: &CancellationToken,
) -> Option<Arc<MsgStore>> {
    for _ in 0..RUN_FEED_STORE_WAIT_ATTEMPTS {
        if cancel.is_cancelled() {
            return None;
        }

        if let Some(store) = tg
            .approvals
            .get_msg_store_for_execution_process(&execution_process_id)
            .await
        {
            return Some(store);
        }

        tokio::select! {
            _ = cancel.cancelled() => return None,
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
        }
    }

    None
}

async fn watch_execution_run_feed(
    tg: &TelegramContext,
    cancel: &CancellationToken,
    accumulator: &mut RunFeedAccumulator,
    store: Arc<MsgStore>,
) {
    let mut receiver = store.get_receiver();
    let history = store.get_history();
    let mut history_finished = false;

    for msg in history {
        match msg {
            LogMsg::JsonPatch(patch) => {
                let _ = process_run_feed_patch(tg, accumulator, patch, false).await;
            }
            LogMsg::Finished => {
                history_finished = true;
            }
            _ => {}
        }
    }

    emit_pending_approval_catch_up_cards(tg, accumulator).await;

    if history_finished {
        emit_stage_summary(tg, accumulator, SummaryTrigger::ExecutionFinished).await;
        return;
    }

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                emit_stage_summary(tg, accumulator, SummaryTrigger::TaskLeftInProgress).await;
                return;
            }
            msg = receiver.recv() => {
                match msg {
                    Ok(LogMsg::JsonPatch(patch)) => {
                        process_run_feed_patch(tg, accumulator, patch, true).await;
                    }
                    Ok(LogMsg::Finished) => {
                        emit_stage_summary(tg, accumulator, SummaryTrigger::ExecutionFinished).await;
                        return;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        emit_stage_summary(tg, accumulator, SummaryTrigger::ExecutionFinished).await;
                        return;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!("run-feed watcher skipped {skipped} log messages due to lag");
                    }
                }
            }
        }
    }
}

async fn process_run_feed_patch(
    tg: &TelegramContext,
    accumulator: &mut RunFeedAccumulator,
    patch: json_patch::Patch,
    emit_realtime: bool,
) {
    let Some(update) = accumulator.apply_patch(&patch) else {
        return;
    };

    if let Some(kind) = stable_text_kind_for_entry_type(&update.current.entry_type) {
        accumulator.track_active_stable_text_entry(update.index, kind);
        return;
    }

    if matches!(update.current.entry_type, NormalizedEntryType::UserMessage) {
        accumulator.note_user_turn(update.index);
    }

    if !emit_realtime {
        return;
    }

    flush_completed_stable_text_card(tg, accumulator).await;

    let behavior = classify_entry_behavior(&update.current);
    match behavior {
        EntryDeliveryBehavior::Ignore | EntryDeliveryBehavior::SummaryOnly => {}
        EntryDeliveryBehavior::RealtimeOnly => {
            emit_realtime_card_for_entry(tg, accumulator, &update).await;
        }
        EntryDeliveryBehavior::RealtimeAndSummary => {
            emit_realtime_card_for_entry(tg, accumulator, &update).await;
        }
        EntryDeliveryBehavior::RealtimeAndSummaryTrigger => {
            emit_realtime_card_for_entry(tg, accumulator, &update).await;
            emit_stage_summary(tg, accumulator, SummaryTrigger::NextAction).await;
        }
    }
}

async fn flush_completed_stable_text_card(
    tg: &TelegramContext,
    accumulator: &mut RunFeedAccumulator,
) {
    let Some((kind, content)) = accumulator.flushable_active_stable_text() else {
        return;
    };

    emit_realtime_stable_text_card(tg, accumulator, kind, &content).await;
}

async fn emit_pending_approval_catch_up_cards(
    tg: &TelegramContext,
    accumulator: &mut RunFeedAccumulator,
) {
    for entry in accumulator.entries_by_index.values() {
        if let NormalizedEntryType::ToolUse {
            tool_name, status, ..
        } = &entry.entry_type
            && let ToolStatus::PendingApproval { approval_id, .. } = status
            && accumulator
                .sent_pending_approvals
                .insert(approval_id.clone())
        {
            send_tool_pending_approval_card(
                tg,
                accumulator.topic_thread_id(),
                approval_id,
                tool_name,
                &entry.content,
            )
            .await;
        }
    }
}

async fn emit_terminal_tool_turn_card(
    tg: &TelegramContext,
    accumulator: &mut RunFeedAccumulator,
    entry_index: usize,
    tool_name: &str,
    status: &ToolStatus,
    action_type: &ActionType,
) {
    let row = render_terminal_tool_row(tool_name, status, action_type);
    let prefix = accumulator.flow_prefix();
    match accumulator.plan_terminal_tool_turn_update(entry_index, row, &prefix) {
        ToolTurnUpdatePlan::SkipDuplicate => {}
        ToolTurnUpdatePlan::EditCurrent { rows, message } => {
            match format::edit_or_send_rich_then_plain_to_thread(
                &tg.bot,
                tg.chat_id,
                accumulator.topic_thread_id(),
                accumulator.tool_turn.telegram_message_id,
                &message,
                None,
            )
            .await
            {
                Ok(message) => {
                    accumulator.apply_terminal_tool_turn_update(entry_index, rows, message.id);
                }
                Err(err) => {
                    tracing::warn!("Failed to upsert telegram tool turn card in topic: {err}");
                    match format::edit_or_send_rich_then_plain(
                        &tg.bot,
                        tg.chat_id,
                        accumulator.tool_turn.telegram_message_id,
                        &message,
                        None,
                    )
                    .await
                    {
                        Ok(message) => {
                            accumulator.apply_terminal_tool_turn_update(
                                entry_index,
                                rows,
                                message.id,
                            );
                        }
                        Err(err) => {
                            tracing::warn!("Failed to upsert telegram tool turn card: {err}")
                        }
                    }
                }
            }
        }
        ToolTurnUpdatePlan::ReplaceMessage {
            old_message_id,
            rows,
            message,
        } => {
            if let Some(message_id) = old_message_id
                && let Err(err) = tg.bot.delete_message(tg.chat_id, message_id).await
            {
                tracing::debug!(
                    "Failed to delete Telegram tool turn message {} in chat {}: {}",
                    message_id.0,
                    tg.chat_id.0,
                    err
                );
            }

            match format::edit_or_send_rich_then_plain_to_thread(
                &tg.bot,
                tg.chat_id,
                accumulator.topic_thread_id(),
                None,
                &message,
                None,
            )
            .await
            {
                Ok(message) => {
                    accumulator.apply_terminal_tool_turn_update(entry_index, rows, message.id);
                }
                Err(err) => {
                    tracing::warn!("Failed to upsert telegram tool turn card in topic: {err}");
                    match format::edit_or_send_rich_then_plain(
                        &tg.bot, tg.chat_id, None, &message, None,
                    )
                    .await
                    {
                        Ok(message) => {
                            accumulator.apply_terminal_tool_turn_update(
                                entry_index,
                                rows,
                                message.id,
                            );
                        }
                        Err(err) => {
                            tracing::warn!("Failed to upsert telegram tool turn card: {err}")
                        }
                    }
                }
            }
        }
    }
}

async fn emit_realtime_stable_text_card(
    tg: &TelegramContext,
    accumulator: &mut RunFeedAccumulator,
    kind: StableTextKind,
    content: &str,
) {
    let prefix = accumulator.flow_prefix();
    let label = stable_text_kind_label(kind);
    let emoji = stable_text_kind_emoji(kind);
    let message = format!(
        "{emoji} {prefix} {label}\n{}",
        truncate_for_telegram(content, TELEGRAM_MESSAGE_LIMIT * 2)
    );
    let source_message_id = accumulator.active_stable_text_message_id();
    match format::edit_or_send_rich_then_plain_to_thread(
        &tg.bot,
        tg.chat_id,
        accumulator.topic_thread_id(),
        source_message_id,
        &message,
        None,
    )
    .await
    {
        Ok(message) => accumulator.set_active_stable_text_message_id(kind, message.id),
        Err(err) => {
            tracing::warn!("Failed to upsert telegram stable text card: {err}");
        }
    }
}

fn render_tool_turn_card(prefix: &str, rows: &[String]) -> String {
    let mut message = format!("🛠️ {prefix} Tool calls");
    for row in rows {
        message.push('\n');
        message.push_str(row);
    }
    message
}

fn render_terminal_tool_row(
    tool_name: &str,
    status: &ToolStatus,
    action_type: &ActionType,
) -> String {
    let status_label = terminal_tool_status_label(status);
    let action_preview = truncate_for_telegram(&action_type_summary(action_type), 120);
    format!("{status_label} {tool_name}: {action_preview}")
}

fn terminal_tool_status_label(status: &ToolStatus) -> &'static str {
    match status {
        ToolStatus::Success => "✅",
        ToolStatus::Failed => "❌",
        ToolStatus::Denied { .. } => "🛑",
        ToolStatus::TimedOut => "⏱️",
        ToolStatus::Created | ToolStatus::PendingApproval { .. } => "•",
    }
}

fn action_type_summary(action_type: &ActionType) -> String {
    match action_type {
        ActionType::FileRead { path } => format!("read {path}"),
        ActionType::FileEdit { path, changes } => {
            format!("edit {path} ({} change sets)", changes.len())
        }
        ActionType::CommandRun { command, .. } => format!("run {command}"),
        ActionType::Search { query } => format!("search {query}"),
        ActionType::WebFetch { url } => format!("fetch {url}"),
        ActionType::Tool {
            tool_name,
            arguments,
            ..
        } => arguments
            .as_ref()
            .map(|arguments| format!("{tool_name} {arguments}"))
            .unwrap_or_else(|| tool_name.clone()),
        ActionType::TaskCreate { description } => format!("task {description}"),
        ActionType::PlanPresentation { plan } => format!("plan {plan}"),
        ActionType::TodoManagement { operation, todos } => {
            format!("todo {operation} ({} items)", todos.len())
        }
        ActionType::Other { description } => description.clone(),
    }
}

async fn delete_active_stable_text_message(
    tg: &TelegramContext,
    accumulator: &mut RunFeedAccumulator,
) {
    let Some(message_id) = accumulator.take_active_stable_text_message_id() else {
        return;
    };

    if let Err(err) = tg.bot.delete_message(tg.chat_id, message_id).await {
        tracing::debug!(
            "Failed to delete Telegram assistant message {} in chat {}: {}",
            message_id.0,
            tg.chat_id.0,
            err
        );
    }
}

async fn delete_tool_turn_message(tg: &TelegramContext, accumulator: &mut RunFeedAccumulator) {
    let Some(message_id) = accumulator.take_tool_turn_message_id() else {
        return;
    };

    if let Err(err) = tg.bot.delete_message(tg.chat_id, message_id).await {
        tracing::debug!(
            "Failed to delete Telegram tool turn message {} in chat {}: {}",
            message_id.0,
            tg.chat_id.0,
            err
        );
    }
}

async fn emit_realtime_card_for_entry(
    tg: &TelegramContext,
    accumulator: &mut RunFeedAccumulator,
    update: &EntryUpdate,
) {
    match &update.current.entry_type {
        NormalizedEntryType::UserFeedback { denied_tool } => {
            let prefix = accumulator.flow_prefix();
            let message = format!(
                "🚫 {prefix} User rejected tool: {}\n{}",
                denied_tool,
                truncate_for_telegram(&update.current.content, 220)
            );
            let _ = send_telegram_card_to_thread(tg, accumulator.topic_thread_id(), message, None)
                .await;
        }
        NormalizedEntryType::ToolUse {
            tool_name,
            action_type,
            status,
        } => match status {
            ToolStatus::PendingApproval { approval_id, .. } => {
                if should_emit_pending_approval_card(&update.previous, approval_id)
                    && accumulator
                        .sent_pending_approvals
                        .insert(approval_id.clone())
                {
                    send_tool_pending_approval_card(
                        tg,
                        accumulator.topic_thread_id(),
                        approval_id,
                        tool_name,
                        &update.current.content,
                    )
                    .await;
                }
            }
            ToolStatus::Created => {}
            status if is_terminal_tool_status(status) => {
                emit_terminal_tool_turn_card(
                    tg,
                    accumulator,
                    update.index,
                    tool_name,
                    status,
                    action_type,
                )
                .await;
            }
            _ => {}
        },
        NormalizedEntryType::NextAction {
            failed,
            execution_processes,
            needs_setup,
        } => {
            let message = format!(
                "🔁 {} Stage changed\nfailed: {}\nexecution_processes: {}\nneeds_setup: {}",
                accumulator.flow_prefix(),
                failed,
                execution_processes,
                needs_setup
            );
            let _ = send_telegram_card_to_thread(tg, accumulator.topic_thread_id(), message, None)
                .await;
        }
        _ => {}
    }
}

fn should_emit_pending_approval_card(
    previous: &Option<NormalizedEntry>,
    approval_id: &str,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };

    if let NormalizedEntryType::ToolUse {
        status:
            ToolStatus::PendingApproval {
                approval_id: previous_approval_id,
                ..
            },
        ..
    } = &previous.entry_type
    {
        return previous_approval_id != approval_id;
    }

    true
}

fn should_send_tool_pending_approval_card(tool_name: &str) -> bool {
    tool_name != EXIT_PLAN_MODE_NAME
}

async fn send_tool_pending_approval_card(
    tg: &TelegramContext,
    thread_id: Option<ThreadId>,
    approval_id: &str,
    tool_name: &str,
    content: &str,
) {
    if !should_send_tool_pending_approval_card(tool_name) {
        return;
    }

    let structured_input = tg
        .approvals
        .pending_by_id(approval_id)
        .is_some_and(|approval| {
            approval
                .tool_name
                .eq_ignore_ascii_case(REQUEST_USER_INPUT_TOOL_NAME)
        });

    let request_preview = truncate_for_telegram(content, 200);
    if structured_input {
        let message = format!(
            "🛑 Need approval for {tool_name}\n{}\nThis request needs structured input. Please handle it in Web UI.",
            request_preview
        );
        let _ = send_telegram_card_to_thread(tg, thread_id, message, None).await;
        return;
    }

    let message = format!("🛑 Need approval for {tool_name}\n{request_preview}");
    let _ = send_telegram_card_to_thread(
        tg,
        thread_id,
        message,
        Some(keyboard::tool_approval_keyboard(approval_id)),
    )
    .await;
}

async fn emit_stage_summary(
    tg: &TelegramContext,
    accumulator: &mut RunFeedAccumulator,
    trigger: SummaryTrigger,
) {
    delete_active_stable_text_message(tg, accumulator).await;
    delete_tool_turn_message(tg, accumulator).await;

    if should_skip_stage_summary_in_plan_mode(tg, accumulator.execution_process_id).await {
        accumulator.finalize_stage();
        return;
    }

    let changed = accumulator.collect_summary_data(true);
    let overall = accumulator.collect_summary_data(false);

    let model_latest = changed.model_latest.or(overall.model_latest);
    let token_usage = changed.token_usage.or(overall.token_usage);

    if changed.assistant_messages.is_empty() {
        let _ = append_lark_wiki_stage_summary(tg, accumulator, trigger).await;
        if matches!(trigger, SummaryTrigger::ExecutionFinished)
            && let Some(session_id) = accumulator.session_id
        {
            delete_running_messages_for_session(tg, session_id).await;
        }
        accumulator.finalize_stage();
        return;
    }

    let prefix = accumulator.flow_prefix();
    let mut lines = vec![format!("🧾 {prefix} Stage summary")];
    lines.push(format!(
        "Task title: {}",
        accumulator
            .task_title
            .as_deref()
            .map(|title| truncate_for_telegram(title, STAGE_SUMMARY_TASK_TITLE_MAX_CHARS))
            .unwrap_or_else(|| "n/a".to_string())
    ));

    append_stage_summary_assistant_lines(&mut lines, &changed.assistant_messages);

    if let Some(model) = model_latest {
        lines.push(format!(
            "Model info: {}",
            truncate_for_telegram(&model, STAGE_SUMMARY_MODEL_MAX_CHARS)
        ));
    } else {
        lines.push("Model info: n/a".to_string());
    }

    append_token_usage_line(&mut lines, token_usage.as_ref());

    let telegraph_urls = append_telegraph_stage_summary(tg, accumulator).await;
    let lark_wiki_url = append_lark_wiki_stage_summary(tg, accumulator, trigger).await;
    append_telegraph_log_lines(&mut lines, &telegraph_urls);
    append_lark_wiki_log_line(&mut lines, lark_wiki_url.as_deref());

    let is_daily_task = is_daily_project_task(tg, accumulator.task_project_id).await;
    let summary_markup = stage_summary_keyboard(
        trigger,
        accumulator
            .flow_context
            .as_ref()
            .map(|ctx| ctx.flow_token.clone()),
        is_daily_task,
        None, // msg_id not known yet; will be filled after send
    );
    let sent_ids = send_split_telegram_card(
        tg,
        accumulator.topic_thread_id(),
        lines.join("\n"),
        summary_markup,
    )
    .await;
    if matches!(trigger, SummaryTrigger::ExecutionFinished)
        && !sent_ids.is_empty()
        && let Some(session_id) = accumulator.session_id
    {
        delete_running_messages_for_session(tg, session_id).await;
    }

    // If we have a flow token and at least one sent message, retroactively edit the
    // last card to include the correct msg_id in the Audio callback button.
    if let Some(flow_token) = accumulator
        .flow_context
        .as_ref()
        .map(|ctx| ctx.flow_token.clone())
        && let Some(last_msg_id) = sent_ids.last().copied()
        && matches!(
            trigger,
            SummaryTrigger::ExecutionFinished | SummaryTrigger::TaskLeftInProgress
        )
    {
        let updated_markup = stage_summary_keyboard(
            trigger,
            Some(flow_token.clone()),
            is_daily_task,
            Some(last_msg_id.0),
        );
        if let Some(markup) = updated_markup {
            let _ = tg
                .bot
                .edit_message_reply_markup(tg.chat_id, last_msg_id)
                .reply_markup(markup)
                .await;
        }

        // Cache the assistant text for the Audio handler to use.
        if let Some(tts_text) = stage_summary_assistant_text(&changed.assistant_messages) {
            tts_text_cache().insert((flow_token, last_msg_id.0), tts_text);
        }
    }

    accumulator.finalize_stage();
}

async fn append_lark_wiki_stage_summary(
    tg: &TelegramContext,
    accumulator: &RunFeedAccumulator,
    trigger: SummaryTrigger,
) -> Option<String> {
    if !matches!(
        trigger,
        SummaryTrigger::ExecutionFinished | SummaryTrigger::TaskLeftInProgress
    ) {
        return None;
    }

    let execution_process_id = accumulator.execution_process_id?;
    let entries: Vec<NormalizedEntry> = accumulator
        .iter_entries_for_summary(true)
        .into_iter()
        .map(|(_, entry)| entry)
        .collect();
    if entries.is_empty() {
        return lark_wiki::get_execution_process_lark_wiki_url(&tg.db.pool, &execution_process_id)
            .await;
    }

    let telegram_config = tg.config.read().await.telegram.clone();

    match lark_wiki::append_stage_summary_markdown(
        &tg.db.pool,
        execution_process_id,
        &telegram_config,
        entries,
    )
    .await
    {
        Some(url) => Some(url),
        None => {
            lark_wiki::get_execution_process_lark_wiki_url(&tg.db.pool, &execution_process_id).await
        }
    }
}

async fn append_telegraph_stage_summary(
    tg: &TelegramContext,
    accumulator: &RunFeedAccumulator,
) -> Vec<String> {
    let Some(execution_process_id) = accumulator.execution_process_id else {
        return Vec::new();
    };
    let entries = accumulator
        .iter_entries_for_summary(true)
        .into_iter()
        .map(|(_, entry)| entry)
        .collect();
    let telegram_config = tg.config.read().await.telegram.clone();

    let urls = telegraph::append_stage_summary_pages(
        &tg.db.pool,
        execution_process_id,
        &telegram_config,
        entries,
    )
    .await;
    if urls.is_empty() {
        telegraph::get_execution_process_telegraph_urls(&tg.db.pool, &execution_process_id).await
    } else {
        urls
    }
}

async fn should_skip_stage_summary_in_plan_mode(
    tg: &TelegramContext,
    execution_process_id: Option<Uuid>,
) -> bool {
    let Some(execution_process_id) = execution_process_id else {
        return false;
    };

    find_exit_plan_approval_for_execution_process(tg, execution_process_id)
        .await
        .is_some()
}

fn append_stage_summary_assistant_lines(lines: &mut Vec<String>, assistant_messages: &[String]) {
    let Some(latest_assistant) = assistant_messages.last() else {
        return;
    };

    lines.push(latest_assistant.clone());
}

fn append_telegraph_log_lines(lines: &mut Vec<String>, telegraph_urls: &[String]) {
    match telegraph_urls {
        [] => {}
        [url] => lines.push(format!("Telegraph log: {url}")),
        urls => {
            lines.push("Telegraph logs:".to_string());
            for (page_index, url) in urls.iter().enumerate() {
                lines.push(format!("{}. {url}", page_index + 1));
            }
        }
    }
}

fn append_lark_wiki_log_line(lines: &mut Vec<String>, lark_wiki_url: Option<&str>) {
    if let Some(url) = lark_wiki_url.filter(|url| !url.trim().is_empty()) {
        lines.push(format!("Lark wiki log: {url}"));
    }
}

fn append_token_usage_line(lines: &mut Vec<String>, token_usage: Option<&TokenUsageInfo>) {
    if let Some(usage) = token_usage {
        if usage.model_context_window > 0 {
            lines.push(format!(
                "Token usage: total={} / context={}",
                usage.total_tokens, usage.model_context_window
            ));
        } else {
            lines.push(format!("Token usage: total={}", usage.total_tokens));
        }
    } else {
        lines.push("Token usage: n/a".to_string());
    }
}

/// Extract the latest assistant text from `assistant_messages` for TTS synthesis.
/// Returns `None` when the message list is empty or the last message is blank.
pub fn stage_summary_assistant_text(assistant_messages: &[String]) -> Option<String> {
    let text = assistant_messages.last()?.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Handle "🔊 Audio" button: start async TTS synthesis for the stage summary card.
pub async fn handle_stage_summary_audio(
    tg: &TelegramContext,
    flow_token: &str,
    source_msg_id: i32,
) {
    use super::{audio_jobs, keyboard};
    use crate::services::tts;

    let key = audio_jobs::AudioJobKey {
        flow_token: flow_token.to_string(),
        source_message_id: MessageId(source_msg_id),
    };

    // If a job is already running, do nothing (button should have been swapped to Stop).
    if audio_jobs::contains(&key).await {
        return;
    }

    // Retrieve config and resolve TTS text from the card message content.
    // We synthesise the flow_token itself as a fallback if we can't look up the card text.
    let tts_config = {
        let cfg = tg.config.read().await;
        cfg.tts.clone()
    };

    // Check TTS is configured before touching the keyboard.
    if tts::resolve_replicate_token(&tts_config).is_none() {
        if let Err(err) = tg
            .bot
            .send_message(
                tg.chat_id,
                "TTS not configured: Replicate API token is missing.",
            )
            .await
        {
            tracing::warn!("Failed to send TTS not-configured notice: {err}");
        }
        return;
    }

    // Swap button to Stop immediately.
    let stop_markup = keyboard::stage_summary_audio_running_keyboard(flow_token, source_msg_id);
    let _ = tg
        .bot
        .edit_message_reply_markup(tg.chat_id, MessageId(source_msg_id))
        .reply_markup(stop_markup)
        .await;

    // Try to fetch the card message text to use as TTS input.
    // Look up the cache populated at card-send time; fall back to a descriptive placeholder.
    let tts_text: String = tts_text_cache()
        .get(&(flow_token.to_string(), source_msg_id))
        .map(|v| v.clone())
        .unwrap_or_else(|| format!("Stage summary for flow {flow_token}"));

    // Register the job.
    let cancel = audio_jobs::insert(key.clone()).await;

    // Spawn async synthesis.
    let tg = tg.clone();
    let flow_token = flow_token.to_string();
    let key_clone = key.clone();
    tokio::spawn(async move {
        let result = tts::synthesize(&tts_config, &tts_text, cancel.clone()).await;

        match result {
            Ok(output) => {
                // Apply speed adjustment via FFmpeg if needed.
                let speed = tts_config.speed;
                let (send_path, speed_path) = match tts::apply_speed(&output.file_path, speed).await
                {
                    Ok(p) if p != output.file_path => (p.clone(), Some(p)),
                    Ok(p) => (p, None),
                    Err(err) => {
                        tracing::warn!("FFmpeg speed adjustment failed, using original: {err}");
                        (output.file_path.clone(), None)
                    }
                };

                // Send audio as voice reply to the source message.
                use teloxide::types::{InputFile, ReplyParameters};
                let voice = InputFile::file(send_path.clone());
                let send_result = tg
                    .bot
                    .send_voice(tg.chat_id, voice)
                    .reply_parameters(ReplyParameters::new(MessageId(source_msg_id)))
                    .await;

                // Clean up temp files.
                let _ = tokio::fs::remove_file(&output.file_path).await;
                if let Some(sp) = speed_path {
                    let _ = tokio::fs::remove_file(&sp).await;
                }

                if let Err(err) = send_result {
                    tracing::warn!("Failed to send TTS voice message: {err}");
                }
            }
            Err(err) => {
                tracing::warn!("TTS synthesis failed: {err}");
            }
        }

        // Remove job from registry.
        audio_jobs::remove(&key_clone).await;

        // Restore the stage summary keyboard after audio completes, fails, or is cancelled.
        let is_daily_task = match flow::resolve_flow_context(&tg.db.pool, &flow_token).await {
            Ok(Some(flow_ctx)) => {
                let task_project_id = match Task::find_by_id(&tg.db.pool, flow_ctx.task_id).await {
                    Ok(Some(task)) => Some(task.project_id),
                    Ok(None) => None,
                    Err(err) => {
                        tracing::warn!(
                            "Failed to load task when restoring stage summary keyboard: {err}"
                        );
                        None
                    }
                };
                is_daily_project_task(&tg, task_project_id).await
            }
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(
                    "Failed to resolve flow context when restoring stage summary keyboard: {err}"
                );
                false
            }
        };
        let audio_markup =
            stage_summary_reply_keyboard_for_task(&flow_token, source_msg_id, is_daily_task);
        let _ = tg
            .bot
            .edit_message_reply_markup(tg.chat_id, MessageId(source_msg_id))
            .reply_markup(audio_markup)
            .await;
    });
}

/// Handle "⏹ Stop" button: send a confirmation prompt.
pub async fn handle_stage_summary_stop_audio(
    tg: &TelegramContext,
    flow_token: &str,
    source_msg_id: i32,
) {
    use super::keyboard;
    let confirm_markup = keyboard::stage_summary_stop_confirm_keyboard(flow_token, source_msg_id);
    let _ = tg
        .bot
        .send_message(tg.chat_id, "Stop audio synthesis?")
        .reply_markup(confirm_markup)
        .await;
}

/// Handle "✅ Yes, stop" button: cancel the running job, delete the confirm message.
pub async fn handle_stage_summary_stop_audio_confirm(
    tg: &TelegramContext,
    flow_token: &str,
    source_msg_id: i32,
    confirm_msg_id: MessageId,
) {
    use super::audio_jobs;

    let key = audio_jobs::AudioJobKey {
        flow_token: flow_token.to_string(),
        source_message_id: MessageId(source_msg_id),
    };

    // Cancel the job (will trigger Cancelled error in the synthesis task).
    if let Some(job) = audio_jobs::remove(&key).await {
        job.cancel.cancel();
    }

    // Delete the confirmation message.
    let _ = tg.bot.delete_message(tg.chat_id, confirm_msg_id).await;
}

async fn delete_running_messages_for_session(tg: &TelegramContext, session_id: Uuid) {
    let message_ids = take_running_message_ids(session_id).await;
    for message_id in message_ids {
        if let Err(err) = tg.bot.delete_message(tg.chat_id, message_id).await {
            tracing::debug!(
                "Failed to delete Telegram running message {} in chat {}: {}",
                message_id.0,
                tg.chat_id.0,
                err
            );
        }
    }
}

fn stage_summary_keyboard(
    trigger: SummaryTrigger,
    flow_token: Option<String>,
    is_daily_task: bool,
    msg_id: Option<i32>,
) -> Option<teloxide::types::InlineKeyboardMarkup> {
    if !matches!(
        trigger,
        SummaryTrigger::ExecutionFinished | SummaryTrigger::TaskLeftInProgress
    ) {
        return None;
    }

    let flow_token = flow_token?;
    let card_msg_id = msg_id.unwrap_or(0);
    Some(stage_summary_reply_keyboard_for_task(
        &flow_token,
        card_msg_id,
        is_daily_task,
    ))
}

fn stage_summary_reply_keyboard_for_task(
    flow_token: &str,
    msg_id: i32,
    is_daily_task: bool,
) -> teloxide::types::InlineKeyboardMarkup {
    if is_daily_task {
        keyboard::stage_summary_reply_done_keyboard(flow_token, msg_id)
    } else {
        keyboard::stage_summary_reply_keyboard(flow_token, msg_id)
    }
}

async fn is_daily_project_task(tg: &TelegramContext, task_project_id: Option<Uuid>) -> bool {
    let configured_daily_project_id = {
        let config = tg.config.read().await;
        config.daily_mode.project_id.clone()
    };

    is_configured_daily_project_task(task_project_id, configured_daily_project_id.as_deref())
}

async fn telegram_topics_enabled(tg: &TelegramContext) -> bool {
    tg.config.read().await.telegram.topic_enabled
}

async fn send_task_topic_close_prompt(tg: &TelegramContext, task_id: Uuid, task_title: &str) {
    if !telegram_topics_enabled(tg).await {
        return;
    }

    let message = format!("Task {task_title} is Done. Close this topic?");
    let _ = send_task_telegram_card(
        tg,
        task_id,
        message,
        Some(keyboard::close_task_topic_keyboard(task_id)),
    )
    .await;
}

fn should_send_task_finished_notification(
    task_project_id: Option<Uuid>,
    configured_daily_project_id: Option<&str>,
) -> bool {
    !is_configured_daily_project_task(task_project_id, configured_daily_project_id)
}

fn is_configured_daily_project_task(
    task_project_id: Option<Uuid>,
    configured_daily_project_id: Option<&str>,
) -> bool {
    let Some(task_project_id) = task_project_id else {
        return false;
    };
    let Some(configured_daily_project_id) = configured_daily_project_id else {
        return false;
    };

    match Uuid::parse_str(configured_daily_project_id) {
        Ok(daily_project_id) => task_project_id == daily_project_id,
        Err(err) => {
            tracing::warn!(
                "Invalid daily_mode.project_id in config: {} ({})",
                configured_daily_project_id,
                err
            );
            false
        }
    }
}

async fn send_task_telegram_card(
    tg: &TelegramContext,
    task_id: Uuid,
    message: String,
    markup: Option<teloxide::types::InlineKeyboardMarkup>,
) -> Option<MessageId> {
    let thread_id = if telegram_topics_enabled(tg).await {
        match topic::find_task_topic(&tg.db.pool, task_id).await {
            Ok(Some(topic)) => Some(topic.thread_id),
            Ok(None) => None,
            Err(err) => {
                tracing::debug!(task_id = %task_id, "Failed to look up Telegram task topic: {err}");
                None
            }
        }
    } else {
        None
    };

    send_telegram_card_to_thread(tg, thread_id, message, markup).await
}

async fn send_telegram_card_to_thread(
    tg: &TelegramContext,
    thread_id: Option<ThreadId>,
    message: String,
    markup: Option<teloxide::types::InlineKeyboardMarkup>,
) -> Option<MessageId> {
    match format::send_rich_then_plain_to_thread(
        &tg.bot,
        tg.chat_id,
        thread_id,
        &message,
        markup.clone(),
    )
    .await
    {
        Ok(message) => Some(message.id),
        Err(err) => {
            if thread_id.is_some() {
                tracing::warn!(
                    "Failed to send telegram run-feed card to topic, falling back to main chat: {err}"
                );
                match format::send_rich_then_plain(&tg.bot, tg.chat_id, &message, markup).await {
                    Ok(message) => Some(message.id),
                    Err(fallback_err) => {
                        tracing::warn!("Failed to send telegram run-feed card: {fallback_err}");
                        None
                    }
                }
            } else {
                tracing::warn!("Failed to send telegram run-feed card: {err}");
                None
            }
        }
    }
}

async fn send_split_telegram_card(
    tg: &TelegramContext,
    thread_id: Option<ThreadId>,
    message: String,
    markup_last: Option<teloxide::types::InlineKeyboardMarkup>,
) -> Vec<MessageId> {
    let chunks = format::split_telegram_chunks(
        &message,
        TELEGRAM_MESSAGE_LIMIT,
        TELEGRAM_CHUNK_INDEX_PREFIX_RESERVE,
    );

    if chunks.len() <= 1 {
        return send_telegram_card_to_thread(tg, thread_id, message, markup_last)
            .await
            .into_iter()
            .collect();
    }

    let mut ids = Vec::new();
    let total = chunks.len();
    for (index, chunk) in chunks.into_iter().enumerate() {
        let markup = if index + 1 == total {
            markup_last.clone()
        } else {
            None
        };
        if let Some(message_id) = send_telegram_card_to_thread(
            tg,
            thread_id,
            format!("[{}/{}]\n{}", index + 1, total, chunk),
            markup,
        )
        .await
        {
            ids.push(message_id);
        }
    }

    ids
}

fn truncate_for_telegram(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }

    let truncated: String = input.chars().take(max_chars).collect();
    format!("{truncated}…")
}

fn extract_model_related_info(system_message: &str) -> Option<String> {
    let trimmed = system_message.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    if let Some(index) = lower.find("model:") {
        return Some(trimmed[index..].trim().to_string());
    }

    None
}

/// helper function

async fn compute_workspace_diff_stats(
    tg: &TelegramContext,
    task_id: uuid::Uuid,
) -> Option<(usize, usize)> {
    let workspaces = Workspace::fetch_all(&tg.db.pool, Some(task_id))
        .await
        .ok()?;
    let workspace = workspaces.into_iter().next()?;
    let container_ref = workspace.container_ref.as_ref()?;

    let workspace_repos =
        WorkspaceRepo::find_repos_with_target_branch_for_workspace(&tg.db.pool, workspace.id)
            .await
            .ok()?;

    let mut total_added = 0usize;
    let mut total_removed = 0usize;

    for repo_with_branch in workspace_repos {
        let worktree_path = PathBuf::from(container_ref).join(&repo_with_branch.repo.name);
        let workspace_branch = workspace.branch.clone();
        let target_branch = repo_with_branch.target_branch.clone();

        let base_commit = tg
            .git
            .get_base_commit(
                &repo_with_branch.repo.path,
                &workspace_branch,
                &target_branch,
            )
            .ok()?;

        let diffs = tg
            .git
            .get_diffs(
                crate::services::git::DiffTarget::Worktree {
                    worktree_path: &worktree_path,
                    base_commit: &base_commit,
                },
                None,
            )
            .ok()?;

        for diff in diffs {
            total_added += diff.additions.unwrap_or(0);
            total_removed += diff.deletions.unwrap_or(0);
        }
    }

    Some((total_added, total_removed))
}

struct PlanApproval {
    execution_process_id: Uuid,
    plan: String,
}

const TELEGRAM_MESSAGE_LIMIT: usize = format::TELEGRAM_MESSAGE_LIMIT;
const TELEGRAM_CHUNK_INDEX_PREFIX_RESERVE: usize = 20;
const STAGE_SUMMARY_TASK_TITLE_MAX_CHARS: usize = 180;
const STAGE_SUMMARY_MODEL_MAX_CHARS: usize = 280;

fn split_plan(plan: &str) -> Vec<String> {
    if plan.is_empty() {
        return vec![];
    }

    // Split into sections at heading boundaries
    let mut sections: Vec<String> = Vec::new();
    let mut current = String::new();

    for line in plan.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') && !current.is_empty() {
            sections.push(current);
            current = String::new();
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        sections.push(current);
    }

    // "Plan Review: [99/99] " is 21 chars; reserve 25 to be safe for any count.
    let prefix_reserve = 25;
    let limit = TELEGRAM_MESSAGE_LIMIT.saturating_sub(prefix_reserve);

    let mut chunks: Vec<String> = Vec::new();
    let mut buf = String::new();

    for section in &sections {
        let needed = if buf.is_empty() {
            section.chars().count()
        } else {
            buf.chars().count() + 1 /* newline */ + section.chars().count()
        };

        if !buf.is_empty() && needed > limit {
            chunks.push(buf);
            buf = String::new();
        }

        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(section);
    }
    if !buf.is_empty() {
        chunks.push(buf);
    }

    // Keep heading-aware grouping, then enforce hard message limits safely.
    let chunks: Vec<String> = chunks
        .into_iter()
        .flat_map(|chunk| {
            format::split_telegram_chunks(&chunk, TELEGRAM_MESSAGE_LIMIT, prefix_reserve)
        })
        .collect();

    if chunks.len() <= 1 {
        return chunks;
    }

    let total = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, c)| format!("Plan Review: [{}/{}] {}", i + 1, total, c))
        .collect()
}

async fn find_exit_plan_approval(
    tg: &TelegramContext,
    task_id: uuid::Uuid,
) -> Option<PlanApproval> {
    for approval in tg.approvals.list_pending() {
        if approval.tool_name != EXIT_PLAN_MODE_NAME {
            continue;
        }

        let ctx = ExecutionProcess::load_context(&tg.db.pool, approval.execution_process_id).await;
        if let Ok(ctx) = ctx
            && ctx.task.id == task_id
        {
            return Some(PlanApproval {
                execution_process_id: approval.execution_process_id,
                plan: approval.entry.content,
            });
        }
    }

    None
}

async fn find_exit_plan_approval_for_execution_process(
    tg: &TelegramContext,
    execution_process_id: Uuid,
) -> Option<PlanApproval> {
    for approval in tg.approvals.list_pending() {
        if approval.tool_name != EXIT_PLAN_MODE_NAME {
            continue;
        }
        if approval.execution_process_id != execution_process_id {
            continue;
        }

        return Some(PlanApproval {
            execution_process_id,
            plan: approval.entry.content,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use executors::logs::{
        ActionType, NormalizedEntry, NormalizedEntryError, NormalizedEntryType, ToolStatus,
    };
    use serde_json::Value;

    use super::*;
    use crate::services::telegram::callback::CallbackAction;

    #[test]
    fn split_plan_empty() {
        assert!(split_plan("").is_empty());
    }

    #[test]
    fn exit_plan_mode_does_not_send_pending_approval_card() {
        assert!(!should_send_tool_pending_approval_card(EXIT_PLAN_MODE_NAME));
        assert!(should_send_tool_pending_approval_card("Bash"));
    }

    #[test]
    fn plan_review_message_registry_records_replaces_and_takes() {
        let execution_process_id = Uuid::new_v4();
        let mut registry = PlanReviewMessageRegistry::default();

        registry.record(execution_process_id, vec![11, 22]);
        assert_eq!(
            registry.by_execution_process.get(&execution_process_id),
            Some(&vec![11, 22])
        );

        registry.record(execution_process_id, vec![33]);
        assert_eq!(
            registry.by_execution_process.get(&execution_process_id),
            Some(&vec![33])
        );

        assert_eq!(registry.take(execution_process_id), vec![33]);
        assert!(registry.take(execution_process_id).is_empty());
    }

    #[test]
    fn extract_model_related_info_reads_model_from_system_message() {
        assert_eq!(
            extract_model_related_info(
                "system initialized; model: gpt-5.2 reasoning effort: medium"
            )
            .as_deref(),
            Some("model: gpt-5.2 reasoning effort: medium")
        );
    }

    #[test]
    fn extract_model_related_info_returns_none_without_model_prefix() {
        assert!(extract_model_related_info("boot complete").is_none());
    }

    #[test]
    fn split_telegram_chunks_splits_when_over_limit() {
        let chunks = format::split_telegram_chunks("abcdefghij", 4, 0);
        assert_eq!(chunks, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn append_telegraph_log_line_adds_tail_line_when_url_exists() {
        let mut lines = vec!["line-1".to_string()];
        append_telegraph_log_lines(&mut lines, &[String::from("https://telegra.ph/abc")]);
        assert_eq!(
            lines.last().map(String::as_str),
            Some("Telegraph log: https://telegra.ph/abc")
        );
    }

    #[test]
    fn append_telegraph_log_line_keeps_original_lines_without_url() {
        let mut lines = vec!["line-1".to_string()];
        append_telegraph_log_lines(&mut lines, &[]);
        assert_eq!(lines, vec!["line-1".to_string()]);
    }

    #[test]
    fn append_telegraph_log_line_adds_all_urls_when_multiple_pages_exist() {
        let mut lines = vec!["line-1".to_string()];
        append_telegraph_log_lines(
            &mut lines,
            &[
                String::from("https://telegra.ph/abc"),
                String::from("https://telegra.ph/def"),
            ],
        );
        assert_eq!(
            lines,
            vec![
                "line-1".to_string(),
                "Telegraph logs:".to_string(),
                "1. https://telegra.ph/abc".to_string(),
                "2. https://telegra.ph/def".to_string(),
            ]
        );
    }

    #[test]
    fn append_lark_wiki_log_line_adds_tail_line_when_url_exists() {
        let mut lines = vec!["line-1".to_string()];
        append_lark_wiki_log_line(&mut lines, Some("https://example.feishu.cn/wiki/doc"));
        assert_eq!(
            lines.last().map(String::as_str),
            Some("Lark wiki log: https://example.feishu.cn/wiki/doc")
        );
    }

    #[test]
    fn append_lark_wiki_log_line_keeps_original_lines_without_url() {
        let mut lines = vec!["line-1".to_string()];
        append_lark_wiki_log_line(&mut lines, None);
        append_lark_wiki_log_line(&mut lines, Some("  "));
        assert_eq!(lines, vec!["line-1".to_string()]);
    }

    #[test]
    fn append_token_usage_line_renders_total_only_without_context_window() {
        let mut lines = Vec::new();
        append_token_usage_line(
            &mut lines,
            Some(&TokenUsageInfo {
                total_tokens: 654,
                model_context_window: 0,
            }),
        );

        assert_eq!(lines, vec!["Token usage: total=654".to_string()]);
    }

    #[test]
    fn stage_summary_keyboard_uses_review_done_for_daily_task() {
        let flow_token = "f-ab12c".to_string();
        let markup = stage_summary_keyboard(
            SummaryTrigger::ExecutionFinished,
            Some(flow_token.clone()),
            true,
            Some(101),
        )
        .expect("keyboard should be present");
        let value = serde_json::to_value(markup).expect("keyboard should serialize");
        let row = value["inline_keyboard"]
            .get(0)
            .and_then(Value::as_array)
            .expect("first row should be present");

        assert_eq!(row.len(), 3);
        assert_eq!(
            CallbackAction::decode(
                row[0]["callback_data"]
                    .as_str()
                    .expect("review callback should exist")
            ),
            Some(CallbackAction::CreateReviewTask {
                flow_token: flow_token.clone(),
            })
        );
        assert_eq!(
            CallbackAction::decode(
                row[1]["callback_data"]
                    .as_str()
                    .expect("done callback should exist")
            ),
            Some(CallbackAction::DoneTask {
                flow_token: flow_token.clone()
            })
        );
        assert_eq!(
            CallbackAction::decode(
                row[2]["callback_data"]
                    .as_str()
                    .expect("audio callback should exist")
            ),
            Some(CallbackAction::StageSummaryAudio {
                flow_token,
                msg_id: 101
            })
        );
    }

    #[test]
    fn stage_summary_keyboard_uses_review_for_non_daily_task() {
        let flow_token = "f-ab12c".to_string();
        let markup = stage_summary_keyboard(
            SummaryTrigger::TaskLeftInProgress,
            Some(flow_token.clone()),
            false,
            Some(101),
        )
        .expect("keyboard should be present");
        let value = serde_json::to_value(markup).expect("keyboard should serialize");
        let row = value["inline_keyboard"]
            .get(0)
            .and_then(Value::as_array)
            .expect("first row should be present");

        assert_eq!(row.len(), 2);
        assert_eq!(
            CallbackAction::decode(
                row[0]["callback_data"]
                    .as_str()
                    .expect("review callback should exist")
            ),
            Some(CallbackAction::CreateReviewTask {
                flow_token: flow_token.clone()
            })
        );
        assert_eq!(
            CallbackAction::decode(
                row[1]["callback_data"]
                    .as_str()
                    .expect("audio callback should exist")
            ),
            Some(CallbackAction::StageSummaryAudio {
                flow_token,
                msg_id: 101
            })
        );
    }

    #[test]
    fn restored_stage_summary_keyboard_keeps_done_button_for_daily_task() {
        let flow_token = "f-ab12c";
        let markup = stage_summary_reply_keyboard_for_task(flow_token, 101, true);
        let value = serde_json::to_value(markup).expect("keyboard should serialize");
        let row = value["inline_keyboard"]
            .get(0)
            .and_then(Value::as_array)
            .expect("first row should be present");

        assert_eq!(row.len(), 3);
        assert_eq!(row[1]["text"], "✅ Done");
        assert_eq!(
            CallbackAction::decode(
                row[1]["callback_data"]
                    .as_str()
                    .expect("done callback should exist")
            ),
            Some(CallbackAction::DoneTask {
                flow_token: flow_token.to_string(),
            })
        );
    }

    #[test]
    fn stage_summary_keyboard_omits_buttons_for_next_action_trigger() {
        assert!(
            stage_summary_keyboard(
                SummaryTrigger::NextAction,
                Some("f-ab12c".to_string()),
                true,
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn configured_daily_project_helper_matches_valid_daily_project_id() {
        let daily_project_id = Uuid::new_v4();

        assert!(is_configured_daily_project_task(
            Some(daily_project_id),
            Some(&daily_project_id.to_string())
        ));
    }

    #[test]
    fn configured_daily_project_helper_returns_false_for_non_match_missing_or_invalid_config() {
        let task_project_id = Uuid::new_v4();

        assert!(!is_configured_daily_project_task(
            Some(task_project_id),
            Some(&Uuid::new_v4().to_string())
        ));
        assert!(!is_configured_daily_project_task(
            Some(task_project_id),
            None
        ));
        assert!(!is_configured_daily_project_task(
            Some(task_project_id),
            Some("not-a-uuid")
        ));
        assert!(!is_configured_daily_project_task(
            None,
            Some(&task_project_id.to_string())
        ));
    }

    #[test]
    fn task_finished_notification_filter_skips_daily_tasks_only() {
        let daily_project_id = Uuid::new_v4();
        let non_daily_project_id = Uuid::new_v4();
        let configured_daily_project_id = daily_project_id.to_string();

        assert!(!should_send_task_finished_notification(
            Some(daily_project_id),
            Some(&configured_daily_project_id)
        ));
        assert!(should_send_task_finished_notification(
            Some(non_daily_project_id),
            Some(&configured_daily_project_id)
        ));
        assert!(should_send_task_finished_notification(
            Some(daily_project_id),
            Some("invalid")
        ));
    }

    fn entry(entry_type: NormalizedEntryType, content: &str) -> NormalizedEntry {
        NormalizedEntry {
            timestamp: None,
            entry_type,
            content: content.to_string(),
            metadata: None,
        }
    }

    #[test]
    fn mapping_rules_cover_all_normalized_entry_types() {
        let now = chrono::Utc::now();
        let approval_id = Uuid::new_v4().to_string();

        let cases = vec![
            (
                entry(NormalizedEntryType::UserMessage, "user"),
                EntryDeliveryBehavior::SummaryOnly,
            ),
            (
                entry(
                    NormalizedEntryType::UserFeedback {
                        denied_tool: "bash".to_string(),
                    },
                    "denied",
                ),
                EntryDeliveryBehavior::RealtimeOnly,
            ),
            (
                entry(NormalizedEntryType::AssistantMessage, "assistant"),
                EntryDeliveryBehavior::RealtimeAndSummary,
            ),
            (
                entry(
                    NormalizedEntryType::ToolUse {
                        tool_name: "bash".to_string(),
                        action_type: ActionType::CommandRun {
                            command: "echo hi".to_string(),
                            result: None,
                        },
                        status: ToolStatus::PendingApproval {
                            approval_id,
                            requested_at: now,
                            timeout_at: now,
                        },
                    },
                    "pending",
                ),
                EntryDeliveryBehavior::RealtimeAndSummary,
            ),
            (
                entry(NormalizedEntryType::SystemMessage, "system"),
                EntryDeliveryBehavior::SummaryOnly,
            ),
            (
                entry(
                    NormalizedEntryType::ErrorMessage {
                        error_type: NormalizedEntryError::Other,
                    },
                    "error",
                ),
                EntryDeliveryBehavior::Ignore,
            ),
            (
                entry(NormalizedEntryType::Thinking, "thinking"),
                EntryDeliveryBehavior::RealtimeAndSummary,
            ),
            (
                entry(NormalizedEntryType::Loading, "loading"),
                EntryDeliveryBehavior::Ignore,
            ),
            (
                entry(
                    NormalizedEntryType::NextAction {
                        failed: false,
                        execution_processes: 1,
                        needs_setup: false,
                    },
                    "next",
                ),
                EntryDeliveryBehavior::RealtimeAndSummaryTrigger,
            ),
            (
                entry(
                    NormalizedEntryType::TokenUsageInfo(TokenUsageInfo {
                        total_tokens: 100,
                        model_context_window: 200_000,
                    }),
                    "usage",
                ),
                EntryDeliveryBehavior::SummaryOnly,
            ),
        ];

        for (entry, expected) in cases {
            assert_eq!(classify_entry_behavior(&entry), expected);
        }
    }

    #[test]
    fn tool_use_created_is_summary_only_others_are_realtime_and_summary() {
        let now = chrono::Utc::now();
        let base = |status| {
            entry(
                NormalizedEntryType::ToolUse {
                    tool_name: "bash".to_string(),
                    action_type: ActionType::CommandRun {
                        command: "echo hi".to_string(),
                        result: None,
                    },
                    status,
                },
                "tool",
            )
        };

        assert_eq!(
            classify_entry_behavior(&base(ToolStatus::Created)),
            EntryDeliveryBehavior::SummaryOnly
        );
        assert_eq!(
            classify_entry_behavior(&base(ToolStatus::Success)),
            EntryDeliveryBehavior::RealtimeAndSummary
        );
        assert_eq!(
            classify_entry_behavior(&base(ToolStatus::Failed)),
            EntryDeliveryBehavior::RealtimeAndSummary
        );
        assert_eq!(
            classify_entry_behavior(&base(ToolStatus::PendingApproval {
                approval_id: Uuid::new_v4().to_string(),
                requested_at: now,
                timeout_at: now,
            })),
            EntryDeliveryBehavior::RealtimeAndSummary
        );
        assert_eq!(
            classify_entry_behavior(&base(ToolStatus::Denied { reason: None })),
            EntryDeliveryBehavior::RealtimeAndSummary
        );
        assert_eq!(
            classify_entry_behavior(&base(ToolStatus::TimedOut)),
            EntryDeliveryBehavior::RealtimeAndSummary
        );
    }

    #[test]
    fn summary_aggregates_assistant_system_tools_and_tokens() {
        let mut acc = RunFeedAccumulator::default();
        let now = chrono::Utc::now();

        let patches = vec![
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                0,
                entry(NormalizedEntryType::UserMessage, "Implement feature X"),
            ),
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                1,
                entry(NormalizedEntryType::AssistantMessage, "First draft"),
            ),
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                2,
                entry(
                    NormalizedEntryType::ToolUse {
                        tool_name: "bash".to_string(),
                        action_type: ActionType::CommandRun {
                            command: "cargo test".to_string(),
                            result: None,
                        },
                        status: ToolStatus::Success,
                    },
                    "tool success",
                ),
            ),
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                3,
                entry(
                    NormalizedEntryType::ToolUse {
                        tool_name: "bash".to_string(),
                        action_type: ActionType::CommandRun {
                            command: "ls".to_string(),
                            result: None,
                        },
                        status: ToolStatus::PendingApproval {
                            approval_id: Uuid::new_v4().to_string(),
                            requested_at: now,
                            timeout_at: now,
                        },
                    },
                    "tool pending",
                ),
            ),
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                4,
                entry(NormalizedEntryType::SystemMessage, "system-1"),
            ),
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                5,
                entry(
                    NormalizedEntryType::TokenUsageInfo(TokenUsageInfo {
                        total_tokens: 321,
                        model_context_window: 128_000,
                    }),
                    "usage",
                ),
            ),
            executors::logs::utils::patch::ConversationPatch::replace(
                1,
                entry(
                    NormalizedEntryType::AssistantMessage,
                    "Latest assistant summary",
                ),
            ),
        ];

        for patch in patches {
            let _ = acc.apply_patch(&patch);
        }

        let summary = acc.collect_summary_data(true);
        assert_eq!(
            summary.assistant_messages,
            vec!["Latest assistant summary".to_string()]
        );
        assert_eq!(summary.system_count, 1);
        assert_eq!(summary.tool_stats.total, 2);
        assert_eq!(summary.tool_stats.success, 1);
        assert_eq!(summary.tool_stats.pending_approval, 1);
        assert_eq!(summary.token_usage.map(|u| u.total_tokens), Some(321));
    }

    #[test]
    fn summary_collects_all_changed_assistant_messages_in_entry_order() {
        let mut acc = RunFeedAccumulator::default();

        let patches = vec![
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                0,
                entry(NormalizedEntryType::AssistantMessage, "First reply"),
            ),
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                1,
                entry(NormalizedEntryType::AssistantMessage, "Second reply"),
            ),
        ];

        for patch in patches {
            let _ = acc.apply_patch(&patch);
        }

        assert_eq!(
            acc.collect_summary_data(true).assistant_messages,
            vec!["First reply".to_string(), "Second reply".to_string()]
        );
    }

    #[test]
    fn summary_keeps_only_latest_text_for_replaced_assistant_entry() {
        let mut acc = RunFeedAccumulator::default();

        let patches = vec![
            executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                0,
                entry(NormalizedEntryType::AssistantMessage, "Draft"),
            ),
            executors::logs::utils::patch::ConversationPatch::replace(
                0,
                entry(NormalizedEntryType::AssistantMessage, "Final"),
            ),
        ];

        for patch in patches {
            let _ = acc.apply_patch(&patch);
        }

        assert_eq!(
            acc.collect_summary_data(true).assistant_messages,
            vec!["Final".to_string()]
        );
    }

    #[test]
    fn summary_changed_entries_exclude_already_finalized_stage() {
        let mut acc = RunFeedAccumulator::default();

        let _ = acc.apply_patch(
            &executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                0,
                entry(NormalizedEntryType::AssistantMessage, "First round"),
            ),
        );
        acc.finalize_stage();
        let _ = acc.apply_patch(
            &executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                1,
                entry(NormalizedEntryType::AssistantMessage, "Second round"),
            ),
        );

        let changed: Vec<String> = acc
            .iter_entries_for_summary(true)
            .into_iter()
            .map(|(_, entry)| entry.content)
            .collect();
        let overall: Vec<String> = acc
            .iter_entries_for_summary(false)
            .into_iter()
            .map(|(_, entry)| entry.content)
            .collect();

        assert_eq!(changed, vec!["Second round".to_string()]);
        assert_eq!(
            overall,
            vec!["First round".to_string(), "Second round".to_string()]
        );
    }

    #[test]
    fn stage_summary_preserves_full_assistant_reply_content() {
        let assistant = "a".repeat(TELEGRAM_MESSAGE_LIMIT * 3 + 17);
        let lines = {
            let mut lines = vec!["🧾 [CODEX · PLAN] Stage summary".to_string()];
            append_stage_summary_assistant_lines(&mut lines, std::slice::from_ref(&assistant));
            lines
        };

        let rendered = lines.join("\n");
        assert!(rendered.contains(&assistant));
        assert!(!rendered.contains('…'));
    }

    #[test]
    fn flow_prefix_uses_executor_and_variant() {
        let mut acc = RunFeedAccumulator::default();
        acc.flow_context = Some(flow::TelegramFlowContext {
            flow_token: "f-abc12".to_string(),
            task_id: Uuid::nil(),
            workspace_id: Uuid::nil(),
            session_id: Uuid::nil(),
            latest_execution_process_id: None,
            executor_label: "CODEX".to_string(),
            variant_label: "PLAN".to_string(),
            topic_thread_id: None,
        });

        assert_eq!(acc.flow_prefix(), "[CODEX · PLAN]");
    }

    #[test]
    fn flow_prefix_defaults_when_flow_context_is_missing() {
        let acc = RunFeedAccumulator::default();

        assert_eq!(acc.flow_prefix(), "[UNKNOWN · DEFAULT]");
    }

    #[test]
    fn stage_summary_assistant_lines_keep_only_latest_message() {
        let mut lines = Vec::new();
        append_stage_summary_assistant_lines(
            &mut lines,
            &["First reply".to_string(), "Second reply".to_string()],
        );

        assert_eq!(lines, vec!["Second reply".to_string()]);
    }

    #[test]
    fn stage_summary_assistant_lines_skip_empty_messages() {
        let mut lines = Vec::new();
        append_stage_summary_assistant_lines(&mut lines, &[]);

        assert!(lines.is_empty());
    }

    #[test]
    fn same_assistant_entry_reuses_existing_message() {
        let mut acc = RunFeedAccumulator::default();
        let message_id = MessageId(101);

        acc.set_active_stable_text_message_id(StableTextKind::Assistant, message_id);

        assert_eq!(acc.active_stable_text_message_id(), Some(message_id));
        assert_eq!(acc.active_stable_text_entry_index, None);
        assert_eq!(acc.active_stable_text_kind, Some(StableTextKind::Assistant));
    }

    #[test]
    fn thinking_is_not_sent_while_still_active() {
        let mut acc = RunFeedAccumulator::default();
        let _ = acc.apply_patch(
            &executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                1,
                entry(NormalizedEntryType::Thinking, "thinking draft"),
            ),
        );
        acc.track_active_stable_text_entry(1, StableTextKind::Thinking);

        assert_eq!(acc.active_stable_text_entry_index, Some(1));
        assert_eq!(acc.active_stable_text_message_id, None);
        assert_eq!(
            acc.flushable_active_stable_text(),
            Some((StableTextKind::Thinking, "thinking draft".to_string()))
        );
    }

    #[test]
    fn thinking_flushes_when_later_non_thinking_entry_arrives() {
        let mut acc = RunFeedAccumulator::default();
        let _ = acc.apply_patch(
            &executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                1,
                entry(NormalizedEntryType::Thinking, "stable thinking"),
            ),
        );
        acc.track_active_stable_text_entry(1, StableTextKind::Thinking);
        let _ = acc.apply_patch(
            &executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                2,
                entry(NormalizedEntryType::UserMessage, "next user turn"),
            ),
        );

        assert_eq!(
            acc.flushable_active_stable_text(),
            Some((StableTextKind::Thinking, "stable thinking".to_string()))
        );
    }

    #[test]
    fn tracking_new_stable_text_entry_keeps_previous_message_id_for_edit_reuse() {
        let mut acc = RunFeedAccumulator::default();
        acc.set_active_stable_text_message_id(StableTextKind::Assistant, MessageId(101));

        acc.track_active_stable_text_entry(2, StableTextKind::Thinking);

        assert_eq!(acc.active_stable_text_entry_index, Some(2));
        assert_eq!(acc.active_stable_text_message_id, Some(MessageId(101)));
        assert_eq!(acc.active_stable_text_kind, Some(StableTextKind::Thinking));
    }

    #[test]
    fn new_execution_process_resets_active_stable_text_state() {
        let mut acc = RunFeedAccumulator::default();
        let process_a = Uuid::new_v4();
        let process_b = Uuid::new_v4();

        acc.remember_execution_process(process_a);
        acc.set_active_stable_text_message_id(StableTextKind::Assistant, MessageId(101));
        acc.remember_execution_process(process_a);
        assert_eq!(acc.active_stable_text_entry_index, None);
        assert_eq!(acc.active_stable_text_message_id, Some(MessageId(101)));

        acc.remember_execution_process(process_b);
        assert_eq!(acc.active_stable_text_entry_index, None);
        assert_eq!(acc.active_stable_text_message_id, None);
        assert_eq!(acc.active_stable_text_kind, None);
    }

    #[test]
    fn new_execution_process_resets_process_scoped_accumulator_state() {
        let mut acc = RunFeedAccumulator::default();
        let process_a = Uuid::new_v4();
        let process_b = Uuid::new_v4();

        acc.remember_execution_process(process_a);
        let _ = acc.apply_patch(
            &executors::logs::utils::patch::ConversationPatch::add_normalized_entry(
                0,
                entry(NormalizedEntryType::AssistantMessage, "draft"),
            ),
        );
        acc.sent_pending_approvals.insert("approval-a".to_string());
        acc.tool_turn.current_user_turn_entry_index = Some(9);
        acc.tool_turn.telegram_message_id = Some(MessageId(222));
        acc.tool_turn.appended_terminal_tool_indexes.insert(0);
        acc.tool_turn.rendered_rows.push("row".to_string());
        acc.finalize_stage();

        acc.remember_execution_process(process_b);

        assert!(acc.entries_by_index.is_empty());
        assert!(acc.update_seq_by_index.is_empty());
        assert_eq!(acc.current_seq, 0);
        assert_eq!(acc.last_summary_seq, 0);
        assert!(acc.sent_pending_approvals.is_empty());
        assert!(acc.tool_turn.appended_terminal_tool_indexes.is_empty());
        assert!(acc.tool_turn.rendered_rows.is_empty());
        assert_eq!(acc.tool_turn.telegram_message_id, None);
        assert_eq!(acc.execution_process_id, Some(process_b));
    }

    #[test]
    fn user_message_starts_new_tool_aggregation_turn() {
        let mut acc = RunFeedAccumulator::default();
        acc.tool_turn.telegram_message_id = Some(MessageId(55));
        acc.tool_turn.appended_terminal_tool_indexes.insert(3);
        acc.tool_turn.rendered_rows.push("old row".to_string());

        acc.note_user_turn(8);

        assert_eq!(acc.tool_turn.current_user_turn_entry_index, Some(8));
        assert_eq!(acc.tool_turn.telegram_message_id, None);
        assert!(acc.tool_turn.appended_terminal_tool_indexes.is_empty());
        assert!(acc.tool_turn.rendered_rows.is_empty());
    }

    #[test]
    fn terminal_tool_statuses_append_once_and_non_terminal_do_not() {
        let mut acc = RunFeedAccumulator::default();

        assert!(!is_terminal_tool_status(&ToolStatus::Created));
        assert!(!is_terminal_tool_status(&ToolStatus::PendingApproval {
            approval_id: Uuid::new_v4().to_string(),
            requested_at: chrono::Utc::now(),
            timeout_at: chrono::Utc::now(),
        }));
        assert!(is_terminal_tool_status(&ToolStatus::Success));
        assert!(is_terminal_tool_status(&ToolStatus::Failed));
        assert!(is_terminal_tool_status(&ToolStatus::Denied {
            reason: None
        }));
        assert!(is_terminal_tool_status(&ToolStatus::TimedOut));
        acc.apply_terminal_tool_turn_update(1, vec!["row-1".to_string()], MessageId(42));
        assert_eq!(acc.tool_turn.rendered_rows, vec!["row-1".to_string()]);
        assert!(acc.tool_turn.appended_terminal_tool_indexes.contains(&1));
    }

    #[test]
    fn tool_turn_update_keeps_appending_when_under_limit() {
        let mut acc = RunFeedAccumulator::default();
        acc.tool_turn.telegram_message_id = Some(MessageId(101));
        acc.tool_turn.rendered_rows = vec!["✅ bash: run cargo test".to_string()];

        let plan = acc.plan_terminal_tool_turn_update(
            2,
            "❌ search: search notifier".to_string(),
            "[CODEX · PLAN]",
        );

        match plan {
            ToolTurnUpdatePlan::EditCurrent { rows, message } => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0], "✅ bash: run cargo test");
                assert_eq!(rows[1], "❌ search: search notifier");
                assert!(message.contains("✅ bash: run cargo test"));
                assert!(message.contains("❌ search: search notifier"));
            }
            other => panic!("expected edit plan, got {other:?}"),
        }
    }

    #[test]
    fn tool_turn_update_replaces_message_when_limit_would_be_exceeded() {
        let mut acc = RunFeedAccumulator::default();
        acc.tool_turn.telegram_message_id = Some(MessageId(202));
        acc.tool_turn.rendered_rows = vec!["x".repeat(TELEGRAM_MESSAGE_LIMIT)];

        let plan = acc.plan_terminal_tool_turn_update(3, "next row".to_string(), "[X]");

        match plan {
            ToolTurnUpdatePlan::ReplaceMessage {
                old_message_id,
                rows,
                message,
            } => {
                assert_eq!(old_message_id, Some(MessageId(202)));
                assert_eq!(rows, vec!["next row".to_string()]);
                assert_eq!(message, render_tool_turn_card("[X]", &rows));
                assert!(message.contains("next row"));
                assert!(!message.contains(&"x".repeat(16)));
            }
            other => panic!("expected replace plan, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_terminal_tool_call_does_not_reappend() {
        let mut acc = RunFeedAccumulator::default();
        acc.tool_turn.appended_terminal_tool_indexes.insert(7);
        acc.tool_turn.rendered_rows = vec!["existing row".to_string()];

        let plan =
            acc.plan_terminal_tool_turn_update(7, "duplicate row".to_string(), "[CODEX · PLAN]");

        assert_eq!(plan, ToolTurnUpdatePlan::SkipDuplicate);
    }

    #[test]
    fn multiple_terminal_tool_calls_render_in_order_in_one_card() {
        let rows = vec![
            render_terminal_tool_row(
                "bash",
                &ToolStatus::Success,
                &ActionType::CommandRun {
                    command: "cargo test -p services".to_string(),
                    result: None,
                },
            ),
            render_terminal_tool_row(
                "search",
                &ToolStatus::Failed,
                &ActionType::Search {
                    query: "telegram notifier".to_string(),
                },
            ),
        ];

        let rendered = render_tool_turn_card("[CODEX · PLAN]", &rows);
        assert!(rendered.contains("🛠️ [CODEX · PLAN] Tool calls"));
        assert!(rendered.contains("✅ bash: run cargo test -p services"));
        assert!(rendered.contains("❌ search: search telegram notifier"));
        let first = rendered.find("✅ bash").expect("first row should exist");
        let second = rendered.find("❌ search").expect("second row should exist");
        assert!(first < second);
    }

    #[test]
    fn auto_executed_tools_do_not_emit_realtime_cards() {
        let entry = entry(
            NormalizedEntryType::ToolUse {
                tool_name: "bash".to_string(),
                action_type: ActionType::CommandRun {
                    command: "echo auto".to_string(),
                    result: None,
                },
                status: ToolStatus::Success,
            },
            "auto tool",
        );

        assert_eq!(
            classify_entry_behavior(&entry),
            EntryDeliveryBehavior::RealtimeAndSummary
        );
    }

    #[test]
    fn created_tools_still_do_not_emit_realtime_cards() {
        let entry = entry(
            NormalizedEntryType::ToolUse {
                tool_name: "bash".to_string(),
                action_type: ActionType::CommandRun {
                    command: "echo pending".to_string(),
                    result: None,
                },
                status: ToolStatus::Created,
            },
            "created tool",
        );

        assert_eq!(
            classify_entry_behavior(&entry),
            EntryDeliveryBehavior::SummaryOnly
        );
    }

    #[test]
    fn split_plan_no_headings_short() {
        let plan = "Just a plain text plan with no headings.";
        let result = split_plan(plan);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], plan);
    }

    #[test]
    fn split_plan_single_heading_short() {
        let plan = "# Heading\nSome content";
        let result = split_plan(plan);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], plan);
    }

    #[test]
    fn split_plan_multiple_headings_within_limit() {
        let plan = "# Part 1\ncontent1\n## Part 2\ncontent2";
        let result = split_plan(plan);
        // Fits in one message, no labels
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], plan);
    }

    #[test]
    fn split_plan_labels_format() {
        // Build a plan with headings where each section is large enough to force splitting
        let section1 = format!("# Section 1\n{}", "a".repeat(3000));
        let section2 = format!("# Section 2\n{}", "b".repeat(3000));
        let plan = format!("{}\n{}", section1, section2);

        let result = split_plan(&plan);
        assert_eq!(result.len(), 2);
        assert!(result[0].starts_with("Plan Review: [1/2] # Section 1"));
        assert!(result[1].starts_with("Plan Review: [2/2] # Section 2"));
    }

    #[test]
    fn split_plan_splits_at_heading_boundary() {
        let section1 = format!("# Introduction\n{}", "x".repeat(2000));
        let section2 = format!("## Details\n{}", "y".repeat(2000));
        let section3 = format!("### Conclusion\n{}", "z".repeat(2000));
        let plan = format!("{}\n{}\n{}", section1, section2, section3);

        let result = split_plan(&plan);
        // Each section is ~2010 chars; two sections ~4020 which exceeds limit after prefix reserve
        assert!(result.len() >= 2);
        // First chunk should contain the introduction heading
        assert!(result[0].contains("# Introduction"));
        // Verify all chunks have [i/n] labels
        for (i, chunk) in result.iter().enumerate() {
            assert!(chunk.starts_with(&format!("Plan Review: [{}/{}]", i + 1, result.len())));
        }
    }

    #[test]
    fn split_plan_merges_small_sections() {
        // Many small sections should be merged into fewer chunks
        let mut parts = Vec::new();
        for i in 0..10 {
            parts.push(format!("# Section {}\nShort content {}", i, i));
        }
        let plan = parts.join("\n");

        let result = split_plan(&plan);
        // All sections are tiny, should fit in one chunk with no labels
        assert_eq!(result.len(), 1);
        assert!(!result[0].starts_with("[1/"));
    }
}
