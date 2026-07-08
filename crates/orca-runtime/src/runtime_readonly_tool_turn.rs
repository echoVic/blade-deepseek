use std::io;
use std::path::Path;

use orca_core::conversation::Conversation;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::hook_types::HookEvent;
use orca_core::tool_types::{ToolOutputTruncation, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;

use crate::hooks::{HookContext, HookRunner};
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

pub(crate) fn execute_readonly_batch<W: io::Write>(
    context: RuntimeReadonlyBatchContext<'_, W>,
) -> io::Result<Vec<ToolResult>> {
    let RuntimeReadonlyBatchContext {
        cwd,
        events,
        sink,
        tool_requests,
        emit_deltas,
        mcp_registry,
        hooks,
        output_truncation,
    } = context;
    let mut hook_failed: Vec<Option<ToolResult>> = vec![None; tool_requests.len()];
    let mut runnable = Vec::new();

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        let invocation =
            prepare_tool_invocation_with_external(tool_request, 0, u32::MAX, mcp_registry, &[]);
        if emit_deltas {
            sink.emit(&events.tool_call_requested(tool_request))?;
        }
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
                match apply_pre_tool_outcome_with_external(invocation, &outcome, mcp_registry, &[])
                {
                    Ok(invocation) => runnable.push((idx, invocation.effective)),
                    Err(error) => hook_failed[idx] = Some(error.into_result()),
                }
            }
            Err(error) => {
                hook_failed[idx] = Some(ToolResult::failed(
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
        if emit_deltas {
            sink.emit(&events.tool_call_completed(result))?;
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
                sink.emit(&events.error(&format!("post_tool_use hook failed: {error}")))?;
            }
        }
    }

    Ok(results)
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
    for result in results {
        record_tool_result_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            &result,
            emit_deltas,
        )?;
    }
    Ok(())
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
    let results = execute_readonly_batch(RuntimeReadonlyBatchContext {
        cwd,
        events,
        sink,
        tool_requests,
        emit_deltas,
        mcp_registry,
        hooks,
        output_truncation,
    })?;

    record_readonly_batch_results(conversation, history_writer, results, emit_deltas)?;
    Ok(ToolTurnOutcome::Continue)
}
