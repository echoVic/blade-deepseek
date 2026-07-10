use orca_core::approval_types::ApprovalRequest;
use orca_core::tool_types::ToolRequest;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::lifecycle::{RuntimePermissionRequest, RuntimeUserInputRequest};
use crate::runtime_permission::RuntimePermissionRequestKind;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimePendingInteractionKind {
    ToolApproval,
    PermissionRequest,
    UserInput,
    McpElicitation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeMcpElicitationMode {
    Form,
    Url,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeMcpElicitationRequest {
    pub id: String,
    pub server_name: String,
    pub request_id: String,
    pub mode: RuntimeMcpElicitationMode,
    pub message: String,
    pub url: Option<String>,
    pub requested_schema_json: Option<String>,
}

impl RuntimeMcpElicitationRequest {
    pub fn new(
        server_name: impl Into<String>,
        request_id: impl Into<String>,
        mode: RuntimeMcpElicitationMode,
        message: impl Into<String>,
        url: Option<String>,
        requested_schema_json: Option<String>,
    ) -> Self {
        let server_name = server_name.into();
        let request_id = request_id.into();
        Self {
            id: mcp_elicitation_pending_id(None, &server_name, &request_id),
            server_name,
            request_id,
            mode,
            message: message.into(),
            url,
            requested_schema_json,
        }
    }

    pub fn new_scoped(
        scope: impl AsRef<str>,
        server_name: impl Into<String>,
        request_id: impl Into<String>,
        mode: RuntimeMcpElicitationMode,
        message: impl Into<String>,
        url: Option<String>,
        requested_schema_json: Option<String>,
    ) -> Self {
        let server_name = server_name.into();
        let request_id = request_id.into();
        Self {
            id: mcp_elicitation_pending_id(Some(scope.as_ref()), &server_name, &request_id),
            server_name,
            request_id,
            mode,
            message: message.into(),
            url,
            requested_schema_json,
        }
    }
}

fn mcp_elicitation_pending_id(scope: Option<&str>, server_name: &str, request_id: &str) -> String {
    match scope {
        Some(scope) => format!("mcp_elicitation:{scope}:{server_name}:{request_id}"),
        None => format!("mcp_elicitation:{server_name}:{request_id}"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeMcpElicitationRecord {
    pub server_name: String,
    pub request_id: String,
    pub mode: RuntimeMcpElicitationMode,
    pub url: Option<String>,
    pub requested_schema_json: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePendingInteractionRecord {
    pub id: String,
    pub kind: RuntimePendingInteractionKind,
    pub permission_kind: Option<RuntimePermissionRequestKind>,
    pub tool: Option<String>,
    pub target: Option<String>,
    pub preview: Option<String>,
    pub question: Option<String>,
    pub choices: Vec<String>,
    pub mcp_elicitation: Option<RuntimeMcpElicitationRecord>,
}

impl RuntimePendingInteractionRecord {
    pub fn from_tool_approval(approval: &ApprovalRequest, request: &ToolRequest) -> Self {
        Self {
            id: approval.id.clone(),
            kind: RuntimePendingInteractionKind::ToolApproval,
            permission_kind: None,
            tool: approval
                .tool
                .clone()
                .or_else(|| Some(request.name.as_str().to_string())),
            target: approval.target.clone().or_else(|| request.target.clone()),
            preview: approval.preview.clone(),
            question: None,
            choices: Vec::new(),
            mcp_elicitation: None,
        }
    }

    pub fn from_permission_request(
        request: &RuntimePermissionRequest,
        tool: impl Into<String>,
        target: Option<String>,
        preview: Option<String>,
    ) -> Self {
        Self {
            id: request.id.clone(),
            kind: RuntimePendingInteractionKind::PermissionRequest,
            permission_kind: permission_kind_from_request(request),
            tool: Some(tool.into()),
            target: target.or_else(|| request.reason.clone()),
            preview,
            question: None,
            choices: Vec::new(),
            mcp_elicitation: None,
        }
    }

    pub fn from_user_input(request: &RuntimeUserInputRequest) -> Self {
        Self {
            id: request.id.clone(),
            kind: RuntimePendingInteractionKind::UserInput,
            permission_kind: None,
            tool: None,
            target: None,
            preview: None,
            question: Some(request.question.clone()),
            choices: request.choices.clone(),
            mcp_elicitation: None,
        }
    }

    pub fn from_mcp_elicitation(request: &RuntimeMcpElicitationRequest) -> Self {
        Self {
            id: request.id.clone(),
            kind: RuntimePendingInteractionKind::McpElicitation,
            permission_kind: None,
            tool: Some("mcp_elicitation".to_string()),
            target: Some(request.server_name.clone()),
            preview: request.url.clone(),
            question: Some(request.message.clone()),
            choices: Vec::new(),
            mcp_elicitation: Some(RuntimeMcpElicitationRecord {
                server_name: request.server_name.clone(),
                request_id: request.request_id.clone(),
                mode: request.mode.clone(),
                url: request.url.clone(),
                requested_schema_json: request.requested_schema_json.clone(),
            }),
        }
    }
}

fn permission_kind_from_request(
    request: &RuntimePermissionRequest,
) -> Option<RuntimePermissionRequestKind> {
    if request
        .permissions
        .network
        .as_ref()
        .is_some_and(|network| network.enabled.is_some() || !network.domains.is_empty())
    {
        return Some(RuntimePermissionRequestKind::NetworkBlock);
    }
    if request
        .permissions
        .file_system
        .as_ref()
        .is_some_and(|file_system| {
            file_system
                .write
                .as_ref()
                .is_some_and(|roots| !roots.is_empty())
        })
    {
        return Some(RuntimePermissionRequestKind::FilesystemWrite);
    }
    if request
        .permissions
        .shell
        .as_ref()
        .is_some_and(|shell| shell.unsandboxed)
    {
        return Some(RuntimePermissionRequestKind::UnsandboxedShellRetry);
    }
    None
}

#[derive(Clone, Default)]
pub struct RuntimePendingInteractionStore {
    pending: Arc<Mutex<HashMap<String, RuntimePendingInteractionRecord>>>,
}

impl RuntimePendingInteractionStore {
    pub fn insert(
        &self,
        record: RuntimePendingInteractionRecord,
    ) -> Result<(), RuntimePendingInteractionRecord> {
        let mut pending = self
            .pending
            .lock()
            .expect("pending interaction store poisoned");
        if pending.contains_key(&record.id) {
            return Err(record);
        }
        pending.insert(record.id.clone(), record);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<RuntimePendingInteractionRecord> {
        self.pending
            .lock()
            .expect("pending interaction store poisoned")
            .get(id)
            .cloned()
    }

    pub fn remove(&self, id: &str) -> Option<RuntimePendingInteractionRecord> {
        self.pending
            .lock()
            .expect("pending interaction store poisoned")
            .remove(id)
    }

    pub fn list(&self) -> Vec<RuntimePendingInteractionRecord> {
        let mut records = self
            .pending
            .lock()
            .expect("pending interaction store poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.id.cmp(&right.id));
        records
    }

    pub fn is_empty(&self) -> bool {
        self.pending
            .lock()
            .expect("pending interaction store poisoned")
            .is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::ToolName;

    use crate::protocol::{RequestFileSystemPermissions, RequestPermissionProfile};

    use super::*;

    #[test]
    fn pending_interaction_record_preserves_tool_approval_display_fields() {
        let approval = ApprovalRequest {
            id: "approval-1".to_string(),
            action: ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("echo hi".to_string()),
            preview: Some("$ echo hi".to_string()),
        };
        let request = ToolRequest {
            id: "call-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("fallback".to_string()),
            raw_arguments: None,
        };

        let record = RuntimePendingInteractionRecord::from_tool_approval(&approval, &request);

        assert_eq!(record.id, "approval-1");
        assert_eq!(record.kind, RuntimePendingInteractionKind::ToolApproval);
        assert_eq!(record.tool.as_deref(), Some("bash"));
        assert_eq!(record.target.as_deref(), Some("echo hi"));
        assert_eq!(record.preview.as_deref(), Some("$ echo hi"));
    }

    #[test]
    fn pending_interaction_record_describes_permission_request_display() {
        let request = RuntimePermissionRequest {
            id: "permission-1".to_string(),
            reason: Some("need repo write".to_string()),
            permissions: RequestPermissionProfile {
                file_system: Some(RequestFileSystemPermissions {
                    write: Some(vec![PathBuf::from("src")]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        };

        let record = RuntimePendingInteractionRecord::from_permission_request(
            &request,
            "request_permissions",
            None,
            Some("+ write src".to_string()),
        );

        assert_eq!(record.id, "permission-1");
        assert_eq!(
            record.kind,
            RuntimePendingInteractionKind::PermissionRequest
        );
        assert_eq!(record.tool.as_deref(), Some("request_permissions"));
        assert_eq!(record.target.as_deref(), Some("need repo write"));
        assert_eq!(record.preview.as_deref(), Some("+ write src"));
        assert_eq!(
            record.permission_kind,
            Some(crate::runtime_permission::RuntimePermissionRequestKind::FilesystemWrite)
        );
    }

    #[test]
    fn pending_interaction_record_describes_user_input_request() {
        let request = RuntimeUserInputRequest {
            id: "input-1".to_string(),
            question: "Choose?".to_string(),
            choices: vec!["A".to_string(), "B".to_string()],
        };

        let record = RuntimePendingInteractionRecord::from_user_input(&request);

        assert_eq!(record.id, "input-1");
        assert_eq!(record.kind, RuntimePendingInteractionKind::UserInput);
        assert_eq!(record.question.as_deref(), Some("Choose?"));
        assert_eq!(record.choices, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn pending_interaction_record_describes_mcp_elicitation_request() {
        let request = RuntimeMcpElicitationRequest {
            id: "mcp-1".to_string(),
            server_name: "github".to_string(),
            request_id: "42".to_string(),
            mode: RuntimeMcpElicitationMode::Url,
            message: "Authorize GitHub".to_string(),
            url: Some("https://github.com/login/device".to_string()),
            requested_schema_json: Some(r#"{"type":"object"}"#.to_string()),
        };

        let record = RuntimePendingInteractionRecord::from_mcp_elicitation(&request);

        assert_eq!(record.id, "mcp-1");
        assert_eq!(record.kind, RuntimePendingInteractionKind::McpElicitation);
        assert_eq!(record.question.as_deref(), Some("Authorize GitHub"));
        let elicitation = record
            .mcp_elicitation
            .as_ref()
            .expect("mcp elicitation detail is recorded");
        assert_eq!(elicitation.server_name, "github");
        assert_eq!(elicitation.request_id, "42");
        assert_eq!(elicitation.mode, RuntimeMcpElicitationMode::Url);
        assert_eq!(
            elicitation.url.as_deref(),
            Some("https://github.com/login/device")
        );
        assert_eq!(
            elicitation.requested_schema_json.as_deref(),
            Some(r#"{"type":"object"}"#)
        );
    }

    #[test]
    fn mcp_elicitation_request_uses_server_scoped_request_id_as_pending_id() {
        let request = RuntimeMcpElicitationRequest::new(
            "github",
            "42",
            RuntimeMcpElicitationMode::Form,
            "Authorize GitHub",
            None,
            Some(r#"{"type":"object"}"#.to_string()),
        );

        assert_eq!(request.id, "mcp_elicitation:github:42");
        assert_eq!(request.server_name, "github");
        assert_eq!(request.request_id, "42");
    }

    #[test]
    fn mcp_elicitation_request_can_scope_pending_id_to_active_turn() {
        let request = RuntimeMcpElicitationRequest::new_scoped(
            "turn-1",
            "github",
            "42",
            RuntimeMcpElicitationMode::Form,
            "Authorize GitHub",
            None,
            Some(r#"{"type":"object"}"#.to_string()),
        );

        assert_eq!(request.id, "mcp_elicitation:turn-1:github:42");
        assert_eq!(request.server_name, "github");
        assert_eq!(request.request_id, "42");
    }

    #[test]
    fn pending_interaction_store_rejects_duplicate_mcp_elicitation_id_without_overwriting() {
        let store = RuntimePendingInteractionStore::default();
        let first = RuntimePendingInteractionRecord::from_mcp_elicitation(
            &RuntimeMcpElicitationRequest::new(
                "github",
                "42",
                RuntimeMcpElicitationMode::Form,
                "Authorize GitHub",
                None,
                None,
            ),
        );
        let duplicate = RuntimePendingInteractionRecord::from_mcp_elicitation(
            &RuntimeMcpElicitationRequest::new(
                "github",
                "42",
                RuntimeMcpElicitationMode::Url,
                "Open browser",
                Some("https://github.com/login/device".to_string()),
                None,
            ),
        );

        assert!(store.insert(first.clone()).is_ok());
        assert_eq!(store.insert(duplicate.clone()), Err(duplicate));
        assert_eq!(store.get("mcp_elicitation:github:42"), Some(first));
    }

    #[test]
    fn pending_interaction_store_tracks_records_by_request_id() {
        let store = RuntimePendingInteractionStore::default();
        let request = RuntimeUserInputRequest {
            id: "input-1".to_string(),
            question: "Choose?".to_string(),
            choices: Vec::new(),
        };
        let record = RuntimePendingInteractionRecord::from_user_input(&request);

        assert!(store.is_empty());
        assert!(store.insert(record.clone()).is_ok());
        assert_eq!(store.get("input-1"), Some(record.clone()));
        assert_eq!(store.list(), vec![record.clone()]);
        assert_eq!(store.remove("input-1"), Some(record));
        assert!(store.is_empty());
    }

    #[test]
    fn pending_interaction_store_rejects_duplicate_request_id_without_overwriting() {
        let store = RuntimePendingInteractionStore::default();
        let first = RuntimePendingInteractionRecord::from_user_input(&RuntimeUserInputRequest {
            id: "input-1".to_string(),
            question: "First?".to_string(),
            choices: Vec::new(),
        });
        let duplicate =
            RuntimePendingInteractionRecord::from_user_input(&RuntimeUserInputRequest {
                id: "input-1".to_string(),
                question: "Second?".to_string(),
                choices: Vec::new(),
            });

        assert!(store.insert(first.clone()).is_ok());
        assert_eq!(store.insert(duplicate.clone()), Err(duplicate));
        assert_eq!(store.get("input-1"), Some(first));
    }
}
