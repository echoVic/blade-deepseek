use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::PermissionProfileNetworkAccess;

use crate::network_proxy::RuntimeNetworkBlockReport;
use crate::protocol::{
    PermissionGrantScope, PermissionResponseDecision, RequestFileSystemPermissions,
    RequestNetworkPermissions, RequestPermissionProfile, RequestShellPermissions,
};
use crate::sandbox_denial::{
    SandboxDenialDiagnostic, should_request_filesystem_permission_with_denied_roots,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePermissionRequest {
    pub id: String,
    pub reason: Option<String>,
    pub permissions: RequestPermissionProfile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePermissionResponse {
    pub decision: PermissionResponseDecision,
    pub scope: PermissionGrantScope,
    pub permissions: RequestPermissionProfile,
    pub strict_auto_review: bool,
}

pub trait RuntimePermissionRequestHandler {
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimePermissionPromptDecision {
    AutoAllow,
    Prompt,
    Reject { reason: &'static str },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimePermissionOrigin {
    Bash,
    CommandExec,
}

impl RuntimePermissionOrigin {
    fn label(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::CommandExec => "command/exec",
        }
    }
}

pub(crate) struct RuntimePermissionPolicy;

impl RuntimePermissionPolicy {
    pub(crate) fn decide_request_permissions_prompt(
        approval_mode: ApprovalMode,
        handler_available: bool,
    ) -> RuntimePermissionPromptDecision {
        if approval_mode == ApprovalMode::FullAuto {
            return RuntimePermissionPromptDecision::AutoAllow;
        }
        if handler_available {
            return RuntimePermissionPromptDecision::Prompt;
        }
        RuntimePermissionPromptDecision::Reject {
            reason: "request_permissions requires a runtime permission handler unless approval mode is full-auto",
        }
    }

    pub(crate) fn network_block_request(
        request_id: &str,
        origin: RuntimePermissionOrigin,
        block: &RuntimeNetworkBlockReport,
    ) -> Option<RuntimePermissionRequest> {
        if block.error == "blocked-by-denylist" {
            return None;
        }

        let mut domains = HashMap::new();
        domains.insert(block.host.clone(), PermissionProfileNetworkAccess::Allow);
        Some(RuntimePermissionRequest {
            id: request_id.to_string(),
            reason: Some(format!(
                "{} attempted network access to {} ({})",
                origin.label(),
                block.host,
                block.error
            )),
            permissions: RequestPermissionProfile {
                file_system: None,
                network: Some(RequestNetworkPermissions {
                    enabled: None,
                    domains,
                }),
                shell: None,
            },
        })
    }

    pub(crate) fn filesystem_write_request(
        request_id: &str,
        origin: RuntimePermissionOrigin,
        diagnostic: &SandboxDenialDiagnostic,
    ) -> Option<RuntimePermissionRequest> {
        let write_root = diagnostic.suggested_write_root.as_ref()?.clone();
        Some(RuntimePermissionRequest {
            id: request_id.to_string(),
            reason: Some(format!(
                "{} attempted filesystem write outside the current sandbox: {}",
                origin.label(),
                write_root.display()
            )),
            permissions: RequestPermissionProfile {
                file_system: Some(RequestFileSystemPermissions {
                    read: None,
                    write: Some(vec![write_root]),
                    entries: None,
                }),
                network: None,
                shell: None,
            },
        })
    }

    pub(crate) fn unsandboxed_shell_request(
        request_id: &str,
        origin: RuntimePermissionOrigin,
        diagnostic: &SandboxDenialDiagnostic,
    ) -> Option<RuntimePermissionRequest> {
        if diagnostic.suggested_write_root.is_some() {
            return None;
        }

        Some(RuntimePermissionRequest {
            id: request_id.to_string(),
            reason: Some(format!(
                "{} needs to re-run without the filesystem sandbox because the sandbox denied access but did not report a filesystem path to grant",
                origin.label()
            )),
            permissions: RequestPermissionProfile {
                file_system: None,
                network: None,
                shell: Some(RequestShellPermissions { unsandboxed: true }),
            },
        })
    }

    pub(crate) fn sandbox_denial_request(
        request_id: &str,
        origin: RuntimePermissionOrigin,
        diagnostic: &SandboxDenialDiagnostic,
    ) -> RuntimePermissionRequest {
        Self::filesystem_write_request(request_id, origin, diagnostic).unwrap_or_else(|| {
            Self::unsandboxed_shell_request(request_id, origin, diagnostic)
                .expect("pathless sandbox denial should request unsandboxed shell retry")
        })
    }

    pub(crate) fn should_request_filesystem_retry(
        cwd: &std::path::Path,
        diagnostic: &SandboxDenialDiagnostic,
        denied_writable_roots: &[PathBuf],
    ) -> bool {
        should_request_filesystem_permission_with_denied_roots(
            cwd,
            diagnostic,
            denied_writable_roots,
        ) || diagnostic.suggested_write_root.is_none()
    }
}

pub(crate) struct AllowRequestedPermissions;

impl RuntimePermissionRequestHandler for AllowRequestedPermissions {
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        Ok(RuntimePermissionResponse {
            decision: PermissionResponseDecision::Allow,
            scope: PermissionGrantScope::Turn,
            permissions: request.permissions.clone(),
            strict_auto_review: false,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TurnPermissionOverlay {
    additional_working_directories: Vec<PathBuf>,
    network_domain_permissions:
        std::collections::HashMap<String, orca_core::config::PermissionProfileNetworkAccess>,
    strict_auto_review: bool,
    preapproved_tool_call_id: Option<String>,
}

impl TurnPermissionOverlay {
    pub fn additional_working_directories(&self) -> &[PathBuf] {
        &self.additional_working_directories
    }

    pub fn network_domain_permissions(
        &self,
    ) -> &std::collections::HashMap<String, orca_core::config::PermissionProfileNetworkAccess> {
        &self.network_domain_permissions
    }

    pub fn strict_auto_review(&self) -> bool {
        self.strict_auto_review
    }

    pub(crate) fn set_preapproved_tool_call_id(&mut self, id: Option<String>) {
        self.preapproved_tool_call_id = id;
    }

    pub(crate) fn consume_preapproved_tool_call_id(&mut self, id: &str) -> bool {
        if self.preapproved_tool_call_id.as_deref() != Some(id) {
            return false;
        }
        self.preapproved_tool_call_id = None;
        true
    }

    pub fn merge(&mut self, other: &Self) {
        for root in &other.additional_working_directories {
            if !self.additional_working_directories.contains(root) {
                self.additional_working_directories.push(root.clone());
            }
        }
        for (domain, access) in &other.network_domain_permissions {
            self.network_domain_permissions
                .insert(domain.clone(), *access);
        }
        self.strict_auto_review |= other.strict_auto_review;
    }

    pub(crate) fn merge_network_permissions(&mut self, permissions: &RequestPermissionProfile) {
        if let Some(network) = permissions.network.as_ref() {
            for (domain, access) in &network.domains {
                self.network_domain_permissions
                    .insert(domain.clone(), *access);
            }
        }
    }

    pub(crate) fn merge_strict_auto_review(&mut self, strict_auto_review: bool) {
        self.strict_auto_review |= strict_auto_review;
    }

    pub fn request_and_merge(
        &mut self,
        handler: &dyn RuntimePermissionRequestHandler,
        request: RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        let response = handler.request_permissions(&request)?;
        if response.decision == PermissionResponseDecision::Allow {
            self.merge_permissions(&response.permissions);
            self.merge_strict_auto_review(response.strict_auto_review);
        }
        Ok(response)
    }

    pub(crate) fn merge_permissions(&mut self, permissions: &RequestPermissionProfile) {
        if let Some(file_system) = permissions.file_system.as_ref()
            && let Some(write_roots) = file_system.write.as_ref()
        {
            for root in write_roots {
                if !root.as_os_str().is_empty()
                    && !self.additional_working_directories.contains(root)
                {
                    self.additional_working_directories.push(root.clone());
                }
            }
        }
        self.merge_network_permissions(permissions);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use orca_core::approval_types::ApprovalMode;

    use crate::network_proxy::RuntimeNetworkBlockReport;
    use crate::sandbox_denial::SandboxDenialDiagnostic;

    use super::{
        RuntimePermissionOrigin, RuntimePermissionPolicy, RuntimePermissionPromptDecision,
        TurnPermissionOverlay,
    };

    #[test]
    fn preapproved_tool_call_id_is_consumed_once_for_exact_match_only() {
        let mut overlay = TurnPermissionOverlay::default();
        overlay.set_preapproved_tool_call_id(Some("tool-1".to_string()));

        assert!(!overlay.consume_preapproved_tool_call_id("tool-2"));
        assert!(overlay.consume_preapproved_tool_call_id("tool-1"));
        assert!(!overlay.consume_preapproved_tool_call_id("tool-1"));
    }

    #[test]
    fn runtime_permission_policy_skips_denylist_network_blocks() {
        let block = RuntimeNetworkBlockReport {
            host: "blocked.orca.invalid".to_string(),
            error: "blocked-by-denylist",
        };

        assert!(
            RuntimePermissionPolicy::network_block_request(
                "permission-1",
                RuntimePermissionOrigin::Bash,
                &block,
            )
            .is_none()
        );
    }

    #[test]
    fn runtime_permission_policy_builds_actor_scoped_network_request() {
        let block = RuntimeNetworkBlockReport {
            host: "api.orca.invalid".to_string(),
            error: "blocked-by-allowlist",
        };

        let bash_request = RuntimePermissionPolicy::network_block_request(
            "permission-1",
            RuntimePermissionOrigin::Bash,
            &block,
        )
        .expect("bash network request");
        let command_request = RuntimePermissionPolicy::network_block_request(
            "permission-2",
            RuntimePermissionOrigin::CommandExec,
            &block,
        )
        .expect("command/exec network request");

        assert_eq!(
            bash_request.reason.as_deref(),
            Some("bash attempted network access to api.orca.invalid (blocked-by-allowlist)")
        );
        assert_eq!(
            command_request.reason.as_deref(),
            Some(
                "command/exec attempted network access to api.orca.invalid (blocked-by-allowlist)"
            )
        );
        assert_eq!(
            command_request
                .permissions
                .network
                .as_ref()
                .and_then(|network| network.domains.get("api.orca.invalid")),
            Some(&orca_core::config::PermissionProfileNetworkAccess::Allow)
        );
    }

    #[test]
    fn runtime_permission_policy_builds_sandbox_denial_requests() {
        let write_diagnostic = SandboxDenialDiagnostic {
            denied_path: Some(PathBuf::from("/repo/.git/index.lock")),
            suggested_write_root: Some(PathBuf::from("/repo/.git")),
            message: "sandbox denied filesystem access".to_string(),
        };
        let pathless_diagnostic = SandboxDenialDiagnostic {
            denied_path: None,
            suggested_write_root: None,
            message: "sandbox denied filesystem access".to_string(),
        };

        let write_request = RuntimePermissionPolicy::sandbox_denial_request(
            "permission-1",
            RuntimePermissionOrigin::Bash,
            &write_diagnostic,
        );
        let unsandboxed_request = RuntimePermissionPolicy::sandbox_denial_request(
            "permission-2",
            RuntimePermissionOrigin::CommandExec,
            &pathless_diagnostic,
        );

        assert_eq!(
            write_request
                .permissions
                .file_system
                .as_ref()
                .and_then(|file_system| file_system.write.as_ref()),
            Some(&vec![PathBuf::from("/repo/.git")])
        );
        assert_eq!(
            write_request.reason.as_deref(),
            Some("bash attempted filesystem write outside the current sandbox: /repo/.git")
        );
        assert_eq!(
            unsandboxed_request
                .permissions
                .shell
                .as_ref()
                .map(|shell| shell.unsandboxed),
            Some(true)
        );
        assert_eq!(
            unsandboxed_request.reason.as_deref(),
            Some(
                "command/exec needs to re-run without the filesystem sandbox because the sandbox denied access but did not report a filesystem path to grant"
            )
        );
    }

    #[test]
    fn runtime_permission_policy_decides_request_permissions_prompt_gate() {
        assert_eq!(
            RuntimePermissionPolicy::decide_request_permissions_prompt(
                ApprovalMode::FullAuto,
                false
            ),
            RuntimePermissionPromptDecision::AutoAllow
        );
        assert_eq!(
            RuntimePermissionPolicy::decide_request_permissions_prompt(ApprovalMode::Suggest, true),
            RuntimePermissionPromptDecision::Prompt
        );
        assert_eq!(
            RuntimePermissionPolicy::decide_request_permissions_prompt(
                ApprovalMode::AutoEdit,
                false
            ),
            RuntimePermissionPromptDecision::Reject {
                reason: "request_permissions requires a runtime permission handler unless approval mode is full-auto",
            }
        );
    }
}
