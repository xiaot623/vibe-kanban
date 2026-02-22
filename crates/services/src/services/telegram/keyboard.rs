//! Inline keyboard builders for Telegram bot screens.

use db::models::{project::Project, task::TaskStatus};
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use uuid::Uuid;

use super::callback::CallbackAction;

/// Build the home screen keyboard shown after `/start`.
pub fn home_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            btn("📋 Tasks", CallbackAction::Projects),
            btn("➕ New", CallbackAction::NewTask),
        ],
        vec![
            btn("⏳ Pending", CallbackAction::Pending),
            btn("❓ Help", CallbackAction::Noop), // help is shown as text, button is for discoverability
        ],
    ])
}

/// Build a project list keyboard for browsing or creating tasks.
pub fn project_list_keyboard(projects: &[Project], for_new_task: bool) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = projects
        .iter()
        .map(|p| {
            let action = if for_new_task {
                CallbackAction::NewTaskProject { project_id: p.id }
            } else {
                CallbackAction::Tasks {
                    project_id: p.id,
                    status: None,
                }
            };
            vec![btn(&p.name, action)]
        })
        .collect();

    rows.push(vec![btn("🏠 Home", CallbackAction::Home)]);
    InlineKeyboardMarkup::new(rows)
}

/// Build status filter buttons for a project's task list.
pub fn status_filter_keyboard(project_id: Uuid) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            btn(
                "📝 Todo",
                CallbackAction::Tasks {
                    project_id,
                    status: Some("todo".into()),
                },
            ),
            btn(
                "🔄 InProgress",
                CallbackAction::Tasks {
                    project_id,
                    status: Some("inprogress".into()),
                },
            ),
        ],
        vec![
            btn(
                "👀 InReview",
                CallbackAction::Tasks {
                    project_id,
                    status: Some("inreview".into()),
                },
            ),
            btn(
                "✅ Done",
                CallbackAction::Tasks {
                    project_id,
                    status: Some("done".into()),
                },
            ),
        ],
        vec![
            btn(
                "🔢 All",
                CallbackAction::Tasks {
                    project_id,
                    status: None,
                },
            ),
            btn("🏠 Home", CallbackAction::Home),
        ],
    ])
}

/// Build task detail action buttons based on current task status.
pub fn task_detail_keyboard(task_id: Uuid, status: &TaskStatus) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    match status {
        TaskStatus::Todo => {
            rows.push(vec![
                btn("▶️ Run", CallbackAction::RunDefault { task_id }),
                btn("⚙️ Pick executor", CallbackAction::RunPick { task_id }),
            ]);
            rows.push(vec![btn("✏️ Edit", CallbackAction::EditTask { task_id })]);
        }
        TaskStatus::InProgress => {
            rows.push(vec![btn("🔄 Refresh", CallbackAction::Refresh { task_id })]);
        }
        TaskStatus::InReview => {
            rows.push(vec![
                btn("✅ Approve", CallbackAction::ApproveConfirm { task_id }),
                btn("📝 Reject", CallbackAction::RejectInput { task_id }),
            ]);
        }
        TaskStatus::Done => {
            rows.push(vec![btn(
                "▶️ Re-run",
                CallbackAction::RunDefault { task_id },
            )]);
        }
        TaskStatus::Cancelled => {
            rows.push(vec![btn(
                "▶️ Re-run",
                CallbackAction::RunDefault { task_id },
            )]);
        }
    }

    rows.push(vec![btn("🏠 Home", CallbackAction::Home)]);
    InlineKeyboardMarkup::new(rows)
}

/// Build executor selection keyboard.
pub fn executor_pick_keyboard(task_id: Uuid) -> InlineKeyboardMarkup {
    let executors = ["CLAUDE_CODE", "CODEX", "GEMINI", "OPENCODE", "DROID"];
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    // Two executors per row
    for chunk in executors.chunks(2) {
        let row: Vec<InlineKeyboardButton> = chunk
            .iter()
            .map(|&ex| {
                btn(
                    ex,
                    CallbackAction::RunWith {
                        task_id,
                        executor: ex.to_string(),
                    },
                )
            })
            .collect();
        rows.push(row);
    }

    rows.push(vec![btn("❌ Cancel", CallbackAction::Cancel)]);
    InlineKeyboardMarkup::new(rows)
}

/// Build run mode selection keyboard for an executor.
pub fn run_mode_pick_keyboard(
    task_id: Uuid,
    executor: &str,
    modes: &[String],
) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut current_row: Vec<InlineKeyboardButton> = Vec::new();

    for (index, mode) in modes.iter().enumerate() {
        let Ok(mode_index) = u16::try_from(index) else {
            break;
        };
        current_row.push(btn(
            &truncate(mode, 24),
            CallbackAction::RunWithMode {
                task_id,
                executor: executor.to_string(),
                mode_index,
            },
        ));

        if current_row.len() == 2 {
            rows.push(std::mem::take(&mut current_row));
        }
    }

    if !current_row.is_empty() {
        rows.push(current_row);
    }

    rows.push(vec![btn("⬅️ Back", CallbackAction::RunPick { task_id })]);
    rows.push(vec![btn("❌ Cancel", CallbackAction::Cancel)]);
    InlineKeyboardMarkup::new(rows)
}

/// Build approval confirmation keyboard (second-click safety).
pub fn approve_confirm_keyboard(task_id: Uuid) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        btn("✅ Yes, approve", CallbackAction::ApproveYes { task_id }),
        btn("❌ Cancel", CallbackAction::Cancel),
    ]])
}

/// Build the inline keyboard shown in InReview notification messages.
pub fn review_notification_keyboard(task_id: Uuid) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        btn("✅ Approve", CallbackAction::ApproveConfirm { task_id }),
        btn("📝 Reject", CallbackAction::RejectInput { task_id }),
    ]])
}

/// Build a task list with inline buttons for each task.
pub fn task_list_keyboard(
    tasks: &[(Uuid, String, String)], // (task_id, short_id, title)
    project_id: Uuid,
    page: u16,
    has_more: bool,
) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = tasks
        .iter()
        .map(|(task_id, short_id, title)| {
            let label = format!("[{}] {}", short_id, truncate(title, 30));
            vec![btn(
                &label,
                CallbackAction::TaskDetail { task_id: *task_id },
            )]
        })
        .collect();

    // Pagination row
    let mut nav_row = Vec::new();
    if page > 0 {
        nav_row.push(btn(
            "⬅️ Prev",
            CallbackAction::TaskPage {
                project_id,
                page: page - 1,
            },
        ));
    }
    if has_more {
        nav_row.push(btn(
            "➡️ Next",
            CallbackAction::TaskPage {
                project_id,
                page: page + 1,
            },
        ));
    }
    if !nav_row.is_empty() {
        rows.push(nav_row);
    }

    rows.push(vec![btn("🏠 Home", CallbackAction::Home)]);
    InlineKeyboardMarkup::new(rows)
}

/// Build a simple cancel keyboard for dialogue steps.
pub fn cancel_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![btn("❌ Cancel", CallbackAction::Cancel)]])
}

/// Build a skip+cancel keyboard for optional dialogue steps.
pub fn skip_cancel_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        btn("⏭️ Skip", CallbackAction::Skip),
        btn("❌ Cancel", CallbackAction::Cancel),
    ]])
}

/// Build a "go home" keyboard for error/terminal states.
pub fn home_only_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![btn("🏠 Home", CallbackAction::Home)]])
}

// --- helpers ---

fn btn(text: &str, action: CallbackAction) -> InlineKeyboardButton {
    InlineKeyboardButton::callback(text.to_string(), action.encode())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}
