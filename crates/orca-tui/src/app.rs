use crossbeam_channel as mpsc;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::ExecutableCommand;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tui_textarea::{Input, TextArea};

#[cfg(test)]
use orca_core::cancel::CancelToken;
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::conversation::Message;
use orca_runtime::history;
use orca_runtime::runtime_host::{
    HostedGenerationHandlers, HostedOperationKind, HostedTurnRequest, HostedWorkflowRequest,
    OperationOutcome, RuntimeHostHandle, RuntimeThreadHandle, RuntimeThreadMutation,
    RuntimeThreadStartRequest,
};

use crate::agent_runtime::TuiAgentRuntime;
use crate::background_approval::submit_background_approval_response_for_tui;
use crate::background_tasks::{
    foreground_task_for_tui, notify_recovered_background_approvals_for_tui, stop_task_for_tui,
};
use crate::bridge;
use crate::capability_backend::CapabilityBackend;
use crate::channels::{tui_event_channel, user_action_channel};
use crate::clipboard;
use crate::composer_input_actions::refresh_input_menus;
use crate::composer_textarea::{
    make_setup_textarea, make_textarea, textarea_cursor_byte_index, textarea_text,
};
use crate::diagnostics::{
    DiagnosticSnapshot, KeybindingsDiagnostic, KeybindingsLocation, SnapshotInput,
};
use crate::frame_scheduler::{FrameScheduler, IterationEvent, run_event_loop_iteration};
use crate::hosted_runtime::{TuiHostedEventObserver, TuiHostedOperationOutcome};
use crate::input_event_actions::{
    BatchedInputEvent, MouseFlow, coalesce_input_events, consume_focus_event, handle_mouse_event,
    handle_paste_event, handle_resize_event, handle_scroll_lines, should_queue_input_event,
};
use crate::input_runtime::{InputControl, InputRuntime, InputRuntimeOptions};
use crate::interaction_broker::TuiInteractionBroker;
use crate::key_event_actions::{DynamicKeyEventFlow, handle_key_event_preflight_dynamic};
#[cfg(test)]
use crate::key_event_actions::{KeyEventFlow, handle_key_event_preflight};
use crate::keybindings::{
    InputOwnerFingerprint, KeymapReloader, KeymapRuntime, ModalOwner, ReloadOutcome,
    ShortcutInvocation, keybindings_location,
};
use crate::mention_search_manager::MentionSearchManager;
use crate::operation_controller::{TuiOperationController, TuiTurnControl};
use crate::runtime_event_actions::handle_runtime_event;
use crate::runtime_interaction_adapter::{
    TuiApprovalHandler, TuiMcpElicitationHandler, TuiPermissionRequestHandler, TuiUserInputHandler,
};
#[cfg(test)]
use crate::status_key_actions::handle_status_key;
use crate::status_key_actions::{StatusKeyFlow, handle_status_key_dynamic};
use crate::submitted_turn::SubmittedTurn;
use crate::terminal_capabilities::{TerminalProfile, resolve_base_theme};
use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};
use crate::theme::Theme;
use crate::types::{AppState, AppStatus, ChatMessage, TuiEvent, UserAction};
use crate::ui;
use crate::vim::{PendingInsertEscapeFlow, VimState};
use crate::workspace_status;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingInsertEscapeRouting {
    Continue,
    Consumed,
}

pub(crate) fn input_owner_fingerprint(
    state: &AppState,
    vim_state: &VimState,
) -> InputOwnerFingerprint {
    let context = match state.status {
        AppStatus::Idle | AppStatus::WaitingUserInput => crate::shortcuts::ShortcutContext::Idle,
        AppStatus::Running | AppStatus::Compacting => crate::shortcuts::ShortcutContext::Running,
        AppStatus::WaitingApproval => crate::shortcuts::ShortcutContext::Approval,
        AppStatus::Setup | AppStatus::SessionPicker => crate::shortcuts::ShortcutContext::Global,
    };
    let modal = if state.status == AppStatus::Setup {
        ModalOwner::Setup
    } else if state.status == AppStatus::SessionPicker {
        ModalOwner::SessionPicker
    } else if state.status == AppStatus::WaitingApproval {
        ModalOwner::Approval
    } else if state.transcript_search.open {
        ModalOwner::TranscriptSearch
    } else if state.show_shortcuts {
        ModalOwner::Shortcuts
    } else if state.slash_menu.is_some() {
        ModalOwner::SlashMenu
    } else if state.mention.phase.is_some() || !state.mention.candidates.is_empty() {
        ModalOwner::MentionMenu
    } else if state.panel_mode != crate::types::PanelMode::Conversation {
        ModalOwner::WorkflowPanel
    } else {
        ModalOwner::None
    };
    InputOwnerFingerprint {
        context,
        modal,
        panel: state.panel_mode,
        vim_mode: vim_state.enabled.then_some(vim_state.mode),
    }
}

fn keybinding_poll_timeout(
    frame_timeout: Duration,
    now: Instant,
    chord_deadline: Option<Instant>,
) -> Duration {
    chord_deadline
        .map(|deadline| deadline.saturating_duration_since(now))
        .map_or(frame_timeout, |chord_wait| frame_timeout.min(chord_wait))
}

fn refresh_after_insert_escape_flush(
    state: &mut AppState,
    config: &RunConfig,
    textarea: &TextArea<'_>,
) {
    state.reset_history_navigation();
    refresh_input_menus(textarea, state, config);
}

fn resolve_pending_insert_escape_before_routing(
    event: &Event,
    now: Instant,
    vim_state: &mut VimState,
    textarea: &mut TextArea<'_>,
    state: &mut AppState,
    config: &RunConfig,
    theme: &Theme,
) -> PendingInsertEscapeRouting {
    let Event::Key(key) = event else {
        return PendingInsertEscapeRouting::Continue;
    };
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return PendingInsertEscapeRouting::Continue;
    }
    match vim_state.resolve_pending_insert_escape(&Input::from(event.clone()), now, textarea) {
        PendingInsertEscapeFlow::Consumed => {
            vim_state.configure_block(textarea, theme);
            PendingInsertEscapeRouting::Consumed
        }
        PendingInsertEscapeFlow::Flushed => {
            refresh_after_insert_escape_flush(state, config, textarea);
            PendingInsertEscapeRouting::Continue
        }
        PendingInsertEscapeFlow::NoPending => PendingInsertEscapeRouting::Continue,
    }
}

fn flush_pending_insert_escape_before_non_key(
    vim_state: &mut VimState,
    textarea: &mut TextArea<'_>,
    state: &mut AppState,
    config: &RunConfig,
) -> bool {
    if !vim_state.flush_pending_insert_escape(textarea) {
        return false;
    }
    refresh_after_insert_escape_flush(state, config, textarea);
    true
}

fn flush_expired_insert_escape(
    now: Instant,
    vim_state: &mut VimState,
    textarea: &mut TextArea<'_>,
    state: &mut AppState,
    config: &RunConfig,
) -> bool {
    if !vim_state.flush_expired_insert_escape(now, textarea) {
        return false;
    }
    refresh_after_insert_escape_flush(state, config, textarea);
    true
}

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
    let pending_input_runtime = InputRuntime::start(InputRuntimeOptions {
        theme: config.theme,
        focus_events: config.terminal_notifications,
    })?;
    let terminal_identity = qwertty::caps::identity_from_env(None, qwertty::caps::std_env_source);
    let terminal_profile = pending_input_runtime.profile();
    let presentation_profile = TerminalPresentationProfile::from_identity(&terminal_identity);
    let theme = Theme::resolve(config.theme, terminal_profile);
    let input_rx = pending_input_runtime.events().clone();
    let focus_rx = pending_input_runtime.focus_events().clone();
    let input_control_rx = pending_input_runtime.controls().clone();
    let presentation =
        TerminalPresentation::new(config.terminal_notifications, presentation_profile);

    const FRAME_INTERVAL: Duration = Duration::from_millis(16);
    const ANIMATION_INTERVAL: Duration = Duration::from_millis(80);
    const MAX_INPUT_EVENTS_PER_BATCH: usize = 64;
    const MAX_RUNTIME_EVENTS_PER_BATCH: usize = 256;
    const MAX_SUPERVISED_TUI_TASKS: usize = 32;

    let backend = CapabilityBackend::new(CrosstermBackend::new(io::stdout()), theme.color_level);

    let workspace_root = syntax_workspace_root(&config);
    let (event_tx, pending_event_rx) = tui_event_channel();
    let (action_tx, action_rx) = user_action_channel();
    let (mention_registry_tx, mention_registry_rx) = mpsc::bounded(1);
    let mut mention_search = MentionSearchManager::new_roots(
        mention_search_roots(&config, &workspace_root),
        event_tx.clone(),
    );
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

    let workspace_status = workspace_status::snapshot(&workspace_root);
    let mut state = AppState::new(
        action_tx.clone(),
        config.app_version.clone(),
        model_name,
        workspace_status.cwd,
    );
    let mut keymap_runtime = KeymapRuntime::new(Arc::clone(&state.keymap));
    let keybindings_location = keybindings_location();
    state.keybindings_diagnostic = KeybindingsDiagnostic::built_ins(keymap_runtime.generation());
    state.diagnostics = diagnostic_snapshot_for_startup(
        &config,
        &terminal_identity,
        terminal_profile,
        presentation_profile,
        keybindings_location,
    );
    let mut keymap_reloader = crate::keybindings::keybindings_path()
        .map(|path| KeymapReloader::start(path, Instant::now()));
    if let Some(reloader) = &mut keymap_reloader {
        reloader.request_reload(Instant::now());
    }
    state.workspace_git = workspace_status.git;
    state.approval_mode = config.approval_mode;
    state.reasoning_effort = config.reasoning_effort;
    state.auth_configured = config.api_key.is_some();
    if should_show_picker && !picker_sessions.is_empty() {
        state.status = AppStatus::SessionPicker;
        state.session_picker_sessions = picker_sessions;
    }

    if needs_setup {
        state.status = AppStatus::Setup;
        state.initialize_onboarding(&config);
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
        orca_runtime::history::load_session(match &config.history_mode {
            HistoryMode::Resume(selector) | HistoryMode::Fork(selector) => selector,
            HistoryMode::Record | HistoryMode::Disabled => "",
        })
        .ok()
    } else {
        None
    };
    let replay_messages = startup_preloaded_transcript
        .iter()
        .flat_map(|transcript| transcript.messages.iter().cloned())
        .filter_map(chat_message_from_history);
    configure_and_preload_tui_state(
        &mut state,
        workspace_root.clone(),
        theme.syntax_theme,
        theme.color_level,
        replay_messages,
    );
    if let Some(transcript) = &startup_preloaded_transcript {
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
    }

    let shared_config = Arc::new(Mutex::new(config.clone()));
    let agent_config = Arc::clone(&shared_config);
    let preloaded_transcript: Arc<Mutex<Option<history::SessionTranscript>>> =
        Arc::new(Mutex::new(startup_preloaded_transcript));
    let agent_preloaded = Arc::clone(&preloaded_transcript);
    let agent_event_tx = event_tx.clone();
    let agent_workflow_notifications = pending_workflow_notifications.clone();
    let agent_mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
    let _ = mention_registry_tx.send(agent_mcp_registry.clone());
    let agent_controller = TuiOperationController::hosted(TuiInteractionBroker::default());

    let mut agent_runtime = match TuiAgentRuntime::spawn_hosted(
        action_rx,
        event_tx.clone(),
        MAX_SUPERVISED_TUI_TASKS,
        agent_controller,
        move |agent_controller, command_rx, host| {
            hosted_tui_controller_loop(
                agent_config,
                agent_preloaded,
                agent_event_tx,
                command_rx,
                agent_controller,
                agent_workflow_notifications,
                agent_mcp_registry,
                host,
            );
        },
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            let mut terminal_input = pending_input_runtime;
            terminal_input.finish()?;
            return Err(error);
        }
    };
    // Declare terminal ownership after the agent runtime. The cleanup wrapper
    // below resets presentation output, drops ratatui, and then joins qwertty
    // on every non-panic return from the frame loop.
    let terminal_input = pending_input_runtime;
    let event_rx = pending_event_rx;

    let mut vim_state =
        VimState::with_insert_escape(config.vim_mode, config.vim_insert_escape.clone());
    state.sync_vim_mode(&vim_state);
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

    let resources = (terminal, presentation, terminal_input);
    let exit_code = with_terminal_presentation_cleanup(
        resources,
        |(terminal, presentation, _terminal_input)| {
            let initial_status = state.status;
            initialize_terminal_presentation(
                terminal,
                |terminal| {
                    let _ = presentation
                        .write_pending(terminal.backend_mut().inner_mut(), initial_status);
                    Ok(())
                },
                |terminal| {
                    let (_, started_at, completed_at) =
                        measure_successful_draw(Instant::now, || {
                            terminal
                                .draw(|f| ui::render(f, &mut state, &textarea, &theme))
                                .map(|_| ())
                        })?;
                    state
                        .frame_metrics
                        .record_successful_draw(started_at, completed_at);
                    Ok(())
                },
            )?;
            let started_at = Instant::now();
            let mut scheduler = FrameScheduler::new(started_at, FRAME_INTERVAL, ANIMATION_INTERVAL);
            scheduler.did_draw(started_at);

            let exit_code = 'main: loop {
                let now = Instant::now();
                keymap_runtime.expire_pending(now);
                if let Some(reloader) = &mut keymap_reloader {
                    reloader.request_reload(now);
                    if let Some(observation) = reloader.try_recv() {
                        match keymap_runtime.apply_observation(observation) {
                            ReloadOutcome::Unchanged => {
                                if !keymap_runtime.last_observation_rejected() {
                                    state
                                        .keybindings_diagnostic
                                        .accepted_unchanged(keymap_runtime.generation());
                                }
                            }
                            ReloadOutcome::Applied => {
                                state
                                    .keybindings_diagnostic
                                    .applied_custom(keymap_runtime.generation());
                                state.keymap = keymap_runtime.keymap();
                                scheduler.mark_dirty();
                            }
                            ReloadOutcome::RestoredDefaults => {
                                state
                                    .keybindings_diagnostic
                                    .restored_built_ins(keymap_runtime.generation());
                                state.keymap = keymap_runtime.keymap();
                                scheduler.mark_dirty();
                            }
                            ReloadOutcome::Rejected(message) => {
                                state
                                    .keybindings_diagnostic
                                    .rejected(keymap_runtime.generation());
                                state.push_message(ChatMessage::System(message));
                                scheduler.mark_dirty();
                            }
                        }
                    }
                }
                if flush_expired_insert_escape(
                    now,
                    &mut vim_state,
                    &mut textarea,
                    &mut state,
                    &config,
                ) {
                    scheduler.mark_dirty();
                }
                if let Ok(registry) = mention_registry_rx.try_recv() {
                    mention_search.install_registry(registry);
                }
                poll_edit_highlight(&mut state, &mut scheduler);
                // The copy notice and edge-drag auto-scroll count as animation so the
                // idle loop keeps drawing frames: the notice until it expires (expiry
                // clears it while THIS iteration still counts as animating, so
                // `did_animate` marks the frame dirty and the final redraw removes it
                // from the screen), and the edge drag so scrolling continues while the
                // pointer sits still on the transcript's first/last row.
                let animation_active = state.status == AppStatus::Running
                    || state.fps_hud_enabled
                    || state.copy_notice.is_some()
                    || state.drag_edge_scroll.is_some()
                    || edit_highlight_animation_active(&state)
                    || presentation.animation_active(state.status);
                if state.copy_notice.is_some() && state.copy_notice_at(now).is_none() {
                    state.copy_notice = None;
                }
                if animation_active && scheduler.animation_due(now) {
                    state.advance_tick();
                    presentation.advance_tick();
                    state.apply_drag_edge_scroll();
                    scheduler.did_animate(now);
                }

                let input_events = match receive_prioritized_input_or_control(
                    &input_rx,
                    &focus_rx,
                    &input_control_rx,
                    keybinding_poll_timeout(
                        scheduler.poll_timeout(now, animation_active),
                        now,
                        keymap_runtime.next_deadline(),
                    ),
                    MAX_INPUT_EVENTS_PER_BATCH,
                ) {
                    Ok(InputWake::Events(events)) => events
                        .into_iter()
                        .filter(should_queue_input_event)
                        .collect(),
                    Ok(InputWake::Suspend { acknowledge }) => {
                        keymap_runtime.clear_for_suspend();
                        state.frame_metrics.reset_rolling();
                        acknowledge.send(()).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "terminal input runtime dropped suspend acknowledgement",
                            )
                        })?;
                        loop {
                            match input_control_rx.recv() {
                                Ok(InputControl::Resumed) => {
                                    resume_terminal_render(terminal, &mut scheduler, presentation)?;
                                    break;
                                }
                                Ok(InputControl::Suspend { acknowledge }) => {
                                    let _ = acknowledge.send(());
                                }
                                Err(_) => {
                                    return Err(io::Error::new(
                                        io::ErrorKind::UnexpectedEof,
                                        "terminal input runtime disconnected while suspended",
                                    ));
                                }
                            }
                        }
                        Vec::new()
                    }
                    Ok(InputWake::Resumed) | Err(mpsc::RecvTimeoutError::Timeout) => Vec::new(),
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "terminal input runtime disconnected",
                        ));
                    }
                };

                let iteration = run_event_loop_iteration(
                    &mut scheduler,
                    coalesce_input_events(input_events, 3),
                    event_rx.try_iter(),
                    usize::MAX,
                    MAX_RUNTIME_EVENTS_PER_BATCH,
                    Instant::now,
                    |event| -> io::Result<Option<i32>> {
                        match event {
                            IterationEvent::Input(input_event) => match input_event {
                                BatchedInputEvent::ScrollLines(lines) => {
                                    keymap_runtime.clear_for_non_key();
                                    flush_pending_insert_escape_before_non_key(
                                        &mut vim_state,
                                        &mut textarea,
                                        &mut state,
                                        &config,
                                    );
                                    vim_state.cancel_pending_command();
                                    handle_scroll_lines(&mut state, lines, Instant::now());
                                }
                                BatchedInputEvent::Event(ev) => {
                                    state.sync_vim_mode(&vim_state);
                                    if consume_focus_event(&ev, presentation) {
                                        keymap_runtime.clear_for_non_key();
                                        return Ok(None);
                                    }
                                    if resolve_pending_insert_escape_before_routing(
                                        &ev,
                                        Instant::now(),
                                        &mut vim_state,
                                        &mut textarea,
                                        &mut state,
                                        &config,
                                        &theme,
                                    ) == PendingInsertEscapeRouting::Consumed
                                    {
                                        return Ok(None);
                                    }
                                    if matches!(ev, Event::Paste(_)) {
                                        keymap_runtime.clear_for_non_key();
                                        flush_pending_insert_escape_before_non_key(
                                            &mut vim_state,
                                            &mut textarea,
                                            &mut state,
                                            &config,
                                        );
                                    }
                                    if handle_paste_event(&ev, &mut state, &config, &mut textarea) {
                                        vim_state.cancel_pending_command();
                                        return Ok(None);
                                    }
                                    if handle_resize_event(&ev, &mut state) {
                                        keymap_runtime.clear_for_non_key();
                                        return Ok(None);
                                    }
                                    if matches!(ev, Event::Mouse(_)) {
                                        keymap_runtime.clear_for_non_key();
                                        flush_pending_insert_escape_before_non_key(
                                            &mut vim_state,
                                            &mut textarea,
                                            &mut state,
                                            &config,
                                        );
                                    }
                                    match handle_mouse_event(
                                        &ev,
                                        &mut state,
                                        &mut textarea,
                                        Instant::now(),
                                    ) {
                                        MouseFlow::NotMouse => {}
                                        MouseFlow::Handled => {
                                            vim_state.cancel_pending_command();
                                            return Ok(None);
                                        }
                                        MouseFlow::SyntheticEnter => {
                                            keymap_runtime.clear_for_non_key();
                                            vim_state.cancel_pending_command();
                                            // A click confirmed the focused row; run
                                            // the exact same path a real Enter takes.
                                            let enter_key =
                                                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
                                            let enter_event = Event::Key(enter_key);
                                            let owner = input_owner_fingerprint(&state, &vim_state);
                                            if let StatusKeyFlow::Exit(code) =
                                                handle_status_key_dynamic(
                                                    &enter_event,
                                                    &enter_key,
                                                    Instant::now(),
                                                    owner,
                                                    &mut keymap_runtime,
                                                    None,
                                                    &mut state,
                                                    &mut config,
                                                    &shared_config,
                                                    &action_tx,
                                                    agent_runtime.controller(),
                                                    &preloaded_transcript,
                                                    &mut textarea,
                                                    &mut vim_state,
                                                    &theme,
                                                    initial_prompt.clone(),
                                                    || clear_terminal_scrollback(terminal),
                                                )?
                                            {
                                                return Ok(Some(code));
                                            }
                                            return Ok(None);
                                        }
                                    }
                                    let Event::Key(key) = &ev else {
                                        return Ok(None);
                                    };
                                    let owner = input_owner_fingerprint(&state, &vim_state);
                                    let contextual_invocation: Option<ShortcutInvocation>;
                                    match handle_key_event_preflight_dynamic(
                                        *key,
                                        Instant::now(),
                                        owner,
                                        &mut keymap_runtime,
                                        &mut state,
                                        &mut config,
                                        &shared_config,
                                        &action_tx,
                                        agent_runtime.controller(),
                                        &mut vim_state,
                                        || clear_terminal_scrollback(terminal),
                                    )? {
                                        DynamicKeyEventFlow::Continue => return Ok(None),
                                        DynamicKeyEventFlow::Exit(code) => return Ok(Some(code)),
                                        DynamicKeyEventFlow::Context(invocation) => {
                                            contextual_invocation = Some(invocation);
                                        }
                                        DynamicKeyEventFlow::Unhandled => {
                                            contextual_invocation = None;
                                        }
                                    }

                                    let owner = input_owner_fingerprint(&state, &vim_state);
                                    if let StatusKeyFlow::Exit(code) = handle_status_key_dynamic(
                                        &ev,
                                        key,
                                        Instant::now(),
                                        owner,
                                        &mut keymap_runtime,
                                        contextual_invocation,
                                        &mut state,
                                        &mut config,
                                        &shared_config,
                                        &action_tx,
                                        agent_runtime.controller(),
                                        &preloaded_transcript,
                                        &mut textarea,
                                        &mut vim_state,
                                        &theme,
                                        initial_prompt.clone(),
                                        || clear_terminal_scrollback(terminal),
                                    )? {
                                        return Ok(Some(code));
                                    }
                                }
                            },
                            IterationEvent::Runtime(tui_event) => match tui_event {
                                TuiEvent::MentionSearchDirty { generation } => {
                                    let text = textarea_text(&textarea);
                                    let cursor = textarea_cursor_byte_index(&textarea);
                                    mention_search.consume_dirty_at_cursor(
                                        generation, &text, cursor, &mut state,
                                    );
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
                                        presentation,
                                    );
                                }
                            },
                        }
                        Ok(None)
                    },
                )?;
                state
                    .frame_metrics
                    .record_iteration(iteration.input_events, iteration.runtime_events);
                state.sync_vim_mode(&vim_state);
                let mention_enabled = MentionSearchManager::is_enabled(&state);
                mention_search
                    .set_roots(mention_search_roots(&config, &workspace_root), &mut state);
                let text = textarea_text(&textarea);
                let cursor = textarea_cursor_byte_index(&textarea);
                state.mention_bindings.reconcile(&text);
                mention_search.sync_at_cursor(
                    &text,
                    cursor,
                    mention_enabled,
                    &mut state,
                    Instant::now(),
                );
                if let Some(code) = iteration.exit_code {
                    keymap_runtime.clear_for_non_key();
                    break 'main code;
                }
                // A finished drag staged its text here; write it out via OSC 52 (plus
                // pbcopy on macOS). The escape sequence is invisible to the UI, so no
                // redraw coordination is needed.
                if let Some(text) = state.pending_clipboard_copy.take() {
                    clipboard::copy_to_clipboard(&text);
                }
                let _ =
                    presentation.write_pending(terminal.backend_mut().inner_mut(), state.status);
                if let Some(draw_at) = iteration.draw_at {
                    let (_, started_at, completed_at) =
                        measure_successful_draw(Instant::now, || {
                            terminal
                                .draw(|f| ui::render(f, &mut state, &textarea, &theme))
                                .map(|_| ())
                        })?;
                    state
                        .frame_metrics
                        .record_successful_draw(started_at, completed_at);
                    scheduler.did_draw(draw_at);
                }
            };
            Ok(exit_code)
        },
        |(terminal, mut presentation, mut terminal_input)| {
            finish_terminal_presentation(
                terminal,
                |terminal| {
                    let _ = presentation.write_reset_title(terminal.backend_mut().inner_mut());
                    Ok(())
                },
                drop,
                || terminal_input.finish(),
            )
        },
    )?;
    mention_search.shutdown();
    drop(event_rx);
    agent_runtime.shutdown()?;

    Ok(exit_code)
}

fn diagnostic_snapshot_for_startup(
    config: &RunConfig,
    terminal_identity: &qwertty::TerminalIdentity,
    terminal_profile: TerminalProfile,
    presentation_profile: TerminalPresentationProfile,
    keybindings_location: KeybindingsLocation,
) -> DiagnosticSnapshot {
    DiagnosticSnapshot::new(SnapshotInput {
        app_version: &config.app_version,
        terminal_identity,
        terminal_profile,
        presentation_profile,
        requested_theme: config.theme,
        resolved_theme: resolve_base_theme(config.theme, terminal_profile.background),
        terminal_notifications: config.terminal_notifications,
        desktop_notifications: config.desktop_notifications,
        focus_events_requested: config.terminal_notifications,
        vim_mode: config.vim_mode,
        keybindings_location,
    })
}

fn measure_successful_draw<T, F, Clock>(
    mut now: Clock,
    draw: F,
) -> io::Result<(T, Instant, Instant)>
where
    F: FnOnce() -> io::Result<T>,
    Clock: FnMut() -> Instant,
{
    let started_at = now();
    let value = draw()?;
    let completed_at = now();
    Ok((value, started_at, completed_at))
}

fn resume_terminal_render(
    terminal: &mut InlineTerminal,
    scheduler: &mut FrameScheduler,
    presentation: &mut TerminalPresentation,
) -> io::Result<()> {
    complete_presentation_resume(
        terminal,
        Terminal::clear,
        |_| presentation.invalidate_title(),
        |_| scheduler.mark_dirty(),
    )
}

fn initialize_terminal_presentation<T>(
    target: &mut T,
    write_title: impl FnOnce(&mut T) -> io::Result<()>,
    draw: impl FnOnce(&mut T) -> io::Result<()>,
) -> io::Result<()> {
    write_title(target)?;
    draw(target)
}

fn complete_presentation_resume<T>(
    target: &mut T,
    clear_terminal: impl FnOnce(&mut T) -> io::Result<()>,
    invalidate_title: impl FnOnce(&mut T),
    mark_dirty: impl FnOnce(&mut T),
) -> io::Result<()> {
    clear_terminal(target)?;
    invalidate_title(target);
    mark_dirty(target);
    Ok(())
}

fn finish_terminal_presentation<T>(
    mut terminal: T,
    reset_title: impl FnOnce(&mut T) -> io::Result<()>,
    drop_terminal: impl FnOnce(T),
    finish_input: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    reset_title(&mut terminal)?;
    drop_terminal(terminal);
    finish_input()
}

fn with_terminal_presentation_cleanup<T, R>(
    mut resource: T,
    body: impl FnOnce(&mut T) -> io::Result<R>,
    cleanup: impl FnOnce(T) -> io::Result<()>,
) -> io::Result<R> {
    let result = body(&mut resource);
    let cleanup_result = cleanup(resource);
    match result {
        Err(error) => Err(error),
        Ok(value) => cleanup_result.map(|()| value),
    }
}

#[cfg(test)]
fn receive_input_batch(
    receiver: &mpsc::Receiver<Event>,
    timeout: Duration,
    limit: usize,
) -> Result<Vec<Event>, mpsc::RecvTimeoutError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let first = receiver.recv_timeout(timeout)?;
    let mut events = Vec::with_capacity(limit.min(receiver.len().saturating_add(1)));
    events.push(first);
    while events.len() < limit {
        match receiver.try_recv() {
            Ok(event) => events.push(event),
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
        }
    }
    Ok(events)
}

enum InputWake {
    Events(Vec<Event>),
    Suspend {
        acknowledge: tokio::sync::oneshot::Sender<()>,
    },
    Resumed,
}

#[cfg(test)]
fn receive_input_or_control(
    events: &mpsc::Receiver<Event>,
    controls: &mpsc::Receiver<InputControl>,
    timeout: Duration,
    limit: usize,
) -> Result<InputWake, mpsc::RecvTimeoutError> {
    let timeout_rx = mpsc::after(timeout);
    crossbeam_channel::select_biased! {
        recv(controls) -> control => {
            match control {
                Ok(InputControl::Suspend { acknowledge }) => {
                    Ok(InputWake::Suspend { acknowledge })
                }
                Ok(InputControl::Resumed) => Ok(InputWake::Resumed),
                Err(_) => Err(mpsc::RecvTimeoutError::Disconnected),
            }
        }
        recv(events) -> event => {
            let first = event.map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
            let mut batch = Vec::with_capacity(limit.max(1).min(events.len().saturating_add(1)));
            if limit > 0 {
                batch.push(first);
                while batch.len() < limit {
                    match events.try_recv() {
                        Ok(event) => batch.push(event),
                        Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            }
            Ok(InputWake::Events(batch))
        }
        recv(timeout_rx) -> _ => Err(mpsc::RecvTimeoutError::Timeout),
    }
}

fn receive_prioritized_input_or_control(
    events: &mpsc::Receiver<Event>,
    focus_events: &mpsc::Receiver<Event>,
    controls: &mpsc::Receiver<InputControl>,
    timeout: Duration,
    ordinary_limit: usize,
) -> Result<InputWake, mpsc::RecvTimeoutError> {
    let timeout_rx = mpsc::after(timeout);
    crossbeam_channel::select_biased! {
        recv(controls) -> control => {
            match control {
                Ok(InputControl::Suspend { acknowledge }) => {
                    Ok(InputWake::Suspend { acknowledge })
                }
                Ok(InputControl::Resumed) => Ok(InputWake::Resumed),
                Err(_) => Err(mpsc::RecvTimeoutError::Disconnected),
            }
        }
        recv(focus_events) -> focus => {
            let first = focus.map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
            let mut batch = Vec::with_capacity(focus_events.len().saturating_add(1));
            batch.push(first);
            batch.extend(focus_events.try_iter());
            for _ in 0..ordinary_limit {
                match events.try_recv() {
                    Ok(event) => batch.push(event),
                    Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
                }
            }
            Ok(InputWake::Events(batch))
        }
        recv(events) -> event => {
            let first = event.map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
            let mut batch = Vec::with_capacity(ordinary_limit.max(1).min(events.len().saturating_add(1)));
            if ordinary_limit > 0 {
                batch.push(first);
                while batch.len() < ordinary_limit {
                    match events.try_recv() {
                        Ok(event) => batch.push(event),
                        Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            }
            Ok(InputWake::Events(batch))
        }
        recv(timeout_rx) -> _ => Err(mpsc::RecvTimeoutError::Timeout),
    }
}

fn poll_edit_highlight(state: &mut AppState, scheduler: &mut FrameScheduler) -> bool {
    let applied = state.poll_edit_highlight_results();
    if applied {
        scheduler.mark_dirty();
    }
    applied
}

fn edit_highlight_animation_active(state: &AppState) -> bool {
    state.edit_highlight_needs_tick()
}

fn mention_search_roots(config: &RunConfig, workspace_fallback: &Path) -> Vec<PathBuf> {
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
                    .unwrap_or_else(|| workspace_fallback.into()),
            ]
        })
}

fn syntax_workspace_root(config: &RunConfig) -> PathBuf {
    config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

fn configure_tui_syntax_state(
    state: &mut AppState,
    workspace_root: PathBuf,
    syntax_theme: crate::syntax_highlight::SyntaxTheme,
    syntax_color_level: crate::terminal_capabilities::TerminalColorLevel,
) {
    state.configure_syntax_highlighting(workspace_root, syntax_theme, syntax_color_level);
}

fn configure_and_preload_tui_state(
    state: &mut AppState,
    workspace_root: PathBuf,
    syntax_theme: crate::syntax_highlight::SyntaxTheme,
    syntax_color_level: crate::terminal_capabilities::TerminalColorLevel,
    messages: impl IntoIterator<Item = ChatMessage>,
) {
    configure_tui_syntax_state(state, workspace_root, syntax_theme, syntax_color_level);
    for message in messages {
        state.push_message(message);
    }
}

type InlineTerminal = Terminal<CapabilityBackend<CrosstermBackend<std::io::Stdout>>>;

fn clear_terminal_scrollback_with<T>(
    target: &mut T,
    mut move_home: impl FnMut(&mut T) -> io::Result<()>,
    mut clear_all: impl FnMut(&mut T) -> io::Result<()>,
    mut clear_purge: impl FnMut(&mut T) -> io::Result<()>,
    mut clear_frame: impl FnMut(&mut T) -> io::Result<()>,
) -> io::Result<()> {
    move_home(target)?;
    clear_all(target)?;
    clear_purge(target)?;
    clear_frame(target)
}

/// Erase the native scrollback and on-screen content. Used by the clear-screen shortcut so a
/// fresh session starts on a clean terminal instead of stacking under the old transcript.
fn clear_terminal_scrollback(terminal: &mut InlineTerminal) -> io::Result<()> {
    use crossterm::terminal::{Clear, ClearType};
    clear_terminal_scrollback_with(
        terminal,
        |terminal| {
            terminal
                .backend_mut()
                .inner_mut()
                .execute(crossterm::cursor::MoveTo(0, 0))?;
            Ok(())
        },
        |terminal| {
            terminal
                .backend_mut()
                .inner_mut()
                .execute(Clear(ClearType::All))?;
            Ok(())
        },
        |terminal| {
            terminal
                .backend_mut()
                .inner_mut()
                .execute(Clear(ClearType::Purge))?;
            Ok(())
        },
        Terminal::clear,
    )
}

#[cfg(test)]
fn run_manual_compaction_with_events(
    event_tx: &mpsc::Sender<TuiEvent>,
    compact: impl FnOnce() -> (usize, usize),
) {
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
fn spawn_hosted_tui_test_runtime(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
) -> TuiAgentRuntime {
    spawn_hosted_tui_test_runtime_with_background_capacity(
        config, preloaded, event_tx, action_rx, 8,
    )
}

#[cfg(test)]
fn spawn_hosted_tui_test_runtime_with_background_capacity(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
    background_capacity: usize,
) -> TuiAgentRuntime {
    let pending = bridge::PendingWorkflowNotifications::new();
    let registry = orca_mcp::initialize_registry(&[]);
    let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
    let agent_config = Arc::clone(&config);
    let agent_preloaded = Arc::clone(&preloaded);
    let agent_events = event_tx.clone();
    let agent_pending = pending.clone();
    let agent_registry = registry.clone();
    TuiAgentRuntime::spawn_hosted(
        action_rx,
        event_tx,
        background_capacity,
        controller,
        move |controller, commands, host| {
            hosted_tui_controller_loop(
                agent_config,
                agent_preloaded,
                agent_events,
                commands,
                controller,
                agent_pending,
                agent_registry,
                host,
            );
        },
    )
    .expect("hosted TUI test runtime")
}

#[cfg(test)]
fn run_hosted_tui_controller_for_test(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
    _cancel: CancelToken,
    _pending_workflow_notifications: bridge::PendingWorkflowNotifications,
) {
    let mut runtime = spawn_hosted_tui_test_runtime(config, preloaded, event_tx, action_rx);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !runtime.controller().is_shutdown() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    runtime.shutdown().expect("hosted TUI test shutdown");
}

fn now_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
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
    use crate::key_event_actions::handle_transcript_search_key;
    use crate::selection::{SelectionGranularity, SelectionPos, TranscriptSelection};
    use crate::slash_command_actions::handle_slash_command;
    use crate::types::{ApprovalOption, PendingTuiInput, SlashMenu, SlashMenuItem, SubMenu};
    use crate::types::{TuiInteractionKey, TuiInteractionKind, TuiInteractionResponse};
    use crate::workflow_notifications::drain_pending_workflow_notifications;

    fn production_app_source() -> &'static str {
        include_str!("app.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("production app source")
    }

    #[test]
    fn startup_captures_one_identity_for_presentation_and_diagnostics() {
        let production = production_app_source();
        assert_eq!(
            production
                .matches("qwertty::caps::identity_from_env(")
                .count(),
            1,
        );
        let identity = production.find("let terminal_identity =").unwrap();
        let presentation = production
            .find("TerminalPresentationProfile::from_identity(&terminal_identity)")
            .unwrap();
        let diagnostics = production.find("DiagnosticSnapshot::new(").unwrap();
        assert!(identity < presentation && identity < diagnostics);
    }

    #[test]
    fn production_diagnostics_use_effective_profile_without_reprobe() {
        let production = production_app_source();
        assert_eq!(production.matches("InputRuntime::start").count(), 1);
        assert!(!production.contains("probe_capabilities("));
        assert!(!production.contains("probe_background("));
    }

    #[test]
    fn startup_snapshot_projects_effective_profile_and_orca_home_location() {
        use crate::diagnostics::KeybindingsLocation;
        use crate::terminal_capabilities::{
            TerminalBackground, TerminalColorLevel, TerminalProfile,
        };

        let identity = qwertty::caps::identity_from_env(None, |key| match key {
            "TERM_PROGRAM" => Some("ghostty".to_string()),
            "TMUX" => Some("session".to_string()),
            _ => None,
        });
        let snapshot = diagnostic_snapshot_for_startup(
            &test_config(HistoryMode::Disabled),
            &identity,
            TerminalProfile {
                background: TerminalBackground::Dark,
                color_level: TerminalColorLevel::Ansi256,
            },
            TerminalPresentationProfile::from_identity(&identity),
            KeybindingsLocation::OrcaHome,
        );

        assert_eq!(snapshot.terminal_program(), "Ghostty");
        assert_eq!(snapshot.multiplexers(), ["tmux"]);
        assert_eq!(snapshot.color_level(), TerminalColorLevel::Ansi256);
        assert_eq!(snapshot.requested_theme(), ThemeName::Dark);
        assert_eq!(snapshot.resolved_theme(), ThemeName::Dark);
        assert_eq!(
            snapshot.keybindings_location(),
            KeybindingsLocation::OrcaHome,
        );
    }

    #[test]
    fn doctor_vim_projection_tracks_real_mode_transitions() {
        use crate::vim::VimMode;

        let theme = Theme::named(ThemeName::Dark);
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );

        let disabled = VimState::new(false);
        state.sync_vim_mode(&disabled);
        assert_eq!(state.vim_mode, None);

        let mut vim = VimState::new(true);
        let mut textarea = make_textarea_with_text("word", &vim, &theme);
        state.sync_vim_mode(&vim);
        assert_eq!(state.vim_mode, Some(VimMode::Normal));

        for (key, expected) in [
            (KeyCode::Char('i'), VimMode::Insert),
            (KeyCode::Esc, VimMode::Normal),
            (KeyCode::Char('v'), VimMode::Visual),
            (KeyCode::Esc, VimMode::Normal),
        ] {
            let event = Event::Key(KeyEvent::new(key, KeyModifiers::NONE));
            vim.handle(Input::from(event), &mut textarea, &theme);
            state.sync_vim_mode(&vim);
            assert_eq!(state.vim_mode, Some(expected));
        }
    }

    #[test]
    fn vim_projection_sync_is_owned_by_app_not_leaf_handlers() {
        let app = production_app_source();
        assert!(app.contains("state.sync_vim_mode(&vim_state)"));
        for source in [
            include_str!("slash_command_actions.rs"),
            include_str!("idle_key_actions.rs"),
            include_str!("queued_input_actions.rs"),
            include_str!("mention_menu_actions.rs"),
            include_str!("slash_menu_actions.rs"),
        ] {
            assert!(!source.contains("sync_vim_mode("));
        }
    }

    #[test]
    fn successful_draw_records_once_and_failed_draw_records_nothing() {
        use std::collections::VecDeque;

        let start = Instant::now();
        let mut metrics = crate::diagnostics::FrameMetrics::default();
        let mut times = VecDeque::from([
            start,
            start + Duration::from_millis(3),
            start + Duration::from_millis(10),
            start + Duration::from_millis(11),
        ]);
        let (_, started, completed) =
            measure_successful_draw(|| times.pop_front().unwrap(), || Ok(())).unwrap();
        metrics.record_successful_draw(started, completed);
        measure_successful_draw(
            || times.pop_front().unwrap(),
            || Err::<(), _>(io::Error::other("draw failed")),
        )
        .unwrap_err();

        assert_eq!(
            metrics
                .snapshot(start + Duration::from_millis(11))
                .total_draws,
            1,
        );
    }

    #[test]
    fn production_draws_and_iteration_counts_are_recorded_once() {
        let source = production_app_source();
        assert_eq!(source.matches("measure_successful_draw(").count(), 2);
        assert_eq!(source.matches(".record_successful_draw(").count(), 2);
        assert_eq!(source.matches(".record_iteration(").count(), 1);
    }

    #[test]
    fn doctor_suspend_resets_rolling_before_acknowledgement() {
        let source = production_app_source();
        let suspend = source.find("InputWake::Suspend").unwrap();
        let reset = source[suspend..]
            .find("frame_metrics.reset_rolling()")
            .unwrap();
        let acknowledge = source[suspend..].find("acknowledge.send").unwrap();
        assert!(reset < acknowledge);
    }

    #[test]
    fn fps_hud_controls_animation_without_changing_frame_interval() {
        let source = production_app_source();
        let animation = source.find("let animation_active =").unwrap();
        let receive = source.find("receive_prioritized_input_or_control").unwrap();
        assert!(source[animation..receive].contains("state.fps_hud_enabled"));
        assert_eq!(source.matches("const FRAME_INTERVAL:").count(), 1);
        assert!(source.contains("Duration::from_millis(16)"));
    }

    #[test]
    fn doctor_keybindings_projection_is_wired_to_reload_outcomes() {
        let source = include_str!("app.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("production app source");
        assert!(source.contains("KeybindingsDiagnostic::built_ins("));
        assert!(source.contains(".applied_custom("));
        assert!(source.contains(".restored_built_ins("));
        assert!(source.contains(".rejected("));
        assert!(source.contains("keybindings_location()"));
    }

    #[test]
    fn doctor_auth_projection_is_initialized_and_updated_from_config_facts() {
        let app = include_str!("app.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("production app source");
        let setup = include_str!("setup_actions.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("production setup source");
        assert!(app.contains("state.auth_configured = config.api_key.is_some();"));
        assert!(setup.contains("state.auth_configured = true;"));
    }

    #[test]
    fn keybinding_owner_tracks_status_modal_panel_and_vim_mode() {
        let (tx, _rx) = mpsc::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut vim = VimState::new(true);

        let idle = input_owner_fingerprint(&state, &vim);
        assert_eq!(idle.context, crate::shortcuts::ShortcutContext::Idle);
        assert_eq!(idle.modal, crate::keybindings::ModalOwner::None);
        assert_eq!(idle.vim_mode, Some(crate::vim::VimMode::Normal));

        state.show_shortcuts = true;
        assert_eq!(
            input_owner_fingerprint(&state, &vim).modal,
            crate::keybindings::ModalOwner::Shortcuts,
        );
        state.show_shortcuts = false;
        state.open_transcript_search();
        assert_eq!(
            input_owner_fingerprint(&state, &vim).modal,
            crate::keybindings::ModalOwner::TranscriptSearch,
        );
        state.close_transcript_search();
        state.set_status(AppStatus::WaitingApproval);
        assert_eq!(
            input_owner_fingerprint(&state, &vim).context,
            crate::shortcuts::ShortcutContext::Approval,
        );
        assert_eq!(
            input_owner_fingerprint(&state, &vim).modal,
            crate::keybindings::ModalOwner::Approval,
        );
        vim.mode = crate::vim::VimMode::Insert;
        assert_eq!(
            input_owner_fingerprint(&state, &vim).vim_mode,
            Some(crate::vim::VimMode::Insert),
        );
    }

    #[test]
    fn chord_deadline_caps_frame_poll_timeout() {
        let now = Instant::now();
        assert_eq!(
            keybinding_poll_timeout(
                Duration::from_millis(16),
                now,
                Some(now + Duration::from_millis(5)),
            ),
            Duration::from_millis(5),
        );
        assert_eq!(
            keybinding_poll_timeout(Duration::from_millis(16), now, Some(now)),
            Duration::ZERO,
        );
        assert_eq!(
            keybinding_poll_timeout(Duration::from_millis(16), now, None),
            Duration::from_millis(16),
        );
    }

    #[test]
    fn shared_global_and_idle_prefixes_are_both_reachable_through_app_routing() {
        let keymap = crate::keybindings::parse_keymap(
            br#"{
                "version": 1,
                "bindings": {
                    "global.open-transcript-search": ["ctrl+x ctrl+f"],
                    "idle.submit": ["ctrl+x ctrl+s"]
                }
            }"#,
        )
        .unwrap();
        let now = Instant::now();
        let operation = crate::test_support::TestOperationInterrupt::default();
        let theme = Theme::named(ThemeName::Dark);

        let (search_tx, _search_rx) = mpsc::unbounded();
        let mut search_state = AppState::new(
            search_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut search_config = test_config(HistoryMode::Disabled);
        let search_shared = Arc::new(Mutex::new(search_config.clone()));
        let mut search_vim = VimState::new(false);
        let mut search_runtime = KeymapRuntime::new(Arc::clone(&keymap));
        let owner = input_owner_fingerprint(&search_state, &search_vim);
        for (offset, character) in [(0, 'x'), (1, 'f')] {
            let key = KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL);
            assert!(matches!(
                handle_key_event_preflight_dynamic(
                    key,
                    now + Duration::from_millis(offset),
                    owner,
                    &mut search_runtime,
                    &mut search_state,
                    &mut search_config,
                    &search_shared,
                    &search_tx,
                    &operation,
                    &mut search_vim,
                    || Ok(()),
                )
                .unwrap(),
                DynamicKeyEventFlow::Continue,
            ));
        }
        assert!(search_state.transcript_search.open);

        let (submit_tx, submit_rx) = mpsc::unbounded();
        let mut submit_state = AppState::new(
            submit_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut submit_config = test_config(HistoryMode::Disabled);
        let submit_shared = Arc::new(Mutex::new(submit_config.clone()));
        let mut submit_vim = VimState::new(false);
        let mut submit_runtime = KeymapRuntime::new(keymap);
        let mut textarea = make_textarea_with_text("send me", &submit_vim, &theme);
        let preloaded = Arc::new(Mutex::new(None));
        let owner = input_owner_fingerprint(&submit_state, &submit_vim);
        let prefix = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_key_event_preflight_dynamic(
                prefix,
                now,
                owner,
                &mut submit_runtime,
                &mut submit_state,
                &mut submit_config,
                &submit_shared,
                &submit_tx,
                &operation,
                &mut submit_vim,
                || Ok(()),
            )
            .unwrap(),
            DynamicKeyEventFlow::Continue,
        ));
        let suffix = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let DynamicKeyEventFlow::Context(invocation) = handle_key_event_preflight_dynamic(
            suffix,
            now + Duration::from_millis(1),
            owner,
            &mut submit_runtime,
            &mut submit_state,
            &mut submit_config,
            &submit_shared,
            &submit_tx,
            &operation,
            &mut submit_vim,
            || Ok(()),
        )
        .unwrap() else {
            panic!("idle chord must complete as a contextual invocation");
        };
        handle_status_key_dynamic(
            &Event::Key(suffix),
            &suffix,
            now + Duration::from_millis(1),
            owner,
            &mut submit_runtime,
            Some(invocation),
            &mut submit_state,
            &mut submit_config,
            &submit_shared,
            &submit_tx,
            &operation,
            &preloaded,
            &mut textarea,
            &mut submit_vim,
            &theme,
            None,
            || Ok(()),
        )
        .unwrap();
        assert!(matches!(
            submit_rx.try_recv(),
            Ok(UserAction::SubmitWithMentions { prompt, .. }) if prompt == "send me"
        ));
    }

    #[test]
    fn contextual_chord_mismatch_retries_current_key_exactly_once() {
        let keymap = crate::keybindings::parse_keymap(
            br#"{"version":1,"bindings":{"idle.submit":["ctrl+x ctrl+s"]}}"#,
        )
        .unwrap();
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_config(HistoryMode::Disabled);
        let shared = Arc::new(Mutex::new(config.clone()));
        let operation = crate::test_support::TestOperationInterrupt::default();
        let preloaded = Arc::new(Mutex::new(None));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("base", &vim, &theme);
        let mut runtime = KeymapRuntime::new(keymap);
        let owner = input_owner_fingerprint(&state, &vim);
        let now = Instant::now();

        let prefix = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_key_event_preflight_dynamic(
                prefix,
                now,
                owner,
                &mut runtime,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &operation,
                &mut vim,
                || Ok(()),
            )
            .unwrap(),
            DynamicKeyEventFlow::Unhandled,
        ));
        handle_status_key_dynamic(
            &Event::Key(prefix),
            &prefix,
            now,
            owner,
            &mut runtime,
            None,
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

        let mismatch = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE);
        assert!(matches!(
            handle_key_event_preflight_dynamic(
                mismatch,
                now + Duration::from_millis(1),
                owner,
                &mut runtime,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &operation,
                &mut vim,
                || Ok(()),
            )
            .unwrap(),
            DynamicKeyEventFlow::Unhandled,
        ));
        handle_status_key_dynamic(
            &Event::Key(mismatch),
            &mismatch,
            now + Duration::from_millis(1),
            owner,
            &mut runtime,
            None,
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

        assert_eq!(textarea_text(&textarea), "basez");
    }
    use crate::workflow_notifications::{
        is_workflow_notification_turn_boundary, queue_workflow_terminal_notification,
        remove_pending_workflow_notification_by_id, submit_pending_workflow_notification,
    };
    use crate::workflow_panel_actions::handle_workflows_panel_key;
    use orca_core::config::{
        ModelRuntimeConfig, OutputFormat, ProviderKind, ThemeName, ToolConfig,
        VimInsertEscapeSequence, WorkflowConfig,
    };
    use tempfile::tempdir;

    fn vim_insert_input(character: char) -> tui_textarea::Input {
        tui_textarea::Input {
            key: tui_textarea::Key::Char(character),
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    fn inserted_source_line<'a>(
        lines: &'a [ratatui::text::Line<'static>],
        source: &str,
    ) -> &'a ratatui::text::Line<'static> {
        lines
            .iter()
            .find(|line| {
                line.to_string().contains(source)
                    && line
                        .spans
                        .first()
                        .is_some_and(|span| span.content.ends_with("+ "))
            })
            .unwrap_or_else(|| panic!("inserted source line containing {source:?}"))
    }

    #[test]
    fn receive_input_batch_waits_drains_and_caps() {
        let (sender, receiver) = mpsc::bounded(128);
        for character in 'a'..='z' {
            sender
                .send(Event::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                )))
                .expect("receiver alive");
        }

        let first = receive_input_batch(&receiver, Duration::from_millis(10), 5)
            .expect("queued input should be received");
        assert_eq!(first.len(), 5);
        assert_eq!(receiver.len(), 21);

        let remaining = receive_input_batch(&receiver, Duration::from_millis(10), 64)
            .expect("remaining queued input should be received");
        assert_eq!(remaining.len(), 21);
        assert!(receiver.is_empty());

        sender
            .send(Event::Key(KeyEvent::new(
                KeyCode::Char('!'),
                KeyModifiers::NONE,
            )))
            .expect("receiver alive");
        assert_eq!(
            receive_input_batch(&receiver, Duration::from_millis(10), 0),
            Ok(Vec::new())
        );
        assert_eq!(receiver.len(), 1);
    }

    #[test]
    fn pending_insert_escape_preflight_precedes_shortcuts_only_after_sequence_started() {
        let theme = Theme::named(ThemeName::Dark);
        let sequence = VimInsertEscapeSequence::parse("jj").unwrap();
        let started = Instant::now();
        let mut vim = VimState::with_insert_escape(true, Some(sequence));
        vim.mode = crate::vim::VimMode::Insert;
        let mut textarea = TextArea::default();
        let mut state = test_state().0;
        let config = test_config(HistoryMode::Disabled);

        let first = Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(
            resolve_pending_insert_escape_before_routing(
                &first,
                started,
                &mut vim,
                &mut textarea,
                &mut state,
                &config,
                &theme,
            ),
            PendingInsertEscapeRouting::Continue,
        );
        vim.handle_at(vim_insert_input('j'), &mut textarea, &theme, started);

        let second = Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(
            resolve_pending_insert_escape_before_routing(
                &second,
                started + Duration::from_millis(1),
                &mut vim,
                &mut textarea,
                &mut state,
                &config,
                &theme,
            ),
            PendingInsertEscapeRouting::Consumed,
        );
        assert_eq!(vim.mode, crate::vim::VimMode::Normal);
        assert!(textarea.is_empty());
    }

    #[test]
    fn pending_insert_escape_flushes_before_submit_and_paste_ownership() {
        let theme = Theme::named(ThemeName::Dark);
        let started = Instant::now();
        let sequence = VimInsertEscapeSequence::parse("jj").unwrap();

        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_config(HistoryMode::Disabled);
        let shared = Arc::new(Mutex::new(config.clone()));
        let mut vim = VimState::with_insert_escape(true, Some(sequence.clone()));
        vim.mode = crate::vim::VimMode::Insert;
        let mut textarea = TextArea::default();
        vim.handle_at(vim_insert_input('j'), &mut textarea, &theme, started);

        let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            resolve_pending_insert_escape_before_routing(
                &enter,
                started + Duration::from_millis(1),
                &mut vim,
                &mut textarea,
                &mut state,
                &config,
                &theme,
            ),
            PendingInsertEscapeRouting::Continue,
        );
        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim,
            &theme,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
        ));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWithMentions { prompt, .. }) if prompt == "j"
        ));

        let mut paste_state = test_state().0;
        let paste_config = test_config(HistoryMode::Disabled);
        let mut paste_vim = VimState::with_insert_escape(true, Some(sequence));
        paste_vim.mode = crate::vim::VimMode::Insert;
        let mut paste_area = TextArea::default();
        paste_vim.handle_at(vim_insert_input('j'), &mut paste_area, &theme, started);
        assert!(flush_pending_insert_escape_before_non_key(
            &mut paste_vim,
            &mut paste_area,
            &mut paste_state,
            &paste_config,
        ));
        assert!(handle_paste_event(
            &Event::Paste("jj".to_string()),
            &mut paste_state,
            &paste_config,
            &mut paste_area,
        ));
        assert_eq!(textarea_text(&paste_area), "jjj");
    }

    #[test]
    fn pending_insert_escape_flushes_before_running_escape_interrupt() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        let mut config = test_config(HistoryMode::Disabled);
        config.vim_mode = true;
        config.vim_insert_escape = Some(VimInsertEscapeSequence::parse("jj").unwrap());
        let shared = Arc::new(Mutex::new(config.clone()));
        let operation = crate::test_support::TestOperationInterrupt::default();
        let preloaded = Arc::new(Mutex::new(None));
        let theme = Theme::named(ThemeName::Dark);
        let started = Instant::now();
        let mut vim = VimState::with_insert_escape(true, config.vim_insert_escape.clone());
        vim.mode = crate::vim::VimMode::Insert;
        let mut textarea = TextArea::default();
        vim.handle_at(vim_insert_input('j'), &mut textarea, &theme, started);
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let event = Event::Key(key);

        assert_eq!(
            resolve_pending_insert_escape_before_routing(
                &event,
                started + Duration::from_millis(1),
                &mut vim,
                &mut textarea,
                &mut state,
                &config,
                &theme,
            ),
            PendingInsertEscapeRouting::Continue,
        );
        handle_status_key(
            &event,
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

        assert_eq!(textarea_text(&textarea), "j");
        assert_eq!(state.status, AppStatus::Running);
        assert_eq!(operation.call_count(), 1);
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    }

    #[test]
    fn expired_insert_escape_flush_refreshes_input_state_once() {
        let theme = Theme::named(ThemeName::Dark);
        let config = test_config(HistoryMode::Disabled);
        let started = Instant::now();
        let mut vim =
            VimState::with_insert_escape(true, Some(VimInsertEscapeSequence::parse("jj").unwrap()));
        vim.mode = crate::vim::VimMode::Insert;
        let mut textarea = TextArea::default();
        let mut state = test_state().0;
        vim.handle_at(vim_insert_input('j'), &mut textarea, &theme, started);

        assert!(flush_expired_insert_escape(
            started + Duration::from_millis(501),
            &mut vim,
            &mut textarea,
            &mut state,
            &config,
        ));
        assert_eq!(textarea_text(&textarea), "j");
        assert!(!vim.has_pending_insert_escape_for_test());
    }

    #[test]
    fn receive_input_batch_reports_timeout_and_disconnect() {
        let (sender, receiver) = mpsc::bounded(1);
        assert_eq!(
            receive_input_batch(&receiver, Duration::from_millis(1), 64),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        drop(sender);
        assert_eq!(
            receive_input_batch(&receiver, Duration::from_millis(1), 64),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn receive_input_or_control_prioritizes_suspend_over_queued_keys() {
        let (event_tx, event_rx) = mpsc::bounded(1);
        event_tx
            .send(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("event receiver alive");
        let (control_tx, control_rx) = mpsc::bounded(1);
        let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
        control_tx
            .send(InputControl::Suspend { acknowledge })
            .expect("control receiver alive");

        let wake = receive_input_or_control(&event_rx, &control_rx, Duration::from_millis(10), 64)
            .expect("suspend control should win");
        let InputWake::Suspend { acknowledge } = wake else {
            panic!("expected suspend control");
        };
        acknowledge
            .send(())
            .expect("acknowledgement receiver alive");
        assert_eq!(
            acknowledged.blocking_recv(),
            Ok(()),
            "input owner receives the frame-loop acknowledgement"
        );
        assert_eq!(event_rx.len(), 1, "queued key waits until resume");
    }

    #[test]
    fn receive_input_or_control_prioritizes_focus_beyond_the_ordinary_input_cap() {
        for focus in [Event::FocusLost, Event::FocusGained] {
            let (event_tx, event_rx) = mpsc::bounded(128);
            for _ in 0..65 {
                event_tx
                    .send(Event::Key(KeyEvent::new(
                        KeyCode::Char('x'),
                        KeyModifiers::NONE,
                    )))
                    .expect("event receiver alive");
            }
            let (focus_tx, focus_rx) = mpsc::bounded(8);
            focus_tx.send(focus.clone()).expect("focus receiver alive");
            let (_control_tx, control_rx) = mpsc::bounded(1);

            let wake = receive_prioritized_input_or_control(
                &event_rx,
                &focus_rx,
                &control_rx,
                Duration::from_millis(10),
                64,
            )
            .expect("queued input should be received");
            let InputWake::Events(events) = wake else {
                panic!("expected input events");
            };

            let started = Instant::now();
            let mut scheduler = FrameScheduler::new(
                started,
                Duration::from_millis(16),
                Duration::from_millis(80),
            );
            scheduler.did_draw(started);
            let mut handled_focus = false;
            let mut handled_keys = 0;
            run_event_loop_iteration(
                &mut scheduler,
                events,
                std::iter::empty::<()>(),
                usize::MAX,
                0,
                || started,
                |event| {
                    if let IterationEvent::Input(event) = event {
                        match event {
                            Event::FocusLost | Event::FocusGained => handled_focus = true,
                            Event::Key(_) => handled_keys += 1,
                            _ => {}
                        }
                    }
                    Ok::<Option<i32>, ()>(None)
                },
            )
            .expect("prioritized iteration");

            assert!(
                handled_focus,
                "focus changes must bypass the ordinary input cap"
            );
            assert_eq!(handled_keys, 64);
            assert_eq!(event_rx.len(), 1, "ordinary overflow remains queued");

            let next = receive_prioritized_input_or_control(
                &event_rx,
                &focus_rx,
                &control_rx,
                Duration::ZERO,
                64,
            )
            .expect("queued overflow should be returned without waiting");
            let InputWake::Events(next) = next else {
                panic!("expected queued overflow");
            };
            assert!(matches!(next.as_slice(), [Event::Key(_)]));
            assert!(event_rx.is_empty());
        }
    }

    #[test]
    fn prioritized_focus_preserves_bounded_ordinary_input_backpressure() {
        let (event_tx, event_rx) = mpsc::bounded(128);
        let (focus_tx, focus_rx) = mpsc::bounded(8);
        let (_control_tx, control_rx) = mpsc::bounded(1);

        for _ in 0..3 {
            while event_tx.len() < 128 {
                event_tx
                    .send(Event::Key(KeyEvent::new(
                        KeyCode::Char('x'),
                        KeyModifiers::NONE,
                    )))
                    .expect("event receiver alive");
            }
            focus_tx
                .send(Event::FocusLost)
                .expect("focus receiver alive");
            let wake = receive_prioritized_input_or_control(
                &event_rx,
                &focus_rx,
                &control_rx,
                Duration::ZERO,
                64,
            )
            .expect("queued input should be received");
            assert!(matches!(wake, InputWake::Events(_)));
        }

        assert_eq!(event_rx.len(), 64);
        assert!(focus_rx.is_empty());
    }

    #[test]
    fn terminal_title_writes_before_initial_draw() {
        let mut calls = Vec::new();
        initialize_terminal_presentation(
            &mut calls,
            |calls| {
                calls.push("write-start");
                Ok(())
            },
            |calls| {
                calls.push("draw-start");
                Ok(())
            },
        )
        .expect("startup presentation");
        assert_eq!(calls, ["write-start", "draw-start"]);
    }

    #[test]
    fn presentation_resume_clears_invalidates_then_marks_dirty() {
        let mut calls = Vec::new();
        complete_presentation_resume(
            &mut calls,
            |calls| {
                calls.push("clear");
                Ok(())
            },
            |calls| calls.push("invalidate"),
            |calls| calls.push("dirty"),
        )
        .expect("resume presentation");
        assert_eq!(calls, ["clear", "invalidate", "dirty"]);

        let mut calls = Vec::new();
        let error = complete_presentation_resume(
            &mut calls,
            |_| Err(io::Error::other("clear failed")),
            |calls| calls.push("invalidate"),
            |calls| calls.push("dirty"),
        )
        .expect_err("clear failure should stop resume");
        assert_eq!(error.to_string(), "clear failed");
        assert!(calls.is_empty());
    }

    #[test]
    fn presentation_exit_resets_drops_then_finishes_input() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let reset_exit = std::rc::Rc::clone(&calls);
        let drop_exit = std::rc::Rc::clone(&calls);
        let finish_exit = std::rc::Rc::clone(&calls);
        finish_terminal_presentation(
            (),
            move |_| {
                reset_exit.borrow_mut().push("reset");
                Ok(())
            },
            move |_| drop_exit.borrow_mut().push("drop"),
            move || {
                finish_exit.borrow_mut().push("finish");
                Ok(())
            },
        )
        .expect("exit presentation");
        assert_eq!(*calls.borrow(), ["reset", "drop", "finish"]);
    }

    #[test]
    fn presentation_exit_cleanup_runs_after_body_error() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let body = std::rc::Rc::clone(&calls);
        let cleanup = std::rc::Rc::clone(&calls);

        let error = with_terminal_presentation_cleanup(
            (),
            move |_| {
                body.borrow_mut().push("body");
                Err::<i32, _>(io::Error::other("body failed"))
            },
            move |_| {
                cleanup.borrow_mut().push("cleanup");
                Ok(())
            },
        )
        .expect_err("body error should be preserved");

        assert_eq!(error.to_string(), "body failed");
        assert_eq!(*calls.borrow(), ["body", "cleanup"]);
    }

    #[test]
    fn terminal_input_ownership_is_single() {
        let production = include_str!("app.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("production source before tests");

        for forbidden in [
            "event::poll",
            "event::read",
            "EventStream",
            "enable_raw_mode",
            "EnterAlternateScreen",
            "EnableMouseCapture",
            "EnableBracketedPaste",
            "PushKeyboardEnhancementFlags",
            "TerminalCleanup",
        ] {
            assert!(
                !production.contains(forbidden),
                "production app must not own terminal input/mode operation {forbidden}"
            );
        }

        let start = production
            .find("InputRuntime::start")
            .expect("qwertty input starts");
        let terminal = production
            .find("Terminal::new")
            .expect("ratatui terminal is constructed");
        assert!(start < terminal);
        assert_eq!(production.matches("InputRuntime::start").count(), 1);
        assert_eq!(production.matches("Terminal::new").count(), 1);
    }

    #[test]
    fn non_composer_input_boundaries_cancel_pending_vim_commands() {
        let production = include_str!("app.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("production app source");

        assert!(
            production
                .matches("vim_state.cancel_pending_command()")
                .count()
                >= 4
        );
        assert!(production.contains(
            "&mut vim_state,\n                                        || clear_terminal_scrollback"
        ));
    }

    #[test]
    fn startup_captures_workspace_status_once_before_frame_loop() {
        let production = include_str!("app.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("production source before tests");
        assert_eq!(
            production
                .matches("workspace_status::snapshot(&workspace_root)")
                .count(),
            1
        );
        let snapshot = production
            .find("workspace_status::snapshot(&workspace_root)")
            .expect("workspace snapshot");
        let state = production
            .find("AppState::new(")
            .expect("app state construction");
        let terminal = production
            .find("Terminal::new")
            .expect("frame loop terminal");
        assert!(snapshot < state);
        assert!(state < terminal);
        assert!(!production[state..].contains("workspace_status::snapshot("));
    }

    #[test]
    fn focus_events_are_consumed_before_normal_input_handlers() {
        let production = include_str!("app.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("production source before tests");
        let focus = production
            .find("consume_focus_event(&ev")
            .expect("focus consumption");
        let paste = production
            .find("handle_paste_event(&ev")
            .expect("paste handling");
        let resize = production
            .find("handle_resize_event(&ev")
            .expect("resize handling");
        let key = production
            .find("let Event::Key(key) = &ev")
            .expect("key handling");

        assert!(focus < paste);
        assert!(focus < resize);
        assert!(focus < key);
    }

    #[test]
    fn synthetic_enter_uses_dynamic_status_routing() {
        let production = include_str!("app.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("production app source");
        let branch_start = production
            .find("MouseFlow::SyntheticEnter =>")
            .expect("synthetic enter branch");
        let branch = &production[branch_start..production[branch_start..]
            .find("\n                                        }\n                                    }")
            .map(|offset| branch_start + offset)
            .expect("synthetic enter branch end")];

        assert!(branch.contains("handle_status_key_dynamic("));
        assert!(!branch.contains("handle_status_key("));
    }

    #[test]
    fn clear_terminal_runs_move_all_purge_then_frame_clear() {
        let mut calls = Vec::new();

        clear_terminal_scrollback_with(
            &mut calls,
            |calls| {
                calls.push("MoveTo");
                Ok(())
            },
            |calls| {
                calls.push("All");
                Ok(())
            },
            |calls| {
                calls.push("Purge");
                Ok(())
            },
            |calls| {
                calls.push("FrameClear");
                Ok(())
            },
        )
        .expect("clear sequence should succeed");

        assert_eq!(calls, ["MoveTo", "All", "Purge", "FrameClear"]);
    }

    #[test]
    fn clear_terminal_preserves_each_stage_error_and_short_circuits() {
        let stages = ["MoveTo", "All", "Purge", "FrameClear"];
        let kinds = [
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::TimedOut,
        ];
        let messages = ["move failed", "all failed", "purge failed", "frame failed"];

        for failing_stage in 0..stages.len() {
            let mut calls = Vec::new();
            let result = clear_terminal_scrollback_with(
                &mut calls,
                |calls| {
                    calls.push("MoveTo");
                    if failing_stage == 0 {
                        Err(io::Error::new(kinds[0], messages[0]))
                    } else {
                        Ok(())
                    }
                },
                |calls| {
                    calls.push("All");
                    if failing_stage == 1 {
                        Err(io::Error::new(kinds[1], messages[1]))
                    } else {
                        Ok(())
                    }
                },
                |calls| {
                    calls.push("Purge");
                    if failing_stage == 2 {
                        Err(io::Error::new(kinds[2], messages[2]))
                    } else {
                        Ok(())
                    }
                },
                |calls| {
                    calls.push("FrameClear");
                    if failing_stage == 3 {
                        Err(io::Error::new(kinds[3], messages[3]))
                    } else {
                        Ok(())
                    }
                },
            );

            let error = result.expect_err("selected clear stage should fail");
            assert_eq!(error.kind(), kinds[failing_stage]);
            assert_eq!(error.to_string(), messages[failing_stage]);
            assert_eq!(calls, stages[..=failing_stage]);
        }
    }

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
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
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

    const POLL_EDIT_DIFF: &str = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

    fn state_with_pending_edit() -> (tempfile::TempDir, AppState) {
        let directory = tempdir().expect("edit workspace");
        std::fs::create_dir_all(directory.path().join("src")).expect("source directory");
        std::fs::write(directory.path().join("src/item.py"), "value = 2\n")
            .expect("post-edit file");
        let (mut state, _rx) = test_state();
        state.configure_syntax_highlighting(
            directory.path().to_path_buf(),
            crate::syntax_highlight::SyntaxTheme::OneHalfDark,
            crate::terminal_capabilities::TerminalColorLevel::TrueColor,
        );
        state.update(TuiEvent::ToolRequested {
            id: "edit-1".to_string(),
            name: "edit".to_string(),
            target: Some("src/item.py".to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "edit-1".to_string(),
            name: "edit".to_string(),
            status: "completed".to_string(),
            output: "edited src/item.py".to_string(),
            diff: Some(POLL_EDIT_DIFF.to_string()),
            kind: None,
        });
        assert!(state.edit_highlight_needs_tick());
        (directory, state)
    }

    fn ready_drain(
        runtime: &mut crate::edit_highlight_worker::EditHighlightRuntime,
    ) -> crate::edit_highlight_worker::DrainResults {
        use ratatui::style::{Color, Style};
        use ratatui::text::Span;

        let job = runtime.pending_job("edit-1").expect("pending edit");
        let styles = crate::diff_highlight::RefinedDiffStyles::from([(
            1,
            vec![Span::styled(
                "value = 2",
                Style::default().fg(Color::Magenta),
            )],
        )]);
        crate::edit_highlight_worker::DrainResults {
            results: vec![crate::edit_highlight_worker::EditHighlightResult {
                job,
                outcome: crate::edit_highlight_worker::EditHighlightOutcome::Ready {
                    styles: Arc::new(styles),
                },
            }],
            disconnected: false,
        }
    }

    fn failed_drain(
        runtime: &mut crate::edit_highlight_worker::EditHighlightRuntime,
    ) -> crate::edit_highlight_worker::DrainResults {
        let job = runtime.pending_job("edit-1").expect("pending edit");
        crate::edit_highlight_worker::DrainResults {
            results: vec![crate::edit_highlight_worker::EditHighlightResult {
                job,
                outcome: crate::edit_highlight_worker::EditHighlightOutcome::Failed,
            }],
            disconnected: false,
        }
    }

    fn stale_drain(
        runtime: &mut crate::edit_highlight_worker::EditHighlightRuntime,
    ) -> crate::edit_highlight_worker::DrainResults {
        let mut job = runtime.pending_job("edit-1").expect("pending edit");
        job.message_revision = job.message_revision.saturating_add(1);
        crate::edit_highlight_worker::DrainResults {
            results: vec![crate::edit_highlight_worker::EditHighlightResult {
                job,
                outcome: crate::edit_highlight_worker::EditHighlightOutcome::Failed,
            }],
            disconnected: false,
        }
    }

    fn disconnected_drain(
        _runtime: &mut crate::edit_highlight_worker::EditHighlightRuntime,
    ) -> crate::edit_highlight_worker::DrainResults {
        crate::edit_highlight_worker::DrainResults {
            results: Vec::new(),
            disconnected: true,
        }
    }

    #[test]
    fn ready_edit_highlight_poll_marks_dirty_and_clears_pending_animation() {
        let (_directory, mut state) = state_with_pending_edit();
        let started = Instant::now();
        let mut scheduler = FrameScheduler::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        scheduler.did_draw(started);
        state.set_edit_highlight_drain_for_test(Some(ready_drain));

        assert!(edit_highlight_animation_active(&state));
        assert!(poll_edit_highlight(&mut state, &mut scheduler));
        assert!(!edit_highlight_animation_active(&state));
        assert!(!scheduler.should_draw(started + Duration::from_millis(15)));
        let draw_at = started + Duration::from_millis(16);
        assert!(scheduler.should_draw(draw_at));
        scheduler.did_draw(draw_at);
        assert!(!scheduler.should_draw(draw_at + Duration::from_secs(1)));
    }

    #[test]
    fn idle_ready_poll_schedules_actual_render_with_refined_styles_once() {
        let (_directory, mut state) = state_with_pending_edit();
        state.push_message(ChatMessage::System("stable".to_string()));
        assert_eq!(state.status, AppStatus::Idle);
        let theme = Theme::named(ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20))
            .expect("test backend");

        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .expect("cold render");
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 2);
        let cold = state
            .transcript_render_cache
            .viewport(0, usize::MAX, usize::MAX);
        let cold_insert = inserted_source_line(&cold.lines, "value = 2");
        assert!(
            cold_insert
                .spans
                .iter()
                .all(|span| span.style.fg != Some(ratatui::style::Color::Magenta))
        );

        let revisions_before = state.message_revisions.clone();
        let started = Instant::now();
        let mut scheduler = FrameScheduler::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        scheduler.did_draw(started);
        state.set_edit_highlight_drain_for_test(Some(ready_drain));

        assert!(edit_highlight_animation_active(&state));
        assert!(poll_edit_highlight(&mut state, &mut scheduler));
        assert_ne!(state.message_revisions[0], revisions_before[0]);
        assert_eq!(state.message_revisions[1], revisions_before[1]);
        assert_eq!(state.pending_edit_highlight_count_for_test(), 0);
        assert!(!edit_highlight_animation_active(&state));

        let draw_at = started + Duration::from_millis(16);
        assert!(scheduler.should_draw(draw_at));
        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .expect("refined render");
        scheduler.did_draw(draw_at);
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 1);
        let warm = state
            .transcript_render_cache
            .viewport(0, usize::MAX, usize::MAX);
        let warm_insert = inserted_source_line(&warm.lines, "value = 2");
        assert_eq!(
            warm_insert
                .spans
                .iter()
                .filter(|span| { span.style.fg == Some(ratatui::style::Color::Magenta) })
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "value = 2"
        );

        let revisions_after = state.message_revisions.clone();
        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .expect("steady render");
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 0);
        assert_eq!(state.message_revisions, revisions_after);
        assert!(!scheduler.should_draw(draw_at + Duration::from_secs(1)));
    }

    #[test]
    fn failed_and_stale_edit_highlight_polls_do_not_mark_dirty() {
        for (drain, remains_pending) in [
            (
                failed_drain
                    as fn(
                        &mut crate::edit_highlight_worker::EditHighlightRuntime,
                    ) -> crate::edit_highlight_worker::DrainResults,
                false,
            ),
            (stale_drain, true),
        ] {
            let (_directory, mut state) = state_with_pending_edit();
            let started = Instant::now();
            let mut scheduler = FrameScheduler::new(
                started,
                Duration::from_millis(16),
                Duration::from_millis(80),
            );
            scheduler.did_draw(started);
            state.set_edit_highlight_drain_for_test(Some(drain));

            assert!(!poll_edit_highlight(&mut state, &mut scheduler));
            assert_eq!(edit_highlight_animation_active(&state), remains_pending);
            assert!(!scheduler.should_draw(started + Duration::from_millis(16)));
        }
    }

    #[test]
    fn disconnected_edit_highlight_poll_drops_runtime_and_stops_pending_tick() {
        let (_directory, mut state) = state_with_pending_edit();
        let started = Instant::now();
        let mut scheduler = FrameScheduler::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        scheduler.did_draw(started);
        state.set_edit_highlight_drain_for_test(Some(disconnected_drain));

        assert!(edit_highlight_animation_active(&state));
        assert!(!poll_edit_highlight(&mut state, &mut scheduler));
        assert!(!state.edit_highlight_runtime_started_for_test());
        assert!(!edit_highlight_animation_active(&state));
        assert!(!scheduler.should_draw(started + Duration::from_millis(16)));
    }

    #[test]
    fn syntax_workspace_root_preserves_real_configured_path() {
        let directory = tempdir().expect("syntax workspace");
        let mut config = test_config(HistoryMode::Disabled);
        config.cwd = Some(directory.path().to_path_buf());

        assert_eq!(
            syntax_workspace_root(&config),
            directory.path().to_path_buf()
        );
    }

    #[test]
    fn mention_search_roots_reuse_captured_workspace_fallback() {
        let directory = tempdir().expect("captured mention workspace");
        let mut config = test_config(HistoryMode::Disabled);
        config.cwd = None;

        assert_eq!(
            mention_search_roots(&config, directory.path()),
            vec![directory.path().to_path_buf()]
        );
    }

    #[test]
    fn startup_configures_exact_workspace_before_replay_without_starting_runtime() {
        let directory = tempdir().expect("startup syntax workspace");
        let theme = Theme::named(ThemeName::Light);
        let (mut state, _rx) = test_state();
        let historical = ChatMessage::ToolCall {
            id: "historical-edit".to_string(),
            name: "edit".to_string(),
            target: Some("src/item.py".to_string()),
            status: "completed".to_string(),
            output: None,
            diff: Some(
                "--- a/src/item.py\n+++ b/src/item.py\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
            ),
            kind: None,
            expanded: false,
        };

        configure_and_preload_tui_state(
            &mut state,
            directory.path().to_path_buf(),
            theme.syntax_theme,
            theme.color_level,
            [historical],
        );

        assert_eq!(
            state.syntax_workspace_root_for_test(),
            Some(directory.path())
        );
        assert_eq!(
            state.syntax_theme_for_test(),
            crate::syntax_highlight::SyntaxTheme::OneHalfLight
        );
        assert_eq!(
            state.syntax_color_level_for_test(),
            crate::terminal_capabilities::TerminalColorLevel::TrueColor
        );
        assert_eq!(state.messages.len(), 1);
        assert!(!state.edit_highlight_runtime_started_for_test());
        assert_eq!(state.pending_edit_highlight_count_for_test(), 0);
    }

    #[test]
    fn startup_configuration_reuses_captured_workspace_after_cwd_changes() {
        struct CurrentDirGuard(PathBuf);

        impl Drop for CurrentDirGuard {
            fn drop(&mut self) {
                std::env::set_current_dir(&self.0).expect("restore current directory");
            }
        }

        let _lock = crate::test_support::lock_process_env();
        let original = std::env::current_dir().expect("original current directory");
        let workspace_a = tempdir().expect("workspace A");
        let workspace_b = tempdir().expect("workspace B");
        let _restore = CurrentDirGuard(original);
        std::env::set_current_dir(workspace_a.path()).expect("set workspace A");
        let mut config = test_config(HistoryMode::Disabled);
        config.cwd = None;
        let captured_workspace = syntax_workspace_root(&config);
        std::env::set_current_dir(workspace_b.path()).expect("set workspace B");
        let theme = Theme::named(ThemeName::Catppuccin);
        let (mut state, _rx) = test_state();

        configure_tui_syntax_state(
            &mut state,
            captured_workspace.clone(),
            theme.syntax_theme,
            theme.color_level,
        );

        assert_eq!(
            state.syntax_workspace_root_for_test(),
            Some(captured_workspace.as_path())
        );
        assert_eq!(
            state.syntax_theme_for_test(),
            crate::syntax_highlight::SyntaxTheme::CatppuccinMocha
        );
    }

    #[test]
    fn syntax_workspace_root_uses_current_dir_when_config_cwd_is_none() {
        struct CurrentDirGuard(PathBuf);

        impl Drop for CurrentDirGuard {
            fn drop(&mut self) {
                std::env::set_current_dir(&self.0).expect("restore current directory");
            }
        }

        let _lock = crate::test_support::lock_process_env();
        let original = std::env::current_dir().expect("original current directory");
        let directory = tempdir().expect("fallback syntax workspace");
        let _restore = CurrentDirGuard(original);
        std::env::set_current_dir(directory.path()).expect("set fallback current directory");
        let mut config = test_config(HistoryMode::Disabled);
        config.cwd = None;
        let theme = Theme::named(ThemeName::Dark);
        let (mut state, _rx) = test_state();
        let expected_workspace = syntax_workspace_root(&config);

        assert_eq!(
            expected_workspace,
            directory
                .path()
                .canonicalize()
                .expect("canonical fallback workspace")
        );
        configure_tui_syntax_state(
            &mut state,
            expected_workspace.clone(),
            theme.syntax_theme,
            theme.color_level,
        );
        assert_eq!(
            state.syntax_workspace_root_for_test(),
            Some(expected_workspace.as_path())
        );
    }

    fn test_pending_workflow_notifications() -> bridge::PendingWorkflowNotifications {
        bridge::PendingWorkflowNotifications::new()
    }

    #[test]
    fn hosted_tui_saved_workflow_routes_through_runtime_host() {
        if !orca_runtime::workflow::host::WorkflowHost::node_available() {
            return;
        }
        with_orca_home(|home| {
            let temp = tempdir().expect("workflow workspace");
            let workflow_dir = temp.path().join(".orca").join("workflows");
            std::fs::create_dir_all(&workflow_dir).expect("workflow directory");
            std::fs::write(
            workflow_dir.join("runtime-owned.js"),
            "export const meta = { name: 'runtime-owned', description: 'Runtime host test', phases: ['main'] };\nexport default await phase('main', async () => agent('inspect repo'));",
        )
        .expect("saved workflow");
            orca_core::config::folder_trust::set_trust_with_config_dir(
                temp.path(),
                home,
                orca_core::config::folder_trust::TrustLevel::Trusted,
            )
            .expect("trust workflow workspace");

            let mut config = test_config(HistoryMode::Disabled);
            config.cwd = Some(temp.path().to_path_buf());
            config.output_format = OutputFormat::Jsonl;
            config.approval_mode = ApprovalMode::FullAuto;
            let config = Arc::new(Mutex::new(config));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                move || {
                    run_hosted_tui_controller_for_test(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        CancelToken::new(),
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::RunWorkflow {
                    name: "runtime-owned".to_string(),
                    args: None,
                })
                .expect("run saved workflow action");
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut events = Vec::new();
            while Instant::now() < deadline
                && !events
                    .iter()
                    .any(|event| matches!(event, TuiEvent::WorkflowNotification { .. }))
            {
                if let Ok(event) = event_rx.recv_timeout(Duration::from_millis(50)) {
                    events.push(event);
                }
            }
            assert!(
            events
                .iter()
                .any(|event| matches!(event, TuiEvent::ToolCompleted { name, status, .. } if name == "Workflow" && status == "completed")),
            "saved workflow should publish a typed tool completion"
        );
            assert!(
            events
                .iter()
                .any(|event| matches!(event, TuiEvent::WorkflowNotification { status, .. } if status == "completed")),
            "saved workflow should publish a terminal notification"
        );
            action_tx
                .send(UserAction::Cancel)
                .expect("stop TUI test loop");
            handle.join().expect("hosted TUI test loop joined");
        });
    }

    fn interaction_key(kind: TuiInteractionKind, id: &str) -> TuiInteractionKey {
        TuiInteractionKey::new(
            orca_core::cancel::OperationIdAllocator::new().allocate(),
            id,
            kind,
        )
    }

    #[test]
    fn user_submission_error_emits_rejection_terminal() {
        let (event_tx, event_rx) = mpsc::unbounded();

        send_submission_error(
            &event_tx,
            None,
            Some("review @gone.txt"),
            "bound file is no longer available".to_string(),
        );

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::SubmissionRejected {
                prompt, message, ..
            })
                if prompt == "review @gone.txt"
                    && message == "bound file is no longer available"
        ));
    }

    #[test]
    fn stale_bound_file_preparation_emits_submission_rejected() {
        with_orca_home(|_| {
            let root = tempdir().expect("workspace root");
            let root_path = root
                .path()
                .canonicalize()
                .expect("canonical workspace root");
            let mut config = test_config(HistoryMode::Disabled);
            config.cwd = Some(root_path.clone());
            config.runtime_workspace_roots = Some(vec![root_path.clone()]);
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
            let mut harness = HostedTuiHarness::start(config, None);

            harness.send(UserAction::SubmitWithMentions {
                prompt: prompt.to_string(),
                bindings,
            });

            let rejection =
                harness.recv_until(|event| matches!(event, TuiEvent::SubmissionRejected { .. }));
            assert!(matches!(
                rejection,
                TuiEvent::SubmissionRejected {
                    prompt, message, ..
                }
                    if prompt == "review @gone.txt"
                        && message.contains("failed to resolve bound @gone.txt")
            ));
            harness.shutdown();
        });
    }

    #[test]
    fn queued_stale_bound_file_rejection_preserves_queued_identity() {
        with_orca_home(|_| {
            let root = tempdir().expect("workspace root");
            let root_path = root
                .path()
                .canonicalize()
                .expect("canonical workspace root");
            let mut config = test_config(HistoryMode::Disabled);
            config.cwd = Some(root_path.clone());
            config.runtime_workspace_roots = Some(vec![root_path.clone()]);
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
            let mut harness = HostedTuiHarness::start(config, None);

            harness.send(UserAction::SubmitQueued {
                id: 42,
                prompt: prompt.to_string(),
                bindings,
            });

            let rejection =
                harness.recv_until(|event| matches!(event, TuiEvent::SubmissionRejected { .. }));
            assert!(matches!(
                rejection,
                TuiEvent::SubmissionRejected {
                    queued_id: Some(42),
                    prompt,
                    message,
                } if prompt == "review @gone.txt"
                    && message.contains("failed to resolve bound @gone.txt")
            ));
            harness.shutdown();
        });
    }

    #[test]
    fn workflow_submission_error_remains_generic() {
        let (event_tx, event_rx) = mpsc::unbounded();

        send_submission_error(&event_tx, None, None, "workflow failed".to_string());

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
        let operation = crate::test_support::TestOperationInterrupt::default();
        let mut vim = VimState::new(false);

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
            &operation,
            &mut vim,
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
            &operation,
            &mut vim,
            || Ok(()),
        )
        .expect("preflight");
        assert!(matches!(flow, KeyEventFlow::Unhandled));
        assert_eq!(operation.call_count(), 0);
    }

    #[test]
    fn manual_compaction_emits_started_before_running_summary_work() {
        let (event_tx, event_rx) = mpsc::unbounded();

        run_manual_compaction_with_events(&event_tx, || {
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
        let previous = crate::test_support::HostedOperationHarness::start();
        previous.controller().interrupt_current();
        assert!(previous.cancel_token().is_cancelled());
        drop(previous);
        let current = crate::test_support::HostedOperationHarness::start();

        run_manual_compaction_with_events(&event_tx, || {
            assert!(
                !current.cancel_token().is_cancelled(),
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
            interaction: None,
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
            key: interaction_key(TuiInteractionKind::Approval, "approval-foreground"),
            tool: "bash".to_string(),
            target: Some("cargo test".to_string()),
            preview: None,
        });

        resolve_approval_option(&mut state, &action_tx, ApprovalOption::Once);

        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::RespondToInteraction {
                key,
                response: TuiInteractionResponse::Approval(true),
            }) if key.request_id == "approval-foreground"
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
                    run_hosted_tui_controller_for_test(
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
            next_event_seq: 0,
            semantic_events: Vec::new(),
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

    struct HostedTuiHarness {
        action_tx: mpsc::Sender<UserAction>,
        event_rx: mpsc::Receiver<TuiEvent>,
        runtime: TuiAgentRuntime,
        config: Arc<Mutex<RunConfig>>,
        preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    }

    impl HostedTuiHarness {
        fn start(config: RunConfig, preloaded: Option<history::SessionTranscript>) -> Self {
            Self::start_with_background_capacity(config, preloaded, 8)
        }

        fn start_with_background_capacity(
            config: RunConfig,
            preloaded: Option<history::SessionTranscript>,
            background_capacity: usize,
        ) -> Self {
            let config = Arc::new(Mutex::new(config));
            let preloaded = Arc::new(Mutex::new(preloaded));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let runtime = spawn_hosted_tui_test_runtime_with_background_capacity(
                Arc::clone(&config),
                Arc::clone(&preloaded),
                event_tx,
                action_rx,
                background_capacity,
            );
            Self {
                action_tx,
                event_rx,
                runtime,
                config,
                preloaded,
            }
        }

        fn send(&self, action: UserAction) {
            self.action_tx.send(action).expect("hosted TUI action");
        }

        fn recv_until(&self, mut predicate: impl FnMut(&TuiEvent) -> bool) -> TuiEvent {
            loop {
                let event = self
                    .event_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("hosted TUI event");
                if predicate(&event) {
                    return event;
                }
            }
        }

        fn shutdown(&mut self) {
            self.runtime.shutdown().expect("hosted TUI shutdown");
        }
    }

    #[test]
    fn hosted_tui_submit_clears_actor_operation_before_terminal_ui_event() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let pending = test_pending_workflow_notifications();
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let registry = orca_mcp::initialize_registry(&[]);
            let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
            let agent_config = Arc::clone(&config);
            let agent_preloaded = Arc::clone(&preloaded);
            let agent_events = event_tx.clone();
            let agent_pending = pending.clone();
            let agent_registry = registry.clone();
            let mut runtime = TuiAgentRuntime::spawn_hosted(
                action_rx,
                event_tx,
                8,
                controller,
                move |controller, commands, host| {
                    hosted_tui_controller_loop(
                        agent_config,
                        agent_preloaded,
                        agent_events,
                        commands,
                        controller,
                        agent_pending,
                        agent_registry,
                        host,
                    );
                },
            )
            .expect("hosted TUI runtime");

            action_tx
                .send(UserAction::Submit("hello from hosted TUI".to_string()))
                .unwrap();
            loop {
                if let TuiEvent::SessionCompleted { status } =
                    event_rx.recv_timeout(Duration::from_secs(10)).unwrap()
                {
                    assert_eq!(status, "success");
                    assert_eq!(runtime.controller().current_id(), None);
                    break;
                }
            }
            action_tx.send(UserAction::Compact).unwrap();
            let mut saw_compaction_start = false;
            loop {
                match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    TuiEvent::CompactionStarted => saw_compaction_start = true,
                    TuiEvent::Compacted { .. } => {
                        assert!(saw_compaction_start);
                        assert_eq!(runtime.controller().current_id(), None);
                        break;
                    }
                    _ => {}
                }
            }
            runtime.shutdown().expect("hosted runtime shutdown");
        });
    }

    #[test]
    fn hosted_tui_foreground_turn_uses_canonical_verifier_terminal() {
        with_orca_home(|_| {
            let mut config = test_config(HistoryMode::Record);
            config.verifier = Some("false".to_string());
            let mut harness = HostedTuiHarness::start(config, None);

            harness.send(UserAction::Submit("verify canonical TUI turn".to_string()));
            let terminal =
                harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));

            assert!(matches!(
                terminal,
                TuiEvent::SessionCompleted { status } if status == "verification_failed"
            ));
            harness.shutdown();
        });
    }

    #[test]
    fn hosted_tui_background_handoff_failure_publishes_terminal_after_operation_join() {
        with_orca_home(|_| {
            let mut harness = HostedTuiHarness::start_with_background_capacity(
                test_config(HistoryMode::Record),
                None,
                0,
            );
            harness.send(UserAction::Submit("mock_stream_delay_ms 1000".to_string()));
            harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started."))
            });

            harness.send(UserAction::BackgroundCurrentTurn);
            let terminal =
                harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));

            assert!(matches!(
                terminal,
                TuiEvent::SessionCompleted { status } if status == "failed"
            ));
            assert_eq!(harness.runtime.controller().current_id(), None);
            harness.shutdown();
        });
    }

    #[test]
    fn hosted_tui_backgrounded_canonical_provider_can_be_stopped_once() {
        with_orca_home(|_| {
            let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
            harness.send(UserAction::Submit("mock_stream_delay_ms 1000".to_string()));
            harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started."))
            });

            harness.send(UserAction::BackgroundCurrentTurn);
            let task = loop {
                let event = harness
                    .event_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("backgrounded task update");
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.status == orca_core::task_types::TaskStatus::Running
                        && task.is_backgrounded
                }) {
                    break task;
                }
            };

            harness.send(UserAction::StopTask {
                task_id: task.id.clone(),
            });
            let stopped = loop {
                let event = harness
                    .event_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("stopped task update");
                if let Some(task) = matching_task_update(event, |candidate| {
                    candidate.id == task.id
                        && candidate.status == orca_core::task_types::TaskStatus::Stopped
                }) {
                    break task;
                }
            };
            assert!(stopped.is_backgrounded);

            harness.send(UserAction::StopTask {
                task_id: task.id.clone(),
            });
            let duplicate_stop = harness.recv_until(
                |event| matches!(event, TuiEvent::Error(message) if message.contains("already stopped")),
            );
            assert!(matches!(duplicate_stop, TuiEvent::Error(_)));
            harness.shutdown();
        });
    }

    #[test]
    fn hosted_tui_backgrounded_canonical_provider_can_be_foregrounded_once() {
        with_orca_home(|_| {
            let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
            harness.send(UserAction::Submit("mock_stream_delay_ms 1000".to_string()));
            harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started."))
            });

            harness.send(UserAction::BackgroundCurrentTurn);
            let task = loop {
                let event = harness
                    .event_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("backgrounded task update");
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.status == orca_core::task_types::TaskStatus::Running
                        && task.is_backgrounded
                }) {
                    break task;
                }
            };

            harness.send(UserAction::ForegroundTask {
                task_id: task.id.clone(),
            });
            harness.recv_until(|event| {
                matching_task_update(event.clone(), |candidate| {
                    candidate.id == task.id
                        && candidate.status == orca_core::task_types::TaskStatus::Running
                        && !candidate.is_backgrounded
                })
                .is_some()
            });

            harness.send(UserAction::ForegroundTask {
                task_id: task.id.clone(),
            });
            harness.recv_until(|event| {
                matches!(event, TuiEvent::Error(message) if message.contains("requires a backgrounded task"))
            });

            let mut saw_completed_delta = false;
            loop {
                match harness
                    .event_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("foregrounded provider completion")
                {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow stream completed.") =>
                    {
                        saw_completed_delta = true;
                    }
                    TuiEvent::SessionCompleted { status } => {
                        assert_eq!(status, "success");
                        break;
                    }
                    _ => {}
                }
            }
            assert!(saw_completed_delta);
            harness.shutdown();
        });
    }

    #[test]
    fn hosted_canonical_approval_uses_operation_fence_and_resumes_turn() {
        with_orca_home(|_| {
            let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
            harness.send(UserAction::Submit(
                "bash printf canonical-approval".to_string(),
            ));

            let key = match harness
                .recv_until(|event| matches!(event, TuiEvent::ApprovalNeeded { .. }))
            {
                TuiEvent::ApprovalNeeded { key, .. } => key,
                _ => unreachable!(),
            };
            assert_eq!(
                Some(key.operation_id),
                harness.runtime.controller().current_id()
            );
            harness.send(UserAction::RespondToInteraction {
                key,
                response: TuiInteractionResponse::Approval(true),
            });

            let terminal =
                harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
            assert!(matches!(
                terminal,
                TuiEvent::SessionCompleted { status } if status == "success"
            ));
            harness.shutdown();
        });
    }

    #[test]
    fn hosted_canonical_permission_uses_operation_fence_and_resumes_turn() {
        with_orca_home(|_| {
            let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
            harness.send(UserAction::Submit(
                "request_network_permissions_then_done example.com".to_string(),
            ));

            let key = match harness
                .recv_until(|event| matches!(event, TuiEvent::PermissionApprovalNeeded { .. }))
            {
                TuiEvent::PermissionApprovalNeeded { key, .. } => key,
                _ => unreachable!(),
            };
            assert_eq!(
                Some(key.operation_id),
                harness.runtime.controller().current_id()
            );
            harness.send(UserAction::RespondToInteraction {
                key,
                response: TuiInteractionResponse::Permission(true),
            });

            let terminal =
                harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
            assert!(matches!(
                terminal,
                TuiEvent::SessionCompleted { status } if status == "success"
            ));
            harness.shutdown();
        });
    }

    #[test]
    fn hosted_canonical_user_input_uses_operation_fence_and_resumes_turn() {
        with_orca_home(|_| {
            let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
            harness.send(UserAction::Submit("ask continue?".to_string()));

            let key = match harness
                .recv_until(|event| matches!(event, TuiEvent::UserInputRequested { .. }))
            {
                TuiEvent::UserInputRequested { key, .. } => key,
                _ => unreachable!(),
            };
            assert_eq!(
                Some(key.operation_id),
                harness.runtime.controller().current_id()
            );
            harness.send(UserAction::RespondToInteraction {
                key,
                response: TuiInteractionResponse::UserInput("yes".to_string()),
            });

            let terminal =
                harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
            assert!(matches!(
                terminal,
                TuiEvent::SessionCompleted { status } if status == "success"
            ));
            harness.shutdown();
        });
    }

    #[test]
    fn hosted_tui_interrupt_targets_activation_race_and_waits_for_terminal() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let pending = test_pending_workflow_notifications();
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let registry = orca_mcp::initialize_registry(&[]);
            let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
            let mut runtime = TuiAgentRuntime::spawn_hosted(
                action_rx,
                event_tx.clone(),
                8,
                controller,
                move |controller, commands, host| {
                    hosted_tui_controller_loop(
                        config, preloaded, event_tx, commands, controller, pending, registry, host,
                    );
                },
            )
            .expect("hosted TUI runtime");

            action_tx
                .send(UserAction::Submit("mock_stream_delay_ms 1000".to_string()))
                .unwrap();
            action_tx.send(UserAction::Interrupt).unwrap();
            loop {
                if let TuiEvent::SessionCompleted { status } =
                    event_rx.recv_timeout(Duration::from_secs(10)).unwrap()
                {
                    assert_eq!(status, "cancelled");
                    assert_eq!(runtime.controller().current_id(), None);
                    break;
                }
            }
            runtime.shutdown().expect("hosted runtime shutdown");
        });
    }

    #[test]
    fn hosted_submission_start_failure_rejects_prompt_and_preserves_preloaded() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(Some(transcript("preserved-session"))));
            let (event_tx, event_rx) = mpsc::unbounded();
            let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
            let pending = test_pending_workflow_notifications();
            let registry = orca_mcp::initialize_registry(&[]);
            let host = orca_runtime::runtime_host::RuntimeHost::start().unwrap();
            let host_handle = host.handle();
            host.shutdown().unwrap();
            let mut thread = None;
            let mut pending_pinned_context = Vec::new();

            handle_hosted_submitted_turn(
                SubmittedTurn::user("retry me".to_string()),
                &config,
                &preloaded,
                &mut thread,
                &mut pending_pinned_context,
                &event_tx,
                &controller,
                &pending,
                &registry,
                &host_handle,
            );

            assert!(matches!(
                event_rx.recv_timeout(Duration::from_secs(1)),
                Ok(TuiEvent::SubmissionRejected {
                    prompt, message, ..
                })
                    if prompt == "retry me"
                        && message.contains("failed to initialize conversation history")
            ));
            assert!(thread.is_none());
            assert_eq!(
                preloaded
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|transcript| transcript.meta.session_id.as_str()),
                Some("preserved-session")
            );
        });
    }

    #[test]
    fn hosted_operation_admission_failure_publishes_terminal_event() {
        with_orca_home(|_| {
            let cfg = test_config(HistoryMode::Record);
            let host = orca_runtime::runtime_host::RuntimeHost::start().unwrap();
            let runtime_thread = host.start_thread(cfg.clone(), "closed thread").unwrap();
            runtime_thread.shutdown().unwrap();
            let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
            let (event_tx, event_rx) = mpsc::unbounded();

            let result = run_hosted_operation(
                &runtime_thread,
                HostedTurnRequest::new("cannot start"),
                cfg,
                &controller,
                &event_tx,
                None,
            );

            assert!(result.is_err());
            assert!(matches!(
                event_rx.recv_timeout(Duration::from_secs(1)),
                Ok(TuiEvent::SessionCompleted { status }) if status == "failed"
            ));
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn queued_operation_controller_install_failure_preserves_queued_identity() {
        with_orca_home(|_| {
            let cfg = test_config(HistoryMode::Record);
            let host = orca_runtime::runtime_host::RuntimeHost::start().unwrap();
            let runtime_thread = host
                .start_thread(cfg.clone(), "queued install failure")
                .unwrap();
            let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
            controller.shutdown();
            let (event_tx, event_rx) = mpsc::unbounded();

            let result = run_hosted_operation(
                &runtime_thread,
                HostedTurnRequest::new("restore queued prompt"),
                cfg,
                &controller,
                &event_tx,
                Some(42),
            );

            assert!(result.is_err());
            let events = event_rx.try_iter().collect::<Vec<_>>();
            let acknowledged = events
                .iter()
                .any(|event| matches!(event, TuiEvent::QueuedSubmissionStarted { id: 42 }));
            let rejected = events.iter().any(|event| {
                matches!(
                    event,
                    TuiEvent::SubmissionRejected {
                        queued_id: Some(42),
                        prompt,
                        message,
                    } if prompt == "restore queued prompt"
                        && message.contains("controller is shutting down")
                )
            });
            assert_ne!(
                acknowledged, rejected,
                "queued identity must be acknowledged or rejected exactly once: {events:?}"
            );
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn queued_goal_preflight_failure_preserves_queued_identity() {
        with_orca_home(|_| {
            let cfg = test_config(HistoryMode::Disabled);
            let host = orca_runtime::runtime_host::RuntimeHost::start().unwrap();
            let runtime_thread = host
                .start_thread(cfg.clone(), "queued preflight failure")
                .unwrap();
            let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
            let (event_tx, event_rx) = mpsc::unbounded();

            run_hosted_goal_run(
                &cfg,
                &runtime_thread,
                SubmittedTurn::queued_user_with_mentions(
                    42,
                    "restore queued prompt".to_string(),
                    orca_runtime::mentions::MentionBindings::new("restore queued prompt"),
                ),
                orca_core::goal_runtime::GoalTurnOrigin::User,
                &event_tx,
                &controller,
                Some(42),
            );

            assert!(matches!(
                event_rx.recv_timeout(Duration::from_secs(1)),
                Ok(TuiEvent::SubmissionRejected {
                    queued_id: Some(42),
                    prompt,
                    message,
                }) if prompt == "restore queued prompt"
                    && message.contains("persistent goals require recorded history")
            ));
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn hosted_tui_shutdown_cancels_and_joins_active_operation() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let pending = test_pending_workflow_notifications();
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let registry = orca_mcp::initialize_registry(&[]);
            let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
            let mut runtime = TuiAgentRuntime::spawn_hosted(
                action_rx,
                event_tx.clone(),
                8,
                controller,
                move |controller, commands, host| {
                    hosted_tui_controller_loop(
                        config, preloaded, event_tx, commands, controller, pending, registry, host,
                    );
                },
            )
            .unwrap();

            action_tx
                .send(UserAction::Submit("mock_stream_delay_ms 1000".to_string()))
                .unwrap();
            loop {
                if matches!(
                    event_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
                    TuiEvent::TurnStarted { .. }
                ) {
                    break;
                }
            }

            runtime.shutdown().expect("hosted runtime shutdown");
        });
    }

    #[test]
    fn running_background_shortcut_dispatches_action_and_returns_to_idle_without_cancelling() {
        let (mut state, action_rx) = test_state();
        state.status = AppStatus::Running;
        let action_tx = state.event_tx.clone();
        let operation = crate::test_support::TestOperationInterrupt::default();

        crate::running_actions::handle_running_shortcut(
            crate::shortcuts::RunningShortcut::BackgroundCurrentTurn,
            &mut state,
            &action_tx,
            &operation,
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::BackgroundCurrentTurn)
        ));
        assert_eq!(operation.call_count(), 0);
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
    fn empty_recorded_hosted_tui_goal_show_reports_no_goal() {
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
                run_hosted_tui_controller_for_test(
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
    fn empty_recorded_hosted_tui_goal_controls_report_session_not_started() {
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
                    run_hosted_tui_controller_for_test(
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
    fn empty_recorded_hosted_tui_goal_resume_without_active_goal_reports_none() {
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
                    run_hosted_tui_controller_for_test(
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
    fn empty_recorded_hosted_tui_goal_resume_restores_latest_active_goal() {
        with_orca_home(|home| {
            let mut writer =
                history::SessionWriter::start(home, "mock", Some("auto".to_string()), "goal")
                    .unwrap();
            writer.enter_turn(orca_core::thread_identity::TurnId::new());
            writer
                .append_message(&orca_core::conversation::Message::user(
                    "previous goal work".to_string(),
                ))
                .unwrap();
            writer.complete("approval_required").unwrap();
            let old_session_id = history::load_session("latest").unwrap().meta.session_id;

            let goal_store = orca_runtime::goal_store::GoalStore::load_default().unwrap();
            let created = goal_store
                .create_goal(orca_runtime::goal_store::CreateGoalInput {
                    session_id: old_session_id.clone(),
                    objective: "resume me".to_string(),
                    token_budget: Some(80_000),
                    now: 1,
                })
                .unwrap();
            goal_store
                .record_usage_once(orca_runtime::goal_store::GoalUsageEvent {
                    usage_event_id: format!("test:{old_session_id}:usage"),
                    goal_id: created.goal_id,
                    source: "test".to_string(),
                    usage: orca_core::goal_runtime::GoalUsage {
                        charged_input_tokens: 23_456,
                        elapsed_seconds: 13 * 60,
                        ..Default::default()
                    },
                    created_at: 2,
                })
                .unwrap();
            let original = goal_store
                .project_thread_goal(&old_session_id)
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
                    run_hosted_tui_controller_for_test(
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
            let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            action_tx.send(UserAction::Interrupt).unwrap();
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
            let store = orca_runtime::goal_store::GoalStore::load_default().unwrap();
            let persisted = store
                .project_thread_goal(&resumed_session_id)
                .unwrap()
                .unwrap();
            assert_eq!(
                persisted.status,
                orca_core::goal_types::ThreadGoalStatus::Paused,
                "interrupting the resumed Goal generation must stop automatic continuation"
            );
            assert_eq!(persisted.token_budget, Some(80_000));
            assert_eq!(persisted.objective, original.objective);
            assert_eq!(persisted.created_at, original.created_at);
            assert!(persisted.tokens_used >= original.tokens_used);
            assert!(persisted.time_used_seconds >= original.time_used_seconds);
        });
    }

    #[test]
    fn goal_auto_continuation_pauses_after_three_no_progress_turns() {
        with_orca_home(|_home| {
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
                    run_hosted_tui_controller_for_test(
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
                .send(UserAction::GoalSet("stall detection goal".to_string()))
                .unwrap();

            // mock provider 不产生 usage，goal 一直 active：
            // 用户 turn 后应跑满 3 个无结构化进展 turn，然后暂停并停。
            let mut stalled_notice = false;
            let mut stalled_status = false;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline && !(stalled_notice && stalled_status) {
                match event_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(TuiEvent::Notice(message)) if message.contains("no measurable progress") => {
                        stalled_notice = true;
                    }
                    Ok(TuiEvent::GoalUpdated(goal))
                        if goal.status == orca_core::goal_types::ThreadGoalStatus::Stalled =>
                    {
                        stalled_status = true;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert!(stalled_notice, "missing stall notice");
            assert!(stalled_status, "missing Stalled goal update");
        });
    }

    #[test]
    fn goal_resume_ignores_legacy_json_temp_directory() {
        with_orca_home(|home| {
            let mut writer =
                history::SessionWriter::start(home, "mock", Some("auto".to_string()), "goal")
                    .unwrap();
            writer.enter_turn(orca_core::thread_identity::TurnId::new());
            writer
                .append_message(&orca_core::conversation::Message::user(
                    "previous goal work".to_string(),
                ))
                .unwrap();
            writer.complete("approval_required").unwrap();
            let old_session_id = history::load_session("latest").unwrap().meta.session_id;

            orca_runtime::goal_store::GoalStore::load_default()
                .unwrap()
                .create_goal(orca_runtime::goal_store::CreateGoalInput {
                    session_id: old_session_id.clone(),
                    objective: "resume atomically".to_string(),
                    token_budget: None,
                    now: 1,
                })
                .unwrap();
            std::fs::create_dir(home.join("goals_1.json.tmp")).unwrap();

            let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);

            harness.send(UserAction::GoalResume);
            let event = harness.recv_until(|event| matches!(event, TuiEvent::GoalUpdated(_)));

            match event {
                TuiEvent::GoalUpdated(goal) => {
                    assert_eq!(goal.objective, "resume atomically");
                    assert_eq!(goal.status, orca_core::goal_types::ThreadGoalStatus::Active);
                }
                other => panic!("expected resumed goal update, got {other:?}"),
            }
            assert!(matches!(
                &harness.config.lock().unwrap().history_mode,
                HistoryMode::Resume(session_id) if session_id == &old_session_id
            ));
            assert!(harness.preloaded.lock().unwrap().is_none());
            harness.shutdown();
        });
    }

    #[test]
    fn preloaded_goal_resume_projects_elapsed_before_first_turn_started() {
        with_orca_home(|_| {
            let session_id = "resume-goal-timer-session";
            let goal_store = orca_runtime::goal_store::GoalStore::load_default().unwrap();
            let created = goal_store
                .create_goal(orca_runtime::goal_store::CreateGoalInput {
                    session_id: session_id.to_string(),
                    objective: "resume with elapsed time".to_string(),
                    token_budget: None,
                    now: 1,
                })
                .unwrap();
            goal_store
                .record_usage_once(orca_runtime::goal_store::GoalUsageEvent {
                    usage_event_id: format!("test:{session_id}:elapsed"),
                    goal_id: created.goal_id,
                    source: "test".to_string(),
                    usage: orca_core::goal_runtime::GoalUsage {
                        charged_input_tokens: 23_456,
                        elapsed_seconds: 13 * 60,
                        ..Default::default()
                    },
                    created_at: 2,
                })
                .unwrap();
            let persisted = goal_store.project_thread_goal(session_id).unwrap().unwrap();
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
                    run_hosted_tui_controller_for_test(
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

            action_tx.send(UserAction::Interrupt).unwrap();
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
            orca_runtime::goal_store::GoalStore::load_default()
                .unwrap()
                .create_goal(orca_runtime::goal_store::CreateGoalInput {
                    session_id: session_id.to_string(),
                    objective: "resumed objective".to_string(),
                    token_budget: None,
                    now: 1,
                })
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
                    run_hosted_tui_controller_for_test(
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
            let reloaded = orca_runtime::goal_store::GoalStore::load_default()
                .unwrap()
                .project_thread_goal(session_id)
                .unwrap()
                .unwrap();
            assert_eq!(
                reloaded.status,
                orca_core::goal_types::ThreadGoalStatus::Paused
            );
        });
    }

    #[test]
    fn active_goal_pause_bypasses_command_backlog_and_cancels_goal_run() {
        with_orca_home(|_| {
            let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
            harness.send(UserAction::GoalSet("mock_stream_delay_ms 5000".to_string()));
            harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started."))
            });

            harness.send(UserAction::GoalPause);
            let deadline = Instant::now() + Duration::from_secs(2);
            let paused = loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "active /goal pause stayed behind the running operation"
                );
                let event = harness
                    .event_rx
                    .recv_timeout(remaining)
                    .expect("active goal pause update");
                if matches!(
                    &event,
                    TuiEvent::GoalUpdated(goal)
                        if goal.status == orca_core::goal_types::ThreadGoalStatus::Paused
                ) {
                    break event;
                }
            };

            assert!(matches!(paused, TuiEvent::GoalUpdated(_)));
            harness.shutdown();
        });
    }

    #[test]
    fn preloaded_resume_goal_show_reads_persisted_goal_before_live_session_exists() {
        with_orca_home(|_| {
            let session_id = "resume-goal-show-session";
            orca_runtime::goal_store::GoalStore::load_default()
                .unwrap()
                .create_goal(orca_runtime::goal_store::CreateGoalInput {
                    session_id: session_id.to_string(),
                    objective: "show resumed objective".to_string(),
                    token_budget: None,
                    now: 1,
                })
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
                    run_hosted_tui_controller_for_test(
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
                run_hosted_tui_controller_for_test(
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
    fn backgrounded_hosted_tui_accepts_next_submit_before_first_turn_completes() {
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
                    run_hosted_tui_controller_for_test(
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
    fn cancelled_hosted_tui_turn_does_not_cancel_next_submit() {
        with_orca_home(|_| {
            let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
            harness.send(UserAction::Submit("mock_stream_delay_ms 1000".to_string()));

            let first_id = loop {
                match harness
                    .event_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                {
                    TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started.") => {
                        break harness
                            .runtime
                            .controller()
                            .current_id()
                            .expect("first operation id");
                    }
                    TuiEvent::Error(message) => panic!("unexpected first-turn error: {message}"),
                    _ => {}
                }
            };

            harness.send(UserAction::Interrupt);
            loop {
                match harness
                    .event_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                {
                    TuiEvent::SessionCompleted { status } => {
                        assert_eq!(status, "cancelled");
                        break;
                    }
                    TuiEvent::Error(message) => panic!("unexpected cancellation error: {message}"),
                    _ => {}
                }
            }

            harness.send(UserAction::Submit("mock_history_echo".to_string()));

            let mut second_id = None;
            let mut saw_second_output = false;
            loop {
                match harness
                    .event_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                {
                    TuiEvent::TurnStarted { .. } => {
                        let current = harness
                            .runtime
                            .controller()
                            .current_id()
                            .expect("second operation id");
                        assert_ne!(current, first_id);
                        second_id = Some(current);
                    }
                    TuiEvent::MessageDelta(text) if text.contains("Mock history users:") => {
                        saw_second_output = true;
                    }
                    TuiEvent::SessionCompleted { status } => {
                        assert_eq!(status, "success");
                        break;
                    }
                    TuiEvent::Error(message) => panic!("unexpected second-turn error: {message}"),
                    _ => {}
                }
            }

            harness.shutdown();

            assert!(
                second_id.is_some(),
                "second turn must start a fresh operation"
            );
            assert!(saw_second_output, "second turn must run to provider output");
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
                    run_hosted_tui_controller_for_test(
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
                    run_hosted_tui_controller_for_test(
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

            let mut tasks = Vec::new();
            loop {
                let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                match event {
                    TuiEvent::WorkflowTaskUpdated { task }
                        if task.task_type == orca_core::task_types::TaskType::MainSession =>
                    {
                        tasks.push(task);
                    }
                    TuiEvent::SessionCompleted { .. } => break,
                    _ => {}
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert!(tasks.len() >= 2);
            assert!(
                tasks.iter().all(|task| task.id == tasks[0].id),
                "actor and temporary TUI executor must share one task id"
            );
            assert!(
                tasks
                    .iter()
                    .all(|task| { task.description == "Workflow notification notification-1" })
            );
            assert_eq!(
                tasks.first().unwrap().status,
                orca_core::task_types::TaskStatus::Running
            );
            assert_eq!(
                tasks.last().unwrap().status,
                orca_core::task_types::TaskStatus::Completed
            );
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
                    run_hosted_tui_controller_for_test(
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
    fn hosted_user_turn_request_opts_into_task_tracking_without_goal_tools() {
        let submitted = SubmittedTurn::user("inspect the runtime".to_string());

        let request = hosted_turn_request(&submitted, false);

        assert!(!request.allows_goal_tools());
        assert!(!request.tracks_goal_usage());
        assert!(request.is_backtrack_target());
        assert_eq!(request.task_description(), Some("inspect the runtime"));
    }

    #[test]
    fn hosted_goal_notification_request_preserves_pinned_task_semantics() {
        let submitted =
            SubmittedTurn::workflow_notification(crate::types::PendingWorkflowNotification {
                id: "notification-42".to_string(),
                prompt: "<task-notification>done</task-notification>".to_string(),
            });

        let request = hosted_turn_request(&submitted, true);

        assert!(request.allows_goal_tools());
        assert!(request.tracks_goal_usage());
        assert!(!request.is_backtrack_target());
        assert_eq!(
            request.task_description(),
            Some("Workflow notification notification-42")
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
    fn backgrounded_hosted_tui_does_not_complete_unexecuted_tool_calls() {
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
                    run_hosted_tui_controller_for_test(
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
    fn backgrounded_hosted_tui_marks_unexecuted_tool_calls_approval_required() {
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
                    run_hosted_tui_controller_for_test(
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
    fn backgrounded_hosted_tui_reports_pending_tool_name() {
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
                    run_hosted_tui_controller_for_test(
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
    fn backgrounded_hosted_tui_notifies_approval_required_in_user_language() {
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
                    run_hosted_tui_controller_for_test(
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
                    run_hosted_tui_controller_for_test(
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
                    run_hosted_tui_controller_for_test(
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
                    Ok(TuiEvent::ApprovalNeeded { key, tool, .. }) => {
                        saw_second_approval = true;
                        seen.push(format!("approval: {tool}"));
                        action_tx
                            .send(UserAction::RespondToInteraction {
                                key,
                                response: TuiInteractionResponse::Approval(false),
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
    fn running_queue_preview_restore_and_terminal_dispatch_frames_are_consistent() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        let mut config = test_config(HistoryMode::Record);
        let shared = Arc::new(Mutex::new(config.clone()));
        let operation = crate::test_support::TestOperationInterrupt::default();
        let preloaded = Arc::new(Mutex::new(None));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = TextArea::default();

        for code in [KeyCode::Char('f'), KeyCode::Char('o'), KeyCode::Char('o')] {
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
        }
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_status_key(
            &Event::Key(enter),
            &enter,
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
        assert_eq!(state.queued_user_messages.len(), 1);
        assert!(action_rx.try_recv().is_err());
        assert!(
            !state
                .messages
                .iter()
                .any(|message| matches!(message, ChatMessage::User(text) if text == "foo"))
        );

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .unwrap();
        assert!(format!("{:?}", terminal.backend().buffer()).contains("Queued 1"));

        let restore = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        handle_status_key(
            &Event::Key(restore),
            &restore,
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
        assert!(state.queued_user_messages.is_empty());
        assert_eq!(textarea_text(&textarea), "foo");

        textarea.insert_char('!');
        handle_status_key(
            &Event::Key(enter),
            &enter,
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
        assert!(action_rx.try_recv().is_err());

        let pending = test_pending_workflow_notifications();
        let mut presentation = TerminalPresentation::new(
            false,
            TerminalPresentationProfile {
                osc9_supported: false,
                tmux_passthrough: false,
            },
        );
        handle_runtime_event(
            TuiEvent::SessionCompleted {
                status: "success".to_string(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitQueued { prompt, .. }) if prompt == "foo!"
        ));
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::User(text)) if text == "foo!"
        ));
        assert!(state.queued_user_messages.is_empty());
        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .unwrap();
        assert!(!format!("{:?}", terminal.backend().buffer()).contains("Queued 1"));
    }

    #[test]
    fn hosted_tui_runs_app_state_queued_follow_ups_one_at_a_time_in_fifo_order() {
        with_orca_home(|_| {
            let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
            harness.send(UserAction::Submit("mock_stream_delay_ms 100".to_string()));
            harness.recv_until(|event| matches!(event, TuiEvent::TurnStarted { .. }));
            let first_terminal = harness.recv_until(|event| {
                matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
            });

            let mut state = AppState::new(
                harness.action_tx.clone(),
                "test".to_string(),
                "mock".to_string(),
                "/tmp".to_string(),
            );
            for _ in 0..2 {
                state
                    .enqueue_user_message(
                        crate::queued_input::QueuedUserMessage::from_composer(
                            "mock_history_echo".to_string(),
                            Vec::new(),
                            orca_runtime::mentions::MentionBindings::default(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
            }
            state.enter_running();
            let pending = test_pending_workflow_notifications();
            let theme = Theme::named(ThemeName::Dark);
            let mut textarea = TextArea::default();
            let mut vim = VimState::new(false);
            let mut presentation = TerminalPresentation::new(
                false,
                TerminalPresentationProfile {
                    osc9_supported: false,
                    tmux_passthrough: false,
                },
            );

            let mut terminal_event = first_terminal;
            for expected_count in [2usize, 3usize] {
                handle_runtime_event(
                    terminal_event,
                    &mut state,
                    &harness.action_tx,
                    &pending,
                    &mut textarea,
                    &mut vim,
                    &theme,
                    &mut presentation,
                );
                let queued_started = harness
                    .recv_until(|event| matches!(event, TuiEvent::QueuedSubmissionStarted { .. }));
                state.update(queued_started);
                let turn_started =
                    harness.recv_until(|event| matches!(event, TuiEvent::TurnStarted { .. }));
                state.update(turn_started);
                let delta = harness.recv_until(|event| {
                    matches!(
                        event,
                        TuiEvent::MessageDelta(text)
                            if text.contains("Mock history users:")
                    )
                });
                let TuiEvent::MessageDelta(text) = delta else {
                    unreachable!()
                };
                let expected = format!(
                    "Mock history users: {}",
                    std::iter::once("mock_stream_delay_ms 100")
                        .chain(std::iter::repeat_n("mock_history_echo", expected_count - 1,))
                        .collect::<Vec<_>>()
                        .join(" | ")
                );
                assert_eq!(text, expected);
                terminal_event = harness.recv_until(|event| {
                    matches!(
                        event,
                        TuiEvent::SessionCompleted { status }
                            if status == "success"
                    )
                });
                if expected_count == 2 {
                    assert_eq!(state.queued_user_messages.len(), 1);
                    state.set_status(AppStatus::Running);
                }
            }

            assert!(state.queued_user_messages.is_empty());
            harness.shutdown();
        });
    }

    #[test]
    fn search_keyboard_frames_move_active_match_without_composer_mutation() {
        let (mut state, _rx) = test_state();
        for index in 0..30 {
            state.push_message(ChatMessage::System(format!("row {index:02} alpha")));
        }
        state.auto_scroll = false;
        let theme = Theme::named(ThemeName::Dark);
        let textarea = TextArea::from(["composer draft"]);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10))
            .expect("test backend");
        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .expect("initial draw");

        state.open_transcript_search();
        state.replace_transcript_search_query("alpha");
        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .expect("search draw");
        let first = state.transcript_search.active_ordinal();
        assert!(format!("{:?}", terminal.backend().buffer()).contains("1/30"));

        handle_transcript_search_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );
        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .expect("next draw");
        assert_ne!(state.transcript_search.active_ordinal(), first);
        assert!(!state.auto_scroll);
        assert_eq!(textarea.lines(), &["composer draft".to_string()]);

        handle_transcript_search_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            &mut state,
        );
        assert_eq!(state.transcript_search.active_ordinal(), first);
    }

    #[test]
    fn running_search_esc_closes_before_interrupt_and_paste_never_touches_composer() {
        let (mut state, _state_action_rx) = test_state();
        state.enter_running();
        state.open_transcript_search();
        let mut textarea = TextArea::from(["composer"]);
        let operation = crate::test_support::TestOperationInterrupt::default();
        let mut config = test_config(HistoryMode::Record);
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut vim = VimState::new(false);

        assert!(handle_paste_event(
            &Event::Paste("alpha\r\nbeta".to_string()),
            &mut state,
            &config,
            &mut textarea,
        ));
        assert_eq!(state.transcript_search.query(), "alpha beta");
        assert_eq!(textarea.lines(), &["composer".to_string()]);
        assert!(state.pending_pastes.is_empty());

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key_event_preflight(
            esc,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &mut vim,
            || Ok(()),
        )
        .unwrap();
        assert!(!state.transcript_search.open);
        assert_eq!(operation.call_count(), 0);

        let preloaded = Arc::new(Mutex::new(None));
        let theme = Theme::named(ThemeName::Dark);
        handle_status_key(
            &Event::Key(esc),
            &esc,
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
        assert_eq!(operation.call_count(), 1);
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    }

    #[test]
    fn mouse_selection_over_search_match_wins_and_copy_stays_exact() {
        let (mut state, _rx) = test_state();
        state.push_message(ChatMessage::System("alpha beta".to_string()));
        state.open_transcript_search();
        state.replace_transcript_search_query("alpha");
        let theme = Theme::named(ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8))
            .expect("test backend");
        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .expect("search draw");

        state.selection = Some(TranscriptSelection::unit(
            SelectionGranularity::Cell,
            SelectionPos { row: 0, col: 1 },
            SelectionPos { row: 0, col: 3 },
        ));
        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .expect("selection draw");
        assert_eq!(
            state
                .transcript_render_cache
                .extract_text(state.selection.as_ref().unwrap()),
            "lph"
        );
        let selected_cells = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.style().bg == theme.selection_style().bg)
            .count();
        assert!(selected_cells >= 3);
    }

    #[test]
    fn streaming_and_resize_refresh_matches_without_stealing_active_identity() {
        let (mut state, _rx) = test_state();
        state.update(TuiEvent::MessageDelta(
            "prefix long words before alpha\n\nhidden alpha".to_string(),
        ));
        state.open_transcript_search();
        state.replace_transcript_search_query("alpha");
        let theme = Theme::named(ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 8))
            .expect("test backend");
        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .expect("held draw");
        assert_eq!(state.transcript_search.match_count(), 1);
        let identity = state
            .transcript_search
            .active_match()
            .unwrap()
            .line_identity;

        state.update(TuiEvent::MessageDelta("\n".to_string()));
        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .expect("released draw");
        assert_eq!(state.transcript_search.match_count(), 2);
        assert_eq!(
            state
                .transcript_search
                .active_match()
                .unwrap()
                .line_identity,
            identity
        );
        let before = state.transcript_search.active_match().unwrap().start;

        let mut resized = ratatui::Terminal::new(ratatui::backend::TestBackend::new(8, 8))
            .expect("resized backend");
        resized
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .expect("resized draw");
        assert_eq!(
            state
                .transcript_search
                .active_match()
                .unwrap()
                .line_identity,
            identity
        );
        assert_ne!(
            state.transcript_search.active_match().unwrap().start,
            before
        );
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
            state
                .enqueue_user_message(
                    crate::queued_input::QueuedUserMessage::from_composer(
                        "queued".to_string(),
                        Vec::new(),
                        orca_runtime::mentions::MentionBindings::default(),
                    )
                    .unwrap(),
                )
                .unwrap();
            state.queued_submission_in_flight = Some(
                crate::queued_input::QueuedUserMessage::from_composer(
                    "in flight".to_string(),
                    Vec::new(),
                    orca_runtime::mentions::MentionBindings::default(),
                )
                .unwrap(),
            );
            state.queued_input_error = Some("error".to_string());
            state.suspend_queued_follow_up_autosend();
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
            assert!(state.queued_user_messages.is_empty());
            assert!(state.queued_submission_in_flight.is_none());
            assert!(state.queued_input_error.is_none());
            assert!(state.queued_follow_up_autosend);
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
    fn waiting_user_input_submit_sends_typed_user_input_response() {
        let (mut state, _rx) = test_state();
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim_state = VimState::new(false);
        let mut textarea = make_textarea_with_text("continue", &vim_state, &theme);
        let key = interaction_key(TuiInteractionKind::UserInput, "ask-1");
        state.set_status(AppStatus::WaitingUserInput);
        state.pending_input = Some(PendingTuiInput::UserInput(key.clone()));

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
            Ok(UserAction::RespondToInteraction {
                key: actual_key,
                response: TuiInteractionResponse::UserInput(answer),
            }) if actual_key == key && answer == "continue"
        ));
        assert!(state.pending_input.is_none());
        assert_eq!(state.status, AppStatus::Running);
    }

    #[test]
    fn waiting_mcp_elicitation_submit_sends_typed_mcp_response() {
        let (mut state, _rx) = test_state();
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim_state = VimState::new(false);
        let mut textarea = make_textarea_with_text(
            r#"{"repository":"echoVic/blade-deepseek"}"#,
            &vim_state,
            &theme,
        );
        let key = interaction_key(TuiInteractionKind::McpElicitation, "mcp-1");
        state.set_status(AppStatus::WaitingUserInput);
        state.pending_input = Some(PendingTuiInput::McpElicitation(key.clone()));

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
            Ok(UserAction::RespondToInteraction {
                key: actual_key,
                response: TuiInteractionResponse::McpElicitation {
                    accepted: true,
                    content_json: Some(content),
                },
            }) if actual_key == key && content == r#"{"repository":"echoVic/blade-deepseek"}"#
        ));
        assert!(state.pending_input.is_none());
        assert_eq!(state.status, AppStatus::Running);
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

fn update_goal_status_for_session(
    thread: Option<&RuntimeThreadHandle>,
    session_id: Option<&str>,
    status: orca_core::goal_types::ThreadGoalStatus,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> bool {
    let Some(session_id) = session_id else {
        let _ = event_tx.send(TuiEvent::Error(
            "persistent goals require a saved session".to_string(),
        ));
        return false;
    };
    let mut detached_join = None;
    let runtime = match thread {
        Some(thread) => match thread.goal_runtime() {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = event_tx.send(TuiEvent::Error(error.to_string()));
                return false;
            }
        },
        None => match orca_runtime::goal_actor::GoalRuntimeHandle::open_default() {
            Ok((runtime, join)) => {
                detached_join = Some(join);
                runtime
            }
            Err(error) => {
                let _ = event_tx.send(TuiEvent::Error(error.to_string()));
                return false;
            }
        },
    };
    let result = match status {
        orca_core::goal_types::ThreadGoalStatus::Active => runtime.resume(
            session_id,
            orca_core::goal_runtime::GoalTurnOrigin::Resume,
            now_timestamp(),
        ),
        orca_core::goal_types::ThreadGoalStatus::Paused => runtime.pause(
            session_id,
            orca_core::goal_runtime::GoalPauseReason::User,
            "paused by user",
            now_timestamp(),
        ),
        _ => Err(orca_runtime::goal_actor::GoalActorError::Invalid(
            "TUI can only pause or resume a goal through this command".to_string(),
        )),
    };
    let updated = match result {
        Ok(_) => match runtime.project_thread_goal(session_id) {
            Ok(Some(goal)) => {
                let _ = event_tx.send(TuiEvent::GoalUpdated(goal));
                true
            }
            Ok(None) => {
                let _ = event_tx.send(TuiEvent::Error("no goal is currently set".to_string()));
                false
            }
            Err(error) => {
                let _ = event_tx.send(TuiEvent::Error(error.to_string()));
                false
            }
        },
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!("failed to update goal: {error}")));
            false
        }
    };
    drop(runtime);
    if let Some(join) = detached_join {
        let _ = join.join();
    }
    updated
}

fn goal_continuation_prompt(objective: &str, continuation: usize) -> String {
    format!(
        "[Goal continuation #{continuation}]\nContinue working on this persistent goal:\n{objective}\n\nWork from current evidence. Preserve the full objective, verify every requirement before completion, and call update_goal only with status \"complete\" when the goal is actually finished or status \"blocked\" after the same blocker has repeated for at least three consecutive goal turns."
    )
}

fn send_submission_error(
    event_tx: &mpsc::Sender<TuiEvent>,
    queued_id: Option<u64>,
    rejection_prompt: Option<&str>,
    message: String,
) {
    if let Some(prompt) = rejection_prompt {
        let _ = event_tx.send(TuiEvent::SubmissionRejected {
            queued_id,
            prompt: prompt.to_string(),
            message,
        });
    } else {
        let _ = event_tx.send(TuiEvent::Error(message));
    }
}

#[allow(clippy::too_many_arguments)]
fn hosted_tui_controller_loop(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
    controller: TuiOperationController,
    pending_workflow_notifications: bridge::PendingWorkflowNotifications,
    mcp_registry: orca_mcp::McpRegistry,
    host: RuntimeHostHandle,
) {
    let mut thread: Option<RuntimeThreadHandle> = None;
    let mut pending_pinned_context = Vec::new();

    loop {
        let action = if controller.is_shutdown() {
            Ok(UserAction::Cancel)
        } else {
            action_rx.recv()
        };
        match action {
            Ok(UserAction::Submit(prompt)) => handle_hosted_submitted_turn(
                SubmittedTurn::user(prompt),
                &config,
                &preloaded,
                &mut thread,
                &mut pending_pinned_context,
                &event_tx,
                &controller,
                &pending_workflow_notifications,
                &mcp_registry,
                &host,
            ),
            Ok(UserAction::SubmitWithMentions { prompt, bindings }) => {
                handle_hosted_submitted_turn(
                    SubmittedTurn::user_with_mentions(prompt, bindings),
                    &config,
                    &preloaded,
                    &mut thread,
                    &mut pending_pinned_context,
                    &event_tx,
                    &controller,
                    &pending_workflow_notifications,
                    &mcp_registry,
                    &host,
                );
            }
            Ok(UserAction::SubmitQueued {
                id,
                prompt,
                bindings,
            }) => {
                handle_hosted_submitted_turn(
                    SubmittedTurn::queued_user_with_mentions(id, prompt, bindings),
                    &config,
                    &preloaded,
                    &mut thread,
                    &mut pending_pinned_context,
                    &event_tx,
                    &controller,
                    &pending_workflow_notifications,
                    &mcp_registry,
                    &host,
                );
            }
            Ok(UserAction::SubmitWorkflowNotification(notification)) => {
                handle_hosted_submitted_turn(
                    SubmittedTurn::workflow_notification(notification),
                    &config,
                    &preloaded,
                    &mut thread,
                    &mut pending_pinned_context,
                    &event_tx,
                    &controller,
                    &pending_workflow_notifications,
                    &mcp_registry,
                    &host,
                );
            }
            Ok(UserAction::RunWorkflow { name, args }) => {
                let cfg = config.lock().unwrap().clone();
                if let Err(error) = ensure_hosted_thread(
                    &mut thread,
                    &host,
                    &cfg,
                    &preloaded,
                    &format!("Run saved workflow `{name}`"),
                    &mcp_registry,
                    &mut pending_pinned_context,
                    &event_tx,
                ) {
                    send_hosted_action_failure(&event_tx, error);
                    continue;
                }
                if let Some(runtime_thread) = thread.as_ref() {
                    let observer = Arc::new(TuiHostedEventObserver::new(event_tx.clone()));
                    let _ = observer.finish_foreground();
                    let mut request = HostedWorkflowRequest::new(name).with_config(cfg.clone());
                    if let Some(args) = args.as_deref() {
                        request = match request.with_command_args(args) {
                            Ok(request) => request,
                            Err(error) => {
                                let _ = event_tx.send(TuiEvent::Error(error));
                                continue;
                            }
                        };
                    }
                    if let Err(error) =
                        runtime_thread.launch_workflow(request.with_event_observer(observer))
                    {
                        let _ = event_tx.send(TuiEvent::Error(error.to_string()));
                        continue;
                    }
                }
                if cfg.desktop_notifications {
                    let _ = orca_runtime::notify::notify("Orca", "Workflow launched");
                }
            }
            Ok(UserAction::Interrupt) | Ok(UserAction::BackgroundCurrentTurn) => {}
            Ok(UserAction::SetModel(model)) => {
                if let Some(runtime_thread) = thread.as_ref()
                    && let Err(error) =
                        runtime_thread.mutate(RuntimeThreadMutation::SetModel(Some(model)))
                {
                    let _ = event_tx.send(TuiEvent::Error(error.to_string()));
                }
            }
            Ok(UserAction::Remember(note)) => {
                let context = format!("[Pinned remembered note]\n{}", note.trim());
                if let Some(runtime_thread) = thread.as_ref() {
                    if let Err(error) =
                        runtime_thread.mutate(RuntimeThreadMutation::AddPinnedContext(context))
                    {
                        let _ = event_tx.send(TuiEvent::Error(error.to_string()));
                    }
                } else {
                    pending_pinned_context.push(context);
                }
            }
            Ok(UserAction::Compact) => {
                let Some(runtime_thread) = thread.as_ref() else {
                    let _ = event_tx.send(TuiEvent::Error("nothing to compact".to_string()));
                    continue;
                };
                let request = HostedTurnRequest::new("")
                    .with_operation_kind(HostedOperationKind::ManualCompaction);
                let cfg = config.lock().unwrap().clone();
                if let Err(error) =
                    run_hosted_operation(runtime_thread, request, cfg, &controller, &event_tx, None)
                {
                    let _ = event_tx.send(TuiEvent::Error(format!(
                        "manual compaction failed: {error}"
                    )));
                }
            }
            Ok(UserAction::Backtrack) => {
                let result = thread
                    .as_ref()
                    .map(RuntimeThreadHandle::backtrack_last_user)
                    .transpose();
                match result {
                    Ok(Some(Some(prompt))) => {
                        let _ = event_tx.send(TuiEvent::Backtracked { prompt });
                    }
                    Ok(Some(None)) | Ok(None) => {
                        let _ = event_tx.send(TuiEvent::Error("nothing to backtrack".to_string()));
                    }
                    Err(error) => {
                        let _ = event_tx.send(TuiEvent::Error(error.to_string()));
                    }
                }
            }
            Ok(UserAction::StopTask { task_id }) => {
                let registry = thread.as_ref().map(RuntimeThreadHandle::task_registry);
                let _ = stop_task_for_tui(registry.as_ref(), &task_id, &event_tx);
            }
            Ok(UserAction::ForegroundTask { task_id }) => {
                let registry = thread.as_ref().map(RuntimeThreadHandle::task_registry);
                let _ = foreground_task_for_tui(registry.as_ref(), &task_id, &event_tx);
            }
            Ok(UserAction::ResolveBackgroundApproval { id, approved }) => {
                let registry = thread.as_ref().map(RuntimeThreadHandle::task_registry);
                let continuation = submit_background_approval_response_for_tui(
                    registry.as_ref(),
                    &id,
                    approved,
                    &event_tx,
                );
                if approved
                    && let (Some(runtime_thread), Some(continuation)) =
                        (thread.as_ref(), continuation)
                {
                    let cfg = config.lock().unwrap().clone();
                    let request = HostedTurnRequest::new("")
                        .with_operation_kind(HostedOperationKind::BackgroundContinuation {
                            task_id: continuation.task_id().to_string(),
                        })
                        .with_goal_usage_tracking(true);
                    match run_hosted_operation(
                        runtime_thread,
                        request,
                        cfg,
                        &controller,
                        &event_tx,
                        None,
                    ) {
                        Ok(TuiHostedOperationOutcome::Turn { .. }) => {}
                        Ok(TuiHostedOperationOutcome::ManualCompaction) => {}
                        Err(error) => {
                            let _ = event_tx.send(TuiEvent::Error(error.to_string()));
                        }
                    }
                }
            }
            Ok(UserAction::GoalShow) => {
                show_hosted_goal(&thread, &preloaded, &config, &event_tx);
            }
            Ok(UserAction::GoalSet(objective)) => {
                let cfg = config.lock().unwrap().clone();
                if let Err(error) = ensure_hosted_thread(
                    &mut thread,
                    &host,
                    &cfg,
                    &preloaded,
                    &objective,
                    &mcp_registry,
                    &mut pending_pinned_context,
                    &event_tx,
                ) {
                    send_hosted_action_failure(&event_tx, error);
                    continue;
                }
                let Some(session_id) = thread
                    .as_ref()
                    .and_then(RuntimeThreadHandle::session_id)
                    .map(str::to_string)
                else {
                    send_goal_history_error(&event_tx);
                    continue;
                };
                let runtime = match thread
                    .as_ref()
                    .and_then(|thread| thread.goal_runtime().ok())
                {
                    Some(runtime) => runtime,
                    None => {
                        let _ = event_tx.send(TuiEvent::Error(
                            "failed to initialize runtime-owned goal actor".to_string(),
                        ));
                        continue;
                    }
                };
                let result = match runtime.read(&session_id) {
                    Ok(Some(_)) => runtime
                        .edit(&session_id, objective.clone(), None, now_timestamp())
                        .map_err(|error| error.to_string()),
                    Ok(None) => runtime
                        .create(orca_runtime::goal_store::CreateGoalInput {
                            session_id: session_id.clone(),
                            objective: objective.clone(),
                            token_budget: None,
                            now: now_timestamp(),
                        })
                        .map(Some)
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                match result {
                    Ok(Some(_)) | Ok(None) => {
                        let goal = runtime.project_thread_goal(&session_id).ok().flatten();
                        let Some(goal) = goal else {
                            let _ = event_tx
                                .send(TuiEvent::Error("no goal is currently set".to_string()));
                            continue;
                        };
                        let _ = event_tx.send(TuiEvent::GoalUpdated(goal));
                        let _ = event_tx.send(TuiEvent::Notice(
                            "Starting goal. Automatic continuation will keep running while it remains active."
                                .to_string(),
                        ));
                        if let Some(runtime_thread) = thread.as_ref() {
                            run_hosted_goal_run(
                                &cfg,
                                runtime_thread,
                                SubmittedTurn::user(objective),
                                orca_core::goal_runtime::GoalTurnOrigin::User,
                                &event_tx,
                                &controller,
                                None,
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
                let Some(session_id) = existing_hosted_goal_session_id(
                    thread.as_ref(),
                    &preloaded,
                    &config,
                    &event_tx,
                ) else {
                    continue;
                };
                let runtime = match thread
                    .as_ref()
                    .and_then(|thread| thread.goal_runtime().ok())
                {
                    Some(runtime) => runtime,
                    None => {
                        let _ = event_tx.send(TuiEvent::Error(
                            "failed to initialize runtime-owned goal actor".to_string(),
                        ));
                        continue;
                    }
                };
                match runtime.edit(&session_id, objective, None, now_timestamp()) {
                    Ok(Some(record)) => {
                        let goal = runtime
                            .project_thread_goal(&session_id)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| orca_core::goal_types::ThreadGoal {
                                session_id: record.session_id.clone(),
                                objective: record.objective.clone(),
                                status: orca_core::goal_types::ThreadGoalStatus::Active,
                                token_budget: record.token_budget,
                                tokens_used: record.usage.charged_tokens(),
                                time_used_seconds: record.usage.elapsed_seconds,
                                created_at: 0,
                                updated_at: now_timestamp(),
                            });
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
                let Some(session_id) = existing_hosted_goal_session_id(
                    thread.as_ref(),
                    &preloaded,
                    &config,
                    &event_tx,
                ) else {
                    continue;
                };
                let runtime = match thread
                    .as_ref()
                    .and_then(|thread| thread.goal_runtime().ok())
                {
                    Some(runtime) => runtime,
                    None => {
                        let _ = event_tx.send(TuiEvent::Error(
                            "failed to initialize runtime-owned goal actor".to_string(),
                        ));
                        continue;
                    }
                };
                match runtime.clear(&session_id) {
                    Ok(()) => {
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
                    existing_hosted_goal_session_id(thread.as_ref(), &preloaded, &config, &event_tx)
                {
                    update_goal_status_for_session(
                        thread.as_ref(),
                        Some(&session_id),
                        orca_core::goal_types::ThreadGoalStatus::Paused,
                        &event_tx,
                    );
                }
            }
            Ok(UserAction::GoalResume) => {
                if current_hosted_goal_session_id(thread.as_ref(), &preloaded).is_none() {
                    resume_latest_active_goal_hosted(
                        &mut thread,
                        &host,
                        &config,
                        &preloaded,
                        &mcp_registry,
                        &event_tx,
                        &controller,
                        &pending_workflow_notifications,
                    );
                    continue;
                }
                let Some(session_id) = current_hosted_goal_session_id(thread.as_ref(), &preloaded)
                else {
                    continue;
                };
                update_goal_status_for_session(
                    thread.as_ref(),
                    Some(&session_id),
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    &event_tx,
                );
                let goal = thread
                    .as_ref()
                    .and_then(|runtime_thread| runtime_thread.goal_runtime().ok())
                    .and_then(|runtime| runtime.project_thread_goal(&session_id).ok().flatten());
                if let (Some(runtime_thread), Some(goal)) = (thread.as_ref(), goal) {
                    let cfg = config.lock().unwrap().clone();
                    run_hosted_goal_run(
                        &cfg,
                        runtime_thread,
                        SubmittedTurn::user(goal_continuation_prompt(&goal.objective, 1)),
                        orca_core::goal_runtime::GoalTurnOrigin::Resume,
                        &event_tx,
                        &controller,
                        None,
                    );
                }
            }
            Ok(UserAction::Cancel) | Err(_) => break,
            Ok(UserAction::RespondToInteraction { .. }) => {}
        }
    }

    if let Some(runtime_thread) = thread {
        let _ = runtime_thread.shutdown();
    }
}

#[allow(clippy::too_many_arguments)]
fn ensure_hosted_thread(
    thread: &mut Option<RuntimeThreadHandle>,
    host: &RuntimeHostHandle,
    config: &RunConfig,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    title: &str,
    mcp_registry: &orca_mcp::McpRegistry,
    pending_pinned_context: &mut Vec<String>,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), String> {
    if thread.is_none() {
        let transcript = preloaded.lock().unwrap().clone();
        let mut request = RuntimeThreadStartRequest::new(config.clone(), title)
            .with_mcp_registry(mcp_registry.clone());
        if let Some(transcript) = transcript {
            request = request.with_preloaded(transcript);
        }
        let started = host
            .start_thread_with_request(request)
            .map_err(|error| format!("failed to initialize conversation history: {error}"))?;
        *preloaded.lock().unwrap() = None;
        notify_recovered_background_approvals_for_tui(&started.task_registry(), event_tx);
        *thread = Some(started);
    }
    if let Some(runtime_thread) = thread.as_ref() {
        while let Some(context) = pending_pinned_context.first().cloned() {
            if let Err(error) =
                runtime_thread.mutate(RuntimeThreadMutation::AddPinnedContext(context))
            {
                return Err(error.to_string());
            }
            pending_pinned_context.remove(0);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_hosted_submitted_turn(
    submitted_turn: SubmittedTurn,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    thread: &mut Option<RuntimeThreadHandle>,
    pending_pinned_context: &mut Vec<String>,
    event_tx: &mpsc::Sender<TuiEvent>,
    controller: &TuiOperationController,
    _pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
    mcp_registry: &orca_mcp::McpRegistry,
    host: &RuntimeHostHandle,
) {
    let rejection_prompt = submitted_turn.rejection_prompt().map(str::to_string);
    let queued_id = submitted_turn.queued_id();
    let cfg = config.lock().unwrap().clone();
    let cwd = cfg
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let title_seed = submitted_turn.title_seed(submitted_turn.prompt());
    if let Err(error) = ensure_hosted_thread(
        thread,
        host,
        &cfg,
        preloaded,
        &title_seed,
        mcp_registry,
        pending_pinned_context,
        event_tx,
    ) {
        send_submission_error(event_tx, queued_id, rejection_prompt.as_deref(), error);
        return;
    }
    let runtime_thread = thread.as_ref().expect("hosted thread initialized");
    let workspace_roots = cfg
        .runtime_workspace_roots
        .clone()
        .filter(|roots| !roots.is_empty())
        .unwrap_or_else(|| vec![cwd.clone()]);
    let prompt = match submitted_turn.prompt_for_model(
        &cwd,
        &workspace_roots,
        &runtime_thread.mcp_registry(),
    ) {
        Ok(prompt) => prompt,
        Err(error) => {
            send_submission_error(event_tx, queued_id, rejection_prompt.as_deref(), error);
            return;
        }
    };
    run_hosted_goal_run(
        &cfg,
        runtime_thread,
        submitted_turn.with_model_prompt(prompt),
        orca_core::goal_runtime::GoalTurnOrigin::User,
        event_tx,
        controller,
        queued_id,
    );
    if cfg.desktop_notifications {
        let _ = orca_runtime::notify::notify("Orca", "Task completed");
    }
}

fn run_hosted_operation(
    thread: &RuntimeThreadHandle,
    request: HostedTurnRequest,
    config: RunConfig,
    controller: &TuiOperationController,
    event_tx: &mpsc::Sender<TuiEvent>,
    queued_id: Option<u64>,
) -> io::Result<TuiHostedOperationOutcome> {
    let operation_kind = request.operation_kind().clone();
    let observer = Arc::new(TuiHostedEventObserver::new_with_queued_id(
        event_tx.clone(),
        queued_id,
    ));
    let pending_interactions =
        orca_runtime::runtime_pending_interaction::RuntimePendingInteractionStore::default();
    let generation_pending_interactions = pending_interactions.clone();
    let generation_controller = controller.clone();
    let generation_event_tx = event_tx.clone();
    let request = request
        .with_pending_interactions(pending_interactions)
        .with_event_observer(observer.clone())
        .with_generation_handlers(move |fence, cancel| {
            let control = TuiTurnControl::for_generation(
                generation_controller.clone(),
                fence.operation_id(),
                cancel,
            );
            HostedGenerationHandlers::default()
                .with_provider_suspension_control(Arc::new(control.clone()))
                .with_approval_handler(Arc::new(
                    TuiApprovalHandler::new(generation_event_tx.clone(), control.clone())
                        .with_pending_interactions(generation_pending_interactions.clone()),
                ))
                .with_permission_handler(Arc::new(
                    TuiPermissionRequestHandler::new(generation_event_tx.clone(), control.clone())
                        .with_pending_interactions(generation_pending_interactions.clone()),
                ))
                .with_user_input_handler(Arc::new(
                    TuiUserInputHandler::new(generation_event_tx.clone(), control.clone())
                        .with_pending_interactions(generation_pending_interactions.clone()),
                ))
                .with_mcp_elicitation_handler(Arc::new(
                    TuiMcpElicitationHandler::new(generation_event_tx.clone(), control)
                        .with_pending_interactions(generation_pending_interactions.clone()),
                ))
        });
    let rejection_prompt = queued_id.map(|_| request.prompt().to_string());
    let operation = match thread.start_turn_with_config(request, io::sink(), config) {
        Ok(operation) => Arc::new(operation),
        Err(error) => {
            if let Some(id) = queued_id {
                let _ = event_tx.send(TuiEvent::SubmissionRejected {
                    queued_id: Some(id),
                    prompt: rejection_prompt.unwrap_or_default(),
                    message: error.to_string(),
                });
            } else {
                send_hosted_operation_terminal_failure(event_tx, &operation_kind);
            }
            return Err(io::Error::other(error.to_string()));
        }
    };
    let operation_id = operation.id();
    if let Err(error) = controller.install_hosted(Arc::clone(&operation)) {
        let _ = operation.interrupt();
        let _ = operation.wait();
        controller.complete_hosted(operation_id);
        if let Some(id) = queued_id
            && !observer.queued_submission_started()
        {
            let _ = event_tx.send(TuiEvent::SubmissionRejected {
                queued_id: Some(id),
                prompt: rejection_prompt.unwrap_or_default(),
                message: error.to_string(),
            });
        }
        let terminal_published = observer.finish_foreground().unwrap_or(false);
        if queued_id.is_none() && !terminal_published {
            send_hosted_operation_terminal_failure(event_tx, &operation_kind);
        }
        return Err(error);
    }
    let terminal = operation.wait();
    controller.complete_hosted(operation_id);
    let terminal_published = observer.finish_foreground()?;
    let outcome = match terminal.outcome() {
        OperationOutcome::Completed(status) => match operation_kind {
            HostedOperationKind::ManualCompaction => {
                Ok(TuiHostedOperationOutcome::ManualCompaction)
            }
            HostedOperationKind::Turn
            | HostedOperationKind::GoalRun
            | HostedOperationKind::BackgroundContinuation { .. } => {
                Ok(TuiHostedOperationOutcome::Turn {
                    status: status.as_str().to_string(),
                })
            }
        },
        OperationOutcome::Backgrounded { .. } => Ok(TuiHostedOperationOutcome::Turn {
            status: "backgrounded".to_string(),
        }),
        OperationOutcome::ExecutionFailed { message, .. }
        | OperationOutcome::Panicked { message } => Err(io::Error::other(message.clone())),
    };
    if outcome.is_err() && !terminal_published {
        send_hosted_operation_terminal_failure(event_tx, &operation_kind);
    }
    outcome
}

fn send_hosted_action_failure(event_tx: &mpsc::Sender<TuiEvent>, message: String) {
    let _ = event_tx.send(TuiEvent::OperationRejected(message));
}

fn send_hosted_operation_terminal_failure(
    event_tx: &mpsc::Sender<TuiEvent>,
    _operation_kind: &HostedOperationKind,
) {
    let _ = event_tx.send(TuiEvent::SessionCompleted {
        status: "failed".to_string(),
    });
}

fn run_hosted_goal_run(
    config: &RunConfig,
    thread: &RuntimeThreadHandle,
    submitted_turn: SubmittedTurn,
    origin: orca_core::goal_runtime::GoalTurnOrigin,
    event_tx: &mpsc::Sender<TuiEvent>,
    controller: &TuiOperationController,
    queued_id: Option<u64>,
) {
    let rejection_prompt = submitted_turn.rejection_prompt().map(str::to_string);
    let Some(session_id) = thread.session_id().map(str::to_string) else {
        if queued_id.is_some() {
            send_submission_error(
                event_tx,
                queued_id,
                rejection_prompt.as_deref(),
                goal_history_error_message().to_string(),
            );
        } else {
            send_goal_history_error(event_tx);
        }
        return;
    };
    let runtime = match thread.goal_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            if queued_id.is_some() {
                send_submission_error(
                    event_tx,
                    queued_id,
                    rejection_prompt.as_deref(),
                    error.to_string(),
                );
            } else {
                let _ = event_tx.send(TuiEvent::Error(error.to_string()));
            }
            return;
        }
    };
    let active_goal = match runtime.project_thread_goal(&session_id) {
        Ok(goal) => goal.filter(|goal| goal.status.should_continue()),
        Err(error) => {
            if queued_id.is_some() {
                send_submission_error(
                    event_tx,
                    queued_id,
                    rejection_prompt.as_deref(),
                    error.to_string(),
                );
            } else {
                let _ = event_tx.send(TuiEvent::Error(error.to_string()));
            }
            return;
        }
    };
    if let Some(goal) = active_goal.as_ref() {
        let _ = event_tx.send(TuiEvent::GoalStatus(Some(goal.clone())));
    }
    let request = hosted_turn_request(&submitted_turn, active_goal.is_some());
    let request = if active_goal.is_some() {
        request
            .with_operation_kind(HostedOperationKind::GoalRun)
            .with_goal_turn_origin(origin)
    } else {
        request
    };
    let status = match run_hosted_operation(
        thread,
        request,
        config.clone(),
        controller,
        event_tx,
        queued_id,
    ) {
        Ok(TuiHostedOperationOutcome::Turn { status }) => status,
        Ok(TuiHostedOperationOutcome::ManualCompaction) => {
            let _ = event_tx.send(TuiEvent::Error(
                "goal run returned a compaction result".to_string(),
            ));
            return;
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error.to_string()));
            return;
        }
    };
    match runtime.project_thread_goal(&session_id) {
        Ok(Some(goal)) => {
            let _ = event_tx.send(TuiEvent::GoalStatus(Some(goal.clone())));
            let _ = event_tx.send(TuiEvent::GoalUpdated(goal.clone()));
            if status != "success" || !goal.status.should_continue() {
                let notice = match runtime.read(&session_id) {
                    Ok(Some(record)) => match record.state {
                        orca_core::goal_runtime::GoalState::Paused {
                            reason: orca_core::goal_runtime::GoalPauseReason::NoProgress,
                            ..
                        } => "Goal paused because the last turns made no measurable progress. Use /goal resume to continue.".to_string(),
                        _ => format!(
                            "Goal run stopped with status `{status}` while the goal is {}.",
                            orca_core::goal_types::goal_status_label(goal.status)
                        ),
                    },
                    _ => format!(
                        "Goal run stopped with status `{status}` while the goal is {}.",
                        orca_core::goal_types::goal_status_label(goal.status)
                    ),
                };
                let _ = event_tx.send(TuiEvent::Notice(notice));
            }
        }
        Ok(None) => {
            let _ = event_tx.send(TuiEvent::GoalStatus(None));
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error.to_string()));
        }
    }
}

fn hosted_turn_request(
    submitted_turn: &SubmittedTurn,
    goal_mode_active: bool,
) -> HostedTurnRequest {
    HostedTurnRequest::new(submitted_turn.prompt().to_string())
        .with_goal_tools(goal_mode_active)
        .with_goal_usage_tracking(goal_mode_active)
        .with_backtrack_target(submitted_turn.is_backtrack_target())
        .with_task_description(
            submitted_turn
                .task_label()
                .unwrap_or_else(|| submitted_turn.prompt()),
        )
}

fn current_hosted_goal_session_id(
    thread: Option<&RuntimeThreadHandle>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
) -> Option<String> {
    thread
        .and_then(RuntimeThreadHandle::session_id)
        .map(str::to_string)
        .or_else(|| {
            preloaded
                .lock()
                .unwrap()
                .as_ref()
                .map(|transcript| transcript.meta.session_id.clone())
        })
}

fn existing_hosted_goal_session_id(
    thread: Option<&RuntimeThreadHandle>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    config: &Arc<Mutex<RunConfig>>,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Option<String> {
    if let Some(session_id) = current_hosted_goal_session_id(thread, preloaded) {
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

fn show_hosted_goal(
    thread: &Option<RuntimeThreadHandle>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    config: &Arc<Mutex<RunConfig>>,
    event_tx: &mpsc::Sender<TuiEvent>,
) {
    let Some(session_id) = current_hosted_goal_session_id(thread.as_ref(), preloaded) else {
        if matches!(config.lock().unwrap().history_mode, HistoryMode::Disabled) {
            send_goal_history_error(event_tx);
        } else {
            let _ = event_tx.send(TuiEvent::GoalStatus(None));
        }
        return;
    };
    let mut detached_join = None;
    let runtime = match thread.as_ref() {
        Some(thread) => thread.goal_runtime().map_err(|error| error.to_string()),
        None => orca_runtime::goal_actor::GoalRuntimeHandle::open_default()
            .map(|(runtime, join)| {
                detached_join = Some(join);
                runtime
            })
            .map_err(|error| error.to_string()),
    };
    let result = runtime.and_then(|runtime| {
        let result = runtime
            .project_thread_goal(&session_id)
            .map_err(|error| error.to_string());
        drop(runtime);
        result
    });
    if let Some(join) = detached_join {
        let _ = join.join();
    }
    match result {
        Ok(goal) => {
            let _ = event_tx.send(TuiEvent::GoalStatus(goal));
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!("failed to read goal: {error}")));
        }
    }
}

fn send_goal_history_error(event_tx: &mpsc::Sender<TuiEvent>) {
    let _ = event_tx.send(TuiEvent::Error(goal_history_error_message().to_string()));
}

fn goal_history_error_message() -> &'static str {
    "persistent goals require recorded history; enable history before using /goal"
}

#[allow(clippy::too_many_arguments)]
fn resume_latest_active_goal_hosted(
    thread: &mut Option<RuntimeThreadHandle>,
    host: &RuntimeHostHandle,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    mcp_registry: &orca_mcp::McpRegistry,
    event_tx: &mpsc::Sender<TuiEvent>,
    controller: &TuiOperationController,
    _pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
) {
    if matches!(config.lock().unwrap().history_mode, HistoryMode::Disabled) {
        send_goal_history_error(event_tx);
        return;
    }
    let (goal_runtime, _goal_actor_join) =
        match orca_runtime::goal_actor::GoalRuntimeHandle::open_default() {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = event_tx.send(TuiEvent::Error(format!("failed to read goals: {error}")));
                return;
            }
        };
    let goal = match goal_runtime.latest_active() {
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
    let request = RuntimeThreadStartRequest::new(cfg.clone(), &goal.objective)
        .with_preloaded(transcript)
        .with_mcp_registry(mcp_registry.clone());
    let resumed = match host.start_thread_with_request(request) {
        Ok(thread) => thread,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "failed to initialize resumed goal session: {error}"
            )));
            return;
        }
    };
    let Some(new_session_id) = resumed.session_id().map(str::to_string) else {
        send_goal_history_error(event_tx);
        let _ = resumed.shutdown();
        return;
    };
    let active_goal =
        match goal_runtime.resume_into(&goal.session_id, &new_session_id, now_timestamp()) {
            Ok(Some(_)) => match resumed
                .goal_runtime()
                .ok()
                .and_then(|runtime| runtime.project_thread_goal(&new_session_id).ok().flatten())
            {
                Some(goal) => goal,
                None => {
                    let _ = event_tx.send(TuiEvent::Error(
                        "goal disappeared while projecting the resumed session".to_string(),
                    ));
                    let _ = resumed.shutdown();
                    return;
                }
            },
            Ok(None) => {
                let _ = event_tx.send(TuiEvent::Error(
                    "goal disappeared while restoring its session".to_string(),
                ));
                let _ = resumed.shutdown();
                return;
            }
            Err(error) => {
                let _ = event_tx.send(TuiEvent::Error(format!(
                    "failed to resume goal in restored session: {error}"
                )));
                let _ = resumed.shutdown();
                return;
            }
        };
    if let Some(previous) = thread.take() {
        let _ = previous.shutdown();
    }
    notify_recovered_background_approvals_for_tui(&resumed.task_registry(), event_tx);
    *thread = Some(resumed);
    *preloaded.lock().unwrap() = None;
    if let Ok(mut shared) = config.lock() {
        shared.history_mode = cfg.history_mode.clone();
    }
    let _ = event_tx.send(TuiEvent::GoalUpdated(active_goal.clone()));
    let _ = event_tx.send(TuiEvent::Notice(
        "Resumed latest active goal in a restored session.".to_string(),
    ));
    if let Some(runtime_thread) = thread.as_ref() {
        run_hosted_goal_run(
            &cfg,
            runtime_thread,
            SubmittedTurn::user(goal_continuation_prompt(&active_goal.objective, 1)),
            orca_core::goal_runtime::GoalTurnOrigin::Resume,
            event_tx,
            controller,
            None,
        );
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
