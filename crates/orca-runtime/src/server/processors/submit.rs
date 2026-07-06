use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::super::*;

pub(in crate::server::router) fn is_submit_operation(op: &ClientOp) -> bool {
    matches!(
        op,
        ClientOp::Submit { .. }
            | ClientOp::ThreadStart { .. }
            | ClientOp::ThreadResume { .. }
            | ClientOp::ThreadFork { .. }
    )
}

pub(in crate::server::router) fn dispatch_submit_operation<W: Write + Send + 'static>(
    config: &ServerConfig,
    state: &mut ServerState,
    op: ClientOp,
    id: Value,
    writer: Arc<Mutex<W>>,
) -> io::Result<()> {
    match &op {
        ClientOp::Submit { thread_id, .. } => {
            if let Some(thread_id) = thread_id {
                if !state.threads.has_thread(thread_id) && !state.active_turns.has_thread(thread_id)
                {
                    protocol::write_server_event(
                        &mut *writer.lock().map_err(lock_error)?,
                        &id,
                        ServerEvent::error(format!("unknown thread: {thread_id}")),
                    )?;
                    return Ok(());
                }
                run_thread_submit_async(config, state, id, op, writer)
            } else {
                let mut writer = writer.lock().map_err(lock_error)?;
                run_submit(config, id, op, &mut *writer)
            }
        }
        ClientOp::ThreadStart {
            runtime_workspace_roots,
        } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_thread_start(
                config,
                state,
                runtime_workspace_roots.clone(),
                id,
                &mut *writer,
            )
        }
        ClientOp::ThreadResume {
            thread_id,
            permissions,
        } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_thread_resume(
                config,
                state,
                thread_id,
                permissions.clone(),
                id.clone(),
                &mut *writer,
            )
        }
        ClientOp::ThreadFork {
            thread_id,
            permissions,
        } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_thread_fork(
                config,
                state,
                thread_id,
                permissions.clone(),
                id.clone(),
                &mut *writer,
            )
        }
        _ => unreachable!("only submit-family operations can reach the submit processor"),
    }
}
