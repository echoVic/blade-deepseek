use std::path::Path;
use std::process::Command;

use crate::tools::{ToolRequest, ToolResult, truncate_output};

pub fn status(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let output = Command::new("git")
        .args(["status", "--short"])
        .current_dir(cwd)
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let text = if stdout.trim().is_empty() {
                "(no changes)".to_string()
            } else {
                stdout.to_string()
            };
            let (text, truncated) = truncate_output(text, max_bytes);
            ToolResult::completed(request, text, truncated)
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            ToolResult::failed(request, stderr.trim().to_string(), output.status.code())
        }
        Err(error) => {
            ToolResult::failed(request, format!("failed to run git status: {error}"), None)
        }
    }
}
