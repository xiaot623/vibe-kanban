use db::models::{
    execution_process::ExecutionProcess, project::Project, short_id_mapping::ShortIdMapping,
    task::Task,
};
use teloxide::{prelude::*, types::InlineKeyboardMarkup};
use uuid::Uuid;

use super::{
    super::shared::{parse_task_status, truncate_text},
    CardRenderContext, TelegramBotService,
};
use crate::services::telegram::{EXIT_PLAN_MODE_NAME, callback::CallbackAction, keyboard};

const PAGE_SIZE: usize = 10;

pub(super) async fn show_projects_for_browsing(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    match Project::find_all(&service.db.pool).await {
        Ok(projects) if projects.is_empty() => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                "No projects found.",
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
        Ok(projects) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                "Select a project:",
                Some(keyboard::project_list_keyboard(&projects, false)),
            )
            .await?;
        }
        Err(e) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to load projects: {e}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
    }
    Ok(())
}

pub(super) async fn show_projects_for_new_task(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    match Project::find_all(&service.db.pool).await {
        Ok(projects) if projects.is_empty() => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                "No projects found. Create a project first.",
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
        Ok(projects) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                "➕ Select a project for the new task:",
                Some(keyboard::project_list_keyboard(&projects, true)),
            )
            .await?;
        }
        Err(e) => {
            super::ui::render_or_send_card(
                bot,
                chat_id,
                card_context,
                format!("Failed to load projects: {e}"),
                Some(super::ui::empty_inline_keyboard()),
            )
            .await?;
        }
    }
    Ok(())
}

pub(super) async fn show_pending_approvals(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let pending = service.approvals.list_pending();
    let plan_approvals: Vec<_> = pending
        .iter()
        .filter(|a| a.tool_name == EXIT_PLAN_MODE_NAME)
        .collect();

    if plan_approvals.is_empty() {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            "No pending approvals.",
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    }

    let mut rows = Vec::new();
    let mut text = format!("⏳ Pending approvals ({}):\n", plan_approvals.len());

    for approval in &plan_approvals {
        let ctx =
            ExecutionProcess::load_context(&service.db.pool, approval.execution_process_id).await;
        if let Ok(ctx) = ctx {
            let short_id = ShortIdMapping::get_or_create(&service.db.pool, ctx.task.id)
                .await
                .unwrap_or_else(|_| "????".to_string());
            text.push_str(&format!(
                "\n[{}] {}",
                short_id,
                truncate_text(&ctx.task.title, 40)
            ));
            rows.push(vec![
                teloxide::types::InlineKeyboardButton::callback(
                    format!("✅ [{}]", short_id),
                    CallbackAction::ApproveConfirm {
                        task_id: ctx.task.id,
                    }
                    .encode(),
                ),
                teloxide::types::InlineKeyboardButton::callback(
                    format!("📝 [{}]", short_id),
                    CallbackAction::RejectInput {
                        task_id: ctx.task.id,
                    }
                    .encode(),
                ),
            ]);
        }
    }

    rows.push(vec![teloxide::types::InlineKeyboardButton::callback(
        "🏠 Home".to_string(),
        CallbackAction::Home.encode(),
    )]);

    super::ui::render_or_send_card(
        bot,
        chat_id,
        card_context,
        text,
        Some(InlineKeyboardMarkup::new(rows)),
    )
    .await?;
    Ok(())
}

pub(super) async fn show_task_list(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    project_id: Uuid,
    status_filter: Option<&str>,
    page: u16,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let Some(project) = super::ui::load_project_or_render_error(
        bot,
        chat_id,
        &service.db.pool,
        project_id,
        card_context,
        "Project not found.",
        "Failed to load project",
    )
    .await?
    else {
        return Ok(());
    };

    // If no status filter, show filter buttons first
    if status_filter.is_none() && page == 0 {
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            format!("📋 {} — filter by status:", project.name),
            Some(keyboard::status_filter_keyboard(project_id)),
        )
        .await?;
        return Ok(());
    }

    let tasks =
        match Task::find_by_project_id_with_attempt_status(&service.db.pool, project.id).await {
            Ok(tasks) => tasks,
            Err(e) => {
                super::ui::render_or_send_card(
                    bot,
                    chat_id,
                    card_context,
                    format!("Failed to load tasks: {e}"),
                    Some(super::ui::empty_inline_keyboard()),
                )
                .await?;
                return Ok(());
            }
        };

    let parsed_status = status_filter.and_then(parse_task_status);
    let filtered: Vec<_> = tasks
        .into_iter()
        .filter(|t| match &parsed_status {
            Some(s) => &t.status == s,
            None => true,
        })
        .collect();

    if filtered.is_empty() {
        let status_label = status_filter.unwrap_or("all");
        super::ui::render_or_send_card(
            bot,
            chat_id,
            card_context,
            format!("No {} tasks in {}.", status_label, project.name),
            Some(super::ui::empty_inline_keyboard()),
        )
        .await?;
        return Ok(());
    }

    let offset = page as usize * PAGE_SIZE;
    let page_tasks = &filtered[offset.min(filtered.len())..];
    let has_more = page_tasks.len() > PAGE_SIZE;
    let page_tasks = &page_tasks[..page_tasks.len().min(PAGE_SIZE)];

    let mut task_buttons = Vec::new();
    for task in page_tasks {
        let short_id = ShortIdMapping::get_or_create(&service.db.pool, task.id)
            .await
            .unwrap_or_else(|_| "????".to_string());
        task_buttons.push((task.id, short_id, task.title.clone()));
    }

    let status_label = status_filter.unwrap_or("all");
    let header = format!(
        "📋 {} — {} tasks ({}):",
        project.name,
        status_label,
        filtered.len()
    );

    super::ui::render_or_send_card(
        bot,
        chat_id,
        card_context,
        header,
        Some(keyboard::task_list_keyboard(
            &task_buttons,
            project_id,
            page,
            has_more,
        )),
    )
    .await?;
    Ok(())
}

pub(super) async fn show_task_detail(
    bot: &Bot,
    chat_id: ChatId,
    service: &TelegramBotService,
    task_id: Uuid,
    card_context: Option<CardRenderContext>,
) -> ResponseResult<()> {
    let Some(task) = super::ui::load_task_or_render_error(
        bot,
        chat_id,
        &service.db.pool,
        task_id,
        card_context,
        "Task not found. It may have been deleted.",
        "Failed to load task",
    )
    .await?
    else {
        return Ok(());
    };

    let project_name = match Project::find_by_id(&service.db.pool, task.project_id).await {
        Ok(Some(project)) => project.name,
        _ => "Unknown project".to_string(),
    };

    let short_id = ShortIdMapping::get_or_create(&service.db.pool, task.id)
        .await
        .unwrap_or_else(|_| "????".to_string());

    let description = task
        .description
        .clone()
        .unwrap_or_else(|| "No description.".to_string());

    let mut message = format!(
        "[{}] {:?}\nProject: {}\nTitle: {}\n\n{}",
        short_id,
        task.status,
        project_name,
        task.title,
        truncate_text(&description, 500)
    );

    if let Some(attempts_info) = service.fetch_attempts_info(task.id).await {
        if !attempts_info.is_empty() {
            message.push_str(&format!("\n\nAttempts ({}):", attempts_info.len()));
            for attempt in attempts_info {
                message.push_str(&format!("\n  • {attempt}"));
            }
        }
    }

    super::ui::render_or_send_card(
        bot,
        chat_id,
        card_context,
        message,
        Some(keyboard::task_detail_keyboard(task.id, &task.status)),
    )
    .await?;
    Ok(())
}
