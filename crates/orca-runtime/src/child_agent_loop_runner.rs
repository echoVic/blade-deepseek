use std::io;
use std::path::Path;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::tool_types::ToolRequest;

use crate::agent_child::run_child_agent_with_executor;
use crate::child_agent_loop_setup::{
    ChildAgentTurnBudget, advance_child_agent_turn, prepare_child_agent_loop,
};
use crate::child_agent_provider_turn::{
    ChildAgentProviderErrorDecision, ChildAgentProviderTurn,
    compact_child_agent_conversation_if_needed, handle_child_agent_provider_error,
    route_child_agent_model, run_child_agent_provider_turn,
};
use crate::child_agent_response_folding::{
    ChildAgentProviderResponseFold, ChildAgentToolContext, ChildAgentToolExecution,
    ChildAgentToolResultFold, child_agent_tool_requests, fold_child_agent_provider_response,
    fold_child_agent_tool_result,
};
use crate::child_agent_types::{ChildAgentRequest, ChildAgentResult};
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;

pub fn run_child_agent_loop_with_tool_executor<F>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    child_cost_tracker: &mut CostTracker,
    mut execute_tool: F,
) -> io::Result<ChildAgentResult>
where
    F: FnMut(&ChildAgentToolContext<'_>, &CancelToken, &ToolRequest) -> ChildAgentToolExecution,
{
    let mut setup = prepare_child_agent_loop(config, request, cwd, instructions, memory);
    loop {
        match advance_child_agent_turn(&mut setup) {
            ChildAgentTurnBudget::Continue => {}
            ChildAgentTurnBudget::Stop(result) => return Ok(result),
        }

        compact_child_agent_conversation_if_needed(config, &mut setup, cwd, hooks)?;

        let child_cancel = CancelToken::new();
        let turn_provider_config =
            route_child_agent_model(config, request, &setup, child_cost_tracker);

        let response = match run_child_agent_provider_turn(
            config,
            &setup,
            cwd,
            hooks,
            &turn_provider_config,
            &child_cancel,
        ) {
            ChildAgentProviderTurn::Response(response) => response,
            ChildAgentProviderTurn::Fail(result) => return Ok(result),
        };

        match handle_child_agent_provider_error(config, &mut setup, cwd, hooks, &response)? {
            Some(ChildAgentProviderErrorDecision::RetryAfterCompaction) => continue,
            Some(ChildAgentProviderErrorDecision::Fail(result)) => return Ok(result),
            None => {}
        }

        match fold_child_agent_provider_response(&mut setup, &response, child_cost_tracker) {
            ChildAgentProviderResponseFold::Complete(result) => return Ok(result),
            ChildAgentProviderResponseFold::ContinueToTools => {}
        }

        for tool_request in child_agent_tool_requests(&response) {
            let tool_context = ChildAgentToolContext {
                policy: &setup.policy,
                mcp_registry: &setup.mcp_registry,
            };
            let tool_execution = execute_tool(&tool_context, &child_cancel, tool_request);
            match fold_child_agent_tool_result(
                &mut setup,
                tool_request,
                tool_execution.should_stop,
                tool_execution.result,
                tool_execution.child_cost,
                child_cost_tracker,
            ) {
                ChildAgentToolResultFold::Continue => {}
                ChildAgentToolResultFold::Stop(result) => return Ok(result),
            }
        }
    }
}

pub fn run_child_agent_with_tool_executor<F>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    mut execute_tool: F,
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
    run_child_agent_with_executor(config, request, |config, request, child_cost_tracker| {
        run_child_agent_loop_with_tool_executor(
            config,
            request,
            cwd,
            instructions,
            memory,
            hooks,
            child_cost_tracker,
            |tool_context, child_cancel, tool_request| {
                execute_tool(config, request, tool_context, child_cancel, tool_request)
            },
        )
    })
}
