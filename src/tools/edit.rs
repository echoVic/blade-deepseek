use std::fs;
use std::path::Path;

use crate::tools::{ToolRequest, ToolResult, resolve_workspace_path};

pub fn execute(request: &ToolRequest, cwd: &Path) -> ToolResult {
    let Some(spec) = request.target.as_deref() else {
        return ToolResult::failed(request, "edit spec is required", None);
    };

    let Some((path_part, replacement_part)) = spec.split_once("::") else {
        return ToolResult::failed(request, "edit spec must be: <path> :: <old> => <new>", None);
    };
    let Some((old, new)) = replacement_part.split_once("=>") else {
        return ToolResult::failed(request, "edit replacement must be: <old> => <new>", None);
    };

    let path = resolve_workspace_path(cwd, Some(path_part.trim()));
    if !is_inside_workspace(cwd, &path) {
        return ToolResult::failed(request, "edit target is outside the workspace", None);
    }

    let old = old.trim();
    let new = new.trim();
    if old.is_empty() {
        return ToolResult::failed(request, "edit old text cannot be empty", None);
    }

    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to read {}: {error}", path.display()),
                None,
            );
        }
    };

    let matches = contents.matches(old).count();
    if matches == 0 {
        return ToolResult::failed(request, "edit old text was not found", None);
    }
    if matches > 1 {
        return ToolResult::failed(request, "edit old text matched multiple locations", None);
    }

    let updated = contents.replacen(old, new, 1);
    if let Err(error) = fs::write(&path, updated) {
        return ToolResult::failed(
            request,
            format!("failed to write {}: {error}", path.display()),
            None,
        );
    }

    ToolResult::completed(
        request,
        format!(
            "edited {}",
            path.strip_prefix(cwd).unwrap_or(&path).display()
        ),
        false,
    )
}

fn is_inside_workspace(cwd: &Path, path: &Path) -> bool {
    let Ok(workspace) = fs::canonicalize(cwd) else {
        return false;
    };
    let parent = path.parent().unwrap_or(path);
    let Ok(parent) = fs::canonicalize(parent) else {
        return false;
    };

    parent.starts_with(workspace)
}
