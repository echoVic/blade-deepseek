use std::io::{self, Write};

use serde_json::Value;

use super::super::*;

pub(in crate::server::router) fn is_shell_operation(op: &ClientOp) -> bool {
    matches!(
        op,
        ClientOp::ShellStart { .. }
            | ClientOp::ShellWrite { .. }
            | ClientOp::ShellUpdate { .. }
            | ClientOp::ShellClose { .. }
            | ClientOp::ShellResize { .. }
            | ClientOp::ShellList
            | ClientOp::ShellRead { .. }
            | ClientOp::ShellKill { .. }
    )
}

pub(in crate::server::router) fn dispatch_shell_operation<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match op {
        ClientOp::ShellStart {
            thread_id,
            command,
            description,
            terminal,
        } => run_shell_start(
            config,
            state,
            thread_id.as_deref(),
            command,
            description.clone(),
            *terminal,
            id,
            writer,
        ),
        ClientOp::ShellWrite { shell_id, input } => {
            run_shell_write(state, shell_id, input, id, writer)
        }
        ClientOp::ShellUpdate {
            shell_id,
            description,
        } => run_shell_update(state, shell_id, description.as_deref(), id, writer),
        ClientOp::ShellClose { shell_id } => run_shell_close(state, shell_id, id, writer),
        ClientOp::ShellResize {
            shell_id,
            cols,
            rows,
        } => run_shell_resize(state, shell_id, *cols, *rows, id, writer),
        ClientOp::ShellList => run_shell_list(state, id, writer),
        ClientOp::ShellRead {
            shell_id,
            timeout_ms,
        } => run_shell_read(state, shell_id, *timeout_ms, id, writer),
        ClientOp::ShellKill { shell_id } => run_shell_kill(state, shell_id, id, writer),
        _ => unreachable!("only shell operations can reach the shell processor"),
    }
}
