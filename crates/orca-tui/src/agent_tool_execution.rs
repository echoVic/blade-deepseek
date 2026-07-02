use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::hook_types::HookEvent;
use orca_core::model::ModelRouteContext;
use orca_core::provider_types::ProviderStep;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types;
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_provider::tool_schema::deepseek_tools_schema_for_type_with_mcp_and_external;
use orca_runtime::agent_common;
use orca_runtime::cost::CostTracker;
use orca_runtime::hooks::{
    HookContext, HookRunner, conversation_with_hook_context, tool_request_with_hook_outcome,
};
use orca_runtime::instructions::ProjectInstructions;
use orca_runtime::lifecycle::RuntimeToolActorContext;
use orca_runtime::memory::MemoryBlock;
use orca_runtime::subagent::{self, SubagentMode};
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::tool_invocation::prepare_tool_invocation;

use crate::agent_runner::{
    DEFAULT_MAX_TURNS, TuiAgentResult, send_runtime_event_as_tui, send_subagent_completed_for_tui,
    send_subagent_started_for_tui, send_tool_completed_for_tui, send_tool_requested_for_tui,
    send_workflow_tasks_updated_for_tui,
};
use crate::agent_workflow_execution::{
    execute_workflow_draft_action_for_tui, execute_workflow_draft_for_tui, execute_workflow_for_tui,
};
use crate::diff;
use crate::runtime_interaction_adapter::{
    TuiToolApprovalOutcome, TuiUserInputHandler, resolve_tui_tool_approval,
};
use crate::types::{TuiEvent, UserAction};

pub(crate) fn should_run_subagent_batch(
    config: &RunConfig,
    tool_request: &tool_types::ToolRequest,
    subagent_depth: u32,
) -> bool {
    tool_request.name == tool_types::ToolName::Subagent
        && subagent_depth < config.subagents.max_depth
        && config.subagents.max_parallel > 1
        && subagent::create_subagent_request(tool_request).mode == SubagentMode::Sync
}

pub(crate) fn collect_subagent_batch(
    config: &RunConfig,
    tool_requests: &[tool_types::ToolRequest],
    start: usize,
) -> usize {
    let max_end = (start + config.subagents.max_parallel).min(tool_requests.len());
    let mut end = start;
    while end < max_end
        && tool_requests[end].name == tool_types::ToolName::Subagent
        && subagent::create_subagent_request(&tool_requests[end]).mode == SubagentMode::Sync
    {
        end += 1;
    }
    end
}

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

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_subagent_batch_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_requests: &[tool_types::ToolRequest],
    event_tx: &Sender<TuiEvent>,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> Vec<(bool, tool_types::ToolResult, CostTracker)> {
    let mut handles = Vec::new();
    let mut results: Vec<Option<(bool, tool_types::ToolResult, CostTracker)>> =
        vec![None; tool_requests.len()];
    let mut events = EventFactory::new("tui-subagent-batch".to_string());

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        let request = subagent::create_subagent_request(tool_request);
        let description = request.description.clone();
        let subagent_type = request.subagent_type;
        send_subagent_started_for_tui(event_tx, &mut events, &tool_request.id, &description);

        if subagent_depth >= config.subagents.max_depth {
            let error = format!("subagent max depth {} reached", config.subagents.max_depth);
            send_subagent_completed_for_tui(
                event_tx,
                &mut events,
                &tool_request.id,
                &description,
                RunStatus::Failed,
                None,
                Some(&error),
            );
            results[idx] = Some((
                false,
                tool_types::ToolResult::failed(tool_request, error, None),
                CostTracker::new(None),
            ));
            continue;
        }

        let mut child_config = config.clone();
        child_config.model = child_config
            .model
            .with_subagent_override(request.model.clone());
        let child_cwd = cwd.to_path_buf();
        let child_prompt = request.prompt;
        let child_instructions = instructions.clone();
        let child_memory = memory.clone();
        let child_hooks = hooks.clone();
        let child_tool_request = tool_request.clone();
        handles.push((
            idx,
            description,
            thread::spawn(move || {
                let child = run_child_agent_for_tui_silent(
                    &child_config,
                    &child_cwd,
                    &child_prompt,
                    subagent_depth + 1,
                    &subagent_type,
                    &child_instructions,
                    &child_memory,
                    &child_hooks,
                );
                (child_tool_request, child)
            }),
        ));
    }

    for (idx, description, handle) in handles {
        let (tool_request, child) = match handle.join() {
            Ok(result) => result,
            Err(_) => {
                let tool_request = &tool_requests[idx];
                let result =
                    tool_types::ToolResult::failed(tool_request, "subagent thread panicked", None);
                send_subagent_completed_for_tui(
                    event_tx,
                    &mut events,
                    &tool_request.id,
                    &description,
                    RunStatus::Failed,
                    None,
                    result.error.as_deref(),
                );
                results[idx] = Some((false, result, CostTracker::new(None)));
                continue;
            }
        };

        let (should_stop, result, cost_tracker) =
            child_result_to_tui_tool_result(&tool_request, &description, child, event_tx);
        results[idx] = Some((should_stop, result, cost_tracker));
    }

    results
        .into_iter()
        .map(|result| result.expect("each subagent batch item has a result"))
        .collect()
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

pub(crate) fn execute_subagent_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    task_registry: Option<&TaskRegistry>,
) -> (tool_types::ToolResult, CostTracker) {
    let request = subagent::create_subagent_request(tool_request);
    let description = request.description.clone();
    let subagent_type = request.subagent_type.clone();
    let mut events = EventFactory::new("tui-subagent".to_string());

    send_subagent_started_for_tui(event_tx, &mut events, &tool_request.id, &description);

    if subagent_depth >= config.subagents.max_depth {
        let error = format!("subagent max depth {} reached", config.subagents.max_depth);
        send_subagent_completed_for_tui(
            event_tx,
            &mut events,
            &tool_request.id,
            &description,
            RunStatus::Failed,
            None,
            Some(&error),
        );
        return (
            tool_types::ToolResult::failed(tool_request, error, None),
            CostTracker::new(None),
        );
    }

    if request.mode == SubagentMode::Async {
        let Some(task_registry) = task_registry else {
            return (
                tool_types::ToolResult::failed(
                    tool_request,
                    "async subagents require a main TUI session",
                    None,
                ),
                CostTracker::new(None),
            );
        };
        let result = launch_async_subagent_for_tui(
            config,
            cwd,
            tool_request,
            request,
            event_tx,
            subagent_depth,
            instructions,
            memory,
            hooks,
            task_registry,
        );
        return (result, CostTracker::new(None));
    }

    let mut child_config = config.clone();
    child_config.model = child_config
        .model
        .with_subagent_override(request.model.clone());
    let child = run_child_agent_for_tui(
        &child_config,
        cwd,
        &request.prompt,
        event_tx,
        action_rx,
        subagent_depth + 1,
        &subagent_type,
        instructions,
        memory,
        hooks,
    );

    if child.status == "success" {
        let output = child
            .final_message
            .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
        send_subagent_completed_for_tui(
            event_tx,
            &mut events,
            &tool_request.id,
            &description,
            RunStatus::Success,
            Some(&output),
            None,
        );
        (
            tool_types::ToolResult::completed(
                tool_request,
                format!("Subagent status: success\n\n{output}"),
                false,
            ),
            child.cost_tracker,
        )
    } else {
        let error = child
            .error
            .unwrap_or_else(|| format!("subagent ended with status {}", child.status));
        send_subagent_completed_for_tui(
            event_tx,
            &mut events,
            &tool_request.id,
            &description,
            RunStatus::Failed,
            child.final_message.as_deref(),
            Some(&error),
        );
        (
            tool_types::ToolResult::failed(tool_request, error, None),
            child.cost_tracker,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn launch_async_subagent_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tool_types::ToolRequest,
    request: subagent::SubagentRequest,
    event_tx: &Sender<TuiEvent>,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    let agent_type = serde_json::to_value(&request.subagent_type)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string));
    let task = task_registry.create_subagent(request.description.clone(), agent_type);
    let agent_id = task.id.clone();
    let mut child_config = config.clone();
    child_config.model = child_config
        .model
        .with_subagent_override(request.model.clone());
    let child_cwd = cwd.to_path_buf();
    let child_prompt = request.prompt;
    let child_type = request.subagent_type;
    let child_instructions = instructions.clone();
    let child_memory = memory.clone();
    let child_hooks = hooks.clone();
    let child_registry = task_registry.clone();
    let child_event_tx = event_tx.clone();
    let thread_agent_id = agent_id.clone();

    thread::spawn(move || {
        let mut events = EventFactory::new(thread_agent_id.clone());
        let _ = child_registry.mark_running(&thread_agent_id);
        let child = run_child_agent_for_tui_silent(
            &child_config,
            &child_cwd,
            &child_prompt,
            subagent_depth + 1,
            &child_type,
            &child_instructions,
            &child_memory,
            &child_hooks,
        );
        let usage = usage_totals_if_non_empty(child.cost_tracker.totals());
        if child.status == "success" {
            let output = child
                .final_message
                .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
            let _ = child_registry.complete_with_usage(&thread_agent_id, output, usage);
        } else {
            let error = child
                .error
                .or(child.final_message)
                .unwrap_or_else(|| format!("subagent ended with status {}", child.status));
            let _ = child_registry.fail_with_usage(&thread_agent_id, error, usage);
        }
        send_workflow_tasks_updated_for_tui(&child_event_tx, &mut events, &child_registry.list());
    });

    let mut events = EventFactory::new(agent_id.clone());
    send_workflow_tasks_updated_for_tui(event_tx, &mut events, &task_registry.list());
    tool_types::ToolResult::completed(
        tool_request,
        serde_json::json!({
            "status": "async_launched",
            "agent_id": agent_id,
            "description": request.description,
        })
        .to_string(),
        false,
    )
}

pub(crate) fn execute_subagent_status_for_tui(
    tool_request: &tool_types::ToolRequest,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    let agent_id = subagent::extract_subagent_field(tool_request, "agent_id")
        .or_else(|| tool_request.target.clone());
    let Some(agent_id) = agent_id else {
        return tool_types::ToolResult::invalid_input(tool_request, "missing agent_id");
    };
    let Some(record) = task_registry.get(&agent_id) else {
        return tool_types::ToolResult::failed(
            tool_request,
            format!("subagent '{agent_id}' not found"),
            None,
        );
    };
    if record.task_type != orca_core::task_types::TaskType::Subagent {
        return tool_types::ToolResult::failed(
            tool_request,
            format!("task '{agent_id}' is not a subagent"),
            None,
        );
    }
    tool_types::ToolResult::completed(
        tool_request,
        serde_json::json!({
            "agent_id": agent_id,
            "status": record.status,
            "description": record.description,
            "agent_type": record.agent_type,
            "created_at_ms": record.created_at_ms,
            "started_at_ms": record.started_at_ms,
            "completed_at_ms": record.completed_at_ms,
            "output": record.result,
            "error": record.error,
            "usage": record.usage.map(usage_totals_json),
        })
        .to_string(),
        false,
    )
}

fn usage_totals_if_non_empty(usage: UsageTotals) -> Option<UsageTotals> {
    if usage.total_tokens() == 0 && usage.cache_tokens == 0 && usage.estimated_cost_usd == 0.0 {
        None
    } else {
        Some(usage)
    }
}

fn usage_totals_json(usage: UsageTotals) -> serde_json::Value {
    serde_json::json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_tokens": usage.cache_tokens,
        "total_tokens": usage.total_tokens(),
        "estimated_cost_usd": usage.estimated_cost_usd,
    })
}

fn child_result_to_tui_tool_result(
    tool_request: &tool_types::ToolRequest,
    description: &str,
    child: TuiAgentResult,
    event_tx: &Sender<TuiEvent>,
) -> (bool, tool_types::ToolResult, CostTracker) {
    let cost_tracker = child.cost_tracker.clone();
    let mut events = EventFactory::new("tui-subagent-child".to_string());
    if child.status == "success" {
        let output = child
            .final_message
            .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
        send_subagent_completed_for_tui(
            event_tx,
            &mut events,
            &tool_request.id,
            description,
            RunStatus::Success,
            Some(&output),
            None,
        );
        (
            false,
            tool_types::ToolResult::completed(
                tool_request,
                format!("Subagent status: success\n\n{output}"),
                false,
            ),
            cost_tracker,
        )
    } else {
        let error = child
            .error
            .unwrap_or_else(|| format!("subagent ended with status {}", child.status));
        send_subagent_completed_for_tui(
            event_tx,
            &mut events,
            &tool_request.id,
            description,
            RunStatus::Failed,
            child.final_message.as_deref(),
            Some(&error),
        );
        (
            false,
            tool_types::ToolResult::failed(tool_request, error, None),
            cost_tracker,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn run_child_agent_for_tui(
    config: &RunConfig,
    cwd: &Path,
    prompt: &str,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> TuiAgentResult {
    let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(orca_core::model::FLASH_MODEL.to_string()),
        reasoning_effort: config.reasoning_effort,
        tools_override: Some(deepseek_tools_schema_for_type_with_mcp_and_external(
            subagent_type,
            Some(&mcp_registry),
            &config.external_tools,
        )),
        mcp_registry: Some(mcp_registry.clone()),
        external_tools: config.external_tools.clone(),
    };

    let budget_model = config.model.as_option();
    let ctx_config = orca_provider::context::ContextConfig::for_model_with_runtime(
        budget_model.as_deref(),
        &config.model_runtime,
    );
    let mut conversation = Conversation::new();
    conversation.add_system(agent_common::build_agent_system_prompt(
        cwd,
        subagent_depth,
        subagent_type,
        Some(instructions),
        config.approval_mode,
        Some(memory),
    ));
    conversation.add_user(prompt.to_string());

    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());
    let mut child_cost_tracker = CostTracker::new(None);
    let mut turn: u32 = 0;
    let mut reactive_compacted = false;
    loop {
        turn += 1;
        if turn > DEFAULT_MAX_TURNS {
            return TuiAgentResult {
                status: "budget_exhausted".to_string(),
                final_message: None,
                error: Some("max turns exhausted".to_string()),
                cost_tracker: child_cost_tracker,
            };
        }

        if orca_provider::context::needs_compaction_wire(
            &conversation,
            &ctx_config,
            &provider_config,
        ) {
            let before_messages = conversation.messages.len();
            if let Ok(outcome) = hooks.run(
                HookEvent::OnBudgetWarning,
                HookContext {
                    cwd: &cwd.display().to_string(),
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: Some(before_messages),
                    after_messages: None,
                    usage: None,
                },
            ) {
                if !outcome.injected_context.is_empty() {
                    conversation = conversation_with_hook_context(&conversation, &outcome);
                }
            }
            conversation = orca_provider::context::compact(&conversation, &ctx_config);
        }

        let child_cancel = CancelToken::new();
        let route_decision = config.model.route(ModelRouteContext {
            subagent_type,
            subagent_model: None,
        });
        child_cost_tracker.set_model(Some(&route_decision.actual_model));
        let mut turn_provider_config = provider_config.clone();
        turn_provider_config.model = Some(route_decision.actual_model.clone());

        let pre_model_outcome = match hooks.run(
            HookEvent::PreModelCall,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: None,
            },
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                return TuiAgentResult {
                    status: "failed".to_string(),
                    final_message: None,
                    error: Some(format!("pre_model_call hook failed: {error}")),
                    cost_tracker: child_cost_tracker,
                };
            }
        };
        let model_conversation = conversation_with_hook_context(&conversation, &pre_model_outcome);

        let response = orca_provider::call_streaming(
            config.provider,
            &model_conversation,
            &turn_provider_config,
            &child_cancel,
            &mut |_| {},
        );

        if let Err(error) = hooks.run(
            HookEvent::PostModelCall,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: response.usage.as_ref(),
            },
        ) {
            return TuiAgentResult {
                status: "failed".to_string(),
                final_message: None,
                error: Some(format!("post_model_call hook failed: {error}")),
                cost_tracker: child_cost_tracker,
            };
        }

        if let Some(error) = response.steps.iter().find_map(|step| match step {
            ProviderStep::Error(message) => Some(message.clone()),
            _ => None,
        }) {
            if orca_provider::context::is_prompt_too_long_error(&error) && !reactive_compacted {
                conversation = orca_provider::context::compact(&conversation, &ctx_config);
                reactive_compacted = true;
                continue;
            }
            return TuiAgentResult {
                status: "failed".to_string(),
                final_message: None,
                error: Some(error),
                cost_tracker: child_cost_tracker,
            };
        }

        reactive_compacted = false;

        if let Some(usage) = response.usage
            && !usage.is_empty()
        {
            child_cost_tracker.add_usage(usage);
        }

        if response.tool_calls.is_empty() {
            conversation.add_assistant(
                response.assistant_content.clone(),
                response.assistant_reasoning,
                vec![],
            );
            return TuiAgentResult {
                status: "success".to_string(),
                final_message: response.assistant_content,
                error: None,
                cost_tracker: child_cost_tracker,
            };
        }

        conversation.add_assistant(
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
        );

        for step in &response.steps {
            if let ProviderStep::ToolCall(tool_request) = step {
                let (should_stop, result, child_cost) = execute_tool_for_tui(
                    config,
                    cwd,
                    tool_request,
                    event_tx,
                    action_rx,
                    subagent_depth,
                    None,
                    &policy,
                    instructions,
                    memory,
                    &mcp_registry,
                    hooks,
                    None,
                    &child_cancel,
                );

                if let Some(c) = child_cost {
                    child_cost_tracker.merge(&c);
                }

                let result_content = agent_common::format_tool_result_for_model(&result);
                conversation.add_tool_result(tool_request.id.clone(), result_content);

                if should_stop {
                    return TuiAgentResult {
                        status: "failed".to_string(),
                        final_message: None,
                        error: result.error,
                        cost_tracker: child_cost_tracker,
                    };
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_child_agent_for_tui_silent(
    config: &RunConfig,
    cwd: &Path,
    prompt: &str,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> TuiAgentResult {
    let (event_tx, _event_rx) = std::sync::mpsc::channel();
    let (action_tx, action_rx) = std::sync::mpsc::channel();
    drop(action_tx);
    run_child_agent_for_tui(
        config,
        cwd,
        prompt,
        &event_tx,
        &action_rx,
        subagent_depth,
        subagent_type,
        instructions,
        memory,
        hooks,
    )
}
