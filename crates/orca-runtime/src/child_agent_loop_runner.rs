use std::io;
use std::path::Path;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::RunStatus;
use orca_core::tool_types::{ToolRequest, ToolStatus};

use crate::child_agent_entrypoints::run_child_agent_with_executor;
use crate::child_agent_loop_setup::{
    ChildAgentTurnBudget, advance_child_agent_turn, prepare_child_agent_loop,
};
use crate::child_agent_provider_turn::{
    ChildAgentProviderErrorDecision, ChildAgentProviderTurn,
    compact_child_agent_conversation_if_needed, handle_child_agent_provider_error,
    route_child_agent_model, run_child_agent_provider_turn,
    run_child_agent_provider_turn_observed,
};
use crate::child_agent_response_folding::{
    ChildAgentProviderResponseFold, ChildAgentToolContext, ChildAgentToolExecution,
    ChildAgentToolResultFold, child_agent_tool_requests, fold_child_agent_provider_response,
    fold_child_agent_tool_result,
};
use crate::child_agent_types::{
    ChildAgentActivity, ChildAgentActivityObserver, ChildAgentRequest, ChildAgentResult,
};
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;

pub struct ChildAgentLoopContext<'a> {
    pub request: &'a ChildAgentRequest,
    pub cwd: &'a Path,
    pub instructions: &'a ProjectInstructions,
    pub memory: &'a MemoryBlock,
    pub hooks: &'a HookRunner,
    pub child_cost_tracker: &'a mut CostTracker,
}

pub fn run_child_agent_loop_with_tool_executor<F>(
    config: &RunConfig,
    context: ChildAgentLoopContext<'_>,
    mut execute_tool: F,
) -> io::Result<ChildAgentResult>
where
    F: FnMut(&ChildAgentToolContext<'_>, &CancelToken, &ToolRequest) -> ChildAgentToolExecution,
{
    let mut setup = prepare_child_agent_loop(
        config,
        context.request,
        context.cwd,
        context.instructions,
        context.memory,
    );
    loop {
        match advance_child_agent_turn(&mut setup) {
            ChildAgentTurnBudget::Continue => {}
            ChildAgentTurnBudget::Stop(result) => return Ok(result),
        }

        compact_child_agent_conversation_if_needed(config, &mut setup, context.cwd, context.hooks)?;

        let child_cancel = CancelToken::new();
        let turn_provider_config =
            route_child_agent_model(config, context.request, &setup, context.child_cost_tracker);

        let response = match run_child_agent_provider_turn(
            config,
            &setup,
            context.cwd,
            context.hooks,
            &turn_provider_config,
            &child_cancel,
        ) {
            ChildAgentProviderTurn::Response(response) => response,
            ChildAgentProviderTurn::Fail(result) => return Ok(result),
        };

        match handle_child_agent_provider_error(
            config,
            &mut setup,
            context.cwd,
            context.hooks,
            &response,
        )? {
            Some(ChildAgentProviderErrorDecision::RetryAfterCompaction) => continue,
            Some(ChildAgentProviderErrorDecision::Fail(result)) => return Ok(result),
            None => {}
        }

        match fold_child_agent_provider_response(&mut setup, &response, context.child_cost_tracker)
        {
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
                context.child_cost_tracker,
            ) {
                ChildAgentToolResultFold::Continue => {}
                ChildAgentToolResultFold::Stop(result) => return Ok(result),
            }
        }
    }
}

pub fn run_child_agent_loop_with_tool_executor_observed<F>(
    config: &RunConfig,
    context: ChildAgentLoopContext<'_>,
    observer: Option<&ChildAgentActivityObserver<'_>>,
    mut execute_tool: F,
) -> io::Result<ChildAgentResult>
where
    F: FnMut(&ChildAgentToolContext<'_>, &CancelToken, &ToolRequest) -> ChildAgentToolExecution,
{
    let mut setup = prepare_child_agent_loop(
        config,
        context.request,
        context.cwd,
        context.instructions,
        context.memory,
    );
    loop {
        match advance_child_agent_turn(&mut setup) {
            ChildAgentTurnBudget::Continue => {
                if let Some(observer) = observer {
                    observer.emit(ChildAgentActivity::TurnStarted { turn: setup.turn });
                }
            }
            ChildAgentTurnBudget::Stop(result) => return Ok(result),
        }

        compact_child_agent_conversation_if_needed(config, &mut setup, context.cwd, context.hooks)?;

        let child_cancel = CancelToken::new();
        let turn_provider_config =
            route_child_agent_model(config, context.request, &setup, context.child_cost_tracker);

        let response = match run_child_agent_provider_turn_observed(
            config,
            &setup,
            context.cwd,
            context.hooks,
            &turn_provider_config,
            &child_cancel,
            observer,
        ) {
            ChildAgentProviderTurn::Response(response) => response,
            ChildAgentProviderTurn::Fail(result) => return Ok(result),
        };

        match handle_child_agent_provider_error(
            config,
            &mut setup,
            context.cwd,
            context.hooks,
            &response,
        )? {
            Some(ChildAgentProviderErrorDecision::RetryAfterCompaction) => continue,
            Some(ChildAgentProviderErrorDecision::Fail(result)) => return Ok(result),
            None => {}
        }

        let had_usage = response.usage.as_ref().is_some_and(|usage| !usage.is_empty());
        let provider_fold =
            fold_child_agent_provider_response(&mut setup, &response, context.child_cost_tracker);
        if had_usage {
            if let Some(observer) = observer {
                observer.emit(ChildAgentActivity::Usage(context.child_cost_tracker.totals()));
            }
        }
        match provider_fold {
            ChildAgentProviderResponseFold::Complete(result) => return Ok(result),
            ChildAgentProviderResponseFold::ContinueToTools => {}
        }

        for tool_request in child_agent_tool_requests(&response) {
            let tool_context = ChildAgentToolContext {
                policy: &setup.policy,
                mcp_registry: &setup.mcp_registry,
            };
            if let Some(observer) = observer {
                observer.emit(ChildAgentActivity::ToolStarted {
                    name: tool_request.name.as_str().to_string(),
                    target: tool_request.target.clone(),
                });
            }
            let tool_execution = execute_tool(&tool_context, &child_cancel, tool_request);
            let had_child_cost = tool_execution.child_cost.is_some();
            if let Some(observer) = observer {
                observer.emit(ChildAgentActivity::ToolCompleted {
                    name: tool_request.name.as_str().to_string(),
                    status: run_status_from_tool_status(tool_execution.result.status),
                });
            }
            match fold_child_agent_tool_result(
                &mut setup,
                tool_request,
                tool_execution.should_stop,
                tool_execution.result,
                tool_execution.child_cost,
                context.child_cost_tracker,
            ) {
                ChildAgentToolResultFold::Continue => {}
                ChildAgentToolResultFold::Stop(result) => return Ok(result),
            }
            if had_child_cost {
                if let Some(observer) = observer {
                    observer.emit(ChildAgentActivity::Usage(context.child_cost_tracker.totals()));
                }
            }
        }
    }
}

fn run_status_from_tool_status(status: ToolStatus) -> RunStatus {
    match status {
        ToolStatus::Completed => RunStatus::Success,
        ToolStatus::Denied => RunStatus::ApprovalRequired,
        ToolStatus::Failed | ToolStatus::NotImplemented => RunStatus::Failed,
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
            ChildAgentLoopContext {
                request,
                cwd,
                instructions,
                memory,
                hooks,
                child_cost_tracker,
            },
            |tool_context, child_cancel, tool_request| {
                execute_tool(config, request, tool_context, child_cancel, tool_request)
            },
        )
    })
}

pub fn run_child_agent_with_tool_executor_observed<F>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    observer: Option<&ChildAgentActivityObserver<'_>>,
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
        run_child_agent_loop_with_tool_executor_observed(
            config,
            ChildAgentLoopContext {
                request,
                cwd,
                instructions,
                memory,
                hooks,
                child_cost_tracker,
            },
            observer,
            |tool_context, child_cancel, tool_request| {
                execute_tool(config, request, tool_context, child_cancel, tool_request)
            },
        )
    })
}
