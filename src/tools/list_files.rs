use std::fs;
use std::path::Path;

use crate::tools::{ToolRequest, ToolResult, resolve_workspace_path, truncate_output};

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let path = match resolve_workspace_path(cwd, request.target.as_deref()) {
        Ok(p) => p,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to list {}: {error}", path.display()),
                None,
            );
        }
    };

    let mut names = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => names.push(entry.file_name().to_string_lossy().to_string()),
            Err(error) => {
                return ToolResult::failed(request, format!("failed to read entry: {error}"), None);
            }
        }
    }
    names.sort();

    let (output, truncated) = truncate_output(names.join("\n"), max_bytes);
    ToolResult::completed(request, output, truncated)
}
