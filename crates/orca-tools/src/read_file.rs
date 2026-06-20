use std::fs;
use std::path::Path;

use orca_core::tool_types::{ToolRequest, ToolResult, truncate_output};

use crate::resolve_workspace_path;

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let path = match resolve_workspace_path(cwd, request.target.as_deref()) {
        Ok(p) => p,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    match fs::read_to_string(&path) {
        Ok(contents) => {
            let (output, truncated) = truncate_output(contents, max_bytes);
            ToolResult::completed(request, output, truncated)
        }
        Err(error) => ToolResult::failed(
            request,
            format!("failed to read {}: {error}", path.display()),
            None,
        ),
    }
}
