use std::path::Path;
use std::process::Command;

use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind, truncate_output};

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
