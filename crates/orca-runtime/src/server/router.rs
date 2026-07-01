use std::io::{self, Write};
use std::sync::{Arc, Mutex};

#[path = "processors/mod.rs"]
mod processors;

use super::*;
use processors::thread;

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
        ClientOp::TurnInterrupt { thread_id, turn_id } => run_turn_control(
            state,
            "interrupt",
            thread_id.as_deref(),
            turn_id,
            None,
            submission.id.clone(),
            writer,
        ),
        ClientOp::TurnResume { thread_id, turn_id } => run_turn_control(
            state,
            "resume",
            thread_id.as_deref(),
            turn_id,
            None,
            submission.id.clone(),
            writer,
        ),
        ClientOp::TurnSteer {
            thread_id,
            turn_id,
            input,
        } => run_turn_control(
            state,
            "steer",
            thread_id.as_deref(),
            turn_id,
            Some(input),
            submission.id.clone(),
            writer,
        ),
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
        ClientOp::ShellStart {
            thread_id,
            command,
            description,
            terminal,
        } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_shell_start(
                config,
                state,
                thread_id.as_deref(),
                command,
                description.clone(),
                *terminal,
                submission.id.clone(),
                &mut *writer,
            )
        }
        ClientOp::ShellWrite { shell_id, input } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_shell_write(state, shell_id, input, submission.id.clone(), &mut *writer)
        }
        ClientOp::ShellUpdate {
            shell_id,
            description,
        } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_shell_update(
                state,
                shell_id,
                description.as_deref(),
                submission.id.clone(),
                &mut *writer,
            )
        }
        ClientOp::ShellClose { shell_id } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_shell_close(state, shell_id, submission.id.clone(), &mut *writer)
        }
        ClientOp::ShellResize {
            shell_id,
            cols,
            rows,
        } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_shell_resize(
                state,
                shell_id,
                *cols,
                *rows,
                submission.id.clone(),
                &mut *writer,
            )
        }
        ClientOp::ShellList => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_shell_list(state, submission.id.clone(), &mut *writer)
        }
        ClientOp::ShellRead {
            shell_id,
            timeout_ms,
        } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_shell_read(
                state,
                shell_id,
                *timeout_ms,
                submission.id.clone(),
                &mut *writer,
            )
        }
        ClientOp::ShellKill { shell_id } => {
            let mut writer = writer.lock().map_err(lock_error)?;
            run_shell_kill(state, shell_id, submission.id.clone(), &mut *writer)
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
        _ => unreachable!("thread query operations are delegated before router dispatch"),
    };
    result
}
