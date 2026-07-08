use std::path::Path;
use std::process::Command;

use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind, truncate_output};
use serde::Deserialize;

const DEFAULT_GREP_HEAD_LIMIT: usize = 250;

#[derive(Default, Deserialize)]
struct GrepArgs {
    pattern: Option<String>,
    path: Option<String>,
    head_limit: Option<usize>,
    offset: Option<usize>,
}

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let args = parse_args(request);
    let pattern = args.pattern.as_deref().or(request
        .target
        .as_deref()
        .filter(|target| !target.is_empty()));
    let Some(pattern) = pattern else {
        return ToolResult::failed(request, "grep pattern is required", None);
    };

    let search_path = args.path.unwrap_or_else(|| ".".to_string());

    if !cwd.join(&search_path).exists() {
        return ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        );
    }

    let output = Command::new("rg")
        .args(["--line-number", "--no-heading", pattern, &search_path])
        .current_dir(cwd)
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stdout = paginate_output(
                stdout.lines().map(String::from).collect::<Vec<_>>(),
                args.offset.unwrap_or(0),
                args.head_limit,
                DEFAULT_GREP_HEAD_LIMIT,
            );
            let (stdout, truncated) = truncate_output(stdout, max_bytes);
            ToolResult::completed(request, stdout, truncated)
        }
        Ok(output) if output.status.code() == Some(1) => ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        ),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            ToolResult::failed(request, stderr, output.status.code())
        }
        Err(error) => ToolResult::failed(request, format!("failed to run rg: {error}"), None),
    }
}

fn parse_args(request: &ToolRequest) -> GrepArgs {
    request
        .raw_arguments
        .as_deref()
        .and_then(|raw| serde_json::from_str::<GrepArgs>(raw).ok())
        .unwrap_or_default()
}

fn paginate_output(
    lines: Vec<String>,
    offset: usize,
    head_limit: Option<usize>,
    default_limit: usize,
) -> String {
    if head_limit == Some(0) {
        return lines.get(offset..).unwrap_or_default().to_vec().join("\n");
    }

    let total = lines.len();
    let limit = head_limit.unwrap_or(default_limit);
    let page = lines
        .iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let next_offset = (total.saturating_sub(offset) > limit).then_some(offset + limit);

    let mut output = page.join("\n");
    if let Some(next_offset) = next_offset {
        let notice = if offset == 0 {
            format!("[Showing first {limit} results; use offset={next_offset} to continue]")
        } else {
            let end = next_offset.min(total);
            format!(
                "[Showing results {}-{} of {total}; use offset={next_offset} to continue]",
                offset + 1,
                end
            )
        };
        if output.is_empty() {
            output = notice;
        } else {
            output.push('\n');
            output.push_str(&notice);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolResultKind, ToolStatus};

    use super::*;

    #[test]
    fn missing_search_path_completes_with_no_matches() {
        let cwd = temp_dir("grep-missing");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        let request = ToolRequest {
            id: "grep-1".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some("needle".to_string()),
            raw_arguments: Some(r#"{"path":"tests/fixtures"}"#.to_string()),
        };

        let result = execute(&request, &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.kind, ToolResultKind::NoMatches);
        assert_eq!(result.output.as_deref(), Some("(no matches)"));
        assert_eq!(result.error, None);
    }

    #[test]
    fn grep_defaults_to_first_250_results() {
        let cwd = temp_dir("grep-default-page");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        let contents = (0..300)
            .map(|index| format!("needle {index:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(cwd.join("notes.txt"), contents).expect("write fixture");
        let request = ToolRequest {
            id: "grep-1".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some("needle".to_string()),
            raw_arguments: Some(r#"{"pattern":"needle","path":"notes.txt"}"#.to_string()),
        };

        let result = execute(&request, &cwd, 100_000);
        let output = result.output.as_deref().expect("grep output");
        let lines = output.lines().collect::<Vec<_>>();

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(lines.len(), 251);
        assert!(lines[0].contains("needle 000"));
        assert!(lines[249].contains("needle 249"));
        assert_eq!(
            lines[250],
            "[Showing first 250 results; use offset=250 to continue]"
        );
    }

    #[test]
    fn grep_respects_explicit_offset_and_head_limit() {
        let cwd = temp_dir("grep-offset-page");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        let contents = (0..300)
            .map(|index| format!("needle {index:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(cwd.join("notes.txt"), contents).expect("write fixture");
        let request = ToolRequest {
            id: "grep-1".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some("needle".to_string()),
            raw_arguments: Some(
                r#"{"pattern":"needle","path":"notes.txt","head_limit":10,"offset":250}"#
                    .to_string(),
            ),
        };

        let result = execute(&request, &cwd, 100_000);
        let output = result.output.as_deref().expect("grep output");
        let lines = output.lines().collect::<Vec<_>>();

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(lines.len(), 11);
        assert!(lines[0].contains("needle 250"));
        assert!(lines[9].contains("needle 259"));
        assert_eq!(
            lines[10],
            "[Showing results 251-260 of 300; use offset=260 to continue]"
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
