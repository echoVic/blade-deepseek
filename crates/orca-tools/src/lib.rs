use std::path::{Path, PathBuf};
use std::thread;

use orca_core::external_config::ExternalToolConfig;
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;

pub mod bash;
pub mod edit;
pub mod external;
pub mod git;
pub mod glob;
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

pub fn tool_is_available_readonly_concurrent(request: &ToolRequest) -> bool {
    let reg = registry::default_tool_registry();
    reg.resolve(request.name.as_str())
        .map(|resolved| {
            resolved.spec.capabilities.is_read_only()
                && resolved.spec.concurrent_safe
                && resolved.tool.is_concurrent_safe(request)
        })
        .unwrap_or(false)
}

pub fn should_run_readonly_batch(max_read_parallel: usize, tool_request: &ToolRequest) -> bool {
    tool_is_available_readonly_concurrent(tool_request) && max_read_parallel > 1
}

pub fn collect_readonly_batch(
    max_read_parallel: usize,
    tool_requests: &[ToolRequest],
    start: usize,
) -> usize {
    let max_end = (start + max_read_parallel).min(tool_requests.len());
    let mut end = start;
    while end < max_end && tool_is_available_readonly_concurrent(&tool_requests[end]) {
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
    use orca_core::approval_types::ActionKind;
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
    fn registry_resolves_list_files_to_discovery_capabilities() {
        let reg = registry::default_tool_registry();
        let resolved = reg.resolve("list_files").expect("list_files alias");

        assert_eq!(resolved.tool.name(), "glob");
        assert!(resolved.spec.capabilities.is_read_only());
        assert_eq!(resolved.requested_name.as_str(), "list_files");
    }

    #[test]
    fn model_visible_tools_hide_list_files_after_glob_exists() {
        let reg = registry::default_tool_registry();
        let names = reg
            .model_visible_tools()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert!(names.contains(&"glob".to_string()));
        assert!(!names.contains(&"list_files".to_string()));
    }

    #[test]
    fn external_tool_cannot_shadow_builtin_list_files_alias() {
        let external_tools = vec![ExternalToolConfig {
            name: "list_files".to_string(),
            description: "external list files".to_string(),
            action_kind: ActionKind::Shell,
            command: "echo external".to_string(),
            schema: serde_json::json!({}),
        }];
        let reg = registry::tool_registry_with_mcp_and_external(None, &external_tools);
        let resolved = reg.resolve("list_files").expect("list_files alias");

        assert_eq!(resolved.tool.name(), "glob");
        assert_eq!(resolved.spec.capabilities.action_kind(), ActionKind::Read);
    }

    #[test]
    fn readonly_batch_ignores_caller_supplied_write_action_for_read_tool() {
        let request = ToolRequest {
            id: "read".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Write,
            target: Some("README.md".to_string()),
            raw_arguments: None,
        };

        assert!(should_run_readonly_batch(2, &request));
    }

    #[test]
    fn registry_executes_glob_with_pattern_and_path() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path().join("src/bin")).expect("fixture dir");
        fs::write(temp_dir.path().join("src/lib.rs"), "lib").expect("fixture");
        fs::write(temp_dir.path().join("src/bin/main.rs"), "main").expect("fixture");
        fs::write(temp_dir.path().join("src/readme.md"), "readme").expect("fixture");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: Some(r#"{"pattern":"**/*.rs","path":"src"}"#.to_string()),
        };

        let result = reg.execute(&request, &registry::ToolContext::new(temp_dir.path()));

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            result.output.as_deref(),
            Some("src/bin/main.rs\nsrc/lib.rs")
        );
    }

    #[test]
    fn registry_executes_glob_with_workspace_prefixed_pattern_from_dot_path() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path().join("src/bin")).expect("fixture dir");
        fs::write(temp_dir.path().join("src/lib.rs"), "lib").expect("fixture");
        fs::write(temp_dir.path().join("src/bin/main.rs"), "main").expect("fixture");
        fs::write(temp_dir.path().join("README.md"), "readme").expect("fixture");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some(".".to_string()),
            raw_arguments: Some(r#"{"pattern":"src/**/*.rs","path":"."}"#.to_string()),
        };

        let result = reg.execute(&request, &registry::ToolContext::new(temp_dir.path()));

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            result.output.as_deref(),
            Some("src/bin/main.rs\nsrc/lib.rs")
        );
    }

    #[test]
    fn registry_executes_glob_with_no_matches() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path()).expect("fixture dir");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some("missing".to_string()),
            raw_arguments: Some(r#"{"pattern":"*.rs","path":"missing"}"#.to_string()),
        };

        let result = reg.execute(&request, &registry::ToolContext::new(temp_dir.path()));

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("(no matches)"));
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
