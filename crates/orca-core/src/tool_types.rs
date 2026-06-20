use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::approval_types::ActionKind;

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
    Workflow,
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
            Self::Workflow => "Workflow",
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
            "Workflow" => Self::Workflow,
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

pub fn truncate_output(output: String, max_bytes: usize) -> (String, bool) {
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
