use std::io;
use std::path::Path;
use std::thread;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventEnvelope, EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::hook_types::HookEvent;
use orca_core::tool_types;
use orca_mcp::McpRegistry;
use serde_json::Value;

use crate::agent_child::{
    ChildAgentExecutor, ChildAgentRequest, ChildAgentResult, ChildAgentRuntime,
    ChildAgentRuntimeContext, run_child_agent,
};
use crate::cost::CostTracker;
use crate::hooks::{HookContext, HookRunError, HookRunner};
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTaskKind, RuntimeTaskStatus};
use crate::memory::MemoryBlock;
use crate::schema_validation::validate_json_schema_subset;
use crate::session::record_tool_result_for_agent;
use crate::subagent::{self, SubagentIsolation, SubagentMode};
use crate::subagent_async_worker::{AsyncSubagentLaunchContext, launch_async_subagent};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_invocation::{
    apply_pre_tool_outcome_with_external, prepare_tool_invocation_with_external,
    validate_tool_invocation_with_external,
};
use crate::tool_turn::ToolTurnOutcome;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::worktree::{WorktreeGuard, WorktreeOutcome};

pub(crate) enum SubagentBatchRecordOutcome {
    Continue,
    Return {
        status: RunStatus,
        error: Option<String>,
    },
}

pub(crate) struct RuntimeSubagentBatchToolTurnContext<'a, W: io::Write> {
    pub(crate) request: RuntimeSubagentBatchToolTurnRequest<'a>,
    pub(crate) io: RuntimeSubagentBatchToolTurnIo<'a, W>,
    pub(crate) services: RuntimeSubagentBatchToolTurnServices<'a>,
    pub(crate) runtime: RuntimeSubagentBatchToolTurnRuntime<'a>,
    pub(crate) child_executor: ChildAgentExecutor<io::Sink>,
}

pub(crate) struct RuntimeSubagentBatchToolTurnRequest<'a> {
    pub(crate) config: &'a RunConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) tool_requests: &'a [tool_types::ToolRequest],
    pub(crate) subagent_depth: u32,
    pub(crate) emit_deltas: bool,
}

pub(crate) struct RuntimeSubagentBatchToolTurnIo<'a, W: io::Write> {
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
    pub(crate) cost_tracker: &'a mut CostTracker,
}

pub(crate) struct RuntimeSubagentBatchToolTurnServices<'a> {
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
}

pub(crate) struct RuntimeSubagentBatchToolTurnRuntime<'a> {
    pub(crate) cancel: &'a CancelToken,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
}

#[derive(Clone, Debug)]
struct SubagentExecutionResult {
    tool_request: tool_types::ToolRequest,
    description: String,
    task: crate::lifecycle::RuntimeTaskLifecycle,
    schema: Option<Value>,
    child: ChildAgentResult,
    cost_tracker: CostTracker,
    worktree: Option<WorktreeOutcome>,
}

struct SubagentBatchExecution {
    results: Vec<(RunStatus, tool_types::ToolResult)>,
    event_error: Option<io::Error>,
}

fn emit_batch_event<W: io::Write>(
    sink: &mut EventSink<W>,
    event: &EventEnvelope,
    event_error: &mut Option<io::Error>,
) -> bool {
    match sink.emit(event) {
        Ok(()) => true,
        Err(error) => {
            if event_error.is_none() {
                *event_error = Some(error);
            }
            false
        }
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
        && is_batchable_subagent_request(tool_request)
}

pub(crate) fn collect_subagent_batch(
    config: &RunConfig,
    tool_requests: &[tool_types::ToolRequest],
    start: usize,
) -> usize {
    let max_end = (start + config.subagents.max_parallel).min(tool_requests.len());
    let mut end = start;
    while end < max_end && is_batchable_subagent_request(&tool_requests[end]) {
        end += 1;
    }
    end
}

pub(crate) fn record_subagent_batch_results(
    conversation: &mut Conversation,
    mut history_writer: Option<&mut SessionWriter>,
    results: Vec<(RunStatus, tool_types::ToolResult)>,
    emit_deltas: bool,
) -> io::Result<SubagentBatchRecordOutcome> {
    let mut terminal = None;
    let mut record_error = None;
    for (status, result) in results {
        if let Err(error) = record_tool_result_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            &result,
            emit_deltas,
        ) && record_error.is_none()
        {
            record_error = Some(error);
        }

        if terminal.is_none()
            && matches!(
                status,
                RunStatus::ApprovalRequired | RunStatus::Failed | RunStatus::Cancelled
            )
        {
            terminal = Some((status, result.error.clone()));
        }
    }

    if let Some(error) = record_error {
        return Err(error);
    }

    Ok(match terminal {
        Some((status, error)) => SubagentBatchRecordOutcome::Return { status, error },
        None => SubagentBatchRecordOutcome::Continue,
    })
}

pub(crate) fn run_subagent_batch_tool_turn<W: io::Write>(
    context: RuntimeSubagentBatchToolTurnContext<'_, W>,
) -> io::Result<ToolTurnOutcome> {
    let RuntimeSubagentBatchToolTurnContext {
        request,
        io,
        services,
        runtime,
        child_executor,
    } = context;
    let RuntimeSubagentBatchToolTurnRequest {
        config,
        cwd,
        tool_requests,
        subagent_depth,
        emit_deltas,
    } = request;
    let RuntimeSubagentBatchToolTurnIo {
        events,
        sink,
        conversation,
        history_writer,
        cost_tracker,
    } = io;
    let RuntimeSubagentBatchToolTurnServices {
        instructions,
        memory,
        mcp_registry,
        hooks,
    } = services;
    let RuntimeSubagentBatchToolTurnRuntime {
        cancel,
        workflow_ipc,
    } = runtime;
    let execution = execute_subagent_batch(
        config,
        cwd,
        events,
        sink,
        tool_requests,
        subagent_depth,
        emit_deltas,
        instructions,
        memory,
        mcp_registry,
        hooks,
        cost_tracker,
        cancel,
        workflow_ipc,
        child_executor,
    );

    let record_outcome = record_subagent_batch_results(
        conversation,
        history_writer,
        execution.results,
        emit_deltas,
    )?;
    if let Some(error) = execution.event_error {
        return Err(error);
    }

    match record_outcome {
        SubagentBatchRecordOutcome::Continue => Ok(ToolTurnOutcome::Continue),
        SubagentBatchRecordOutcome::Return { status, error } => {
            Ok(ToolTurnOutcome::Return { status, error })
        }
    }
}

fn is_batchable_subagent_request(tool_request: &tool_types::ToolRequest) -> bool {
    if tool_request.name != tool_types::ToolName::Subagent {
        return false;
    }
    let request = subagent::create_subagent_request(tool_request);
    request.mode == SubagentMode::Sync && request.isolation == SubagentIsolation::None
}

#[allow(clippy::too_many_arguments)]
fn execute_subagent_batch(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_requests: &[tool_types::ToolRequest],
    subagent_depth: u32,
    emit_deltas: bool,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
    workflow_ipc: Option<&WorkflowIpcContext>,
    child_executor: ChildAgentExecutor<io::Sink>,
) -> SubagentBatchExecution {
    let mut results: Vec<Option<(RunStatus, tool_types::ToolResult)>> =
        vec![None; tool_requests.len()];
    let mut event_error = None;

    thread::scope(|scope| {
        let mut handles = Vec::new();

        for (idx, tool_request) in tool_requests.iter().enumerate() {
            if event_error.is_some() {
                let result = tool_types::ToolResult::failed_before_start(
                    tool_request,
                    "subagent dispatch stopped after event delivery failed",
                    None,
                );
                if emit_deltas {
                    emit_batch_event(
                        sink,
                        &events.tool_call_requested(tool_request),
                        &mut event_error,
                    );
                    emit_batch_event(sink, &events.tool_call_completed(&result), &mut event_error);
                }
                results[idx] = Some((RunStatus::Failed, result));
                continue;
            }
            if emit_deltas {
                let requested = events.tool_call_requested(tool_request);
                if !emit_batch_event(sink, &requested, &mut event_error) {
                    let result = tool_types::ToolResult::failed_before_start(
                        tool_request,
                        "subagent dispatch stopped because the requested event could not be delivered",
                        None,
                    );
                    emit_batch_event(sink, &events.tool_call_completed(&result), &mut event_error);
                    results[idx] = Some((RunStatus::Failed, result));
                    continue;
                }
            }
            if cancel.is_cancelled() {
                let result = tool_types::ToolResult::cancelled_before_start(
                    tool_request,
                    "the subagent batch was cancelled before dispatch",
                );
                if emit_deltas {
                    emit_batch_event(sink, &events.tool_call_completed(&result), &mut event_error);
                }
                results[idx] = Some((RunStatus::Cancelled, result));
                continue;
            }

            let invocation = prepare_tool_invocation_with_external(
                tool_request,
                subagent_depth,
                config.subagents.max_depth,
                mcp_registry,
                &[],
            );
            if let Err(error) =
                validate_tool_invocation_with_external(&invocation, mcp_registry, &[])
            {
                let result = error.into_result();
                if emit_deltas {
                    emit_batch_event(sink, &events.tool_call_completed(&result), &mut event_error);
                }
                results[idx] = Some((RunStatus::Failed, result));
                continue;
            }

            let pre_tool_outcome = match hooks.run_with_cancel_result(
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
                cancel,
            ) {
                Ok(outcome) => outcome,
                Err(error) => {
                    let (status, result) = match error {
                        HookRunError::Cancelled(_) => (
                            RunStatus::Cancelled,
                            tool_types::ToolResult::cancelled_before_start(
                                tool_request,
                                "the pre-tool hook was cancelled",
                            ),
                        ),
                        HookRunError::Failed(error) => (
                            RunStatus::Failed,
                            tool_types::ToolResult::failed_before_start(
                                tool_request,
                                format!("pre_tool_use hook blocked tool: {error}"),
                                None,
                            ),
                        ),
                    };
                    if emit_deltas {
                        emit_batch_event(
                            sink,
                            &events.tool_call_completed(&result),
                            &mut event_error,
                        );
                    }
                    results[idx] = Some((status, result));
                    continue;
                }
            };
            let invocation = match apply_pre_tool_outcome_with_external(
                invocation,
                &pre_tool_outcome,
                mcp_registry,
                &[],
            ) {
                Ok(invocation) => invocation,
                Err(error) => {
                    let result = error.into_result();
                    if emit_deltas {
                        emit_batch_event(
                            sink,
                            &events.tool_call_completed(&result),
                            &mut event_error,
                        );
                    }
                    results[idx] = Some((RunStatus::Failed, result));
                    continue;
                }
            };

            if cancel.is_cancelled() {
                let result = tool_types::ToolResult::cancelled_before_start(
                    tool_request,
                    "the subagent batch was cancelled before dispatch",
                );
                if emit_deltas {
                    emit_batch_event(sink, &events.tool_call_completed(&result), &mut event_error);
                }
                results[idx] = Some((RunStatus::Cancelled, result));
                continue;
            }

            let request = subagent::create_subagent_request(&invocation.effective);
            let mut subagent_lifecycle =
                RuntimeSessionLifecycle::new(format!("subagent-{}", tool_request.id));
            let subagent_task = subagent_lifecycle
                .start_task(RuntimeTaskKind::Subagent)
                .clone();
            if emit_deltas {
                let event = subagent_task.attach_to_event(
                    events.subagent_started(&tool_request.id, &request.description),
                );
                if !emit_batch_event(sink, &event, &mut event_error) {
                    let error = "subagent dispatch stopped because its started event could not be delivered";
                    let failed_task = subagent_task.with_status(RuntimeTaskStatus::Failed);
                    emit_batch_event(
                        sink,
                        &failed_task.attach_to_event(events.subagent_completed(
                            &tool_request.id,
                            &request.description,
                            RunStatus::Failed,
                            None,
                            Some(error),
                        )),
                        &mut event_error,
                    );
                    let result =
                        tool_types::ToolResult::failed_before_start(tool_request, error, None);
                    emit_batch_event(sink, &events.tool_call_completed(&result), &mut event_error);
                    results[idx] = Some((RunStatus::Failed, result));
                    continue;
                }
            }

            let child_request = ChildAgentRequest {
                prompt: request.prompt.clone(),
                subagent_type: request.subagent_type,
                model: request.model.clone(),
                depth: subagent_depth + 1,
                emit_deltas: false,
                allowed_tools: None,
                tool_policy_label: None,
                workflow_ipc: workflow_ipc.cloned(),
            };
            let child_tool_request = invocation.effective;
            let child_config = config.clone();
            let child_cwd = cwd.to_path_buf();
            let child_instructions = instructions.clone();
            let child_memory = memory.clone();
            let child_mcp_registry = mcp_registry.clone();
            let child_hooks = hooks.clone();
            let child_cancel = cancel.clone();
            let panic_task = subagent_task.clone();
            let panic_description = request.description.clone();
            handles.push((
                idx,
                panic_task,
                panic_description,
                scope.spawn(move || {
                    let mut child_events =
                        EventFactory::new(format!("subagent-{}", child_tool_request.id));
                    let mut child_sink = EventSink::new(io::sink(), child_config.output_format);
                    let mut child_runtime = ChildAgentRuntime::new(ChildAgentRuntimeContext {
                        cwd: &child_cwd,
                        events: &mut child_events,
                        sink: &mut child_sink,
                        instructions: &child_instructions,
                        memory: &child_memory,
                        mcp_registry: &child_mcp_registry,
                        hooks: &child_hooks,
                        cancel: &child_cancel,
                        lifecycle: Some(&mut subagent_lifecycle),
                        executor: child_executor,
                    });
                    let (child, child_cost_tracker) =
                        run_child_agent(&child_config, &child_request, &mut child_runtime);
                    subagent_lifecycle.finish_task(child.status);

                    SubagentExecutionResult {
                        tool_request: child_tool_request,
                        description: request.description,
                        task: subagent_lifecycle
                            .active_task()
                            .cloned()
                            .unwrap_or(subagent_task),
                        schema: request.schema,
                        child,
                        cost_tracker: child_cost_tracker,
                        worktree: None,
                    }
                }),
            ));
        }

        for (idx, panic_task, panic_description, handle) in handles {
            let execution = match handle.join() {
                Ok(execution) => execution,
                Err(_) => {
                    let tool_request = &tool_requests[idx];
                    let error = "Subagent worker panicked after execution started; its side effects and final output are indeterminate. Inspect external state before retrying.";
                    let result =
                        tool_types::ToolResult::indeterminate_after_start(tool_request, error);
                    if emit_deltas {
                        let failed_task = panic_task.with_status(RuntimeTaskStatus::Failed);
                        emit_batch_event(
                            sink,
                            &failed_task.attach_to_event(events.subagent_completed(
                                &tool_request.id,
                                &panic_description,
                                RunStatus::Failed,
                                None,
                                Some(error),
                            )),
                            &mut event_error,
                        );
                        emit_batch_event(
                            sink,
                            &events.tool_call_completed(&result),
                            &mut event_error,
                        );
                    }
                    results[idx] = Some((RunStatus::Failed, result));
                    continue;
                }
            };

            cost_tracker.merge(&execution.cost_tracker);

            let (status, result) =
                match subagent_execution_to_tool_result(events, sink, &execution, emit_deltas) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        if event_error.is_none() {
                            event_error = Some(error);
                        }
                        subagent_execution_to_tool_result(events, sink, &execution, false)
                            .expect("subagent result folding without events cannot fail")
                    }
                };
            if emit_deltas {
                emit_batch_event(sink, &events.tool_call_completed(&result), &mut event_error);
                if let Err(error) = hooks.run(
                    HookEvent::PostToolUse,
                    HookContext {
                        cwd: &cwd.display().to_string(),
                        session_status: None,
                        tool_request: Some(&execution.tool_request),
                        tool_result: Some(&result),
                        before_messages: None,
                        after_messages: None,
                        usage: None,
                    },
                ) {
                    emit_batch_event(
                        sink,
                        &events.error(&format!("post_tool_use hook failed: {error}")),
                        &mut event_error,
                    );
                }
            }
            results[idx] = Some((status, result));
        }
    });

    SubagentBatchExecution {
        results: results
            .into_iter()
            .map(|result| result.expect("each subagent batch item has a result"))
            .collect(),
        event_error,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_subagent_tool<W: io::Write>(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    tool_request: &tool_types::ToolRequest,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    emit_deltas: bool,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
    task_registry: &TaskRegistry,
    workflow_ipc: Option<&WorkflowIpcContext>,
    child_executor: ChildAgentExecutor<W>,
) -> io::Result<tool_types::ToolResult> {
    let request = subagent::create_subagent_request(tool_request);
    let description = request.description.clone();
    let schema = request.schema.clone();
    let mut subagent_lifecycle =
        RuntimeSessionLifecycle::new(format!("subagent-{}", tool_request.id));
    let subagent_task = subagent_lifecycle
        .start_task(RuntimeTaskKind::Subagent)
        .clone();

    if emit_deltas {
        let event =
            subagent_task.attach_to_event(events.subagent_started(&tool_request.id, &description));
        sink.emit(&event)?;
    }

    if subagent_depth >= config.subagents.max_depth {
        let error = format!("subagent max depth {} reached", config.subagents.max_depth);
        let failed_task = subagent_lifecycle
            .finish_task(RunStatus::Failed)
            .cloned()
            .unwrap_or_else(|| subagent_task.clone());
        if emit_deltas {
            let event = failed_task.attach_to_event(events.subagent_completed(
                &tool_request.id,
                &description,
                RunStatus::Failed,
                None,
                Some(&error),
            ));
            sink.emit(&event)?;
        }
        return Ok(tool_types::ToolResult::failed(tool_request, error, None));
    }

    if request.mode == SubagentMode::Async && config.max_budget_usd.is_some() {
        let error = "async subagents are unavailable while max_budget_usd is active; use sync mode so usage can be admitted and reconciled in the parent turn";
        let failed_task = subagent_lifecycle
            .finish_task(RunStatus::Failed)
            .cloned()
            .unwrap_or_else(|| subagent_task.clone());
        if emit_deltas {
            let event = failed_task.attach_to_event(events.subagent_completed(
                &tool_request.id,
                &description,
                RunStatus::Failed,
                None,
                Some(error),
            ));
            sink.emit(&event)?;
        }
        return Ok(tool_types::ToolResult::failed(tool_request, error, None));
    }

    if request.mode == SubagentMode::Async {
        return Ok(launch_async_subagent(AsyncSubagentLaunchContext {
            config,
            cwd,
            tool_request,
            request,
            subagent_depth,
            task_registry,
        }));
    }

    let worktree_guard = if request.isolation == SubagentIsolation::Worktree {
        match WorktreeGuard::create(cwd) {
            Ok(guard) => Some(guard),
            Err(error) => {
                let error = format!("failed to create subagent worktree: {error}");
                let failed_task = subagent_lifecycle
                    .finish_task(RunStatus::Failed)
                    .cloned()
                    .unwrap_or_else(|| subagent_task.clone());
                if emit_deltas {
                    let event = failed_task.attach_to_event(events.subagent_completed(
                        &tool_request.id,
                        &description,
                        RunStatus::Failed,
                        None,
                        Some(&error),
                    ));
                    sink.emit(&event)?;
                }
                return Ok(tool_types::ToolResult::failed(tool_request, error, None));
            }
        }
    } else {
        None
    };
    let child_cwd = worktree_guard
        .as_ref()
        .map(|guard| guard.path())
        .unwrap_or(cwd);
    let child_request = ChildAgentRequest {
        prompt: request.prompt,
        subagent_type: request.subagent_type,
        model: request.model,
        depth: subagent_depth + 1,
        emit_deltas: false,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc: workflow_ipc.cloned(),
    };
    let mut runtime = ChildAgentRuntime::new(ChildAgentRuntimeContext {
        cwd: child_cwd,
        events,
        sink,
        instructions,
        memory,
        mcp_registry,
        hooks,
        cancel,
        lifecycle: Some(&mut subagent_lifecycle),
        executor: child_executor,
    });
    let child_config = config_for_remaining_subagent_budget(config, cost_tracker);
    let (child, child_cost_tracker) = run_child_agent(&child_config, &child_request, &mut runtime);
    drop(runtime);
    let completed_task = subagent_lifecycle
        .finish_task(child.status)
        .cloned()
        .unwrap_or_else(|| subagent_task.clone());
    cost_tracker.merge(&child_cost_tracker);
    let worktree = match worktree_guard.map(WorktreeGuard::finish).transpose() {
        Ok(worktree) => worktree,
        Err(cleanup_error) => {
            let mut error = format!(
                "failed to finish subagent worktree after child status {:?}: {cleanup_error}",
                child.status
            );
            if let Some(child_error) = child.error.as_deref() {
                error.push_str(&format!("\n\nChild error: {child_error}"));
            }
            let failed_task = subagent_lifecycle
                .finish_task(RunStatus::Failed)
                .cloned()
                .unwrap_or_else(|| completed_task.clone());
            if emit_deltas {
                let event = failed_task.attach_to_event(events.subagent_completed(
                    &tool_request.id,
                    &description,
                    RunStatus::Failed,
                    child.final_message.as_deref(),
                    Some(&error),
                ));
                sink.emit(&event)?;
            }
            return Ok(tool_types::ToolResult::failed_after_start(
                tool_request,
                format!("Subagent status: Failed\n\n{error}"),
                None,
            ));
        }
    };

    match child.status {
        RunStatus::Success => {
            let mut output = child
                .final_message
                .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
            if let Err(mut error) =
                validate_subagent_output_schema(&description, schema.as_ref(), &output)
            {
                append_worktree_outcome(&mut error, worktree.as_ref());
                let failed_task = subagent_lifecycle
                    .finish_task(RunStatus::Failed)
                    .cloned()
                    .unwrap_or_else(|| completed_task.clone());
                if emit_deltas {
                    let event = failed_task.attach_to_event(events.subagent_completed(
                        &tool_request.id,
                        &description,
                        RunStatus::Failed,
                        Some(&output),
                        Some(&error),
                    ));
                    sink.emit(&event)?;
                }
                return Ok(tool_types::ToolResult::failed(
                    tool_request,
                    format!("Subagent status: Failed\n\n{error}"),
                    None,
                ));
            }
            append_worktree_outcome(&mut output, worktree.as_ref());
            if emit_deltas {
                let event = completed_task.attach_to_event(events.subagent_completed(
                    &tool_request.id,
                    &description,
                    child.status,
                    Some(&output),
                    None,
                ));
                sink.emit(&event)?;
            }
            Ok(tool_types::ToolResult::completed(
                tool_request,
                format!("Subagent status: success\n\n{output}"),
                false,
            ))
        }
        RunStatus::Cancelled => {
            let mut error = child
                .error
                .unwrap_or_else(|| "subagent ended with status Cancelled".to_string());
            append_worktree_outcome(&mut error, worktree.as_ref());
            if emit_deltas {
                let event = completed_task.attach_to_event(events.subagent_completed(
                    &tool_request.id,
                    &description,
                    RunStatus::Cancelled,
                    child.final_message.as_deref(),
                    Some(&error),
                ));
                sink.emit(&event)?;
            }
            Ok(tool_types::ToolResult::cancelled(
                tool_request,
                format!("Subagent status: Cancelled\n\n{error}"),
                None,
            ))
        }
        status => {
            let mut error = child
                .error
                .unwrap_or_else(|| format!("subagent ended with status {status:?}"));
            append_worktree_outcome(&mut error, worktree.as_ref());
            if emit_deltas {
                let event = completed_task.attach_to_event(events.subagent_completed(
                    &tool_request.id,
                    &description,
                    status,
                    child.final_message.as_deref(),
                    Some(&error),
                ));
                sink.emit(&event)?;
            }
            Ok(tool_types::ToolResult::failed(
                tool_request,
                format!("Subagent status: {status:?}\n\n{error}"),
                None,
            ))
        }
    }
}

fn config_for_remaining_subagent_budget(
    config: &RunConfig,
    cost_tracker: &CostTracker,
) -> RunConfig {
    let mut child_config = config.clone();
    if let Some(max_budget) = config.max_budget_usd {
        child_config.max_budget_usd =
            Some((max_budget - cost_tracker.totals().estimated_cost_usd).max(0.0));
    }
    child_config
}

fn subagent_execution_to_tool_result(
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    execution: &SubagentExecutionResult,
    emit_deltas: bool,
) -> io::Result<(RunStatus, tool_types::ToolResult)> {
    match execution.child.status {
        RunStatus::Success => {
            let mut output = execution
                .child
                .final_message
                .clone()
                .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
            if let Err(mut error) = validate_subagent_output_schema(
                &execution.description,
                execution.schema.as_ref(),
                &output,
            ) {
                append_worktree_outcome(&mut error, execution.worktree.as_ref());
                if emit_deltas {
                    let failed_task = execution.task.with_status(RuntimeTaskStatus::Failed);
                    let event = failed_task.attach_to_event(events.subagent_completed(
                        &execution.tool_request.id,
                        &execution.description,
                        RunStatus::Failed,
                        Some(&output),
                        Some(&error),
                    ));
                    sink.emit(&event)?;
                }
                return Ok((
                    RunStatus::Failed,
                    tool_types::ToolResult::failed(
                        &execution.tool_request,
                        format!("Subagent status: Failed\n\n{error}"),
                        None,
                    ),
                ));
            }
            append_worktree_outcome(&mut output, execution.worktree.as_ref());
            if emit_deltas {
                let event = execution.task.attach_to_event(events.subagent_completed(
                    &execution.tool_request.id,
                    &execution.description,
                    execution.child.status,
                    Some(&output),
                    None,
                ));
                sink.emit(&event)?;
            }
            Ok((
                RunStatus::Success,
                tool_types::ToolResult::completed(
                    &execution.tool_request,
                    format!("Subagent status: success\n\n{output}"),
                    false,
                ),
            ))
        }
        RunStatus::Cancelled => {
            let mut error = execution
                .child
                .error
                .clone()
                .unwrap_or_else(|| "subagent ended with status Cancelled".to_string());
            append_worktree_outcome(&mut error, execution.worktree.as_ref());
            if emit_deltas {
                let event = execution.task.attach_to_event(events.subagent_completed(
                    &execution.tool_request.id,
                    &execution.description,
                    RunStatus::Cancelled,
                    execution.child.final_message.as_deref(),
                    Some(&error),
                ));
                sink.emit(&event)?;
            }
            Ok((
                RunStatus::Cancelled,
                tool_types::ToolResult::cancelled(
                    &execution.tool_request,
                    format!("Subagent status: Cancelled\n\n{error}"),
                    None,
                ),
            ))
        }
        status => {
            let mut error = execution
                .child
                .error
                .clone()
                .unwrap_or_else(|| format!("subagent ended with status {status:?}"));
            append_worktree_outcome(&mut error, execution.worktree.as_ref());
            if emit_deltas {
                let event = execution.task.attach_to_event(events.subagent_completed(
                    &execution.tool_request.id,
                    &execution.description,
                    status,
                    execution.child.final_message.as_deref(),
                    Some(&error),
                ));
                sink.emit(&event)?;
            }
            Ok((
                RunStatus::Failed,
                tool_types::ToolResult::failed(
                    &execution.tool_request,
                    format!("Subagent status: {status:?}\n\n{error}"),
                    None,
                ),
            ))
        }
    }
}

pub(crate) fn append_worktree_outcome(output: &mut String, outcome: Option<&WorktreeOutcome>) {
    if let Some(outcome) = outcome {
        let status = if outcome.preserved {
            "preserved"
        } else {
            "cleaned"
        };
        output.push_str(&format!(
            "\n\nWorktree {status}: {}",
            outcome.path.display()
        ));
    }
}

pub(crate) fn validate_subagent_output_schema(
    description: &str,
    schema: Option<&Value>,
    output: &str,
) -> Result<(), String> {
    let Some(schema) = schema else {
        return Ok(());
    };
    let value = subagent_output_value(output);
    validate_json_schema_subset(schema, &value, "$").map_err(|error| {
        format!("subagent output schema validation failed for {description}: {error}")
    })
}

fn subagent_output_value(output: &str) -> Value {
    serde_json::from_str(output).unwrap_or_else(|_| Value::String(output.to_string()))
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
    use crate::cost::CostTracker;
    use crate::hooks::HookRunner;
    use crate::instructions::ProjectInstructions;
    use crate::memory::MemoryBlock;
    use crate::tasks::TaskRegistry;
    use orca_core::approval_types::ActionKind;
    use orca_core::cancel::CancelToken;
    use orca_core::config::{OutputFormat, ProviderKind, RunConfig};
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::event_sink::EventSink;
    use orca_core::hook_types::{HookConfig, HookEvent};
    use orca_core::model::ModelSelection;
    use orca_core::provider_types::Usage;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types;
    use orca_mcp::McpRegistry;

    use crate::tool_turn::ToolTurnOutcome;

    fn config(subagents: SubagentConfig) -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: orca_core::approval_types::ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            history_mode: orca_core::config::HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            mcp_servers: Vec::new(),
            external_tools: Vec::new(),
            hooks: Vec::new(),
            subagents,
            tools: Default::default(),
            workflows: Default::default(),
            theme: orca_core::config::ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn subagent_request(id: &str) -> tool_types::ToolRequest {
        tool_types::ToolRequest {
            id: id.to_string(),
            name: tool_types::ToolName::Subagent,
            action: ActionKind::Agent,
            target: Some(format!("inspect {id}")),
            raw_arguments: Some(
                serde_json::json!({
                    "description": format!("inspect {id}"),
                    "prompt": format!("inspect {id}")
                })
                .to_string(),
            ),
        }
    }

    fn history_writer_that_fails_on_append(
        label: &str,
    ) -> (tempfile::TempDir, crate::thread_store::SessionWriter) {
        let history = tempfile::tempdir().expect("history tempdir");
        let history_path = history.path().join("session.jsonl");
        let meta = crate::history::create_meta(history.path(), "mock", None, label);
        let mut meta_record = serde_json::to_value(meta)
            .expect("serialize history metadata")
            .as_object()
            .cloned()
            .expect("history metadata object");
        meta_record.insert("type".to_string(), serde_json::json!("session.meta"));
        std::fs::write(
            &history_path,
            format!("{}\n", serde_json::Value::Object(meta_record)),
        )
        .expect("seed history file");
        let writer = crate::thread_store::SessionWriter::append_to_existing(history_path.clone())
            .expect("open existing history");
        std::fs::remove_file(&history_path).expect("remove history file");
        std::fs::create_dir(&history_path).expect("replace history file with directory");
        (history, writer)
    }

    #[test]
    fn batch_plan_stops_at_async_request_boundary() {
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 3;
        let config = config(subagents);
        let async_request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async task",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
            ..subagent_request("async")
        };
        let requests = vec![subagent_request("a"), async_request, subagent_request("b")];

        assert!(super::should_run_subagent_batch(&config, &requests[0], 0));
        assert_eq!(super::collect_subagent_batch(&config, &requests, 0), 1);
    }

    #[test]
    fn budget_mode_disables_parallel_subagent_batching() {
        let subagents = SubagentConfig {
            max_parallel: 3,
            ..SubagentConfig::default()
        };
        let mut config = config(subagents);
        config.max_budget_usd = Some(1.0);
        let requests = [subagent_request("a"), subagent_request("b")];

        assert!(!super::should_run_subagent_batch(&config, &requests[0], 0));
    }

    #[test]
    fn sync_subagent_receives_only_remaining_aggregate_budget() {
        let mut config = config(SubagentConfig::default());
        config.max_budget_usd = Some(0.5);
        let mut cost_tracker = CostTracker::new(None);
        cost_tracker.add_usage(Usage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_tokens: 0,
        });

        let child_config = super::config_for_remaining_subagent_budget(&config, &cost_tracker);

        let remaining = child_config.max_budget_usd.expect("remaining budget");
        assert!((remaining - 0.36).abs() < 1e-12);
        assert_eq!(config.max_budget_usd, Some(0.5));
    }

    #[test]
    fn record_subagent_batch_results_records_tools_and_returns_failure() {
        let request = subagent_request("failed");
        let result = tool_types::ToolResult::failed(&request, "child failed", None);
        let mut conversation = orca_core::conversation::Conversation::new();

        let outcome = super::record_subagent_batch_results(
            &mut conversation,
            None,
            vec![(RunStatus::Failed, result)],
            true,
        )
        .expect("records subagent batch result");

        match outcome {
            super::SubagentBatchRecordOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::Failed);
                assert_eq!(error.as_deref(), Some("child failed"));
            }
            super::SubagentBatchRecordOutcome::Continue => {
                panic!("failed subagent batch should request early return")
            }
        }
        assert_eq!(conversation.messages.len(), 1);
    }

    #[test]
    fn record_subagent_batch_results_records_executed_suffix_before_returning_first_failure() {
        let first_request = subagent_request("first");
        let failed_request = subagent_request("failed");
        let third_request = subagent_request("third");
        let mut conversation = orca_core::conversation::Conversation::new();

        let outcome = super::record_subagent_batch_results(
            &mut conversation,
            None,
            vec![
                (
                    RunStatus::Success,
                    tool_types::ToolResult::completed(
                        &first_request,
                        "first completed".to_string(),
                        false,
                    ),
                ),
                (
                    RunStatus::Failed,
                    tool_types::ToolResult::failed(&failed_request, "child failed", None),
                ),
                (
                    RunStatus::Success,
                    tool_types::ToolResult::completed(
                        &third_request,
                        "third completed".to_string(),
                        false,
                    ),
                ),
            ],
            false,
        )
        .expect("record complete subagent batch");

        assert!(matches!(
            outcome,
            super::SubagentBatchRecordOutcome::Return {
                status: RunStatus::Failed,
                error: Some(ref error),
            } if error == "child failed"
        ));
        assert_eq!(conversation.messages.len(), 3);
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|message| match message {
                    orca_core::conversation::Message::Tool { tool_call_id, .. } => {
                        tool_call_id.as_str()
                    }
                    _ => panic!("expected tool result"),
                })
                .collect::<Vec<_>>(),
            vec!["first", "failed", "third"]
        );
    }

    #[test]
    fn record_subagent_batch_results_keeps_live_terminals_after_history_failure() {
        let (_history, mut writer) =
            history_writer_that_fails_on_append("subagent batch history failure");
        let first = subagent_request("first");
        let second = subagent_request("second");
        let mut conversation = orca_core::conversation::Conversation::new();

        let error = match super::record_subagent_batch_results(
            &mut conversation,
            Some(&mut writer),
            vec![
                (
                    RunStatus::Success,
                    tool_types::ToolResult::completed(&first, "first completed".to_string(), false),
                ),
                (
                    RunStatus::Success,
                    tool_types::ToolResult::completed(
                        &second,
                        "second completed".to_string(),
                        false,
                    ),
                ),
            ],
            true,
        ) {
            Err(error) => error,
            Ok(_) => panic!("history append must fail"),
        };

        assert!(error.raw_os_error().is_some());
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|message| match message {
                    orca_core::conversation::Message::Tool { tool_call_id, .. } => {
                        tool_call_id.as_str()
                    }
                    _ => panic!("expected tool result"),
                })
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
    }

    #[test]
    fn run_subagent_batch_tool_turn_executes_and_records_results() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let mut events = EventFactory::new("subagent-batch-turn".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let requests = vec![subagent_request("injected"), subagent_request("injected")];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let mut conversation = orca_core::conversation::Conversation::new();

        let outcome =
            super::run_subagent_batch_tool_turn(super::RuntimeSubagentBatchToolTurnContext {
                request: super::RuntimeSubagentBatchToolTurnRequest {
                    config: &config,
                    cwd: cwd.path(),
                    tool_requests: &requests,
                    subagent_depth: 0,
                    emit_deltas: true,
                },
                io: super::RuntimeSubagentBatchToolTurnIo {
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut conversation,
                    history_writer: None,
                    cost_tracker: &mut cost_tracker,
                },
                services: super::RuntimeSubagentBatchToolTurnServices {
                    instructions: &instructions,
                    memory: &memory,
                    mcp_registry: &mcp_registry,
                    hooks: &hooks,
                },
                runtime: super::RuntimeSubagentBatchToolTurnRuntime {
                    cancel: &cancel,
                    workflow_ipc: None,
                },
                child_executor: fake_child_executor::<std::io::Sink>,
            })
            .expect("run subagent batch tool turn");

        assert!(matches!(outcome, ToolTurnOutcome::Continue));
        assert_eq!(conversation.messages.len(), 2);
        assert!(
            matches!(&conversation.messages[0], orca_core::conversation::Message::Tool { tool_call_id, content, .. }
                if tool_call_id == "injected" && content.contains("injected child result"))
        );
        assert!(
            matches!(&conversation.messages[1], orca_core::conversation::Message::Tool { tool_call_id, content, .. }
                if tool_call_id == "injected" && content.contains("injected child result"))
        );
    }

    fn fake_child_executor<W: io::Write>(
        _config: &RunConfig,
        request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        assert_eq!(request.prompt, "inspect injected");
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("injected child result".to_string()),
            error: None,
        })
    }

    fn unexpected_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("budget-rejected async subagent must not start a child executor")
    }

    fn cancelled_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        Ok(ChildAgentResult {
            status: RunStatus::Cancelled,
            final_message: None,
            error: Some("child turn cancelled".to_string()),
        })
    }

    fn cancelling_child_executor<W: io::Write>(
        _config: &RunConfig,
        request: &ChildAgentRequest,
        runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        let marker = runtime
            .cwd
            .join(format!("{}.started", request.prompt.replace(' ', "-")));
        std::fs::write(marker, "started\n")?;
        runtime.cancel.cancel();
        Ok(ChildAgentResult {
            status: RunStatus::Cancelled,
            final_message: None,
            error: Some("child cancelled parent batch".to_string()),
        })
    }

    fn delayed_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        std::thread::sleep(Duration::from_millis(250));
        std::fs::write(runtime.cwd.join("delayed-child-finished"), "finished\n")?;
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("finished".to_string()),
            error: None,
        })
    }

    fn panic_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("child worker panic")
    }

    #[derive(Default)]
    struct FailThirdFlush {
        flushes: usize,
    }

    impl io::Write for FailThirdFlush {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == 3 {
                return Err(io::Error::other("event consumer disconnected"));
            }
            Ok(())
        }
    }

    #[test]
    fn subagent_batch_cancellation_stops_blocked_hook_and_unstarted_sibling() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let mut events = EventFactory::new("subagent-batch-hook-cancel".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let requests = vec![subagent_request("first"), subagent_request("second")];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: r#"if [ "$ORCA_TOOL_TARGET" = "inspect second" ]; then sleep 5; fi"#
                .to_string(),
            tool: Some("subagent".to_string()),
        }]);
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let mut conversation = orca_core::conversation::Conversation::new();
        let started = Instant::now();

        let outcome =
            super::run_subagent_batch_tool_turn(super::RuntimeSubagentBatchToolTurnContext {
                request: super::RuntimeSubagentBatchToolTurnRequest {
                    config: &config,
                    cwd: cwd.path(),
                    tool_requests: &requests,
                    subagent_depth: 0,
                    emit_deltas: false,
                },
                io: super::RuntimeSubagentBatchToolTurnIo {
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut conversation,
                    history_writer: None,
                    cost_tracker: &mut cost_tracker,
                },
                services: super::RuntimeSubagentBatchToolTurnServices {
                    instructions: &instructions,
                    memory: &memory,
                    mcp_registry: &mcp_registry,
                    hooks: &hooks,
                },
                runtime: super::RuntimeSubagentBatchToolTurnRuntime {
                    cancel: &cancel,
                    workflow_ipc: None,
                },
                child_executor: cancelling_child_executor::<std::io::Sink>,
            })
            .expect("cancel subagent batch");

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(
            outcome,
            ToolTurnOutcome::Return {
                status: RunStatus::Cancelled,
                ..
            }
        ));
        assert!(cwd.path().join("inspect-first.started").exists());
        assert!(!cwd.path().join("inspect-second.started").exists());
        assert_eq!(conversation.messages.len(), 2);
        assert!(matches!(
            &conversation.messages[1],
            orca_core::conversation::Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == tool_types::ToolStatus::Cancelled
                && terminal.started == tool_types::ToolInvocationStarted::No
        ));
    }

    #[test]
    fn subagent_batch_joins_started_worker_before_event_io_error_returns() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let mut events = EventFactory::new("subagent-batch-event-error".to_string());
        let mut sink = EventSink::new(FailThirdFlush::default(), OutputFormat::Text);
        let requests = vec![subagent_request("first"), subagent_request("second")];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let mut conversation = orca_core::conversation::Conversation::new();
        let started = Instant::now();

        let error =
            match super::run_subagent_batch_tool_turn(super::RuntimeSubagentBatchToolTurnContext {
                request: super::RuntimeSubagentBatchToolTurnRequest {
                    config: &config,
                    cwd: cwd.path(),
                    tool_requests: &requests,
                    subagent_depth: 0,
                    emit_deltas: true,
                },
                io: super::RuntimeSubagentBatchToolTurnIo {
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut conversation,
                    history_writer: None,
                    cost_tracker: &mut cost_tracker,
                },
                services: super::RuntimeSubagentBatchToolTurnServices {
                    instructions: &instructions,
                    memory: &memory,
                    mcp_registry: &mcp_registry,
                    hooks: &hooks,
                },
                runtime: super::RuntimeSubagentBatchToolTurnRuntime {
                    cancel: &cancel,
                    workflow_ipc: None,
                },
                child_executor: delayed_child_executor::<std::io::Sink>,
            }) {
                Err(error) => error,
                Ok(_) => panic!("third event flush should fail after recording terminals"),
            };

        assert!(error.to_string().contains("event consumer disconnected"));
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(cwd.path().join("delayed-child-finished").exists());
        assert_eq!(conversation.messages.len(), 2);
        assert!(matches!(
            &conversation.messages[0],
            orca_core::conversation::Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == tool_types::ToolStatus::Completed
        ));
        assert!(matches!(
            &conversation.messages[1],
            orca_core::conversation::Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == tool_types::ToolStatus::Failed
                && terminal.started == tool_types::ToolInvocationStarted::No
        ));
    }

    #[test]
    fn subagent_batch_panic_is_indeterminate_and_closes_lifecycle_event() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let mut events = EventFactory::new("subagent-batch-panic".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let requests = vec![subagent_request("panic")];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let mut conversation = orca_core::conversation::Conversation::new();

        let outcome =
            super::run_subagent_batch_tool_turn(super::RuntimeSubagentBatchToolTurnContext {
                request: super::RuntimeSubagentBatchToolTurnRequest {
                    config: &config,
                    cwd: cwd.path(),
                    tool_requests: &requests,
                    subagent_depth: 0,
                    emit_deltas: true,
                },
                io: super::RuntimeSubagentBatchToolTurnIo {
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut conversation,
                    history_writer: None,
                    cost_tracker: &mut cost_tracker,
                },
                services: super::RuntimeSubagentBatchToolTurnServices {
                    instructions: &instructions,
                    memory: &memory,
                    mcp_registry: &mcp_registry,
                    hooks: &hooks,
                },
                runtime: super::RuntimeSubagentBatchToolTurnRuntime {
                    cancel: &cancel,
                    workflow_ipc: None,
                },
                child_executor: panic_child_executor::<std::io::Sink>,
            })
            .expect("panic must become a terminal result");

        assert!(matches!(
            outcome,
            ToolTurnOutcome::Return {
                status: RunStatus::Failed,
                ..
            }
        ));
        assert!(matches!(
            &conversation.messages[0],
            orca_core::conversation::Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == tool_types::ToolStatus::Indeterminate
                && terminal.started == tool_types::ToolInvocationStarted::Yes
        ));
        let emitted = String::from_utf8(sink.writer_mut().clone()).expect("jsonl events");
        let parsed = emitted
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(parsed.iter().any(|event| {
            event["type"] == "subagent.completed"
                && event["payload"]["status"] == "failed"
                && event["payload"]["task"]["status"] == "failed"
        }));
        assert!(parsed.iter().any(|event| {
            event["type"] == "tool.call.completed" && event["payload"]["status"] == "indeterminate"
        }));
    }

    fn remove_worktree_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        std::fs::remove_dir_all(runtime.cwd).expect("remove child worktree");
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("child completed before cleanup".to_string()),
            error: None,
        })
    }

    fn run_git(cwd: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn isolated_subagent_cleanup_failure_returns_started_terminal() {
        let repo = tempfile::tempdir().expect("temp repo");
        run_git(repo.path(), &["init"]);
        run_git(repo.path(), &["config", "user.email", "orca@example.test"]);
        run_git(repo.path(), &["config", "user.name", "Orca Test"]);
        std::fs::write(repo.path().join("tracked.txt"), "tracked\n").expect("write fixture");
        run_git(repo.path(), &["add", "tracked.txt"]);
        run_git(repo.path(), &["commit", "-m", "seed"]);

        let config = config(SubagentConfig::default());
        let request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "cleanup failure",
                    "prompt": "cleanup failure",
                    "isolation": "worktree"
                })
                .to_string(),
            ),
            ..subagent_request("cleanup-failure")
        };
        let mut events = EventFactory::new("subagent-cleanup-failure".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-cleanup-failure".to_string());

        let result = super::execute_subagent_tool(
            &config,
            repo.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            true,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            remove_worktree_child_executor::<Vec<u8>>,
        )
        .expect("cleanup failure must return a tool terminal");

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert_eq!(
            result.terminal().started,
            tool_types::ToolInvocationStarted::Yes
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("failed to finish subagent worktree"))
        );
        let emitted = String::from_utf8(sink.writer_mut().clone()).expect("jsonl events");
        assert!(emitted.lines().any(|line| {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            event["type"] == "subagent.completed" && event["payload"]["status"] == "failed"
        }));
    }

    #[test]
    fn subagent_batch_preserves_cancelled_child_terminals() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let mut events = EventFactory::new("subagent-batch-cancelled".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let requests = vec![
            subagent_request("cancelled-1"),
            subagent_request("cancelled-2"),
        ];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let mut conversation = orca_core::conversation::Conversation::new();

        let outcome =
            super::run_subagent_batch_tool_turn(super::RuntimeSubagentBatchToolTurnContext {
                request: super::RuntimeSubagentBatchToolTurnRequest {
                    config: &config,
                    cwd: cwd.path(),
                    tool_requests: &requests,
                    subagent_depth: 0,
                    emit_deltas: true,
                },
                io: super::RuntimeSubagentBatchToolTurnIo {
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut conversation,
                    history_writer: None,
                    cost_tracker: &mut cost_tracker,
                },
                services: super::RuntimeSubagentBatchToolTurnServices {
                    instructions: &instructions,
                    memory: &memory,
                    mcp_registry: &mcp_registry,
                    hooks: &hooks,
                },
                runtime: super::RuntimeSubagentBatchToolTurnRuntime {
                    cancel: &cancel,
                    workflow_ipc: None,
                },
                child_executor: cancelled_child_executor::<std::io::Sink>,
            })
            .expect("run cancelled subagent batch");

        assert!(matches!(
            outcome,
            ToolTurnOutcome::Return {
                status: RunStatus::Cancelled,
                ..
            }
        ));
        assert_eq!(conversation.messages.len(), 2);
        for message in &conversation.messages {
            assert!(matches!(
                message,
                orca_core::conversation::Message::Tool {
                    terminal: Some(terminal),
                    ..
                } if terminal.status == tool_types::ToolStatus::Cancelled
                    && terminal.started == tool_types::ToolInvocationStarted::Yes
            ));
        }
    }

    #[test]
    fn sync_subagent_uses_injected_child_executor() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("subagent-injected".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "inspect injected",
                    "prompt": "inspect injected"
                })
                .to_string(),
            ),
            ..subagent_request("injected")
        };
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-injected".to_string());

        let result = super::execute_subagent_tool(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            true,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            fake_child_executor::<Vec<u8>>,
        )
        .expect("subagent tool");

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert!(
            result
                .output
                .as_deref()
                .unwrap_or_default()
                .contains("injected child result")
        );
    }

    #[test]
    fn budget_mode_rejects_async_subagent_before_task_launch() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut config = config(SubagentConfig::default());
        config.max_budget_usd = Some(1.0);
        let mut events = EventFactory::new("subagent-budget-async".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "inspect later",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
            ..subagent_request("budget-async")
        };
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-budget-async".to_string());

        let result = super::execute_subagent_tool(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            true,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            unexpected_child_executor::<Vec<u8>>,
        )
        .expect("budget-rejected subagent tool");

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("max_budget_usd is active"))
        );
        assert!(task_registry.list().is_empty());
        assert_eq!(cost_tracker.totals(), Default::default());
    }

    #[test]
    fn sync_subagent_preserves_cancelled_child_terminal() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("subagent-cancelled".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = subagent_request("cancelled");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-cancelled".to_string());

        let result = super::execute_subagent_tool(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            true,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            cancelled_child_executor::<Vec<u8>>,
        )
        .expect("cancelled subagent tool");

        assert_eq!(result.status, tool_types::ToolStatus::Cancelled);
        assert_eq!(
            result.terminal().started,
            tool_types::ToolInvocationStarted::Yes
        );
        assert_eq!(
            result.error.as_deref(),
            Some("Subagent status: Cancelled\n\nchild turn cancelled")
        );
    }
}
