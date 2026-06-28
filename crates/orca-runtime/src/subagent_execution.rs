use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::hook_types::HookEvent;
use orca_core::tool_types;
use orca_mcp::McpRegistry;
use serde_json::Value;

use crate::agent_child::{
    ChildAgentExecutor, ChildAgentRequest, ChildAgentResult, ChildAgentRuntime, run_child_agent,
};
use crate::agent_loop::execute_child_agent_loop;
use crate::cost::CostTracker;
use crate::hooks::{HookContext, HookRunner};
use crate::instructions::{self, ProjectInstructions};
use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTaskKind, RuntimeTaskStatus};
use crate::memory::{self, MemoryBlock};
use crate::schema_validation::validate_json_schema_subset;
use crate::session::record_tool_result_for_agent;
use crate::subagent::{self, SubagentIsolation, SubagentMode};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_invocation::{
    apply_pre_tool_outcome_with_external, prepare_tool_invocation_with_external,
    validate_tool_invocation_with_external,
};
use crate::workflow::ipc::WorkflowIpcContext;
use crate::worktree::{WorktreeGuard, WorktreeOutcome};

#[derive(Clone, Debug)]
pub struct AsyncSubagentWorktree {
    pub repo_root: PathBuf,
    pub path: PathBuf,
}

pub(crate) enum SubagentBatchRecordOutcome {
    Continue,
    Return {
        status: RunStatus,
        error: Option<String>,
    },
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

pub(crate) fn should_run_subagent_batch(
    config: &RunConfig,
    tool_request: &tool_types::ToolRequest,
    subagent_depth: u32,
) -> bool {
    tool_request.name == tool_types::ToolName::Subagent
        && subagent_depth < config.subagents.max_depth
        && config.subagents.max_parallel > 1
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
    for (status, result) in results {
        record_tool_result_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            &result,
            emit_deltas,
        )?;

        if status == RunStatus::ApprovalRequired {
            return Ok(SubagentBatchRecordOutcome::Return {
                status,
                error: result.error.clone(),
            });
        }
        if status == RunStatus::Failed {
            return Ok(SubagentBatchRecordOutcome::Return {
                status: RunStatus::Failed,
                error: result.error.clone(),
            });
        }
    }

    Ok(SubagentBatchRecordOutcome::Continue)
}

fn is_batchable_subagent_request(tool_request: &tool_types::ToolRequest) -> bool {
    if tool_request.name != tool_types::ToolName::Subagent {
        return false;
    }
    let request = subagent::create_subagent_request(tool_request);
    request.mode == SubagentMode::Sync && request.isolation == SubagentIsolation::None
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_subagent_batch(
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
) -> io::Result<Vec<(RunStatus, tool_types::ToolResult)>> {
    let mut handles = Vec::new();
    let mut results: Vec<Option<(RunStatus, tool_types::ToolResult)>> =
        vec![None; tool_requests.len()];

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        let invocation = prepare_tool_invocation_with_external(
            tool_request,
            subagent_depth,
            config.subagents.max_depth,
            mcp_registry,
            &[],
        );
        if emit_deltas {
            sink.emit(&events.tool_call_requested(tool_request))?;
        }
        if let Err(error) = validate_tool_invocation_with_external(&invocation, mcp_registry, &[]) {
            let result = error.into_result();
            if emit_deltas {
                sink.emit(&events.tool_call_completed(&result))?;
            }
            results[idx] = Some((RunStatus::Failed, result));
            continue;
        }
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
                if emit_deltas {
                    sink.emit(&events.tool_call_completed(&result))?;
                }
                results[idx] = Some((RunStatus::Failed, result));
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
                    sink.emit(&events.tool_call_completed(&result))?;
                }
                results[idx] = Some((RunStatus::Failed, result));
                continue;
            }
        };

        let request = subagent::create_subagent_request(&invocation.effective);
        let mut subagent_lifecycle =
            RuntimeSessionLifecycle::new(format!("subagent-{}", tool_request.id));
        let subagent_task = subagent_lifecycle
            .start_task(RuntimeTaskKind::Subagent)
            .clone();
        if emit_deltas {
            let event = subagent_task
                .attach_to_event(events.subagent_started(&tool_request.id, &request.description));
            sink.emit(&event)?;
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
        handles.push((
            idx,
            thread::spawn(move || {
                let mut child_events =
                    EventFactory::new(format!("subagent-{}", child_tool_request.id));
                let mut child_sink = EventSink::new(io::sink(), child_config.output_format);
                let mut child_runtime = ChildAgentRuntime::new(
                    &child_cwd,
                    &mut child_events,
                    &mut child_sink,
                    &child_instructions,
                    &child_memory,
                    &child_mcp_registry,
                    &child_hooks,
                    &child_cancel,
                    Some(&mut subagent_lifecycle),
                    child_executor,
                );
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

    for (idx, handle) in handles {
        let execution = match handle.join() {
            Ok(execution) => execution,
            Err(_) => {
                let tool_request = &tool_requests[idx];
                let result =
                    tool_types::ToolResult::failed(tool_request, "subagent thread panicked", None);
                if emit_deltas {
                    sink.emit(&events.tool_call_completed(&result))?;
                }
                results[idx] = Some((RunStatus::Failed, result));
                continue;
            }
        };

        cost_tracker.merge(&execution.cost_tracker);

        let (status, result) =
            subagent_execution_to_tool_result(events, sink, &execution, emit_deltas)?;
        if emit_deltas {
            sink.emit(&events.tool_call_completed(&result))?;
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
                sink.emit(&events.error(&format!("post_tool_use hook failed: {error}")))?;
            }
        }
        results[idx] = Some((status, result));
    }

    Ok(results
        .into_iter()
        .map(|result| result.expect("each subagent batch item has a result"))
        .collect())
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

    if request.mode == SubagentMode::Async {
        return Ok(launch_async_subagent(
            config,
            cwd,
            tool_request,
            request,
            subagent_depth,
            task_registry,
        ));
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
    let mut runtime = ChildAgentRuntime::new(
        child_cwd,
        events,
        sink,
        instructions,
        memory,
        mcp_registry,
        hooks,
        cancel,
        Some(&mut subagent_lifecycle),
        child_executor,
    );
    let (child, child_cost_tracker) = run_child_agent(config, &child_request, &mut runtime);
    drop(runtime);
    let completed_task = subagent_lifecycle
        .finish_task(child.status)
        .cloned()
        .unwrap_or_else(|| subagent_task.clone());
    let worktree = worktree_guard
        .map(WorktreeGuard::finish)
        .transpose()
        .map_err(io::Error::other)?;

    cost_tracker.merge(&child_cost_tracker);

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

pub fn run_async_subagent_worker(
    config: RunConfig,
    cwd: PathBuf,
    child_cwd: PathBuf,
    task_session_id: String,
    agent_id: String,
    request: subagent::SubagentRequest,
    child_depth: u32,
    worktree: Option<AsyncSubagentWorktree>,
) -> i32 {
    run_async_subagent_worker_with_executor(
        config,
        cwd,
        child_cwd,
        task_session_id,
        agent_id,
        request,
        child_depth,
        worktree,
        execute_child_agent_loop,
    )
}

pub(crate) fn run_async_subagent_worker_with_executor(
    config: RunConfig,
    cwd: PathBuf,
    child_cwd: PathBuf,
    task_session_id: String,
    agent_id: String,
    request: subagent::SubagentRequest,
    child_depth: u32,
    worktree: Option<AsyncSubagentWorktree>,
    child_executor: ChildAgentExecutor<io::Sink>,
) -> i32 {
    let task_registry = TaskRegistry::new_for_cwd(task_session_id, &cwd);
    let _ = task_registry.mark_running(&agent_id);
    let instructions = instructions::load_for_cwd_or_default(&cwd);
    let memory = memory::load_for_cwd(&cwd);
    let hooks = HookRunner::new(config.hooks.clone());
    let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
    let cancel = CancelToken::new();
    let child_request = ChildAgentRequest {
        prompt: request.prompt,
        subagent_type: request.subagent_type,
        model: request.model,
        depth: child_depth,
        emit_deltas: false,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc: None,
    };
    let mut child_events = EventFactory::new(format!("subagent-{agent_id}"));
    let mut child_lifecycle = RuntimeSessionLifecycle::new(format!("subagent-{agent_id}"));
    child_lifecycle.start_task(RuntimeTaskKind::Subagent);
    let mut child_sink = EventSink::new(io::sink(), config.output_format);
    let mut child_runtime = ChildAgentRuntime::new(
        &child_cwd,
        &mut child_events,
        &mut child_sink,
        &instructions,
        &memory,
        &mcp_registry,
        &hooks,
        &cancel,
        Some(&mut child_lifecycle),
        child_executor,
    );
    let (child, child_cost_tracker) = run_child_agent(&config, &child_request, &mut child_runtime);
    drop(child_runtime);
    let completed_task = child_lifecycle
        .finish_task(child.status)
        .cloned()
        .unwrap_or_else(|| {
            child_lifecycle.active_task().cloned().unwrap_or_else(|| {
                RuntimeSessionLifecycle::new(format!("subagent-{agent_id}"))
                    .start_task(RuntimeTaskKind::Subagent)
                    .clone()
            })
        });
    let worktree = worktree.and_then(|worktree| {
        WorktreeGuard::finish_existing(worktree.repo_root, worktree.path).ok()
    });
    let usage = usage_totals_if_non_empty(child_cost_tracker.totals());
    if child.status == RunStatus::Success {
        let mut output = child
            .final_message
            .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
        if let Err(mut error) =
            validate_subagent_output_schema(&request.description, request.schema.as_ref(), &output)
        {
            append_worktree_outcome(&mut error, worktree.as_ref());
            let failed_task = completed_task.with_status(RuntimeTaskStatus::Failed);
            let error = async_subagent_result_payload(error, Some(failed_task.payload()));
            if task_registry
                .fail_with_usage(&agent_id, error, usage)
                .is_ok()
            {
                return 1;
            }
            return 1;
        }
        append_worktree_outcome(&mut output, worktree.as_ref());
        let output = async_subagent_result_payload(output, Some(completed_task.payload()));
        if task_registry
            .complete_with_usage(&agent_id, output, usage)
            .is_ok()
        {
            return 0;
        }
    } else {
        let mut error = child
            .error
            .or(child.final_message)
            .unwrap_or_else(|| format!("subagent ended with status {:?}", child.status));
        append_worktree_outcome(&mut error, worktree.as_ref());
        let error = async_subagent_result_payload(error, Some(completed_task.payload()));
        if task_registry
            .fail_with_usage(&agent_id, error, usage)
            .is_ok()
        {
            return 1;
        }
    }
    1
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

#[allow(clippy::too_many_arguments)]
fn launch_async_subagent(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tool_types::ToolRequest,
    request: subagent::SubagentRequest,
    subagent_depth: u32,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    let agent_type = serde_json::to_value(&request.subagent_type)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string));
    let task = task_registry.create_subagent(request.description.clone(), agent_type);
    let agent_id = task.id.clone();
    let worktree_guard = if request.isolation == SubagentIsolation::Worktree {
        match WorktreeGuard::create(cwd) {
            Ok(guard) => Some(guard),
            Err(error) => {
                let error = format!("failed to create subagent worktree: {error}");
                let _ = task_registry.fail(&agent_id, error.clone());
                return tool_types::ToolResult::failed(tool_request, error, None);
            }
        }
    } else {
        None
    };
    let child_cwd = worktree_guard
        .as_ref()
        .map(|guard| guard.path().to_path_buf())
        .unwrap_or_else(|| cwd.to_path_buf());
    let worktree = worktree_guard.as_ref().map(|guard| AsyncSubagentWorktree {
        repo_root: guard.repo_root().to_path_buf(),
        path: guard.path().to_path_buf(),
    });
    if let Err(error) = task_registry.mark_worker_spawned(&agent_id, 0) {
        let _ = task_registry.fail(&agent_id, error.clone());
        return tool_types::ToolResult::failed(tool_request, error, None);
    }
    match spawn_async_subagent_worker(
        config,
        cwd,
        &child_cwd,
        task_registry.session_id(),
        &agent_id,
        &request,
        subagent_depth + 1,
        worktree.as_ref(),
    ) {
        Ok(pid) => {
            let _ = task_registry.mark_worker_spawned(&agent_id, pid);
            std::mem::forget(worktree_guard);
        }
        Err(error) => {
            let worktree = worktree_guard.and_then(|guard| guard.finish().ok());
            let mut error = format!("failed to start async subagent worker: {error}");
            append_worktree_outcome(&mut error, worktree.as_ref());
            let _ = task_registry.fail(&agent_id, error.clone());
            return tool_types::ToolResult::failed(tool_request, error, None);
        }
    }

    let output = serde_json::json!({
        "status": "async_launched",
        "agent_id": agent_id,
        "description": request.description,
    })
    .to_string();
    tool_types::ToolResult::completed(tool_request, output, false)
}

#[allow(clippy::too_many_arguments)]
fn spawn_async_subagent_worker(
    config: &RunConfig,
    cwd: &Path,
    child_cwd: &Path,
    task_session_id: &str,
    agent_id: &str,
    request: &subagent::SubagentRequest,
    child_depth: u32,
    worktree: Option<&AsyncSubagentWorktree>,
) -> Result<u32, String> {
    let current_exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let request_json = serde_json::to_string(request).map_err(|error| error.to_string())?;
    let mut command = ProcessCommand::new(current_exe);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg("subagent-worker")
        .arg("--cwd")
        .arg(cwd)
        .arg("--child-cwd")
        .arg(child_cwd)
        .arg("--provider")
        .arg(config.provider.as_str())
        .arg("--session-id")
        .arg(task_session_id)
        .arg("--agent-id")
        .arg(agent_id)
        .arg("--subagent-depth")
        .arg(child_depth.to_string())
        .arg("--request-json")
        .arg(request_json);
    if let Some(model) = config.model.as_history_value() {
        command.arg("--model").arg(model);
    }
    if let Some(api_key) = config.api_key.as_deref() {
        command.arg("--api-key").arg(api_key);
    }
    if let Some(base_url) = config.base_url.as_deref() {
        command.arg("--base-url").arg(base_url);
    }
    if let Some(worktree) = worktree {
        command
            .arg("--worktree-repo-root")
            .arg(&worktree.repo_root)
            .arg("--worktree-path")
            .arg(&worktree.path);
    }
    command
        .spawn()
        .map(|child| child.id())
        .map_err(|error| error.to_string())
}

fn append_worktree_outcome(output: &mut String, outcome: Option<&WorktreeOutcome>) {
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

fn validate_subagent_output_schema(
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

fn usage_totals_if_non_empty(usage: UsageTotals) -> Option<UsageTotals> {
    if usage.total_tokens() == 0 && usage.cache_tokens == 0 && usage.estimated_cost_usd == 0.0 {
        None
    } else {
        Some(usage)
    }
}

fn async_subagent_result_payload(output: String, task: Option<serde_json::Value>) -> String {
    serde_json::json!({
        "output": output,
        "task": task,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use std::io;

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
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types;
    use orca_mcp::McpRegistry;

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
}
