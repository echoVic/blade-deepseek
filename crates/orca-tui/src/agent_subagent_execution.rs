use crossbeam_channel::{Receiver, Sender};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::thread;

use orca_core::config::RunConfig;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types;
use orca_runtime::agent_child::{
    ChildAgentActivity, ChildAgentActivityObserver, ChildAgentPromptContext, ChildAgentResult,
    ChildAgentToolExecution, run_child_agent_prompt_with_tool_executor_observed,
};
use orca_runtime::cost::CostTracker;
use orca_runtime::hooks::HookRunner;
use orca_runtime::instructions::ProjectInstructions;
use orca_runtime::memory::MemoryBlock;
use orca_runtime::runtime_pending_interaction::RuntimePendingInteractionStore;
use orca_runtime::subagent::{self, SubagentMode};
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::tool_invocation::{prepare_tool_invocation, validate_tool_invocation};

use crate::agent_runner::{
    send_subagent_completed_for_tui, send_subagent_started_for_tui,
    send_task_status_updated_for_tui, send_tool_completed_for_tui, send_tool_requested_for_tui,
    task_summary_for_tui,
};
use crate::agent_tool_execution::execute_tool_for_tui;
use crate::types::{TuiEvent, UserAction};

pub(crate) fn config_for_remaining_subagent_budget(
    config: &RunConfig,
    parent_usage: UsageTotals,
) -> RunConfig {
    let mut child_config = config.clone();
    if let Some(max_budget) = config.max_budget_usd {
        child_config.max_budget_usd = Some((max_budget - parent_usage.estimated_cost_usd).max(0.0));
    }
    child_config
}

fn send_subagent_task_status_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    registry: &TaskRegistry,
    task_id: &str,
) {
    if let Some(task) = task_summary_for_tui(registry, task_id) {
        send_task_status_updated_for_tui(event_tx, events, &task);
    }
}

pub(crate) fn should_run_subagent_batch(
    config: &RunConfig,
    tool_request: &tool_types::ToolRequest,
    subagent_depth: u32,
) -> bool {
    tool_request.name == tool_types::ToolName::Subagent
        && subagent_depth < config.subagents.max_depth
        && config.subagents.max_parallel > 1
        && config.max_budget_usd.is_none()
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
    mcp_registry: &orca_mcp::McpRegistry,
    hooks: &HookRunner,
    task_registry: Option<&TaskRegistry>,
) -> Vec<(bool, tool_types::ToolResult, CostTracker)> {
    let mut handles = Vec::new();
    let mut results: Vec<Option<(bool, tool_types::ToolResult, CostTracker)>> =
        vec![None; tool_requests.len()];
    let mut events = EventFactory::new("tui-subagent-batch".to_string());

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        let invocation =
            prepare_tool_invocation(tool_request, subagent_depth, mcp_registry, config);
        if tool_request.raw_arguments.is_some()
            && let Err(error) = validate_tool_invocation(&invocation, mcp_registry, config)
        {
            send_tool_requested_for_tui(event_tx, &mut events, tool_request);
            let result = error.into_result();
            send_tool_completed_for_tui(event_tx, &mut events, &result, None);
            results[idx] = Some((false, result, CostTracker::new(None)));
            continue;
        }
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

        let registry_task_id = task_registry.map(|registry| {
            let agent_type = serde_json::to_value(&subagent_type)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string));
            let task = registry.create_subagent(description.clone(), agent_type);
            let _ = registry.mark_running(&task.id);
            let mut task_events = EventFactory::new(task.id.clone());
            send_subagent_task_status_for_tui(event_tx, &mut task_events, registry, &task.id);
            task.id
        });
        let child_config = config.clone();
        let child_cwd = cwd.to_path_buf();
        let child_prompt = request.prompt;
        let child_model = request.model;
        let child_instructions = instructions.clone();
        let child_memory = memory.clone();
        let child_hooks = hooks.clone();
        let child_tool_request = tool_request.clone();
        let child_event_tx = event_tx.clone();
        let child_registry = task_registry.cloned();
        let child_registry_task_id = registry_task_id.clone();
        handles.push((
            idx,
            description,
            registry_task_id,
            thread::spawn(move || {
                let observer = make_subagent_progress_observer(
                    child_tool_request.id.clone(),
                    child_registry,
                    child_registry_task_id,
                    child_event_tx,
                );
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
                    Some(&observer),
                );
                (child_tool_request, child, child_cost_tracker)
            }),
        ));
    }

    for (idx, description, registry_task_id, handle) in handles {
        let (tool_request, child, child_cost_tracker) = match handle.join() {
            Ok(result) => result,
            Err(_) => {
                let tool_request = &tool_requests[idx];
                let result =
                    tool_types::ToolResult::failed(tool_request, "subagent thread panicked", None);
                if let (Some(registry), Some(task_id)) =
                    (task_registry, registry_task_id.as_deref())
                {
                    let _ = registry.fail_with_usage(
                        task_id,
                        "subagent thread panicked".to_string(),
                        None,
                    );
                    let mut task_events = EventFactory::new(task_id.to_string());
                    send_subagent_task_status_for_tui(
                        event_tx,
                        &mut task_events,
                        registry,
                        task_id,
                    );
                }
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

        if let (Some(registry), Some(task_id)) = (task_registry, registry_task_id.as_deref()) {
            let usage = usage_totals_if_non_empty(child_cost_tracker.totals());
            if child.status == RunStatus::Success {
                let output = child
                    .final_message
                    .clone()
                    .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
                let _ = registry.complete_with_usage(task_id, output, usage);
            } else {
                let error = child
                    .error
                    .clone()
                    .or_else(|| child.final_message.clone())
                    .unwrap_or_else(|| format!("subagent ended with status {:?}", child.status));
                let _ = registry.fail_with_usage(task_id, error, usage);
            }
            let mut task_events = EventFactory::new(task_id.to_string());
            send_subagent_task_status_for_tui(event_tx, &mut task_events, registry, task_id);
        }

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
    pending_actions: &RefCell<VecDeque<UserAction>>,
    pending_interactions: Option<RuntimePendingInteractionStore>,
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
        if config.max_budget_usd.is_some() {
            let error = "async subagents are unavailable while max_budget_usd is active; use sync mode so usage can be admitted and reconciled in the parent turn";
            send_subagent_completed_for_tui(
                event_tx,
                &mut events,
                &tool_request.id,
                &description,
                RunStatus::Failed,
                None,
                Some(error),
            );
            return (
                tool_types::ToolResult::failed(tool_request, error, None),
                CostTracker::new(None),
            );
        }
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
        pending_actions,
        pending_interactions,
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
            child.status,
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
    let child_chat_id = tool_request.id.clone();
    let child_description = request.description.clone();

    thread::spawn(move || {
        let mut events = EventFactory::new(thread_agent_id.clone());
        let _ = child_registry.mark_running(&thread_agent_id);
        let observer = make_subagent_progress_observer(
            child_chat_id.clone(),
            Some(child_registry.clone()),
            Some(thread_agent_id.clone()),
            child_event_tx.clone(),
        );
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
            Some(&observer),
        );
        let usage = usage_totals_if_non_empty(child_cost_tracker.totals());
        if child.status == RunStatus::Success {
            let output = child
                .final_message
                .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
            send_subagent_completed_for_tui(
                &child_event_tx,
                &mut events,
                &child_chat_id,
                &child_description,
                RunStatus::Success,
                Some(&output),
                None,
            );
            let _ = child_registry.complete_with_usage(&thread_agent_id, output, usage);
        } else {
            let error = child
                .error
                .or_else(|| child.final_message.clone())
                .unwrap_or_else(|| format!("subagent ended with status {:?}", child.status));
            send_subagent_completed_for_tui(
                &child_event_tx,
                &mut events,
                &child_chat_id,
                &child_description,
                RunStatus::Failed,
                child.final_message.as_deref(),
                Some(&error),
            );
            let _ = child_registry.fail_with_usage(&thread_agent_id, error, usage);
        }
        send_subagent_task_status_for_tui(
            &child_event_tx,
            &mut events,
            &child_registry,
            &thread_agent_id,
        );
    });

    let mut events = EventFactory::new(agent_id.clone());
    send_subagent_task_status_for_tui(event_tx, &mut events, task_registry, &agent_id);
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
            "current_activity": record.subagent_current_activity,
            "turn": record.subagent_turn,
            "last_activity_at_ms": record.last_activity_at_ms,
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
            child.status,
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
    pending_actions: &RefCell<VecDeque<UserAction>>,
    pending_interactions: Option<RuntimePendingInteractionStore>,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> (ChildAgentResult, CostTracker) {
    run_child_agent_for_tui_observed(
        config,
        cwd,
        prompt,
        subagent_model,
        event_tx,
        action_rx,
        pending_actions,
        pending_interactions,
        subagent_depth,
        subagent_type,
        instructions,
        memory,
        hooks,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_child_agent_for_tui_observed(
    config: &RunConfig,
    cwd: &Path,
    prompt: &str,
    subagent_model: Option<String>,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    pending_interactions: Option<RuntimePendingInteractionStore>,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    observer: Option<&ChildAgentActivityObserver<'_>>,
) -> (ChildAgentResult, CostTracker) {
    let current_child_usage = std::cell::Cell::new(UsageTotals::default());
    let current_child_usage_ref = &current_child_usage;
    let current_child_usage_for_tools = &current_child_usage;
    let tracking_observer = ChildAgentActivityObserver::new(move |activity| {
        if let ChildAgentActivity::Usage(usage) = activity {
            current_child_usage_ref.set(*usage);
        }
        if let Some(observer) = observer {
            observer.emit(activity.clone());
        }
    });
    run_child_agent_prompt_with_tool_executor_observed(
        config,
        ChildAgentPromptContext {
            prompt: prompt.to_string(),
            subagent_type,
            subagent_model,
            subagent_depth,
            cwd,
            instructions,
            memory,
            hooks,
        },
        Some(&tracking_observer),
        {
            // Permission grants persist across the child agent's tool calls,
            // mirroring the per-turn overlay in the main TUI loop.
            let permission_overlay =
                std::cell::RefCell::new(orca_runtime::lifecycle::TurnPermissionOverlay::default());
            move |config, request, tool_context, child_cancel, tool_request| {
                let budget_config =
                    (tool_request.name == tool_types::ToolName::Subagent).then(|| {
                        config_for_remaining_subagent_budget(
                            config,
                            current_child_usage_for_tools.get(),
                        )
                    });
                let tool_config = budget_config.as_ref().unwrap_or(config);
                let (should_stop, result, child_cost) = execute_tool_for_tui(
                    tool_config,
                    cwd,
                    tool_request,
                    event_tx,
                    action_rx,
                    pending_actions,
                    pending_interactions.clone(),
                    request.depth,
                    None,
                    None,
                    tool_context.policy,
                    instructions,
                    memory,
                    tool_context.mcp_registry,
                    hooks,
                    None,
                    &mut permission_overlay.borrow_mut(),
                    child_cancel,
                );
                ChildAgentToolExecution {
                    should_stop,
                    result,
                    child_cost,
                }
            }
        },
    )
}

fn subagent_activity_to_tui_progress(
    id: &str,
    activity: &ChildAgentActivity,
    turn: Option<u32>,
) -> TuiEvent {
    let (activity, usage) = match activity {
        ChildAgentActivity::TurnStarted { turn } => (format!("turn {turn} started"), None),
        ChildAgentActivity::ToolStarted { name, target } => {
            let target = target
                .as_deref()
                .map(|target| format!(": {target}"))
                .unwrap_or_default();
            (format!("{name}{target}"), None)
        }
        ChildAgentActivity::ToolCompleted { name, status } => {
            (format!("{name} {}", status.as_str()), None)
        }
        ChildAgentActivity::Streaming => ("streaming response".to_string(), None),
        ChildAgentActivity::Usage(usage) => ("usage updated".to_string(), Some(*usage)),
    };
    TuiEvent::SubagentProgress {
        id: id.to_string(),
        activity,
        turn,
        usage,
    }
}

/// Shared progress wiring for every child-agent path: `chat_id` is the tool
/// call id the conversation's subagent card is keyed by, while the registry
/// task keeps its own id — the two are never the same value.
fn make_subagent_progress_observer(
    chat_id: String,
    registry: Option<TaskRegistry>,
    registry_task_id: Option<String>,
    event_tx: Sender<TuiEvent>,
) -> ChildAgentActivityObserver<'static> {
    let mut current_turn = None;
    ChildAgentActivityObserver::new(move |activity| {
        if let ChildAgentActivity::TurnStarted { turn } = activity {
            current_turn = Some(*turn);
        }
        let progress = subagent_activity_to_tui_progress(&chat_id, activity, current_turn);
        if let (Some(registry), Some(task_id)) = (&registry, &registry_task_id) {
            update_registry_from_subagent_progress(registry, task_id, &progress);
            let mut progress_events = EventFactory::new(task_id.clone());
            send_subagent_task_status_for_tui(&event_tx, &mut progress_events, registry, task_id);
        }
        let _ = event_tx.send(progress);
    })
}

fn update_registry_from_subagent_progress(
    registry: &TaskRegistry,
    task_id: &str,
    progress: &TuiEvent,
) {
    if let TuiEvent::SubagentProgress {
        activity,
        turn,
        usage,
        ..
    } = progress
    {
        let current_activity = registry
            .get(task_id)
            .and_then(|record| record.subagent_current_activity);
        let registry_activity = if usage.is_some()
            || (current_activity
                .as_deref()
                .is_some_and(|current| current.contains(": "))
                && !activity.contains(": ")
                && is_less_specific_subagent_activity(activity))
        {
            current_activity.unwrap_or_else(|| activity.clone())
        } else {
            activity.clone()
        };
        let _ = registry.update_subagent_activity(task_id, registry_activity, *turn, *usage);
    }
}

fn is_less_specific_subagent_activity(activity: &str) -> bool {
    activity.starts_with("turn ")
        || activity == "streaming response"
        || is_tool_completion_activity(activity)
}

fn is_tool_completion_activity(activity: &str) -> bool {
    activity.ends_with(" success")
        || activity.ends_with(" failed")
        || activity.ends_with(" approval_required")
}

/// Runs a child agent with no live event/action wiring to the main
/// conversation; progress (if any) flows only through `observer`.
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
    observer: Option<&ChildAgentActivityObserver<'_>>,
) -> (ChildAgentResult, CostTracker) {
    let (event_tx, event_rx) = crate::channels::tui_event_channel();
    let event_drain = thread::spawn(move || while event_rx.recv().is_ok() {});
    let (action_tx, action_rx) = crate::channels::user_action_channel();
    let pending_actions = RefCell::new(VecDeque::new());
    drop(action_tx);
    let result = run_child_agent_for_tui_observed(
        config,
        cwd,
        prompt,
        subagent_model,
        &event_tx,
        &action_rx,
        &pending_actions,
        None,
        subagent_depth,
        subagent_type,
        instructions,
        memory,
        hooks,
        observer,
    );
    drop(event_tx);
    let _ = event_drain.join();
    result
}
