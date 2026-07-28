use crossbeam_channel as mpsc;
use std::io;
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyEvent};
use tui_textarea::TextArea;

use orca_core::config::RunConfig;
use orca_runtime::history::SessionTranscript;

use crate::approval_dialog_actions::handle_approval_dialog_key;
use crate::idle_key_actions::handle_idle_key;
use crate::operation_controller::TuiOperationInterrupt;
use crate::queued_input_actions::handle_running_key;
use crate::running_actions::handle_running_shortcut;
use crate::session_picker_actions::handle_session_picker_key;
use crate::setup_actions::{SetupFlow, handle_setup_key};
use crate::shortcuts::{RunningShortcut, ShortcutAction, ShortcutContext, resolve_shortcut};
use crate::theme::Theme;
use crate::types::{AppState, AppStatus, UserAction};
use crate::vim::{VimState, VimTranscriptSearchIntent};

pub(crate) enum StatusKeyFlow {
    Continue,
    Exit(i32),
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_status_key<F>(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    operation: &impl TuiOperationInterrupt,
    preloaded_transcript: &Arc<Mutex<Option<SessionTranscript>>>,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
    initial_prompt: Option<String>,
    clear_terminal: F,
) -> io::Result<StatusKeyFlow>
where
    F: FnOnce() -> io::Result<()>,
{
    if state.status == AppStatus::Setup {
        vim_state.cancel_pending_command();
        return match handle_setup_key(
            ev,
            key,
            state,
            config,
            shared_config,
            action_tx,
            textarea,
            vim_state,
            theme,
            initial_prompt,
        )? {
            SetupFlow::Continue => Ok(StatusKeyFlow::Continue),
            SetupFlow::Exit(code) => Ok(StatusKeyFlow::Exit(code)),
        };
    }

    if state.status == AppStatus::SessionPicker {
        vim_state.cancel_pending_command();
        handle_session_picker_key(
            key,
            state,
            config,
            shared_config,
            preloaded_transcript,
            clear_terminal,
        )?;
        return Ok(StatusKeyFlow::Continue);
    }

    if state.status == AppStatus::WaitingApproval {
        vim_state.cancel_pending_command();
        handle_approval_dialog_key(key, state, action_tx);
        return Ok(StatusKeyFlow::Continue);
    }

    if matches!(
        state.status,
        AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
    ) && let Some(intent) = vim_state.transcript_search_intent(key.code)
    {
        let handled = match intent {
            VimTranscriptSearchIntent::Open => {
                state.open_transcript_search();
                true
            }
            VimTranscriptSearchIntent::Next if state.transcript_search.has_query() => {
                state.search_next();
                true
            }
            VimTranscriptSearchIntent::Previous if state.transcript_search.has_query() => {
                state.search_previous();
                true
            }
            _ => false,
        };
        if handled {
            vim_state.cancel_pending_command();
            return Ok(StatusKeyFlow::Continue);
        }
    }

    if matches!(state.status, AppStatus::Idle | AppStatus::WaitingUserInput) {
        handle_idle_key(
            ev,
            key,
            state,
            config,
            shared_config,
            action_tx,
            textarea,
            vim_state,
            theme,
        );
        return Ok(StatusKeyFlow::Continue);
    }

    if state.status == AppStatus::Running {
        handle_running_key(
            ev, key, state, config, action_tx, operation, textarea, vim_state, theme,
        );
    }

    if state.status == AppStatus::Compacting
        && let Some(ShortcutAction::Running(shortcut)) =
            resolve_shortcut(ShortcutContext::Running, *key)
        && compacting_shortcut_allowed(shortcut)
    {
        handle_running_shortcut(shortcut, state, action_tx, operation);
    }

    Ok(StatusKeyFlow::Continue)
}

fn compacting_shortcut_allowed(shortcut: RunningShortcut) -> bool {
    match shortcut {
        RunningShortcut::Interrupt
        | RunningShortcut::ScrollUp
        | RunningShortcut::ScrollDown
        | RunningShortcut::PageUp
        | RunningShortcut::PageDown
        | RunningShortcut::HalfPageUp
        | RunningShortcut::HalfPageDown => true,
        RunningShortcut::BackgroundCurrentTurn
        | RunningShortcut::SubmitQueued
        | RunningShortcut::Newline
        | RunningShortcut::EditLatestQueued => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestOperationInterrupt;
    use crossterm::event::{KeyCode, KeyModifiers};
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, ThemeName, ToolConfig,
        WorkflowConfig,
    };
    use orca_core::model::ModelSelection;
    use orca_file_search::{MatchKind, SearchMatch, SearchPhase};
    use orca_runtime::mentions::MentionCandidate;
    use std::path::PathBuf;

    fn config() -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("auto".to_string())),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            subagents: Default::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    fn prepare_two_search_matches(state: &mut AppState) {
        state.push_message(crate::types::ChatMessage::System("alpha one".to_string()));
        state.push_message(crate::types::ChatMessage::System("alpha two".to_string()));
        let theme = Theme::named(ThemeName::Dark);
        let messages = &state.messages;
        let revisions = &state.message_revisions;
        state.transcript_render_cache.prepare(
            messages,
            revisions,
            crate::transcript_view::TranscriptRenderContext::new(&theme, 40, 0, false),
            |_, message, theme, width, tick, force_expand| {
                crate::ui::build_lines_for_messages(
                    std::slice::from_ref(message),
                    theme,
                    width,
                    tick,
                    force_expand,
                )
            },
        );
        state.open_transcript_search();
        state.replace_transcript_search_query("alpha");
        state.refresh_transcript_search();
    }

    #[allow(clippy::too_many_arguments)]
    fn press_status_key(
        code: KeyCode,
        modifiers: KeyModifiers,
        state: &mut AppState,
        config: &mut RunConfig,
        shared: &Arc<Mutex<RunConfig>>,
        action_tx: &mpsc::Sender<UserAction>,
        operation: &TestOperationInterrupt,
        textarea: &mut TextArea,
        vim: &mut VimState,
        theme: &Theme,
    ) {
        let key = KeyEvent::new(code, modifiers);
        let preloaded = Arc::new(Mutex::new(None));
        handle_status_key(
            &Event::Key(key),
            &key,
            state,
            config,
            shared,
            action_tx,
            operation,
            &preloaded,
            textarea,
            vim,
            theme,
            None,
            || Ok(()),
        )
        .unwrap();
    }

    #[test]
    fn running_composer_edits_newlines_queues_and_keeps_scroll_shortcuts() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        state.total_lines = 20;
        state.visible_height = 5;
        state.scroll_offset = 10;
        state.auto_scroll = false;
        let mut config = config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let operation = TestOperationInterrupt::default();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = TextArea::default();

        press_status_key(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(textarea.lines(), &["x".to_string()]);

        press_status_key(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(textarea.lines(), &["x".to_string(), String::new()]);

        assert!(textarea.insert_str("/compact"));
        press_status_key(
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(state.queued_user_messages.len(), 1);
        assert_eq!(state.queued_user_messages[0].visible_text(), "x\n/compact");
        assert!(textarea.is_empty());
        assert_eq!(state.status, AppStatus::Running);
        assert!(action_rx.try_recv().is_err());

        press_status_key(
            KeyCode::Up,
            KeyModifiers::NONE,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(state.scroll_offset, 9);
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn running_vim_edits_and_queued_submit_uses_existing_reset_mode() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        let mut config = config();
        config.vim_mode = true;
        let shared = Arc::new(Mutex::new(config.clone()));
        let operation = TestOperationInterrupt::default();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(true);
        vim.mode = crate::vim::VimMode::Insert;
        let mut textarea = TextArea::default();

        press_status_key(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(textarea.lines(), &["x".to_string()]);

        press_status_key(
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(state.queued_user_messages.len(), 1);
        assert_eq!(vim.mode, crate::vim::VimMode::Normal);
        assert!(textarea.is_empty());
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn running_mention_enter_selects_before_queueing() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/workspace".to_string(),
        );
        state.enter_running();
        state.mention.candidates = vec![MentionCandidate::from_file_match(&SearchMatch {
            root: PathBuf::from("/workspace"),
            path: "item.rs".to_string(),
            kind: MatchKind::File,
            score: 42,
            indices: vec![0],
        })];
        state.mention.phase = Some(SearchPhase::Complete);
        let mut config = config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let operation = TestOperationInterrupt::default();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = crate::composer_textarea::make_textarea_with_text("@ite", &vim, &theme);

        press_status_key(
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(
            crate::composer_textarea::textarea_text(&textarea),
            "@item.rs "
        );
        assert_eq!(state.mention_bindings.bindings().len(), 1);
        assert!(state.queued_user_messages.is_empty());

        press_status_key(
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(state.queued_user_messages.len(), 1);
        assert_eq!(
            state.queued_user_messages[0]
                .submission_bindings()
                .bindings()
                .len(),
            1
        );
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn idle_alt_up_restores_but_waiting_input_keeps_queue_owned() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let queued = || {
            crate::queued_input::QueuedUserMessage::from_composer(
                "queued".to_string(),
                Vec::new(),
                orca_runtime::mentions::MentionBindings::default(),
            )
            .unwrap()
        };
        state.enqueue_user_message(queued()).unwrap();
        let mut config = config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let operation = TestOperationInterrupt::default();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = TextArea::from(["draft"]);

        press_status_key(
            KeyCode::Up,
            KeyModifiers::ALT,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(crate::composer_textarea::textarea_text(&textarea), "queued");
        assert!(state.queued_user_messages.is_empty());

        state.enqueue_user_message(queued()).unwrap();
        state.set_status(AppStatus::WaitingUserInput);
        press_status_key(
            KeyCode::Up,
            KeyModifiers::ALT,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(state.queued_user_messages.len(), 1);
        assert_eq!(crate::composer_textarea::textarea_text(&textarea), "queued");
    }

    #[test]
    fn vim_slash_opens_search_in_every_conversation_status_without_composer_edit() {
        for status in [
            AppStatus::Idle,
            AppStatus::Running,
            AppStatus::WaitingUserInput,
        ] {
            let (action_tx, _action_rx) = mpsc::unbounded();
            let mut state = AppState::new(
                action_tx.clone(),
                "test".to_string(),
                "mock".to_string(),
                "/tmp".to_string(),
            );
            state.set_status(status);
            let mut config = config();
            config.vim_mode = true;
            let shared = Arc::new(Mutex::new(config.clone()));
            let operation = TestOperationInterrupt::default();
            let preloaded = Arc::new(Mutex::new(None));
            let mut textarea = TextArea::from(["draft"]);
            let mut vim = VimState::new(true);
            let theme = Theme::named(ThemeName::Dark);
            let key = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);

            handle_status_key(
                &Event::Key(key),
                &key,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &operation,
                &preloaded,
                &mut textarea,
                &mut vim,
                &theme,
                None,
                || Ok(()),
            )
            .unwrap();

            assert!(state.transcript_search.open, "{status:?}");
            assert_eq!(textarea.lines(), &["draft".to_string()]);
            assert_eq!(operation.call_count(), 0);
        }
    }

    #[test]
    fn vim_search_intent_clears_pending_prefix_before_opening_search() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = config();
        config.vim_mode = true;
        let shared = Arc::new(Mutex::new(config.clone()));
        let operation = TestOperationInterrupt::default();
        let preloaded = Arc::new(Mutex::new(None));
        let mut textarea = TextArea::from(["draft"]);
        let mut vim = VimState::new(true);
        vim.seed_pending_count_for_test();
        let theme = Theme::named(ThemeName::Dark);
        let key = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);

        handle_status_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &preloaded,
            &mut textarea,
            &mut vim,
            &theme,
            None,
            || Ok(()),
        )
        .unwrap();

        assert!(state.transcript_search.open);
        assert!(!vim.has_pending_command_for_test());
    }

    #[test]
    fn vim_n_and_shift_n_navigate_closed_running_search_without_interrupt() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        prepare_two_search_matches(&mut state);
        state.close_transcript_search();
        let first = state.transcript_search.active_ordinal();
        let mut config = config();
        config.vim_mode = true;
        let shared = Arc::new(Mutex::new(config.clone()));
        let operation = TestOperationInterrupt::default();
        let preloaded = Arc::new(Mutex::new(None));
        let mut textarea = TextArea::from(["draft"]);
        let mut vim = VimState::new(true);
        let theme = Theme::named(ThemeName::Dark);

        for code in [KeyCode::Char('n'), KeyCode::Char('N')] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            handle_status_key(
                &Event::Key(key),
                &key,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &operation,
                &preloaded,
                &mut textarea,
                &mut vim,
                &theme,
                None,
                || Ok(()),
            )
            .unwrap();
            if code == KeyCode::Char('n') {
                assert_ne!(state.transcript_search.active_ordinal(), first);
            } else {
                assert_eq!(state.transcript_search.active_ordinal(), first);
            }
        }
        assert_eq!(operation.call_count(), 0);
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn vim_insert_slash_remains_composer_text() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = config();
        config.vim_mode = true;
        let shared = Arc::new(Mutex::new(config.clone()));
        let operation = TestOperationInterrupt::default();
        let preloaded = Arc::new(Mutex::new(None));
        let mut textarea = TextArea::default();
        let mut vim = VimState::new(true);
        vim.mode = crate::vim::VimMode::Insert;
        let theme = Theme::named(ThemeName::Dark);
        let key = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);

        handle_status_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &preloaded,
            &mut textarea,
            &mut vim,
            &theme,
            None,
            || Ok(()),
        )
        .unwrap();

        assert_eq!(textarea.lines(), &["/".to_string()]);
        assert!(!state.transcript_search.open);
    }

    #[test]
    fn compacting_status_keeps_running_interrupt_shortcut() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.set_status(AppStatus::Compacting);
        let mut config = config();
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let operation = TestOperationInterrupt::default();
        let preloaded = Arc::new(Mutex::new(None));
        let mut textarea = TextArea::default();
        let mut vim_state = VimState::new(false);
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let event = Event::Key(key);

        handle_status_key(
            &event,
            &key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &operation,
            &preloaded,
            &mut textarea,
            &mut vim_state,
            &theme,
            None,
            || Ok(()),
        )
        .expect("handle compacting shortcut");

        assert_eq!(operation.call_count(), 1);
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    }

    #[test]
    fn esc_interrupts_running_without_exiting_or_marking_idle() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        let mut config = config();
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let operation = TestOperationInterrupt::default();
        let preloaded = Arc::new(Mutex::new(None));
        let mut textarea = TextArea::default();
        let mut vim_state = VimState::new(false);
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let event = Event::Key(key);

        let flow = handle_status_key(
            &event,
            &key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &operation,
            &preloaded,
            &mut textarea,
            &mut vim_state,
            &theme,
            None,
            || Ok(()),
        )
        .expect("handle running shortcut");

        assert!(matches!(flow, StatusKeyFlow::Continue));
        assert_eq!(state.status, AppStatus::Running);
        assert_eq!(operation.call_count(), 1);
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    }

    #[test]
    fn compacting_status_rejects_background_current_turn_shortcut() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.set_status(AppStatus::Compacting);
        let mut config = config();
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let operation = TestOperationInterrupt::default();
        let preloaded = Arc::new(Mutex::new(None));
        let mut textarea = TextArea::default();
        let mut vim_state = VimState::new(false);
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let key = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        let event = Event::Key(key);

        handle_status_key(
            &event,
            &key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &operation,
            &preloaded,
            &mut textarea,
            &mut vim_state,
            &theme,
            None,
            || Ok(()),
        )
        .expect("handle compacting shortcut");

        assert_eq!(state.status, AppStatus::Compacting);
        assert!(action_rx.try_recv().is_err());
        assert_eq!(operation.call_count(), 0);
    }
}
