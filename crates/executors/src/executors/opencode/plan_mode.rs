use std::path::{Component, Path};

use super::types::QuestionInfo;

pub const EXIT_PLAN_MODE_NAME: &str = "ExitPlanMode";
pub(super) const REQUEST_USER_INPUT_TOOL_NAME: &str = "request_user_input";
const PLAN_TITLE_KEYWORD: &str = "Plan";
const PLAN_MODE_NAME: &str = "plan";
const PLAN_MODE_PROMPT_GUIDANCE: &str = "\n\n[Plan mode guidance]\nWhen asking to exit planning and switch to implementation:\n- Set the question title/header to include the exact keyword `Plan` (for example: `Plan Review`).\n- Include the generated `.opencode/plans/...` path in the question text whenever possible.\n";

const PLAN_RELATIVE_PREFIX: &str = ".opencode/plans/";
const PATH_TOKEN_BOUNDARY_CHARS: [char; 13] = [
    '"', '\'', '`', '(', ')', '[', ']', '{', '}', '<', '>', ',', ';',
];
const PATH_TOKEN_END_CHARS: [char; 14] = [
    '"', '\'', '`', '(', ')', '[', ']', '{', '}', '<', '>', ',', ';', ':',
];

#[derive(Debug, Clone)]
pub(super) struct PlanExitQuestion {
    pub(super) plan_relative_path: Option<String>,
}

pub(super) fn append_plan_mode_prompt_guidance(mode: Option<&str>, prompt: String) -> String {
    if !is_plan_mode(mode) {
        return prompt;
    }

    format!("{prompt}{PLAN_MODE_PROMPT_GUIDANCE}")
}

pub(super) fn detect_plan_exit_question(questions: &[QuestionInfo]) -> Option<PlanExitQuestion> {
    let has_plan_title = questions.iter().any(|item| {
        item.header
            .as_deref()
            .is_some_and(|title| title.contains(PLAN_TITLE_KEYWORD))
    });
    if !has_plan_title {
        return None;
    }

    let plan_relative_path = questions
        .iter()
        .find_map(|item| parse_plan_exit_relative_path(&item.question));

    Some(PlanExitQuestion { plan_relative_path })
}

fn is_plan_mode(mode: Option<&str>) -> bool {
    mode.map(str::trim)
        .is_some_and(|mode| mode.eq_ignore_ascii_case(PLAN_MODE_NAME))
}

fn parse_plan_exit_relative_path(question_text: &str) -> Option<String> {
    let trimmed = question_text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized_question = trimmed.replace('\\', "/");
    let prefix_index = normalized_question.find(PLAN_RELATIVE_PREFIX)?;
    let token_start = normalized_question[..prefix_index]
        .rfind(|character: char| {
            character.is_whitespace() || PATH_TOKEN_BOUNDARY_CHARS.contains(&character)
        })
        .map(|index| index + 1)
        .unwrap_or(0);
    let token_end = normalized_question[prefix_index..]
        .find(|character: char| {
            character.is_whitespace() || PATH_TOKEN_END_CHARS.contains(&character)
        })
        .map(|offset| prefix_index + offset)
        .unwrap_or(normalized_question.len());
    let token = normalized_question[token_start..token_end].trim();

    normalize_plan_relative_path(token)
}

fn normalize_plan_relative_path(path: &str) -> Option<String> {
    let candidate = path.trim();
    if candidate.is_empty() {
        return None;
    }

    let normalized = candidate.replace('\\', "/");
    let prefix_index = normalized.find(PLAN_RELATIVE_PREFIX)?;
    let relative = normalized[prefix_index..]
        .trim_end_matches(|character: char| {
            PATH_TOKEN_END_CHARS.contains(&character) || matches!(character, '!' | '?' | '.')
        })
        .to_string();

    if !relative.starts_with(PLAN_RELATIVE_PREFIX) {
        return None;
    }

    let path = Path::new(&relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }

    Some(relative)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::executors::opencode::types::QuestionInfo;

    #[test]
    fn parse_plan_exit_relative_path_accepts_valid_relative_path() {
        let question = "Plan ready at .opencode/plans/2026-03-07-plan.md. Proceed?";
        assert_eq!(
            parse_plan_exit_relative_path(&question),
            Some(".opencode/plans/2026-03-07-plan.md".to_string())
        );
    }

    #[test]
    fn parse_plan_exit_relative_path_accepts_absolute_path_and_normalizes_to_relative() {
        let question = "Plan file: /tmp/worktree/.opencode/plans/2026-03-07-plan.md; switch mode?";
        assert_eq!(
            parse_plan_exit_relative_path(&question),
            Some(".opencode/plans/2026-03-07-plan.md".to_string())
        );
    }

    #[test]
    fn parse_plan_exit_relative_path_accepts_non_legacy_question_text() {
        let question =
            "Please review `.opencode/plans/next-step.md` before switching to build mode.";
        assert_eq!(
            parse_plan_exit_relative_path(question),
            Some(".opencode/plans/next-step.md".to_string())
        );
    }

    #[test]
    fn parse_plan_exit_relative_path_strips_trailing_sentence_punctuation() {
        let question = "Plan ready at .opencode/plans/next-step.md. Switch to build mode?";
        assert_eq!(
            parse_plan_exit_relative_path(question),
            Some(".opencode/plans/next-step.md".to_string())
        );
    }

    #[test]
    fn parse_plan_exit_relative_path_rejects_absolute_paths() {
        let question = "Plan file: /tmp/plan.md";
        assert_eq!(parse_plan_exit_relative_path(&question), None);
    }

    #[test]
    fn parse_plan_exit_relative_path_rejects_parent_dir_escape() {
        let question = "Plan file: .opencode/plans/../../../etc/passwd";
        assert_eq!(parse_plan_exit_relative_path(&question), None);
    }

    #[test]
    fn parse_plan_exit_relative_path_rejects_backslash_parent_dir_escape() {
        let question = "Plan file: .opencode\\plans\\..\\..\\..\\etc\\passwd";
        assert_eq!(parse_plan_exit_relative_path(&question), None);
    }

    #[test]
    fn detect_plan_exit_question_requires_plan_keyword_in_title() {
        let questions: Vec<QuestionInfo> = serde_json::from_value(json!([
            {
                "question": "Plan generated at .opencode/plans/test.md",
                "header": "Build Agent",
                "options": []
            }
        ]))
        .expect("question payload should parse");

        assert!(detect_plan_exit_question(&questions).is_none());
    }

    #[test]
    fn detect_plan_exit_question_accepts_plan_title_without_path() {
        let questions: Vec<QuestionInfo> = serde_json::from_value(json!([
            {
                "question": "Would you like to continue?",
                "header": "Plan Review",
                "options": []
            }
        ]))
        .expect("question payload should parse");

        let detected = detect_plan_exit_question(&questions).expect("should be detected");
        assert_eq!(detected.plan_relative_path, None);
    }

    #[test]
    fn detect_plan_exit_question_extracts_plan_path_when_available() {
        let questions: Vec<QuestionInfo> = serde_json::from_value(json!([
            {
                "question": "Plan generated at /tmp/worktree/.opencode/plans/test.md",
                "header": "Plan Review",
                "options": []
            }
        ]))
        .expect("question payload should parse");

        let detected = detect_plan_exit_question(&questions).expect("should be detected");
        assert_eq!(
            detected.plan_relative_path.as_deref(),
            Some(".opencode/plans/test.md")
        );
    }

    #[test]
    fn append_plan_mode_prompt_guidance_applies_in_plan_mode() {
        let prompt = append_plan_mode_prompt_guidance(Some("plan"), "Task".to_string());
        assert!(prompt.contains("[Plan mode guidance]"));
    }

    #[test]
    fn append_plan_mode_prompt_guidance_skips_non_plan_mode() {
        let prompt = append_plan_mode_prompt_guidance(Some("build"), "Task".to_string());
        assert_eq!(prompt, "Task");
    }
}
