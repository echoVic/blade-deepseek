use std::sync::mpsc::{Receiver, Sender};

use orca_approval::ApprovalPolicy;
use orca_core::approval_types::{ApprovalDecision, ApprovalRequest, ApprovalResolution};
use orca_core::tool_types;
use orca_runtime::lifecycle::{
    RuntimeApprovalDecision, RuntimeApprovalHandler, RuntimeToolActorContext,
    RuntimeUserInputHandler, RuntimeUserInputRequest,
};
use orca_runtime::tool_invocation::{ToolInvocation, approval_request_for_invocation};

use crate::types::{TuiEvent, UserAction};

pub(crate) enum TuiToolApprovalOutcome {
    Continue,
    Denied(tool_types::ToolResult),
}

pub(crate) struct TuiApprovalHandler<'a> {
    action_rx: &'a Receiver<UserAction>,
}

impl<'a> TuiApprovalHandler<'a> {
    pub(crate) fn new(action_rx: &'a Receiver<UserAction>) -> Self {
        Self { action_rx }
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
                Ok(UserAction::Approve(value)) => break value,
                Ok(UserAction::Interrupt) | Ok(UserAction::Cancel) | Err(_) => break false,
                _ => continue,
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
            let _ = event_tx.send(TuiEvent::ApprovalNeeded {
                id: approval.id.clone(),
                tool: approval.tool.clone().unwrap_or_default(),
                target: approval.target.clone(),
                preview: approval.preview.clone(),
            });

            let handler = TuiApprovalHandler::new(action_rx);
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

pub(crate) struct TuiUserInputHandler<'a> {
    event_tx: &'a Sender<TuiEvent>,
    action_rx: &'a Receiver<UserAction>,
}

impl<'a> TuiUserInputHandler<'a> {
    pub(crate) fn new(event_tx: &'a Sender<TuiEvent>, action_rx: &'a Receiver<UserAction>) -> Self {
        Self {
            event_tx,
            action_rx,
        }
    }
}

impl RuntimeUserInputHandler for TuiUserInputHandler<'_> {
    fn request_user_input(
        &self,
        request: &RuntimeUserInputRequest,
    ) -> std::io::Result<Option<String>> {
        let _ = self.event_tx.send(TuiEvent::UserInputRequested {
            id: request.id.clone(),
            question: request.question.clone(),
            choices: request.choices.clone(),
        });

        loop {
            match self.action_rx.recv() {
                Ok(UserAction::RespondToUserInput(answer)) => return Ok(Some(answer)),
                Ok(UserAction::Interrupt) | Ok(UserAction::Cancel) | Err(_) => return Ok(None),
                _ => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use orca_runtime::lifecycle::RuntimeToolActorContext;

    use super::*;

    #[test]
    fn tui_approval_handler_resolves_approve_action_through_runtime_context() {
        let (action_tx, action_rx) = mpsc::channel();
        action_tx
            .send(UserAction::Approve(true))
            .expect("send approval");
        let handler = TuiApprovalHandler::new(&action_rx);
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
    fn tui_approval_handler_maps_cancel_to_runtime_denial() {
        let (action_tx, action_rx) = mpsc::channel();
        action_tx.send(UserAction::Cancel).expect("send cancel");
        let handler = TuiApprovalHandler::new(&action_rx);
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
            .send(UserAction::RespondToUserInput("yes".to_string()))
            .expect("send answer");
        let handler = TuiUserInputHandler::new(&event_tx, &action_rx);
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
    fn tui_user_input_handler_maps_cancel_to_runtime_failure() {
        let (event_tx, _event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx.send(UserAction::Cancel).expect("send cancel");
        let handler = TuiUserInputHandler::new(&event_tx, &action_rx);
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
}
