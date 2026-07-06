use std::io;
use std::path::Path;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::RunStatus;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types::ToolRequest;

use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;

pub use crate::child_agent_loop_runner::{
    run_child_agent_loop_with_tool_executor, run_child_agent_with_tool_executor,
};
pub use crate::child_agent_loop_setup::{
    ChildAgentLoopSetup, ChildAgentTurnBudget, DEFAULT_CHILD_AGENT_MAX_TURNS,
    advance_child_agent_turn, advance_child_agent_turn_with_limit, prepare_child_agent_loop,
};
pub use crate::child_agent_provider_turn::{
    ChildAgentProviderErrorDecision, ChildAgentProviderTurn,
    compact_child_agent_conversation_if_needed, handle_child_agent_provider_error,
    route_child_agent_model, run_child_agent_provider_turn,
};
pub use crate::child_agent_response_folding::{
    ChildAgentProviderResponseFold, ChildAgentToolContext, ChildAgentToolExecution,
    ChildAgentToolResultFold, child_agent_tool_requests, fold_child_agent_provider_response,
    fold_child_agent_tool_result,
};
pub(crate) use crate::child_agent_types::{ChildAgentExecutor, ChildAgentRuntime};
pub use crate::child_agent_types::{ChildAgentRequest, ChildAgentResult};

pub(crate) fn run_child_agent<W: io::Write>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    runtime: &mut ChildAgentRuntime<'_, W>,
) -> (ChildAgentResult, CostTracker) {
    run_child_agent_with_executor(
        config,
        request,
        |child_config, request, child_cost_tracker| {
            runtime.execute(child_config, request, child_cost_tracker)
        },
    )
}

pub fn run_child_agent_with_executor<F>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    mut executor: F,
) -> (ChildAgentResult, CostTracker)
where
    F: FnMut(&RunConfig, &ChildAgentRequest, &mut CostTracker) -> io::Result<ChildAgentResult>,
{
    let mut child_config = config.clone();
    child_config.model = child_config
        .model
        .with_subagent_override(request.model.clone());
    let mut child_cost_tracker = CostTracker::new(child_config.model.as_deref());
    let result =
        executor(&child_config, request, &mut child_cost_tracker).unwrap_or_else(|error| {
            ChildAgentResult {
                status: RunStatus::Failed,
                final_message: None,
                error: Some(error.to_string()),
            }
        });
    (result, child_cost_tracker)
}

pub fn run_child_agent_prompt_with_tool_executor<F>(
    config: &RunConfig,
    prompt: String,
    subagent_type: &SubagentType,
    subagent_model: Option<String>,
    subagent_depth: u32,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    execute_tool: F,
) -> (ChildAgentResult, CostTracker)
where
    F: FnMut(
        &RunConfig,
        &ChildAgentRequest,
        &ChildAgentToolContext<'_>,
        &CancelToken,
        &ToolRequest,
    ) -> ChildAgentToolExecution,
{
    let request = ChildAgentRequest::new(
        prompt,
        subagent_type.clone(),
        subagent_model,
        subagent_depth,
        false,
    );
    run_child_agent_with_tool_executor(
        config,
        &request,
        cwd,
        instructions,
        memory,
        hooks,
        execute_tool,
    )
}
