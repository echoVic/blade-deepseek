use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

use crate::approval_types::ActionKind;

pub const MAX_TOOL_OUTPUT_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolName {
    ReadFile,
    ListFiles,
    Glob,
    Grep,
    Bash,
    Edit,
    WriteFile,
    GitStatus,
    Subagent,
    Workflow,
    WebSearch,
    UpdateGoal,
    UpdatePlan,
    Namespaced {
        namespace: String,
        name: String,
        serialized: String,
    },
    Mcp(String),
    External(String),
}

impl ToolName {
    pub fn plain(name: impl Into<String>) -> Self {
        let name = name.into();
        match name.as_str() {
            "read_file" => Self::ReadFile,
            "list_files" => Self::ListFiles,
            "glob" => Self::Glob,
            "grep" => Self::Grep,
            "bash" => Self::Bash,
            "edit" => Self::Edit,
            "write_file" => Self::WriteFile,
            "git_status" => Self::GitStatus,
            "subagent" => Self::Subagent,
            "Workflow" | "workflow" => Self::Workflow,
            "web_search" => Self::WebSearch,
            "update_goal" => Self::UpdateGoal,
            "update_plan" => Self::UpdatePlan,
            other => Self::External(other.to_string()),
        }
    }

    pub fn namespaced(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        let namespace = namespace.into();
        let name = name.into();
        let serialized = format!("{namespace}__{name}");
        Self::Namespaced {
            namespace,
            name,
            serialized,
        }
    }

    pub fn namespace(&self) -> Option<&str> {
        match self {
            Self::Namespaced { namespace, .. } => Some(namespace),
            Self::Mcp(name) => name.rsplit_once("__").map(|(namespace, _)| namespace),
            _ => None,
        }
    }

    pub fn local_name(&self) -> &str {
        match self {
            Self::ReadFile => "read_file",
            Self::ListFiles => "list_files",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Bash => "bash",
            Self::Edit => "edit",
            Self::WriteFile => "write_file",
            Self::GitStatus => "git_status",
            Self::Subagent => "subagent",
            Self::Workflow => "Workflow",
            Self::WebSearch => "web_search",
            Self::UpdateGoal => "update_goal",
            Self::UpdatePlan => "update_plan",
            Self::Namespaced { name, .. } => name,
            Self::Mcp(name) => name
                .rsplit_once("__")
                .map(|(_, local)| local)
                .unwrap_or(name),
            Self::External(name) => name,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::ReadFile => "read_file",
            Self::ListFiles => "list_files",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Bash => "bash",
            Self::Edit => "edit",
            Self::WriteFile => "write_file",
            Self::GitStatus => "git_status",
            Self::Subagent => "subagent",
            Self::Workflow => "Workflow",
            Self::WebSearch => "web_search",
            Self::UpdateGoal => "update_goal",
            Self::UpdatePlan => "update_plan",
            Self::Namespaced { serialized, .. } => serialized,
            Self::Mcp(name) | Self::External(name) => name,
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        if value.starts_with("mcp__") {
            return Some(Self::Mcp(value.to_string()));
        }
        if let Some((namespace, name)) = parse_namespaced_tool(value) {
            return Some(Self::namespaced(namespace, name));
        }
        Some(match value {
            "read_file" => Self::ReadFile,
            "list_files" => Self::ListFiles,
            "glob" => Self::Glob,
            "grep" => Self::Grep,
            "bash" => Self::Bash,
            "edit" => Self::Edit,
            "write_file" => Self::WriteFile,
            "git_status" => Self::GitStatus,
            "subagent" => Self::Subagent,
            "Workflow" | "workflow" => Self::Workflow,
            "web_search" => Self::WebSearch,
            "update_goal" => Self::UpdateGoal,
            "update_plan" => Self::UpdatePlan,
            other => Self::External(other.to_string()),
        })
    }

    pub fn is_builtin(&self, builtin: &str) -> bool {
        self.namespace().is_none() && self.as_str() == builtin
    }

    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::ReadFile | Self::ListFiles | Self::Glob | Self::Grep | Self::GitStatus
        )
    }
}

fn parse_namespaced_tool(value: &str) -> Option<(&str, &str)> {
    let (namespace, name) = value.rsplit_once("__")?;
    if !namespace.is_empty() && !name.is_empty() {
        Some((namespace, name))
    } else {
        None
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolCapability {
    FsRead,
    FsList,
    FsSearch,
    FsWrite,
    ShellExecute,
    GitInspect,
    NetworkSearch,
    AgentDelegate,
    WorkflowRun,
    PlanUpdate,
    GoalUpdate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilitySet {
    capabilities: Vec<ToolCapability>,
}

impl CapabilitySet {
    pub fn new(capabilities: Vec<ToolCapability>) -> Self {
        Self { capabilities }
    }

    pub fn read_only_fs() -> Self {
        Self::new(vec![ToolCapability::FsRead])
    }

    pub fn filesystem_write() -> Self {
        Self::new(vec![ToolCapability::FsWrite])
    }

    pub fn shell_execute() -> Self {
        Self::new(vec![ToolCapability::ShellExecute])
    }

    pub fn network_search() -> Self {
        Self::new(vec![ToolCapability::NetworkSearch])
    }

    pub fn agent_delegate() -> Self {
        Self::new(vec![ToolCapability::AgentDelegate])
    }

    pub fn contains(&self, capability: ToolCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn is_read_only(&self) -> bool {
        !self.capabilities.is_empty()
            && self.capabilities.iter().all(|capability| {
                matches!(
                    capability,
                    ToolCapability::FsRead
                        | ToolCapability::FsList
                        | ToolCapability::FsSearch
                        | ToolCapability::GitInspect
                        | ToolCapability::PlanUpdate
                        | ToolCapability::GoalUpdate
                )
            })
    }

    pub fn action_kind(&self) -> ActionKind {
        if self.contains(ToolCapability::ShellExecute) {
            ActionKind::Shell
        } else if self.contains(ToolCapability::FsWrite) {
            ActionKind::Write
        } else if self.contains(ToolCapability::NetworkSearch) {
            ActionKind::Network
        } else if self.contains(ToolCapability::AgentDelegate)
            || self.contains(ToolCapability::WorkflowRun)
        {
            ActionKind::Agent
        } else {
            ActionKind::Read
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolExposure {
    Direct,
    Deferred,
    ModelOnly,
    Hidden,
}

impl ToolExposure {
    pub fn is_model_visible(self) -> bool {
        matches!(self, Self::Direct | Self::ModelOnly)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RendererHint {
    FileRead,
    FileList,
    FileSearch,
    Shell,
    Write,
    Network,
    Agent,
    State,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultSemantics {
    Standard,
    EmptyIsSuccess,
    NoMatchesIsSuccess,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultKind {
    Success,
    Empty,
    NoMatches,
    Truncated,
    PermissionDenied,
    InvalidInput,
    RuntimeError,
}

impl ToolResultKind {
    pub fn success() -> Self {
        Self::Success
    }

    pub fn is_success(&self) -> bool {
        *self == Self::Success
    }

    pub fn status(self) -> ToolStatus {
        match self {
            Self::Success | Self::Empty | Self::NoMatches | Self::Truncated => {
                ToolStatus::Completed
            }
            Self::PermissionDenied => ToolStatus::Denied,
            Self::InvalidInput | Self::RuntimeError => ToolStatus::Failed,
        }
    }
}

impl Default for ToolResultKind {
    fn default() -> Self {
        Self::Success
    }
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: ToolName,
    pub aliases: Vec<ToolName>,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: Option<serde_json::Value>,
    pub capabilities: CapabilitySet,
    pub exposure: ToolExposure,
    pub result_semantics: ResultSemantics,
    pub renderer: RendererHint,
    pub concurrent_safe: bool,
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
    #[serde(
        default = "ToolResultKind::success",
        skip_serializing_if = "ToolResultKind::is_success"
    )]
    pub kind: ToolResultKind,
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
            kind: ToolResultKind::Success,
        }
    }

    pub fn completed_kind(
        request: &ToolRequest,
        output: String,
        truncated: bool,
        kind: ToolResultKind,
    ) -> Self {
        Self {
            id: request.id.clone(),
            name: request.name.clone(),
            status: kind.status(),
            output: Some(output),
            error: None,
            exit_code: Some(0),
            truncated,
            kind,
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
            kind: ToolResultKind::RuntimeError,
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
            kind: ToolResultKind::PermissionDenied,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ToolOutputTruncation {
    Bytes { limit: usize },
    Tokens { limit: usize },
}

impl ToolOutputTruncation {
    pub fn bytes(limit: usize) -> Self {
        Self::Bytes { limit }
    }

    pub fn tokens(limit: usize) -> Self {
        Self::Tokens { limit }
    }

    pub fn limit(self) -> usize {
        match self {
            Self::Bytes { limit } | Self::Tokens { limit } => limit,
        }
    }

    pub fn normalized(self) -> Self {
        match self {
            Self::Bytes { limit } => Self::Bytes {
                limit: limit.max(1),
            },
            Self::Tokens { limit } => Self::Tokens {
                limit: limit.max(1),
            },
        }
    }
}

impl Default for ToolOutputTruncation {
    fn default() -> Self {
        Self::Bytes {
            limit: MAX_TOOL_OUTPUT_BYTES,
        }
    }
}

impl fmt::Display for ToolOutputTruncation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes { limit } => write!(f, "bytes:{limit}"),
            Self::Tokens { limit } => write!(f, "tokens:{limit}"),
        }
    }
}

pub fn truncate_output(output: String, max_bytes: usize) -> (String, bool) {
    truncate_output_with_policy(output, ToolOutputTruncation::bytes(max_bytes))
}

pub fn truncate_output_with_policy(
    output: String,
    policy: ToolOutputTruncation,
) -> (String, bool) {
    match policy.normalized() {
        ToolOutputTruncation::Bytes { limit } => truncate_output_bytes(output, limit),
        ToolOutputTruncation::Tokens { limit } => truncate_output_tokens(output, limit),
    }
}

fn truncate_output_bytes(output: String, max_bytes: usize) -> (String, bool) {
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

fn truncate_output_tokens(output: String, max_tokens: usize) -> (String, bool) {
    let original_tokens = approx_tool_tokens(&output);
    if original_tokens <= max_tokens {
        return (output, false);
    }

    let line_count = output.lines().count();
    let byte_budget = approx_bytes_for_tokens(max_tokens);
    let (snippet, _) = truncate_output_bytes(output, byte_budget.max(1));
    (
        format!(
            "Warning: truncated tool output\nOriginal token count: {original_tokens}\nOriginal line count: {line_count}\n\n{snippet}"
        ),
        true,
    )
}

fn approx_tool_tokens(text: &str) -> usize {
    text.split_whitespace().count().max((text.len() + 3) / 4)
}

fn approx_bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_round_trips_plain_names() {
        let name = ToolName::from_str("read_file").expect("known tool");
        assert_eq!(name, ToolName::ReadFile);
        assert_eq!(name.as_str(), "read_file");
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"read_file\"");
        assert_eq!(
            serde_json::from_str::<ToolName>("\"read_file\"").unwrap(),
            name
        );
    }

    #[test]
    fn tool_name_preserves_mcp_namespace() {
        let name = ToolName::from_str("mcp__foo__exec_command").expect("mcp tool");
        assert_eq!(name, ToolName::Mcp("mcp__foo__exec_command".to_string()));
        assert_eq!(name.namespace(), Some("mcp__foo"));
        assert_eq!(name.local_name(), "exec_command");
        assert_eq!(name.as_str(), "mcp__foo__exec_command");
    }

    #[test]
    fn tool_name_helpers_support_non_legacy_namespaced_tools() {
        let name = ToolName::namespaced("vendor", "inspect");
        assert_eq!(name, ToolName::namespaced("vendor", "inspect"));
        assert_eq!(name.namespace(), Some("vendor"));
        assert_eq!(name.local_name(), "inspect");
        assert_eq!(name.as_str(), "vendor__inspect");
    }

    #[test]
    fn capability_set_derives_action_kind() {
        assert_eq!(
            CapabilitySet::read_only_fs().action_kind(),
            ActionKind::Read
        );
        assert_eq!(
            CapabilitySet::filesystem_write().action_kind(),
            ActionKind::Write
        );
        assert_eq!(
            CapabilitySet::shell_execute().action_kind(),
            ActionKind::Shell
        );
        assert_eq!(
            CapabilitySet::network_search().action_kind(),
            ActionKind::Network
        );
        assert_eq!(
            CapabilitySet::agent_delegate().action_kind(),
            ActionKind::Agent
        );
    }

    #[test]
    fn empty_capability_set_is_not_read_only() {
        assert!(!CapabilitySet::new(Vec::new()).is_read_only());
    }

    #[test]
    fn completed_result_kinds_remain_completed_status() {
        assert_eq!(ToolResultKind::Success.status(), ToolStatus::Completed);
        assert_eq!(ToolResultKind::Empty.status(), ToolStatus::Completed);
        assert_eq!(ToolResultKind::NoMatches.status(), ToolStatus::Completed);
        assert_eq!(ToolResultKind::Truncated.status(), ToolStatus::Completed);
        assert_eq!(ToolResultKind::InvalidInput.status(), ToolStatus::Failed);
        assert_eq!(ToolResultKind::RuntimeError.status(), ToolStatus::Failed);
    }

    #[test]
    fn truncation_policy_bytes_keeps_existing_middle_compaction_marker() {
        let policy = ToolOutputTruncation::bytes(64);
        let text = "abcdefghijklmnopqrstuvwxyz".repeat(8);

        let (output, truncated) = truncate_output_with_policy(text, policy);

        assert!(truncated);
        assert!(output.contains("tool output micro-compacted"));
        assert!(output.starts_with("a"));
        assert!(output.ends_with("z"));
    }

    #[test]
    fn truncation_policy_tokens_reports_original_size_and_line_count() {
        let policy = ToolOutputTruncation::tokens(16);
        let text = format!(
            "{}\n{}",
            "alpha beta gamma delta epsilon zeta ".repeat(4),
            "eta theta iota kappa lambda zeta ".repeat(4)
        );

        let (output, truncated) = truncate_output_with_policy(text, policy);

        assert!(truncated);
        assert!(output.starts_with("Warning: truncated tool output"));
        assert!(output.contains("Original token count:"));
        assert!(output.contains("Original line count: 2"));
        assert!(output.contains("alpha"));
        assert!(output.contains("zeta"));
    }
}
