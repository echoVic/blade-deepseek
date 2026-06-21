use std::path::{Path, PathBuf};
use std::thread;

use orca_core::approval_types::ActionKind;
use orca_core::external_config::ExternalToolConfig;
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;

pub mod bash;
pub mod edit;
pub mod external;
pub mod git;
pub mod grep;
pub mod list_files;
pub mod read_file;
pub mod registry;
pub mod sandbox;
pub mod update_goal;
pub mod update_plan;
pub mod web_search;
pub mod write_file;

pub use registry::{Tool, ToolContext, ToolRegistry};

pub fn execute_with_mcp(
    request: &ToolRequest,
    cwd: &Path,
    mcp_registry: &McpRegistry,
) -> ToolResult {
    execute_with_mcp_and_external(request, cwd, mcp_registry, &[])
}

pub fn execute_with_mcp_and_external(
    request: &ToolRequest,
    cwd: &Path,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> ToolResult {
    if !matches!(&request.name, ToolName::Mcp(_)) {
        if external_tools.is_empty() {
            let reg = registry::default_tool_registry();
            let ctx = registry::ToolContext::new(cwd);
            return reg.execute(request, &ctx);
        }
        let reg = registry::tool_registry_with_mcp_and_external(None, external_tools);
        let ctx = registry::ToolContext::new(cwd);
        return reg.execute(request, &ctx);
    }

    let reg = registry::tool_registry_with_mcp_and_external(Some(mcp_registry), external_tools);
    let ctx = registry::ToolContext::new(cwd).with_mcp(mcp_registry);
    reg.execute(request, &ctx)
}

fn is_concurrent_safe_read(request: &ToolRequest) -> bool {
    if request.action != ActionKind::Read {
        return false;
    }

    let reg = registry::default_tool_registry();
    reg.get(request.name.as_str())
        .map(|tool| tool.is_concurrent_safe(request))
        .unwrap_or_else(|| request.name.is_read_only())
}

pub fn should_run_readonly_batch(max_read_parallel: usize, tool_request: &ToolRequest) -> bool {
    is_concurrent_safe_read(tool_request) && max_read_parallel > 1
}

pub fn collect_readonly_batch(
    max_read_parallel: usize,
    tool_requests: &[ToolRequest],
    start: usize,
) -> usize {
    let max_end = (start + max_read_parallel).min(tool_requests.len());
    let mut end = start;
    while end < max_end && is_concurrent_safe_read(&tool_requests[end]) {
        end += 1;
    }
    end
}

pub fn run_readonly_batch_parallel(
    tool_requests: &[ToolRequest],
    runnable: Vec<(usize, ToolRequest)>,
    cwd: &Path,
    mcp_registry: &McpRegistry,
) -> Vec<ToolResult> {
    let mut results: Vec<Option<ToolResult>> = vec![None; tool_requests.len()];
    let cwd = cwd.to_path_buf();
    let mcp_registry = mcp_registry.clone();

    thread::scope(|scope| {
        let mut handles = Vec::new();
        for (idx, tool_request) in runnable {
            let cwd = cwd.clone();
            let mcp_registry = mcp_registry.clone();
            handles.push((
                idx,
                scope.spawn(move || execute_with_mcp(&tool_request, &cwd, &mcp_registry)),
            ));
        }

        for (idx, handle) in handles {
            results[idx] = Some(match handle.join() {
                Ok(result) => result,
                Err(_) => {
                    ToolResult::failed(&tool_requests[idx], "read-only tool thread panicked", None)
                }
            });
        }
    });

    results
        .into_iter()
        .map(|result| result.expect("each read-only batch item has a result"))
        .collect()
}

pub fn resolve_workspace_path(cwd: &Path, target: Option<&str>) -> Result<PathBuf, String> {
    let target = target.unwrap_or(".");
    let candidate = PathBuf::from(target);
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    };

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

    if !normalized.starts_with(cwd) {
        return Err(format!("path escapes workspace: {target}"));
    }

    if normalized.exists() {
        let canonical = normalized
            .canonicalize()
            .map_err(|e| format!("cannot resolve path: {e}"))?;
        let canonical_cwd = cwd
            .canonicalize()
            .map_err(|e| format!("cannot resolve cwd: {e}"))?;
        if !canonical.starts_with(&canonical_cwd) {
            return Err(format!("path escapes workspace via symlink: {target}"));
        }
    }

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::tool_types::{ToolStatus, truncate_output};
    use std::fs;

    #[test]
    fn micro_compact_preserves_head_and_tail() {
        let output = format!("{}{}{}", "a".repeat(80), "middle", "z".repeat(80));
        let (truncated, was_truncated) = truncate_output(output, 80);
        assert!(was_truncated);
        assert!(truncated.starts_with("aaaa"));
        assert!(truncated.contains("micro-compacted"));
        assert!(truncated.ends_with("zzzz"));
    }

    #[test]
    fn default_registry_exposes_builtin_tool_metadata() {
        let reg = registry::default_tool_registry();

        let tool = reg
            .get("read_file")
            .expect("read_file is registered as a tool");
        let request = ToolRequest {
            id: "read".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("README.md".to_string()),
            raw_arguments: None,
        };

        assert_eq!(tool.name(), "read_file");
        assert_eq!(tool.action_kind(), ActionKind::Read);
        assert!(tool.is_read_only(&request));
        assert!(tool.is_concurrent_safe(&request));
        assert_eq!(reg.iter().next().map(|tool| tool.name()), Some("read_file"));

        assert_eq!(
            reg.get("web_search").unwrap().action_kind(),
            ActionKind::Network
        );
        assert_eq!(
            reg.get("subagent").unwrap().action_kind(),
            ActionKind::Agent
        );
    }

    #[test]
    fn registry_executes_builtin_tool_by_registered_name() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        fs::write(temp_dir.path().join("note.txt"), "hello registry\n").expect("fixture");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "read".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("note.txt".to_string()),
            raw_arguments: None,
        };
        let ctx = registry::ToolContext::new(temp_dir.path());

        let result = reg.execute(&request, &ctx);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("hello registry\n"));
    }
}
