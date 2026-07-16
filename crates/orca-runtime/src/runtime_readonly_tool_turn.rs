use std::io;
use std::path::Path;

use orca_core::cancel::CancelToken;
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::hook_types::HookEvent;
use orca_core::tool_types::{ToolOutputTruncation, ToolRequest, ToolResult, ToolStatus};
use orca_mcp::McpRegistry;

use crate::hooks::{HookContext, HookRunError, HookRunner};
use crate::session::record_tool_result_for_agent;
use crate::thread_store::SessionWriter;
use crate::tool_invocation::{
    apply_pre_tool_outcome_with_external, prepare_tool_invocation_with_external,
};
use crate::tool_turn::ToolTurnOutcome;

pub(crate) struct RuntimeReadonlyBatchContext<'a, W: io::Write> {
    pub(crate) cwd: &'a Path,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) tool_requests: &'a [ToolRequest],
    pub(crate) emit_deltas: bool,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) output_truncation: ToolOutputTruncation,
}

pub(crate) struct RuntimeReadonlyToolTurnContext<'a, W: io::Write> {
    pub(crate) request: RuntimeReadonlyToolTurnRequest<'a>,
    pub(crate) io: RuntimeReadonlyToolTurnIo<'a, W>,
    pub(crate) services: RuntimeReadonlyToolTurnServices<'a>,
}

pub(crate) struct RuntimeReadonlyToolTurnRequest<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) tool_requests: &'a [ToolRequest],
    pub(crate) emit_deltas: bool,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) output_truncation: ToolOutputTruncation,
}

pub(crate) struct RuntimeReadonlyToolTurnIo<'a, W: io::Write> {
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
}

pub(crate) struct RuntimeReadonlyToolTurnServices<'a> {
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
}

pub(crate) struct RuntimeReadonlyBatchExecution {
    results: Vec<ToolResult>,
    event_error: Option<io::Error>,
}

fn retain_first_event_error(slot: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result
        && slot.is_none()
    {
        *slot = Some(error);
    }
}

pub(crate) fn execute_readonly_batch<W: io::Write>(
    context: RuntimeReadonlyBatchContext<'_, W>,
) -> RuntimeReadonlyBatchExecution {
    let RuntimeReadonlyBatchContext {
        cwd,
        events,
        sink,
        tool_requests,
        emit_deltas,
        mcp_registry,
        hooks,
        cancel,
        output_truncation,
    } = context;
    let mut early_results: Vec<Option<ToolResult>> = vec![None; tool_requests.len()];
    let mut runnable = Vec::new();
    let mut event_error = None;

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        if emit_deltas {
            retain_first_event_error(
                &mut event_error,
                sink.emit(events.tool_call_requested(tool_request)),
            );
        }
        if cancel.is_cancelled() {
            early_results[idx] = Some(ToolResult::cancelled_before_start(
                tool_request,
                "the read-only batch was cancelled before dispatch",
            ));
            continue;
        }
        let invocation =
            prepare_tool_invocation_with_external(tool_request, 0, u32::MAX, mcp_registry, &[]);
        match hooks.run_with_cancel_result(
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
            Ok(outcome) => {
                match apply_pre_tool_outcome_with_external(invocation, &outcome, mcp_registry, &[])
                {
                    Ok(invocation) => runnable.push((idx, invocation.effective)),
                    Err(error) => early_results[idx] = Some(error.into_result()),
                }
            }
            Err(error) => {
                early_results[idx] = Some(match error {
                    HookRunError::Cancelled(_) => ToolResult::cancelled_before_start(
                        tool_request,
                        "the pre-tool hook was cancelled",
                    ),
                    HookRunError::Failed(error) => ToolResult::failed_before_start(
                        tool_request,
                        format!("pre_tool_use hook blocked tool: {error}"),
                        None,
                    ),
                });
            }
        }
    }

    let runnable_requests = runnable
        .iter()
        .map(|(_, request)| request.clone())
        .collect::<Vec<_>>();
    let dense_runnable = runnable_requests.iter().cloned().enumerate().collect();
    let runnable_results = orca_tools::run_readonly_batch_parallel_with_policy_or_cancel(
        &runnable_requests,
        dense_runnable,
        cwd,
        mcp_registry,
        output_truncation,
        || cancel.is_cancelled(),
    );

    let mut results = early_results;
    for ((original_idx, _), result) in runnable.into_iter().zip(runnable_results) {
        results[original_idx] = Some(result);
    }
    let results = results
        .into_iter()
        .map(|result| result.expect("each read-only batch item has a result"))
        .collect::<Vec<_>>();

    for (tool_request, result) in tool_requests.iter().zip(results.iter()) {
        if emit_deltas {
            retain_first_event_error(
                &mut event_error,
                sink.emit(events.tool_call_completed(result)),
            );
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
                retain_first_event_error(
                    &mut event_error,
                    sink.emit(events.error(&format!("post_tool_use hook failed: {error}"))),
                );
            }
        }
    }

    RuntimeReadonlyBatchExecution {
        results,
        event_error,
    }
}

pub(crate) fn should_run_readonly_batch(
    max_read_parallel: usize,
    tool_request: &ToolRequest,
) -> bool {
    orca_tools::should_run_readonly_batch(max_read_parallel, tool_request)
}

pub(crate) fn collect_readonly_batch(
    max_read_parallel: usize,
    tool_requests: &[ToolRequest],
    start: usize,
) -> usize {
    orca_tools::collect_readonly_batch(max_read_parallel, tool_requests, start)
}

pub(crate) fn record_readonly_batch_results(
    conversation: &mut Conversation,
    mut history_writer: Option<&mut SessionWriter>,
    results: Vec<ToolResult>,
    emit_deltas: bool,
) -> io::Result<()> {
    let mut record_error = None;
    for result in results {
        if let Err(error) = record_tool_result_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            &result,
            emit_deltas,
        ) && record_error.is_none()
        {
            record_error = Some(error);
        }
    }
    match record_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub(crate) fn run_readonly_tool_turn<W: io::Write>(
    context: RuntimeReadonlyToolTurnContext<'_, W>,
) -> io::Result<ToolTurnOutcome> {
    let RuntimeReadonlyToolTurnContext {
        request,
        io,
        services,
    } = context;
    let RuntimeReadonlyToolTurnRequest {
        cwd,
        tool_requests,
        emit_deltas,
        cancel,
        output_truncation,
    } = request;
    let RuntimeReadonlyToolTurnIo {
        events,
        sink,
        conversation,
        history_writer,
    } = io;
    let RuntimeReadonlyToolTurnServices {
        mcp_registry,
        hooks,
    } = services;
    let execution = execute_readonly_batch(RuntimeReadonlyBatchContext {
        cwd,
        events,
        sink,
        tool_requests,
        emit_deltas,
        mcp_registry,
        hooks,
        cancel,
        output_truncation,
    });
    let cancelled_result = execution
        .results
        .iter()
        .find(|result| result.status == ToolStatus::Cancelled);
    let cancelled_error = cancelled_result.and_then(|result| result.error.clone());
    let turn_cancelled = cancel.is_cancelled() || cancelled_result.is_some();

    record_readonly_batch_results(conversation, history_writer, execution.results, emit_deltas)?;
    if let Some(error) = execution.event_error {
        return Err(error);
    }
    if turn_cancelled {
        return Ok(ToolTurnOutcome::Return {
            status: RunStatus::Cancelled,
            error: cancelled_error.or_else(|| Some("read-only tool turn cancelled".to_string())),
        });
    }
    Ok(ToolTurnOutcome::Continue)
}
