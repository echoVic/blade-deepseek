use std::io::{self, Write};
use std::sync::{Arc, Mutex};

#[path = "processors/mod.rs"]
mod processors;

use super::*;
use processors::{command_exec, permission, shell, submit, thread, turn, user_input};

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

    if user_input::is_user_input_operation(&submission.op) {
        let mut writer = writer.lock().map_err(lock_error)?;
        return user_input::dispatch_user_input_operation(
            state,
            &submission.op,
            submission.id.clone(),
            &mut *writer,
        );
    }

    if submit::is_submit_operation(&submission.op) {
        return submit::dispatch_submit_operation(
            config,
            state,
            submission.op,
            submission.id,
            writer,
        );
    }

    unreachable!(
        "thread query, turn control, shell, command exec, permission, user input, and submit operations are delegated before router dispatch"
    )
}
