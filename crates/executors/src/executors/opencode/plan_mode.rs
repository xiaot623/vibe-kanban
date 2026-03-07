use std::path::{Component, Path};

pub const EXIT_PLAN_MODE_NAME: &str = "ExitPlanMode";
pub(super) const REQUEST_USER_INPUT_TOOL_NAME: &str = "request_user_input";

const PLAN_EXIT_QUESTION_PREFIX: &str = "Plan at ";
const PLAN_EXIT_QUESTION_SUFFIX: &str =
    " is complete. Would you like to switch to the build agent and start implementing?";
const PLAN_RELATIVE_PREFIX: &str = ".opencode/plans/";
const PATH_TOKEN_BOUNDARY_CHARS: [char; 13] = [
    '"', '\'', '`', '(', ')', '[', ']', '{', '}', '<', '>', ',', ';',
];
const PATH_TOKEN_END_CHARS: [char; 14] = [
    '"', '\'', '`', '(', ')', '[', ']', '{', '}', '<', '>', ',', ';', ':',
];

pub(super) fn parse_plan_exit_relative_path(question_text: &str) -> Option<String> {
    let trimmed = question_text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized_question = trimmed.replace('\\', "/");
    let legacy_path = normalized_question
        .strip_prefix(PLAN_EXIT_QUESTION_PREFIX)
        .and_then(|value| value.strip_suffix(PLAN_EXIT_QUESTION_SUFFIX))
        .map(str::trim);
    if let Some(relative) = legacy_path.and_then(normalize_plan_relative_path) {
        return Some(relative);
    }

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
    use super::*;

    fn build_plan_exit_question(path: &str) -> String {
        format!("{PLAN_EXIT_QUESTION_PREFIX}{path}{PLAN_EXIT_QUESTION_SUFFIX}")
    }

    #[test]
    fn parse_plan_exit_relative_path_accepts_valid_relative_path() {
        let question = build_plan_exit_question(".opencode/plans/2026-03-07-plan.md");
        assert_eq!(
            parse_plan_exit_relative_path(&question),
            Some(".opencode/plans/2026-03-07-plan.md".to_string())
        );
    }

    #[test]
    fn parse_plan_exit_relative_path_accepts_absolute_path_and_normalizes_to_relative() {
        let question = build_plan_exit_question("/tmp/worktree/.opencode/plans/2026-03-07-plan.md");
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
        let question = build_plan_exit_question("/tmp/plan.md");
        assert_eq!(parse_plan_exit_relative_path(&question), None);
    }

    #[test]
    fn parse_plan_exit_relative_path_rejects_parent_dir_escape() {
        let question = build_plan_exit_question(".opencode/plans/../../../etc/passwd");
        assert_eq!(parse_plan_exit_relative_path(&question), None);
    }

    #[test]
    fn parse_plan_exit_relative_path_rejects_backslash_parent_dir_escape() {
        let question = build_plan_exit_question(".opencode\\plans\\..\\..\\..\\etc\\passwd");
        assert_eq!(parse_plan_exit_relative_path(&question), None);
    }
}
