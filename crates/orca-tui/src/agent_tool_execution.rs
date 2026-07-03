use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::EventFactory;
use orca_core::hook_types::HookEvent;
use orca_core::tool_types;
use orca_mcp::McpRegistry;
use orca_runtime::cost::CostTracker;
use orca_runtime::hooks::{HookContext, HookRunner, tool_request_with_hook_outcome};
use orca_runtime::instructions::ProjectInstructions;
use orca_runtime::lifecycle::RuntimeToolActorContext;
use orca_runtime::memory::MemoryBlock;
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::tool_invocation::prepare_tool_invocation;

use crate::agent_runner::{
    DEFAULT_MAX_TURNS, send_runtime_event_as_tui, send_tool_completed_for_tui,
    send_tool_requested_for_tui,
};
use crate::agent_subagent_execution::{execute_subagent_for_tui, execute_subagent_status_for_tui};
use crate::agent_workflow_execution::{
    execute_workflow_draft_action_for_tui, execute_workflow_draft_for_tui, execute_workflow_for_tui,
};
use crate::diff;
use crate::runtime_interaction_adapter::{
    TuiToolApprovalOutcome, TuiUserInputHandler, resolve_tui_tool_approval,
};
use crate::types::{TuiEvent, UserAction};

pub(crate) fn execute_readonly_batch_for_tui(
    cwd: &Path,
    tool_requests: &[tool_types::ToolRequest],
    event_tx: &Sender<TuiEvent>,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    output_truncation: tool_types::ToolOutputTruncation,
) -> Vec<tool_types::ToolResult> {
    let mut hook_failed: Vec<Option<tool_types::ToolResult>> = vec![None; tool_requests.len()];
    let mut runnable = Vec::new();
    let mut events = EventFactory::new("tui-readonly-batch".to_string());

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        send_tool_requested_for_tui(event_tx, &mut events, tool_request);
        match hooks.run(
            HookEvent::PreToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: None,
            },
        ) {
            Ok(outcome) => {
                runnable.push((idx, tool_request_with_hook_outcome(tool_request, &outcome)));
            }
            Err(error) => {
                hook_failed[idx] = Some(tool_types::ToolResult::failed(
                    tool_request,
                    format!("pre_tool_use hook blocked tool: {error}"),
                    None,
                ));
            }
        }
    }

    let mut results = orca_tools::run_readonly_batch_parallel_with_policy(
        tool_requests,
        runnable,
        cwd,
        mcp_registry,
        output_truncation,
    );

    for (idx, failed) in hook_failed.into_iter().enumerate() {
        if let Some(result) = failed {
            results[idx] = result;
        }
    }

    for (tool_request, result) in tool_requests.iter().zip(results.iter()) {
        send_tool_completed_for_tui(event_tx, &mut events, result, None);
        if let Err(error) = hooks.run(
            HookEvent::PostToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: Some(result),
                before_messages: None,
                after_messages: None,
                usage: None,
            },
        ) {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "post_tool_use hook failed: {error}"
            )));
        }
    }

    results
}

pub(crate) fn execute_tool_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
    session_id: Option<&str>,
    policy: &ApprovalPolicy,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    task_registry: Option<&TaskRegistry>,
    cancel: &CancelToken,
) -> (bool, tool_types::ToolResult, Option<CostTracker>) {
    let invocation = prepare_tool_invocation(tool_request, subagent_depth, mcp_registry, config);
    let mut events = EventFactory::new(
        session_id
            .map(str::to_string)
            .unwrap_or_else(|| "tui-tool-session".to_string()),
    );
    let mut runtime_context =
        RuntimeToolActorContext::new(events.run_id().to_string(), DEFAULT_MAX_TURNS);
    if let TuiToolApprovalOutcome::Denied(result) = resolve_tui_tool_approval(
        &invocation,
        tool_request,
        policy,
        &mut runtime_context,
        event_tx,
        action_rx,
    ) {
        send_tool_requested_for_tui(event_tx, &mut events, tool_request);
        send_tool_completed_for_tui(event_tx, &mut events, &result, None);
        return (true, result, None);
    }

    let mut rendered_diff = None;
    let (result, child_cost) = if tool_request.name == tool_types::ToolName::Subagent {
        let (r, c) = execute_subagent_for_tui(
            config,
            cwd,
            tool_request,
            event_tx,
            action_rx,
            subagent_depth,
            instructions,
            memory,
            hooks,
            task_registry,
        );
        (r, Some(c))
    } else {
        send_tool_requested_for_tui(event_tx, &mut events, tool_request);
        let pre_tool_outcome = match hooks.run(
            HookEvent::PreToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: None,
            },
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let result = tool_types::ToolResult::failed(
                    tool_request,
                    format!("pre_tool_use hook blocked tool: {error}"),
                    None,
                );
                send_tool_completed_for_tui(event_tx, &mut events, &result, None);
                return (true, result, None);
            }
        };
        let effective_tool_request =
            tool_request_with_hook_outcome(tool_request, &pre_tool_outcome);
        let execution_request = &effective_tool_request;
        let before = diff::capture_before(execution_request, cwd);
        let result = if execution_request.name == tool_types::ToolName::Bash {
            let mut on_output = |chunk: &str| {
                let _ = event_tx.send(TuiEvent::ToolOutputDelta {
                    id: execution_request.id.clone(),
                    chunk: chunk.to_string(),
                });
            };
            orca_tools::bash::execute_streaming_with_policy_or_cancel(
                execution_request,
                cwd,
                config.tools.output_truncation,
                std::time::Duration::from_secs(config.tools.shell_timeout_secs.max(1)),
                &mut on_output,
                || cancel.is_cancelled(),
            )
        } else if execution_request.name == tool_types::ToolName::RequestUserInput {
            execute_user_input_request_for_tui(execution_request, event_tx, action_rx)
        } else if execution_request.name == tool_types::ToolName::WorkflowDraft {
            let Some(task_registry) = task_registry else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "workflow draft tools require a main TUI session",
                        None,
                    ),
                    None,
                );
            };
            execute_workflow_draft_for_tui(config, cwd, execution_request, task_registry)
        } else if execution_request.name == tool_types::ToolName::WorkflowDraftAction {
            let Some(task_registry) = task_registry else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "workflow draft action tools require a main TUI session",
                        None,
                    ),
                    None,
                );
            };
            execute_workflow_draft_action_for_tui(
                config,
                cwd,
                execution_request,
                event_tx,
                task_registry,
            )
        } else if execution_request.name == tool_types::ToolName::Workflow {
            let Some(task_registry) = task_registry else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "workflow tools require a main TUI session",
                        None,
                    ),
                    None,
                );
            };
            execute_workflow_for_tui(config, cwd, execution_request, event_tx, task_registry)
        } else if execution_request.name == tool_types::ToolName::SubagentStatus {
            let Some(task_registry) = task_registry else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "subagent_status requires a main TUI session",
                        None,
                    ),
                    None,
                );
            };
            execute_subagent_status_for_tui(execution_request, task_registry)
        } else if execution_request.name == tool_types::ToolName::TaskList {
            let Some(task_registry) = task_registry else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "task_list requires a main TUI session",
                        None,
                    ),
                    None,
                );
            };
            let mut runtime_context =
                RuntimeToolActorContext::new("tui-task-list", DEFAULT_MAX_TURNS);
            runtime_context.execute_task_list_tool(execution_request, task_registry)
        } else if execution_request.name == tool_types::ToolName::TaskStop {
            let Some(task_registry) = task_registry else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "task_stop requires a main TUI session",
                        None,
                    ),
                    None,
                );
            };
            let mut runtime_context =
                RuntimeToolActorContext::new("tui-task-stop", DEFAULT_MAX_TURNS);
            runtime_context.execute_task_stop_tool(execution_request, task_registry)
        } else if matches!(
            execution_request.name,
            tool_types::ToolName::GetGoal
                | tool_types::ToolName::CreateGoal
                | tool_types::ToolName::UpdateGoal
        ) {
            let Some(session_id) = session_id.map(str::to_string) else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "goal tools require a persistent goal session",
                        None,
                    ),
                    None,
                );
            };
            let handler = Arc::new(
                move |operation: orca_tools::update_goal::GoalToolOperation| {
                    let mut store = orca_runtime::goals::GoalStore::load_default();
                    match operation {
                        orca_tools::update_goal::GoalToolOperation::Get => {
                            store.get(&session_id).map_err(|error| error.to_string())
                        }
                        orca_tools::update_goal::GoalToolOperation::Create {
                            objective,
                            token_budget,
                        } => match store.get(&session_id).map_err(|error| error.to_string())? {
                            Some(goal) if goal.status.should_continue() => Ok(None),
                            Some(goal) if !goal.status.is_terminal() => Ok(None),
                            _ => store
                                .replace(
                                    &session_id,
                                    &objective,
                                    orca_core::goal_types::ThreadGoalStatus::Active,
                                    token_budget,
                                )
                                .map(Some)
                                .map_err(|error| error.to_string()),
                        },
                        orca_tools::update_goal::GoalToolOperation::Update(update) => store
                            .update(&session_id, update)
                            .map_err(|error| error.to_string()),
                    }
                },
            );
            orca_tools::update_goal::with_goal_handler(handler, || {
                orca_tools::execute_with_mcp_external_and_policy(
                    execution_request,
                    cwd,
                    mcp_registry,
                    &config.external_tools,
                    config.tools.output_truncation,
                    config.tools.shell_timeout_secs,
                )
            })
        } else {
            orca_tools::execute_with_mcp_external_and_policy(
                execution_request,
                cwd,
                mcp_registry,
                &config.external_tools,
                config.tools.output_truncation,
                config.tools.shell_timeout_secs,
            )
        };
        if matches!(result.status, tool_types::ToolStatus::Completed) {
            rendered_diff = before.and_then(diff::render_after);
        }
        (result, None)
    };

    if tool_request.name != tool_types::ToolName::Subagent {
        send_tool_completed_for_tui(event_tx, &mut events, &result, rendered_diff);
        if tool_request.name == tool_types::ToolName::UpdatePlan
            && result.status == tool_types::ToolStatus::Completed
        {
            match orca_tools::update_plan::parse_args(tool_request) {
                Ok(update) => {
                    send_runtime_event_as_tui(event_tx, events.plan_updated(&update));
                }
                Err(error) => {
                    let _ = event_tx.send(TuiEvent::Error(format!(
                        "failed to render plan update: {error}"
                    )));
                }
            }
        }
        if let Err(error) = hooks.run(
            HookEvent::PostToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: Some(&result),
                before_messages: None,
                after_messages: None,
                usage: None,
            },
        ) {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "post_tool_use hook failed: {error}"
            )));
        }
    }

    let should_stop = should_stop_after_tui_tool_result(tool_request, &result);
    (should_stop, result, child_cost)
}

fn should_stop_after_tui_tool_result(
    tool_request: &tool_types::ToolRequest,
    result: &tool_types::ToolResult,
) -> bool {
    matches!(result.status, tool_types::ToolStatus::Denied)
        || (tool_request.name == tool_types::ToolName::RequestUserInput
            && result.status == tool_types::ToolStatus::Failed)
}

fn execute_user_input_request_for_tui(
    request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
) -> tool_types::ToolResult {
    let handler = TuiUserInputHandler::new(event_tx, action_rx);
    let mut runtime_context = RuntimeToolActorContext::new("tui-user-input", DEFAULT_MAX_TURNS);
    match runtime_context.execute_user_input_tool(request, &handler) {
        Ok(result) => result,
        Err(error) => tool_types::ToolResult::failed(request, error.to_string(), None),
    }
}

#[cfg(test)]
pub(crate) fn canonical_action_for_tool(
    tool_request: &tool_types::ToolRequest,
    mcp_registry: &McpRegistry,
    external_tools: &[orca_core::external_config::ExternalToolConfig],
) -> orca_core::approval_types::ActionKind {
    orca_tools::canonical_action_kind_with_mcp_and_external(
        tool_request,
        Some(mcp_registry),
        external_tools,
    )
}
