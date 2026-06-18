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
    Network,
    Agent,
    Shell,
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Network => "network",
            Self::Agent => "agent",
            Self::Shell => "shell",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Prompt,
    Deny,
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
    pub fn with_rules(mut self, rules: Vec<PermissionRule>) -> Self {
        self.rules = CompiledPermissionRules::from_rules(PermissionRules { rules });
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
        if let Some(decision) = self.rules.matching_decision(tool, target) {
            let approval_decision = match decision {
                Decision::Allow => ApprovalDecision::Allow,
                Decision::Prompt => ApprovalDecision::Ask,
                Decision::Deny => ApprovalDecision::Deny,
            };
            return ApprovalResolution {
                id: request.id.clone(),
                decision: approval_decision,
                reason: format!(
                    "permission {} rule matches {tool} {}",
                    decision.as_str(),
                    target.unwrap_or("")
                ),
            };
        }

        let decision = match (self.mode, request.action) {
            (_, ActionKind::Read) => ApprovalDecision::Allow,
            (
                ApprovalMode::Plan,
                ActionKind::Write | ActionKind::Network | ActionKind::Agent | ActionKind::Shell,
            ) => ApprovalDecision::Deny,
            (
                ApprovalMode::Suggest,
                ActionKind::Write | ActionKind::Network | ActionKind::Agent | ActionKind::Shell,
            ) => ApprovalDecision::Ask,
            (ApprovalMode::AutoEdit, ActionKind::Write) => ApprovalDecision::Allow,
            (
                ApprovalMode::AutoEdit,
                ActionKind::Network | ActionKind::Agent | ActionKind::Shell,
            ) => ApprovalDecision::Ask,
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

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Prompt => "prompt",
            Self::Deny => "deny",
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
        let policy =
            ApprovalPolicy::new(ApprovalMode::FullAuto).with_rules(vec![PermissionRule::new(
                "bash",
                "rm -rf *",
                Decision::Deny,
            )]);
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
        let policy =
            ApprovalPolicy::new(ApprovalMode::Suggest).with_rules(vec![PermissionRule::new(
                "bash",
                "cargo *",
                Decision::Allow,
            )]);
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
        let policy =
            ApprovalPolicy::new(ApprovalMode::Suggest).with_rules(vec![PermissionRule::new(
                "bash",
                "cargo *",
                Decision::Allow,
            )]);
        let req = ApprovalRequest {
            id: "other".to_string(),
            action: ActionKind::Shell,
            description: "bash requested npm test".to_string(),
        };

        let res = policy.resolve_for_tool(&req, "bash", Some("npm test"));

        assert_eq!(res.decision, ApprovalDecision::Ask);
    }

    #[test]
    fn prompt_rule_overrides_full_auto_to_ask() {
        let policy =
            ApprovalPolicy::new(ApprovalMode::FullAuto).with_rules(vec![PermissionRule::new(
                "bash",
                "curl *",
                Decision::Prompt,
            )]);
        let req = ApprovalRequest {
            id: "curl".to_string(),
            action: ActionKind::Shell,
            description: "bash requested curl example.com".to_string(),
        };

        let res = policy.resolve_for_tool(&req, "bash", Some("curl example.com"));

        assert_eq!(res.decision, ApprovalDecision::Ask);
        assert!(res.reason.contains("permission prompt rule"));
    }

    #[test]
    fn strictest_matching_rule_wins() {
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest).with_rules(vec![
            PermissionRule::new("bash", "cargo *", Decision::Allow),
            PermissionRule::new("bash", "cargo publish *", Decision::Prompt),
            PermissionRule::new("bash", "cargo publish secret*", Decision::Deny),
        ]);
        let req = ApprovalRequest {
            id: "publish".to_string(),
            action: ActionKind::Shell,
            description: "bash requested cargo publish secret-crate".to_string(),
        };

        let res = policy.resolve_for_tool(&req, "bash", Some("cargo publish secret-crate"));

        assert_eq!(res.decision, ApprovalDecision::Deny);
        assert!(res.reason.contains("permission deny rule"));
    }
}
