use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use orca_core::cancel::CancelToken;

use crate::shortcuts::GlobalShortcut;
use crate::types::{AppState, AppStatus, ChatMessage, UserAction};

pub(crate) enum GlobalShortcutFlow {
    Continue,
    Exit(i32),
}

pub(crate) fn handle_global_shortcut<F>(
    shortcut: GlobalShortcut,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    cancel_token: &CancelToken,
    clear_terminal: F,
) -> io::Result<GlobalShortcutFlow>
where
    F: FnOnce() -> io::Result<()>,
{
    match shortcut {
        GlobalShortcut::Cancel => {
            if state.status == AppStatus::Running {
                cancel_token.cancel();
                let _ = action_tx.send(UserAction::Interrupt);
                return Ok(GlobalShortcutFlow::Continue);
            }
            let now = Instant::now();
            if state
                .last_ctrl_c
                .is_some_and(|t| now.duration_since(t) < Duration::from_secs(2))
            {
                let _ = action_tx.send(UserAction::Cancel);
                return Ok(GlobalShortcutFlow::Exit(130));
            }
            state.last_ctrl_c = Some(now);
            state
                .messages
                .push(ChatMessage::System("Press Ctrl+C again to quit.".into()));
            state.scroll_to_bottom();
        }
        GlobalShortcut::ToggleShortcuts => {
            state.toggle_shortcuts();
        }
        GlobalShortcut::ScrollBottom => {
            state.scroll_to_bottom();
        }
        GlobalShortcut::ScrollTop => {
            state.scroll_to_top();
        }
        GlobalShortcut::ClearScreen => {
            state.messages.clear();
            state.finalized_count = 0;
            state.flushed_count = 0;
            state.scroll_offset = 0;
            state.auto_scroll = true;
            clear_terminal()?;
        }
    }
    Ok(GlobalShortcutFlow::Continue)
}
