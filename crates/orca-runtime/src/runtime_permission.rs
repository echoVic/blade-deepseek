use std::io;
use std::path::PathBuf;

use crate::protocol::{PermissionGrantScope, PermissionResponseDecision, RequestPermissionProfile};

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
