use crossbeam_channel::Sender;
use std::io;

#[cfg(test)]
use orca_approval::ApprovalPolicy;
use orca_core::approval_types::{ApprovalDecision, ApprovalRequest, ApprovalResolution};
use orca_core::tool_types;
use orca_mcp::{
    McpElicitationHandler, McpElicitationMode, McpElicitationRequest, McpElicitationResponse,
};
#[cfg(test)]
use orca_runtime::lifecycle::{RuntimeApprovalDecision, RuntimeToolActorContext};
use orca_runtime::lifecycle::{
    RuntimeApprovalHandler, RuntimePermissionRequest, RuntimePermissionRequestHandler,
    RuntimePermissionResponse, RuntimeUserInputHandler, RuntimeUserInputRequest,
};
use orca_runtime::protocol::{PermissionGrantScope, PermissionResponseDecision};
use orca_runtime::runtime_pending_interaction::{
    RuntimeMcpElicitationRequest, RuntimePendingInteractionRecord, RuntimePendingInteractionStore,
};
#[cfg(test)]
use orca_runtime::tool_invocation::{ToolInvocation, approval_request_for_invocation};

use crate::operation_controller::TuiTurnControl;
use crate::types::{TuiEvent, TuiInteractionKey, TuiInteractionKind, TuiInteractionResponse};

#[cfg(test)]
pub(crate) enum TuiToolApprovalOutcome {
    Continue,
    Denied(tool_types::ToolResult),
}

pub(crate) struct TuiApprovalHandler {
    event_tx: Sender<TuiEvent>,
    control: TuiTurnControl,
    pending_interactions: Option<RuntimePendingInteractionStore>,
}

impl TuiApprovalHandler {
    pub(crate) fn new(event_tx: Sender<TuiEvent>, control: TuiTurnControl) -> Self {
        Self {
            event_tx,
            control,
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

impl RuntimeApprovalHandler for TuiApprovalHandler {
    fn resolve_interactive(
        &self,
        approval: &ApprovalRequest,
        _request: &tool_types::ToolRequest,
    ) -> std::io::Result<ApprovalResolution> {
        let pending = RuntimePendingInteractionRecord::from_tool_approval(approval, _request);
        let waiter = self
            .control
            .register_interaction(TuiInteractionKind::Approval, &approval.id)?;
        let key = waiter.key().clone();
        let projected =
            project_pending_interaction(self.pending_interactions.as_ref(), pending.clone());
        if self
            .event_tx
            .send(approval_event_from_pending_interaction(&key, &pending))
            .is_err()
        {
            remove_projected_interaction(
                self.pending_interactions.as_ref(),
                &approval.id,
                projected,
            );
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TUI event channel closed while waiting for approval",
            ));
        }
        let response = waiter.wait();
        remove_projected_interaction(self.pending_interactions.as_ref(), &approval.id, projected);
        let allowed = match response? {
            TuiInteractionResponse::Approval(allowed) => allowed,
            _ => return Err(io::Error::other("invalid TUI approval response")),
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

#[cfg(test)]
pub(crate) fn resolve_tui_tool_approval(
    invocation: &ToolInvocation,
    tool_request: &tool_types::ToolRequest,
    policy: &ApprovalPolicy,
    runtime_context: &mut RuntimeToolActorContext,
    event_tx: &Sender<TuiEvent>,
    control: &TuiTurnControl,
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
            let handler = match pending_interactions {
                Some(store) => TuiApprovalHandler::new(event_tx.clone(), control.clone())
                    .with_pending_interactions(store.clone()),
                None => TuiApprovalHandler::new(event_tx.clone(), control.clone()),
            };
            let resolution = runtime_context
                .resolve_interactive_tool_approval(&handler, &approval, tool_request)
                .unwrap_or_else(|error| ApprovalResolution {
                    id: approval.id.clone(),
                    decision: ApprovalDecision::Deny,
                    reason: format!("interactive approval failed: {error}"),
                });
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
#[cfg(test)]
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
pub(crate) struct TuiPermissionRequestHandler {
    event_tx: Sender<TuiEvent>,
    control: TuiTurnControl,
    tool: String,
    target: Option<String>,
    preview: Option<String>,
    pending_interactions: Option<RuntimePendingInteractionStore>,
}

impl TuiPermissionRequestHandler {
    pub(crate) fn new(event_tx: Sender<TuiEvent>, control: TuiTurnControl) -> Self {
        Self {
            event_tx,
            control,
            tool: "request_permissions".to_string(),
            target: None,
            preview: None,
            pending_interactions: None,
        }
    }

    #[cfg(test)]
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

impl RuntimePermissionRequestHandler for TuiPermissionRequestHandler {
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
        let waiter = self
            .control
            .register_interaction(TuiInteractionKind::Permission, &request.id)?;
        let key = waiter.key().clone();
        let projected =
            project_pending_interaction(self.pending_interactions.as_ref(), pending.clone());
        if self
            .event_tx
            .send(approval_event_from_pending_interaction(&key, &pending))
            .is_err()
        {
            remove_projected_interaction(
                self.pending_interactions.as_ref(),
                &request.id,
                projected,
            );
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TUI event channel closed while waiting for permission",
            ));
        }
        let response = waiter.wait();
        remove_projected_interaction(self.pending_interactions.as_ref(), &request.id, projected);
        let allowed = match response {
            Ok(TuiInteractionResponse::Permission(allowed)) => allowed,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => false,
            Err(error) => return Err(error),
            Ok(_) => return Err(io::Error::other("invalid TUI permission response")),
        };
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
#[cfg(test)]
pub(crate) struct AutoAllowPermissionRequests;

#[cfg(test)]
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

pub(crate) struct TuiUserInputHandler {
    event_tx: Sender<TuiEvent>,
    control: TuiTurnControl,
    pending_interactions: Option<RuntimePendingInteractionStore>,
}

impl TuiUserInputHandler {
    pub(crate) fn new(event_tx: Sender<TuiEvent>, control: TuiTurnControl) -> Self {
        Self {
            event_tx,
            control,
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

impl RuntimeUserInputHandler for TuiUserInputHandler {
    fn request_user_input(
        &self,
        request: &RuntimeUserInputRequest,
    ) -> std::io::Result<Option<String>> {
        let pending = RuntimePendingInteractionRecord::from_user_input(request);
        let waiter = self
            .control
            .register_interaction(TuiInteractionKind::UserInput, &request.id)?;
        let key = waiter.key().clone();
        let projected =
            project_pending_interaction(self.pending_interactions.as_ref(), pending.clone());
        if self
            .event_tx
            .send(user_input_event_from_pending_interaction(&key, &pending))
            .is_err()
        {
            remove_projected_interaction(
                self.pending_interactions.as_ref(),
                &request.id,
                projected,
            );
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TUI event channel closed while waiting for user input",
            ));
        }
        let response = waiter.wait();
        remove_projected_interaction(self.pending_interactions.as_ref(), &request.id, projected);
        match response {
            Ok(TuiInteractionResponse::UserInput(answer)) => Ok(Some(answer)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(None),
            Err(error) => Err(error),
            Ok(_) => Err(io::Error::other("invalid TUI user-input response")),
        }
    }
}

pub(crate) struct TuiMcpElicitationHandler {
    event_tx: Sender<TuiEvent>,
    control: TuiTurnControl,
    pending_interactions: Option<RuntimePendingInteractionStore>,
}

impl TuiMcpElicitationHandler {
    pub(crate) fn new(event_tx: Sender<TuiEvent>, control: TuiTurnControl) -> Self {
        Self {
            event_tx,
            control,
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

    pub(crate) fn request_mcp_elicitation(
        &self,
        request: &RuntimeMcpElicitationRequest,
    ) -> io::Result<Option<String>> {
        let pending = RuntimePendingInteractionRecord::from_mcp_elicitation(request);
        let waiter = self
            .control
            .register_interaction(TuiInteractionKind::McpElicitation, &request.id)?;
        let key = waiter.key().clone();
        let projected =
            project_pending_interaction(self.pending_interactions.as_ref(), pending.clone());
        if self
            .event_tx
            .send(mcp_elicitation_event_from_pending_interaction(
                &key, &pending,
            ))
            .is_err()
        {
            remove_projected_interaction(
                self.pending_interactions.as_ref(),
                &request.id,
                projected,
            );
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TUI event channel closed while waiting for MCP elicitation",
            ));
        }
        let response = waiter.wait();
        remove_projected_interaction(self.pending_interactions.as_ref(), &request.id, projected);
        match response {
            Ok(TuiInteractionResponse::McpElicitation {
                accepted,
                content_json,
            }) => Ok(accepted.then_some(content_json.unwrap_or_else(|| "{}".to_string()))),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(None),
            Err(error) => Err(error),
            Ok(_) => Err(io::Error::other("invalid TUI MCP elicitation response")),
        }
    }
}

impl McpElicitationHandler for TuiMcpElicitationHandler {
    fn handle_elicitation(
        &self,
        request: McpElicitationRequest,
    ) -> Result<McpElicitationResponse, String> {
        let mode = match request.mode {
            McpElicitationMode::Form => {
                orca_runtime::runtime_pending_interaction::RuntimeMcpElicitationMode::Form
            }
            McpElicitationMode::Url => {
                orca_runtime::runtime_pending_interaction::RuntimeMcpElicitationMode::Url
            }
        };
        let runtime_request = RuntimeMcpElicitationRequest::new(
            request.server_name,
            request.id,
            mode,
            request.message,
            request.url,
            request.requested_schema.map(|schema| schema.to_string()),
        );
        let response = self
            .request_mcp_elicitation(&runtime_request)
            .map_err(|error| error.to_string())?;
        match response {
            Some(content) => {
                let content = serde_json::from_str(&content)
                    .map_err(|error| format!("invalid MCP elicitation response JSON: {error}"))?;
                Ok(McpElicitationResponse::accept(content))
            }
            None => Ok(McpElicitationResponse::decline()),
        }
    }
}

fn project_pending_interaction(
    store: Option<&RuntimePendingInteractionStore>,
    record: RuntimePendingInteractionRecord,
) -> bool {
    if let Some(store) = store {
        return store.insert(record).is_ok();
    }
    false
}

fn remove_projected_interaction(
    store: Option<&RuntimePendingInteractionStore>,
    id: &str,
    projected: bool,
) {
    if projected && let Some(store) = store {
        store.remove(id);
    }
}

fn approval_event_from_pending_interaction(
    key: &TuiInteractionKey,
    record: &RuntimePendingInteractionRecord,
) -> TuiEvent {
    if let Some(permission_kind) = record.permission_kind {
        return TuiEvent::PermissionApprovalNeeded {
            key: key.clone(),
            tool: record.tool.clone().unwrap_or_default(),
            target: record.target.clone(),
            preview: record.preview.clone(),
            permission_kind,
        };
    }

    TuiEvent::ApprovalNeeded {
        key: key.clone(),
        tool: record.tool.clone().unwrap_or_default(),
        target: record.target.clone(),
        preview: record.preview.clone(),
    }
}

fn user_input_event_from_pending_interaction(
    key: &TuiInteractionKey,
    record: &RuntimePendingInteractionRecord,
) -> TuiEvent {
    TuiEvent::UserInputRequested {
        key: key.clone(),
        question: record.question.clone().unwrap_or_default(),
        choices: record.choices.clone(),
    }
}

fn mcp_elicitation_event_from_pending_interaction(
    key: &TuiInteractionKey,
    record: &RuntimePendingInteractionRecord,
) -> TuiEvent {
    let elicitation = record
        .mcp_elicitation
        .as_ref()
        .expect("mcp elicitation pending record has details");
    TuiEvent::McpElicitationRequested {
        key: key.clone(),
        server_name: elicitation.server_name.clone(),
        mode: elicitation.mode.clone(),
        message: record.question.clone().unwrap_or_default(),
        url: elicitation.url.clone(),
        requested_schema_json: elicitation.requested_schema_json.clone(),
    }
}

#[cfg(test)]
#[path = "runtime_interaction_adapter_tests.rs"]
mod tests;
