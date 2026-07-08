use orca_core::approval_types::ApprovalRequest;
use orca_core::tool_types::ToolRequest;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::lifecycle::{RuntimePermissionRequest, RuntimeUserInputRequest};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimePendingInteractionKind {
    ToolApproval,
    PermissionRequest,
    UserInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePendingInteractionRecord {
    pub id: String,
    pub kind: RuntimePendingInteractionKind,
    pub tool: Option<String>,
    pub target: Option<String>,
    pub preview: Option<String>,
    pub question: Option<String>,
    pub choices: Vec<String>,
}

impl RuntimePendingInteractionRecord {
    pub fn from_tool_approval(approval: &ApprovalRequest, request: &ToolRequest) -> Self {
        Self {
            id: approval.id.clone(),
            kind: RuntimePendingInteractionKind::ToolApproval,
            tool: approval
                .tool
                .clone()
                .or_else(|| Some(request.name.as_str().to_string())),
            target: approval.target.clone().or_else(|| request.target.clone()),
            preview: approval.preview.clone(),
            question: None,
            choices: Vec::new(),
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
            tool: Some(tool.into()),
            target: target.or_else(|| request.reason.clone()),
            preview,
            question: None,
            choices: Vec::new(),
        }
    }

    pub fn from_user_input(request: &RuntimeUserInputRequest) -> Self {
        Self {
            id: request.id.clone(),
            kind: RuntimePendingInteractionKind::UserInput,
            tool: None,
            target: None,
            preview: None,
            question: Some(request.question.clone()),
            choices: request.choices.clone(),
        }
    }
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
