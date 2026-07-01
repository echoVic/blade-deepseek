use crate::shell_session::ShellTerminalMode;

use super::wire::WireParams;

pub fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_escape(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':' | '='))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r#"'\''"#))
}

pub(super) fn shell_terminal_mode_from_params(params: &WireParams) -> ShellTerminalMode {
    let cols = params.size.as_ref().map(|size| size.cols).or(params.cols);
    let rows = params.size.as_ref().map(|size| size.rows).or(params.rows);
    match params.terminal_mode.as_deref() {
        Some("pty") => ShellTerminalMode::pty(cols, rows),
        Some("pipe") => ShellTerminalMode::pipe(),
        _ if params.pty || params.tty => ShellTerminalMode::pty(cols, rows),
        _ => ShellTerminalMode::pipe(),
    }
}
