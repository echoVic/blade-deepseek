use std::fs;
use std::path::Path;

use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind, truncate_output};

use crate::resolve_workspace_path;

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let path = match resolve_workspace_path(cwd, request.target.as_deref()) {
        Ok(p) => p,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    match fs::read_to_string(&path) {
        Ok(contents) => {
            let (output, truncated) = truncate_output(contents, max_bytes);
            let kind = if truncated {
                ToolResultKind::Truncated
            } else {
                ToolResultKind::Success
            };
            ToolResult::completed_kind(request, output, truncated, kind)
        }
        Err(error) => ToolResult::failed(
            request,
            format!("failed to read {}: {error}", path.display()),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolResultKind, ToolStatus};

    use super::*;

    #[test]
    fn truncated_read_completes_with_truncated_kind() {
        let cwd = temp_dir("read-file-truncated");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("notes.txt"), "abcdef").expect("write fixture");
        let request = ToolRequest {
            id: "read-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("notes.txt".to_string()),
            raw_arguments: None,
        };

        let result = execute(&request, &cwd, 3);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.kind, ToolResultKind::Truncated);
        assert!(result.truncated);
        assert_eq!(result.output.as_deref(), Some("abc"));
    }

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "orca-{prefix}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
