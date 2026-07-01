use std::io::{self, Write};
use std::sync::{Arc, Mutex};

#[path = "processors/mod.rs"]
mod processors;

use super::*;
use processors::{command_exec, permission, shell, thread, turn};

pub(super) fn dispatch_submission<W: Write + Send + 'static>(
    config: &ServerConfig,
    state: &mut ServerState,
    submission: Submission,
    writer: Arc<Mutex<W>>,
) -> io::Result<()> {
    if thread::is_query_operation(&submission.op) {
        let mut writer = writer.lock().map_err(lock_error)?;
        return thread::dispatch_query_operation(
            state,
            &submission.op,
            submission.id.clone(),
            &mut *writer,
        );
    }

    if turn::is_control_operation(&submission.op) {
        return turn::dispatch_control_operation(
            state,
            &submission.op,
            submission.id.clone(),
            writer,
        );
    }

    if shell::is_shell_operation(&submission.op) {
        let mut writer = writer.lock().map_err(lock_error)?;
        return shell::dispatch_shell_operation(
            config,
            state,
            &submission.op,
            submission.id.clone(),
            &mut *writer,
        );
    }

    if command_exec::is_command_exec_operation(&submission.op) {
        let mut writer = writer.lock().map_err(lock_error)?;
        return command_exec::dispatch_command_exec_operation(
            config,
            state,
            &submission.op,
            submission.id.clone(),
            &mut *writer,
        );
    }

    if permission::is_permission_operation(&submission.op) {
        let mut writer = writer.lock().map_err(lock_error)?;
        return permission::dispatch_permission_operation(
            config,
            state,
            &submission.op,
            submission.id.clone(),
            &mut *writer,
        );
    }

    let result = match &submission.op {
        ClientOp::Submit { thread_id, .. } => {
            if let Some(thread_id) = thread_id {
                if !state.threads.has_thread(thread_id)
                    && !state
                        .active_turns
                        .values()
                        .any(|turn| turn.thread_id == *thread_id)
                {
                    protocol::write_server_event(
                        &mut *writer.lock().map_err(lock_error)?,
                        &submission.id,
                        ServerEvent::error(format!("unknown thread: {thread_id}")),
                    )?;
                    return Ok(());
                }
                run_thread_submit_async(config, state, submission.id, submission.op, writer)
            } else {
                let mut writer = writer.lock().map_err(lock_error)?;
                run_submit(config, submission.id, submission.op, &mut *writer)
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
                submission.id,
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
                submission.id.clone(),
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
                submission.id.clone(),
                &mut *writer,
            )
        }
        _ => unreachable!(
            "thread query, turn control, shell, command exec, and permission operations are delegated before router dispatch"
        ),
    };
    result
}
