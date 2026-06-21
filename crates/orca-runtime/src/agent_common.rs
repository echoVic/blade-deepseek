use std::path::Path;

use orca_core::approval_types::{ActionKind, ApprovalMode};
use orca_core::goal_types::ThreadGoal;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types::ToolResult;
use orca_provider::system_prompt::build_system_prompt;
use orca_tools::skills;

use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;

pub fn build_agent_system_prompt(
    cwd: &Path,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: Option<&ProjectInstructions>,
    approval_mode: ApprovalMode,
    memory: Option<&MemoryBlock>,
) -> String {
    build_agent_system_prompt_with_goal(
        cwd,
        subagent_depth,
        subagent_type,
        instructions,
        approval_mode,
        memory,
        None,
    )
}

pub fn build_agent_system_prompt_with_goal(
    cwd: &Path,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: Option<&ProjectInstructions>,
    approval_mode: ApprovalMode,
    memory: Option<&MemoryBlock>,
    active_goal: Option<&ThreadGoal>,
) -> String {
    let mut prompt = build_system_prompt(cwd);
    if let Some(block) = memory.and_then(MemoryBlock::to_system_prompt_block) {
        prompt.push_str("\n\n");
        prompt.push_str(&block);
    }
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
    if approval_mode == ApprovalMode::Plan {
        prompt.push_str(
            "\n\n## Plan Mode\nYou are in read-only planning mode. You may analyze and inspect context, but you must not modify files, run shell commands, or perform write actions.",
        );
    }
    if let Some(goal) = active_goal {
        prompt.push_str("\n\n");
        prompt.push_str(&format_goal_mode_instructions(goal));
    }
    prompt
}

pub fn explicit_skill_context(cwd: &Path, prompt: &str) -> Option<String> {
    match skills::explicit_skill_prompt_block(cwd, prompt) {
        Ok(block) => block,
        Err(error) => {
            eprintln!("orca: warning: failed to load explicit skills: {error}");
            None
        }
    }
}

pub fn append_explicit_skill_context(system_prompt: &mut String, cwd: &Path, prompt: &str) {
    if let Some(block) = explicit_skill_context(cwd, prompt) {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&block);
    }
}

pub fn format_goal_mode_instructions(goal: &ThreadGoal) -> String {
    format!(
        "## Goal Mode\nThe active goal is: {}\nContinue working until the goal is complete or genuinely blocked. When the goal is complete, call update_goal with status \"complete\". When progress is genuinely blocked and needs user input or an external change, call update_goal with status \"blocked\" and explain the blocker. Do not mark the goal complete just because one turn ended.",
        goal.objective
    )
}

pub fn format_tool_result_for_model(result: &ToolResult) -> String {
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
    matches!(
        action,
        ActionKind::Write | ActionKind::Network | ActionKind::Agent | ActionKind::Shell
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::goal_types::{ThreadGoal, ThreadGoalStatus};

    #[test]
    fn goal_mode_instructions_name_objective_and_update_tool() {
        let goal = ThreadGoal {
            session_id: "session-1".to_string(),
            objective: "Finish persistent goal mode".to_string(),
            status: ThreadGoalStatus::Active,
            token_budget: None,
            tokens_used: 0,
            time_used_seconds: 0,
            created_at: 1,
            updated_at: 2,
        };

        let instructions = format_goal_mode_instructions(&goal);

        assert!(instructions.contains("Finish persistent goal mode"));
        assert!(instructions.contains("update_goal"));
        assert!(instructions.contains("complete"));
        assert!(instructions.contains("blocked"));
    }
}
