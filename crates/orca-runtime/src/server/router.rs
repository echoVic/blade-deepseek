use std::io::{self, Write};
use std::sync::{Arc, Mutex};

#[path = "processors/mod.rs"]
mod processors;

use super::*;
use processors::{shell, thread, turn};

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
        ClientOp::PermissionRespond {
            request_id,
            decision,
            scope,
            permissions,
            strict_auto_review,
        } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_permission_respond(
                config,
                state,
                request_id,
                *decision,
                *scope,
                permissions.clone(),
                *strict_auto_review,
                submission.id.clone(),
                &mut *writer,
            )
        }
        ClientOp::CommandExec {
            thread_id,
            command,
            process_id,
            cwd,
            env,
            options,
            terminal,
        } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_command_exec(
                config,
                state,
                thread_id.as_deref(),
                command,
                process_id.as_deref(),
                cwd.as_ref(),
                env,
                options,
                *terminal,
                submission.id.clone(),
                &mut *writer,
            )
        }
        ClientOp::CommandExecWrite {
            process_id,
            delta_base64,
            close_stdin,
        } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_command_exec_write(
                state,
                process_id,
                delta_base64.as_deref(),
                *close_stdin,
                submission.id.clone(),
                &mut *writer,
            )
        }
        ClientOp::CommandExecResize {
            process_id,
            cols,
            rows,
        } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_command_exec_resize(
                state,
                process_id,
                *cols,
                *rows,
                submission.id.clone(),
                &mut *writer,
            )
        }
        ClientOp::CommandExecTerminate { process_id } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_command_exec_terminate(state, process_id, submission.id.clone(), &mut *writer)
        }
        _ => unreachable!(
            "thread query, turn control, and shell operations are delegated before router dispatch"
        ),
    };
    result
}
