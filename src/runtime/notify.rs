use std::process::Command;

pub fn notify(title: &str, message: &str) -> Result<(), String> {
    match std::env::consts::OS {
        "macos" => notify_macos(title, message),
        "linux" => notify_linux(title, message),
        _ => Err("desktop notifications are not supported on this platform".to_string()),
    }
}

fn notify_macos(title: &str, message: &str) -> Result<(), String> {
    let script = format!(
        "display notification {} with title {}",
        applescript_string(message),
        applescript_string(title)
    );
    run(Command::new("osascript").arg("-e").arg(script))
}

fn notify_linux(title: &str, message: &str) -> Result<(), String> {
    run(Command::new("notify-send").arg(title).arg(message))
}

fn run(command: &mut Command) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|error| format!("failed to send notification: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("notification command exited with {status}"))
    }
}

fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_string_escapes_quotes() {
        assert_eq!(applescript_string("a \"quote\""), "\"a \\\"quote\\\"\"");
    }
}
