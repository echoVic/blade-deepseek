use std::path::Path;
use std::process::Command;

use orca_core::tool_types::{ToolRequest, ToolResult, truncate_output};

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let Some(pattern) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "grep pattern is required", None);
    };

    let search_path = request
        .raw_arguments
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|args| args["path"].as_str().map(String::from))
        .unwrap_or_else(|| ".".to_string());

    let output = Command::new("rg")
        .args(["--line-number", "--no-heading", pattern, &search_path])
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
