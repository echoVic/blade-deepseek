use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::sync::mpsc::{Receiver, Sender};

use orca_approval::ApprovalPolicy;
use orca_core::approval_types::{ApprovalDecision, ApprovalRequest, ApprovalResolution};
use orca_core::tool_types;
use orca_runtime::lifecycle::{
    RuntimeApprovalDecision, RuntimeApprovalHandler, RuntimePermissionRequest,
    RuntimePermissionRequestHandler, RuntimePermissionResponse, RuntimeToolActorContext,
    RuntimeUserInputHandler, RuntimeUserInputRequest,
};
use orca_runtime::protocol::{PermissionGrantScope, PermissionResponseDecision};
use orca_runtime::runtime_pending_interaction::{
    RuntimePendingInteractionRecord, RuntimePendingInteractionStore,
};
use orca_runtime::tool_invocation::{ToolInvocation, approval_request_for_invocation};

use crate::types::{TuiEvent, UserAction};

pub(crate) enum TuiToolApprovalOutcome {
    Continue,
    Denied(tool_types::ToolResult),
}

pub(crate) struct TuiApprovalHandler<'a> {
    action_rx: &'a Receiver<UserAction>,
    pending_actions: &'a RefCell<VecDeque<UserAction>>,
}

impl<'a> TuiApprovalHandler<'a> {
    pub(crate) fn new(
        action_rx: &'a Receiver<UserAction>,
        pending_actions: &'a RefCell<VecDeque<UserAction>>,
    ) -> Self {
        Self {
            action_rx,
            pending_actions,
        }
    }
}

impl RuntimeApprovalHandler for TuiApprovalHandler<'_> {
    fn resolve_interactive(
        &self,
        approval: &ApprovalRequest,
        _request: &tool_types::ToolRequest,
    ) -> std::io::Result<ApprovalResolution> {
        let allowed = loop {
            match self.action_rx.recv() {
                Ok(UserAction::Approve { id, approved }) if id == approval.id => break approved,
                Ok(action @ UserAction::Approve { .. }) => {
                    self.pending_actions.borrow_mut().push_back(action)
                }
                Ok(UserAction::Interrupt) | Ok(UserAction::Cancel) | Err(_) => break false,
                Ok(action) => self.pending_actions.borrow_mut().push_back(action),
            }
        };
        Ok(ApprovalResolution {
            id: approval.id.clone(),
            decision: if allowed {
                ApprovalDecision::Allow
            } else {
                ApprovalDecision::Deny
            },
            reason: if allowed {
                "user approved".to_string()
            } else {
                "user denied".to_string()
            },
        })
    }
}

pub(crate) fn resolve_tui_tool_approval(
    invocation: &ToolInvocation,
    tool_request: &tool_types::ToolRequest,
    policy: &ApprovalPolicy,
    runtime_context: &mut RuntimeToolActorContext,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    pending_interactions: Option<&RuntimePendingInteractionStore>,
) -> TuiToolApprovalOutcome {
    let Some(approval) = approval_request_for_invocation(invocation) else {
        return TuiToolApprovalOutcome::Continue;
    };
    if !orca_runtime::agent_common::requires_approval(approval.action) {
        return TuiToolApprovalOutcome::Continue;
    }

    let approval_decision =
        runtime_context.resolve_tool_approval(policy, Some(approval.clone()), tool_request);
    match approval_decision {
        RuntimeApprovalDecision::Allowed(_) | RuntimeApprovalDecision::NotRequired => {
            TuiToolApprovalOutcome::Continue
        }
        RuntimeApprovalDecision::Ask(approval) => {
            let mut approval = approval.clone();
            approval.preview = build_approval_preview(tool_request);
            let pending =
                RuntimePendingInteractionRecord::from_tool_approval(&approval, tool_request);
            if let Err(error) = insert_pending_interaction(pending_interactions, pending.clone()) {
                return TuiToolApprovalOutcome::Denied(tool_types::ToolResult::denied(
                    tool_request,
                    error.to_string(),
                ));
            }
            let _ = event_tx.send(approval_event_from_pending_interaction(&pending));

            let handler = TuiApprovalHandler::new(action_rx, pending_actions);
            let resolution = runtime_context
                .resolve_interactive_tool_approval(&handler, &approval, tool_request)
                .unwrap_or_else(|error| ApprovalResolution {
                    id: approval.id.clone(),
                    decision: ApprovalDecision::Deny,
                    reason: format!("interactive approval failed: {error}"),
                });
            remove_pending_interaction(pending_interactions, &approval.id);

            if resolution.decision == ApprovalDecision::Deny {
                TuiToolApprovalOutcome::Denied(tool_types::ToolResult::denied(
                    tool_request,
                    resolution.reason,
                ))
            } else {
                TuiToolApprovalOutcome::Continue
            }
        }
        RuntimeApprovalDecision::Denied { result, .. } => TuiToolApprovalOutcome::Denied(result),
    }
}

/// Build a human-readable preview of what a tool call will do, parsed from its
/// raw JSON arguments. Returns `None` when there is nothing meaningful to show.
/// This is best-effort: the strings come straight from the pending request, so
/// the diff/command shown is exactly what would run.
fn build_approval_preview(request: &tool_types::ToolRequest) -> Option<String> {
    use orca_core::tool_types::ToolName;

    let raw = request.raw_arguments.as_deref()?;
    let args: serde_json::Value = serde_json::from_str(raw).ok()?;

    match &request.name {
        ToolName::Edit => {
            let path = args["path"].as_str().unwrap_or("(file)");
            let old_text = args["old_text"].as_str().unwrap_or_default();
            let new_text = args["new_text"].as_str().unwrap_or_default();
            let mut out = format!("@@ {path} @@\n");
            for line in old_text.lines() {
                out.push_str(&format!("- {line}\n"));
            }
            for line in new_text.lines() {
                out.push_str(&format!("+ {line}\n"));
            }
            Some(out.trim_end().to_string())
        }
        ToolName::WriteFile => {
            let path = args["path"].as_str().unwrap_or("(file)");
            let content = args["content"]
                .as_str()
                .or_else(|| args["contents"].as_str())
                .unwrap_or_default();
            let mut out = format!("@@ write {path} @@\n");
            for line in content.lines().take(40) {
                out.push_str(&format!("+ {line}\n"));
            }
            let total = content.lines().count();
            if total > 40 {
                out.push_str(&format!("+ … (+{} more lines)\n", total - 40));
            }
            Some(out.trim_end().to_string())
        }
        ToolName::Bash => {
            let command = args["command"].as_str().or_else(|| args.as_str())?;
            Some(format!("$ {command}"))
        }
        _ => None,
    }
}

/// Routes runtime permission requests (sandbox escalations and the
/// `request_permissions` tool) through the TUI approval channel. Display
/// fields can be overridden so callers like the bash sandbox escalation can
/// show the failing command instead of the generic permission summary.
pub(crate) struct TuiPermissionRequestHandler<'a> {
    event_tx: &'a Sender<TuiEvent>,
    action_rx: &'a Receiver<UserAction>,
    pending_actions: &'a RefCell<VecDeque<UserAction>>,
    tool: String,
    target: Option<String>,
    preview: Option<String>,
    pending_interactions: Option<RuntimePendingInteractionStore>,
}

impl<'a> TuiPermissionRequestHandler<'a> {
    pub(crate) fn new(
        event_tx: &'a Sender<TuiEvent>,
        action_rx: &'a Receiver<UserAction>,
        pending_actions: &'a RefCell<VecDeque<UserAction>>,
    ) -> Self {
        Self {
            event_tx,
            action_rx,
            pending_actions,
            tool: "request_permissions".to_string(),
            target: None,
            preview: None,
            pending_interactions: None,
        }
    }

    pub(crate) fn with_display(
        mut self,
        tool: impl Into<String>,
        target: Option<String>,
        preview: Option<String>,
    ) -> Self {
        self.tool = tool.into();
        self.target = target;
        self.preview = preview;
        self
    }

    pub(crate) fn with_pending_interactions(
        mut self,
        store: RuntimePendingInteractionStore,
    ) -> Self {
        self.pending_interactions = Some(store);
        self
    }
}

impl RuntimePermissionRequestHandler for TuiPermissionRequestHandler<'_> {
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> std::io::Result<RuntimePermissionResponse> {
        let preview = self
            .preview
            .clone()
            .unwrap_or_else(|| describe_permission_request(request));
        let pending = RuntimePendingInteractionRecord::from_permission_request(
            request,
            self.tool.clone(),
            self.target.clone(),
            Some(preview),
        );
        insert_pending_interaction(self.pending_interactions.as_ref(), pending.clone())?;
        let _ = self
            .event_tx
            .send(approval_event_from_pending_interaction(&pending));
        let allowed = loop {
            match self.action_rx.recv() {
                Ok(UserAction::Approve { id, approved }) if id == request.id => break approved,
                Ok(action @ UserAction::Approve { .. }) => {
                    self.pending_actions.borrow_mut().push_back(action)
                }
                Ok(UserAction::Interrupt) | Ok(UserAction::Cancel) | Err(_) => break false,
                Ok(action) => self.pending_actions.borrow_mut().push_back(action),
            }
        };
        remove_pending_interaction(self.pending_interactions.as_ref(), &request.id);
        Ok(RuntimePermissionResponse {
            decision: if allowed {
                PermissionResponseDecision::Allow
            } else {
                PermissionResponseDecision::Deny
            },
            scope: PermissionGrantScope::Turn,
            permissions: request.permissions.clone(),
            strict_auto_review: false,
        })
    }
}

/// Grants whatever was requested without prompting; used when the approval
/// policy already resolved the escalation to Allow (e.g. full-auto mode).
pub(crate) struct AutoAllowPermissionRequests;

impl RuntimePermissionRequestHandler for AutoAllowPermissionRequests {
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> std::io::Result<RuntimePermissionResponse> {
        Ok(RuntimePermissionResponse {
            decision: PermissionResponseDecision::Allow,
            scope: PermissionGrantScope::Turn,
            permissions: request.permissions.clone(),
            strict_auto_review: false,
        })
    }
}

fn describe_permission_request(request: &RuntimePermissionRequest) -> String {
    let mut lines = Vec::new();
    if let Some(reason) = &request.reason {
        lines.push(reason.clone());
    }
    if let Some(file_system) = &request.permissions.file_system {
        for root in file_system.read.iter().flatten() {
            lines.push(format!("+ read {}", root.display()));
        }
        for root in file_system.write.iter().flatten() {
            lines.push(format!("+ write {}", root.display()));
        }
    }
    if let Some(network) = &request.permissions.network {
        if let Some(enabled) = network.enabled {
            lines.push(format!(
                "+ network {}",
                if enabled { "enabled" } else { "disabled" }
            ));
        }
        for (domain, access) in &network.domains {
            lines.push(format!("+ network domain {domain}: {access:?}"));
        }
    }
    if request
        .permissions
        .shell
        .as_ref()
        .is_some_and(|shell| shell.unsandboxed)
    {
        lines.push("+ shell without filesystem sandbox".to_string());
    }
    if lines.is_empty() {
        lines.push("(no specific permissions requested)".to_string());
    }
    lines.join("\n")
}

pub(crate) struct TuiUserInputHandler<'a> {
    event_tx: &'a Sender<TuiEvent>,
    action_rx: &'a Receiver<UserAction>,
    pending_actions: &'a RefCell<VecDeque<UserAction>>,
    pending_interactions: Option<RuntimePendingInteractionStore>,
}

impl<'a> TuiUserInputHandler<'a> {
    pub(crate) fn new(
        event_tx: &'a Sender<TuiEvent>,
        action_rx: &'a Receiver<UserAction>,
        pending_actions: &'a RefCell<VecDeque<UserAction>>,
    ) -> Self {
        Self {
            event_tx,
            action_rx,
            pending_actions,
            pending_interactions: None,
        }
    }

    pub(crate) fn with_pending_interactions(
        mut self,
        store: RuntimePendingInteractionStore,
    ) -> Self {
        self.pending_interactions = Some(store);
        self
    }
}

impl RuntimeUserInputHandler for TuiUserInputHandler<'_> {
    fn request_user_input(
        &self,
        request: &RuntimeUserInputRequest,
    ) -> std::io::Result<Option<String>> {
        let pending = RuntimePendingInteractionRecord::from_user_input(request);
        insert_pending_interaction(self.pending_interactions.as_ref(), pending.clone())?;
        let _ = self
            .event_tx
            .send(user_input_event_from_pending_interaction(&pending));

        let response = loop {
            match self.action_rx.recv() {
                Ok(UserAction::RespondToUserInput { id, answer }) if id == request.id => {
                    break Ok(Some(answer));
                }
                Ok(action @ UserAction::RespondToUserInput { .. }) => {
                    self.pending_actions.borrow_mut().push_back(action)
                }
                Ok(UserAction::Interrupt) | Ok(UserAction::Cancel) | Err(_) => break Ok(None),
                Ok(action) => self.pending_actions.borrow_mut().push_back(action),
            }
        };
        remove_pending_interaction(self.pending_interactions.as_ref(), &request.id);
        response
    }
}

fn insert_pending_interaction(
    store: Option<&RuntimePendingInteractionStore>,
    record: RuntimePendingInteractionRecord,
) -> io::Result<()> {
    if let Some(store) = store {
        let id = record.id.clone();
        store.insert(record).map_err(|_| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate pending interaction id: {id}"),
            )
        })?;
    }
    Ok(())
}

fn remove_pending_interaction(store: Option<&RuntimePendingInteractionStore>, id: &str) {
    if let Some(store) = store {
        store.remove(id);
    }
}

fn approval_event_from_pending_interaction(record: &RuntimePendingInteractionRecord) -> TuiEvent {
    if let Some(permission_kind) = record.permission_kind {
        return TuiEvent::PermissionApprovalNeeded {
            id: record.id.clone(),
            tool: record.tool.clone().unwrap_or_default(),
            target: record.target.clone(),
            preview: record.preview.clone(),
            permission_kind,
        };
    }

    TuiEvent::ApprovalNeeded {
        id: record.id.clone(),
        tool: record.tool.clone().unwrap_or_default(),
        target: record.target.clone(),
        preview: record.preview.clone(),
    }
}

fn user_input_event_from_pending_interaction(record: &RuntimePendingInteractionRecord) -> TuiEvent {
    TuiEvent::UserInputRequested {
        id: record.id.clone(),
        question: record.question.clone().unwrap_or_default(),
        choices: record.choices.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::mpsc;

    use orca_runtime::lifecycle::RuntimeToolActorContext;
    use orca_runtime::runtime_pending_interaction::{
        RuntimePendingInteractionKind, RuntimePendingInteractionStore,
    };

    use super::*;

    #[test]
    fn tui_approval_handler_resolves_approve_action_through_runtime_context() {
        let (action_tx, action_rx) = mpsc::channel();
        action_tx
            .send(UserAction::Approve {
                id: "approval-1".to_string(),
                approved: true,
            })
            .expect("send approval");
        let pending_actions = RefCell::new(VecDeque::new());
        let handler = TuiApprovalHandler::new(&action_rx, &pending_actions);
        let mut context = RuntimeToolActorContext::new("tui-approval", 2);
        let approval = orca_core::approval_types::ApprovalRequest {
            id: "approval-1".to_string(),
            action: orca_core::approval_types::ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("echo hi".to_string()),
            preview: Some("$ echo hi".to_string()),
        };
        let request = tool_types::ToolRequest {
            id: "bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "echo hi" }).to_string()),
        };

        let resolution = context
            .resolve_interactive_tool_approval(&handler, &approval, &request)
            .expect("approval resolution");

        assert_eq!(resolution.id, "approval-1");
        assert_eq!(
            resolution.decision,
            orca_core::approval_types::ApprovalDecision::Allow
        );
        assert_eq!(resolution.reason, "user approved");
    }

    #[test]
    fn tui_approval_handler_preserves_queued_app_actions() {
        let (action_tx, action_rx) = mpsc::channel();
        action_tx
            .send(UserAction::Submit("next prompt".to_string()))
            .expect("send queued submit");
        action_tx
            .send(UserAction::Approve {
                id: "approval-1".to_string(),
                approved: true,
            })
            .expect("send approval");
        let pending_actions = RefCell::new(VecDeque::new());
        let handler = TuiApprovalHandler::new(&action_rx, &pending_actions);
        let mut context = RuntimeToolActorContext::new("tui-approval", 2);
        let approval = orca_core::approval_types::ApprovalRequest {
            id: "approval-1".to_string(),
            action: orca_core::approval_types::ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("echo hi".to_string()),
            preview: Some("$ echo hi".to_string()),
        };
        let request = tool_types::ToolRequest {
            id: "bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "echo hi" }).to_string()),
        };

        let resolution = context
            .resolve_interactive_tool_approval(&handler, &approval, &request)
            .expect("approval resolution");

        assert_eq!(
            resolution.decision,
            orca_core::approval_types::ApprovalDecision::Allow
        );
        assert!(matches!(
            pending_actions.borrow_mut().pop_front(),
            Some(UserAction::Submit(prompt)) if prompt == "next prompt"
        ));
    }

    #[test]
    fn tui_approval_handler_resolves_only_matching_runtime_interaction_id() {
        let (action_tx, action_rx) = mpsc::channel();
        action_tx
            .send(UserAction::Approve {
                id: "approval-other".to_string(),
                approved: false,
            })
            .expect("send unrelated approval");
        action_tx
            .send(UserAction::Approve {
                id: "approval-1".to_string(),
                approved: true,
            })
            .expect("send matching approval");
        let pending_actions = RefCell::new(VecDeque::new());
        let handler = TuiApprovalHandler::new(&action_rx, &pending_actions);
        let mut context = RuntimeToolActorContext::new("tui-approval", 2);
        let approval = orca_core::approval_types::ApprovalRequest {
            id: "approval-1".to_string(),
            action: orca_core::approval_types::ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("echo hi".to_string()),
            preview: Some("$ echo hi".to_string()),
        };
        let request = tool_types::ToolRequest {
            id: "bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "echo hi" }).to_string()),
        };

        let resolution = context
            .resolve_interactive_tool_approval(&handler, &approval, &request)
            .expect("approval resolution");

        assert_eq!(
            resolution.decision,
            orca_core::approval_types::ApprovalDecision::Allow
        );
        assert!(matches!(
            pending_actions.borrow_mut().pop_front(),
            Some(UserAction::Approve { id, approved: false }) if id == "approval-other"
        ));
    }

    #[test]
    fn tui_tool_approval_rejects_duplicate_pending_interaction_id_before_prompting() {
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx
            .send(UserAction::Approve {
                id: "approval-bash-1".to_string(),
                approved: true,
            })
            .expect("send approval");
        let pending_actions = RefCell::new(VecDeque::new());
        let store = RuntimePendingInteractionStore::default();
        let approval = orca_core::approval_types::ApprovalRequest {
            id: "approval-bash-1".to_string(),
            action: orca_core::approval_types::ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("echo existing".to_string()),
            preview: Some("$ echo existing".to_string()),
        };
        let request = tool_types::ToolRequest {
            id: "bash-1".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("echo duplicate".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "echo duplicate" }).to_string()),
        };
        let first = RuntimePendingInteractionRecord::from_tool_approval(&approval, &request);
        store.insert(first.clone()).expect("seed pending");
        let invocation = ToolInvocation {
            requested: request.clone(),
            effective: request.clone(),
            action: Some(orca_core::approval_types::ActionKind::Shell),
        };
        let mut context = RuntimeToolActorContext::new("tui-approval", 2);

        let outcome = resolve_tui_tool_approval(
            &invocation,
            &request,
            &ApprovalPolicy::new(orca_core::approval_types::ApprovalMode::Suggest),
            &mut context,
            &event_tx,
            &action_rx,
            &pending_actions,
            Some(&store),
        );

        let TuiToolApprovalOutcome::Denied(result) = outcome else {
            panic!("duplicate pending id should deny before prompting");
        };
        assert_eq!(result.status, tool_types::ToolStatus::Denied);
        assert!(result.error.as_deref().is_some_and(|error| {
            error.contains("duplicate pending interaction id: approval-bash-1")
        }));
        assert_eq!(store.get("approval-bash-1"), Some(first));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn tui_approval_handler_maps_cancel_to_runtime_denial() {
        let (action_tx, action_rx) = mpsc::channel();
        action_tx.send(UserAction::Cancel).expect("send cancel");
        let pending_actions = RefCell::new(VecDeque::new());
        let handler = TuiApprovalHandler::new(&action_rx, &pending_actions);
        let mut context = RuntimeToolActorContext::new("tui-approval", 2);
        let approval = orca_core::approval_types::ApprovalRequest {
            id: "approval-1".to_string(),
            action: orca_core::approval_types::ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("echo hi".to_string()),
            preview: Some("$ echo hi".to_string()),
        };
        let request = tool_types::ToolRequest {
            id: "bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "echo hi" }).to_string()),
        };

        let resolution = context
            .resolve_interactive_tool_approval(&handler, &approval, &request)
            .expect("approval resolution");

        assert_eq!(resolution.id, "approval-1");
        assert_eq!(
            resolution.decision,
            orca_core::approval_types::ApprovalDecision::Deny
        );
        assert_eq!(resolution.reason, "user denied");
    }

    #[test]
    fn tui_user_input_handler_routes_answer_through_runtime_context() {
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx
            .send(UserAction::RespondToUserInput {
                id: "ask".to_string(),
                answer: "yes".to_string(),
            })
            .expect("send answer");
        let pending_actions = RefCell::new(VecDeque::new());
        let handler = TuiUserInputHandler::new(&event_tx, &action_rx, &pending_actions);
        let mut context = RuntimeToolActorContext::new("tui-user-input", 2);
        let request = tool_types::ToolRequest {
            id: "ask".to_string(),
            name: tool_types::ToolName::RequestUserInput,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(
                serde_json::json!({
                    "question": "Continue?",
                    "choices": ["yes", "no"]
                })
                .to_string(),
            ),
        };

        let result = context
            .execute_user_input_tool(&request, &handler)
            .expect("user input result");
        let events: Vec<TuiEvent> = event_rx.try_iter().collect();

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("yes"));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::UserInputRequested { id, question, choices }
                if id == "ask"
                    && question == "Continue?"
                    && choices == &vec!["yes".to_string(), "no".to_string()]
            )
        }));
    }

    #[test]
    fn tui_user_input_handler_tracks_runtime_pending_interaction_until_answered() {
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        let store = RuntimePendingInteractionStore::default();
        let request = tool_types::ToolRequest {
            id: "ask".to_string(),
            name: tool_types::ToolName::RequestUserInput,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "question": "Continue?" }).to_string()),
        };
        let worker_store = store.clone();

        let handle = std::thread::spawn(move || {
            let pending_actions = RefCell::new(VecDeque::new());
            let handler = TuiUserInputHandler::new(&event_tx, &action_rx, &pending_actions)
                .with_pending_interactions(worker_store);
            let mut context = RuntimeToolActorContext::new("tui-user-input", 2);
            context
                .execute_user_input_tool(&request, &handler)
                .expect("user input result")
        });
        let prompt = event_rx.recv().expect("user input prompt");
        assert!(matches!(
            prompt,
            TuiEvent::UserInputRequested { id, .. } if id == "ask"
        ));
        assert_eq!(
            store.get("ask").map(|record| record.kind),
            Some(RuntimePendingInteractionKind::UserInput)
        );

        action_tx
            .send(UserAction::RespondToUserInput {
                id: "ask".to_string(),
                answer: "yes".to_string(),
            })
            .expect("send answer");
        let result = handle.join().expect("user input thread");

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert!(store.is_empty());
    }

    #[test]
    fn tui_user_input_handler_rejects_duplicate_pending_interaction_id_before_waiting() {
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx.send(UserAction::Cancel).expect("send cancel");
        let store = RuntimePendingInteractionStore::default();
        let first = RuntimePendingInteractionRecord::from_user_input(&RuntimeUserInputRequest {
            id: "ask".to_string(),
            question: "Existing?".to_string(),
            choices: Vec::new(),
        });
        store.insert(first.clone()).expect("seed pending");
        let pending_actions = RefCell::new(VecDeque::new());
        let handler = TuiUserInputHandler::new(&event_tx, &action_rx, &pending_actions)
            .with_pending_interactions(store.clone());
        let mut context = RuntimeToolActorContext::new("tui-user-input", 2);
        let request = tool_types::ToolRequest {
            id: "ask".to_string(),
            name: tool_types::ToolName::RequestUserInput,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "question": "Duplicate?" }).to_string()),
        };

        let error = context
            .execute_user_input_tool(&request, &handler)
            .expect_err("duplicate pending id should fail before waiting");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(store.get("ask"), Some(first));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn tui_user_input_handler_preserves_queued_app_actions() {
        let (event_tx, _event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx
            .send(UserAction::Submit("next prompt".to_string()))
            .expect("send queued submit");
        action_tx
            .send(UserAction::RespondToUserInput {
                id: "ask".to_string(),
                answer: "yes".to_string(),
            })
            .expect("send answer");
        let pending_actions = RefCell::new(VecDeque::new());
        let handler = TuiUserInputHandler::new(&event_tx, &action_rx, &pending_actions);
        let mut context = RuntimeToolActorContext::new("tui-user-input", 2);
        let request = tool_types::ToolRequest {
            id: "ask".to_string(),
            name: tool_types::ToolName::RequestUserInput,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "question": "Continue?" }).to_string()),
        };

        let result = context
            .execute_user_input_tool(&request, &handler)
            .expect("user input result");

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert!(matches!(
            pending_actions.borrow_mut().pop_front(),
            Some(UserAction::Submit(prompt)) if prompt == "next prompt"
        ));
    }

    #[test]
    fn tui_user_input_handler_resolves_only_matching_runtime_interaction_id() {
        let (event_tx, _event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx
            .send(UserAction::RespondToUserInput {
                id: "ask-other".to_string(),
                answer: "wrong".to_string(),
            })
            .expect("send unrelated answer");
        action_tx
            .send(UserAction::RespondToUserInput {
                id: "ask".to_string(),
                answer: "yes".to_string(),
            })
            .expect("send matching answer");
        let pending_actions = RefCell::new(VecDeque::new());
        let handler = TuiUserInputHandler::new(&event_tx, &action_rx, &pending_actions);
        let mut context = RuntimeToolActorContext::new("tui-user-input", 2);
        let request = tool_types::ToolRequest {
            id: "ask".to_string(),
            name: tool_types::ToolName::RequestUserInput,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "question": "Continue?" }).to_string()),
        };

        let result = context
            .execute_user_input_tool(&request, &handler)
            .expect("user input result");

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("yes"));
        assert!(matches!(
            pending_actions.borrow_mut().pop_front(),
            Some(UserAction::RespondToUserInput { id, answer }) if id == "ask-other" && answer == "wrong"
        ));
    }

    #[test]
    fn tui_user_input_handler_maps_cancel_to_runtime_failure() {
        let (event_tx, _event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx.send(UserAction::Cancel).expect("send cancel");
        let pending_actions = RefCell::new(VecDeque::new());
        let handler = TuiUserInputHandler::new(&event_tx, &action_rx, &pending_actions);
        let mut context = RuntimeToolActorContext::new("tui-user-input", 2);
        let request = tool_types::ToolRequest {
            id: "ask".to_string(),
            name: tool_types::ToolName::RequestUserInput,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "question": "Continue?" }).to_string()),
        };

        let result = context
            .execute_user_input_tool(&request, &handler)
            .expect("user input result");

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some("user input request cancelled")
        );
    }

    #[test]
    fn tui_permission_handler_tracks_runtime_pending_interaction_until_resolved() {
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        let store = RuntimePendingInteractionStore::default();
        let request = RuntimePermissionRequest {
            id: "permission-1".to_string(),
            reason: Some("need write".to_string()),
            permissions: Default::default(),
        };
        let worker_store = store.clone();

        let handle = std::thread::spawn(move || {
            let pending_actions = RefCell::new(VecDeque::new());
            let handler = TuiPermissionRequestHandler::new(&event_tx, &action_rx, &pending_actions)
                .with_pending_interactions(worker_store);
            handler
                .request_permissions(&request)
                .expect("permission response")
        });
        let prompt = event_rx.recv().expect("approval prompt");
        assert!(matches!(
            prompt,
            TuiEvent::ApprovalNeeded { id, .. } if id == "permission-1"
        ));
        assert_eq!(
            store.get("permission-1").map(|record| record.kind),
            Some(RuntimePendingInteractionKind::PermissionRequest)
        );

        action_tx
            .send(UserAction::Approve {
                id: "permission-1".to_string(),
                approved: true,
            })
            .expect("send approval");
        let response = handle.join().expect("permission thread");

        assert_eq!(response.decision, PermissionResponseDecision::Allow);
        assert!(store.is_empty());
    }

    #[test]
    fn tui_permission_handler_projects_runtime_permission_kind() {
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        let store = RuntimePendingInteractionStore::default();
        let mut domains = std::collections::HashMap::new();
        domains.insert(
            "api.orca.invalid".to_string(),
            orca_core::config::PermissionProfileNetworkAccess::Allow,
        );
        let request = RuntimePermissionRequest {
            id: "permission-network".to_string(),
            reason: Some("bash attempted network access to api.orca.invalid".to_string()),
            permissions: orca_runtime::protocol::RequestPermissionProfile {
                file_system: None,
                network: Some(orca_runtime::protocol::RequestNetworkPermissions {
                    enabled: None,
                    domains,
                }),
                shell: None,
            },
        };
        let worker_store = store.clone();

        let handle = std::thread::spawn(move || {
            let pending_actions = RefCell::new(VecDeque::new());
            let handler = TuiPermissionRequestHandler::new(&event_tx, &action_rx, &pending_actions)
                .with_pending_interactions(worker_store);
            handler
                .request_permissions(&request)
                .expect("permission response")
        });
        let prompt = event_rx.recv().expect("approval prompt");

        assert!(matches!(
            prompt,
            TuiEvent::PermissionApprovalNeeded {
                id,
                permission_kind:
                    orca_runtime::runtime_permission::RuntimePermissionRequestKind::NetworkBlock,
                ..
            } if id == "permission-network"
        ));

        action_tx
            .send(UserAction::Approve {
                id: "permission-network".to_string(),
                approved: true,
            })
            .expect("send approval");
        let response = handle.join().expect("permission thread");

        assert_eq!(response.decision, PermissionResponseDecision::Allow);
        assert!(store.is_empty());
    }

    #[test]
    fn tui_permission_handler_rejects_duplicate_pending_interaction_id_before_waiting() {
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx.send(UserAction::Cancel).expect("send cancel");
        let store = RuntimePendingInteractionStore::default();
        let request = RuntimePermissionRequest {
            id: "permission-1".to_string(),
            reason: Some("need write".to_string()),
            permissions: Default::default(),
        };
        let first = RuntimePendingInteractionRecord::from_permission_request(
            &request,
            "request_permissions",
            None,
            Some("existing".to_string()),
        );
        store.insert(first.clone()).expect("seed pending");
        let pending_actions = RefCell::new(VecDeque::new());
        let handler = TuiPermissionRequestHandler::new(&event_tx, &action_rx, &pending_actions)
            .with_pending_interactions(store.clone());

        let error = handler
            .request_permissions(&request)
            .expect_err("duplicate pending id should fail before waiting");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(store.get("permission-1"), Some(first));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn tui_permission_handler_resolves_only_matching_runtime_interaction_id() {
        let (event_tx, _event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx
            .send(UserAction::Approve {
                id: "permission-other".to_string(),
                approved: false,
            })
            .expect("send unrelated approval");
        action_tx
            .send(UserAction::Approve {
                id: "permission-1".to_string(),
                approved: true,
            })
            .expect("send matching approval");
        let pending_actions = RefCell::new(VecDeque::new());
        let handler = TuiPermissionRequestHandler::new(&event_tx, &action_rx, &pending_actions);
        let request = RuntimePermissionRequest {
            id: "permission-1".to_string(),
            reason: Some("need write".to_string()),
            permissions: Default::default(),
        };

        let response = handler
            .request_permissions(&request)
            .expect("permission response");

        assert_eq!(response.decision, PermissionResponseDecision::Allow);
        assert!(matches!(
            pending_actions.borrow_mut().pop_front(),
            Some(UserAction::Approve { id, approved: false }) if id == "permission-other"
        ));
    }
}
