use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::approval::policy::ActionKind;
use crate::mcp::McpRegistry;

pub mod bash;
pub mod edit;
pub mod git;
pub mod grep;
pub mod list_files;
pub mod read_file;
pub mod update_plan;
pub mod web_search;
pub mod write_file;

const MAX_TOOL_OUTPUT_BYTES: usize = 8 * 1024;

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
        }
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
        Ok(match value.as_str() {
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
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unknown tool name: {other}"
                )));
            }
        })
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

pub fn execute(request: &ToolRequest, cwd: &Path) -> ToolResult {
    match &request.name {
        ToolName::ReadFile => read_file::execute(request, cwd, MAX_TOOL_OUTPUT_BYTES),
        ToolName::ListFiles => list_files::execute(request, cwd, MAX_TOOL_OUTPUT_BYTES),
        ToolName::GitStatus => git::status(request, cwd, MAX_TOOL_OUTPUT_BYTES),
        ToolName::Grep => grep::execute(request, cwd, MAX_TOOL_OUTPUT_BYTES),
        ToolName::Bash => bash::execute(request, cwd, MAX_TOOL_OUTPUT_BYTES),
        ToolName::Edit => edit::execute(request, cwd),
        ToolName::WriteFile => write_file::execute(request, cwd),
        ToolName::WebSearch => web_search::execute(request, MAX_TOOL_OUTPUT_BYTES),
        ToolName::UpdatePlan => update_plan::execute(request),
        ToolName::Subagent => ToolResult::failed(
            request,
            "subagent tool must be executed by the runtime",
            None,
        ),
        ToolName::Mcp(_) => ToolResult::failed(request, "MCP registry is not initialized", None),
    }
}

pub fn execute_with_mcp(
    request: &ToolRequest,
    cwd: &Path,
    mcp_registry: &McpRegistry,
) -> ToolResult {
    match &request.name {
        ToolName::Mcp(schema_name) => execute_mcp(request, schema_name, mcp_registry),
        _ => execute(request, cwd),
    }
}

fn execute_mcp(request: &ToolRequest, schema_name: &str, mcp_registry: &McpRegistry) -> ToolResult {
    let Some(tool_ref) = mcp_registry.resolve_tool(schema_name) else {
        return ToolResult::failed(request, format!("unknown MCP tool: {schema_name}"), None);
    };
    let arguments = match request
        .raw_arguments
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
    {
        Ok(Some(value)) => value,
        Ok(None) => serde_json::Value::Object(Default::default()),
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("invalid MCP arguments JSON: {error}"),
                None,
            );
        }
    };

    match mcp_registry.call_tool(&tool_ref, arguments) {
        Ok(result) if result.is_error => ToolResult::failed(request, result.output, None),
        Ok(result) => ToolResult::completed(request, result.output, false),
        Err(error) => ToolResult::failed(request, error, None),
    }
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

    #[test]
    fn micro_compact_preserves_head_and_tail() {
        let output = format!("{}{}{}", "a".repeat(80), "middle", "z".repeat(80));
        let (truncated, was_truncated) = truncate_output(output, 80);
        assert!(was_truncated);
        assert!(truncated.starts_with("aaaa"));
        assert!(truncated.contains("micro-compacted"));
        assert!(truncated.ends_with("zzzz"));
    }
}
