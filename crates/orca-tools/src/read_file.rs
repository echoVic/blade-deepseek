use std::fs;
use std::path::Path;

use serde::Deserialize;

use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind, truncate_output};

use crate::resolve_workspace_path;

#[derive(Debug, Default, Deserialize)]
struct ReadFileArgs {
    path: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let args = parse_args(request);
    let target = args.path.as_deref().or(request.target.as_deref());
    let path = match resolve_workspace_path(cwd, target) {
        Ok(p) => p,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    match fs::read_to_string(&path) {
        Ok(contents) => {
            let output = if args.offset.is_some() || args.limit.is_some() {
                render_range(&contents, args.offset.unwrap_or(1).max(1), args.limit)
            } else {
                contents
            };
            let (output, truncated) = truncate_output(output, max_bytes);
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

fn parse_args(request: &ToolRequest) -> ReadFileArgs {
    request
        .raw_arguments
        .as_deref()
        .and_then(|raw| serde_json::from_str::<ReadFileArgs>(raw).ok())
        .unwrap_or_default()
}

fn render_range(contents: &str, offset: usize, limit: Option<usize>) -> String {
    let total = contents.lines().count();
    if offset > total {
        return format!("[file has {total} lines; requested offset {offset} is past end]");
    }
    let take = limit.unwrap_or(usize::MAX);
    contents
        .lines()
        .enumerate()
        .skip(offset.saturating_sub(1))
        .take(take)
        .map(|(idx, line)| format!("{}: {}", idx + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
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

    #[test]
    fn read_file_respects_offset_and_limit() {
        let cwd = temp_dir("read-file-range");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("notes.txt"), "one\ntwo\nthree\nfour\n").expect("write fixture");
        let request = ToolRequest {
            id: "read-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("notes.txt".to_string()),
            raw_arguments: Some(r#"{"path":"notes.txt","offset":2,"limit":2}"#.to_string()),
        };

        let result = execute(&request, &cwd, 1024);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("2: two\n3: three"));
        assert!(!result.truncated);
    }

    #[test]
    fn read_file_reports_short_file_when_offset_is_past_end() {
        let cwd = temp_dir("read-file-short");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("notes.txt"), "one\n").expect("write fixture");
        let request = ToolRequest {
            id: "read-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("notes.txt".to_string()),
            raw_arguments: Some(r#"{"path":"notes.txt","offset":5,"limit":2}"#.to_string()),
        };

        let result = execute(&request, &cwd, 1024);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            result.output.as_deref(),
            Some("[file has 1 lines; requested offset 5 is past end]")
        );
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
