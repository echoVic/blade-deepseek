use std::path::Path;

use crate::approval::policy::ActionKind;
use crate::provider::system_prompt::build_system_prompt;
use crate::runtime::instructions::ProjectInstructions;
use crate::runtime::subagent_types::SubagentType;
use crate::tools;

pub fn build_agent_system_prompt(
    cwd: &Path,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: Option<&ProjectInstructions>,
) -> String {
    let mut prompt = build_system_prompt(cwd);
    if let Some(block) = instructions.and_then(ProjectInstructions::to_system_prompt_block) {
        prompt.push_str("\n\n");
        prompt.push_str(&block);
    }
    if subagent_depth > 0 {
        prompt.push_str(
            "\n\n## Subagent Role\nYou are running as a synchronous subagent. Complete only the delegated task and return a concise report for the parent agent. Do not assume the user can see your intermediate tool output.",
        );
        let suffix = subagent_type.system_prompt_suffix();
        if !suffix.is_empty() {
            prompt.push_str(suffix);
        }
    }
    prompt
}

pub fn format_tool_result_for_model(result: &tools::ToolResult) -> String {
    match (&result.output, &result.error) {
        (Some(output), _) => {
            if result.truncated {
                format!("{output}\n[output truncated]")
            } else {
                output.clone()
            }
        }
        (_, Some(error)) => format!("ERROR: {error}"),
        _ => "(no output)".to_string(),
    }
}

pub fn requires_approval(action: ActionKind) -> bool {
    matches!(action, ActionKind::Write | ActionKind::Shell)
}
