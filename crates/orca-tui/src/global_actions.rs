use crossbeam_channel as mpsc;
use std::io;
use std::time::{Duration, Instant};

use orca_core::cancel::OperationCancellation;

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
    cancellation: &OperationCancellation,
    clear_terminal: F,
) -> io::Result<GlobalShortcutFlow>
where
    F: FnOnce() -> io::Result<()>,
{
    match shortcut {
        GlobalShortcut::Cancel => {
            if matches!(state.status, AppStatus::Running | AppStatus::Compacting) {
                cancellation.cancel_current();
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
            state.push_message(ChatMessage::System("Press Ctrl+C again to quit.".into()));
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
            state.clear_messages();
            state.scroll_offset = 0;
            state.auto_scroll = true;
            clear_terminal()?;
        }
    }
    Ok(GlobalShortcutFlow::Continue)
}

#[cfg(test)]
mod tests {
    use crossbeam_channel as mpsc;

    use orca_core::cancel::OperationCancellation;

    use super::handle_global_shortcut;
    use crate::shortcuts::GlobalShortcut;
    use crate::types::{AppState, AppStatus, ChatMessage, UserAction};

    #[test]
    fn cancel_interrupts_while_context_is_compacting() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        );
        state.set_status(AppStatus::Compacting);
        let cancellation = OperationCancellation::new();
        let operation = cancellation.start();

        handle_global_shortcut(
            GlobalShortcut::Cancel,
            &mut state,
            &action_tx,
            &cancellation,
            || Ok(()),
        )
        .expect("cancel compaction");

        assert!(operation.token().is_cancelled());
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    }

    #[test]
    fn clear_screen_atomically_clears_messages_revisions_and_render_cache() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        );
        state.push_message(ChatMessage::Assistant("cached".to_string()));
        assert_eq!(state.message_revisions.len(), 1);
        assert_eq!(state.transcript_render_cache.len(), 1);

        handle_global_shortcut(
            GlobalShortcut::ClearScreen,
            &mut state,
            &action_tx,
            &OperationCancellation::new(),
            || Ok(()),
        )
        .expect("clear screen");

        assert!(state.messages.is_empty());
        assert!(state.message_revisions.is_empty());
        assert_eq!(state.transcript_render_cache.len(), 0);
    }
}
