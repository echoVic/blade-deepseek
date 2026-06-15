use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    ReadOnly,
    #[default]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(action: ActionKind) -> ApprovalRequest {
        ApprovalRequest {
            id: "test-1".to_string(),
            action,
            description: "test".to_string(),
        }
    }

    #[test]
    fn read_only_allows_read() {
        let policy = ApprovalPolicy::new(ApprovalMode::ReadOnly);
        let res = policy.resolve(&make_request(ActionKind::Read));
        assert_eq!(res.decision, ApprovalDecision::Allow);
    }

    #[test]
    fn read_only_denies_write() {
        let policy = ApprovalPolicy::new(ApprovalMode::ReadOnly);
        let res = policy.resolve(&make_request(ActionKind::Write));
        assert_eq!(res.decision, ApprovalDecision::Deny);
    }

    #[test]
    fn read_only_denies_shell() {
        let policy = ApprovalPolicy::new(ApprovalMode::ReadOnly);
        let res = policy.resolve(&make_request(ActionKind::Shell));
        assert_eq!(res.decision, ApprovalDecision::Deny);
    }

    #[test]
    fn workspace_write_allows_read() {
        let policy = ApprovalPolicy::new(ApprovalMode::WorkspaceWrite);
        let res = policy.resolve(&make_request(ActionKind::Read));
        assert_eq!(res.decision, ApprovalDecision::Allow);
    }

    #[test]
    fn workspace_write_allows_write() {
        let policy = ApprovalPolicy::new(ApprovalMode::WorkspaceWrite);
        let res = policy.resolve(&make_request(ActionKind::Write));
        assert_eq!(res.decision, ApprovalDecision::Allow);
    }

    #[test]
    fn workspace_write_denies_shell() {
        let policy = ApprovalPolicy::new(ApprovalMode::WorkspaceWrite);
        let res = policy.resolve(&make_request(ActionKind::Shell));
        assert_eq!(res.decision, ApprovalDecision::Deny);
    }

    #[test]
    fn full_auto_allows_all() {
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        assert_eq!(policy.resolve(&make_request(ActionKind::Read)).decision, ApprovalDecision::Allow);
        assert_eq!(policy.resolve(&make_request(ActionKind::Write)).decision, ApprovalDecision::Allow);
        assert_eq!(policy.resolve(&make_request(ActionKind::Shell)).decision, ApprovalDecision::Allow);
    }

    #[test]
    fn resolution_preserves_request_id() {
        let policy = ApprovalPolicy::new(ApprovalMode::ReadOnly);
        let req = ApprovalRequest {
            id: "custom-id-42".to_string(),
            action: ActionKind::Read,
            description: "test".to_string(),
        };
        let res = policy.resolve(&req);
        assert_eq!(res.id, "custom-id-42");
    }
}
