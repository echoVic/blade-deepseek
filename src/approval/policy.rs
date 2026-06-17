use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::approval::rules::CompiledPermissionRules;
#[cfg(test)]
use crate::approval::rules::PermissionRule;
pub use crate::approval::rules::PermissionRules;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    #[default]
    Suggest,
    #[value(name = "auto-edit")]
    AutoEdit,
    FullAuto,
    Plan,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Suggest => "suggest",
            Self::AutoEdit => "auto-edit",
            Self::FullAuto => "full-auto",
            Self::Plan => "plan",
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

#[derive(Clone, Debug)]
pub struct ApprovalPolicy {
    mode: ApprovalMode,
    rules: CompiledPermissionRules,
}

impl ApprovalPolicy {
    pub fn new(mode: ApprovalMode) -> Self {
        Self {
            mode,
            rules: CompiledPermissionRules::default(),
        }
    }

    #[cfg(test)]
    pub fn with_rules(mut self, allow: Vec<PermissionRule>, deny: Vec<PermissionRule>) -> Self {
        self.rules = CompiledPermissionRules::from_rules(PermissionRules { allow, deny });
        self
    }

    pub fn with_permission_rules(mut self, rules: PermissionRules) -> Self {
        self.rules = CompiledPermissionRules::from_rules(rules);
        self
    }

    #[cfg(test)]
    pub fn resolve(&self, request: &ApprovalRequest) -> ApprovalResolution {
        self.resolve_for_tool(request, "", None)
    }

    pub fn resolve_for_tool(
        &self,
        request: &ApprovalRequest,
        tool: &str,
        target: Option<&str>,
    ) -> ApprovalResolution {
        if self.rules.deny_matches(tool, target) {
            return ApprovalResolution {
                id: request.id.clone(),
                decision: ApprovalDecision::Deny,
                reason: format!(
                    "permission deny rule matches {tool} {}",
                    target.unwrap_or("")
                ),
            };
        }

        if self.rules.allow_matches(tool, target) {
            return ApprovalResolution {
                id: request.id.clone(),
                decision: ApprovalDecision::Allow,
                reason: format!(
                    "permission allow rule matches {tool} {}",
                    target.unwrap_or("")
                ),
            };
        }

        let decision = match (self.mode, request.action) {
            (_, ActionKind::Read) => ApprovalDecision::Allow,
            (ApprovalMode::Plan, ActionKind::Write | ActionKind::Shell) => ApprovalDecision::Deny,
            (ApprovalMode::Suggest, ActionKind::Write | ActionKind::Shell) => ApprovalDecision::Ask,
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
    fn plan_mode_allows_read_and_denies_mutation() {
        let policy = ApprovalPolicy::new(ApprovalMode::Plan);
        assert_eq!(
            policy.resolve(&make_request(ActionKind::Read)).decision,
            ApprovalDecision::Allow
        );
        assert_eq!(
            policy.resolve(&make_request(ActionKind::Write)).decision,
            ApprovalDecision::Deny
        );
        assert_eq!(
            policy.resolve(&make_request(ActionKind::Shell)).decision,
            ApprovalDecision::Deny
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

    #[test]
    fn matching_deny_rule_overrides_full_auto() {
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto)
            .with_rules(Vec::new(), vec![PermissionRule::new("bash", "rm -rf *")]);
        let req = ApprovalRequest {
            id: "danger".to_string(),
            action: ActionKind::Shell,
            description: "bash requested rm -rf target".to_string(),
        };

        let res = policy.resolve_for_tool(&req, "bash", Some("rm -rf target"));

        assert_eq!(res.decision, ApprovalDecision::Deny);
        assert!(res.reason.contains("permission deny rule"));
    }

    #[test]
    fn matching_allow_rule_overrides_suggest_prompt() {
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest)
            .with_rules(vec![PermissionRule::new("bash", "cargo *")], Vec::new());
        let req = ApprovalRequest {
            id: "cargo".to_string(),
            action: ActionKind::Shell,
            description: "bash requested cargo test".to_string(),
        };

        let res = policy.resolve_for_tool(&req, "bash", Some("cargo test"));

        assert_eq!(res.decision, ApprovalDecision::Allow);
        assert!(res.reason.contains("permission allow rule"));
    }

    #[test]
    fn no_matching_rule_uses_mode_default() {
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest)
            .with_rules(vec![PermissionRule::new("bash", "cargo *")], Vec::new());
        let req = ApprovalRequest {
            id: "other".to_string(),
            action: ActionKind::Shell,
            description: "bash requested npm test".to_string(),
        };

        let res = policy.resolve_for_tool(&req, "bash", Some("npm test"));

        assert_eq!(res.decision, ApprovalDecision::Ask);
    }
}
