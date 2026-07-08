use std::io;

use orca_approval::ApprovalPolicy;
use orca_core::approval_types::{ApprovalDecision, ApprovalRequest, ApprovalResolution};
use orca_core::config::RunConfig;
use orca_core::tool_types::{ToolRequest, ToolResult};

use crate::runtime_permission::TurnPermissionOverlay;

#[derive(Clone, Debug)]
pub enum RuntimeApprovalDecision {
    NotRequired,
    Allowed(ApprovalResolution),
    Ask(ApprovalRequest),
    Denied {
        resolution: ApprovalResolution,
        result: ToolResult,
    },
}

pub trait RuntimeApprovalHandler {
    fn resolve_interactive(
        &self,
        approval: &ApprovalRequest,
        request: &ToolRequest,
    ) -> io::Result<ApprovalResolution>;
}

pub struct RuntimeConfigApprovalHandler<'a> {
    config: &'a RunConfig,
}

pub(crate) struct RuntimeToolApprovalPolicy<'a> {
    policy: &'a ApprovalPolicy,
    permission_overlay: &'a mut TurnPermissionOverlay,
}

impl<'a> RuntimeToolApprovalPolicy<'a> {
    pub(crate) fn new(
        policy: &'a ApprovalPolicy,
        permission_overlay: &'a mut TurnPermissionOverlay,
    ) -> Self {
        Self {
            policy,
            permission_overlay,
        }
    }

    pub(crate) fn resolve(
        &mut self,
        approval: ApprovalRequest,
        request: &ToolRequest,
    ) -> RuntimeApprovalDecision {
        let preapproved = self
            .permission_overlay
            .consume_preapproved_tool_call_id(&request.id);
        if preapproved {
            return RuntimeApprovalDecision::Allowed(ApprovalResolution {
                id: approval.id,
                decision: ApprovalDecision::Allow,
                reason: "approved background continuation".to_string(),
            });
        }

        let decision = self.policy.resolve_for_tool(
            &approval,
            request.name.as_str(),
            request.target.as_deref(),
        );
        if self.permission_overlay.strict_auto_review()
            && decision.decision == ApprovalDecision::Allow
        {
            return RuntimeApprovalDecision::Ask(approval);
        }

        match decision.decision {
            ApprovalDecision::Allow => RuntimeApprovalDecision::Allowed(decision),
            ApprovalDecision::Ask => RuntimeApprovalDecision::Ask(approval),
            ApprovalDecision::Deny => {
                let result = ToolResult::denied(request, decision.reason.clone());
                RuntimeApprovalDecision::Denied {
                    resolution: decision,
                    result,
                }
            }
        }
    }
}

impl<'a> RuntimeConfigApprovalHandler<'a> {
    pub fn new(config: &'a RunConfig) -> Self {
        Self { config }
    }
}

impl RuntimeApprovalHandler for RuntimeConfigApprovalHandler<'_> {
    fn resolve_interactive(
        &self,
        approval: &ApprovalRequest,
        request: &ToolRequest,
    ) -> io::Result<ApprovalResolution> {
        crate::approval_resolution::resolve_interactive(self.config, approval, request)
    }
}

#[cfg(test)]
mod tests {
    use orca_approval::ApprovalPolicy;
    use orca_core::approval_types::{ActionKind, ApprovalDecision, ApprovalMode, ApprovalRequest};
    use orca_core::tool_types::{ToolName, ToolRequest};

    use super::{RuntimeApprovalDecision, RuntimeToolApprovalPolicy};
    use crate::runtime_permission::TurnPermissionOverlay;

    fn shell_approval_and_request() -> (ApprovalRequest, ToolRequest) {
        (
            ApprovalRequest {
                id: "approval-1".to_string(),
                action: ActionKind::Shell,
                description: "run shell command".to_string(),
                tool: Some("bash".to_string()),
                target: Some("echo hi".to_string()),
                preview: None,
            },
            ToolRequest {
                id: "shell-1".to_string(),
                name: ToolName::Bash,
                action: ActionKind::Shell,
                target: Some("echo hi".to_string()),
                raw_arguments: None,
            },
        )
    }

    #[test]
    fn runtime_tool_approval_policy_preserves_preapproved_allow_under_strict_auto_review() {
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        let mut overlay = TurnPermissionOverlay::default();
        overlay.set_preapproved_tool_call_id(Some("shell-1".to_string()));
        overlay.merge_strict_auto_review(true);
        let (approval, request) = shell_approval_and_request();

        let decision =
            RuntimeToolApprovalPolicy::new(&policy, &mut overlay).resolve(approval, &request);

        match decision {
            RuntimeApprovalDecision::Allowed(resolution) => {
                assert_eq!(resolution.decision, ApprovalDecision::Allow);
                assert_eq!(resolution.reason, "approved background continuation");
            }
            other => panic!("preapproved request should be allowed, got {other:?}"),
        }
        assert!(!overlay.consume_preapproved_tool_call_id("shell-1"));
    }

    #[test]
    fn runtime_tool_approval_policy_downgrades_auto_allow_when_strict_auto_review_is_enabled() {
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        let mut overlay = TurnPermissionOverlay::default();
        overlay.merge_strict_auto_review(true);
        let (approval, request) = shell_approval_and_request();

        let decision =
            RuntimeToolApprovalPolicy::new(&policy, &mut overlay).resolve(approval, &request);

        assert!(matches!(decision, RuntimeApprovalDecision::Ask(_)));
    }
}
