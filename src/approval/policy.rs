use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    ReadOnly,
    WorkspaceWrite,
    FullAuto,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::FullAuto => "full-auto",
        }
    }
}

impl Default for ApprovalMode {
    fn default() -> Self {
        Self::WorkspaceWrite
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Read,
    Write,
    Shell,
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Shell => "shell",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub action: ActionKind,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalResolution {
    pub id: String,
    pub decision: ApprovalDecision,
    pub reason: String,
}

#[derive(Clone, Copy, Debug)]
pub struct ApprovalPolicy {
    mode: ApprovalMode,
}

impl ApprovalPolicy {
    pub fn new(mode: ApprovalMode) -> Self {
        Self { mode }
    }

    pub fn resolve(self, request: &ApprovalRequest) -> ApprovalResolution {
        let decision = match (self.mode, request.action) {
            (ApprovalMode::ReadOnly, ActionKind::Read) => ApprovalDecision::Allow,
            (ApprovalMode::ReadOnly, ActionKind::Write | ActionKind::Shell) => {
                ApprovalDecision::Deny
            }
            (ApprovalMode::WorkspaceWrite, ActionKind::Shell) => ApprovalDecision::Deny,
            (ApprovalMode::WorkspaceWrite, ActionKind::Read | ActionKind::Write) => {
                ApprovalDecision::Allow
            }
            (ApprovalMode::FullAuto, _) => ApprovalDecision::Allow,
        };

        let reason = match decision {
            ApprovalDecision::Allow => {
                format!("{} permits {}", self.mode.as_str(), request.action.as_str())
            }
            ApprovalDecision::Deny => {
                format!("{} denies {}", self.mode.as_str(), request.action.as_str())
            }
        };

        ApprovalResolution {
            id: request.id.clone(),
            decision,
            reason,
        }
    }
}
