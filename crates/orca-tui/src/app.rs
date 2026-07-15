use crossbeam_channel as mpsc;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::ExecutableCommand;
use crossterm::event::{
    self, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    KeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{self, EnterAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use orca_core::cancel::CancelToken;
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::conversation::Message;
use orca_runtime::history;

use crate::background_approval::submit_background_approval_response_for_tui;
use crate::background_tasks::{
    foreground_task_for_tui, notify_recovered_background_approvals_for_tui, stop_task_for_tui,
};
use crate::bridge;
use crate::channels::{tui_event_channel, user_action_channel};
use crate::clipboard;
use crate::composer_textarea::{
    make_setup_textarea, make_textarea, textarea_cursor_byte_index, textarea_text,
};
use crate::frame_scheduler::{FrameScheduler, IterationEvent, run_event_loop_iteration};
use crate::input_event_actions::{
    BatchedInputEvent, MouseFlow, coalesce_input_events, handle_mouse_event, handle_paste_event,
    handle_resize_event, handle_scroll_lines, should_queue_input_event,
};
use crate::key_event_actions::{KeyEventFlow, handle_key_event_preflight};
use crate::mention_search_manager::MentionSearchManager;
use crate::runtime_event_actions::handle_runtime_event;
use crate::status_key_actions::{StatusKeyFlow, handle_status_key};
use crate::submitted_turn::SubmittedTurn;
use crate::terminal_lifecycle::TerminalCleanup;
use crate::theme::Theme;
use crate::types::{AppState, AppStatus, ChatMessage, TuiEvent, UserAction};
use crate::ui;
use crate::vim::VimState;

pub fn run_tui(config: RunConfig) -> i32 {
    match run_tui_inner(config) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("TUI error: {e}");
            1
        }
    }
}

fn run_tui_inner(mut config: RunConfig) -> io::Result<i32> {
    const FRAME_INTERVAL: Duration = Duration::from_millis(16);
    const ANIMATION_INTERVAL: Duration = Duration::from_millis(80);
    const MAX_INPUT_EVENTS_PER_BATCH: usize = 64;
    const MAX_RUNTIME_EVENTS_PER_BATCH: usize = 256;

    terminal::enable_raw_mode()?;
    let mut terminal_cleanup = TerminalCleanup::raw_mode_enabled();
    let mut stdout = io::stdout();
    // Alternate screen: the fullscreen UI owns the whole viewport, and the
    // alt buffer has NO scrollback — so the terminal's native scrollbar
    // cannot drag the viewport away from the frame we repaint (which used to
    // shear the UI). Selection, copying, and wheel scrolling are all
    // implemented in-app, so nothing native is lost; on exit the primary
    // screen returns with the shell's history intact.
    terminal_cleanup.set_alternate_screen(stdout.execute(EnterAlternateScreen).is_ok());
    terminal_cleanup.set_mouse_captured(stdout.execute(EnableMouseCapture).is_ok());
    terminal_cleanup.set_bracketed_paste(stdout.execute(EnableBracketedPaste).is_ok());
    // Kitty keyboard protocol: push enhancement AFTER entering alternate screen,
    // otherwise the terminal may reset the keyboard state stack on screen switch.
    terminal_cleanup.set_keyboard_enhanced(
        stdout
            .execute(PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
            ))
            .is_ok(),
    );

    let backend = CrosstermBackend::new(stdout);

    let (event_tx, event_rx) = tui_event_channel();
    let (action_tx, action_rx) = user_action_channel();
    let (mention_registry_tx, mention_registry_rx) = mpsc::bounded(1);
    let mut mention_search =
        MentionSearchManager::new_roots(mention_search_roots(&config), event_tx.clone());
    let pending_workflow_notifications: bridge::PendingWorkflowNotifications =
        bridge::PendingWorkflowNotifications::new();

    let model_name = config.model.display_name().to_string();

    let needs_setup = config.api_key.is_none();
    let should_show_picker = config.show_session_picker
        && !needs_setup
        && config.prompt.trim().is_empty()
        && !matches!(
            config.history_mode,
            HistoryMode::Resume(_) | HistoryMode::Fork(_)
        );
    let picker_sessions = if should_show_picker {
        orca_runtime::history::list_sessions(20).unwrap_or_default()
    } else {
        Vec::new()
    };

    let cwd_display = config
        .cwd
        .as_deref()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
        .display()
        .to_string();
    let cwd_display = shorten_home(&cwd_display);

    let mut state = AppState::new(
        action_tx.clone(),
        config.app_version.clone(),
        model_name,
        cwd_display,
    );
    state.approval_mode = config.approval_mode;
    state.reasoning_effort = config.reasoning_effort;
    let theme = Theme::named(config.theme);
    if should_show_picker && !picker_sessions.is_empty() {
        state.status = AppStatus::SessionPicker;
        state.session_picker_sessions = picker_sessions;
    }

    if needs_setup {
        state.status = AppStatus::Setup;
        state.setup_step = 0;
    }

    let initial_prompt = if config.prompt.trim().is_empty() {
        None
    } else {
        Some(config.prompt.clone())
    };

    let startup_preloaded_transcript = if matches!(
        config.history_mode,
        HistoryMode::Resume(_) | HistoryMode::Fork(_)
    ) {
        if let Ok(transcript) = orca_runtime::history::load_session(match &config.history_mode {
            HistoryMode::Resume(selector) | HistoryMode::Fork(selector) => selector,
            HistoryMode::Record | HistoryMode::Disabled => "",
        }) {
            for message in &transcript.messages {
                if let Some(chat_message) = chat_message_from_history(message.clone()) {
                    state.push_message(chat_message);
                }
            }
            if let Some((explanation, plan)) = &transcript.plan {
                state.current_plan = Some((explanation.clone(), plan.clone()));
            }
            if !state.messages.is_empty() {
                let label = if matches!(config.history_mode, HistoryMode::Fork(_)) {
                    "Forked saved conversation."
                } else {
                    "Resumed saved conversation."
                };
                state.push_message(ChatMessage::System(label.to_string()));
            }
            // The preloaded transcript is entirely past turns; freeze it so the next
            // turn (or an initial prompt) starts a fresh live suffix.
            state.finalized_count = state.messages.len();
            Some(transcript)
        } else {
            None
        }
    } else {
        None
    };

    let shared_config = Arc::new(Mutex::new(config.clone()));
    let agent_config = Arc::clone(&shared_config);
    let preloaded_transcript: Arc<Mutex<Option<history::SessionTranscript>>> =
        Arc::new(Mutex::new(startup_preloaded_transcript));
    let agent_preloaded = Arc::clone(&preloaded_transcript);
    let agent_event_tx = event_tx.clone();
    let cancel_token = CancelToken::new();
    let agent_cancel = cancel_token.clone();
    let agent_workflow_notifications = pending_workflow_notifications.clone();
    let agent_mcp_configs = config.mcp_servers.clone();

    let _agent_handle = std::thread::spawn(move || {
        let agent_mcp_registry = orca_mcp::initialize_registry(&agent_mcp_configs);
        let _ = mention_registry_tx.send(agent_mcp_registry.clone());
        agent_loop_thread_with_registry(
            agent_config,
            agent_preloaded,
            agent_event_tx,
            action_rx,
            agent_cancel,
            agent_workflow_notifications,
            agent_mcp_registry,
        );
    });

    let mut vim_state = VimState::new(config.vim_mode);
    let mut textarea = if needs_setup {
        make_setup_textarea(&theme)
    } else {
        if let Some(prompt) = initial_prompt.clone() {
            state.push_message(ChatMessage::User(prompt.clone()));
            state.enter_running();
            let _ = action_tx.send(UserAction::Submit(prompt));
        }
        make_textarea(&vim_state, &theme)
    };

    // Fullscreen viewport inside the alternate screen: the UI owns the whole
    // terminal and is fully repainted every frame. Mouse capture is on — the
    // wheel scrolls the conversation and drag-select/copy is implemented
    // in-app (the terminal's modifier-drag still bypasses capture if wanted).
    let mut terminal = Terminal::new(backend)?;
    // Clear once on startup so the first diffing draw starts from a known
    // blank canvas rather than whatever the alt screen came up with.
    terminal.clear()?;

    let exit_code;

    terminal.draw(|f| ui::render(f, &mut state, &textarea, &theme))?;
    let started_at = Instant::now();
    let mut scheduler = FrameScheduler::new(started_at, FRAME_INTERVAL, ANIMATION_INTERVAL);
    scheduler.did_draw(started_at);

    'main: loop {
        let now = Instant::now();
        if let Ok(registry) = mention_registry_rx.try_recv() {
            mention_search.install_registry(registry);
        }
        // The copy notice and edge-drag auto-scroll count as animation so the
        // idle loop keeps drawing frames: the notice until it expires (expiry
        // clears it while THIS iteration still counts as animating, so
        // `did_animate` marks the frame dirty and the final redraw removes it
        // from the screen), and the edge drag so scrolling continues while the
        // pointer sits still on the transcript's first/last row.
        let animation_active = state.status == AppStatus::Running
            || state.copy_notice.is_some()
            || state.drag_edge_scroll.is_some();
        if state.copy_notice.is_some() && state.copy_notice_at(now).is_none() {
            state.copy_notice = None;
        }
        if animation_active && scheduler.animation_due(now) {
            state.advance_tick();
            state.apply_drag_edge_scroll();
            scheduler.did_animate(now);
        }

        let mut input_events = Vec::new();
        if event::poll(scheduler.poll_timeout(now, animation_active))? {
            let first = event::read()?;
            if should_queue_input_event(&first) {
                input_events.push(first);
            }
            while input_events.len() < MAX_INPUT_EVENTS_PER_BATCH && event::poll(Duration::ZERO)? {
                let next = event::read()?;
                if should_queue_input_event(&next) {
                    input_events.push(next);
                }
            }
        }

        let iteration = run_event_loop_iteration(
            &mut scheduler,
            coalesce_input_events(input_events, 3),
            event_rx.try_iter(),
            MAX_INPUT_EVENTS_PER_BATCH,
            MAX_RUNTIME_EVENTS_PER_BATCH,
            Instant::now,
            |event| -> io::Result<Option<i32>> {
                match event {
                    IterationEvent::Input(input_event) => match input_event {
                        BatchedInputEvent::ScrollLines(lines) => {
                            handle_scroll_lines(&mut state, lines, Instant::now());
                        }
                        BatchedInputEvent::Event(ev) => {
                            if handle_paste_event(&ev, &mut state, &config, &mut textarea) {
                                return Ok(None);
                            }
                            if handle_resize_event(&ev, &mut state) {
                                return Ok(None);
                            }
                            match handle_mouse_event(&ev, &mut state, &mut textarea, Instant::now())
                            {
                                MouseFlow::NotMouse => {}
                                MouseFlow::Handled => return Ok(None),
                                MouseFlow::SyntheticEnter => {
                                    // A click confirmed the focused row; run
                                    // the exact same path a real Enter takes.
                                    let enter_key =
                                        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
                                    let enter_event = Event::Key(enter_key);
                                    if let StatusKeyFlow::Exit(code) = handle_status_key(
                                        &enter_event,
                                        &enter_key,
                                        &mut state,
                                        &mut config,
                                        &shared_config,
                                        &action_tx,
                                        &cancel_token,
                                        &preloaded_transcript,
                                        &mut textarea,
                                        &mut vim_state,
                                        &theme,
                                        initial_prompt.clone(),
                                        || clear_terminal_scrollback(&mut terminal),
                                    )? {
                                        return Ok(Some(code));
                                    }
                                    return Ok(None);
                                }
                            }
                            let Event::Key(key) = &ev else {
                                return Ok(None);
                            };
                            match handle_key_event_preflight(
                                *key,
                                &mut state,
                                &mut config,
                                &shared_config,
                                &action_tx,
                                &cancel_token,
                                || clear_terminal_scrollback(&mut terminal),
                            )? {
                                KeyEventFlow::Continue => return Ok(None),
                                KeyEventFlow::Exit(code) => return Ok(Some(code)),
                                KeyEventFlow::Unhandled => {}
                            }

                            if let StatusKeyFlow::Exit(code) = handle_status_key(
                                &ev,
                                key,
                                &mut state,
                                &mut config,
                                &shared_config,
                                &action_tx,
                                &cancel_token,
                                &preloaded_transcript,
                                &mut textarea,
                                &mut vim_state,
                                &theme,
                                initial_prompt.clone(),
                                || clear_terminal_scrollback(&mut terminal),
                            )? {
                                return Ok(Some(code));
                            }
                        }
                    },
                    IterationEvent::Runtime(tui_event) => match tui_event {
                        TuiEvent::MentionSearchDirty { generation } => {
                            let text = textarea_text(&textarea);
                            let cursor = textarea_cursor_byte_index(&textarea);
                            mention_search
                                .consume_dirty_at_cursor(generation, &text, cursor, &mut state);
                        }
                        TuiEvent::MentionCatalogDirty { generation } => {
                            mention_search.consume_catalog_dirty(generation, &mut state);
                        }
                        tui_event => {
                            handle_runtime_event(
                                tui_event,
                                &mut state,
                                &action_tx,
                                &pending_workflow_notifications,
                                &mut textarea,
                                &mut vim_state,
                                &theme,
                            );
                        }
                    },
                }
                Ok(None)
            },
        )?;
        let mention_enabled = MentionSearchManager::is_enabled(&state);
        mention_search.set_roots(mention_search_roots(&config), &mut state);
        let text = textarea_text(&textarea);
        let cursor = textarea_cursor_byte_index(&textarea);
        state.mention_bindings.reconcile(&text);
        mention_search.sync_at_cursor(&text, cursor, mention_enabled, &mut state, Instant::now());
        if let Some(code) = iteration.exit_code {
            exit_code = code;
            break 'main;
        }
        // A finished drag staged its text here; write it out via OSC 52 (plus
        // pbcopy on macOS). The escape sequence is invisible to the UI, so no
        // redraw coordination is needed.
        if let Some(text) = state.pending_clipboard_copy.take() {
            clipboard::copy_to_clipboard(&text);
        }
        if let Some(draw_at) = iteration.draw_at {
            terminal.draw(|f| ui::render(f, &mut state, &textarea, &theme))?;
            scheduler.did_draw(draw_at);
        }
    }

    // TerminalCleanup leaves the alternate screen (restoring the shell's
    // scrollback) and unwinds raw mode / capture modes.
    drop(terminal);
    terminal_cleanup.finish();
    mention_search.shutdown();

    Ok(exit_code)
}

fn mention_search_roots(config: &RunConfig) -> Vec<PathBuf> {
    config
        .runtime_workspace_roots
        .as_ref()
        .filter(|roots| !roots.is_empty())
        .cloned()
        .unwrap_or_else(|| {
            vec![
                config
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
            ]
        })
}

fn shorten_home(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home.as_ref()) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

type InlineTerminal = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Erase the native scrollback and on-screen content. Used by the clear-screen shortcut so a
/// fresh session starts on a clean terminal instead of stacking under the old transcript.
fn clear_terminal_scrollback(terminal: &mut InlineTerminal) -> io::Result<()> {
    use crossterm::terminal::{Clear, ClearType};
    let stdout = terminal.backend_mut();
    stdout.execute(crossterm::cursor::MoveTo(0, 0))?;
    stdout.execute(Clear(ClearType::All))?;
    stdout.execute(Clear(ClearType::Purge))?;
    terminal.clear()?;
    Ok(())
}

fn run_manual_compaction_with_events(
    event_tx: &mpsc::Sender<TuiEvent>,
    cancel: &CancelToken,
    compact: impl FnOnce() -> (usize, usize),
) {
    cancel.reset();
    let _ = event_tx.send(TuiEvent::CompactionStarted);
    let (before_messages, after_messages) = compact();
    let _ = event_tx.send(TuiEvent::Compacted {
        before_messages,
        after_messages,
        reason: "manual".to_string(),
        strategy: "manual".to_string(),
        collapsed_messages: before_messages.saturating_sub(after_messages),
        status_text: "compacted context manually".to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::model::ModelSelection;
    use tui_textarea::TextArea;

    use crate::approval_actions::resolve_approval_option;
    use crate::commands;
    use crate::composer_textarea::{
        insert_composer_paste, insert_pasted_text, make_textarea_with_text, textarea_text,
    };
    use crate::idle_submit_actions::handle_idle_submit;
    use crate::slash_command_actions::handle_slash_command;
    use crate::types::{ApprovalOption, SlashMenu, SlashMenuItem, SubMenu};
    use crate::workflow_notifications::drain_pending_workflow_notifications;
    use crate::workflow_notifications::{
        is_workflow_notification_turn_boundary, queue_workflow_terminal_notification,
        remove_pending_workflow_notification_by_id, submit_pending_workflow_notification,
    };
    use crate::workflow_panel_actions::handle_workflows_panel_key;
    use orca_core::config::{
        ModelRuntimeConfig, OutputFormat, ProviderKind, ThemeName, ToolConfig, WorkflowConfig,
    };
    use tempfile::tempdir;

    fn test_config(history_mode: HistoryMode) -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("auto".to_string())),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: Some("sk-test".to_string()),
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode,
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
            auto_memory: false,
        }
    }

    fn test_state() -> (AppState, mpsc::Receiver<UserAction>) {
        let (tx, rx) = mpsc::unbounded();
        (
            AppState::new(
                tx,
                "0.0.0-test".to_string(),
                "auto".to_string(),
                "/tmp".to_string(),
            ),
            rx,
        )
    }

    #[test]
    fn user_submission_error_emits_rejection_terminal() {
        let (event_tx, event_rx) = mpsc::unbounded();

        send_submission_error(
            &event_tx,
            Some("review @gone.txt"),
            "bound file is no longer available".to_string(),
        );

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::SubmissionRejected { prompt, message })
                if prompt == "review @gone.txt"
                    && message == "bound file is no longer available"
        ));
    }

    #[test]
    fn stale_bound_file_preparation_emits_submission_rejected() {
        let root = tempdir().expect("workspace root");
        let root_path = root
            .path()
            .canonicalize()
            .expect("canonical workspace root");
        let mut config = test_config(HistoryMode::Disabled);
        config.cwd = Some(root_path.clone());
        config.runtime_workspace_roots = Some(vec![root_path.clone()]);
        let config = Arc::new(Mutex::new(config));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let pending_actions = RefCell::new(VecDeque::new());
        let cancel = CancelToken::new();
        let pending_workflow_notifications = test_pending_workflow_notifications();
        let mut session = None;
        let mut pending_pinned_context = Vec::new();
        let prompt = "review @gone.txt";
        let bindings = orca_runtime::mentions::MentionBindings::from_bindings(
            prompt,
            vec![orca_runtime::mentions::MentionBinding {
                start: 7,
                end: prompt.len(),
                visible: "@gone.txt".to_string(),
                target: orca_runtime::mentions::MentionTarget::File {
                    root: root_path,
                    path: "gone.txt".to_string(),
                    kind: orca_runtime::mentions::MentionFileKind::File,
                },
            }],
        );

        handle_submitted_turn_for_tui(
            SubmittedTurn::user_with_mentions(prompt.to_string(), bindings),
            &config,
            &preloaded,
            &mut session,
            &mut pending_pinned_context,
            &event_tx,
            &action_rx,
            &pending_actions,
            &cancel,
            &pending_workflow_notifications,
            &orca_mcp::McpRegistry::default(),
        );

        let rejection = event_rx
            .try_iter()
            .find(|event| matches!(event, TuiEvent::SubmissionRejected { .. }))
            .expect("submission rejection event");
        assert!(matches!(
            rejection,
            TuiEvent::SubmissionRejected { prompt, message }
                if prompt == "review @gone.txt"
                    && message.contains("failed to resolve bound @gone.txt")
        ));
    }

    #[test]
    fn workflow_submission_error_remains_generic() {
        let (event_tx, event_rx) = mpsc::unbounded();

        send_submission_error(&event_tx, None, "workflow failed".to_string());

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Error(message)) if message == "workflow failed"
        ));
    }

    #[test]
    fn esc_clears_mouse_selection_before_other_esc_semantics() {
        let (mut state, _rx) = test_state();
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, _action_rx) = mpsc::unbounded();
        let cancel_token = CancelToken::new();

        let pos = crate::selection::SelectionPos { row: 0, col: 0 };
        let head = crate::selection::SelectionPos { row: 2, col: 5 };
        state.selection = Some(crate::selection::TranscriptSelection {
            anchor: pos,
            head,
            dragging: false,
            granularity: crate::selection::SelectionGranularity::Cell,
            origin: (pos, head),
        });

        let flow = handle_key_event_preflight(
            crossterm::event::KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &cancel_token,
            || Ok(()),
        )
        .expect("preflight");

        assert!(matches!(flow, KeyEventFlow::Continue));
        assert_eq!(state.selection, None);

        // Without a selection, Esc falls through to its usual handling.
        let flow = handle_key_event_preflight(
            crossterm::event::KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &cancel_token,
            || Ok(()),
        )
        .expect("preflight");
        assert!(matches!(flow, KeyEventFlow::Unhandled));
    }

    #[test]
    fn manual_compaction_emits_started_before_running_summary_work() {
        let (event_tx, event_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        run_manual_compaction_with_events(&event_tx, &cancel, || {
            assert!(matches!(
                event_rx.try_recv(),
                Ok(TuiEvent::CompactionStarted)
            ));
            (12, 5)
        });

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Compacted {
                before_messages: 12,
                after_messages: 5,
                ..
            })
        ));
    }

    #[test]
    fn manual_compaction_starts_with_a_fresh_cancel_state() {
        let (event_tx, _event_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        cancel.cancel();

        run_manual_compaction_with_events(&event_tx, &cancel, || {
            assert!(
                !cancel.is_cancelled(),
                "a prior turn interrupt must not cancel the next manual compaction"
            );
            (8, 3)
        });
    }

    fn matching_task_update(
        event: TuiEvent,
        predicate: impl Fn(&orca_core::task_types::BackgroundTaskSummary) -> bool,
    ) -> Option<orca_core::task_types::BackgroundTaskSummary> {
        match event {
            TuiEvent::WorkflowTasksUpdated { tasks } => tasks.into_iter().find(predicate),
            TuiEvent::WorkflowTaskUpdated { task } if predicate(&task) => Some(task),
            _ => None,
        }
    }

    fn workflow_task(id: &str, name: &str) -> orca_core::task_types::BackgroundTaskSummary {
        orca_core::task_types::BackgroundTaskSummary {
            id: id.to_string(),
            task_type: orca_core::task_types::TaskType::Workflow,
            status: orca_core::task_types::TaskStatus::Running,
            is_backgrounded: false,
            description: name.to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some(name.to_string()),
            workflow_run_id: Some(format!("run-{id}")),
            phase_count: Some(1),
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            result: None,
            error: None,
        }
    }

    #[test]
    fn workflows_panel_keys_move_selected_task() {
        let (mut state, _rx) = test_state();
        state.show_workflows();
        state.workflow_panel.tasks = vec![
            workflow_task("task-1", "audit"),
            workflow_task("task-2", "repair"),
        ];

        let action_tx = state.event_tx.clone();

        assert!(handle_workflows_panel_key(
            KeyCode::Down,
            &mut state,
            &action_tx
        ));
        assert_eq!(state.workflow_panel.selected, 1);

        assert!(handle_workflows_panel_key(
            KeyCode::Up,
            &mut state,
            &action_tx
        ));
        assert_eq!(state.workflow_panel.selected, 0);
    }

    #[test]
    fn workflows_panel_enter_opens_selected_background_approval() {
        let (mut state, _rx) = test_state();
        let mut task = workflow_task("task-approval", "approval");
        task.task_type = orca_core::task_types::TaskType::MainSession;
        task.status = orca_core::task_types::TaskStatus::ApprovalRequired;
        task.is_backgrounded = true;
        task.pending_tool_call = Some(orca_core::task_types::PendingToolCallSummary {
            id: "mock-tool-1".to_string(),
            name: "task_list".to_string(),
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            arguments: "{}".to_string(),
        });
        state.show_workflows();
        state.workflow_panel.tasks = vec![task];

        let action_tx = state.event_tx.clone();
        assert!(handle_workflows_panel_key(
            KeyCode::Enter,
            &mut state,
            &action_tx
        ));

        let dialog = state.approval_dialog.as_ref().expect("approval dialog");
        assert_eq!(dialog.background_task_id.as_deref(), Some("task-approval"));
        assert_eq!(state.status, AppStatus::WaitingApproval);
    }

    #[test]
    fn workflows_panel_s_key_handles_selected_running_task() {
        let (mut state, rx) = test_state();
        let mut task = workflow_task("task-running", "running");
        task.status = orca_core::task_types::TaskStatus::Running;
        state.show_workflows();
        state.workflow_panel.tasks = vec![task];

        let action_tx = state.event_tx.clone();
        assert!(handle_workflows_panel_key(
            KeyCode::Char('s'),
            &mut state,
            &action_tx
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::StopTask { task_id }) if task_id == "task-running"
        ));
    }

    #[test]
    fn workflows_panel_f_key_handles_selected_backgrounded_main_session() {
        let (mut state, rx) = test_state();
        let mut task = workflow_task("task-main", "backgrounded");
        task.task_type = orca_core::task_types::TaskType::MainSession;
        task.status = orca_core::task_types::TaskStatus::Running;
        task.is_backgrounded = true;
        state.show_workflows();
        state.workflow_panel.tasks = vec![task];

        let action_tx = state.event_tx.clone();
        assert!(handle_workflows_panel_key(
            KeyCode::Char('f'),
            &mut state,
            &action_tx
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::ForegroundTask { task_id }) if task_id == "task-main"
        ));
    }

    #[test]
    fn background_approval_resolution_sends_request_scoped_action() {
        let (mut state, rx) = test_state();
        let action_tx = state.event_tx.clone();
        state.approval_dialog = Some(crate::types::ApprovalDialog {
            id: "approval-background".to_string(),
            tool: "task_list".to_string(),
            target: None,
            permission_kind: None,
            background_task_id: Some("task-approval".to_string()),
            selected: 0,
            options: vec![ApprovalOption::Once, ApprovalOption::Deny],
            diff: None,
        });
        state.set_status(AppStatus::WaitingApproval);

        resolve_approval_option(&mut state, &action_tx, ApprovalOption::Once);

        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::ResolveBackgroundApproval { id, approved })
                if id == "approval-background" && approved
        ));
        assert_eq!(state.status, AppStatus::Idle);
        assert!(state.approval_dialog.is_none());
    }

    #[test]
    fn foreground_approval_resolution_sends_runtime_interaction_id() {
        let (mut state, rx) = test_state();
        let action_tx = state.event_tx.clone();
        state.update(TuiEvent::ApprovalNeeded {
            id: "approval-foreground".to_string(),
            tool: "bash".to_string(),
            target: Some("cargo test".to_string()),
            preview: None,
        });

        resolve_approval_option(&mut state, &action_tx, ApprovalOption::Once);

        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::Approve { id, approved })
                if id == "approval-foreground" && approved
        ));
        assert_eq!(state.status, AppStatus::Running);
        assert!(state.approval_dialog.is_none());
    }

    #[test]
    fn recovered_background_approval_notifies_tui_user() {
        let registry = orca_runtime::tasks::TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(orca_core::task_types::PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: orca_core::approval_types::ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();
        let (event_tx, event_rx) = mpsc::unbounded();

        assert_eq!(
            notify_recovered_background_approvals_for_tui(&registry, &event_tx),
            1
        );

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::WorkflowTasksUpdated { tasks })
                if tasks.len() == 1
                    && tasks[0].id == task.id
                    && tasks[0].status == orca_core::task_types::TaskStatus::ApprovalRequired
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Notice(message))
                if message.contains("Recovered background session")
                    && message.contains("task_list")
                    && message.contains("waiting for approval")
        ));
    }

    #[test]
    fn resumed_session_announces_recovered_background_approval_on_first_submit() {
        with_orca_home(|home| {
            let session_id = "resume-background-approval-session";
            let registry = orca_runtime::tasks::TaskRegistry::new_persistent(
                session_id.to_string(),
                home.join("task-sessions"),
            )
            .unwrap();
            let task = registry.create_main_session("Needs approval".to_string());
            let task_id = task.id.clone();
            registry.mark_running(&task.id).unwrap();
            registry.mark_backgrounded(&task.id).unwrap();
            registry
                .approval_required_for_pending_tool(
                    &task.id,
                    "approval_required".to_string(),
                    Some(orca_core::task_types::PendingToolCallSummary {
                        id: "mock-tool-1".to_string(),
                        name: "task_list".to_string(),
                        action: orca_core::approval_types::ActionKind::Read,
                        target: None,
                        arguments: "{}".to_string(),
                    }),
                )
                .unwrap();
            drop(registry);

            let config = Arc::new(Mutex::new(test_config(HistoryMode::Resume(
                session_id.to_string(),
            ))));
            let fixture = transcript(session_id);
            let mut writer = history::SessionWriter::start_from_meta(fixture.meta)
                .expect("create resumable approval transcript");
            writer.complete("approval_required").unwrap();
            let transcript =
                history::load_session(session_id).expect("load resumable approval transcript");
            let preloaded = Arc::new(Mutex::new(Some(transcript)));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit("hello".to_string()))
                .unwrap();

            let mut saw_task_refresh = false;
            let mut saw_notice = false;
            let mut seen = Vec::new();
            for _ in 0..20 {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::WorkflowTasksUpdated { tasks } => {
                        saw_task_refresh |= tasks.into_iter().any(|task| {
                            task.id == task_id
                                && task.status
                                    == orca_core::task_types::TaskStatus::ApprovalRequired
                                && task.is_backgrounded
                        });
                    }
                    TuiEvent::Notice(message)
                        if message.contains("Recovered background session")
                            && message.contains("task_list") =>
                    {
                        saw_notice = true;
                    }
                    event => seen.push(format!("{event:?}")),
                }
                if saw_task_refresh && saw_notice {
                    break;
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert!(
                saw_task_refresh,
                "missing recovered task refresh; saw {seen:?}"
            );
            assert!(
                saw_notice,
                "missing recovered approval notice; saw {seen:?}"
            );
        });
    }

    #[test]
    fn background_approval_action_denial_stops_task_and_refreshes_tasks() {
        let registry = orca_runtime::tasks::TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(orca_core::task_types::PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: orca_core::approval_types::ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();
        let (event_tx, event_rx) = mpsc::unbounded();

        let continuation_request = submit_background_approval_response_for_tui(
            Some(&registry),
            "mock-tool-1",
            false,
            &event_tx,
        );

        assert!(continuation_request.is_none());
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, orca_core::task_types::TaskStatus::Stopped);
        assert_eq!(record.pending_tool_call, None);
        assert_eq!(record.pending_tool_approval_response, None);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::WorkflowTasksUpdated { tasks })
                if tasks.len() == 1
                    && tasks[0].status == orca_core::task_types::TaskStatus::Stopped
                    && tasks[0].pending_tool_call.is_none()
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Notice(message))
                if message.contains("Background approval denied")
        ));
    }

    #[test]
    fn stop_task_for_tui_requests_stop_and_refreshes_tasks() {
        let registry = orca_runtime::tasks::TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Running in background".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        let (event_tx, event_rx) = mpsc::unbounded();

        assert!(stop_task_for_tui(Some(&registry), &task.id, &event_tx));

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, orca_core::task_types::TaskStatus::Stopping);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::WorkflowTasksUpdated { tasks })
                if tasks.len() == 1
                    && tasks[0].status == orca_core::task_types::TaskStatus::Stopping
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Notice(message))
                if message.contains("Task stop requested")
                    && message.contains(&task.id)
        ));
    }

    #[test]
    fn stop_task_for_tui_stops_approval_required_task_immediately() {
        let registry = orca_runtime::tasks::TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(orca_core::task_types::PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: orca_core::approval_types::ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();
        let (event_tx, event_rx) = mpsc::unbounded();

        assert!(stop_task_for_tui(Some(&registry), &task.id, &event_tx));

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, orca_core::task_types::TaskStatus::Stopped);
        assert_eq!(record.result.as_deref(), Some("Task stopped"));
        assert_eq!(record.pending_tool_call, None);
        assert_eq!(record.pending_tool_approval_response, None);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::WorkflowTasksUpdated { tasks })
                if tasks.len() == 1
                    && tasks[0].status == orca_core::task_types::TaskStatus::Stopped
                    && tasks[0].pending_tool_call.is_none()
        ));
    }

    #[test]
    fn foreground_task_for_tui_marks_backgrounded_task_and_refreshes_tasks() {
        let registry = orca_runtime::tasks::TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Long answer".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        let (event_tx, event_rx) = mpsc::unbounded();

        assert!(foreground_task_for_tui(
            Some(&registry),
            &task.id,
            &event_tx
        ));

        let record = registry.get(&task.id).unwrap();
        assert!(!record.is_backgrounded);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::WorkflowTasksUpdated { tasks })
                if tasks.len() == 1 && !tasks[0].is_backgrounded
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Notice(message)) if message.contains("returned to foreground")
        ));
    }

    fn transcript(session_id: &str) -> history::SessionTranscript {
        history::SessionTranscript {
            meta: history::SessionMeta {
                schema_version: 1,
                session_id: session_id.to_string(),
                cwd: "/tmp".to_string(),
                provider: "mock".to_string(),
                model: Some("auto".to_string()),
                title: "resumed goal".to_string(),
                created_at: chrono::Utc::now(),
                parent_id: None,
                forked: false,
                approval_mode: None,
                active_permission_profile: None,
                runtime_workspace_roots: Vec::new(),
                permission_rules: Default::default(),
                additional_working_directories: Vec::new(),
                network_domain_permissions: Default::default(),
            },
            messages: Vec::new(),
            compactions: Vec::new(),
            summaries: Vec::new(),
            usage: None,
            plan: None,
            completion_status: None,
            completion_error: None,
            path: std::path::PathBuf::from("/tmp/resumed-goal.jsonl"),
        }
    }

    fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = crate::test_support::lock_process_env();
        let home = tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(home.path())));
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("ORCA_HOME", previous);
            } else {
                std::env::remove_var("ORCA_HOME");
            }
        }
        match result {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    #[test]
    fn incident_goal_usage_counts_cache_as_input_subset() {
        let incident_usage = orca_core::cost_types::UsageTotals {
            input_tokens: 49_909_209,
            output_tokens: 191_567,
            cache_tokens: 47_879_040,
            estimated_cost_usd: 3.156_464_565,
        };

        assert_eq!(goal_tokens_for_usage(incident_usage), 50_100_776);
        assert_eq!(
            goal_token_delta(
                orca_core::cost_types::UsageTotals::default(),
                incident_usage,
            ),
            50_100_776
        );
        assert_ne!(
            goal_tokens_for_usage(incident_usage),
            97_979_816,
            "cache hits are already included in input_tokens"
        );
    }

    #[test]
    fn background_goal_completion_accounts_usage_exactly_once() {
        with_orca_home(|_| {
            let session_id = "background-goal-usage";
            orca_runtime::goals::GoalStore::load_default()
                .replace(
                    session_id,
                    "account detached provider usage",
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    None,
                )
                .unwrap();
            let (event_tx, event_rx) = mpsc::unbounded();
            let (foreground_token_tx, foreground_token_rx) = mpsc::bounded(1);
            let handler = background_goal_completion_handler(
                session_id.to_string(),
                Instant::now(),
                event_tx,
                foreground_token_rx,
            );
            foreground_token_tx.send(200).unwrap();

            handler(bridge::TuiBackgroundTurnCompletion {
                usage: Some(orca_core::cost_types::UsageTotals {
                    input_tokens: 49_909_209,
                    output_tokens: 191_567,
                    cache_tokens: 47_879_040,
                    estimated_cost_usd: 3.156_464_565,
                }),
            });

            let goal = orca_runtime::goals::GoalStore::load_default()
                .get(session_id)
                .unwrap()
                .unwrap();
            assert_eq!(goal.tokens_used, 50_100_976);
            assert_eq!(
                event_rx
                    .try_iter()
                    .filter(|event| matches!(event, TuiEvent::GoalStatus(Some(_))))
                    .count(),
                1
            );
        });
    }

    fn test_pending_workflow_notifications() -> bridge::PendingWorkflowNotifications {
        bridge::PendingWorkflowNotifications::new()
    }

    #[test]
    fn running_background_shortcut_dispatches_action_and_returns_to_idle_without_cancelling() {
        let (mut state, action_rx) = test_state();
        state.status = AppStatus::Running;
        let action_tx = state.event_tx.clone();
        let cancel = CancelToken::new();

        crate::running_actions::handle_running_shortcut(
            crate::shortcuts::RunningShortcut::BackgroundCurrentTurn,
            &mut state,
            &action_tx,
            &cancel,
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::BackgroundCurrentTurn)
        ));
        assert!(!cancel.is_cancelled());
        assert_eq!(state.status, AppStatus::Idle);
    }

    #[test]
    fn empty_recorded_session_goal_show_dispatches_agent_action() {
        let (mut state, rx) = test_state();
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));

        handle_slash_command("/goal", &mut config, &shared_config, &mut state, &action_tx);

        assert!(rx.try_recv().is_err());
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::GoalShow)));
        assert_eq!(state.status, AppStatus::Running);
    }

    #[test]
    fn empty_recorded_agent_loop_goal_show_reports_no_goal() {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                agent_loop_thread(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx.send(UserAction::GoalShow).unwrap();
        let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert!(matches!(event, TuiEvent::GoalStatus(None)));
    }

    #[test]
    fn empty_recorded_agent_loop_goal_controls_report_session_not_started() {
        let cases = [
            UserAction::GoalEdit("better goal".to_string()),
            UserAction::GoalClear,
            UserAction::GoalPause,
        ];

        for action in cases {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx.send(action).unwrap();
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            match event {
                TuiEvent::Error(message) => {
                    assert_eq!(
                        message,
                        "The session must start before you can change a goal."
                    );
                }
                other => panic!("expected goal control error, got {other:?}"),
            }
        }
    }

    #[test]
    fn empty_recorded_agent_loop_goal_resume_without_active_goal_reports_none() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx.send(UserAction::GoalResume).unwrap();
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            cancel.cancel();
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert!(matches!(event, TuiEvent::GoalStatus(None)));
        });
    }

    #[test]
    fn empty_recorded_agent_loop_goal_resume_restores_latest_active_goal() {
        with_orca_home(|home| {
            let mut writer =
                history::SessionWriter::start(home, "mock", Some("auto".to_string()), "goal")
                    .unwrap();
            writer
                .append_message(&orca_core::conversation::Message::user(
                    "previous goal work".to_string(),
                ))
                .unwrap();
            writer.complete("approval_required").unwrap();
            let old_session_id = history::load_session("latest").unwrap().meta.session_id;

            let mut goal_store = orca_runtime::goals::GoalStore::load_default();
            goal_store
                .replace(
                    &old_session_id,
                    "resume me",
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    Some(80_000),
                )
                .unwrap();
            let original = goal_store
                .account_usage(&old_session_id, 23_456, 13 * 60)
                .unwrap()
                .unwrap();
            assert_eq!(original.token_budget, Some(80_000));

            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx.send(UserAction::GoalResume).unwrap();
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            cancel.cancel();
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            let resumed_session_id = match event {
                TuiEvent::GoalUpdated(goal) => {
                    assert_eq!(goal.objective, "resume me");
                    assert_eq!(goal.status, orca_core::goal_types::ThreadGoalStatus::Active);
                    // Resume continues the same thread: the goal must stay on
                    // the original session id; only fork mints a new one.
                    assert_eq!(goal.session_id, old_session_id);
                    assert_eq!(goal.token_budget, Some(80_000));
                    assert_eq!(goal.tokens_used, 23_456);
                    assert_eq!(goal.time_used_seconds, 13 * 60);
                    assert_eq!(goal.created_at, original.created_at);
                    goal.session_id
                }
                other => panic!("expected resumed goal update, got {other:?}"),
            };
            let store = orca_runtime::goals::GoalStore::load_default();
            let persisted = store.get(&resumed_session_id).unwrap().unwrap();
            assert_eq!(
                persisted.status,
                orca_core::goal_types::ThreadGoalStatus::Active
            );
            assert_eq!(persisted.token_budget, Some(80_000));
            assert_eq!(persisted.objective, original.objective);
            assert_eq!(persisted.created_at, original.created_at);
            assert!(persisted.tokens_used >= original.tokens_used);
            assert!(persisted.time_used_seconds >= original.time_used_seconds);
        });
    }

    #[test]
    fn goal_resume_store_failure_preserves_shared_loop_state() {
        with_orca_home(|home| {
            let mut writer =
                history::SessionWriter::start(home, "mock", Some("auto".to_string()), "goal")
                    .unwrap();
            writer
                .append_message(&orca_core::conversation::Message::user(
                    "previous goal work".to_string(),
                ))
                .unwrap();
            writer.complete("approval_required").unwrap();
            let old_session_id = history::load_session("latest").unwrap().meta.session_id;

            orca_runtime::goals::GoalStore::load_default()
                .replace(
                    &old_session_id,
                    "resume atomically",
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    None,
                )
                .unwrap();
            std::fs::create_dir(home.join("goals_1.json.tmp")).unwrap();

            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(Some(transcript("untouched-preloaded"))));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (_action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();
            let pending_actions = RefCell::new(VecDeque::new());
            let pending_workflow_notifications = test_pending_workflow_notifications();
            let mut session = None;

            resume_latest_active_goal_for_tui(
                &mut session,
                &config,
                &preloaded,
                &event_tx,
                &action_rx,
                &pending_actions,
                &cancel,
                &pending_workflow_notifications,
            );
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();

            match event {
                TuiEvent::Error(message) => assert!(
                    message.starts_with("failed to resume goal in restored session:"),
                    "unexpected error: {message}"
                ),
                other => panic!("expected restored-session error, got {other:?}"),
            }
            assert!(session.is_none());
            assert!(matches!(
                &config.lock().unwrap().history_mode,
                HistoryMode::Record
            ));
            assert_eq!(
                preloaded
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|transcript| transcript.meta.session_id.as_str()),
                Some("untouched-preloaded")
            );
        });
    }

    #[test]
    fn preloaded_goal_resume_projects_elapsed_before_first_turn_started() {
        with_orca_home(|_| {
            let session_id = "resume-goal-timer-session";
            let mut goal_store = orca_runtime::goals::GoalStore::load_default();
            goal_store
                .replace(
                    session_id,
                    "resume with elapsed time",
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    None,
                )
                .unwrap();
            let persisted = goal_store
                .account_usage(session_id, 23_456, 13 * 60)
                .unwrap()
                .unwrap();
            assert_eq!(persisted.time_used_seconds, 13 * 60);

            let config = Arc::new(Mutex::new(test_config(HistoryMode::Resume(
                session_id.to_string(),
            ))));
            let fixture = transcript(session_id);
            history::SessionWriter::start_from_meta(fixture.meta)
                .expect("create resumable goal transcript");
            let restored =
                history::load_session(session_id).expect("load resumable goal transcript");
            let preloaded = Arc::new(Mutex::new(Some(restored)));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit("mock_stream_delay_ms 250".to_string()))
                .unwrap();
            let mut projected_goal = None;
            loop {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::GoalStatus(Some(goal)) if goal.session_id == session_id => {
                        projected_goal = Some(goal);
                    }
                    TuiEvent::TurnStarted { .. } => break,
                    TuiEvent::Error(message) => panic!("unexpected resume error: {message}"),
                    _ => {}
                }
            }

            cancel.cancel();
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            let projected_goal = projected_goal
                .expect("active GoalStatus with elapsed time must precede TurnStarted");
            assert_eq!(projected_goal.time_used_seconds, 13 * 60);
        });
    }

    #[test]
    fn preloaded_resume_goal_pause_updates_persisted_goal_before_live_session_exists() {
        with_orca_home(|_| {
            let session_id = "resume-goal-session";
            orca_runtime::goals::GoalStore::load_default()
                .replace(
                    session_id,
                    "resumed objective",
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    None,
                )
                .unwrap();

            let config = Arc::new(Mutex::new(test_config(HistoryMode::Resume(
                session_id.to_string(),
            ))));
            let preloaded = Arc::new(Mutex::new(Some(transcript(session_id))));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx.send(UserAction::GoalPause).unwrap();
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            match event {
                TuiEvent::GoalUpdated(goal) => {
                    assert_eq!(goal.session_id, session_id);
                    assert_eq!(goal.status, orca_core::goal_types::ThreadGoalStatus::Paused);
                }
                other => panic!("expected paused goal update, got {other:?}"),
            }
            let reloaded = orca_runtime::goals::GoalStore::load_default()
                .get(session_id)
                .unwrap()
                .unwrap();
            assert_eq!(
                reloaded.status,
                orca_core::goal_types::ThreadGoalStatus::Paused
            );
        });
    }

    #[test]
    fn preloaded_resume_goal_show_reads_persisted_goal_before_live_session_exists() {
        with_orca_home(|_| {
            let session_id = "resume-goal-show-session";
            orca_runtime::goals::GoalStore::load_default()
                .replace(
                    session_id,
                    "show resumed objective",
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    None,
                )
                .unwrap();

            let config = Arc::new(Mutex::new(test_config(HistoryMode::Resume(
                session_id.to_string(),
            ))));
            let preloaded = Arc::new(Mutex::new(Some(transcript(session_id))));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx.send(UserAction::GoalShow).unwrap();
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            match event {
                TuiEvent::GoalStatus(Some(goal)) => {
                    assert_eq!(goal.session_id, session_id);
                    assert_eq!(goal.objective, "show resumed objective");
                    assert_eq!(goal.status, orca_core::goal_types::ThreadGoalStatus::Active);
                }
                other => panic!("expected resumed goal status, got {other:?}"),
            }
        });
    }

    #[test]
    fn disabled_history_goal_show_still_reports_recorded_history_requirement() {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Disabled)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                agent_loop_thread(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx.send(UserAction::GoalShow).unwrap();
        let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        match event {
            TuiEvent::Error(message) => {
                assert_eq!(
                    message,
                    "persistent goals require recorded history; enable history before using /goal"
                );
            }
            other => panic!("expected recorded-history error, got {other:?}"),
        }
    }

    #[test]
    fn backgrounded_agent_loop_accepts_next_submit_before_first_turn_completes() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit("mock_stream_delay_ms 250".to_string()))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started.") => {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();
            action_tx
                .send(UserAction::Submit("mock_history_echo".to_string()))
                .unwrap();

            let first_followup = loop {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::MessageDelta(text) if text.contains("Mock history users:") => {
                        break "next-submit";
                    }
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow stream completed.") =>
                    {
                        break "first-turn-completed";
                    }
                    _ => {}
                }
            };

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert_eq!(
                first_followup, "next-submit",
                "backgrounding must let the next foreground submit run before the backgrounded turn finishes"
            );
        });
    }

    #[test]
    fn workflow_notification_submit_bypasses_user_file_mention_expansion() {
        with_orca_home(|_| {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            std::fs::write(temp.path().join("outside.txt"), "outside").unwrap();

            let mut cfg = test_config(HistoryMode::Record);
            cfg.cwd = Some(workspace);
            let config = Arc::new(Mutex::new(cfg));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::SubmitWorkflowNotification(
                    crate::types::PendingWorkflowNotification {
                        id: "notification-1".to_string(),
                        prompt: "mock_history_echo\nread @../outside.txt".to_string(),
                    },
                ))
                .unwrap();

            let mut saw_history_echo = false;
            let mut unexpected_error = None;
            for _ in 0..10 {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::MessageDelta(text) if text.contains("Mock history users:") => {
                        saw_history_echo = true;
                        break;
                    }
                    TuiEvent::Error(message) => {
                        unexpected_error = Some(message);
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert_eq!(unexpected_error, None);
            assert!(
                saw_history_echo,
                "workflow notifications should not be preprocessed as user-authored @file mentions"
            );
        });
    }

    #[test]
    fn workflow_notification_submit_uses_notification_task_label() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::SubmitWorkflowNotification(
                    crate::types::PendingWorkflowNotification {
                        id: "notification-1".to_string(),
                        prompt: "<task-notification>mock_history_echo</task-notification>"
                            .to_string(),
                    },
                ))
                .unwrap();

            let task = loop {
                let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                }) {
                    break task;
                }
            };

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert_eq!(task.description, "Workflow notification notification-1");
        });
    }

    #[test]
    fn workflow_notification_first_turn_uses_notification_label_for_session_title() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::SubmitWorkflowNotification(
                    crate::types::PendingWorkflowNotification {
                        id: "notification-1".to_string(),
                        prompt: "<task-notification>mock_history_echo</task-notification>"
                            .to_string(),
                    },
                ))
                .unwrap();

            loop {
                let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                if matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                })
                .is_some()
                {
                    break;
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            let transcript = history::load_session("latest").expect("latest session");
            assert_eq!(
                transcript.meta.title,
                "Workflow notification notification-1"
            );
            assert!(!transcript.meta.title.contains("<task-notification>"));
        });
    }

    #[test]
    fn submitted_turn_boundary_owns_goal_loop_presentation_inputs() {
        let source = include_str!("app.rs");
        let goal_loop_start = source
            .rfind("fn run_goal_turns_for_tui(")
            .expect("goal loop function");
        let agent_loop_start = source
            .rfind("fn agent_loop_thread(")
            .expect("agent loop function");
        let goal_loop_section = &source[goal_loop_start..agent_loop_start];

        assert!(
            goal_loop_section.contains("fn run_goal_turns_for_tui(\n")
                && goal_loop_section.contains("    submitted_turn: SubmittedTurn,\n"),
            "goal loop entry should receive the typed submitted-turn boundary"
        );
        assert!(
            !goal_loop_section.contains("initial_task_description"),
            "task labels should stay inside SubmittedTurn presentation metadata"
        );
        assert!(
            !goal_loop_section.contains("initial_backtrack_target"),
            "backtrack policy should stay inside SubmittedTurn presentation metadata"
        );
        assert!(
            goal_loop_section.contains("submitted_turn.task_label()"),
            "goal loop should read the task label through the submitted-turn boundary"
        );
        assert!(
            goal_loop_section.contains("submitted_turn.is_backtrack_target()"),
            "goal loop should read backtrack policy through the submitted-turn boundary"
        );
        assert!(
            !goal_loop_section.contains(".presentation."),
            "goal loop should not reach into SubmittedTurnPresentation internals"
        );
    }

    #[test]
    fn submitted_turn_workflow_notification_carries_notification_boundary() {
        let source = std::fs::read_to_string(format!(
            "{}/src/submitted_turn.rs",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("submitted_turn source should be readable");
        let impl_start = source
            .find("impl SubmittedTurn {")
            .expect("SubmittedTurn impl");
        let submitted_turn_impl = &source[impl_start..];

        assert!(
            submitted_turn_impl
                .contains("fn workflow_notification(notification: PendingWorkflowNotification)"),
            "workflow notification submitted turns should carry the typed notification boundary"
        );
        assert!(
            !submitted_turn_impl.contains("fn workflow_notification(id: String, prompt: String)"),
            "submitted turns should not split workflow notification id and prompt at construction"
        );
    }

    #[test]
    fn submitted_turn_kind_owns_prompt_source_state() {
        let source = std::fs::read_to_string(format!(
            "{}/src/submitted_turn.rs",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("submitted_turn source should be readable");
        let kind_start = source
            .rfind("enum SubmittedTurnKind {")
            .expect("SubmittedTurnKind enum");
        let submitted_turn_start = source
            .rfind("struct SubmittedTurn {")
            .expect("SubmittedTurn struct");
        let submitted_turn_section = &source[submitted_turn_start..];
        let struct_body = submitted_turn_section
            .split("}")
            .next()
            .expect("SubmittedTurn struct body");

        assert!(
            kind_start < submitted_turn_start,
            "submitted-turn kind should be declared before SubmittedTurn"
        );
        assert!(
            struct_body.contains("kind: SubmittedTurnKind"),
            "SubmittedTurn should store a single kind that owns the prompt/source data"
        );
        assert!(
            !struct_body.contains("prompt: String"),
            "prompt text should live inside SubmittedTurnKind variants"
        );
        assert!(
            !struct_body.contains("source: SubmittedTurnSource"),
            "source state should live inside SubmittedTurnKind variants"
        );
    }

    #[test]
    fn backgrounded_agent_loop_does_not_complete_unexecuted_tool_calls() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit(
                    "mock_stream_tool_delay_ms 250 task_list".to_string(),
                ))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow tool stream started.") =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

            let status = loop {
                let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.is_backgrounded
                        && task.status != orca_core::task_types::TaskStatus::Running
                }) {
                    break task.status;
                }
            };

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert_ne!(
                status,
                orca_core::task_types::TaskStatus::Completed,
                "background completion must not report success for tool calls that were not executed"
            );
        });
    }

    #[test]
    fn backgrounded_agent_loop_marks_unexecuted_tool_calls_approval_required() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit(
                    "mock_stream_tool_delay_ms 250 task_list".to_string(),
                ))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow tool stream started.") =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

            let status = loop {
                let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.is_backgrounded
                        && task.status != orca_core::task_types::TaskStatus::Running
                }) {
                    break task.status;
                }
            };

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::json!("approval_required"),
                "backgrounded turns that stop before executing tool calls must be actionable"
            );
        });
    }

    #[test]
    fn backgrounded_agent_loop_reports_pending_tool_name() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit(
                    "mock_stream_tool_delay_ms 250 task_list".to_string(),
                ))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow tool stream started.") =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

            let pending_tool = loop {
                let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.is_backgrounded
                        && task.status == orca_core::task_types::TaskStatus::ApprovalRequired
                }) {
                    break task.pending_tool_call;
                }
            };

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            let pending_tool = pending_tool.expect("pending tool call");
            assert_eq!(pending_tool.id, "mock-tool-1");
            assert_eq!(pending_tool.name, "task_list");
            assert_eq!(
                pending_tool.action,
                orca_core::approval_types::ActionKind::Read
            );
            assert_eq!(pending_tool.arguments, "{}");
        });
    }

    #[test]
    fn backgrounded_agent_loop_notifies_approval_required_in_user_language() {
        with_orca_home(|home| {
            let mut cfg = test_config(HistoryMode::Record);
            cfg.cwd = Some(home.to_path_buf());
            let config = Arc::new(Mutex::new(cfg));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit(
                    "mock_stream_tool_delay_ms 250 task_list".to_string(),
                ))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow tool stream started.") =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

            let mut notice = None;
            let mut seen = Vec::new();
            for _ in 0..20 {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::Notice(message) if message.starts_with("Background session") => {
                        notice = Some(message);
                        break;
                    }
                    TuiEvent::Notice(message) => {
                        seen.push(format!("notice: {message}"));
                    }
                    TuiEvent::WorkflowTasksUpdated { tasks } => {
                        let statuses = tasks
                            .into_iter()
                            .filter(|task| {
                                task.task_type == orca_core::task_types::TaskType::MainSession
                            })
                            .map(|task| format!("{:?}", task.status))
                            .collect::<Vec<_>>();
                        seen.push(format!("tasks: {}", statuses.join(",")));
                    }
                    TuiEvent::WorkflowTaskUpdated { task }
                        if task.task_type == orca_core::task_types::TaskType::MainSession =>
                    {
                        seen.push(format!("task: {:?}", task.status));
                    }
                    event => seen.push(format!("{event:?}")),
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert_eq!(
                notice.unwrap_or_else(|| panic!("missing background notice; saw {seen:?}")),
                "Background session needs approval for task_list before it can continue."
            );
        });
    }

    #[test]
    fn approved_background_tool_call_executes_and_completes_session() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit(
                    "mock_stream_tool_delay_ms 250 task_list".to_string(),
                ))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow tool stream started.") =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

            let (task_id, approval_id) = loop {
                let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.is_backgrounded
                        && task.status == orca_core::task_types::TaskStatus::ApprovalRequired
                }) {
                    let approval_id = task
                        .pending_tool_call
                        .as_ref()
                        .expect("pending tool call")
                        .id
                        .clone();
                    break (task.id, approval_id);
                }
            };

            action_tx
                .send(UserAction::ResolveBackgroundApproval {
                    id: approval_id,
                    approved: true,
                })
                .unwrap();

            let mut saw_completion_message = false;
            let mut saw_completed_task = false;
            let mut seen = Vec::new();
            for _ in 0..40 {
                match event_rx.recv_timeout(Duration::from_secs(10)) {
                    Ok(TuiEvent::MessageDelta(text)) => {
                        if text.contains("Mock completed after tool execution.") {
                            saw_completion_message = true;
                        }
                        seen.push(format!("delta: {text}"));
                    }
                    Ok(TuiEvent::WorkflowTasksUpdated { tasks }) => {
                        saw_completed_task |= tasks.into_iter().any(|task| {
                            task.id == task_id
                                && task.status == orca_core::task_types::TaskStatus::Completed
                        });
                    }
                    Ok(TuiEvent::WorkflowTaskUpdated { task })
                        if task.id == task_id
                            && task.status == orca_core::task_types::TaskStatus::Completed =>
                    {
                        saw_completed_task = true;
                    }
                    Ok(event) => seen.push(format!("{event:?}")),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        seen.push("timeout".to_string());
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        panic!("agent event channel disconnected before background continuation")
                    }
                }
                if saw_completion_message && saw_completed_task {
                    break;
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert!(
                saw_completion_message,
                "approved background tool call should continue the model loop; saw {seen:?}"
            );
            assert!(
                saw_completed_task,
                "approved background tool call should complete the background task; saw {seen:?}"
            );
        });
    }

    #[test]
    fn approved_background_tool_call_does_not_prompt_again_for_same_tool() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit(
                    "mock_stream_tool_delay_ms 250 mcp__broken__tool".to_string(),
                ))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow tool stream started.") =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

            let approval_id = loop {
                let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.is_backgrounded
                        && task.status == orca_core::task_types::TaskStatus::ApprovalRequired
                        && task
                            .pending_tool_call
                            .as_ref()
                            .is_some_and(|tool| tool.name == "mcp__broken__tool")
                }) {
                    break task
                        .pending_tool_call
                        .as_ref()
                        .expect("pending tool call")
                        .id
                        .clone();
                }
            };

            action_tx
                .send(UserAction::ResolveBackgroundApproval {
                    id: approval_id,
                    approved: true,
                })
                .unwrap();

            let mut saw_tool_requested = false;
            let mut saw_second_approval = false;
            let mut seen = Vec::new();
            for _ in 0..20 {
                match event_rx.recv_timeout(Duration::from_secs(10)) {
                    Ok(TuiEvent::ToolRequested { name, .. }) if name == "mcp__broken__tool" => {
                        saw_tool_requested = true;
                        break;
                    }
                    Ok(TuiEvent::ApprovalNeeded { id, tool, .. }) => {
                        saw_second_approval = true;
                        seen.push(format!("approval: {tool}"));
                        action_tx
                            .send(UserAction::Approve {
                                id,
                                approved: false,
                            })
                            .unwrap();
                        break;
                    }
                    Ok(event) => seen.push(format!("{event:?}")),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        seen.push("timeout".to_string());
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        panic!("agent event channel disconnected before background tool execution")
                    }
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert!(
                saw_tool_requested,
                "approved background tool should execute without a second approval; saw {seen:?}"
            );
            assert!(
                !saw_second_approval,
                "approved background tool should not prompt again for the same call"
            );
        });
    }

    #[test]
    fn idle_app_submits_pending_workflow_notification() {
        let (mut state, _rx) = test_state();
        let (action_tx, action_rx) = mpsc::unbounded();
        state
            .pending_workflow_notifications
            .push_back(crate::types::PendingWorkflowNotification {
                id: "notification-1".to_string(),
                prompt: "<task-notification>done</task-notification>".to_string(),
            });

        submit_pending_workflow_notification(&mut state, &action_tx, true);

        assert_eq!(state.status, AppStatus::Running);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWorkflowNotification(notification))
                if notification.id == "notification-1"
                    && notification.prompt == "<task-notification>done</task-notification>"
        ));
    }

    #[test]
    fn tool_completion_is_not_a_workflow_notification_turn_boundary() {
        assert!(!is_workflow_notification_turn_boundary(
            &TuiEvent::ToolCompleted {
                id: "tool-1".to_string(),
                name: "bash".to_string(),
                status: "completed".to_string(),
                output: String::new(),
                diff: None,
                kind: None,
            }
        ));
        assert!(!is_workflow_notification_turn_boundary(
            &TuiEvent::SubagentCompleted {
                id: "agent-1".to_string(),
                description: "inspect".to_string(),
                status: "success".to_string(),
                output: None,
                error: None,
            }
        ));
    }

    #[test]
    fn session_completion_submits_pending_workflow_notification() {
        let (mut state, _rx) = test_state();
        let (action_tx, action_rx) = mpsc::unbounded();
        state.status = AppStatus::Running;
        state
            .pending_workflow_notifications
            .push_back(crate::types::PendingWorkflowNotification {
                id: "notification-1".to_string(),
                prompt: "<task-notification>failed</task-notification>".to_string(),
            });

        assert!(is_workflow_notification_turn_boundary(
            &TuiEvent::SessionCompleted {
                status: "success".to_string(),
            }
        ));
        submit_pending_workflow_notification(&mut state, &action_tx, false);

        assert_eq!(state.status, AppStatus::Running);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWorkflowNotification(notification))
                if notification.id == "notification-1"
                    && notification.prompt == "<task-notification>failed</task-notification>"
        ));
    }

    #[test]
    fn session_completion_drains_batch_boundary_queue_before_submitting_notification() {
        let (mut state, _rx) = test_state();
        let (action_tx, action_rx) = mpsc::unbounded();
        let queue = test_pending_workflow_notifications();
        assert!(
            queue.push_unique(crate::types::PendingWorkflowNotification {
                id: "notification-1".to_string(),
                prompt: "<task-notification>failed</task-notification>".to_string(),
            })
        );
        state.status = AppStatus::Running;

        drain_pending_workflow_notifications(&mut state, &queue);
        submit_pending_workflow_notification(&mut state, &action_tx, false);

        assert!(queue.is_empty());
        assert!(state.pending_workflow_notifications.is_empty());
        assert_eq!(state.status, AppStatus::Running);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWorkflowNotification(notification))
                if notification.id == "notification-1"
                    && notification.prompt == "<task-notification>failed</task-notification>"
        ));
    }

    #[test]
    fn terminal_workflow_notifications_enter_batch_boundary_queue() {
        let queue = test_pending_workflow_notifications();
        let queued = queue_workflow_terminal_notification(
            &TuiEvent::WorkflowNotification {
                id: "notification-1".to_string(),
                prompt: "<task-notification>done</task-notification>".to_string(),
                status: "completed".to_string(),
                summary: "done".to_string(),
            },
            &queue,
            true,
        );
        assert_eq!(queued.as_deref(), Some("notification-1"));
        let notification = queue.pop_front().expect("notification");
        assert_eq!(notification.id, "notification-1");
        assert_eq!(
            notification.prompt,
            "<task-notification>done</task-notification>"
        );

        let queued = queue_workflow_terminal_notification(
            &TuiEvent::WorkflowNotification {
                id: "notification-2".to_string(),
                prompt: "<task-notification>failed</task-notification>".to_string(),
                status: "failed".to_string(),
                summary: "failed".to_string(),
            },
            &queue,
            true,
        );
        assert_eq!(queued.as_deref(), Some("notification-2"));
        let notification = queue.pop_front().expect("notification");
        assert_eq!(notification.id, "notification-2");
        assert_eq!(
            notification.prompt,
            "<task-notification>failed</task-notification>"
        );

        let queued = queue_workflow_terminal_notification(
            &TuiEvent::WorkflowNotification {
                id: "notification-3".to_string(),
                prompt: "<task-notification>failed</task-notification>".to_string(),
                status: "failed".to_string(),
                summary: "failed".to_string(),
            },
            &queue,
            false,
        );
        assert!(queued.is_none());
        assert!(queue.is_empty());
    }

    #[test]
    fn terminal_workflow_notifications_skip_duplicate_batch_queue_id() {
        let queue = test_pending_workflow_notifications();
        let event = TuiEvent::WorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>done</task-notification>".to_string(),
            status: "completed".to_string(),
            summary: "done".to_string(),
        };

        assert_eq!(
            queue_workflow_terminal_notification(&event, &queue, true).as_deref(),
            Some("notification-1")
        );
        assert!(queue_workflow_terminal_notification(&event, &queue, true).is_none());
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn batch_queued_workflow_notification_is_removed_from_ui_pending_queue_by_id() {
        let (mut state, _rx) = test_state();
        state
            .pending_workflow_notifications
            .push_back(crate::types::PendingWorkflowNotification {
                id: "notification-1".to_string(),
                prompt: "<task-notification>completed</task-notification>".to_string(),
            });
        state
            .pending_workflow_notifications
            .push_back(crate::types::PendingWorkflowNotification {
                id: "notification-2".to_string(),
                prompt: "<task-notification>failed</task-notification>".to_string(),
            });

        remove_pending_workflow_notification_by_id(&mut state, "notification-2");

        assert_eq!(
            state
                .pending_workflow_notifications
                .iter()
                .map(|notification| notification.prompt.as_str())
                .collect::<Vec<_>>(),
            vec!["<task-notification>completed</task-notification>"]
        );
    }

    #[test]
    fn batch_queued_workflow_notification_removal_uses_notification_id() {
        let (mut state, _rx) = test_state();
        state
            .pending_workflow_notifications
            .push_back(crate::types::PendingWorkflowNotification {
                id: "workflow-run-1:tool-use-1".to_string(),
                prompt: "<task-notification>same</task-notification>".to_string(),
            });
        state
            .pending_workflow_notifications
            .push_back(crate::types::PendingWorkflowNotification {
                id: "workflow-run-2:tool-use-2".to_string(),
                prompt: "<task-notification>same</task-notification>".to_string(),
            });

        remove_pending_workflow_notification_by_id(&mut state, "workflow-run-2:tool-use-2");

        let pending = state
            .pending_workflow_notifications
            .iter()
            .map(|notification| notification.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(pending, vec!["workflow-run-1:tool-use-1"]);
    }

    #[test]
    fn settled_messages_remain_in_fullscreen_transcript_after_turn_end() {
        let theme = Theme::named(ThemeName::Dark);
        let (tx, _rx) = mpsc::unbounded();
        let mut state = AppState::new(
            tx,
            "0.0.0-test".to_string(),
            "auto".to_string(),
            "/tmp".to_string(),
        );
        state.messages.push(ChatMessage::User("hi".to_string()));
        state
            .messages
            .push(ChatMessage::Assistant("answer".to_string()));
        state.finalized_count = state.messages.len();
        state.status = AppStatus::Idle;

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10))
            .expect("test backend");

        terminal
            .draw(|frame| ui::render(frame, &mut state, &TextArea::default(), &theme))
            .expect("draw");

        assert_eq!(state.flushed_count, 0);
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("hi"));
        assert!(rendered.contains("answer"));
    }

    #[test]
    fn slash_menu_tab_opens_history_picker_like_enter() {
        with_orca_home(|home| {
            orca_runtime::history::SessionWriter::start(
                home,
                "mock",
                Some("auto".to_string()),
                "history tab test",
            )
            .unwrap();

            let (mut state, _rx) = test_state();
            state.status = AppStatus::Idle;
            state.slash_menu = Some(SlashMenu {
                items: commands::all_commands()
                    .iter()
                    .map(|(command, description)| SlashMenuItem {
                        command: (*command).to_string(),
                        description: (*description).to_string(),
                    })
                    .collect(),
                selected: commands::all_commands()
                    .iter()
                    .position(|(command, _)| *command == "/history")
                    .unwrap(),
                sub_menu: None,
            });
            let mut config = test_config(HistoryMode::Record);
            let shared_config = Arc::new(Mutex::new(config.clone()));
            let (action_tx, _action_rx) = mpsc::unbounded();
            let theme = Theme::named(ThemeName::Dark);
            let mut textarea = make_textarea(&VimState::new(false), &theme);
            let vim_state = VimState::new(false);
            let event = Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ));
            let key = match &event {
                Event::Key(key) => key,
                _ => unreachable!(),
            };

            assert!(crate::slash_menu_actions::handle_slash_menu_key(
                &event,
                key,
                &mut state,
                &mut config,
                &shared_config,
                &action_tx,
                &mut textarea,
                &vim_state,
                &theme,
            ));

            assert_eq!(state.status, AppStatus::SessionPicker);
            assert!(!state.session_picker_sessions.is_empty());
            assert!(state.slash_menu.is_none());
        });
    }

    #[test]
    fn slash_menu_tab_completes_goal_objective_prefix_without_dispatching() {
        let (mut state, _rx) = test_state();
        state.status = AppStatus::Idle;
        state.slash_menu = Some(SlashMenu {
            items: commands::all_commands()
                .iter()
                .map(|(command, description)| SlashMenuItem {
                    command: (*command).to_string(),
                    description: (*description).to_string(),
                })
                .collect(),
            selected: commands::all_commands()
                .iter()
                .position(|(command, _)| *command == "/goal")
                .unwrap(),
            sub_menu: None,
        });
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = make_textarea(&VimState::new(false), &theme);
        let vim_state = VimState::new(false);
        let event = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        ));
        let key = match &event {
            Event::Key(key) => key,
            _ => unreachable!(),
        };

        assert!(crate::slash_menu_actions::handle_slash_menu_key(
            &event,
            key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &mut textarea,
            &vim_state,
            &theme,
        ));

        assert_eq!(textarea_text(&textarea), "/goal ");
        assert_eq!(state.status, AppStatus::Idle);
        assert!(state.slash_menu.is_none());
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn slash_submenu_model_flow_asks_for_reasoning_effort_then_applies_both() {
        let (mut state, _rx) = test_state();
        state.slash_menu = Some(SlashMenu {
            items: Vec::new(),
            selected: 0,
            sub_menu: Some(SubMenu {
                title: "/model".to_string(),
                items: vec!["deepseek-v4-pro".to_string()],
                selected: 0,
                context: None,
            }),
        });
        let mut config = test_config(HistoryMode::Record);
        config.reasoning_effort = orca_core::config::ReasoningEffort::Max;
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = make_textarea(&VimState::new(false), &theme);
        let vim_state = VimState::new(false);

        let press = |key_code: KeyCode,
                     state: &mut AppState,
                     config: &mut RunConfig,
                     textarea: &mut TextArea| {
            let event = Event::Key(crossterm::event::KeyEvent::new(
                key_code,
                crossterm::event::KeyModifiers::NONE,
            ));
            let key = match &event {
                Event::Key(key) => *key,
                _ => unreachable!(),
            };
            assert!(crate::slash_menu_actions::handle_slash_menu_key(
                &event,
                &key,
                state,
                config,
                &shared_config,
                &action_tx,
                textarea,
                &vim_state,
                &theme,
            ));
        };

        // Step 1: picking a model must NOT apply anything yet — it opens the
        // reasoning-effort picker, pre-selected on the current effort (max).
        press(KeyCode::Tab, &mut state, &mut config, &mut textarea);
        let sub = state
            .slash_menu
            .as_ref()
            .and_then(|menu| menu.sub_menu.as_ref())
            .expect("reasoning submenu should open");
        assert_eq!(
            sub.title,
            crate::slash_menu_actions::REASONING_SUBMENU_TITLE
        );
        assert_eq!(sub.context.as_deref(), Some("deepseek-v4-pro"));
        assert!(sub.items[sub.selected].starts_with("max"));
        assert_eq!(state.model_name, "auto", "not applied yet");

        // Step 2: pick "high" (first item), applying model + effort together.
        press(KeyCode::Up, &mut state, &mut config, &mut textarea);
        press(KeyCode::Enter, &mut state, &mut config, &mut textarea);

        assert_eq!(state.model_name, "deepseek-v4-pro");
        assert_eq!(
            state.reasoning_effort,
            orca_core::config::ReasoningEffort::High
        );
        assert_eq!(config.model.display_name(), "deepseek-v4-pro");
        assert_eq!(
            config.reasoning_effort,
            orca_core::config::ReasoningEffort::High
        );
        let shared = shared_config.lock().unwrap();
        assert_eq!(shared.model.display_name(), "deepseek-v4-pro");
        assert_eq!(
            shared.reasoning_effort,
            orca_core::config::ReasoningEffort::High
        );
        drop(shared);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SetModel(model)) if model == "deepseek-v4-pro"
        ));
        assert!(state.slash_menu.is_none());
    }

    #[test]
    fn workflow_slash_command_dispatches_structured_run_action() {
        let (mut state, _rx) = test_state();
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();

        handle_slash_command(
            "/workflow:security-audit target=src maxAgents=8",
            &mut config,
            &shared_config,
            &mut state,
            &action_tx,
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::RunWorkflow { name, args })
                if name == "security-audit" && args.as_deref() == Some("target=src maxAgents=8")
        ));
    }

    #[test]
    fn bracketed_paste_inserts_multiline_text_without_submitting() {
        let (_state, _rx) = test_state();
        let (_action_tx, action_rx) = mpsc::unbounded::<UserAction>();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = make_textarea(&VimState::new(false), &theme);

        assert!(insert_pasted_text(&mut textarea, "alpha\nbravo\ncharlie"));

        assert_eq!(textarea_text(&textarea), "alpha\nbravo\ncharlie");
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn bracketed_paste_can_insert_newline_after_existing_text() {
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = make_textarea_with_text("prefix", &VimState::new(false), &theme);

        assert!(insert_pasted_text(&mut textarea, "\nnext"));

        assert_eq!(textarea_text(&textarea), "prefix\nnext");
    }

    #[test]
    fn large_paste_submits_full_content_and_clears_pending_payload() {
        let (mut state, _rx) = test_state();
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim_state = VimState::new(false);
        let mut textarea = make_textarea(&vim_state, &theme);
        let pasted = "long line\n".repeat(120);

        assert!(insert_composer_paste(
            &mut textarea,
            &mut state.pending_pastes,
            &pasted,
        ));
        assert!(textarea_text(&textarea).starts_with("[Pasted Content "));

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
        ));

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWithMentions { prompt, bindings })
                if prompt == pasted.trim() && bindings.is_empty()
        ));
        assert!(state.pending_pastes.is_empty());
        assert!(textarea_text(&textarea).is_empty());
        assert_eq!(state.input_history, vec![pasted.trim().to_string()]);
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::User(display)) if display.starts_with("[Pasted Content ")
        ));
    }

    #[test]
    fn large_paste_rebases_atomic_mention_binding_before_submit() {
        let (mut state, _rx) = test_state();
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim_state = VimState::new(false);
        let mut textarea = make_textarea(&vim_state, &theme);
        let pasted = "long line\n".repeat(120);
        let mention = "@same.txt";

        assert!(insert_composer_paste(
            &mut textarea,
            &mut state.pending_pastes,
            &pasted,
        ));
        assert!(textarea.insert_str(&format!(" review {mention}")));

        let visible_prompt = textarea_text(&textarea);
        let mention_start = visible_prompt.find(mention).expect("visible mention");
        state.mention_bindings = orca_runtime::mentions::MentionBindings::from_bindings(
            &visible_prompt,
            vec![orca_runtime::mentions::MentionBinding {
                start: mention_start,
                end: mention_start + mention.len(),
                visible: mention.to_string(),
                target: orca_runtime::mentions::MentionTarget::File {
                    root: PathBuf::from("/workspace/backend"),
                    path: "same.txt".to_string(),
                    kind: orca_runtime::mentions::MentionFileKind::File,
                },
            }],
        );

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
        ));

        let action = action_rx.try_recv().expect("submit action");
        let UserAction::SubmitWithMentions { prompt, bindings } = action else {
            panic!("expected mention-aware submit");
        };
        assert_eq!(prompt, format!("{pasted} review {mention}"));
        assert_eq!(bindings.bindings().len(), 1);
        let binding = &bindings.bindings()[0];
        let rebased_start = prompt.find(mention).expect("expanded mention");
        assert_eq!(binding.start, rebased_start);
        assert_eq!(binding.end, rebased_start + mention.len());
        assert_eq!(binding.visible, mention);
    }

    #[test]
    fn repaired_indeterminate_history_tool_renders_state_inspection_warning() {
        let request = orca_core::tool_types::ToolRequest {
            id: "legacy-call".to_string(),
            name: orca_core::tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("deploy".to_string()),
            raw_arguments: None,
        };
        let result = orca_core::tool_types::ToolResult::indeterminate(
            &request,
            "legacy tool call has no terminal result",
        )
        .with_terminal_source(orca_core::tool_types::ToolTerminalSource::CompatibilityRepair);

        let message = chat_message_from_history(Message::Tool {
            tool_call_id: request.id,
            content: "legacy missing result".to_string(),
            terminal: Some(result.terminal().clone()),
            pinned: false,
        })
        .expect("history tool message");

        let ChatMessage::ToolCall {
            status,
            output,
            kind,
            ..
        } = message
        else {
            panic!("expected tool row")
        };
        assert_eq!(status, "indeterminate");
        assert_eq!(kind.as_deref(), Some("indeterminate"));
        assert!(
            output
                .as_deref()
                .is_some_and(|output| output.contains("Inspect external state before retrying"))
        );
    }

    #[test]
    fn idle_submit_carries_atomic_mention_bindings() {
        let (mut state, _rx) = test_state();
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim_state = VimState::new(false);
        let prompt = "review @same.txt";
        let mut textarea = make_textarea_with_text(prompt, &vim_state, &theme);
        state.mention_bindings = orca_runtime::mentions::MentionBindings::from_bindings(
            prompt,
            vec![orca_runtime::mentions::MentionBinding {
                start: 7,
                end: prompt.len(),
                visible: "@same.txt".to_string(),
                target: orca_runtime::mentions::MentionTarget::File {
                    root: PathBuf::from("/workspace/backend"),
                    path: "same.txt".to_string(),
                    kind: orca_runtime::mentions::MentionFileKind::File,
                },
            }],
        );

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
        ));

        let action = action_rx.try_recv().expect("submit action");
        let UserAction::SubmitWithMentions { prompt, bindings } = action else {
            panic!("expected mention-aware submit");
        };
        assert_eq!(prompt, "review @same.txt");
        assert_eq!(bindings.bindings().len(), 1);
        assert_eq!(
            bindings.bindings()[0].target,
            orca_runtime::mentions::MentionTarget::File {
                root: PathBuf::from("/workspace/backend"),
                path: "same.txt".to_string(),
                kind: orca_runtime::mentions::MentionFileKind::File,
            }
        );
    }

    #[test]
    fn idle_submit_with_open_empty_mention_popup_keeps_unbound_at_literal() {
        let (mut state, _rx) = test_state();
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim_state = VimState::new(false);
        let prompt = "@oai/sky还能逆向吗";
        let mut textarea = make_textarea_with_text(prompt, &vim_state, &theme);
        state.mention.phase = Some(orca_file_search::SearchPhase::Scanning);
        assert!(state.mention.candidates.is_empty());
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        crate::idle_key_actions::handle_idle_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &mut textarea,
            &mut vim_state,
            &theme,
        );

        let action = action_rx.try_recv().expect("literal submit action");
        let UserAction::SubmitWithMentions { prompt, bindings } = action else {
            panic!("expected mention-aware submit boundary");
        };
        assert_eq!(prompt, "@oai/sky还能逆向吗");
        assert!(bindings.is_empty());
    }
}

fn ensure_tui_session(
    session: &mut Option<bridge::TuiConversationSession>,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    prompt_for_title: &str,
    event_tx: &mpsc::Sender<TuiEvent>,
    mcp_registry: &orca_mcp::McpRegistry,
) -> Option<String> {
    if session.is_none() {
        let cfg = config.lock().unwrap().clone();
        let transcript = preloaded.lock().unwrap().take();
        *session = match bridge::TuiConversationSession::new_with_preloaded_and_mcp_registry(
            &cfg,
            prompt_for_title,
            transcript,
            mcp_registry.clone(),
        ) {
            Ok(session) => Some(session),
            Err(error) => {
                let _ = event_tx.send(TuiEvent::Error(format!(
                    "failed to initialize conversation history: {error}"
                )));
                None
            }
        };
        if let Some(session) = session.as_ref() {
            notify_recovered_background_approvals_for_tui(session.task_registry(), event_tx);
        }
    }
    match session.as_ref().and_then(|session| session.session_id()) {
        Some(session_id) => Some(session_id.to_string()),
        None => {
            let _ = event_tx.send(TuiEvent::Error(
                "persistent goals require recorded history; enable history before using /goal"
                    .to_string(),
            ));
            None
        }
    }
}

fn update_goal_status_for_session(
    session_id: Option<&str>,
    status: orca_core::goal_types::ThreadGoalStatus,
    event_tx: &mpsc::Sender<TuiEvent>,
) {
    let Some(session_id) = session_id else {
        let _ = event_tx.send(TuiEvent::Error(
            "persistent goals require a saved session".to_string(),
        ));
        return;
    };
    let mut store = orca_runtime::goals::GoalStore::load_default();
    match store.update(
        session_id,
        orca_core::goal_types::GoalUpdate {
            objective: None,
            status: Some(status),
            token_budget: None,
        },
    ) {
        Ok(Some(goal)) => {
            let _ = event_tx.send(TuiEvent::GoalUpdated(goal));
        }
        Ok(None) => {
            let _ = event_tx.send(TuiEvent::Error("no goal is currently set".to_string()));
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!("failed to update goal: {error}")));
        }
    }
}

fn existing_goal_session_id(
    session: Option<&bridge::TuiConversationSession>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    config: &Arc<Mutex<RunConfig>>,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Option<String> {
    if let Some(session_id) = current_goal_session_id(session, preloaded) {
        return Some(session_id);
    }

    let history_mode = config.lock().unwrap().history_mode.clone();
    let message = if matches!(history_mode, HistoryMode::Disabled) {
        "persistent goals require recorded history; enable history before using /goal"
    } else {
        "The session must start before you can change a goal."
    };
    let _ = event_tx.send(TuiEvent::Error(message.to_string()));
    None
}

fn current_goal_session_id(
    session: Option<&bridge::TuiConversationSession>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
) -> Option<String> {
    if let Some(session_id) = session.and_then(|session| session.session_id().map(str::to_string)) {
        return Some(session_id);
    }
    preloaded
        .lock()
        .unwrap()
        .as_ref()
        .map(|transcript| transcript.meta.session_id.clone())
}

const MAX_GOAL_CONTINUATIONS: usize = 64;

fn goal_continuation_prompt(objective: &str, continuation: usize) -> String {
    format!(
        "[Goal continuation #{continuation}]\nContinue working on this persistent goal:\n{objective}\n\nWork from current evidence. Preserve the full objective, verify every requirement before completion, and call update_goal only with status \"complete\" when the goal is actually finished or status \"blocked\" after the same blocker has repeated for at least three consecutive goal turns."
    )
}

fn goal_tokens_for_usage(usage: orca_core::cost_types::UsageTotals) -> i64 {
    usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .min(i64::MAX as u64) as i64
}

fn goal_token_delta(
    before: orca_core::cost_types::UsageTotals,
    after: orca_core::cost_types::UsageTotals,
) -> i64 {
    after
        .input_tokens
        .saturating_sub(before.input_tokens)
        .saturating_add(after.output_tokens.saturating_sub(before.output_tokens))
        .min(i64::MAX as u64) as i64
}

fn account_goal_usage_for_tui(
    session_id: &str,
    token_delta: i64,
    elapsed_delta: i64,
    event_tx: &mpsc::Sender<TuiEvent>,
) {
    if token_delta <= 0 && elapsed_delta <= 0 {
        return;
    }
    if let Ok(Some(goal)) = orca_runtime::goals::GoalStore::load_default().account_usage(
        session_id,
        token_delta,
        elapsed_delta,
    ) {
        let _ = event_tx.send(TuiEvent::GoalStatus(Some(goal)));
    }
}

fn background_goal_completion_handler(
    session_id: String,
    started_at: Instant,
    event_tx: mpsc::Sender<TuiEvent>,
    foreground_token_rx: mpsc::Receiver<i64>,
) -> bridge::TuiBackgroundTurnCompletionHandler {
    Box::new(move |completion: bridge::TuiBackgroundTurnCompletion| {
        let foreground_token_delta = foreground_token_rx.recv().unwrap_or_default();
        let token_delta = foreground_token_delta.saturating_add(
            completion
                .usage
                .map(goal_tokens_for_usage)
                .unwrap_or_default(),
        );
        let elapsed_delta = started_at.elapsed().as_secs().min(i64::MAX as u64) as i64;
        account_goal_usage_for_tui(&session_id, token_delta, elapsed_delta, &event_tx);
    })
}

fn run_goal_turns_for_tui(
    config: &RunConfig,
    session: &mut bridge::TuiConversationSession,
    submitted_turn: SubmittedTurn,
    event_tx: &mpsc::Sender<TuiEvent>,
    action_rx: &mpsc::Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    cancel: &CancelToken,
    starting_continuation: usize,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
) {
    let Some(session_id) = session.session_id().map(str::to_string) else {
        let _ = event_tx.send(TuiEvent::Error(
            "persistent goals require recorded history".to_string(),
        ));
        return;
    };

    let mut submitted_turn = submitted_turn;
    let mut continuation = starting_continuation;
    loop {
        if let Ok(Some(goal)) = orca_runtime::goals::GoalStore::load_default().get(&session_id)
            && goal.status.should_continue()
        {
            session.replace_goal_context(
                orca_runtime::agent_common::format_goal_mode_instructions(&goal),
            );
            let _ = event_tx.send(TuiEvent::GoalStatus(Some(goal)));
        }
        let before_usage = session.runtime_usage_totals();
        let started_at = std::time::Instant::now();
        let (foreground_token_tx, foreground_token_rx) = mpsc::bounded(1);
        let background_completion_handler = background_goal_completion_handler(
            session_id.clone(),
            started_at,
            event_tx.clone(),
            foreground_token_rx,
        );
        let prompt = submitted_turn.prompt().to_string();
        let turn_result = bridge::run_agent_for_tui_with_notification_queue(
            config,
            session,
            &prompt,
            event_tx,
            action_rx,
            pending_actions,
            cancel,
            true,
            submitted_turn.task_label(),
            submitted_turn.is_backtrack_target(),
            Some(pending_workflow_notifications),
            Some(background_completion_handler),
        );
        let status = turn_result.status;
        let token_delta = goal_token_delta(before_usage, session.runtime_usage_totals());
        if status == "backgrounded" {
            let _ = foreground_token_tx.send(token_delta);
        } else {
            let elapsed_delta = started_at.elapsed().as_secs().min(i64::MAX as u64) as i64;
            account_goal_usage_for_tui(&session_id, token_delta, elapsed_delta, event_tx);
        }
        if status != "success" {
            if let Ok(Some(goal)) = orca_runtime::goals::GoalStore::load_default().get(&session_id)
                && goal.status.should_continue()
            {
                let _ = event_tx.send(TuiEvent::Notice(format!(
                    "Goal paused because the last turn ended with status `{status}`. Resolve the issue, then use /goal resume to continue."
                )));
            }
            break;
        }
        if let Some(continuation) = turn_result.continuation {
            // Workflow failure notifications are diagnostic follow-ups for the turn that just
            // finished, so they do not consume goal-continuation quota or wait for the next
            // goal-status poll.
            match continuation {
                bridge::TuiAgentTurnContinuation::WorkflowNotification(notification) => {
                    submitted_turn = SubmittedTurn::workflow_notification(notification);
                    continue;
                }
            }
        }
        if session.has_active_workflows() {
            let _ = event_tx.send(TuiEvent::Notice(
                "Goal is waiting for active workflow tasks to finish.".to_string(),
            ));
            break;
        }

        let goal = match orca_runtime::goals::GoalStore::load_default().get(&session_id) {
            Ok(Some(goal)) => goal,
            Ok(None) => break,
            Err(error) => {
                let _ = event_tx.send(TuiEvent::Error(format!("failed to read goal: {error}")));
                break;
            }
        };
        let _ = event_tx.send(TuiEvent::GoalStatus(Some(goal.clone())));
        if !goal.status.should_continue() {
            let label = orca_core::goal_types::goal_status_label(goal.status);
            let _ = event_tx.send(TuiEvent::Notice(format!(
                "Goal auto-continuation stopped because the goal is {label}."
            )));
            break;
        }
        continuation += 1;
        if continuation > MAX_GOAL_CONTINUATIONS {
            update_goal_status_for_session(
                Some(&session_id),
                orca_core::goal_types::ThreadGoalStatus::UsageLimited,
                event_tx,
            );
            let _ = event_tx.send(TuiEvent::Notice(
                "Goal auto-continuation stopped after reaching the continuation limit.".to_string(),
            ));
            break;
        }
        submitted_turn =
            SubmittedTurn::user(goal_continuation_prompt(&goal.objective, continuation));
    }
}

#[cfg(test)]
fn resume_latest_active_goal_for_tui(
    session: &mut Option<bridge::TuiConversationSession>,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: &mpsc::Sender<TuiEvent>,
    action_rx: &mpsc::Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    cancel: &CancelToken,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
) {
    let registry = orca_mcp::initialize_registry(&config.lock().unwrap().mcp_servers);
    resume_latest_active_goal_for_tui_with_registry(
        session,
        config,
        preloaded,
        event_tx,
        action_rx,
        pending_actions,
        cancel,
        pending_workflow_notifications,
        &registry,
    );
}

#[allow(clippy::too_many_arguments)]
fn resume_latest_active_goal_for_tui_with_registry(
    session: &mut Option<bridge::TuiConversationSession>,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: &mpsc::Sender<TuiEvent>,
    action_rx: &mpsc::Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    cancel: &CancelToken,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
    mcp_registry: &orca_mcp::McpRegistry,
) {
    if matches!(config.lock().unwrap().history_mode, HistoryMode::Disabled) {
        let _ = event_tx.send(TuiEvent::Error(
            "persistent goals require recorded history; enable history before using /goal"
                .to_string(),
        ));
        return;
    }

    let mut store = orca_runtime::goals::GoalStore::load_default();
    let goal = match store.latest_active() {
        Ok(Some(goal)) => goal,
        Ok(None) => {
            let _ = event_tx.send(TuiEvent::GoalStatus(None));
            return;
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!("failed to read goals: {error}")));
            return;
        }
    };

    let transcript = match history::load_session(&goal.session_id) {
        Ok(transcript) => transcript,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "failed to load goal session {}: {error}",
                goal.session_id
            )));
            return;
        }
    };

    let mut cfg = config.lock().unwrap().clone();
    cfg.history_mode = HistoryMode::Resume(goal.session_id.clone());

    let resumed = match bridge::TuiConversationSession::new_with_preloaded_and_mcp_registry(
        &cfg,
        &goal.objective,
        Some(transcript),
        mcp_registry.clone(),
    ) {
        Ok(session) => session,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "failed to initialize resumed goal session: {error}"
            )));
            return;
        }
    };

    let Some(new_session_id) = resumed.session_id().map(str::to_string) else {
        let _ = event_tx.send(TuiEvent::Error(
            "persistent goals require recorded history; enable history before using /goal"
                .to_string(),
        ));
        return;
    };

    let active_goal = match store.resume_into(&goal.session_id, &new_session_id) {
        Ok(Some(goal)) => goal,
        Ok(None) => {
            let _ = event_tx.send(TuiEvent::Error(
                "goal disappeared while restoring its session".to_string(),
            ));
            return;
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "failed to resume goal in restored session: {error}"
            )));
            return;
        }
    };

    if let Ok(mut shared) = config.lock() {
        shared.history_mode = cfg.history_mode.clone();
    }
    *preloaded.lock().unwrap() = None;
    *session = Some(resumed);
    if let Some(session) = session.as_ref() {
        notify_recovered_background_approvals_for_tui(session.task_registry(), event_tx);
    }
    let _ = event_tx.send(TuiEvent::GoalUpdated(active_goal.clone()));
    let _ = event_tx.send(TuiEvent::Notice(
        "Resumed latest active goal in a restored session.".to_string(),
    ));

    if let Some(session) = session.as_mut() {
        let prompt = goal_continuation_prompt(&active_goal.objective, 1);
        run_goal_turns_for_tui(
            &cfg,
            session,
            SubmittedTurn::user(prompt),
            event_tx,
            action_rx,
            pending_actions,
            cancel,
            1,
            pending_workflow_notifications,
        );
    }
}

fn recv_next_user_action(
    action_rx: &mpsc::Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
) -> Result<UserAction, mpsc::RecvError> {
    if let Some(action) = pending_actions.borrow_mut().pop_front() {
        return Ok(action);
    }
    action_rx.recv()
}

fn handle_submitted_turn_for_tui(
    submitted_turn: SubmittedTurn,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    session: &mut Option<bridge::TuiConversationSession>,
    pending_pinned_context: &mut Vec<String>,
    event_tx: &mpsc::Sender<TuiEvent>,
    action_rx: &mpsc::Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    cancel: &CancelToken,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
    mcp_registry: &orca_mcp::McpRegistry,
) {
    cancel.reset();
    let rejection_prompt = submitted_turn.rejection_prompt().map(str::to_string);
    let cfg = config.lock().unwrap().clone();
    let cwd = cfg
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    if session.is_none() {
        let transcript = preloaded.lock().unwrap().take();
        let title_seed = submitted_turn.title_seed(submitted_turn.prompt());
        *session = match bridge::TuiConversationSession::new_with_preloaded_and_mcp_registry(
            &cfg,
            &title_seed,
            transcript,
            mcp_registry.clone(),
        ) {
            Ok(session) => Some(session),
            Err(error) => {
                send_submission_error(
                    event_tx,
                    rejection_prompt.as_deref(),
                    format!("failed to initialize conversation history: {error}"),
                );
                return;
            }
        };
        if let Some(session) = session.as_ref() {
            notify_recovered_background_approvals_for_tui(session.task_registry(), event_tx);
        }
    }
    let workspace_roots = cfg
        .runtime_workspace_roots
        .clone()
        .filter(|roots| !roots.is_empty())
        .unwrap_or_else(|| vec![cwd.clone()]);
    let prompt = match submitted_turn.prompt_for_model(
        &cwd,
        &workspace_roots,
        session
            .as_ref()
            .expect("session initialized")
            .mcp_registry(),
    ) {
        Ok(prompt) => prompt,
        Err(error) => {
            send_submission_error(event_tx, rejection_prompt.as_deref(), error);
            return;
        }
    };
    if let Some(session) = session.as_mut() {
        for context in pending_pinned_context.drain(..) {
            session.add_pinned_context(context);
        }
    }
    run_goal_turns_for_tui(
        &cfg,
        session.as_mut().expect("session initialized"),
        submitted_turn.with_model_prompt(prompt),
        event_tx,
        action_rx,
        pending_actions,
        cancel,
        0,
        pending_workflow_notifications,
    );
    if cfg.desktop_notifications {
        let _ = orca_runtime::notify::notify("Orca", "Task completed");
    }
}

fn send_submission_error(
    event_tx: &mpsc::Sender<TuiEvent>,
    rejection_prompt: Option<&str>,
    message: String,
) {
    if let Some(prompt) = rejection_prompt {
        let _ = event_tx.send(TuiEvent::SubmissionRejected {
            prompt: prompt.to_string(),
            message,
        });
    } else {
        let _ = event_tx.send(TuiEvent::Error(message));
    }
}

#[cfg(test)]
fn agent_loop_thread(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
    cancel: CancelToken,
    pending_workflow_notifications: bridge::PendingWorkflowNotifications,
) {
    let mcp_registry = orca_mcp::initialize_registry(&config.lock().unwrap().mcp_servers);
    agent_loop_thread_with_registry(
        config,
        preloaded,
        event_tx,
        action_rx,
        cancel,
        pending_workflow_notifications,
        mcp_registry,
    );
}

fn agent_loop_thread_with_registry(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
    cancel: CancelToken,
    pending_workflow_notifications: bridge::PendingWorkflowNotifications,
    mcp_registry: orca_mcp::McpRegistry,
) {
    let mut session: Option<bridge::TuiConversationSession> = None;
    let mut pending_pinned_context: Vec<String> = Vec::new();
    let pending_actions: RefCell<VecDeque<UserAction>> = RefCell::new(VecDeque::new());

    loop {
        match recv_next_user_action(&action_rx, &pending_actions) {
            Ok(UserAction::Submit(prompt)) => {
                handle_submitted_turn_for_tui(
                    SubmittedTurn::user(prompt),
                    &config,
                    &preloaded,
                    &mut session,
                    &mut pending_pinned_context,
                    &event_tx,
                    &action_rx,
                    &pending_actions,
                    &cancel,
                    &pending_workflow_notifications,
                    &mcp_registry,
                );
            }
            Ok(UserAction::SubmitWithMentions { prompt, bindings }) => {
                handle_submitted_turn_for_tui(
                    SubmittedTurn::user_with_mentions(prompt, bindings),
                    &config,
                    &preloaded,
                    &mut session,
                    &mut pending_pinned_context,
                    &event_tx,
                    &action_rx,
                    &pending_actions,
                    &cancel,
                    &pending_workflow_notifications,
                    &mcp_registry,
                );
            }
            Ok(UserAction::SubmitWorkflowNotification(notification)) => {
                handle_submitted_turn_for_tui(
                    SubmittedTurn::workflow_notification(notification),
                    &config,
                    &preloaded,
                    &mut session,
                    &mut pending_pinned_context,
                    &event_tx,
                    &action_rx,
                    &pending_actions,
                    &cancel,
                    &pending_workflow_notifications,
                    &mcp_registry,
                );
            }
            Ok(UserAction::RunWorkflow { name, args }) => {
                cancel.reset();
                let cfg = config.lock().unwrap().clone();
                if session.is_none() {
                    let prompt = format!("Run saved workflow `{name}`");
                    let transcript = preloaded.lock().unwrap().take();
                    session =
                        match bridge::TuiConversationSession::new_with_preloaded_and_mcp_registry(
                            &cfg,
                            &prompt,
                            transcript,
                            mcp_registry.clone(),
                        ) {
                            Ok(session) => Some(session),
                            Err(error) => {
                                let _ = event_tx.send(TuiEvent::Error(format!(
                                    "failed to initialize conversation history: {error}"
                                )));
                                continue;
                            }
                        };
                    if let Some(session) = session.as_ref() {
                        notify_recovered_background_approvals_for_tui(
                            session.task_registry(),
                            &event_tx,
                        );
                    }
                }
                if let Some(session) = session.as_ref() {
                    bridge::launch_saved_workflow_for_tui(
                        &cfg,
                        session,
                        &name,
                        args.as_deref(),
                        &event_tx,
                    );
                }
                if cfg.desktop_notifications {
                    let _ = orca_runtime::notify::notify("Orca", "Workflow launched");
                }
            }
            Ok(UserAction::Interrupt) => {
                // Cancel already set by TUI thread; just continue waiting for next Submit
            }
            Ok(UserAction::SetModel(model)) => {
                if let Some(session) = session.as_mut() {
                    session.set_model(Some(&model));
                }
            }
            Ok(UserAction::Remember(note)) => {
                let context = format!("[Pinned remembered note]\n{}", note.trim());
                if let Some(session) = session.as_mut() {
                    session.add_pinned_context(context);
                } else {
                    pending_pinned_context.push(context);
                }
            }
            Ok(UserAction::Compact) => {
                if let Some(session) = session.as_mut() {
                    let cfg = config.lock().unwrap().clone();
                    let cwd = cfg
                        .cwd
                        .clone()
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                    run_manual_compaction_with_events(&event_tx, &cancel, || {
                        session.compact(&cfg, &cwd, &cancel)
                    });
                } else {
                    let _ = event_tx.send(TuiEvent::Error("nothing to compact".to_string()));
                }
            }
            Ok(UserAction::Backtrack) => {
                if let Some(session) = session.as_mut() {
                    match session.backtrack_last_user() {
                        Some(prompt) => {
                            let _ = event_tx.send(TuiEvent::Backtracked { prompt });
                        }
                        None => {
                            let _ =
                                event_tx.send(TuiEvent::Error("nothing to backtrack".to_string()));
                        }
                    }
                } else {
                    let _ = event_tx.send(TuiEvent::Error("nothing to backtrack".to_string()));
                }
            }
            Ok(UserAction::BackgroundCurrentTurn) => {}
            Ok(UserAction::StopTask { task_id }) => {
                stop_task_for_tui(
                    session.as_ref().map(|session| session.task_registry()),
                    &task_id,
                    &event_tx,
                );
            }
            Ok(UserAction::ForegroundTask { task_id }) => {
                foreground_task_for_tui(
                    session.as_ref().map(|session| session.task_registry()),
                    &task_id,
                    &event_tx,
                );
            }
            Ok(UserAction::ResolveBackgroundApproval { id, approved }) => {
                let continuation_request = submit_background_approval_response_for_tui(
                    session.as_ref().map(|session| session.task_registry()),
                    &id,
                    approved,
                    &event_tx,
                );
                if approved
                    && let Some(continuation_request) = continuation_request
                    && let Some(session) = session.as_mut()
                {
                    let cfg = config.lock().unwrap().clone();
                    let goal_session_id = session.session_id().map(str::to_string);
                    let before_usage = session.runtime_usage_totals();
                    let started_at = Instant::now();
                    let result = bridge::continue_approved_background_turn_for_tui(
                        &cfg,
                        session,
                        &continuation_request,
                        &event_tx,
                        &action_rx,
                        &pending_actions,
                        &cancel,
                        Some(&pending_workflow_notifications),
                    );
                    if let Some(goal_session_id) = goal_session_id {
                        let token_delta =
                            goal_token_delta(before_usage, session.runtime_usage_totals());
                        let elapsed_delta =
                            started_at.elapsed().as_secs().min(i64::MAX as u64) as i64;
                        account_goal_usage_for_tui(
                            &goal_session_id,
                            token_delta,
                            elapsed_delta,
                            &event_tx,
                        );
                    }
                    if let Some(continuation) = result.continuation {
                        match continuation {
                            bridge::TuiAgentTurnContinuation::WorkflowNotification(
                                notification,
                            ) => {
                                pending_actions.borrow_mut().push_front(
                                    UserAction::SubmitWorkflowNotification(notification),
                                );
                            }
                        }
                    }
                }
            }
            Ok(UserAction::GoalShow) => {
                let session_id = session
                    .as_ref()
                    .and_then(|s| s.session_id().map(str::to_string))
                    .or_else(|| {
                        preloaded
                            .lock()
                            .unwrap()
                            .as_ref()
                            .map(|transcript| transcript.meta.session_id.clone())
                    });
                let Some(session_id) = session_id else {
                    let history_mode = config.lock().unwrap().history_mode.clone();
                    if matches!(history_mode, HistoryMode::Disabled) {
                        let _ = event_tx.send(TuiEvent::Error(
                            "persistent goals require recorded history; enable history before using /goal"
                                .to_string(),
                        ));
                    } else {
                        let _ = event_tx.send(TuiEvent::GoalStatus(None));
                    }
                    continue;
                };
                let store = orca_runtime::goals::GoalStore::load_default();
                match store.get(&session_id) {
                    Ok(goal) => {
                        let _ = event_tx.send(TuiEvent::GoalStatus(goal));
                    }
                    Err(error) => {
                        let _ =
                            event_tx.send(TuiEvent::Error(format!("failed to read goal: {error}")));
                    }
                }
            }
            Ok(UserAction::GoalSet(objective)) => {
                let Some(session_id) = ensure_tui_session(
                    &mut session,
                    &config,
                    &preloaded,
                    &objective,
                    &event_tx,
                    &mcp_registry,
                ) else {
                    continue;
                };
                let mut store = orca_runtime::goals::GoalStore::load_default();
                match store.replace(
                    &session_id,
                    &objective,
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    None,
                ) {
                    Ok(goal) => {
                        let _ = event_tx.send(TuiEvent::GoalUpdated(goal));
                        let _ = event_tx.send(TuiEvent::Notice(
                            "Starting goal. Automatic continuation will keep running while it remains active.".to_string(),
                        ));
                        if let Some(session) = session.as_mut() {
                            let cfg = config.lock().unwrap().clone();
                            run_goal_turns_for_tui(
                                &cfg,
                                session,
                                SubmittedTurn::user(objective),
                                &event_tx,
                                &action_rx,
                                &pending_actions,
                                &cancel,
                                0,
                                &pending_workflow_notifications,
                            );
                        }
                    }
                    Err(error) => {
                        let _ =
                            event_tx.send(TuiEvent::Error(format!("failed to set goal: {error}")));
                    }
                }
            }
            Ok(UserAction::GoalEdit(objective)) => {
                let Some(session_id) =
                    existing_goal_session_id(session.as_ref(), &preloaded, &config, &event_tx)
                else {
                    continue;
                };
                let mut store = orca_runtime::goals::GoalStore::load_default();
                match store.update(
                    &session_id,
                    orca_core::goal_types::GoalUpdate {
                        objective: Some(objective),
                        status: Some(orca_core::goal_types::ThreadGoalStatus::Active),
                        token_budget: None,
                    },
                ) {
                    Ok(Some(goal)) => {
                        let _ = event_tx.send(TuiEvent::GoalUpdated(goal));
                    }
                    Ok(None) => {
                        let _ =
                            event_tx.send(TuiEvent::Error("no goal is currently set".to_string()));
                    }
                    Err(error) => {
                        let _ =
                            event_tx.send(TuiEvent::Error(format!("failed to edit goal: {error}")));
                    }
                }
            }
            Ok(UserAction::GoalClear) => {
                let Some(session_id) =
                    existing_goal_session_id(session.as_ref(), &preloaded, &config, &event_tx)
                else {
                    continue;
                };
                let mut store = orca_runtime::goals::GoalStore::load_default();
                match store.clear(&session_id) {
                    Ok(_) => {
                        let _ = event_tx.send(TuiEvent::GoalCleared);
                    }
                    Err(error) => {
                        let _ = event_tx
                            .send(TuiEvent::Error(format!("failed to clear goal: {error}")));
                    }
                }
            }
            Ok(UserAction::GoalPause) => {
                if let Some(session_id) =
                    existing_goal_session_id(session.as_ref(), &preloaded, &config, &event_tx)
                {
                    update_goal_status_for_session(
                        Some(&session_id),
                        orca_core::goal_types::ThreadGoalStatus::Paused,
                        &event_tx,
                    );
                }
            }
            Ok(UserAction::GoalResume) => {
                let Some(session_id) = current_goal_session_id(session.as_ref(), &preloaded) else {
                    resume_latest_active_goal_for_tui_with_registry(
                        &mut session,
                        &config,
                        &preloaded,
                        &event_tx,
                        &action_rx,
                        &pending_actions,
                        &cancel,
                        &pending_workflow_notifications,
                        &mcp_registry,
                    );
                    continue;
                };
                update_goal_status_for_session(
                    Some(&session_id),
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    &event_tx,
                );
                if let Some(session) = session.as_mut() {
                    if let Some(goal) = session
                        .session_id()
                        .filter(|id| *id == session_id)
                        .and_then(|id| orca_runtime::goals::GoalStore::load_default().get(id).ok())
                        .flatten()
                    {
                        let cfg = config.lock().unwrap().clone();
                        let prompt = goal_continuation_prompt(&goal.objective, 1);
                        run_goal_turns_for_tui(
                            &cfg,
                            session,
                            SubmittedTurn::user(prompt),
                            &event_tx,
                            &action_rx,
                            &pending_actions,
                            &cancel,
                            1,
                            &pending_workflow_notifications,
                        );
                    }
                }
            }
            Ok(UserAction::Cancel) | Err(_) => break,
            Ok(
                UserAction::Approve { .. }
                | UserAction::RespondToUserInput { .. }
                | UserAction::RespondToMcpElicitation { .. },
            ) => {}
        }
    }
}

pub(crate) fn chat_message_from_history(message: Message) -> Option<ChatMessage> {
    match message {
        Message::System { .. } => None,
        Message::User { content, .. } => Some(ChatMessage::User(content)),
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => {
            if let Some(content) = content.filter(|text| !text.trim().is_empty()) {
                Some(ChatMessage::Assistant(content))
            } else if let Some(reasoning) = reasoning_content.filter(|text| !text.trim().is_empty())
            {
                Some(ChatMessage::Reasoning(reasoning))
            } else if !tool_calls.is_empty() {
                let names = tool_calls
                    .iter()
                    .map(|tool| tool.function_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(ChatMessage::System(format!(
                    "Previous assistant requested tools: {names}"
                )))
            } else {
                None
            }
        }
        Message::Tool {
            tool_call_id,
            content,
            terminal,
            ..
        } => {
            let status = terminal
                .as_ref()
                .map(|terminal| terminal.status.as_str())
                .unwrap_or("completed")
                .to_string();
            let kind = terminal
                .as_ref()
                .and_then(|terminal| serde_json::to_value(terminal.kind).ok())
                .and_then(|value| value.as_str().map(str::to_string));
            let mut output = content;
            if output.is_empty()
                && let Some(error) = terminal
                    .as_ref()
                    .and_then(|terminal| terminal.error.as_ref())
            {
                output = error.clone();
            }
            if status == "indeterminate" && !output.contains("Inspect external state") {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str("State is unknown. Inspect external state before retrying.");
            }
            Some(ChatMessage::ToolCall {
                id: tool_call_id.clone(),
                name: format!("tool:{tool_call_id}"),
                target: None,
                status,
                output: (!output.is_empty()).then_some(output),
                diff: None,
                kind,
                expanded: false,
            })
        }
    }
}
