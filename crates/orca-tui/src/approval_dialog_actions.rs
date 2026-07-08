use std::sync::mpsc;

use crossterm::event::{KeyCode, KeyEvent};

use crate::approval_actions::resolve_approval_option;
use crate::shortcuts::{ApprovalShortcut, approval_shortcut};
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

    match approval_shortcut(*key) {
        Some(ApprovalShortcut::SelectAllow) => {
            if let Some(dialog) = &mut state.approval_dialog {
                dialog.selected = dialog.selected.saturating_sub(1);
            }
        }
        Some(ApprovalShortcut::SelectDeny) => {
            if let Some(dialog) = &mut state.approval_dialog {
                let last = dialog.options.len().saturating_sub(1);
                dialog.selected = (dialog.selected + 1).min(last);
            }
        }
        Some(ApprovalShortcut::ToggleSelection) => {
            if let Some(dialog) = &mut state.approval_dialog {
                let len = dialog.options.len().max(1);
                dialog.selected = (dialog.selected + 1) % len;
            }
        }
        Some(ApprovalShortcut::Confirm) => {
            let option = state
                .approval_dialog
                .as_ref()
                .map(|dialog| dialog.current());
            if let Some(option) = option {
                resolve_approval_option(state, action_tx, option);
            }
        }
        Some(ApprovalShortcut::Approve) => {
            resolve_approval_option(state, action_tx, ApprovalOption::Once);
        }
        Some(ApprovalShortcut::Deny) => {
            resolve_approval_option(state, action_tx, ApprovalOption::Deny);
        }
        None => {}
    }
}
