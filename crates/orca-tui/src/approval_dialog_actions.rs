use crossbeam_channel as mpsc;

use crossterm::event::{KeyCode, KeyEvent};

use crate::approval_actions::resolve_approval_option;
use crate::keybindings::{InputOwnerFingerprint, KeymapRuntime, ShortcutResolution};
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

pub(crate) fn handle_approval_dialog_key_dynamic(
    key: &KeyEvent,
    now: std::time::Instant,
    owner: InputOwnerFingerprint,
    keymap: &mut KeymapRuntime,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> bool {
    if let KeyCode::Char(character) = key.code
        && let Some(option) = state
            .approval_dialog
            .as_ref()
            .and_then(|dialog| dialog.option_for_key(character))
    {
        resolve_approval_option(state, action_tx, option);
        return true;
    }
    if key.code == KeyCode::Char('d') {
        resolve_approval_option(state, action_tx, ApprovalOption::Deny);
        return true;
    }
    match keymap.resolve_new_context(owner, *key, now) {
        ShortcutResolution::Action(invocation) => {
            let ShortcutAction::Approval(shortcut) = invocation.action else {
                return false;
            };
            handle_approval_shortcut(shortcut, state, action_tx);
            true
        }
        ShortcutResolution::Pending => true,
        ShortcutResolution::RetryCurrentKey | ShortcutResolution::NoMatch => false,
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

    #[test]
    fn fixed_d_key_keeps_deny_meaning() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state();
        let key = KeyEvent::new(KeyCode::Char('d'), crossterm::event::KeyModifiers::NONE);
        let mut runtime =
            crate::keybindings::KeymapRuntime::new(crate::keybindings::Keymap::built_in());
        let owner = crate::keybindings::InputOwnerFingerprint {
            context: ShortcutContext::Approval,
            modal: crate::keybindings::ModalOwner::Approval,
            panel: crate::types::PanelMode::Conversation,
            vim_mode: None,
        };

        assert!(handle_approval_dialog_key_dynamic(
            &key,
            std::time::Instant::now(),
            owner,
            &mut runtime,
            &mut state,
            &action_tx,
        ));

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::ResolveBackgroundApproval { id, approved })
                if id == "approval" && !approved
        ));
    }

    #[test]
    fn dynamic_approval_chord_moves_without_synthetic_key() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = state();
        let keymap = crate::keybindings::parse_keymap(
            br#"{"version":1,"bindings":{"approval.select-deny":["g g"]}}"#,
        )
        .unwrap();
        let mut runtime = crate::keybindings::KeymapRuntime::new(keymap);
        let owner = crate::keybindings::InputOwnerFingerprint {
            context: ShortcutContext::Approval,
            modal: crate::keybindings::ModalOwner::Approval,
            panel: crate::types::PanelMode::Conversation,
            vim_mode: None,
        };
        let now = std::time::Instant::now();
        let key = KeyEvent::new(KeyCode::Char('g'), crossterm::event::KeyModifiers::NONE);

        assert!(handle_approval_dialog_key_dynamic(
            &key,
            now,
            owner,
            &mut runtime,
            &mut state,
            &action_tx,
        ));
        let continuation =
            runtime.advance_pending(owner, key, now + std::time::Duration::from_millis(1));
        let crate::keybindings::ShortcutResolution::Action(invocation) = continuation else {
            panic!("expected completed approval chord");
        };
        let ShortcutAction::Approval(shortcut) = invocation.action else {
            panic!("expected approval action");
        };
        handle_approval_shortcut(shortcut, &mut state, &action_tx);
        assert_eq!(state.approval_dialog.as_ref().unwrap().selected, 1);
    }
}
