use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    Suggest,
    #[value(name = "auto-edit")]
    #[default]
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

    /// Next mode in the Shift+Tab cycle: suggest -> auto-edit -> full-auto -> plan -> suggest.
    pub fn next(self) -> Self {
        match self {
            Self::Suggest => Self::AutoEdit,
            Self::AutoEdit => Self::FullAuto,
            Self::FullAuto => Self::Plan,
            Self::Plan => Self::Suggest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ApprovalMode;

    #[test]
    fn defaults_to_auto_edit_without_changing_the_mode_cycle() {
        assert_eq!(ApprovalMode::default(), ApprovalMode::AutoEdit);
        assert_eq!(ApprovalMode::Suggest.next(), ApprovalMode::AutoEdit);
        assert_eq!(ApprovalMode::Plan.next(), ApprovalMode::Suggest);
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

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Prompt => "prompt",
            Self::Deny => "deny",
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
    pub tool: Option<String>,
    pub target: Option<String>,
    pub preview: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalResolution {
    pub id: String,
    pub decision: ApprovalDecision,
    pub reason: String,
}
