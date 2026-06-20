use std::fs;
use std::path::{Path, PathBuf};

use orca_core::tool_types::{ToolRequest, ToolResult};

pub fn execute(request: &ToolRequest, cwd: &Path) -> ToolResult {
    let raw = match &request.raw_arguments {
        Some(r) => r,
        None => return ToolResult::failed(request, "missing arguments", None),
    };

    let args: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return ToolResult::failed(request, format!("invalid arguments: {e}"), None),
    };

    let path_str = match args["path"].as_str() {
        Some(p) => p,
        None => return ToolResult::failed(request, "missing required parameter: path", None),
    };

    let content = match args["content"].as_str() {
        Some(c) => c,
        None => return ToolResult::failed(request, "missing required parameter: content", None),
    };

    let canonical_cwd = match cwd.canonicalize() {
        Ok(p) => p,
        Err(e) => return ToolResult::failed(request, format!("cannot resolve cwd: {e}"), None),
    };

    let joined = canonical_cwd.join(path_str);

    // Normalize by resolving ".." components without filesystem access
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            _ => normalized.push(component),
        }
    }

    if !normalized.starts_with(&canonical_cwd) {
        return ToolResult::failed(
            request,
            format!("path escapes workspace: {}", path_str),
            None,
        );
    }

    if let Some(parent) = normalized.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ToolResult::failed(request, format!("failed to create directories: {e}"), None);
        }
    }

    match fs::write(&normalized, content) {
        Ok(()) => {
            let bytes = content.len();
            ToolResult::completed(
                request,
                format!("wrote {} bytes to {}", bytes, path_str),
                false,
            )
        }
        Err(e) => ToolResult::failed(request, format!("failed to write file: {e}"), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolStatus};
    use tempfile::TempDir;

    fn make_request(path: &str, content: &str) -> ToolRequest {
        ToolRequest {
            id: "test-1".to_string(),
            name: ToolName::WriteFile,
            action: ActionKind::Write,
            target: Some(path.to_string()),
            raw_arguments: Some(
                serde_json::json!({ "path": path, "content": content }).to_string(),
            ),
        }
    }

    #[test]
    fn write_creates_file() {
        let dir = TempDir::new().unwrap();
        let req = make_request("hello.txt", "world");
        let result = execute(&req, dir.path());
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn write_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let req = make_request("a/b/c.txt", "nested");
        let result = execute(&req, dir.path());
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "nested"
        );
    }

    #[test]
    fn write_rejects_path_escape() {
        let dir = TempDir::new().unwrap();
        let req = make_request("../escape.txt", "bad");
        let result = execute(&req, dir.path());
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.error.unwrap().contains("escapes workspace"));
    }
}
