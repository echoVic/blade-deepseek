use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    #[default]
    Suggest,
    #[value(name = "auto-edit")]
    AutoEdit,
    FullAuto,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Suggest => "suggest",
            Self::AutoEdit => "auto-edit",
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
    Ask,
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
            (_, ActionKind::Read) => ApprovalDecision::Allow,
            (ApprovalMode::Suggest, ActionKind::Write | ActionKind::Shell) => {
                ApprovalDecision::Ask
            }
            (ApprovalMode::AutoEdit, ActionKind::Write) => ApprovalDecision::Allow,
            (ApprovalMode::AutoEdit, ActionKind::Shell) => ApprovalDecision::Ask,
            (ApprovalMode::FullAuto, _) => ApprovalDecision::Allow,
        };

        let reason = match decision {
            ApprovalDecision::Allow => {
                format!("{} permits {}", self.mode.as_str(), request.action.as_str())
            }
            ApprovalDecision::Ask => {
                format!(
                    "{} requires confirmation for {}",
                    self.mode.as_str(),
                    request.action.as_str()
                )
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
    fn suggest_allows_read() {
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest);
        let res = policy.resolve(&make_request(ActionKind::Read));
        assert_eq!(res.decision, ApprovalDecision::Allow);
    }

    #[test]
    fn suggest_asks_write() {
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest);
        let res = policy.resolve(&make_request(ActionKind::Write));
        assert_eq!(res.decision, ApprovalDecision::Ask);
    }

    #[test]
    fn suggest_asks_shell() {
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest);
        let res = policy.resolve(&make_request(ActionKind::Shell));
        assert_eq!(res.decision, ApprovalDecision::Ask);
    }

    #[test]
    fn auto_edit_allows_read() {
        let policy = ApprovalPolicy::new(ApprovalMode::AutoEdit);
        let res = policy.resolve(&make_request(ActionKind::Read));
        assert_eq!(res.decision, ApprovalDecision::Allow);
    }

    #[test]
    fn auto_edit_allows_write() {
        let policy = ApprovalPolicy::new(ApprovalMode::AutoEdit);
        let res = policy.resolve(&make_request(ActionKind::Write));
        assert_eq!(res.decision, ApprovalDecision::Allow);
    }

    #[test]
    fn auto_edit_asks_shell() {
        let policy = ApprovalPolicy::new(ApprovalMode::AutoEdit);
        let res = policy.resolve(&make_request(ActionKind::Shell));
        assert_eq!(res.decision, ApprovalDecision::Ask);
    }

    #[test]
    fn full_auto_allows_all() {
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        assert_eq!(
            policy.resolve(&make_request(ActionKind::Read)).decision,
            ApprovalDecision::Allow
        );
        assert_eq!(
            policy.resolve(&make_request(ActionKind::Write)).decision,
            ApprovalDecision::Allow
        );
        assert_eq!(
            policy.resolve(&make_request(ActionKind::Shell)).decision,
            ApprovalDecision::Allow
        );
    }

    #[test]
    fn resolution_preserves_request_id() {
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest);
        let req = ApprovalRequest {
            id: "custom-id-42".to_string(),
            action: ActionKind::Read,
            description: "test".to_string(),
        };
        let res = policy.resolve(&req);
        assert_eq!(res.id, "custom-id-42");
    }
}
