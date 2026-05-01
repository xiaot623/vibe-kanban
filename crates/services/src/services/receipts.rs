use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, Local, Offset, Utc};
use chrono_tz::Tz;
use db::{
    models::{execution_process::ExecutionProcess, task::TaskStatus},
    task_state::{
        TaskStateTransition,
        dispatcher::TaskStateDispatcher,
        handler::{TransitionFilter, fn_handler},
    },
};
use executors::{
    executors::BaseCodingAgent,
    receipts::{ExecutorReceiptSupport, ReceiptError, ReceiptRequest},
};
use resvg::tiny_skia;
use sqlx::{Row, SqlitePool};
use teloxide::{payloads::SendPhotoSetters, prelude::Requester, types::InputFile};
use utils::assets::asset_dir;
use uuid::Uuid;

static RECEIPTS_HANDLER_REGISTERED: OnceLock<()> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiptExecution {
    execution_process_id: Uuid,
    agent_session_id: String,
    executor: BaseCodingAgent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiptPaths {
    directory: PathBuf,
    file_path: PathBuf,
}

#[derive(Clone)]
pub struct ReceiptsService {
    output_root: PathBuf,
    svg_generator: Arc<dyn ReceiptSvgGenerator>,
}

impl ReceiptsService {
    pub fn new() -> Self {
        Self::with_svg_generator_and_root(
            Arc::new(ExecutorReceiptSvgGenerator),
            asset_dir().join("receipts"),
        )
    }

    fn with_svg_generator_and_root(
        svg_generator: Arc<dyn ReceiptSvgGenerator>,
        output_root: PathBuf,
    ) -> Self {
        Self {
            output_root,
            svg_generator,
        }
    }

    pub async fn register_once(dispatcher: &Arc<TaskStateDispatcher>) {
        if RECEIPTS_HANDLER_REGISTERED.get().is_some() {
            return;
        }

        let service = Arc::new(Self::new());
        let handler_service = Arc::clone(&service);
        let handler = fn_handler(
            "receipts_generate_on_done",
            TransitionFilter::new().to(vec![TaskStatus::Done]),
            move |ctx, transition| {
                let service = Arc::clone(&handler_service);
                Box::pin(async move {
                    if !should_generate_receipt(transition) {
                        return;
                    }

                    if let Err(error) = service.handle_done_transition(&ctx.pool, transition).await
                    {
                        tracing::warn!(
                            task_id = %transition.task_id(),
                            error = %error,
                            "Skipping executor session receipt generation"
                        );
                    }
                })
            },
        );

        dispatcher.register_handler(handler).await;
        let _ = RECEIPTS_HANDLER_REGISTERED.set(());
    }

    async fn handle_done_transition(
        &self,
        pool: &SqlitePool,
        transition: &TaskStateTransition,
    ) -> anyhow::Result<()> {
        let Some(execution) = find_latest_receipt_execution(pool, transition.task.id).await? else {
            tracing::info!(
                task_id = %transition.task_id(),
                "No coding-agent execution with agent session id found for completed task"
            );
            return Ok(());
        };
        let done_at_display = format_receipt_done_at(transition.task.updated_at);

        let svg = match self
            .svg_generator
            .generate_receipt_svg(
                execution.executor,
                ReceiptRequest {
                    agent_session_id: &execution.agent_session_id,
                    task_title: &transition.task.title,
                    done_at: transition.task.updated_at,
                    done_at_display: &done_at_display,
                },
            )
            .await
        {
            Ok(svg) => svg,
            Err(error) => {
                tracing::warn!(
                    task_id = %transition.task_id(),
                    executor = %execution.executor,
                    session_id = execution.agent_session_id,
                    error = %error,
                    "Failed to generate executor session receipt SVG"
                );
                return Ok(());
            }
        };

        let paths = reserve_receipt_path(
            &self.output_root,
            transition.task.updated_at,
            execution.executor,
            &transition.task.title,
        )?;
        fs::create_dir_all(&paths.directory).with_context(|| {
            format!(
                "failed to create receipt directory {}",
                paths.directory.display()
            )
        })?;

        let png_bytes =
            render_receipt_png(&svg).context("failed to render SVG to PNG for storage")?;
        fs::write(&paths.file_path, png_bytes)
            .with_context(|| format!("failed to write receipt {}", paths.file_path.display()))?;
        tracing::info!(
            task_id = %transition.task_id(),
            receipt_path = %paths.file_path.display(),
            executor = %execution.executor,
            session_id = execution.agent_session_id,
            "Generated executor session receipt"
        );

        if let Err(error) = self
            .send_receipt_png_if_enabled(&transition.task.title, &svg)
            .await
        {
            tracing::warn!(
                task_id = %transition.task_id(),
                executor = %execution.executor,
                session_id = execution.agent_session_id,
                error = %error,
                "Failed to send executor session receipt to Telegram"
            );
        }

        Ok(())
    }

    async fn send_receipt_png_if_enabled(&self, task_title: &str, svg: &str) -> anyhow::Result<()> {
        let Some(tg) = crate::services::telegram::notifier::get_context().await else {
            return Ok(());
        };

        let config = tg.config.read().await;
        if !config.telegram.enabled || !config.telegram.send_session_receipt {
            return Ok(());
        }

        let png_bytes = render_receipt_png(svg)?;
        let file_name = format!(
            "session-receipt-{}.png",
            sanitize_filename_segment(task_title).trim_matches('_')
        );

        tg.bot
            .send_photo(
                tg.chat_id,
                InputFile::memory(png_bytes).file_name(if file_name == "session-receipt-.png" {
                    "session-receipt.png".to_string()
                } else {
                    file_name
                }),
            )
            .caption(format!("Session receipt: {}", task_title))
            .await
            .context("telegram send_photo failed")?;

        Ok(())
    }
}

#[async_trait]
trait ReceiptSvgGenerator: Send + Sync {
    async fn generate_receipt_svg(
        &self,
        executor: BaseCodingAgent,
        request: ReceiptRequest<'_>,
    ) -> Result<String, ReceiptError>;
}

struct ExecutorReceiptSvgGenerator;

#[async_trait]
impl ReceiptSvgGenerator for ExecutorReceiptSvgGenerator {
    async fn generate_receipt_svg(
        &self,
        executor: BaseCodingAgent,
        request: ReceiptRequest<'_>,
    ) -> Result<String, ReceiptError> {
        executor.generate_receipt_svg(request).await
    }
}

fn should_generate_receipt(transition: &TaskStateTransition) -> bool {
    transition.is_change()
        && !transition.is_creation()
        && transition.to_status() == &TaskStatus::Done
}

async fn find_latest_receipt_execution(
    pool: &SqlitePool,
    task_id: Uuid,
) -> anyhow::Result<Option<ReceiptExecution>> {
    let row = sqlx::query(
        r#"SELECT
                ep.id as execution_process_id,
                cat.agent_session_id as agent_session_id,
                ep.executor_action as executor_action
           FROM execution_processes ep
           JOIN sessions s ON s.id = ep.session_id
           JOIN workspaces w ON w.id = s.workspace_id
           LEFT JOIN coding_agent_turns cat ON cat.execution_process_id = ep.id
           WHERE w.task_id = $1
             AND ep.run_reason = 'codingagent'
             AND ep.dropped = FALSE
           ORDER BY ep.created_at DESC
           LIMIT 1"#,
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let execution_process_id = row.try_get::<Uuid, _>("execution_process_id")?;
    let Some(agent_session_id) = row
        .try_get::<Option<String>, _>("agent_session_id")?
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };

    let execution = ExecutionProcess::find_by_id(pool, execution_process_id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    let executor = execution
        .executor_action()?
        .base_executor()
        .ok_or_else(|| anyhow!("execution action had no coding executor"))?;

    Ok(Some(ReceiptExecution {
        execution_process_id,
        agent_session_id,
        executor,
    }))
}

fn reserve_receipt_path(
    output_root: &Path,
    done_at: DateTime<Utc>,
    executor: BaseCodingAgent,
    task_title: &str,
) -> anyhow::Result<ReceiptPaths> {
    let local_time = receipt_local_time(done_at);
    let month = local_time.format("%Y%m").to_string();
    let timestamp = local_time.format("%Y%m%d%H%M%S").to_string();
    let directory = output_root.join(&month);
    let executor_segment = sanitize_filename_segment(executor.receipt_slug());
    let title_segment = sanitize_filename_segment(task_title);
    let stem = format!("{timestamp}{executor_segment}-{title_segment}");

    let mut counter = 1usize;
    loop {
        let suffix = if counter == 1 {
            String::new()
        } else {
            format!("-{counter}")
        };
        let file_path = directory.join(format!("{stem}{suffix}.png"));
        if !file_path.exists() {
            return Ok(ReceiptPaths {
                directory,
                file_path,
            });
        }
        counter += 1;
    }
}

fn sanitize_filename_segment(value: &str) -> String {
    let mut sanitized = String::new();
    let mut previous_dash = false;

    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            previous_dash = false;
            Some(ch.to_ascii_lowercase())
        } else if !previous_dash {
            previous_dash = true;
            Some('-')
        } else {
            None
        };

        if let Some(ch) = mapped {
            sanitized.push(ch);
        }
    }

    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "task".to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptTimeZone {
    Iana(Tz),
    Fixed(FixedOffset),
}

fn configured_receipt_time_zone() -> Option<ReceiptTimeZone> {
    configured_receipt_time_zone_from_str(env::var("TZ").ok().as_deref())
}

fn configured_receipt_time_zone_from_str(raw: Option<&str>) -> Option<ReceiptTimeZone> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }

    if let Ok(tz) = raw.parse::<Tz>() {
        return Some(ReceiptTimeZone::Iana(tz));
    }

    parse_fixed_offset(raw).map(ReceiptTimeZone::Fixed)
}

fn parse_fixed_offset(raw: &str) -> Option<FixedOffset> {
    let normalized = raw
        .strip_prefix("UTC")
        .or_else(|| raw.strip_prefix("GMT"))
        .unwrap_or(raw);
    let normalized = normalized.trim();
    if normalized.is_empty() {
        return Some(Utc.fix());
    }

    let sign = match normalized.as_bytes().first().copied()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &normalized[1..];
    let (hours, minutes) = if let Some((hours, minutes)) = rest.split_once(':') {
        (hours, minutes)
    } else if rest.len() == 4 {
        (&rest[..2], &rest[2..])
    } else if rest.len() <= 2 {
        (rest, "0")
    } else {
        return None;
    };

    let hours = hours.parse::<i32>().ok()?;
    let minutes = minutes.parse::<i32>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }

    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

fn receipt_local_time(done_at: DateTime<Utc>) -> DateTime<FixedOffset> {
    match configured_receipt_time_zone() {
        Some(ReceiptTimeZone::Iana(tz)) => done_at.with_timezone(&tz).fixed_offset(),
        Some(ReceiptTimeZone::Fixed(offset)) => done_at.with_timezone(&offset),
        None => done_at.with_timezone(&Local).fixed_offset(),
    }
}

fn format_receipt_done_at(done_at: DateTime<Utc>) -> String {
    receipt_local_time(done_at)
        .format("%b %d, %Y, %I:%M %p %:z")
        .to_string()
}

fn render_receipt_png(svg_content: &str) -> anyhow::Result<Vec<u8>> {
    let mut fontdb = usvg::fontdb::Database::new();
    fontdb.load_system_fonts();

    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_str(svg_content, &opt, &fontdb).context("failed to parse SVG")?;

    let pixmap_size = tree.size().to_int_size();
    let scale = 3.0;
    let mut pixmap = tiny_skia::Pixmap::new(
        (pixmap_size.width() as f32 * scale) as u32,
        (pixmap_size.height() as f32 * scale) as u32,
    )
    .context("failed to create pixmap")?;

    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    let png_bytes = pixmap.encode_png().context("failed to encode PNG")?;
    Ok(png_bytes)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use db::models::{
        coding_agent_turn::{CodingAgentTurn, CreateCodingAgentTurn},
        execution_process::{
            CreateExecutionProcess, ExecutionProcessRunReason, ExecutionProcessStatus,
        },
        project::{CreateProject, Project},
        session::{CreateSession, Session},
        task::{CreateTask, Task},
        workspace::{CreateWorkspace, Workspace},
    };
    use executors::{
        actions::{
            ExecutorAction, ExecutorActionType, coding_agent_initial::CodingAgentInitialRequest,
        },
        profile::ExecutorProfileId,
    };
    use tempfile::TempDir;
    use tokio::time::{Duration, sleep, timeout};

    use super::*;

    const TEST_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="400" viewBox="0 0 400 400">
  <rect width="400" height="400" fill="#fdfdfd"/>
  <text x="20" y="40" font-family="monospace" font-size="16">Receipt</text>
  <text x="20" y="80" font-family="monospace" font-size="14">Render receipt</text>
</svg>"##;

    #[derive(Clone)]
    struct MockReceiptSvgGenerator {
        svg: Arc<String>,
    }

    #[async_trait]
    impl ReceiptSvgGenerator for MockReceiptSvgGenerator {
        async fn generate_receipt_svg(
            &self,
            _executor: BaseCodingAgent,
            _request: ReceiptRequest<'_>,
        ) -> Result<String, ReceiptError> {
            Ok(self.svg.as_ref().clone())
        }
    }

    fn mock_service(output_root: PathBuf) -> Arc<ReceiptsService> {
        Arc::new(ReceiptsService::with_svg_generator_and_root(
            Arc::new(MockReceiptSvgGenerator {
                svg: Arc::new(TEST_SVG.to_string()),
            }),
            output_root,
        ))
    }

    fn sample_transition(
        from_status: Option<TaskStatus>,
        to_status: TaskStatus,
    ) -> TaskStateTransition {
        let now = Utc::now();
        let task = Task {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            title: "Ship receipt".to_string(),
            description: None,
            status: to_status,
            parent_workspace_id: None,
            source_cron_task_id: None,
            diff_additions: None,
            diff_deletions: None,
            created_at: now,
            updated_at: now,
        };
        TaskStateTransition::new(task, from_status)
    }

    #[test]
    fn filters_real_done_transitions() {
        assert!(should_generate_receipt(&sample_transition(
            Some(TaskStatus::InProgress),
            TaskStatus::Done,
        )));
        assert!(!should_generate_receipt(&sample_transition(
            None,
            TaskStatus::Done,
        )));
        assert!(!should_generate_receipt(&sample_transition(
            Some(TaskStatus::Done),
            TaskStatus::Done,
        )));
    }

    #[test]
    fn generates_unique_filenames() {
        let temp_dir = TempDir::new().unwrap();
        let output_root = temp_dir.path().join("receipts");
        let done_at = Utc.with_ymd_and_hms(2026, 5, 1, 9, 8, 7).unwrap();

        let first =
            reserve_receipt_path(&output_root, done_at, BaseCodingAgent::Codex, "Fix login!")
                .unwrap();
        fs::create_dir_all(&first.directory).unwrap();
        fs::write(&first.file_path, "one").unwrap();

        let second =
            reserve_receipt_path(&output_root, done_at, BaseCodingAgent::Codex, "Fix login!")
                .unwrap();

        let local_time = done_at.with_timezone(&Local);
        let expected_timestamp = local_time.format("%Y%m%d%H%M%S").to_string();

        assert_eq!(
            first.file_path.file_name().unwrap().to_string_lossy(),
            format!("{}codex-fix-login.png", expected_timestamp)
        );
        assert_eq!(
            second.file_path.file_name().unwrap().to_string_lossy(),
            format!("{}codex-fix-login-2.png", expected_timestamp)
        );
    }

    #[test]
    fn uses_configured_receipt_timezone_for_display_and_paths() {
        let done_at = Utc.with_ymd_and_hms(2026, 5, 1, 9, 8, 7).unwrap();
        let tz = configured_receipt_time_zone_from_str(Some("Asia/Shanghai")).unwrap();
        let local_time = match tz {
            ReceiptTimeZone::Iana(tz) => done_at.with_timezone(&tz).fixed_offset(),
            ReceiptTimeZone::Fixed(offset) => done_at.with_timezone(&offset),
        };

        assert_eq!(
            local_time.format("%Y%m%d%H%M%S").to_string(),
            "20260501170807"
        );
        assert_eq!(
            local_time.format("%b %d, %Y, %I:%M %p %:z").to_string(),
            "May 01, 2026, 05:08 PM +08:00"
        );
    }

    #[test]
    fn renders_receipt_png() {
        let png = render_receipt_png(TEST_SVG).expect("render PNG");

        assert!(png.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]));
        assert!(png.len() > 1_000);
    }

    #[tokio::test]
    async fn writes_receipt_when_task_transitions_to_done() {
        let db = db::DBService::new().await.unwrap();
        let temp_dir = TempDir::new().unwrap();
        let service = mock_service(temp_dir.path().join("receipts"));

        let project = Project::create(
            &db.pool,
            &CreateProject {
                name: "Receipts".to_string(),
                repositories: vec![],
            },
            Uuid::new_v4(),
        )
        .await
        .unwrap();

        let task = Task::create(
            &db.pool,
            &CreateTask {
                project_id: project.id,
                title: "Render receipt".to_string(),
                description: None,
                status: Some(TaskStatus::InProgress),
                parent_workspace_id: None,
                source_cron_task_id: None,
                image_ids: None,
            },
            Uuid::new_v4(),
        )
        .await
        .unwrap();

        let workspace = Workspace::create(
            &db.pool,
            &CreateWorkspace {
                branch: "receipt-branch".to_string(),
                agent_working_dir: None,
            },
            Uuid::new_v4(),
            task.id,
        )
        .await
        .unwrap();

        let session = Session::create(
            &db.pool,
            &CreateSession {
                executor: Some("CODEX".to_string()),
            },
            Uuid::new_v4(),
            workspace.id,
        )
        .await
        .unwrap();

        let action = ExecutorAction::new(
            ExecutorActionType::CodingAgentInitialRequest(CodingAgentInitialRequest {
                prompt: "Implement it".to_string(),
                executor_profile_id: ExecutorProfileId::new(BaseCodingAgent::Codex),
                working_dir: None,
            }),
            None,
        );

        let execution = ExecutionProcess::create(
            &db.pool,
            &CreateExecutionProcess {
                session_id: session.id,
                executor_action: action,
                run_reason: ExecutionProcessRunReason::CodingAgent,
            },
            Uuid::new_v4(),
            &[],
        )
        .await
        .unwrap();
        ExecutionProcess::update_completion(
            &db.pool,
            execution.id,
            ExecutionProcessStatus::Completed,
            Some(0),
        )
        .await
        .unwrap();

        let _turn = CodingAgentTurn::create(
            &db.pool,
            &CreateCodingAgentTurn {
                execution_process_id: execution.id,
                prompt: Some("Implement it".to_string()),
            },
            Uuid::new_v4(),
        )
        .await
        .unwrap();
        CodingAgentTurn::update_agent_session_id(&db.pool, execution.id, "session-xyz")
            .await
            .unwrap();

        let done_task = Task::update(
            &db.pool,
            task.id,
            task.project_id,
            task.title.clone(),
            None,
            TaskStatus::Done,
            None,
        )
        .await
        .unwrap();

        let transition = TaskStateTransition::new(done_task, Some(TaskStatus::InProgress));
        service
            .handle_done_transition(&db.pool, &transition)
            .await
            .unwrap();

        timeout(Duration::from_secs(3), async {
            loop {
                let month_dir = temp_dir
                    .path()
                    .join("receipts")
                    .join(Utc::now().format("%Y%m").to_string());
                let files = fs::read_dir(&month_dir)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .collect::<Vec<_>>();
                if files.len() == 1 {
                    let png = fs::read(files[0].path()).unwrap();
                    assert!(png.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]));
                    assert!(png.len() > 1_000);
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }
}
