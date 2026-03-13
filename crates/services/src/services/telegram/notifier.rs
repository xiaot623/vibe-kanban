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

use db::{
    DBService,
    models::{
        execution_process::{ExecutionProcess, ExecutionProcessRunReason, ExecutionProcessStatus},
        short_id_mapping::ShortIdMapping,
        task::{Task, TaskStatus},
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
    NormalizedEntry, NormalizedEntryType, TokenUsageInfo, ToolStatus,
    utils::patch::extract_normalized_entry_from_patch,
};
use teloxide::{prelude::*, types::MessageId};
use tokio::sync::{OnceCell, RwLock};
use tokio_util::sync::CancellationToken;
use utils::{log_msg::LogMsg, msg_store::MsgStore};
use uuid::Uuid;

use super::{EXIT_PLAN_MODE_NAME, format, keyboard};
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

#[derive(Debug, Default)]
struct PlanReviewMessageRegistry {
    by_task: HashMap<Uuid, Vec<i32>>,
}

impl PlanReviewMessageRegistry {
    /// Store a non-empty set of message IDs for a task.
    /// To remove an entry use [`Self::clear`] or [`Self::take`].
    fn record(&mut self, task_id: Uuid, message_ids: Vec<i32>) {
        debug_assert!(
            !message_ids.is_empty(),
            "record() called with empty ids; use clear() instead"
        );
        if !message_ids.is_empty() {
            self.by_task.insert(task_id, message_ids);
        }
    }

    /// Remove and return any recorded message IDs for a task (one-shot).
    fn take(&mut self, task_id: Uuid) -> Vec<i32> {
        self.by_task.remove(&task_id).unwrap_or_default()
    }

    /// Discard any recorded message IDs for a task without returning them.
    fn clear(&mut self, task_id: Uuid) {
        self.by_task.remove(&task_id);
    }
}

pub fn set_telegram_context(ctx: TelegramContext) {
    let lock = TELEGRAM_CONTEXT.get_or_init(|| Arc::new(RwLock::new(None)));
    if let Ok(mut guard) = lock.try_write() {
        *guard = Some(ctx);
    }
}

/// Clear the telegram context (for shutdown).
pub fn clear_telegram_context() {
    if let Some(lock) = TELEGRAM_CONTEXT.get() {
        if let Ok(mut guard) = lock.try_write() {
            *guard = None;
        }
    }
    cancel_all_run_feed_watchers();
    clear_plan_review_message_registry();
}

async fn get_context() -> Option<TelegramContext> {
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
        guard.by_task.clear();
    }
}

async fn record_plan_review_message_ids(task_id: Uuid, message_ids: Vec<MessageId>) {
    let ids: Vec<i32> = message_ids.into_iter().map(|id| id.0).collect();
    plan_review_message_registry()
        .write()
        .await
        .record(task_id, ids);
}

/// Discard tracked plan-review message IDs for a task (no Telegram delete).
/// Used when no plan-review cards were sent for this transition.
async fn clear_plan_review_message_ids(task_id: Uuid) {
    plan_review_message_registry().write().await.clear(task_id);
}

pub(super) async fn take_plan_review_message_ids(task_id: Uuid) -> Vec<MessageId> {
    plan_review_message_registry()
        .write()
        .await
        .take(task_id)
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
        let task = &transition.task;
        start_run_feed_watcher(tg.clone(), task.id).await;
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
        stop_run_feed_watcher(transition.task.id).await;
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

        // If the task has a pending plan, split and send as multiple messages
        if let Some(plan) = find_exit_plan_approval(tg, task.id).await {
            let chunks = split_plan(&plan.plan);
            let total = chunks.len();
            let mut sent_message_ids = Vec::new();
            for (i, chunk) in chunks.into_iter().enumerate() {
                // Attach approve/reject buttons to the last chunk.
                let markup = if i == total - 1 {
                    Some(keyboard::review_notification_keyboard(task.id))
                } else {
                    None
                };
                if let Some(message_id) = send_telegram_card(tg, chunk, markup).await {
                    sent_message_ids.push(message_id);
                }
            }
            record_plan_review_message_ids(task.id, sent_message_ids).await;
            return;
        }

        // No split review cards for this task in the current transition.
        // Explicitly clear any IDs that might have been recorded from a prior InReview
        // cycle so they are not stale-deleted on a future approve/reject.
        clear_plan_review_message_ids(task.id).await;
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
        let short_id = ShortIdMapping::get_or_create(&tg.db.pool, task.id)
            .await
            .unwrap_or_else(|_| "????".to_string());

        // Use stored diff stats from the task (computed before merge),
        // falling back to runtime computation if not available
        let diff_stats = match (task.diff_additions, task.diff_deletions) {
            (Some(added), Some(removed)) => Some((added as usize, removed as usize)),
            _ => compute_workspace_diff_stats(tg, task.id).await,
        };

        let message = format!(
            "[{}] 🎉🎉🎉 Task {} Finished\ndiff: {}",
            short_id,
            task.title,
            match diff_stats {
                Some((added, removed)) => format!("+{} / -{}", added, removed),
                None => format!("+{0} / -{0}", 0),
            }
        );

        if let Err(err) = format::send_rich_then_plain(&tg.bot, tg.chat_id, &message, None).await {
            tracing::warn!("Failed to send telegram notification: {}", err);
        }
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
    assistant_latest: Option<String>,
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

#[derive(Default)]
struct RunFeedAccumulator {
    entries_by_index: BTreeMap<usize, NormalizedEntry>,
    update_seq_by_index: HashMap<usize, usize>,
    current_seq: usize,
    last_summary_seq: usize,
    task_id: Option<Uuid>,
    task_project_id: Option<Uuid>,
    task_title: Option<String>,
    sent_pending_approvals: HashSet<String>,
    sent_terminal_tool_updates: HashSet<(usize, &'static str)>,
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

    fn changed_entries_since_last_summary(&self) -> Vec<(usize, NormalizedEntry)> {
        let mut changed: Vec<_> = self
            .update_seq_by_index
            .iter()
            .filter_map(|(idx, seq)| {
                (*seq > self.last_summary_seq)
                    .then_some(
                        self.entries_by_index
                            .get(idx)
                            .cloned()
                            .map(|entry| (*seq, entry)),
                    )
                    .flatten()
            })
            .collect();
        changed.sort_by_key(|(seq, _)| *seq);
        changed
    }

    fn collect_summary_data(&self, changed_only: bool) -> StageSummaryData {
        let entries: Vec<NormalizedEntry> = if changed_only {
            self.changed_entries_since_last_summary()
                .into_iter()
                .map(|(_, entry)| entry)
                .collect()
        } else {
            self.entries_by_index.values().cloned().collect()
        };

        let mut data = StageSummaryData::default();
        for entry in entries {
            match entry.entry_type {
                NormalizedEntryType::UserMessage => {
                    if data.user_first.is_none() {
                        data.user_first = Some(entry.content.clone());
                    }
                    data.user_latest = Some(entry.content);
                }
                NormalizedEntryType::AssistantMessage => {
                    data.assistant_latest = Some(entry.content);
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
}

fn classify_entry_behavior(entry: &NormalizedEntry) -> EntryDeliveryBehavior {
    match &entry.entry_type {
        NormalizedEntryType::Thinking | NormalizedEntryType::Loading => {
            EntryDeliveryBehavior::Ignore
        }
        NormalizedEntryType::UserFeedback { .. } => EntryDeliveryBehavior::RealtimeOnly,
        NormalizedEntryType::ErrorMessage { .. } => EntryDeliveryBehavior::Ignore,
        NormalizedEntryType::NextAction { .. } => EntryDeliveryBehavior::RealtimeAndSummaryTrigger,
        NormalizedEntryType::ToolUse { status, .. } => match status {
            ToolStatus::PendingApproval { .. }
            | ToolStatus::Denied { .. }
            | ToolStatus::TimedOut => EntryDeliveryBehavior::RealtimeAndSummary,
            ToolStatus::Created | ToolStatus::Success | ToolStatus::Failed => {
                EntryDeliveryBehavior::SummaryOnly
            }
        },
        NormalizedEntryType::UserMessage
        | NormalizedEntryType::AssistantMessage
        | NormalizedEntryType::SystemMessage
        | NormalizedEntryType::TokenUsageInfo(_) => EntryDeliveryBehavior::SummaryOnly,
    }
}

async fn start_run_feed_watcher(tg: TelegramContext, task_id: Uuid) {
    let watcher_id = Uuid::new_v4();
    let cancel = CancellationToken::new();

    {
        let mut watchers = run_feed_watchers().write().await;
        if let Some(existing) = watchers.insert(
            task_id,
            RunFeedWatcherHandle {
                watcher_id,
                cancel: cancel.clone(),
            },
        ) {
            existing.cancel.cancel();
        }
    }

    tokio::spawn(async move {
        run_feed_watcher_loop(tg, task_id, watcher_id, cancel.clone()).await;
        cleanup_run_feed_watcher(task_id, watcher_id).await;
    });
}

async fn stop_run_feed_watcher(task_id: Uuid) {
    let handle = { run_feed_watchers().write().await.remove(&task_id) };
    if let Some(handle) = handle {
        handle.cancel.cancel();
    }
}

async fn cleanup_run_feed_watcher(task_id: Uuid, watcher_id: Uuid) {
    let mut watchers = run_feed_watchers().write().await;
    if watchers
        .get(&task_id)
        .is_some_and(|handle| handle.watcher_id == watcher_id)
    {
        watchers.remove(&task_id);
    }
}

async fn run_feed_watcher_loop(
    tg: TelegramContext,
    task_id: Uuid,
    watcher_id: Uuid,
    cancel: CancellationToken,
) {
    let mut accumulator = RunFeedAccumulator::default();
    let mut watched_processes = HashSet::new();

    loop {
        if cancel.is_cancelled() {
            emit_stage_summary(&tg, &mut accumulator, SummaryTrigger::TaskLeftInProgress).await;
            return;
        }

        let task = match Task::find_by_id(&tg.db.pool, task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!("run-feed watcher {watcher_id} failed to load task: {err}");
                return;
            }
        };
        accumulator.remember_task(task.id, task.project_id, &task.title);

        if task.status != TaskStatus::InProgress {
            emit_stage_summary(&tg, &mut accumulator, SummaryTrigger::TaskLeftInProgress).await;
            return;
        }

        let Some(process) =
            find_running_coding_agent_process(&tg, task_id, &watched_processes).await
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

async fn find_running_coding_agent_process(
    tg: &TelegramContext,
    task_id: Uuid,
    exclude: &HashSet<Uuid>,
) -> Option<ExecutionProcess> {
    let workspaces = Workspace::fetch_all(&tg.db.pool, Some(task_id))
        .await
        .ok()?;
    let mut latest_running: Option<ExecutionProcess> = None;

    for workspace in workspaces {
        let maybe_process = ExecutionProcess::find_latest_by_workspace_and_run_reason(
            &tg.db.pool,
            workspace.id,
            &ExecutionProcessRunReason::CodingAgent,
        )
        .await
        .ok()
        .flatten();

        let Some(process) = maybe_process else {
            continue;
        };

        if process.status != ExecutionProcessStatus::Running || exclude.contains(&process.id) {
            continue;
        }

        if latest_running
            .as_ref()
            .is_none_or(|current| process.created_at > current.created_at)
        {
            latest_running = Some(process);
        }
    }

    latest_running
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

    if !emit_realtime {
        return;
    }

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
            send_tool_pending_approval_card(tg, approval_id, tool_name, &entry.content).await;
        }
    }
}

async fn emit_realtime_card_for_entry(
    tg: &TelegramContext,
    accumulator: &mut RunFeedAccumulator,
    update: &EntryUpdate,
) {
    match &update.current.entry_type {
        NormalizedEntryType::UserFeedback { denied_tool } => {
            let message = format!(
                "🚫 User rejected tool: {}\n{}",
                denied_tool,
                truncate_for_telegram(&update.current.content, 220)
            );
            let _ = send_telegram_card(tg, message, None).await;
        }
        NormalizedEntryType::ToolUse {
            tool_name, status, ..
        } => match status {
            ToolStatus::PendingApproval { approval_id, .. } => {
                if should_emit_pending_approval_card(&update.previous, approval_id)
                    && accumulator
                        .sent_pending_approvals
                        .insert(approval_id.clone())
                {
                    send_tool_pending_approval_card(
                        tg,
                        approval_id,
                        tool_name,
                        &update.current.content,
                    )
                    .await;
                }
            }
            ToolStatus::Denied { reason } => {
                if should_emit_terminal_tool_card(&update.previous, "denied")
                    && accumulator
                        .sent_terminal_tool_updates
                        .insert((update.index, "denied"))
                {
                    let reason = reason.as_deref().unwrap_or("No reason provided");
                    let message = format!(
                        "🛑 Tool denied: {tool_name}\nReason: {}\n{}",
                        truncate_for_telegram(reason, 140),
                        truncate_for_telegram(&update.current.content, 180)
                    );
                    let _ = send_telegram_card(tg, message, None).await;
                }
            }
            ToolStatus::TimedOut => {
                if should_emit_terminal_tool_card(&update.previous, "timed_out")
                    && accumulator
                        .sent_terminal_tool_updates
                        .insert((update.index, "timed_out"))
                {
                    let message = format!(
                        "⏱️ Tool approval timed out: {tool_name}\n{}",
                        truncate_for_telegram(&update.current.content, 200)
                    );
                    let _ = send_telegram_card(tg, message, None).await;
                }
            }
            ToolStatus::Created | ToolStatus::Success | ToolStatus::Failed => {}
        },
        NormalizedEntryType::NextAction {
            failed,
            execution_processes,
            needs_setup,
        } => {
            let message = format!(
                "🔁 Stage changed\nfailed: {}\nexecution_processes: {}\nneeds_setup: {}",
                failed, execution_processes, needs_setup
            );
            let _ = send_telegram_card(tg, message, None).await;
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

fn should_emit_terminal_tool_card(
    previous: &Option<NormalizedEntry>,
    terminal_state: &str,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };

    if let NormalizedEntryType::ToolUse { status, .. } = &previous.entry_type {
        match (terminal_state, status) {
            ("denied", ToolStatus::Denied { .. }) => return false,
            ("timed_out", ToolStatus::TimedOut) => return false,
            _ => {}
        }
    }

    true
}

async fn send_tool_pending_approval_card(
    tg: &TelegramContext,
    approval_id: &str,
    tool_name: &str,
    content: &str,
) {
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
        let _ = send_telegram_card(tg, message, None).await;
        return;
    }

    let message = format!("🛑 Need approval for {tool_name}\n{request_preview}");
    let _ = send_telegram_card(
        tg,
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
    let changed = accumulator.collect_summary_data(true);
    let overall = accumulator.collect_summary_data(false);

    let assistant_latest = changed.assistant_latest.or(overall.assistant_latest);
    let model_latest = changed.model_latest.or(overall.model_latest);
    let token_usage = changed.token_usage.or(overall.token_usage);

    let trigger_label = match trigger {
        SummaryTrigger::NextAction => "next_action",
        SummaryTrigger::ExecutionFinished => "execution_finished",
        SummaryTrigger::TaskLeftInProgress => "task_left_in_progress",
    };

    let mut lines = vec![format!("🧾 Stage summary ({trigger_label})")];
    lines.push(format!(
        "Task title: {}",
        accumulator
            .task_title
            .as_deref()
            .map(|title| truncate_for_telegram(title, STAGE_SUMMARY_TASK_TITLE_MAX_CHARS))
            .unwrap_or_else(|| "n/a".to_string())
    ));

    if let Some(assistant) = assistant_latest {
        lines.push(format!(
            "Assistant final reply:\n{}",
            truncate_for_telegram(&assistant, STAGE_SUMMARY_ASSISTANT_MAX_CHARS)
        ));
    } else {
        lines.push("Assistant final reply: n/a".to_string());
    }

    if let Some(model) = model_latest {
        lines.push(format!(
            "Model info: {}",
            truncate_for_telegram(&model, STAGE_SUMMARY_MODEL_MAX_CHARS)
        ));
    } else {
        lines.push("Model info: n/a".to_string());
    }

    if let Some(usage) = token_usage {
        lines.push(format!(
            "Token usage: total={} / context={}",
            usage.total_tokens, usage.model_context_window
        ));
    } else {
        lines.push("Token usage: n/a".to_string());
    }

    let is_daily_task = is_daily_project_task(tg, accumulator.task_project_id).await;
    let summary_markup = stage_summary_keyboard(trigger, accumulator.task_id, is_daily_task);
    let _ = send_split_telegram_card(tg, lines.join("\n"), summary_markup).await;
    accumulator.finalize_stage();
}

fn stage_summary_keyboard(
    trigger: SummaryTrigger,
    task_id: Option<Uuid>,
    is_daily_task: bool,
) -> Option<teloxide::types::InlineKeyboardMarkup> {
    if !matches!(
        trigger,
        SummaryTrigger::ExecutionFinished | SummaryTrigger::TaskLeftInProgress
    ) {
        return None;
    }

    let task_id = task_id?;
    Some(if is_daily_task {
        keyboard::stage_summary_reply_done_keyboard(task_id)
    } else {
        keyboard::stage_summary_reply_keyboard(task_id)
    })
}

async fn is_daily_project_task(tg: &TelegramContext, task_project_id: Option<Uuid>) -> bool {
    let Some(task_project_id) = task_project_id else {
        return false;
    };

    let daily_project_id = {
        let config = tg.config.read().await;
        config.daily_mode.project_id.clone()
    };

    let Some(daily_project_id) = daily_project_id else {
        return false;
    };

    match Uuid::parse_str(&daily_project_id) {
        Ok(daily_project_id) => task_project_id == daily_project_id,
        Err(err) => {
            tracing::warn!(
                "Invalid daily_mode.project_id in config: {} ({})",
                daily_project_id,
                err
            );
            false
        }
    }
}

async fn send_telegram_card(
    tg: &TelegramContext,
    message: String,
    markup: Option<teloxide::types::InlineKeyboardMarkup>,
) -> Option<MessageId> {
    match format::send_rich_then_plain(&tg.bot, tg.chat_id, &message, markup).await {
        Ok(message) => Some(message.id),
        Err(err) => {
            tracing::warn!("Failed to send telegram run-feed card: {err}");
            None
        }
    }
}

async fn send_split_telegram_card(
    tg: &TelegramContext,
    message: String,
    markup_last: Option<teloxide::types::InlineKeyboardMarkup>,
) -> Vec<MessageId> {
    let chunks = format::split_telegram_chunks(
        &message,
        TELEGRAM_MESSAGE_LIMIT,
        TELEGRAM_CHUNK_INDEX_PREFIX_RESERVE,
    );

    if chunks.len() <= 1 {
        return send_telegram_card(tg, message, markup_last)
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
        if let Some(message_id) =
            send_telegram_card(tg, format!("[{}/{}]\n{}", index + 1, total, chunk), markup).await
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
    plan: String,
}

const TELEGRAM_MESSAGE_LIMIT: usize = format::TELEGRAM_MESSAGE_LIMIT;
const TELEGRAM_CHUNK_INDEX_PREFIX_RESERVE: usize = 20;
const STAGE_SUMMARY_TASK_TITLE_MAX_CHARS: usize = 180;
const STAGE_SUMMARY_MODEL_MAX_CHARS: usize = 280;
const STAGE_SUMMARY_ASSISTANT_MAX_CHARS: usize = TELEGRAM_MESSAGE_LIMIT * 3;

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
                plan: approval.entry.content,
            });
        }
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
    fn plan_review_message_registry_records_replaces_and_takes() {
        let task_id = Uuid::new_v4();
        let mut registry = PlanReviewMessageRegistry::default();

        registry.record(task_id, vec![11, 22]);
        assert_eq!(registry.by_task.get(&task_id), Some(&vec![11, 22]));

        registry.record(task_id, vec![33]);
        assert_eq!(registry.by_task.get(&task_id), Some(&vec![33]));

        assert_eq!(registry.take(task_id), vec![33]);
        assert!(registry.take(task_id).is_empty());
    }

    #[test]
    fn plan_review_message_registry_clear_does_not_erase_subsequent_record() {
        let task_id = Uuid::new_v4();
        let mut registry = PlanReviewMessageRegistry::default();

        // Simulate first InReview: plan found, IDs recorded.
        registry.record(task_id, vec![1, 2, 3]);
        assert_eq!(registry.by_task.get(&task_id), Some(&vec![1, 2, 3]));

        // Simulate second InReview: no plan, clear is called (not record with empty vec).
        registry.clear(task_id);
        assert!(registry.by_task.get(&task_id).is_none());

        // A subsequent record (third InReview with plan) is unaffected.
        registry.record(task_id, vec![4, 5]);
        assert_eq!(registry.take(task_id), vec![4, 5]);
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
    fn stage_summary_keyboard_uses_reply_review_done_for_daily_task() {
        let task_id = Uuid::new_v4();
        let markup = stage_summary_keyboard(SummaryTrigger::ExecutionFinished, Some(task_id), true)
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
                    .expect("reply callback should exist")
            ),
            Some(CallbackAction::FollowUpReply { task_id })
        );
        assert_eq!(
            CallbackAction::decode(
                row[1]["callback_data"]
                    .as_str()
                    .expect("review callback should exist")
            ),
            Some(CallbackAction::CreateReviewTask { task_id })
        );
        assert_eq!(
            CallbackAction::decode(
                row[2]["callback_data"]
                    .as_str()
                    .expect("done callback should exist")
            ),
            Some(CallbackAction::DoneTask { task_id })
        );
    }

    #[test]
    fn stage_summary_keyboard_uses_reply_review_for_non_daily_task() {
        let task_id = Uuid::new_v4();
        let markup =
            stage_summary_keyboard(SummaryTrigger::TaskLeftInProgress, Some(task_id), false)
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
                    .expect("reply callback should exist")
            ),
            Some(CallbackAction::FollowUpReply { task_id })
        );
        assert_eq!(
            CallbackAction::decode(
                row[1]["callback_data"]
                    .as_str()
                    .expect("review callback should exist")
            ),
            Some(CallbackAction::CreateReviewTask { task_id })
        );
    }

    #[test]
    fn stage_summary_keyboard_omits_buttons_for_next_action_trigger() {
        assert!(
            stage_summary_keyboard(SummaryTrigger::NextAction, Some(Uuid::new_v4()), true)
                .is_none()
        );
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
                EntryDeliveryBehavior::SummaryOnly,
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
                EntryDeliveryBehavior::Ignore,
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
    fn tool_use_realtime_only_for_pending_denied_timed_out() {
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
            EntryDeliveryBehavior::SummaryOnly
        );
        assert_eq!(
            classify_entry_behavior(&base(ToolStatus::Failed)),
            EntryDeliveryBehavior::SummaryOnly
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
            summary.assistant_latest.as_deref(),
            Some("Latest assistant summary")
        );
        assert_eq!(summary.system_count, 1);
        assert_eq!(summary.tool_stats.total, 2);
        assert_eq!(summary.tool_stats.success, 1);
        assert_eq!(summary.tool_stats.pending_approval, 1);
        assert_eq!(summary.token_usage.map(|u| u.total_tokens), Some(321));
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
