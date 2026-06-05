use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::approval::policy::ActionKind;

pub mod bash;
pub mod edit;
pub mod git;
pub mod grep;
pub mod list_files;
pub mod read_file;

const MAX_TOOL_OUTPUT_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    ReadFile,
    ListFiles,
    Grep,
    Bash,
    Edit,
    GitStatus,
}

impl ToolName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::ListFiles => "list_files",
            Self::Grep => "grep",
            Self::Bash => "bash",
            Self::Edit => "edit",
            Self::GitStatus => "git_status",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolRequest {
    pub id: String,
    pub name: ToolName,
    pub action: ActionKind,
    pub target: Option<String>,
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
            name: request.name,
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
            name: request.name,
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
            name: request.name,
            status: ToolStatus::Denied,
            output: None,
            error: Some(reason.into()),
            exit_code: None,
            truncated: false,
        }
    }
}

pub fn execute(request: &ToolRequest, cwd: &Path) -> ToolResult {
    match request.name {
        ToolName::ReadFile => read_file::execute(request, cwd, MAX_TOOL_OUTPUT_BYTES),
        ToolName::ListFiles => list_files::execute(request, cwd, MAX_TOOL_OUTPUT_BYTES),
        ToolName::GitStatus => git::status(request, cwd, MAX_TOOL_OUTPUT_BYTES),
        ToolName::Grep => grep::execute(request, cwd, MAX_TOOL_OUTPUT_BYTES),
        ToolName::Bash => bash::execute(request, cwd, MAX_TOOL_OUTPUT_BYTES),
        ToolName::Edit => edit::execute(request, cwd),
    }
}

pub fn request_from_prompt(prompt: &str) -> Option<ToolRequest> {
    let prompt = prompt.trim();
    let lower = prompt.to_ascii_lowercase();

    if lower.contains("git status") {
        return Some(ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::GitStatus,
            action: ActionKind::Read,
            target: Some(".".to_string()),
        });
    }

    if lower.contains("list files") || lower == "ls" {
        return Some(ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::ListFiles,
            action: ActionKind::Read,
            target: Some(".".to_string()),
        });
    }

    if let Some(target) = target_after(prompt, "read") {
        return Some(ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some(target),
        });
    }

    if let Some(target) = target_after(prompt, "grep") {
        return Some(ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some(target),
        });
    }

    if lower.starts_with("bash ") {
        return Some(ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some(prompt[5..].trim().to_string()),
        });
    }

    if lower.starts_with("edit ") {
        return Some(ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: Some(prompt[5..].trim().to_string()),
        });
    }

    None
}

fn target_after(prompt: &str, command: &str) -> Option<String> {
    let mut parts = prompt.split_whitespace();
    if parts.next()?.eq_ignore_ascii_case(command) {
        let target = parts.collect::<Vec<_>>().join(" ");
        if target.is_empty() {
            None
        } else {
            Some(target)
        }
    } else {
        None
    }
}

fn resolve_workspace_path(cwd: &Path, target: Option<&str>) -> PathBuf {
    let target = target.unwrap_or(".");
    let candidate = PathBuf::from(target);
    if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    }
}

fn truncate_output(output: String, max_bytes: usize) -> (String, bool) {
    if output.len() <= max_bytes {
        return (output, false);
    }

    let mut end = max_bytes;
    while !output.is_char_boundary(end) {
        end -= 1;
    }

    (format!("{}\\n[truncated]", &output[..end]), true)
}
