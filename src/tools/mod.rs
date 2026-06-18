use std::path::{Path, PathBuf};
use std::thread;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::approval::policy::ActionKind;
use crate::mcp::McpRegistry;

pub mod bash;
pub mod edit;
pub mod external;
pub mod git;
pub mod grep;
pub mod list_files;
pub mod read_file;
pub mod registry;
pub mod update_plan;
pub mod web_search;
pub mod write_file;

pub const MAX_TOOL_OUTPUT_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolName {
    ReadFile,
    ListFiles,
    Grep,
    Bash,
    Edit,
    WriteFile,
    GitStatus,
    Subagent,
    WebSearch,
    UpdatePlan,
    Mcp(String),
    External(String),
}

impl ToolName {
    pub fn as_str(&self) -> &str {
        match self {
            Self::ReadFile => "read_file",
            Self::ListFiles => "list_files",
            Self::Grep => "grep",
            Self::Bash => "bash",
            Self::Edit => "edit",
            Self::WriteFile => "write_file",
            Self::GitStatus => "git_status",
            Self::Subagent => "subagent",
            Self::WebSearch => "web_search",
            Self::UpdatePlan => "update_plan",
            Self::Mcp(name) => name,
            Self::External(name) => name,
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "read_file" => Self::ReadFile,
            "list_files" => Self::ListFiles,
            "grep" => Self::Grep,
            "bash" => Self::Bash,
            "edit" => Self::Edit,
            "write_file" => Self::WriteFile,
            "git_status" => Self::GitStatus,
            "subagent" => Self::Subagent,
            "web_search" => Self::WebSearch,
            "update_plan" => Self::UpdatePlan,
            other if other.starts_with("mcp__") => Self::Mcp(other.to_string()),
            other => Self::External(other.to_string()),
        })
    }

    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::ReadFile | Self::ListFiles | Self::Grep | Self::GitStatus
        )
    }
}

impl Serialize for ToolName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown tool name: {value}")))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolRequest {
    pub id: String,
    pub name: ToolName,
    pub action: ActionKind,
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_arguments: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Completed,
    Failed,
    Denied,
    NotImplemented,
}

impl ToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::NotImplemented => "not_implemented",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolResult {
    pub id: String,
    pub name: ToolName,
    pub status: ToolStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub exit_code: Option<i32>,
    pub truncated: bool,
}

impl ToolResult {
    pub fn completed(request: &ToolRequest, output: String, truncated: bool) -> Self {
        Self {
            id: request.id.clone(),
            name: request.name.clone(),
            status: ToolStatus::Completed,
            output: Some(output),
            error: None,
            exit_code: Some(0),
            truncated,
        }
    }

    pub fn failed(request: &ToolRequest, error: impl Into<String>, exit_code: Option<i32>) -> Self {
        Self {
            id: request.id.clone(),
            name: request.name.clone(),
            status: ToolStatus::Failed,
            output: None,
            error: Some(error.into()),
            exit_code,
            truncated: false,
        }
    }

    pub fn denied(request: &ToolRequest, reason: impl Into<String>) -> Self {
        Self {
            id: request.id.clone(),
            name: request.name.clone(),
            status: ToolStatus::Denied,
            output: None,
            error: Some(reason.into()),
            exit_code: None,
            truncated: false,
        }
    }
}

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
    external_tools: &[external::ExternalToolConfig],
) -> ToolResult {
    if !matches!(&request.name, ToolName::Mcp(_)) {
        if external_tools.is_empty() {
            let registry = registry::default_tool_registry();
            let ctx = registry::ToolContext::new(cwd);
            return registry.execute(request, &ctx);
        }
        let registry = registry::tool_registry_with_mcp_and_external(None, external_tools);
        let ctx = registry::ToolContext::new(cwd);
        return registry.execute(request, &ctx);
    }

    let registry =
        registry::tool_registry_with_mcp_and_external(Some(mcp_registry), external_tools);
    let ctx = registry::ToolContext::new(cwd).with_mcp(mcp_registry);
    registry.execute(request, &ctx)
}

fn is_concurrent_safe_read(request: &ToolRequest) -> bool {
    if request.action != ActionKind::Read {
        return false;
    }

    let registry = registry::default_tool_registry();
    registry
        .get(request.name.as_str())
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

fn resolve_workspace_path(cwd: &Path, target: Option<&str>) -> Result<PathBuf, String> {
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

fn truncate_output(output: String, max_bytes: usize) -> (String, bool) {
    if output.len() <= max_bytes {
        return (output, false);
    }

    let marker = "\n[... tool output micro-compacted ...]\n";
    if max_bytes <= marker.len() + 2 {
        let mut end = max_bytes;
        while end > 0 && !output.is_char_boundary(end) {
            end -= 1;
        }
        return (output[..end].to_string(), true);
    }

    let side_budget = (max_bytes - marker.len()) / 2;
    let mut head_end = side_budget;
    while !output.is_char_boundary(head_end) {
        head_end -= 1;
    }

    let mut tail_start = output.len().saturating_sub(side_budget);
    while !output.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    (
        format!("{}{}{}", &output[..head_end], marker, &output[tail_start..]),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::types::McpTool;
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
        let registry = registry::default_tool_registry();

        let tool = registry
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
        assert_eq!(
            registry.iter().next().map(|tool| tool.name()),
            Some("read_file")
        );

        assert_eq!(
            registry.get("web_search").unwrap().action_kind(),
            ActionKind::Network
        );
        assert_eq!(
            registry.get("subagent").unwrap().action_kind(),
            ActionKind::Agent
        );
    }

    #[test]
    fn registry_can_include_mcp_proxy_tools() {
        let mcp_registry = McpRegistry::from_tools_for_test(vec![McpTool {
            server: "demo".to_string(),
            name: "search".to_string(),
            schema_name: "mcp__demo__search".to_string(),
            description: Some("search docs".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }),
        }]);

        let registry = registry::tool_registry_with_mcp_and_external(Some(&mcp_registry), &[]);
        let tool = registry
            .get("mcp__demo__search")
            .expect("MCP tool is registered as a proxy tool");
        let request = ToolRequest {
            id: "mcp".to_string(),
            name: ToolName::Mcp("mcp__demo__search".to_string()),
            action: ActionKind::Read,
            target: Some("mcp__demo__search".to_string()),
            raw_arguments: Some(r#"{"query":"orca"}"#.to_string()),
        };

        assert_eq!(tool.name(), "mcp__demo__search");
        assert_eq!(tool.action_kind(), ActionKind::Write);
        assert!(!tool.is_read_only(&request));
        assert!(!tool.is_concurrent_safe(&request));
    }

    #[test]
    fn registry_executes_builtin_tool_by_registered_name() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        fs::write(temp_dir.path().join("note.txt"), "hello registry\n").expect("fixture");
        let registry = registry::default_tool_registry();
        let request = ToolRequest {
            id: "read".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("note.txt".to_string()),
            raw_arguments: None,
        };
        let ctx = registry::ToolContext::new(temp_dir.path());

        let result = registry.execute(&request, &ctx);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("hello registry\n"));
    }
}
