use crossbeam_channel as mpsc;

use tui_textarea::TextArea;

use crate::bridge;
use crate::composer_textarea::make_textarea_with_text;
use crate::terminal_presentation::{TerminalNotification, TerminalPresentation};
use crate::theme::Theme;
use crate::types::{AppState, AppStatus, TuiEvent, UserAction};
use crate::vim::VimState;
use crate::workflow_notifications::{
    drain_pending_workflow_notifications, is_workflow_notification_turn_boundary,
    queue_workflow_terminal_notification, remove_pending_workflow_notification_by_id,
    submit_pending_workflow_notification,
};

pub(crate) fn terminal_notification_for_event(
    event: &TuiEvent,
    state: &AppState,
) -> Option<TerminalNotification> {
    let message = match event {
        TuiEvent::ApprovalNeeded { tool, target, .. }
            if !state.approval_is_allowlisted(tool, target.as_deref()) =>
        {
            "Approval required"
        }
        TuiEvent::ApprovalNeeded { .. } => return None,
        TuiEvent::PermissionApprovalNeeded { .. } => "Permission approval required",
        TuiEvent::UserInputRequested { .. } => "Input required",
        TuiEvent::McpElicitationRequested { .. } => "MCP input required",
        TuiEvent::SessionCompleted { status } if status == "success" => "Task completed",
        TuiEvent::SessionCompleted { status } => {
            return Some(TerminalNotification::new(format!("Task {status}")));
        }
        TuiEvent::WorkflowNotification { status, .. } if status == "completed" => {
            "Workflow completed"
        }
        TuiEvent::WorkflowNotification { status, .. } => {
            return Some(TerminalNotification::new(format!("Workflow {status}")));
        }
        _ => return None,
    };
    Some(TerminalNotification::new(message))
}

pub(crate) fn handle_runtime_event(
    tui_event: TuiEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
    presentation: &mut TerminalPresentation,
) {
    let terminal_notification = terminal_notification_for_event(&tui_event, state);
    if let TuiEvent::ApprovalNeeded {
        key, tool, target, ..
    } = &tui_event
        && state.approval_is_allowlisted(tool, target.as_deref())
    {
        let _ = action_tx.send(UserAction::RespondToInteraction {
            key: key.clone(),
            response: crate::types::TuiInteractionResponse::Approval(true),
        });
        state.enter_running();
        return;
    }

    let restored_prompt = match &tui_event {
        TuiEvent::Backtracked { prompt } | TuiEvent::SubmissionRejected { prompt, .. } => {
            Some(prompt.clone())
        }
        _ => None,
    };
    let workflow_notification_turn_boundary = is_workflow_notification_turn_boundary(&tui_event);
    let batch_queued_workflow_notification_id = queue_workflow_terminal_notification(
        &tui_event,
        pending_workflow_notifications,
        state.status == AppStatus::Running,
    );

    state.update(tui_event);
    if let Some(notification) = terminal_notification {
        presentation.enqueue(notification);
    }

    if let Some(id) = batch_queued_workflow_notification_id {
        remove_pending_workflow_notification_by_id(state, &id);
    }
    if let Some(prompt) = restored_prompt {
        vim_state.reset_insert(textarea, theme);
        *textarea = make_textarea_with_text(&prompt, vim_state, theme);
    }
    if workflow_notification_turn_boundary {
        drain_pending_workflow_notifications(state, pending_workflow_notifications);
        submit_pending_workflow_notification(state, action_tx, false);
    } else {
        submit_pending_workflow_notification(state, action_tx, true);
    }
    if state.auto_scroll {
        state.scroll_to_bottom();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_textarea::textarea_text;
    use crate::terminal_presentation::TerminalNotification;
    use crate::types::{TuiInteractionKey, TuiInteractionKind};
    use orca_core::cancel::OperationIdAllocator;
    use orca_core::config::ThemeName;
    use orca_runtime::runtime_pending_interaction::RuntimeMcpElicitationMode;

    fn interaction_key(kind: TuiInteractionKind, id: &str) -> TuiInteractionKey {
        TuiInteractionKey::new(OperationIdAllocator::new().allocate(), id, kind)
    }

    fn notification_message(event: &TuiEvent, state: &AppState) -> Option<TerminalNotification> {
        terminal_notification_for_event(event, state)
    }

    #[test]
    fn submission_rejection_restores_prompt_to_composer() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "0.0.0-test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.push_message(crate::types::ChatMessage::User(
            "review @gone.txt".to_string(),
        ));
        state.enter_running();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim_state = VimState::new(false);
        let mut textarea = TextArea::default();
        let mut presentation = TerminalPresentation::new(
            false,
            crate::terminal_presentation::TerminalPresentationProfile {
                osc9_supported: false,
                tmux_passthrough: false,
            },
        );

        handle_runtime_event(
            TuiEvent::SubmissionRejected {
                prompt: "review @gone.txt".to_string(),
                message: "bound file is no longer available".to_string(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut presentation,
        );

        assert_eq!(textarea_text(&textarea), "review @gone.txt");
        assert_eq!(state.status, AppStatus::Idle);
    }

    #[test]
    fn terminal_notification_for_event_matches_fixed_safe_matrix() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let state = AppState::new(
            action_tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );

        let cases = [
            (
                TuiEvent::ApprovalNeeded {
                    key: interaction_key(TuiInteractionKind::Approval, "approval"),
                    tool: "secret-tool".to_string(),
                    target: Some("secret-target".to_string()),
                    preview: Some("secret-preview".to_string()),
                },
                "Approval required",
            ),
            (
                TuiEvent::PermissionApprovalNeeded {
                    key: interaction_key(TuiInteractionKind::Permission, "permission"),
                    tool: "secret-tool".to_string(),
                    target: Some("secret-target".to_string()),
                    preview: Some("secret-preview".to_string()),
                    permission_kind:
                        orca_runtime::runtime_permission::RuntimePermissionRequestKind::UnsandboxedShellRetry,
                },
                "Permission approval required",
            ),
            (
                TuiEvent::UserInputRequested {
                    key: interaction_key(TuiInteractionKind::UserInput, "input"),
                    question: "secret-question".to_string(),
                    choices: vec!["secret-choice".to_string()],
                },
                "Input required",
            ),
            (
                TuiEvent::McpElicitationRequested {
                    key: interaction_key(TuiInteractionKind::McpElicitation, "mcp"),
                    server_name: "secret-server".to_string(),
                    mode: RuntimeMcpElicitationMode::Form,
                    message: "secret-message".to_string(),
                    url: Some("secret-url".to_string()),
                    requested_schema_json: Some("secret-schema".to_string()),
                },
                "MCP input required",
            ),
            (
                TuiEvent::SessionCompleted {
                    status: "success".to_string(),
                },
                "Task completed",
            ),
            (
                TuiEvent::SessionCompleted {
                    status: "verification_failed".to_string(),
                },
                "Task verification_failed",
            ),
            (
                TuiEvent::WorkflowNotification {
                    id: "secret-id".to_string(),
                    prompt: "secret-prompt".to_string(),
                    status: "completed".to_string(),
                    summary: "secret-summary".to_string(),
                },
                "Workflow completed",
            ),
            (
                TuiEvent::WorkflowNotification {
                    id: "secret-id".to_string(),
                    prompt: "secret-prompt".to_string(),
                    status: "failed".to_string(),
                    summary: "secret-summary".to_string(),
                },
                "Workflow failed",
            ),
        ];

        for (event, expected) in cases {
            let notification = notification_message(&event, &state).expect("notification");
            assert_eq!(notification.message(), expected);
            for secret in [
                "secret-tool",
                "secret-target",
                "secret-preview",
                "secret-question",
                "secret-choice",
                "secret-server",
                "secret-message",
                "secret-url",
                "secret-schema",
                "secret-id",
                "secret-prompt",
                "secret-summary",
            ] {
                assert!(!notification.message().contains(secret));
            }
        }
        assert!(notification_message(&TuiEvent::Notice("ignored".to_string()), &state).is_none());
    }

    #[test]
    fn terminal_notification_for_event_skips_allowlisted_approval() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state
            .approval_allowlist
            .insert(AppState::approval_key_target("bash", "cargo test"));

        let event = TuiEvent::ApprovalNeeded {
            key: interaction_key(TuiInteractionKind::Approval, "approval"),
            tool: "bash".to_string(),
            target: Some("cargo test".to_string()),
            preview: None,
        };

        assert!(terminal_notification_for_event(&event, &state).is_none());
    }

    #[test]
    fn handle_runtime_event_enqueues_only_when_presentation_is_unfocused() {
        for (focused, expected_pending) in [(true, 0), (false, 1)] {
            let (action_tx, _action_rx) = mpsc::unbounded();
            let mut state = AppState::new(
                action_tx.clone(),
                "test".to_string(),
                "mock".to_string(),
                "/tmp".to_string(),
            );
            state.enter_running();
            let pending = bridge::PendingWorkflowNotifications::new();
            let theme = Theme::named(ThemeName::Dark);
            let mut vim_state = VimState::new(false);
            let mut textarea = TextArea::default();
            let mut presentation = TerminalPresentation::new(
                true,
                crate::terminal_presentation::TerminalPresentationProfile {
                    osc9_supported: true,
                    tmux_passthrough: false,
                },
            );
            presentation.set_focused(focused);

            handle_runtime_event(
                TuiEvent::SessionCompleted {
                    status: "success".to_string(),
                },
                &mut state,
                &action_tx,
                &pending,
                &mut textarea,
                &mut vim_state,
                &theme,
                &mut presentation,
            );

            assert_eq!(presentation.pending_len_for_test(), expected_pending);
        }
    }
}
