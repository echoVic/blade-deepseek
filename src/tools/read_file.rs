use std::fs;
use std::path::Path;

use crate::tools::{ToolRequest, ToolResult, resolve_workspace_path, truncate_output};

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let path = resolve_workspace_path(cwd, request.target.as_deref());
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
