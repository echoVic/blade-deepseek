use std::fs;
use std::path::Path;

use crate::tools::{ToolRequest, ToolResult, resolve_workspace_path};

pub fn execute(request: &ToolRequest, cwd: &Path) -> ToolResult {
    let (path_str, old, new) = match parse_edit_args(request) {
        Ok(args) => args,
        Err(error) => return ToolResult::failed(request, error, None),
    };

    let path = resolve_workspace_path(cwd, Some(&path_str));
    if !is_inside_workspace(cwd, &path) {
        return ToolResult::failed(request, "edit target is outside the workspace", None);
    }

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

    let matches = contents.matches(&*old).count();
    if matches == 0 {
        return ToolResult::failed(request, "edit old text was not found", None);
    }
    if matches > 1 {
        return ToolResult::failed(request, "edit old text matched multiple locations", None);
    }

    let updated = contents.replacen(&*old, &new, 1);
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

fn parse_edit_args(request: &ToolRequest) -> Result<(String, String, String), String> {
    if let Some(raw) = request.raw_arguments.as_deref() {
        let args: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("invalid edit arguments: {e}"))?;
        let path = args["path"]
            .as_str()
            .ok_or("edit requires 'path' argument")?
            .to_string();
        let old_text = args["old_text"]
            .as_str()
            .ok_or("edit requires 'old_text' argument")?
            .to_string();
        let new_text = args["new_text"]
            .as_str()
            .ok_or("edit requires 'new_text' argument")?
            .to_string();
        return Ok((path, old_text, new_text));
    }

    let spec = request
        .target
        .as_deref()
        .ok_or("edit spec is required")?;
    let (path_part, replacement_part) = spec
        .split_once("::")
        .ok_or("edit spec must be: <path> :: <old> => <new>")?;
    let (old, new) = replacement_part
        .split_once("=>")
        .ok_or("edit replacement must be: <old> => <new>")?;

    Ok((
        path_part.trim().to_string(),
        old.trim().to_string(),
        new.trim().to_string(),
    ))
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
