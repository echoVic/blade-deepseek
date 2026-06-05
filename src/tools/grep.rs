use std::path::Path;
use std::process::Command;

use crate::tools::{ToolRequest, ToolResult, truncate_output};

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let Some(pattern) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "grep pattern is required", None);
    };

    let output = Command::new("rg")
        .args(["--line-number", "--no-heading", pattern, "."])
        .current_dir(cwd)
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let (stdout, truncated) = truncate_output(stdout, max_bytes);
            ToolResult::completed(request, stdout, truncated)
        }
        Ok(output) if output.status.code() == Some(1) => {
            ToolResult::completed(request, "(no matches)".to_string(), false)
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            ToolResult::failed(request, stderr, output.status.code())
        }
        Err(error) => ToolResult::failed(request, format!("failed to run rg: {error}"), None),
    }
}
