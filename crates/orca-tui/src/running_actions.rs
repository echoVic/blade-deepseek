use crossbeam_channel as mpsc;

use orca_core::cancel::OperationCancellation;

use crate::shortcuts::RunningShortcut;
use crate::types::{AppState, AppStatus, UserAction};

pub(crate) fn handle_running_shortcut(
    shortcut: RunningShortcut,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    cancellation: &OperationCancellation,
) {
    match shortcut {
        RunningShortcut::BackgroundCurrentTurn => {
            let _ = action_tx.send(UserAction::BackgroundCurrentTurn);
            state.set_status(AppStatus::Idle);
        }
        RunningShortcut::Interrupt => {
            cancellation.cancel_current();
            let _ = action_tx.send(UserAction::Interrupt);
        }
        RunningShortcut::ScrollUp => {
            state.scroll_up(1);
        }
        RunningShortcut::ScrollDown => {
            state.scroll_down(1);
        }
        RunningShortcut::PageUp => {
            let page = state.visible_height.saturating_sub(2);
            state.scroll_up(page);
        }
        RunningShortcut::PageDown => {
            let page = state.visible_height.saturating_sub(2);
            state.scroll_down(page);
        }
        RunningShortcut::HalfPageUp => {
            let page = state.visible_height / 2;
            state.scroll_up(page);
        }
        RunningShortcut::HalfPageDown => {
            let page = state.visible_height / 2;
            state.scroll_down(page);
        }
    }
}
