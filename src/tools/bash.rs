use std::path::Path;
use std::process::Command;

use crate::tools::{ToolRequest, ToolResult, truncate_output};

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let Some(command) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "bash command is required", None);
    };

    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string();
            let (stdout, truncated) = truncate_output(stdout, max_bytes);
            ToolResult::completed(request, stdout, truncated)
        }
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string();
            let stderr = String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_string();
            let message = if stderr.is_empty() {
                stdout
            } else if stdout.is_empty() {
                stderr
            } else {
                format!("{stdout}\n{stderr}")
            };
            let (message, truncated) = truncate_output(message, max_bytes);
            let mut result = ToolResult::failed(request, message, output.status.code());
            result.truncated = truncated;
            result
        }
        Err(error) => ToolResult::failed(
            request,
            format!("failed to run shell command: {error}"),
            None,
        ),
    }
}
