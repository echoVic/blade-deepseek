use crossbeam_channel as mpsc;

use tui_textarea::TextArea;

use crate::bridge;
use crate::composer_textarea::make_textarea_with_text;
use crate::theme::Theme;
use crate::types::{AppState, AppStatus, TuiEvent, UserAction};
use crate::vim::VimState;
use crate::workflow_notifications::{
    drain_pending_workflow_notifications, is_workflow_notification_turn_boundary,
    queue_workflow_terminal_notification, remove_pending_workflow_notification_by_id,
    submit_pending_workflow_notification,
};

pub(crate) fn handle_runtime_event(
    tui_event: TuiEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) {
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
    use orca_core::config::ThemeName;

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
        );

        assert_eq!(textarea_text(&textarea), "review @gone.txt");
        assert_eq!(state.status, AppStatus::Idle);
    }
}
