use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use db::models::{
    execution_process::{ExecutionContext, ExecutionProcess},
    execution_process_telegraph_page::{
        ExecutionProcessTelegraphPage, NewExecutionProcessTelegraphPage,
    },
};
use executors::{
    actions::ExecutorActionType,
    logs::{
        ActionType, NormalizedEntry, NormalizedEntryType, NormalizedEventStream,
        NormalizedLogEvent, ToolStatus,
    },
};
use futures::StreamExt;
use git2::Config as GitConfig;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use reqwest::Client;
use serde::Deserialize;
use sqlx::SqlitePool;
use telegraph_rs::{Account, Node, NodeElement, Telegraph};
use tokio::{
    sync::Mutex,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};
use uuid::Uuid;

use crate::services::config::{Config, TelegramConfig};

const TELEGRAPH_FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const TELEGRAPH_MAX_PAGE_ENTRIES: usize = 120;
const TELEGRAPH_MAX_TEXT_CHARS: usize = 28_000;
const TELEGRAPH_TITLE_MAX_CHARS: usize = 256;
const TELEGRAPH_SECTION_TITLE_MAX_CHARS: usize = 140;
const TELEGRAPH_CODE_BLOCK_MAX_CHARS: usize = 8_000;
const TELEGRAPH_DEFAULT_SHORT_NAME: &str = "vibe_kanban";

#[derive(Debug, Clone)]
pub struct TelegraphMirrorConfig {
    pub access_token: String,
    pub short_name: String,
    pub author_name: Option<String>,
}

impl TelegraphMirrorConfig {
    pub fn from_telegram_config(config: &TelegramConfig) -> Option<Self> {
        if !config.enabled || !config.telegraph_enabled {
            return None;
        }

        Some(Self {
            access_token: normalized_optional_string(config.telegraph_access_token.as_deref())?,
            short_name: TELEGRAPH_DEFAULT_SHORT_NAME.to_string(),
            author_name: git_author_name(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct TelegraphApiResponse<T> {
    ok: bool,
    result: Option<T>,
    error: Option<String>,
}

pub async fn ensure_telegraph_config(config: &mut Config) -> Result<bool> {
    let changed = normalize_hidden_telegraph_fields(&mut config.telegram);
    if !should_initialize_telegraph_account(&config.telegram) {
        return Ok(changed);
    }

    let author_name = git_author_name();
    let account = create_telegraph_account(author_name.as_deref()).await?;
    let access_token = account
        .access_token
        .context("telegraph createAccount response missing access_token")?;

    config.telegram.telegraph_access_token = Some(access_token);
    Ok(true)
}

/// Per-session in-memory telegraph state shared across all follow-up executions.
/// Keyed by `session_id`. Each session lock is independent.
pub type TelegraphSessionStore = Mutex<HashMap<Uuid, Arc<Mutex<SessionTelegraphState>>>>;

pub fn new_telegraph_session_store() -> Arc<TelegraphSessionStore> {
    Arc::new(Mutex::new(HashMap::new()))
}

/// In-memory state for all telegraph pages belonging to one session.
pub struct SessionTelegraphState {
    /// Ordered list of execution_process_ids in the order they were first seen.
    execution_order: Vec<Uuid>,
    /// Per-execution normalized entries (index → entry).
    entries_by_execution: HashMap<Uuid, BTreeMap<usize, NormalizedEntry>>,
    /// Per-execution user input to prepend as a "User" section.
    user_inputs: HashMap<Uuid, String>,
    /// Current telegraph page metadata (URL/path/title) for the session.
    page_meta: Vec<TelegraphPageMeta>,
}

impl SessionTelegraphState {
    fn new() -> Self {
        Self {
            execution_order: Vec::new(),
            entries_by_execution: HashMap::new(),
            user_inputs: HashMap::new(),
            page_meta: Vec::new(),
        }
    }

    /// Merge local entries for one execution into the session state.
    fn merge_execution_entries(
        &mut self,
        execution_process_id: Uuid,
        local_entries: &BTreeMap<usize, NormalizedEntry>,
        user_input: Option<&str>,
    ) {
        if !self.execution_order.contains(&execution_process_id) {
            self.execution_order.push(execution_process_id);
        }
        let slot = self
            .entries_by_execution
            .entry(execution_process_id)
            .or_default();
        *slot = local_entries.clone();
        if let Some(input) = user_input {
            self.user_inputs
                .entry(execution_process_id)
                .or_insert_with(|| input.to_owned());
        }
    }

    /// Render all entries in execution order, producing one `Vec<Node>` per page.
    fn render_all_page_contents(&self) -> Option<Vec<String>> {
        let rendered_entries: Vec<RenderedEntry> = self
            .execution_order
            .iter()
            .flat_map(|eid| {
                let user_entry = self.user_inputs.get(eid).and_then(|input| {
                    if input.is_empty() {
                        None
                    } else {
                        Some(RenderedEntry {
                            nodes: section_nodes("User", markdown_nodes(input)),
                        })
                    }
                });
                let agent_entries = self
                    .entries_by_execution
                    .get(eid)
                    .map(|entries| {
                        entries
                            .values()
                            .filter_map(render_entry_nodes)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                user_entry.into_iter().chain(agent_entries)
            })
            .collect();

        if rendered_entries.is_empty() {
            return None;
        }

        let pages = paginate_rendered_entries(rendered_entries);
        let mut contents = Vec::with_capacity(pages.len());
        for nodes in pages {
            match serde_json::to_string(&nodes)
                .context("failed to serialize telegraph node content")
            {
                Ok(content) => contents.push(content),
                Err(err) => {
                    tracing::warn!("Telegraph content serialization failed: {}", err);
                    return None;
                }
            }
        }

        Some(contents)
    }
}

#[derive(Debug, Clone)]
pub struct TelegraphPageMeta {
    pub url: String,
    pub path: String,
    pub title: String,
}

pub async fn get_execution_process_telegraph_urls(
    pool: &SqlitePool,
    execution_process_id: &Uuid,
) -> Vec<String> {
    match ExecutionProcessTelegraphPage::find_urls_by_execution_process_id(
        pool,
        *execution_process_id,
    )
    .await
    {
        Ok(urls) => urls,
        Err(err) => {
            tracing::warn!(
                "Failed to load telegraph URLs for execution_process_id={}: {}",
                execution_process_id,
                err
            );
            Vec::new()
        }
    }
}

pub async fn append_stage_summary_pages(
    pool: &SqlitePool,
    execution_process_id: Uuid,
    config: &TelegramConfig,
    entries: Vec<NormalizedEntry>,
) -> Vec<String> {
    let Some(mirror_config) = TelegraphMirrorConfig::from_telegram_config(config) else {
        return Vec::new();
    };
    let ctx = match ExecutionProcess::load_context(pool, execution_process_id).await {
        Ok(ctx) => ctx,
        Err(err) => {
            tracing::warn!(
                "Failed to load execution context for Telegraph summary append execution {}: {}",
                execution_process_id,
                err
            );
            return Vec::new();
        }
    };
    let existing_urls = get_execution_process_telegraph_urls(pool, &execution_process_id).await;
    if !existing_urls.is_empty() {
        return existing_urls;
    }

    let user_input = existing_urls
        .is_empty()
        .then(|| user_input_for_context(&ctx))
        .flatten();
    let Some(contents) = render_entries_page_contents(entries, user_input.as_deref()) else {
        return Vec::new();
    };
    let client = match TelegraphRsClient::new(&mirror_config).await {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(
                "Failed to initialize Telegraph summary append for execution {}: {}",
                execution_process_id,
                err
            );
            return Vec::new();
        }
    };
    let page_title = page_title_for_execution(&ctx.task.title, execution_process_id);

    append_stage_summary_pages_with_client(
        pool,
        execution_process_id,
        ctx.execution_process.session_id,
        &page_title,
        contents,
        &client,
    )
    .await
}

async fn append_stage_summary_pages_with_client(
    pool: &SqlitePool,
    execution_process_id: Uuid,
    session_id: Uuid,
    page_title: &str,
    contents: Vec<String>,
    client: &dyn TelegraphClient,
) -> Vec<String> {
    let mut metas = match find_session_telegraph_pages(pool, session_id).await {
        Ok(pages) => pages,
        Err(err) => {
            tracing::warn!(
                "Failed to find existing Telegraph pages for session {}: {}",
                session_id,
                err
            );
            Vec::new()
        }
    };

    let mut content_iter = contents.into_iter();
    if let Some(first_content) = content_iter.next() {
        match metas.last().cloned() {
            Some(last_meta) => {
                let update_result =
                    append_content_to_existing_page(client, &last_meta, &first_content).await;
                match update_result {
                    Ok(meta) => {
                        if let Some(last) = metas.last_mut() {
                            *last = meta;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            "Telegraph page append failed for execution {} path {}: {}",
                            execution_process_id,
                            last_meta.path,
                            err
                        );
                        match client
                            .create_page(
                                &page_title_for_page(page_title, metas.len()),
                                &first_content,
                            )
                            .await
                        {
                            Ok(meta) => metas.push(meta),
                            Err(create_err) => {
                                tracing::warn!(
                                    "Telegraph fallback create failed for execution {}: {}",
                                    execution_process_id,
                                    create_err
                                );
                            }
                        }
                    }
                }
            }
            None => {
                match client
                    .create_page(&page_title_for_page(page_title, 0), &first_content)
                    .await
                {
                    Ok(meta) => metas.push(meta),
                    Err(err) => {
                        tracing::warn!(
                            "Telegraph stage summary create failed for execution {} page 0: {}",
                            execution_process_id,
                            err
                        );
                    }
                }
            }
        };
    }

    for content in content_iter {
        let page_index = metas.len();
        let title = page_title_for_page(page_title, page_index);
        match client.create_page(&title, &content).await {
            Ok(meta) => metas.push(meta),
            Err(err) => {
                tracing::warn!(
                    "Telegraph stage summary create failed for execution {} page {}: {}",
                    execution_process_id,
                    page_index,
                    err
                );
                break;
            }
        }
    }

    let urls = metas
        .iter()
        .map(|meta| meta.url.clone())
        .collect::<Vec<_>>();
    persist_replaced_page_meta(pool, execution_process_id, &metas).await;
    urls
}

async fn append_content_to_existing_page(
    client: &dyn TelegraphClient,
    meta: &TelegraphPageMeta,
    content: &str,
) -> Result<TelegraphPageMeta> {
    let mut existing_nodes = client.get_page_content(&meta.path).await?;
    let mut new_nodes: Vec<Node> =
        serde_json::from_str(content).context("failed to parse rendered Telegraph content")?;

    if !existing_nodes.is_empty() && !new_nodes.is_empty() {
        existing_nodes.push(horizontal_rule());
    }
    existing_nodes.append(&mut new_nodes);

    let merged_content = serde_json::to_string(&existing_nodes)
        .context("failed to serialize merged Telegraph content")?;
    client
        .edit_page(&meta.path, &meta.title, &merged_content)
        .await
}

async fn find_session_telegraph_pages(
    pool: &SqlitePool,
    session_id: Uuid,
) -> Result<Vec<TelegraphPageMeta>, sqlx::Error> {
    let processes = ExecutionProcess::find_by_session_id(pool, session_id, false).await?;
    for process in processes.into_iter().rev() {
        let pages =
            ExecutionProcessTelegraphPage::find_by_execution_process_id(pool, process.id).await?;
        if !pages.is_empty() {
            return Ok(pages
                .into_iter()
                .map(|page| TelegraphPageMeta {
                    url: page.url,
                    path: page.path,
                    title: page.title,
                })
                .collect());
        }
    }

    Ok(Vec::new())
}

fn user_input_for_context(ctx: &ExecutionContext) -> Option<String> {
    let action = ctx.execution_process.executor_action().ok()?;
    match &action.typ {
        ExecutorActionType::CodingAgentInitialRequest(_) => {
            let title = &ctx.task.title;
            let detail = ctx.task.description.as_deref().unwrap_or("");
            let text = if detail.is_empty() {
                title.clone()
            } else {
                format!("{}\n{}", title, detail)
            };
            Some(text)
        }
        ExecutorActionType::CodingAgentFollowUpRequest(req) => Some(req.prompt.clone()),
        _ => None,
    }
}

#[async_trait]
pub trait TelegraphClient: Send + Sync {
    async fn create_page(&self, title: &str, content: &str) -> Result<TelegraphPageMeta>;
    async fn edit_page(&self, path: &str, title: &str, content: &str) -> Result<TelegraphPageMeta>;
    async fn get_page_content(&self, path: &str) -> Result<Vec<Node>>;
}

#[derive(Clone)]
struct TelegraphRsClient {
    telegraph: Telegraph,
}

impl TelegraphRsClient {
    async fn new(config: &TelegraphMirrorConfig) -> Result<Self> {
        let mut builder = Telegraph::new(&config.short_name).access_token(&config.access_token);
        if let Some(author_name) = config.author_name.as_deref() {
            builder = builder.author_name(author_name);
        }
        let telegraph = builder
            .create()
            .await
            .context("failed to initialize telegraph account")?;

        Ok(Self { telegraph })
    }
}

#[async_trait]
impl TelegraphClient for TelegraphRsClient {
    async fn create_page(&self, title: &str, content: &str) -> Result<TelegraphPageMeta> {
        let page = self
            .telegraph
            .create_page(title, &content, false)
            .await
            .context("create_page failed")?;

        Ok(page_meta_from_page(page))
    }

    async fn edit_page(&self, path: &str, title: &str, content: &str) -> Result<TelegraphPageMeta> {
        let page = self
            .telegraph
            .edit_page(path, title, &content, false)
            .await
            .context("edit_page failed")?;

        Ok(page_meta_from_page(page))
    }

    async fn get_page_content(&self, path: &str) -> Result<Vec<Node>> {
        let page = Telegraph::get_page(path, true)
            .await
            .context("get_page failed")?;

        Ok(page.content.unwrap_or_default())
    }
}

pub fn spawn_telegraph_log_consumer(
    session_id: Uuid,
    execution_process_id: Uuid,
    page_title: String,
    stream: NormalizedEventStream,
    config: TelegraphMirrorConfig,
    db_pool: SqlitePool,
    session_store: Arc<TelegraphSessionStore>,
    user_input: Option<String>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let client = match TelegraphRsClient::new(&config).await {
            Ok(client) => Arc::new(client) as Arc<dyn TelegraphClient>,
            Err(err) => {
                tracing::warn!(
                    "Failed to initialize telegraph mirror for execution {}: {}",
                    execution_process_id,
                    err
                );
                return;
            }
        };

        run_telegraph_log_consumer_with_client(
            session_id,
            execution_process_id,
            page_title,
            stream,
            client,
            Some(db_pool),
            session_store,
            user_input,
        )
        .await;
    })
}

async fn run_telegraph_log_consumer_with_client(
    session_id: Uuid,
    execution_process_id: Uuid,
    page_title: String,
    mut stream: NormalizedEventStream,
    client: Arc<dyn TelegraphClient>,
    db_pool: Option<SqlitePool>,
    session_store: Arc<TelegraphSessionStore>,
    user_input: Option<String>,
) {
    // Get or create per-session state (shared, locked per session).
    let session_state: Arc<Mutex<SessionTelegraphState>> = {
        let mut store = session_store.lock().await;
        store
            .entry(session_id)
            .or_insert_with(|| Arc::new(Mutex::new(SessionTelegraphState::new())))
            .clone()
    };

    let mut consumer = TelegraphLogConsumer::new(
        execution_process_id,
        page_title,
        client,
        db_pool,
        session_state,
        user_input,
    );
    let mut flush_interval = tokio::time::interval_at(
        Instant::now() + TELEGRAPH_FLUSH_INTERVAL,
        TELEGRAPH_FLUSH_INTERVAL,
    );
    flush_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = flush_interval.tick() => {
                consumer.flush_pending_updates().await;
            }
            next = stream.next() => {
                match next {
                    Some(Ok(NormalizedLogEvent::Finished)) => {
                        consumer.flush_pending_updates().await;
                        break;
                    }
                    Some(Ok(event)) => {
                        consumer.apply_event(event);
                        if consumer.no_pages_yet() && consumer.has_renderable_content() {
                            consumer.flush_pending_updates().await;
                        }
                    }
                    Some(Err(err)) => {
                        tracing::warn!(
                            "Telegraph consumer stream error for execution {}: {}",
                            execution_process_id,
                            err
                        );
                        consumer.flush_pending_updates().await;
                        break;
                    }
                    None => {
                        consumer.flush_pending_updates().await;
                        break;
                    }
                }
            }
        }
    }
}

struct TelegraphLogConsumer {
    execution_process_id: Uuid,
    page_title: String,
    client: Arc<dyn TelegraphClient>,
    db_pool: Option<SqlitePool>,
    /// Local entries for the current execution only (index → entry).
    local_entries: BTreeMap<usize, NormalizedEntry>,
    /// Shared session-level state (page_meta, all executions' entries).
    session_state: Arc<Mutex<SessionTelegraphState>>,
    dirty: bool,
    /// True after the first successful flush (pages exist).
    flushed_once: bool,
    /// User-provided input to display at the top of this execution's section.
    user_input: Option<String>,
}

impl TelegraphLogConsumer {
    fn new(
        execution_process_id: Uuid,
        page_title: String,
        client: Arc<dyn TelegraphClient>,
        db_pool: Option<SqlitePool>,
        session_state: Arc<Mutex<SessionTelegraphState>>,
        user_input: Option<String>,
    ) -> Self {
        Self {
            execution_process_id,
            page_title,
            client,
            db_pool,
            local_entries: BTreeMap::new(),
            session_state,
            dirty: false,
            flushed_once: false,
            user_input,
        }
    }

    fn apply_event(&mut self, event: NormalizedLogEvent) {
        match event {
            NormalizedLogEvent::UpsertEntry { index, entry } => {
                self.local_entries.insert(index, entry);
                self.dirty = true;
            }
            NormalizedLogEvent::RemoveEntry { index } => {
                if self.local_entries.remove(&index).is_some() {
                    self.dirty = true;
                }
            }
            NormalizedLogEvent::Finished => {}
        }
    }

    fn has_renderable_content(&self) -> bool {
        self.local_entries
            .values()
            .any(|e| render_entry_nodes(e).is_some())
    }

    fn no_pages_yet(&self) -> bool {
        !self.flushed_once
    }

    async fn flush_pending_updates(&mut self) {
        if !self.dirty {
            return;
        }

        let mut session = self.session_state.lock().await;

        // Merge local entries into session state.
        session.merge_execution_entries(
            self.execution_process_id,
            &self.local_entries,
            self.user_input.as_deref(),
        );

        let Some(contents) = session.render_all_page_contents() else {
            return;
        };

        let page_count = contents.len();
        let mut next_page_meta = Vec::with_capacity(page_count);

        for (page_index, content) in contents.iter().enumerate() {
            let title = page_title_for_page(&self.page_title, page_index);
            let update_result = match session.page_meta.get(page_index) {
                Some(meta) => self.client.edit_page(&meta.path, &title, content).await,
                None => self.client.create_page(&title, content).await,
            };

            match update_result {
                Ok(meta) => next_page_meta.push(meta),
                Err(err) => {
                    tracing::warn!(
                        "Telegraph mirror update failed for execution {} page {}: {}",
                        self.execution_process_id,
                        page_index,
                        err
                    );
                    return;
                }
            }
        }

        session.page_meta = next_page_meta;
        self.dirty = false;
        self.flushed_once = true;

        // Persist URL cache for this execution_process_id.
        self.persist_page_meta(&session.page_meta).await;
    }

    async fn persist_page_meta(&self, meta: &[TelegraphPageMeta]) {
        let Some(db_pool) = self.db_pool.as_ref() else {
            return;
        };

        let pages = meta
            .iter()
            .enumerate()
            .map(|(page_index, page)| NewExecutionProcessTelegraphPage {
                page_index: page_index as i64,
                url: &page.url,
                path: &page.path,
                title: &page.title,
            })
            .collect::<Vec<_>>();

        if let Err(err) =
            ExecutionProcessTelegraphPage::replace_all(db_pool, self.execution_process_id, &pages)
                .await
        {
            tracing::warn!(
                "Failed to persist telegraph page metadata for execution {}: {}",
                self.execution_process_id,
                err
            );
        }
    }
}

fn render_entries_page_contents(
    entries: Vec<NormalizedEntry>,
    user_input: Option<&str>,
) -> Option<Vec<String>> {
    let has_user_message = entries
        .iter()
        .any(|entry| matches!(entry.entry_type, NormalizedEntryType::UserMessage));
    let mut rendered_entries = Vec::new();

    if !has_user_message && let Some(input) = user_input.filter(|input| !input.trim().is_empty()) {
        rendered_entries.push(RenderedEntry {
            nodes: section_nodes("User", markdown_nodes(input)),
        });
    }

    rendered_entries.extend(entries.iter().filter_map(render_entry_nodes));

    if rendered_entries.is_empty() {
        return None;
    }

    let pages = paginate_rendered_entries(rendered_entries);
    let mut contents = Vec::with_capacity(pages.len());
    for nodes in pages {
        match serde_json::to_string(&nodes).context("failed to serialize telegraph node content") {
            Ok(content) => contents.push(content),
            Err(err) => {
                tracing::warn!("Telegraph content serialization failed: {}", err);
                return None;
            }
        }
    }

    Some(contents)
}

async fn persist_replaced_page_meta(
    pool: &SqlitePool,
    execution_process_id: Uuid,
    meta: &[TelegraphPageMeta],
) {
    let pages = meta
        .iter()
        .map(|page| NewExecutionProcessTelegraphPage {
            page_index: 0,
            url: &page.url,
            path: &page.path,
            title: &page.title,
        })
        .collect::<Vec<_>>();

    if let Err(err) =
        ExecutionProcessTelegraphPage::replace_all(pool, execution_process_id, &pages).await
    {
        tracing::warn!(
            "Failed to persist telegraph page metadata for execution {}: {}",
            execution_process_id,
            err
        );
    }
}

fn paginate_rendered_entries(rendered_entries: Vec<RenderedEntry>) -> Vec<Vec<Node>> {
    let mut pages = Vec::new();
    let mut current_nodes = Vec::new();
    let mut current_chars = 0usize;
    let mut current_entries = 0usize;

    for rendered in rendered_entries {
        let rendered_char_count = rendered.char_count();
        let separator_chars = usize::from(current_entries > 0);
        let needs_new_page = current_entries >= TELEGRAPH_MAX_PAGE_ENTRIES
            || (current_entries > 0
                && current_chars + separator_chars + rendered_char_count
                    > TELEGRAPH_MAX_TEXT_CHARS);

        if needs_new_page {
            pages.push(current_nodes);
            current_nodes = Vec::new();
            current_chars = 0;
            current_entries = 0;
        }

        if current_entries > 0 {
            current_nodes.push(horizontal_rule());
            current_chars += 1;
        }

        current_chars += rendered_char_count;
        current_entries += 1;
        current_nodes.extend(rendered.nodes);
    }

    if !current_nodes.is_empty() {
        pages.push(current_nodes);
    }

    pages
}

fn page_title_for_page(base_title: &str, page_index: usize) -> String {
    let suffix = format!(" ({})", page_index + 1);
    let available_chars = TELEGRAPH_TITLE_MAX_CHARS.saturating_sub(suffix.chars().count());
    let trimmed_base = truncate_chars(base_title, available_chars.max(1));
    format!("{trimmed_base}{suffix}")
}

#[derive(Clone)]
struct RenderedEntry {
    nodes: Vec<Node>,
}

impl RenderedEntry {
    fn char_count(&self) -> usize {
        self.nodes.iter().map(node_text_len).sum()
    }
}

#[derive(Clone, Copy)]
enum ExpectedEnd {
    Paragraph,
    Heading,
    BlockQuote,
    CodeBlock,
    List,
    Item,
    Emphasis,
    Strong,
    Strikethrough,
    Link,
    Image,
}

fn render_entry_nodes(entry: &NormalizedEntry) -> Option<RenderedEntry> {
    let nodes = match &entry.entry_type {
        NormalizedEntryType::UserMessage => section_nodes("User", markdown_nodes(&entry.content)),
        NormalizedEntryType::AssistantMessage => {
            section_nodes("Assistant", markdown_nodes(&entry.content))
        }
        NormalizedEntryType::SystemMessage => {
            section_nodes("System", markdown_nodes(&entry.content))
        }
        NormalizedEntryType::Thinking => section_nodes("Thinking", markdown_nodes(&entry.content)),
        NormalizedEntryType::UserFeedback { denied_tool } => section_nodes(
            &format!("Denied By User: {denied_tool}"),
            markdown_nodes(&entry.content),
        ),
        NormalizedEntryType::ErrorMessage { .. } => {
            section_nodes("Error", vec![code_block_node(&entry.content)])
        }
        NormalizedEntryType::ToolUse {
            tool_name,
            action_type,
            status,
        } => render_tool_use_nodes(tool_name, action_type, status),
        NormalizedEntryType::NextAction {
            failed,
            execution_processes,
            needs_setup,
        } => section_nodes(
            "Next Action",
            vec![paragraph_text(&format!(
                "failed={}, execution_processes={}, needs_setup={}",
                failed, execution_processes, needs_setup
            ))],
        ),
        NormalizedEntryType::Loading | NormalizedEntryType::TokenUsageInfo(_) => return None,
    };

    if nodes.is_empty() {
        None
    } else {
        Some(RenderedEntry { nodes })
    }
}

fn render_tool_use_nodes(
    tool_name: &str,
    action_type: &ActionType,
    status: &ToolStatus,
) -> Vec<Node> {
    let title = format!(
        "{} [{}]",
        truncate_chars(tool_name, TELEGRAPH_SECTION_TITLE_MAX_CHARS),
        tool_status_label(status)
    );

    let mut body = match action_type {
        ActionType::FileRead { path } => vec![labeled_code_block("Path", path)],
        ActionType::FileEdit { path, changes } => {
            let mut nodes = vec![labeled_code_block("Path", path)];
            for change in changes {
                nodes.extend(render_file_change_nodes(change));
            }
            nodes
        }
        ActionType::CommandRun { command, result } => {
            let mut nodes = vec![labeled_code_block("Args", command)];
            if let Some(result) = result {
                if let Some(exit_status) = &result.exit_status {
                    nodes.push(paragraph_text(&format!(
                        "Exit status: {}",
                        format_exit_status(exit_status)
                    )));
                }
                let should_render_output = !tool_name.eq_ignore_ascii_case("bash");
                if should_render_output {
                    if let Some(output) = result.output.as_deref() {
                        nodes.push(label_paragraph("Output"));
                        nodes.push(code_block_node(output));
                    }
                }
            }
            nodes
        }
        ActionType::Search { query } => vec![labeled_code_block("Query", query)],
        ActionType::WebFetch { url } => vec![labeled_link_paragraph("URL", url)],
        ActionType::Tool {
            arguments, result, ..
        } => {
            let mut nodes = Vec::new();
            if let Some(arguments) = arguments {
                nodes.push(label_paragraph("Args"));
                nodes.push(code_block_node(&pretty_json(arguments)));
            }
            if let Some(result) = result {
                nodes.push(label_paragraph("Result"));
                nodes.extend(render_tool_result_nodes(result));
            }
            nodes
        }
        ActionType::TaskCreate { description } => markdown_nodes(description),
        ActionType::PlanPresentation { plan } => markdown_nodes(plan),
        ActionType::TodoManagement { todos, operation } => {
            let mut items = Vec::new();
            for todo in todos {
                items.push(element(
                    "li",
                    None,
                    vec![Node::Text(format!(
                        "[{}] {}{}",
                        todo.status,
                        todo.content,
                        todo.priority
                            .as_ref()
                            .map(|priority| format!(" ({priority})"))
                            .unwrap_or_default()
                    ))],
                ));
            }
            let mut nodes = vec![paragraph_text(&format!("Operation: {operation}"))];
            if !items.is_empty() {
                nodes.push(element("ul", None, items));
            }
            nodes
        }
        ActionType::Other { description } => markdown_nodes(description),
    };

    if body.is_empty() {
        body.push(paragraph_text("No details."));
    }

    section_nodes(&title, body)
}

fn render_file_change_nodes(change: &executors::logs::FileChange) -> Vec<Node> {
    match change {
        executors::logs::FileChange::Write { content } => {
            vec![label_paragraph("Write"), code_block_node(content)]
        }
        executors::logs::FileChange::Delete => vec![paragraph_text("Delete")],
        executors::logs::FileChange::Rename { new_path } => {
            vec![labeled_code_block("Rename To", new_path)]
        }
        executors::logs::FileChange::Edit { unified_diff, .. } => {
            vec![label_paragraph("Diff"), code_block_node(unified_diff)]
        }
    }
}

fn render_tool_result_nodes(result: &executors::logs::ToolResult) -> Vec<Node> {
    match result.r#type {
        executors::logs::ToolResultValueType::Markdown => result
            .value
            .as_str()
            .map(markdown_nodes)
            .filter(|nodes| !nodes.is_empty())
            .unwrap_or_else(|| vec![code_block_node(&pretty_json(&result.value))]),
        executors::logs::ToolResultValueType::Json => {
            vec![code_block_node(&pretty_json(&result.value))]
        }
    }
}

fn markdown_nodes(markdown: &str) -> Vec<Node> {
    let trimmed = markdown.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let events = Parser::new_ext(trimmed, Options::ENABLE_STRIKETHROUGH).collect::<Vec<_>>();
    let mut cursor = 0;
    parse_block_nodes(&events, &mut cursor, None)
}

fn parse_block_nodes<'a>(
    events: &[Event<'a>],
    cursor: &mut usize,
    until: Option<ExpectedEnd>,
) -> Vec<Node> {
    let mut nodes = Vec::new();

    while *cursor < events.len() {
        match &events[*cursor] {
            Event::End(tag_end) if until.is_some_and(|expected| matches_end(tag_end, expected)) => {
                *cursor += 1;
                break;
            }
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    *cursor += 1;
                    let children = parse_inline_nodes(events, cursor, ExpectedEnd::Paragraph);
                    if !children.is_empty() {
                        nodes.push(element("p", None, children));
                    }
                }
                Tag::Heading { level, .. } => {
                    *cursor += 1;
                    let children = parse_inline_nodes(events, cursor, ExpectedEnd::Heading);
                    if !children.is_empty() {
                        nodes.push(element(heading_tag_name(*level), None, children));
                    }
                }
                Tag::BlockQuote(_) => {
                    *cursor += 1;
                    let children = parse_block_nodes(events, cursor, Some(ExpectedEnd::BlockQuote));
                    if !children.is_empty() {
                        nodes.push(element("blockquote", None, children));
                    }
                }
                Tag::CodeBlock(_) => {
                    *cursor += 1;
                    let text = collect_text_until(events, cursor, ExpectedEnd::CodeBlock);
                    if !text.is_empty() {
                        nodes.push(code_block_node(&text));
                    }
                }
                Tag::List(start) => {
                    *cursor += 1;
                    let children = parse_block_nodes(events, cursor, Some(ExpectedEnd::List));
                    if !children.is_empty() {
                        let tag_name = if start.is_some() { "ol" } else { "ul" };
                        nodes.push(element(tag_name, None, children));
                    }
                }
                Tag::Item => {
                    *cursor += 1;
                    let children = parse_block_nodes(events, cursor, Some(ExpectedEnd::Item));
                    if !children.is_empty() {
                        nodes.push(element("li", None, children));
                    }
                }
                _ => {
                    *cursor += 1;
                    if let Some(expected_end) = expected_end_for_tag(tag) {
                        let text = collect_text_until(events, cursor, expected_end);
                        if !text.is_empty() {
                            nodes.push(paragraph_text(&text));
                        }
                    }
                }
            },
            Event::Text(text) => {
                let text = text.trim();
                if !text.is_empty() {
                    nodes.push(paragraph_text(text));
                }
                *cursor += 1;
            }
            Event::Code(text) => {
                nodes.push(element(
                    "p",
                    None,
                    vec![element("code", None, vec![Node::Text(text.to_string())])],
                ));
                *cursor += 1;
            }
            Event::Rule => {
                nodes.push(horizontal_rule());
                *cursor += 1;
            }
            Event::SoftBreak | Event::HardBreak => {
                *cursor += 1;
            }
            Event::Html(text)
            | Event::InlineHtml(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text) => {
                let text = text.trim();
                if !text.is_empty() {
                    nodes.push(paragraph_text(text));
                }
                *cursor += 1;
            }
            Event::TaskListMarker(checked) => {
                nodes.push(paragraph_text(if *checked { "[x]" } else { "[ ]" }));
                *cursor += 1;
            }
            Event::FootnoteReference(name) => {
                nodes.push(paragraph_text(&format!("[{name}]")));
                *cursor += 1;
            }
            Event::End(_) => {
                *cursor += 1;
            }
        }
    }

    nodes
}

fn parse_inline_nodes<'a>(
    events: &[Event<'a>],
    cursor: &mut usize,
    until: ExpectedEnd,
) -> Vec<Node> {
    let mut nodes = Vec::new();

    while *cursor < events.len() {
        match &events[*cursor] {
            Event::End(tag_end) if matches_end(tag_end, until) => {
                *cursor += 1;
                break;
            }
            Event::Start(tag) => match tag {
                Tag::Emphasis => {
                    *cursor += 1;
                    let children = parse_inline_nodes(events, cursor, ExpectedEnd::Emphasis);
                    if !children.is_empty() {
                        nodes.push(element("em", None, children));
                    }
                }
                Tag::Strong => {
                    *cursor += 1;
                    let children = parse_inline_nodes(events, cursor, ExpectedEnd::Strong);
                    if !children.is_empty() {
                        nodes.push(element("strong", None, children));
                    }
                }
                Tag::Strikethrough => {
                    *cursor += 1;
                    let children = parse_inline_nodes(events, cursor, ExpectedEnd::Strikethrough);
                    if !children.is_empty() {
                        nodes.push(element("s", None, children));
                    }
                }
                Tag::Link { dest_url, .. } => {
                    let href = dest_url.to_string();
                    *cursor += 1;
                    let children = parse_inline_nodes(events, cursor, ExpectedEnd::Link);
                    let children = if children.is_empty() {
                        vec![Node::Text(href.clone())]
                    } else {
                        children
                    };
                    nodes.push(element("a", href_attrs("href", &href), children));
                }
                Tag::Image { dest_url, .. } => {
                    let src = dest_url.to_string();
                    *cursor += 1;
                    let _alt = parse_inline_nodes(events, cursor, ExpectedEnd::Image);
                    nodes.push(element("img", href_attrs("src", &src), Vec::new()));
                }
                _ => {
                    *cursor += 1;
                    if let Some(expected_end) = expected_end_for_tag(tag) {
                        let text = collect_text_until(events, cursor, expected_end);
                        if !text.is_empty() {
                            nodes.push(Node::Text(text));
                        }
                    }
                }
            },
            Event::Text(text) => {
                nodes.push(Node::Text(text.to_string()));
                *cursor += 1;
            }
            Event::Code(text) => {
                nodes.push(element("code", None, vec![Node::Text(text.to_string())]));
                *cursor += 1;
            }
            Event::SoftBreak | Event::HardBreak => {
                nodes.push(line_break());
                *cursor += 1;
            }
            Event::Html(text)
            | Event::InlineHtml(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text) => {
                nodes.push(Node::Text(text.to_string()));
                *cursor += 1;
            }
            Event::TaskListMarker(checked) => {
                nodes.push(Node::Text(if *checked {
                    "[x] ".to_string()
                } else {
                    "[ ] ".to_string()
                }));
                *cursor += 1;
            }
            Event::FootnoteReference(name) => {
                nodes.push(Node::Text(format!("[{name}]")));
                *cursor += 1;
            }
            Event::Rule => {
                nodes.push(Node::Text("----".to_string()));
                *cursor += 1;
            }
            Event::End(_) => {
                *cursor += 1;
            }
        }
    }

    nodes
}

fn collect_text_until<'a>(events: &[Event<'a>], cursor: &mut usize, until: ExpectedEnd) -> String {
    let mut out = String::new();

    while *cursor < events.len() {
        match &events[*cursor] {
            Event::End(tag_end) if matches_end(tag_end, until) => {
                *cursor += 1;
                break;
            }
            Event::Start(tag) => {
                *cursor += 1;
                if let Some(expected_end) = expected_end_for_tag(tag) {
                    out.push_str(&collect_text_until(events, cursor, expected_end));
                }
            }
            Event::Text(text)
            | Event::Code(text)
            | Event::Html(text)
            | Event::InlineHtml(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text) => {
                out.push_str(text);
                *cursor += 1;
            }
            Event::SoftBreak | Event::HardBreak => {
                out.push('\n');
                *cursor += 1;
            }
            Event::Rule => {
                out.push_str("\n----\n");
                *cursor += 1;
            }
            Event::TaskListMarker(checked) => {
                out.push_str(if *checked { "[x] " } else { "[ ] " });
                *cursor += 1;
            }
            Event::FootnoteReference(name) => {
                out.push_str(&format!("[{name}]"));
                *cursor += 1;
            }
            Event::End(_) => {
                *cursor += 1;
            }
        }
    }

    out
}

fn expected_end_for_tag(tag: &Tag<'_>) -> Option<ExpectedEnd> {
    match tag {
        Tag::Paragraph => Some(ExpectedEnd::Paragraph),
        Tag::Heading { .. } => Some(ExpectedEnd::Heading),
        Tag::BlockQuote(_) => Some(ExpectedEnd::BlockQuote),
        Tag::CodeBlock(_) => Some(ExpectedEnd::CodeBlock),
        Tag::List(_) => Some(ExpectedEnd::List),
        Tag::Item => Some(ExpectedEnd::Item),
        Tag::Emphasis => Some(ExpectedEnd::Emphasis),
        Tag::Strong => Some(ExpectedEnd::Strong),
        Tag::Strikethrough => Some(ExpectedEnd::Strikethrough),
        Tag::Link { .. } => Some(ExpectedEnd::Link),
        Tag::Image { .. } => Some(ExpectedEnd::Image),
        _ => None,
    }
}

fn matches_end(tag_end: &TagEnd, expected: ExpectedEnd) -> bool {
    matches!(
        (tag_end, expected),
        (TagEnd::Paragraph, ExpectedEnd::Paragraph)
            | (TagEnd::Heading(_), ExpectedEnd::Heading)
            | (TagEnd::BlockQuote(_), ExpectedEnd::BlockQuote)
            | (TagEnd::CodeBlock, ExpectedEnd::CodeBlock)
            | (TagEnd::List(_), ExpectedEnd::List)
            | (TagEnd::Item, ExpectedEnd::Item)
            | (TagEnd::Emphasis, ExpectedEnd::Emphasis)
            | (TagEnd::Strong, ExpectedEnd::Strong)
            | (TagEnd::Strikethrough, ExpectedEnd::Strikethrough)
            | (TagEnd::Link, ExpectedEnd::Link)
            | (TagEnd::Image, ExpectedEnd::Image)
    )
}

fn heading_tag_name(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3 => "h3",
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => "h4",
    }
}

fn tool_status_label(status: &ToolStatus) -> &'static str {
    match status {
        ToolStatus::Created => "created",
        ToolStatus::Success => "success",
        ToolStatus::Failed => "failed",
        ToolStatus::Denied { .. } => "denied",
        ToolStatus::PendingApproval { .. } => "pending_approval",
        ToolStatus::TimedOut => "timed_out",
    }
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }

    let truncated: String = input.chars().take(max_chars).collect();
    format!("{truncated}…")
}

pub fn page_title_for_execution(task_title: &str, execution_process_id: Uuid) -> String {
    let normalized = normalized_optional_string(Some(task_title));
    if let Some(title) = normalized {
        return truncate_chars(&title, TELEGRAPH_TITLE_MAX_CHARS);
    }

    let id = execution_process_id.to_string();
    format!("Execution {}", &id[..8])
}

fn section_nodes(title: &str, mut body: Vec<Node>) -> Vec<Node> {
    let mut nodes = vec![element(
        "h4",
        None,
        vec![Node::Text(truncate_chars(
            title,
            TELEGRAPH_SECTION_TITLE_MAX_CHARS,
        ))],
    )];
    nodes.append(&mut body);
    nodes
}

fn labeled_code_block(label: &str, text: &str) -> Node {
    code_block_node(&format!("{label}:\n{text}"))
}

fn labeled_link_paragraph(label: &str, url: &str) -> Node {
    element(
        "p",
        None,
        vec![
            Node::Text(format!("{label}: ")),
            element(
                "a",
                href_attrs("href", url),
                vec![Node::Text(url.to_string())],
            ),
        ],
    )
}

fn label_paragraph(label: &str) -> Node {
    element(
        "p",
        None,
        vec![element("strong", None, vec![Node::Text(label.to_string())])],
    )
}

fn paragraph_text(text: &str) -> Node {
    element("p", None, vec![Node::Text(text.to_string())])
}

fn code_block_node(text: &str) -> Node {
    element(
        "pre",
        None,
        vec![Node::Text(truncate_chars(
            text,
            TELEGRAPH_CODE_BLOCK_MAX_CHARS,
        ))],
    )
}

fn line_break() -> Node {
    Node::NodeElement(NodeElement {
        tag: "br".to_string(),
        attrs: None,
        children: None,
    })
}

fn horizontal_rule() -> Node {
    Node::NodeElement(NodeElement {
        tag: "hr".to_string(),
        attrs: None,
        children: None,
    })
}

fn element(tag: &str, attrs: Option<HashMap<String, Option<String>>>, children: Vec<Node>) -> Node {
    Node::NodeElement(NodeElement {
        tag: tag.to_string(),
        attrs,
        children: (!children.is_empty()).then_some(children),
    })
}

fn href_attrs(key: &str, value: &str) -> Option<HashMap<String, Option<String>>> {
    let mut attrs = HashMap::new();
    attrs.insert(key.to_string(), Some(value.to_string()));
    Some(attrs)
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn format_exit_status(status: &executors::logs::CommandExitStatus) -> String {
    match status {
        executors::logs::CommandExitStatus::ExitCode { code } => format!("exit_code({code})"),
        executors::logs::CommandExitStatus::Success { success } => {
            format!("success({success})")
        }
    }
}

fn node_text_len(node: &Node) -> usize {
    match node {
        Node::Text(text) => text.chars().count(),
        Node::NodeElement(node) => node
            .children
            .as_ref()
            .map(|children| children.iter().map(node_text_len).sum())
            .unwrap_or(0),
    }
}

fn normalize_hidden_telegraph_fields(config: &mut TelegramConfig) -> bool {
    if !config.enabled || !config.telegraph_enabled {
        return false;
    }

    let mut changed = false;

    if config.telegraph_short_name.as_deref() != Some(TELEGRAPH_DEFAULT_SHORT_NAME) {
        config.telegraph_short_name = Some(TELEGRAPH_DEFAULT_SHORT_NAME.to_string());
        changed = true;
    }

    let normalized_token = normalized_optional_string(config.telegraph_access_token.as_deref());
    if config.telegraph_access_token != normalized_token {
        config.telegraph_access_token = normalized_token;
        changed = true;
    }

    changed
}

fn should_initialize_telegraph_account(config: &TelegramConfig) -> bool {
    config.enabled
        && config.telegraph_enabled
        && normalized_optional_string(config.bot_token.as_deref()).is_some()
        && config.chat_id.is_some()
        && config.telegraph_access_token.is_none()
}

fn normalized_optional_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn git_author_name() -> Option<String> {
    normalized_optional_string(
        GitConfig::open_default()
            .ok()
            .and_then(|config| config.get_string("user.name").ok())
            .as_deref(),
    )
}

async fn create_telegraph_account(author_name: Option<&str>) -> Result<Account> {
    let mut query = vec![(
        "short_name".to_string(),
        TELEGRAPH_DEFAULT_SHORT_NAME.to_string(),
    )];
    if let Some(author_name) = author_name {
        query.push(("author_name".to_string(), author_name.to_string()));
    }

    let response = Client::new()
        .get("https://api.telegra.ph/createAccount")
        .query(&query)
        .send()
        .await
        .context("failed to call telegra.ph createAccount")?
        .error_for_status()
        .context("telegra.ph createAccount returned an error status")?;

    let body: TelegraphApiResponse<Account> = response
        .json()
        .await
        .context("failed to decode telegra.ph createAccount response")?;

    if !body.ok {
        let error = body
            .error
            .unwrap_or_else(|| "unknown telegraph API error".to_string());
        anyhow::bail!("telegra.ph createAccount failed: {error}");
    }

    body.result
        .context("telegra.ph createAccount response missing result payload")
}

fn page_meta_from_page(page: telegraph_rs::Page) -> TelegraphPageMeta {
    TelegraphPageMeta {
        url: page.url,
        path: page.path,
        title: page.title,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use chrono::Utc;
    use executors::logs::{
        ActionType, NormalizedEntry, NormalizedEntryType, NormalizedLogEvent, ToolResult,
        ToolStatus,
    };
    use futures::StreamExt;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::UnboundedReceiverStream;

    use super::*;

    #[derive(Default)]
    struct MockTelegraphClient {
        calls: Mutex<Vec<String>>,
        pages: Mutex<HashMap<String, String>>,
    }

    impl MockTelegraphClient {
        fn create_count(&self) -> usize {
            self.calls
                .lock()
                .expect("lock calls")
                .iter()
                .filter(|c| c.starts_with("create"))
                .count()
        }

        fn edit_count(&self) -> usize {
            self.calls
                .lock()
                .expect("lock calls")
                .iter()
                .filter(|c| c.starts_with("edit"))
                .count()
        }

        fn all_calls(&self) -> Vec<String> {
            self.calls.lock().expect("lock calls").clone()
        }
    }

    #[async_trait]
    impl TelegraphClient for MockTelegraphClient {
        async fn create_page(&self, title: &str, content: &str) -> Result<TelegraphPageMeta> {
            let mut calls = self.calls.lock().expect("lock calls");
            calls.push(format!("create:{title}:{content}"));
            let create_index = calls
                .iter()
                .filter(|call| call.starts_with("create:"))
                .count();
            let path = format!("mock-{create_index}");
            self.pages
                .lock()
                .expect("lock pages")
                .insert(path.clone(), content.to_string());
            Ok(TelegraphPageMeta {
                url: format!("https://telegra.ph/mock-{create_index}"),
                path,
                title: title.to_string(),
            })
        }

        async fn edit_page(
            &self,
            path: &str,
            title: &str,
            content: &str,
        ) -> Result<TelegraphPageMeta> {
            self.calls
                .lock()
                .expect("lock calls")
                .push(format!("edit:{path}:{title}:{content}"));
            self.pages
                .lock()
                .expect("lock pages")
                .insert(path.to_string(), content.to_string());
            Ok(TelegraphPageMeta {
                url: format!("https://telegra.ph/{path}"),
                path: path.to_string(),
                title: title.to_string(),
            })
        }

        async fn get_page_content(&self, path: &str) -> Result<Vec<Node>> {
            let content = self
                .pages
                .lock()
                .expect("lock pages")
                .get(path)
                .cloned()
                .unwrap_or_default();
            Ok(serde_json::from_str(&content).unwrap_or_default())
        }
    }

    fn entry(entry_type: NormalizedEntryType, content: &str) -> NormalizedEntry {
        NormalizedEntry {
            timestamp: None,
            entry_type,
            content: content.to_string(),
            metadata: None,
        }
    }

    fn make_session_state() -> Arc<tokio::sync::Mutex<SessionTelegraphState>> {
        Arc::new(tokio::sync::Mutex::new(SessionTelegraphState::new()))
    }

    #[test]
    fn stage_summary_pages_prepend_user_input_when_missing_from_entries() {
        let rendered = render_entries_page_contents(
            vec![entry(
                NormalizedEntryType::AssistantMessage,
                "summary complete",
            )],
            Some("initial task"),
        )
        .expect("pages should render");

        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].contains("User"));
        assert!(rendered[0].contains("initial task"));
        assert!(rendered[0].contains("Assistant"));
        assert!(rendered[0].contains("summary complete"));
    }

    async fn run_consumer_with_client_for_test(
        execution_process_id: Uuid,
        page_title: &str,
        stream: NormalizedEventStream,
        client: Arc<dyn TelegraphClient>,
        db_pool: Option<SqlitePool>,
        session_id: Option<Uuid>,
        session_store: Option<Arc<TelegraphSessionStore>>,
    ) {
        let sid = session_id.unwrap_or_else(Uuid::new_v4);
        let store = session_store.unwrap_or_else(new_telegraph_session_store);
        run_telegraph_log_consumer_with_client(
            sid,
            execution_process_id,
            page_title.to_string(),
            stream,
            client,
            db_pool,
            store,
            None,
        )
        .await;
    }

    #[test]
    fn upsert_and_remove_apply_in_order() {
        let mock = Arc::new(MockTelegraphClient::default());
        let session_state = make_session_state();
        let mut consumer = TelegraphLogConsumer::new(
            Uuid::new_v4(),
            "Task Title".to_string(),
            mock,
            None,
            session_state,
            None,
        );

        consumer.apply_event(NormalizedLogEvent::UpsertEntry {
            index: 2,
            entry: entry(NormalizedEntryType::AssistantMessage, "assistant-2"),
        });
        consumer.apply_event(NormalizedLogEvent::UpsertEntry {
            index: 1,
            entry: entry(NormalizedEntryType::AssistantMessage, "assistant-1"),
        });
        consumer.apply_event(NormalizedLogEvent::RemoveEntry { index: 1 });

        // Render via the session state directly
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            consumer.flush_pending_updates().await;
            let session = consumer.session_state.lock().await;
            let rendered = session
                .render_all_page_contents()
                .expect("rendered text should exist");
            assert_eq!(rendered.len(), 1);
            assert!(!rendered[0].contains("assistant-1"));
            assert!(rendered[0].contains("assistant-2"));
            assert!(rendered[0].contains("Assistant"));
        });
    }

    #[test]
    fn tool_markdown_result_is_rendered() {
        let mock = Arc::new(MockTelegraphClient::default());
        let session_state = make_session_state();
        let mut consumer = TelegraphLogConsumer::new(
            Uuid::new_v4(),
            "Task Title".to_string(),
            mock,
            None,
            session_state,
            None,
        );

        consumer.apply_event(NormalizedLogEvent::UpsertEntry {
            index: 3,
            entry: entry(
                NormalizedEntryType::ToolUse {
                    tool_name: "web".to_string(),
                    action_type: ActionType::Tool {
                        tool_name: "search_docs".to_string(),
                        arguments: None,
                        result: Some(ToolResult::markdown("## Result\n\n- item")),
                    },
                    status: ToolStatus::Success,
                },
                "",
            ),
        });

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            consumer.flush_pending_updates().await;
            let session = consumer.session_state.lock().await;
            let rendered = session
                .render_all_page_contents()
                .expect("rendered text should exist");
            assert_eq!(rendered.len(), 1);
            assert!(rendered[0].contains("web [success]"));
            assert!(rendered[0].contains("Result"));
            assert!(rendered[0].contains("item"));
        });
    }

    #[test]
    fn bash_output_is_not_rendered_for_telegraph() {
        let nodes = render_tool_use_nodes(
            "bash",
            &ActionType::CommandRun {
                command: "echo hello".to_string(),
                result: Some(executors::logs::CommandRunResult {
                    exit_status: Some(executors::logs::CommandExitStatus::ExitCode { code: 0 }),
                    output: Some("stdout:\nhello".to_string()),
                }),
            },
            &ToolStatus::Success,
        );

        let rendered = serde_json::to_string(&nodes).expect("serialize nodes");
        assert!(rendered.contains("Exit status: exit_code(0)"));
        assert!(!rendered.contains("Output"));
        assert!(!rendered.contains("stdout:\\nhello"));
    }

    #[tokio::test]
    async fn throttled_updates_and_finished_flush_work() {
        let mock = Arc::new(MockTelegraphClient::default());
        let (tx, rx) = mpsc::unbounded_channel::<NormalizedLogEvent>();

        let stream: NormalizedEventStream =
            Box::pin(UnboundedReceiverStream::new(rx).map(Ok::<_, std::io::Error>));
        let execution_process_id = Uuid::new_v4();
        let handle = tokio::spawn(run_consumer_with_client_for_test(
            execution_process_id,
            "Task Title",
            stream,
            mock.clone(),
            None,
            None,
            None,
        ));

        tx.send(NormalizedLogEvent::UpsertEntry {
            index: 0,
            entry: entry(NormalizedEntryType::AssistantMessage, "first"),
        })
        .expect("send first event");
        tokio::task::yield_now().await;
        assert_eq!(mock.create_count(), 1);
        assert_eq!(mock.edit_count(), 0);

        tx.send(NormalizedLogEvent::UpsertEntry {
            index: 1,
            entry: entry(NormalizedEntryType::AssistantMessage, "second"),
        })
        .expect("send second event");
        tokio::task::yield_now().await;
        assert_eq!(mock.edit_count(), 0);

        tokio::time::sleep(Duration::from_millis(2200)).await;
        tokio::task::yield_now().await;
        assert_eq!(mock.edit_count(), 1);

        tx.send(NormalizedLogEvent::UpsertEntry {
            index: 2,
            entry: entry(NormalizedEntryType::AssistantMessage, "third"),
        })
        .expect("send third event");
        tx.send(NormalizedLogEvent::Finished)
            .expect("send finished event");
        drop(tx);

        handle.await.expect("consumer task should finish");
        assert_eq!(mock.edit_count(), 2);
    }

    #[test]
    fn render_splits_into_multiple_pages_when_too_long() {
        let session_state = Arc::new(tokio::sync::Mutex::new(SessionTelegraphState::new()));
        let mut state = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build runtime")
            .block_on(async { session_state.lock().await });
        let exec_id = Uuid::new_v4();

        let mut local_entries = BTreeMap::new();
        for i in 0..(TELEGRAPH_MAX_PAGE_ENTRIES + 30) {
            local_entries.insert(
                i,
                entry(
                    NormalizedEntryType::AssistantMessage,
                    &format!("assistant-line-{i}"),
                ),
            );
        }
        state.merge_execution_entries(exec_id, &local_entries, None);

        let rendered_pages = state
            .render_all_page_contents()
            .expect("rendered text should exist");
        assert_eq!(rendered_pages.len(), 2);
        assert!(rendered_pages[0].contains("assistant-line-0"));
        assert!(rendered_pages[1].contains(&format!(
            "assistant-line-{}",
            TELEGRAPH_MAX_PAGE_ENTRIES + 29
        )));
    }

    #[test]
    fn page_title_for_page_appends_page_counter() {
        assert_eq!(page_title_for_page("Task Title", 1), "Task Title (2)");
    }

    #[test]
    fn page_title_prefers_task_title() {
        let execution_process_id = Uuid::new_v4();
        assert_eq!(
            page_title_for_execution("Ship Telegraph polish", execution_process_id),
            "Ship Telegraph polish"
        );
    }

    #[test]
    fn normalized_optional_string_trims_and_filters_empty_values() {
        assert_eq!(
            normalized_optional_string(Some("  token  ")).as_deref(),
            Some("token")
        );
        assert_eq!(normalized_optional_string(Some("   ")), None);
    }

    #[test]
    fn config_gate_requires_telegram_and_token() {
        let mut config = TelegramConfig::default();
        config.enabled = true;
        config.telegraph_enabled = true;
        config.telegraph_access_token = Some("token".to_string());

        let enabled = TelegraphMirrorConfig::from_telegram_config(&config);
        assert!(enabled.is_some());

        config.telegraph_access_token = Some(" ".to_string());
        let disabled = TelegraphMirrorConfig::from_telegram_config(&config);
        assert!(disabled.is_none());
    }

    #[test]
    fn hidden_fields_are_normalized_when_telegraph_is_enabled() {
        let mut config = TelegramConfig {
            enabled: true,
            telegraph_enabled: true,
            telegraph_access_token: Some(" token ".to_string()),
            telegraph_short_name: Some("custom_name".to_string()),
            ..TelegramConfig::default()
        };

        assert!(normalize_hidden_telegraph_fields(&mut config));
        assert_eq!(
            config.telegraph_short_name.as_deref(),
            Some(TELEGRAPH_DEFAULT_SHORT_NAME)
        );
        assert_eq!(config.telegraph_access_token.as_deref(), Some("token"));
    }

    #[test]
    fn hidden_fields_remain_unchanged_when_telegraph_is_disabled() {
        let mut config = TelegramConfig {
            enabled: true,
            telegraph_enabled: false,
            telegraph_access_token: Some(" token ".to_string()),
            telegraph_short_name: Some("custom_name".to_string()),
            ..TelegramConfig::default()
        };

        assert!(!normalize_hidden_telegraph_fields(&mut config));
        assert_eq!(config.telegraph_access_token.as_deref(), Some(" token "));
        assert_eq!(config.telegraph_short_name.as_deref(), Some("custom_name"));
    }

    #[test]
    fn account_initialization_requires_full_telegram_setup() {
        let mut config = TelegramConfig {
            enabled: true,
            telegraph_enabled: true,
            bot_token: Some("bot".to_string()),
            chat_id: Some(42),
            ..TelegramConfig::default()
        };

        assert!(should_initialize_telegraph_account(&config));

        config.telegraph_access_token = Some("existing".to_string());
        assert!(!should_initialize_telegraph_account(&config));

        config.telegraph_access_token = None;
        config.chat_id = None;
        assert!(!should_initialize_telegraph_account(&config));

        config.chat_id = Some(42);
        config.telegraph_enabled = false;
        assert!(!should_initialize_telegraph_account(&config));
    }

    async fn make_test_pool() -> SqlitePool {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("create sqlite memory pool");
        sqlx::query(
            r#"CREATE TABLE execution_process_telegraph_pages (
                execution_process_id TEXT NOT NULL,
                page_index INTEGER NOT NULL,
                url TEXT NOT NULL,
                path TEXT NOT NULL,
                title TEXT NOT NULL,
                created_at DATETIME NOT NULL,
                updated_at DATETIME NOT NULL,
                PRIMARY KEY (execution_process_id, page_index)
            )"#,
        )
        .execute(&pool)
        .await
        .expect("create test table");
        pool
    }

    async fn insert_test_execution_processes(
        pool: &SqlitePool,
        session_id: Uuid,
        execution_ids: &[Uuid],
    ) {
        sqlx::query(
            r#"CREATE TABLE execution_processes (
                id BLOB PRIMARY KEY,
                session_id BLOB NOT NULL,
                run_reason TEXT NOT NULL,
                executor_action TEXT NOT NULL,
                status TEXT NOT NULL,
                exit_code INTEGER,
                dropped INTEGER NOT NULL DEFAULT 0,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )"#,
        )
        .execute(pool)
        .await
        .expect("create execution_processes");

        let now = Utc::now();
        for (idx, execution_id) in execution_ids.iter().enumerate() {
            let ts = now + chrono::Duration::seconds(idx as i64);
            sqlx::query(
                r#"INSERT INTO execution_processes (
                    id, session_id, run_reason, executor_action, status, dropped,
                    started_at, created_at, updated_at
                ) VALUES (?, ?, 'codingagent', '{}', 'completed', 0, ?, ?, ?)"#,
            )
            .bind(*execution_id)
            .bind(session_id)
            .bind(ts)
            .bind(ts)
            .bind(ts)
            .execute(pool)
            .await
            .expect("insert execution process");
        }
    }

    #[tokio::test]
    async fn final_summary_append_edits_existing_session_page() {
        let pool = make_test_pool().await;
        let mock = MockTelegraphClient::default();
        let session_id = Uuid::new_v4();
        let exec1 = Uuid::new_v4();
        let exec2 = Uuid::new_v4();
        insert_test_execution_processes(&pool, session_id, &[exec1, exec2]).await;

        let first_urls = append_stage_summary_pages_with_client(
            &pool,
            exec1,
            session_id,
            "Task Title",
            render_entries_page_contents(
                vec![entry(
                    NormalizedEntryType::AssistantMessage,
                    "first summary",
                )],
                Some("initial prompt"),
            )
            .unwrap(),
            &mock,
        )
        .await;
        let second_urls = append_stage_summary_pages_with_client(
            &pool,
            exec2,
            session_id,
            "Task Title",
            render_entries_page_contents(
                vec![entry(
                    NormalizedEntryType::AssistantMessage,
                    "second summary",
                )],
                Some("follow-up prompt"),
            )
            .unwrap(),
            &mock,
        )
        .await;

        assert_eq!(first_urls, vec!["https://telegra.ph/mock-1".to_string()]);
        assert_eq!(second_urls, vec!["https://telegra.ph/mock-1".to_string()]);
        assert_eq!(mock.create_count(), 1);
        assert_eq!(mock.edit_count(), 1);

        let calls = mock.all_calls();
        let edit_call = calls
            .iter()
            .find(|call| call.starts_with("edit:"))
            .expect("existing page should be edited");
        assert!(edit_call.contains("first summary"));
        assert!(edit_call.contains("second summary"));
        assert_eq!(
            get_execution_process_telegraph_urls(&pool, &exec2).await,
            vec!["https://telegra.ph/mock-1".to_string()]
        );
    }

    #[tokio::test]
    async fn page_metadata_is_persisted_to_db() {
        let mock = Arc::new(MockTelegraphClient::default());
        let (tx, rx) = mpsc::unbounded_channel::<NormalizedLogEvent>();
        let stream: NormalizedEventStream =
            Box::pin(UnboundedReceiverStream::new(rx).map(Ok::<_, std::io::Error>));

        let execution_process_id = Uuid::new_v4();
        let pool = make_test_pool().await;
        let handle = tokio::spawn(run_consumer_with_client_for_test(
            execution_process_id,
            "Task Title",
            stream,
            mock.clone(),
            Some(pool.clone()),
            None,
            None,
        ));

        tx.send(NormalizedLogEvent::UpsertEntry {
            index: 0,
            entry: entry(NormalizedEntryType::AssistantMessage, "create page"),
        })
        .expect("send event");
        tx.send(NormalizedLogEvent::Finished)
            .expect("send finished event");
        drop(tx);

        handle.await.expect("consumer task should finish");
        assert_eq!(
            get_execution_process_telegraph_urls(&pool, &execution_process_id).await,
            vec!["https://telegra.ph/mock-1".to_string()]
        );

        let calls = mock.all_calls();
        assert!(calls.iter().any(|call| call.starts_with("create:")));
    }

    #[tokio::test]
    async fn page_metadata_persists_all_pages_for_multi_page_render() {
        let mock = Arc::new(MockTelegraphClient::default());
        let (tx, rx) = mpsc::unbounded_channel::<NormalizedLogEvent>();
        let stream: NormalizedEventStream =
            Box::pin(UnboundedReceiverStream::new(rx).map(Ok::<_, std::io::Error>));

        let execution_process_id = Uuid::new_v4();
        let pool = make_test_pool().await;

        let handle = tokio::spawn(run_consumer_with_client_for_test(
            execution_process_id,
            "Task Title",
            stream,
            mock.clone(),
            Some(pool.clone()),
            None,
            None,
        ));

        for i in 0..(TELEGRAPH_MAX_PAGE_ENTRIES + 5) {
            tx.send(NormalizedLogEvent::UpsertEntry {
                index: i,
                entry: entry(
                    NormalizedEntryType::AssistantMessage,
                    &format!("assistant-line-{i}"),
                ),
            })
            .expect("send event");
        }
        tx.send(NormalizedLogEvent::Finished)
            .expect("send finished event");
        drop(tx);

        handle.await.expect("consumer task should finish");
        assert_eq!(
            get_execution_process_telegraph_urls(&pool, &execution_process_id).await,
            vec![
                "https://telegra.ph/mock-1".to_string(),
                "https://telegra.ph/mock-2".to_string(),
            ]
        );
    }

    /// Second execution in same session appends to last existing page when there is room.
    #[tokio::test]
    async fn second_execution_appends_to_last_page_when_room() {
        let mock = Arc::new(MockTelegraphClient::default());
        let session_id = Uuid::new_v4();
        let store = new_telegraph_session_store();

        // First execution: a few entries (fits on one page).
        let exec1 = Uuid::new_v4();
        {
            let (tx, rx) = mpsc::unbounded_channel::<NormalizedLogEvent>();
            let stream: NormalizedEventStream =
                Box::pin(UnboundedReceiverStream::new(rx).map(Ok::<_, std::io::Error>));
            let handle = tokio::spawn(run_consumer_with_client_for_test(
                exec1,
                "Task Title",
                stream,
                mock.clone(),
                None,
                Some(session_id),
                Some(store.clone()),
            ));
            tx.send(NormalizedLogEvent::UpsertEntry {
                index: 0,
                entry: entry(NormalizedEntryType::AssistantMessage, "first-exec-entry"),
            })
            .expect("send");
            tx.send(NormalizedLogEvent::Finished).expect("send");
            drop(tx);
            handle.await.expect("consumer task should finish");
        }
        // After first execution: 1 page created.
        assert_eq!(mock.create_count(), 1);
        assert_eq!(mock.edit_count(), 0);

        // Second execution in same session: adds a few more entries (still fits on one page).
        let exec2 = Uuid::new_v4();
        {
            let (tx, rx) = mpsc::unbounded_channel::<NormalizedLogEvent>();
            let stream: NormalizedEventStream =
                Box::pin(UnboundedReceiverStream::new(rx).map(Ok::<_, std::io::Error>));
            let handle = tokio::spawn(run_consumer_with_client_for_test(
                exec2,
                "Task Title",
                stream,
                mock.clone(),
                None,
                Some(session_id),
                Some(store.clone()),
            ));
            tx.send(NormalizedLogEvent::UpsertEntry {
                index: 0,
                entry: entry(NormalizedEntryType::AssistantMessage, "second-exec-entry"),
            })
            .expect("send");
            tx.send(NormalizedLogEvent::Finished).expect("send");
            drop(tx);
            handle.await.expect("consumer task should finish");
        }

        // Still 1 page total (edits the existing page, no new page created).
        assert_eq!(mock.create_count(), 1, "no new page should be created");
        assert!(mock.edit_count() >= 1, "existing page should be edited");

        // Content of page should contain both executions' entries.
        let session_lock = store.lock().await;
        let session = session_lock.get(&session_id).expect("session exists");
        let session = session.lock().await;
        let contents = session
            .render_all_page_contents()
            .expect("should have content");
        assert_eq!(contents.len(), 1, "should still be one page");
        assert!(contents[0].contains("first-exec-entry"));
        assert!(contents[0].contains("second-exec-entry"));
    }

    /// When second execution exceeds page capacity, new pages are created.
    #[tokio::test]
    async fn second_execution_creates_new_pages_on_overflow() {
        let mock = Arc::new(MockTelegraphClient::default());
        let session_id = Uuid::new_v4();
        let store = new_telegraph_session_store();

        // First execution: fill multiple pages.
        let exec1 = Uuid::new_v4();
        {
            let (tx, rx) = mpsc::unbounded_channel::<NormalizedLogEvent>();
            let stream: NormalizedEventStream =
                Box::pin(UnboundedReceiverStream::new(rx).map(Ok::<_, std::io::Error>));
            let handle = tokio::spawn(run_consumer_with_client_for_test(
                exec1,
                "Task Title",
                stream,
                mock.clone(),
                None,
                Some(session_id),
                Some(store.clone()),
            ));
            for i in 0..TELEGRAPH_MAX_PAGE_ENTRIES {
                tx.send(NormalizedLogEvent::UpsertEntry {
                    index: i,
                    entry: entry(
                        NormalizedEntryType::AssistantMessage,
                        &format!("exec1-entry-{i}"),
                    ),
                })
                .expect("send");
            }
            tx.send(NormalizedLogEvent::Finished).expect("send");
            drop(tx);
            handle.await.expect("consumer task should finish");
        }
        let creates_after_exec1 = mock.create_count();
        assert!(
            creates_after_exec1 >= 1,
            "exec1 should create at least 1 page"
        );

        // Second execution: adds enough entries to overflow the last page.
        let exec2 = Uuid::new_v4();
        {
            let (tx, rx) = mpsc::unbounded_channel::<NormalizedLogEvent>();
            let stream: NormalizedEventStream =
                Box::pin(UnboundedReceiverStream::new(rx).map(Ok::<_, std::io::Error>));
            let handle = tokio::spawn(run_consumer_with_client_for_test(
                exec2,
                "Task Title",
                stream,
                mock.clone(),
                None,
                Some(session_id),
                Some(store.clone()),
            ));
            // Add enough entries to push beyond the page limit.
            for i in 0..30 {
                tx.send(NormalizedLogEvent::UpsertEntry {
                    index: i,
                    entry: entry(
                        NormalizedEntryType::AssistantMessage,
                        &format!("exec2-entry-{i}"),
                    ),
                })
                .expect("send");
            }
            tx.send(NormalizedLogEvent::Finished).expect("send");
            drop(tx);
            handle.await.expect("consumer task should finish");
        }

        // After exec2, total pages > creates_after_exec1 (new pages were created).
        let total_creates = mock.create_count();
        assert!(
            total_creates > creates_after_exec1,
            "exec2 should have created additional pages"
        );

        // Both executions' content should appear in the session render.
        let session_lock = store.lock().await;
        let session = session_lock.get(&session_id).expect("session exists");
        let session = session.lock().await;
        let contents = session
            .render_all_page_contents()
            .expect("should have content");
        assert!(contents.len() >= 2, "should span at least 2 pages");
        let all_content = contents.join(" ");
        assert!(all_content.contains("exec1-entry-0"));
        assert!(all_content.contains("exec2-entry-0"));
    }

    /// DB URL cache: first execution persists its URLs; second execution under same session
    /// persists the full session page URL list under the second execution_process_id.
    #[tokio::test]
    async fn db_url_cache_persists_per_execution_process() {
        let mock = Arc::new(MockTelegraphClient::default());
        let pool = make_test_pool().await;
        let session_id = Uuid::new_v4();
        let store = new_telegraph_session_store();

        let exec1 = Uuid::new_v4();
        {
            let (tx, rx) = mpsc::unbounded_channel::<NormalizedLogEvent>();
            let stream: NormalizedEventStream =
                Box::pin(UnboundedReceiverStream::new(rx).map(Ok::<_, std::io::Error>));
            let handle = tokio::spawn(run_consumer_with_client_for_test(
                exec1,
                "Task Title",
                stream,
                mock.clone(),
                Some(pool.clone()),
                Some(session_id),
                Some(store.clone()),
            ));
            tx.send(NormalizedLogEvent::UpsertEntry {
                index: 0,
                entry: entry(NormalizedEntryType::AssistantMessage, "exec1 content"),
            })
            .expect("send");
            tx.send(NormalizedLogEvent::Finished).expect("send");
            drop(tx);
            handle.await.expect("consumer task should finish");
        }

        let urls_exec1 = get_execution_process_telegraph_urls(&pool, &exec1).await;
        assert!(!urls_exec1.is_empty(), "exec1 should have persisted URLs");

        let exec2 = Uuid::new_v4();
        {
            let (tx, rx) = mpsc::unbounded_channel::<NormalizedLogEvent>();
            let stream: NormalizedEventStream =
                Box::pin(UnboundedReceiverStream::new(rx).map(Ok::<_, std::io::Error>));
            let handle = tokio::spawn(run_consumer_with_client_for_test(
                exec2,
                "Task Title",
                stream,
                mock.clone(),
                Some(pool.clone()),
                Some(session_id),
                Some(store.clone()),
            ));
            tx.send(NormalizedLogEvent::UpsertEntry {
                index: 0,
                entry: entry(NormalizedEntryType::AssistantMessage, "exec2 content"),
            })
            .expect("send");
            tx.send(NormalizedLogEvent::Finished).expect("send");
            drop(tx);
            handle.await.expect("consumer task should finish");
        }

        let urls_exec2 = get_execution_process_telegraph_urls(&pool, &exec2).await;
        assert!(
            !urls_exec2.is_empty(),
            "exec2 should have persisted session URLs under its own execution_process_id"
        );
        // Both should reference the same pages (session-level state).
        assert_eq!(
            urls_exec1.len(),
            urls_exec2.len(),
            "exec1 and exec2 should reference the same number of pages (session-level)"
        );
    }
}
