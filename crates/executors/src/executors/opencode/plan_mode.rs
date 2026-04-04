pub const EXIT_PLAN_MODE_NAME: &str = "ExitPlanMode";
const PLAN_MODE_NAME: &str = "plan";
const PLAN_MODE_PROMPT_GUIDANCE: &str = "\n\n[Plan mode guidance]\nWhen asking to exit planning and switch to implementation:\n- Set the question title/header to include the exact keyword `Plan` (for example: `Plan Review`).\n- Include the generated `.opencode/plans/...` path in the question text whenever possible.\n";

pub(super) fn append_plan_mode_prompt_guidance(mode: Option<&str>, prompt: String) -> String {
    if !is_plan_mode(mode) {
        return prompt;
    }

    format!("{prompt}{PLAN_MODE_PROMPT_GUIDANCE}")
}

fn is_plan_mode(mode: Option<&str>) -> bool {
    mode.map(str::trim)
        .is_some_and(|mode| mode.eq_ignore_ascii_case(PLAN_MODE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

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
