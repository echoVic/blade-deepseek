use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::hook_types::HookEvent;
use orca_core::provider_types::ProviderStep;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types;
use orca_runtime::agent_child::{
    ChildAgentRequest, ChildAgentResult, compact_child_agent_conversation_if_needed,
    prepare_child_agent_loop, route_child_agent_model, run_child_agent_with_executor,
};
use orca_runtime::agent_common;
use orca_runtime::cost::CostTracker;
use orca_runtime::hooks::{HookContext, HookRunner, conversation_with_hook_context};
use orca_runtime::instructions::ProjectInstructions;
use orca_runtime::memory::MemoryBlock;
use orca_runtime::subagent::{self, SubagentMode};
use orca_runtime::tasks::TaskRegistry;

use crate::agent_runner::{
    DEFAULT_MAX_TURNS, send_subagent_completed_for_tui, send_subagent_started_for_tui,
    send_workflow_tasks_updated_for_tui,
};
use crate::agent_tool_execution::execute_tool_for_tui;
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

        let child_config = config.clone();
        let child_cwd = cwd.to_path_buf();
        let child_prompt = request.prompt;
        let child_model = request.model;
        let child_instructions = instructions.clone();
        let child_memory = memory.clone();
        let child_hooks = hooks.clone();
        let child_tool_request = tool_request.clone();
        handles.push((
            idx,
            description,
            thread::spawn(move || {
                let (child, child_cost_tracker) = run_child_agent_for_tui_silent(
                    &child_config,
                    &child_cwd,
                    &child_prompt,
                    child_model,
                    subagent_depth + 1,
                    &subagent_type,
                    &child_instructions,
                    &child_memory,
                    &child_hooks,
                );
                (child_tool_request, child, child_cost_tracker)
            }),
        ));
    }

    for (idx, description, handle) in handles {
        let (tool_request, child, child_cost_tracker) = match handle.join() {
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

        let (should_stop, result, cost_tracker) = child_result_to_tui_tool_result(
            &tool_request,
            &description,
            child,
            child_cost_tracker,
            event_tx,
        );
        results[idx] = Some((should_stop, result, cost_tracker));
    }

    results
        .into_iter()
        .map(|result| result.expect("each subagent batch item has a result"))
        .collect()
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

    let (child, child_cost_tracker) = run_child_agent_for_tui(
        config,
        cwd,
        &request.prompt,
        request.model.clone(),
        event_tx,
        action_rx,
        subagent_depth + 1,
        &subagent_type,
        instructions,
        memory,
        hooks,
    );

    if child.status == RunStatus::Success {
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
            child_cost_tracker,
        )
    } else {
        let error = child
            .error
            .unwrap_or_else(|| format!("subagent ended with status {:?}", child.status));
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
            child_cost_tracker,
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
    let child_config = config.clone();
    let child_cwd = cwd.to_path_buf();
    let child_prompt = request.prompt;
    let child_model = request.model;
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
        let (child, child_cost_tracker) = run_child_agent_for_tui_silent(
            &child_config,
            &child_cwd,
            &child_prompt,
            child_model,
            subagent_depth + 1,
            &child_type,
            &child_instructions,
            &child_memory,
            &child_hooks,
        );
        let usage = usage_totals_if_non_empty(child_cost_tracker.totals());
        if child.status == RunStatus::Success {
            let output = child
                .final_message
                .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
            let _ = child_registry.complete_with_usage(&thread_agent_id, output, usage);
        } else {
            let error = child
                .error
                .or(child.final_message)
                .unwrap_or_else(|| format!("subagent ended with status {:?}", child.status));
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
    child: ChildAgentResult,
    cost_tracker: CostTracker,
    event_tx: &Sender<TuiEvent>,
) -> (bool, tool_types::ToolResult, CostTracker) {
    let mut events = EventFactory::new("tui-subagent-child".to_string());
    if child.status == RunStatus::Success {
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
            .unwrap_or_else(|| format!("subagent ended with status {:?}", child.status));
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
    subagent_model: Option<String>,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> (ChildAgentResult, CostTracker) {
    let child_request = ChildAgentRequest::new(
        prompt.to_string(),
        subagent_type.clone(),
        subagent_model,
        subagent_depth,
        false,
    );
    run_child_agent_with_executor(
        config,
        &child_request,
        |config, request, child_cost_tracker| {
            let mut setup = prepare_child_agent_loop(config, request, cwd, instructions, memory);
            let mut turn: u32 = 0;
            let mut reactive_compacted = false;
            loop {
                turn += 1;
                if turn > DEFAULT_MAX_TURNS {
                    return Ok(ChildAgentResult {
                        status: RunStatus::BudgetExhausted,
                        final_message: None,
                        error: Some("max turns exhausted".to_string()),
                    });
                }

                compact_child_agent_conversation_if_needed(config, &mut setup, cwd, hooks)?;

                let child_cancel = CancelToken::new();
                let turn_provider_config =
                    route_child_agent_model(config, request, &setup, child_cost_tracker);

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
                        return Ok(ChildAgentResult {
                            status: RunStatus::Failed,
                            final_message: None,
                            error: Some(format!("pre_model_call hook failed: {error}")),
                        });
                    }
                };
                let model_conversation =
                    conversation_with_hook_context(&setup.conversation, &pre_model_outcome);

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
                    return Ok(ChildAgentResult {
                        status: RunStatus::Failed,
                        final_message: None,
                        error: Some(format!("post_model_call hook failed: {error}")),
                    });
                }

                if let Some(error) = response.steps.iter().find_map(|step| match step {
                    ProviderStep::Error(message) => Some(message.clone()),
                    _ => None,
                }) {
                    if orca_provider::context::is_prompt_too_long_error(&error)
                        && !reactive_compacted
                    {
                        setup.conversation = orca_provider::context::compact(
                            &setup.conversation,
                            &setup.context_config,
                        );
                        reactive_compacted = true;
                        continue;
                    }
                    return Ok(ChildAgentResult {
                        status: RunStatus::Failed,
                        final_message: None,
                        error: Some(error),
                    });
                }

                reactive_compacted = false;

                if let Some(usage) = response.usage
                    && !usage.is_empty()
                {
                    child_cost_tracker.add_usage(usage);
                }

                if response.tool_calls.is_empty() {
                    setup.conversation.add_assistant(
                        response.assistant_content.clone(),
                        response.assistant_reasoning,
                        vec![],
                    );
                    return Ok(ChildAgentResult {
                        status: RunStatus::Success,
                        final_message: response.assistant_content,
                        error: None,
                    });
                }

                setup.conversation.add_assistant(
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
                            request.depth,
                            None,
                            &setup.policy,
                            instructions,
                            memory,
                            &setup.mcp_registry,
                            hooks,
                            None,
                            &child_cancel,
                        );

                        if let Some(c) = child_cost {
                            child_cost_tracker.merge(&c);
                        }

                        let result_content = agent_common::format_tool_result_for_model(&result);
                        setup
                            .conversation
                            .add_tool_result(tool_request.id.clone(), result_content);

                        if should_stop {
                            return Ok(ChildAgentResult {
                                status: RunStatus::Failed,
                                final_message: None,
                                error: result.error,
                            });
                        }
                    }
                }
            }
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_child_agent_for_tui_silent(
    config: &RunConfig,
    cwd: &Path,
    prompt: &str,
    subagent_model: Option<String>,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> (ChildAgentResult, CostTracker) {
    let (event_tx, _event_rx) = std::sync::mpsc::channel();
    let (action_tx, action_rx) = std::sync::mpsc::channel();
    drop(action_tx);
    run_child_agent_for_tui(
        config,
        cwd,
        prompt,
        subagent_model,
        &event_tx,
        &action_rx,
        subagent_depth,
        subagent_type,
        instructions,
        memory,
        hooks,
    )
}
