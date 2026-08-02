use std::io;
use std::time::Instant;

use crossbeam_channel as mpsc;
use crossterm::event::{KeyCode, KeyEvent};

use crate::types::{AppState, AppStatus, SessionPickerPhase, UserAction};

const SESSION_ACTION_COUNT: usize = 6;

pub(crate) fn handle_session_picker_key<F>(
    key: &KeyEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    clear_terminal: F,
) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()>,
{
    let phase = state.session_picker_phase.clone();
    match phase {
        SessionPickerPhase::Browsing => match key.code {
            KeyCode::Up => state.select_previous_session(),
            KeyCode::Down => state.select_next_session(),
            KeyCode::Backspace => state.session_query_pop(),
            KeyCode::Char(c) => state.session_query_push(c),
            KeyCode::Enter => dispatch_selected_resume(state, action_tx, clear_terminal)?,
            KeyCode::Tab => {
                if let Some(session_id) = state.selected_session_id() {
                    state.session_picker_phase = SessionPickerPhase::Actions {
                        session_id,
                        selected: 0,
                    };
                }
            }
            KeyCode::Esc => close_picker(state),
            _ => {}
        },
        SessionPickerPhase::Actions {
            session_id,
            mut selected,
        } => match key.code {
            KeyCode::Up => {
                selected = selected.saturating_sub(1);
                state.session_picker_phase = SessionPickerPhase::Actions {
                    session_id,
                    selected,
                };
            }
            KeyCode::Down => {
                selected = (selected + 1).min(SESSION_ACTION_COUNT - 1);
                state.session_picker_phase = SessionPickerPhase::Actions {
                    session_id,
                    selected,
                };
            }
            KeyCode::Enter => {
                activate_action(state, action_tx, session_id, selected, clear_terminal)?;
            }
            KeyCode::Esc | KeyCode::Tab => {
                state.session_picker_phase = SessionPickerPhase::Browsing;
            }
            _ => {}
        },
        SessionPickerPhase::Renaming {
            session_id,
            mut value,
        } => match key.code {
            KeyCode::Char(c) => {
                value.push(c);
                state.session_picker_phase = SessionPickerPhase::Renaming { session_id, value };
            }
            KeyCode::Backspace => {
                value.pop();
                state.session_picker_phase = SessionPickerPhase::Renaming { session_id, value };
            }
            KeyCode::Enter if !value.trim().is_empty() => {
                state.enter_running();
                let _ = action_tx.send(UserAction::RenameSavedSession {
                    session_id,
                    title: value.trim().to_string(),
                });
            }
            KeyCode::Esc => {
                state.session_picker_phase = SessionPickerPhase::Actions {
                    session_id,
                    selected: 2,
                };
            }
            _ => {}
        },
        SessionPickerPhase::ConfirmArchive {
            session_id,
            title,
            mut selected,
        } => match key.code {
            KeyCode::Left | KeyCode::Up => {
                selected = 0;
                state.session_picker_phase = SessionPickerPhase::ConfirmArchive {
                    session_id,
                    title,
                    selected,
                };
            }
            KeyCode::Right | KeyCode::Down => {
                selected = 1;
                state.session_picker_phase = SessionPickerPhase::ConfirmArchive {
                    session_id,
                    title,
                    selected,
                };
            }
            KeyCode::Enter if selected == 1 => {
                state.enter_running();
                let _ = action_tx.send(UserAction::ArchiveSavedSession { session_id });
            }
            KeyCode::Enter | KeyCode::Esc => {
                state.session_picker_phase = SessionPickerPhase::Actions {
                    session_id,
                    selected: 3,
                };
            }
            _ => {}
        },
        SessionPickerPhase::ConfirmDelete {
            session_id,
            title,
            mut selected,
        } => match key.code {
            KeyCode::Left | KeyCode::Up => {
                selected = 0;
                state.session_picker_phase = SessionPickerPhase::ConfirmDelete {
                    session_id,
                    title,
                    selected,
                };
            }
            KeyCode::Right | KeyCode::Down => {
                selected = 1;
                state.session_picker_phase = SessionPickerPhase::ConfirmDelete {
                    session_id,
                    title,
                    selected,
                };
            }
            KeyCode::Enter if selected == 1 => {
                state.enter_running();
                let _ = action_tx.send(UserAction::DeleteSavedSession { session_id });
            }
            KeyCode::Enter | KeyCode::Esc => {
                state.session_picker_phase = SessionPickerPhase::Actions {
                    session_id,
                    selected: 4,
                };
            }
            _ => {}
        },
    }
    Ok(())
}

fn dispatch_selected_resume<F>(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    clear_terminal: F,
) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()>,
{
    let Some(session_id) = state.selected_session_id() else {
        return Ok(());
    };
    clear_terminal()?;
    state.enter_running();
    let _ = action_tx.send(UserAction::ResumeSavedSession { session_id });
    Ok(())
}

fn activate_action<F>(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    session_id: String,
    selected: usize,
    clear_terminal: F,
) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()>,
{
    match selected {
        0 => {
            clear_terminal()?;
            state.enter_running();
            let _ = action_tx.send(UserAction::ResumeSavedSession { session_id });
        }
        1 => {
            state.enter_running();
            let _ = action_tx.send(UserAction::ForkSavedSession { session_id });
        }
        2 => {
            state.session_picker_phase = SessionPickerPhase::Renaming {
                session_id,
                value: String::new(),
            };
        }
        3 | 4 => {
            let title = state
                .session_picker_sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map(|session| session.title.clone())
                .unwrap_or_else(|| session_id.clone());
            state.session_picker_phase = if selected == 3 {
                SessionPickerPhase::ConfirmArchive {
                    session_id,
                    title,
                    selected: 0,
                }
            } else {
                SessionPickerPhase::ConfirmDelete {
                    session_id,
                    title,
                    selected: 0,
                }
            };
        }
        5 => {
            state.stage_clipboard_copy(session_id, Instant::now());
            state.session_picker_phase = SessionPickerPhase::Browsing;
        }
        _ => {}
    }
    Ok(())
}

fn close_picker(state: &mut AppState) {
    state.set_status(AppStatus::Idle);
    state.session_picker_sessions.clear();
    state.session_picker_query.clear();
    state.session_picker_phase = SessionPickerPhase::Browsing;
    state.session_picker_error = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crossterm::event::KeyModifiers;
    use orca_runtime::history::SessionSummary;

    fn session(id: &str, title: &str) -> SessionSummary {
        SessionSummary {
            session_id: id.to_string(),
            title: title.to_string(),
            cwd: ".".to_string(),
            provider: "deepseek".to_string(),
            model: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            path: std::path::PathBuf::new(),
            archived: false,
            parent_id: None,
            forked: false,
            approval_mode: None,
            active_permission_profile: None,
            runtime_workspace_roots: Vec::new(),
            permission_rule_count: 0,
            additional_working_directories: Vec::new(),
            network_domain_permissions: Default::default(),
        }
    }

    fn state() -> (AppState, mpsc::Receiver<UserAction>) {
        let (tx, rx) = mpsc::unbounded();
        let mut state = AppState::new(tx.clone(), "test".into(), "auto".into(), ".".into());
        state.status = AppStatus::SessionPicker;
        state.session_picker_sessions = vec![session("one", "First"), session("two", "Second")];
        state.session_picker_selected = 1;
        (state, rx)
    }

    fn press(code: KeyCode, state: &mut AppState) {
        let tx = state.event_tx.clone();
        handle_session_picker_key(&KeyEvent::new(code, KeyModifiers::NONE), state, &tx, || {
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn session_picker_actions_capture_selected_session_id() {
        let (mut state, _) = state();
        press(KeyCode::Tab, &mut state);
        assert_eq!(
            state.session_picker_phase,
            SessionPickerPhase::Actions {
                session_id: "two".to_string(),
                selected: 0,
            }
        );

        state.session_picker_selected = 0;
        press(KeyCode::Down, &mut state);
        press(KeyCode::Down, &mut state);
        press(KeyCode::Enter, &mut state);
        assert!(matches!(
            state.session_picker_phase,
            SessionPickerPhase::Renaming { ref session_id, .. } if session_id == "two"
        ));
    }

    #[test]
    fn session_picker_delete_confirmation_uses_captured_id() {
        let (mut state, rx) = state();
        press(KeyCode::Tab, &mut state);
        for _ in 0..4 {
            press(KeyCode::Down, &mut state);
        }
        press(KeyCode::Enter, &mut state);
        state.session_picker_selected = 0;
        press(KeyCode::Right, &mut state);
        press(KeyCode::Enter, &mut state);

        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::DeleteSavedSession { session_id }) if session_id == "two"
        ));
    }
}
