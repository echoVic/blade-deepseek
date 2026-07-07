use std::io::{self, Write};

use serde_json::Value;

use super::super::*;

pub(in crate::server::router) fn is_command_exec_operation(op: &ClientOp) -> bool {
    matches!(
        op,
        ClientOp::CommandExec { .. }
            | ClientOp::CommandExecList
            | ClientOp::CommandExecWrite { .. }
            | ClientOp::CommandExecRead { .. }
            | ClientOp::CommandExecResize { .. }
            | ClientOp::CommandExecTerminate { .. }
    )
}

pub(in crate::server::router) fn dispatch_command_exec_operation<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match op {
        ClientOp::CommandExec {
            thread_id,
            command,
            process_id,
            cwd,
            env,
            options,
            terminal,
        } => run_command_exec(
            config,
            state,
            thread_id.as_deref(),
            command,
            process_id.as_deref(),
            cwd.as_ref(),
            env,
            options,
            *terminal,
            id,
            writer,
        ),
        ClientOp::CommandExecList => run_command_exec_list(state, id, writer),
        ClientOp::CommandExecWrite {
            process_id,
            delta_base64,
            close_stdin,
        } => run_command_exec_write(
            state,
            process_id,
            delta_base64.as_deref(),
            *close_stdin,
            id,
            writer,
        ),
        ClientOp::CommandExecRead {
            process_id,
            timeout_ms,
            output_bytes_cap,
        } => run_command_exec_read(
            state,
            process_id,
            *timeout_ms,
            *output_bytes_cap,
            id,
            writer,
        ),
        ClientOp::CommandExecResize {
            process_id,
            cols,
            rows,
        } => run_command_exec_resize(state, process_id, *cols, *rows, id, writer),
        ClientOp::CommandExecTerminate { process_id } => {
            run_command_exec_terminate(state, process_id, id, writer)
        }
        _ => unreachable!("only command exec operations can reach the command exec processor"),
    }
}
