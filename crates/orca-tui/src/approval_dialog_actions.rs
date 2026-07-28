use crossbeam_channel as mpsc;

use crossterm::event::{KeyCode, KeyEvent};

use crate::approval_actions::resolve_approval_option;
use crate::shortcuts::{ApprovalShortcut, ShortcutAction, ShortcutContext, resolve_shortcut};
use crate::types::{AppState, ApprovalOption, UserAction};

pub(crate) fn handle_approval_dialog_key(
    key: &KeyEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) {
    if let KeyCode::Char(c) = key.code
        && let Some(option) = state
            .approval_dialog
            .as_ref()
            .and_then(|dialog| dialog.option_for_key(c))
    {
        resolve_approval_option(state, action_tx, option);
        return;
    }

    if let Some(ShortcutAction::Approval(shortcut)) =
        resolve_shortcut(ShortcutContext::Approval, *key)
    {
        handle_approval_shortcut(shortcut, state, action_tx);
    }
}

pub(crate) fn handle_approval_shortcut(
    shortcut: ApprovalShortcut,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) {
    match shortcut {
        ApprovalShortcut::SelectAllow => {
            if let Some(dialog) = &mut state.approval_dialog {
                dialog.selected = dialog.selected.saturating_sub(1);
            }
        }
        ApprovalShortcut::SelectDeny => {
            if let Some(dialog) = &mut state.approval_dialog {
                let last = dialog.options.len().saturating_sub(1);
                dialog.selected = (dialog.selected + 1).min(last);
            }
        }
        ApprovalShortcut::ToggleSelection => {
            if let Some(dialog) = &mut state.approval_dialog {
                let len = dialog.options.len().max(1);
                dialog.selected = (dialog.selected + 1) % len;
            }
        }
        ApprovalShortcut::Confirm => {
            let option = state
                .approval_dialog
                .as_ref()
                .map(|dialog| dialog.current());
            if let Some(option) = option {
                resolve_approval_option(state, action_tx, option);
            }
        }
        ApprovalShortcut::Approve => {
            resolve_approval_option(state, action_tx, ApprovalOption::Once);
        }
        ApprovalShortcut::Deny => {
            resolve_approval_option(state, action_tx, ApprovalOption::Deny);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ApprovalDialog, ApprovalOption};

    fn state() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.approval_dialog = Some(ApprovalDialog {
            id: "approval".to_string(),
            interaction: None,
            tool: "shell".to_string(),
            target: None,
            permission_kind: None,
            background_task_id: Some("task".to_string()),
            selected: 0,
            options: vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTool,
                ApprovalOption::Deny,
            ],
            diff: None,
        });
        state
    }

    #[test]
    fn action_only_approval_moves_and_confirms_without_synthetic_key() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state();

        handle_approval_shortcut(ApprovalShortcut::SelectDeny, &mut state, &action_tx);
        assert_eq!(state.approval_dialog.as_ref().unwrap().selected, 1);
        handle_approval_shortcut(ApprovalShortcut::Confirm, &mut state, &action_tx);

        assert!(state.approval_dialog.is_none());
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::ResolveBackgroundApproval { id, approved })
                if id == "approval" && approved
        ));
        assert!(
            state
                .approval_allowlist
                .contains(&AppState::approval_key_tool("shell"))
        );
    }

    #[test]
    fn fixed_a_key_keeps_always_tool_meaning() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state();
        let key = KeyEvent::new(KeyCode::Char('a'), crossterm::event::KeyModifiers::NONE);

        handle_approval_dialog_key(&key, &mut state, &action_tx);

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::ResolveBackgroundApproval { id, approved })
                if id == "approval" && approved
        ));
        assert!(
            state
                .approval_allowlist
                .contains(&AppState::approval_key_tool("shell"))
        );
    }
}
