use std::sync::mpsc::{Receiver, Sender};

use orca_core::approval_types::{ApprovalDecision, ApprovalRequest, ApprovalResolution};
use orca_core::tool_types;
use orca_runtime::lifecycle::{
    RuntimeApprovalHandler, RuntimeUserInputHandler, RuntimeUserInputRequest,
};

use crate::types::{TuiEvent, UserAction};

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
