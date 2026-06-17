use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tui_textarea::{CursorMove, Input, TextArea};

use crate::config::file::save_api_key;
use crate::config::{HistoryMode, RunConfig};
use crate::runtime::cancel::CancelToken;
use crate::runtime::history;
use crate::tui::bridge;
use crate::tui::commands::{self, SlashCommand};
use crate::tui::mentions;
use crate::tui::shortcuts::{
    ApprovalShortcut, GlobalShortcut, IdleShortcut, RunningShortcut, approval_shortcut,
    global_shortcut, idle_shortcut, running_shortcut,
};
use crate::tui::types::{AppState, AppStatus, ChatMessage, TuiEvent, UserAction};
use crate::tui::ui;

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
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (event_tx, event_rx) = mpsc::channel::<TuiEvent>();
    let (action_tx, action_rx) = mpsc::channel::<UserAction>();

    let model_name = config
        .model
        .clone()
        .unwrap_or_else(|| "deepseek-v4-flash".to_string());

    let needs_setup = config.api_key.is_none();
    let should_show_picker = config.show_session_picker
        && !needs_setup
        && config.prompt.trim().is_empty()
        && !matches!(
            config.history_mode,
            HistoryMode::Resume(_) | HistoryMode::Fork(_)
        );
    let picker_sessions = if should_show_picker {
        crate::runtime::history::list_sessions(20).unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut state = AppState::new(action_tx.clone(), model_name);
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

    if matches!(
        config.history_mode,
        HistoryMode::Resume(_) | HistoryMode::Fork(_)
    ) {
        if let Ok(transcript) = crate::runtime::history::load_session(match &config.history_mode {
            HistoryMode::Resume(selector) | HistoryMode::Fork(selector) => selector,
            HistoryMode::Record | HistoryMode::Disabled => "",
        }) {
            for message in transcript.messages {
                if let Some(chat_message) = chat_message_from_history(message) {
                    state.messages.push(chat_message);
                }
            }
            if !state.messages.is_empty() {
                let label = if matches!(config.history_mode, HistoryMode::Fork(_)) {
                    "Forked saved conversation."
                } else {
                    "Resumed saved conversation."
                };
                state.messages.push(ChatMessage::System(label.to_string()));
            }
        }
    }

    let shared_config = Arc::new(Mutex::new(config.clone()));
    let agent_config = Arc::clone(&shared_config);
    let preloaded_transcript: Arc<Mutex<Option<history::SessionTranscript>>> =
        Arc::new(Mutex::new(None));
    let agent_preloaded = Arc::clone(&preloaded_transcript);
    let agent_event_tx = event_tx.clone();
    let cancel_token = CancelToken::new();
    let agent_cancel = cancel_token.clone();

    let _agent_handle = thread::spawn(move || {
        agent_loop_thread(
            agent_config,
            agent_preloaded,
            agent_event_tx,
            action_rx,
            agent_cancel,
        );
    });

    let mut textarea = if needs_setup {
        make_setup_textarea()
    } else {
        if let Some(prompt) = initial_prompt.clone() {
            state.messages.push(ChatMessage::User(prompt.clone()));
            state.status = AppStatus::Running;
            let _ = action_tx.send(UserAction::Submit(prompt));
        }
        make_textarea()
    };

    let exit_code;

    loop {
        terminal.draw(|f| ui::render(f, &mut state, &textarea))?;

        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;

            if let Event::Key(key) = &ev {
                if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    continue;
                }

                if let Some(shortcut) = global_shortcut(*key) {
                    match shortcut {
                        GlobalShortcut::Cancel => {
                            if state.status == AppStatus::Running {
                                cancel_token.cancel();
                                let _ = action_tx.send(UserAction::Interrupt);
                                continue;
                            }
                            let now = Instant::now();
                            if state
                                .last_ctrl_c
                                .is_some_and(|t| now.duration_since(t) < Duration::from_secs(2))
                            {
                                let _ = action_tx.send(UserAction::Cancel);
                                exit_code = 130;
                                break;
                            }
                            state.last_ctrl_c = Some(now);
                            state
                                .messages
                                .push(ChatMessage::System("Press Ctrl+C again to quit.".into()));
                            state.scroll_to_bottom();
                            continue;
                        }
                        GlobalShortcut::ToggleShortcuts => {
                            state.toggle_shortcuts();
                            continue;
                        }
                        GlobalShortcut::ScrollBottom => {
                            state.scroll_to_bottom();
                            continue;
                        }
                        GlobalShortcut::ScrollTop => {
                            state.scroll_to_top();
                            continue;
                        }
                        GlobalShortcut::ClearScreen => {
                            state.messages.clear();
                            state.scroll_offset = 0;
                            state.auto_scroll = true;
                            continue;
                        }
                    }
                }

                if state.show_shortcuts && key.code == KeyCode::Esc {
                    state.show_shortcuts = false;
                    continue;
                }

                // Setup mode: step-by-step
                if state.status == AppStatus::Setup {
                    match state.setup_step {
                        0 => {
                            // Welcome screen — Enter to continue, Esc to quit
                            match key.code {
                                KeyCode::Enter => {
                                    state.setup_step = 1;
                                    textarea = make_setup_textarea();
                                }
                                KeyCode::Esc => {
                                    exit_code = 0;
                                    break;
                                }
                                _ => {}
                            }
                        }
                        1 => {
                            // API key input
                            match key.code {
                                KeyCode::Enter => {
                                    let lines: Vec<String> = textarea.lines().to_vec();
                                    let key_input = lines.join("").trim().to_string();
                                    if !key_input.is_empty() {
                                        save_api_key(&key_input);
                                        config.api_key = Some(key_input.clone());
                                        if let Ok(mut cfg) = shared_config.lock() {
                                            cfg.api_key = Some(key_input);
                                        }
                                        state.setup_step = 2;
                                    }
                                }
                                KeyCode::Esc => {
                                    exit_code = 0;
                                    break;
                                }
                                _ => {
                                    textarea.input(Input::from(ev));
                                }
                            }
                        }
                        2 => {
                            // Completion screen — Enter to start
                            match key.code {
                                KeyCode::Enter => {
                                    state.status = AppStatus::Idle;
                                    state.setup_step = 0;
                                    textarea = make_textarea();

                                    if let Some(prompt) = initial_prompt.clone() {
                                        state.messages.push(ChatMessage::User(prompt.clone()));
                                        state.status = AppStatus::Running;
                                        let _ = action_tx.send(UserAction::Submit(prompt));
                                    }
                                }
                                KeyCode::Esc => {
                                    exit_code = 0;
                                    break;
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if state.status == AppStatus::SessionPicker {
                    match key.code {
                        KeyCode::Up => state.select_previous_session(),
                        KeyCode::Down => state.select_next_session(),
                        KeyCode::Char('n') | KeyCode::Char('N') => {
                            state.status = AppStatus::Idle;
                            state.session_picker_sessions.clear();
                            config.history_mode = HistoryMode::Record;
                            if let Ok(mut cfg) = shared_config.lock() {
                                cfg.history_mode = HistoryMode::Record;
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(session_id) = state.selected_session_id() {
                                config.history_mode = HistoryMode::Resume(session_id.clone());
                                if let Ok(mut cfg) = shared_config.lock() {
                                    cfg.history_mode = HistoryMode::Resume(session_id.clone());
                                }
                                if let Ok(transcript) = history::load_session(&session_id) {
                                    state.messages.clear();
                                    for message in &transcript.messages {
                                        if let Some(chat_message) =
                                            chat_message_from_history(message.clone())
                                        {
                                            state.messages.push(chat_message);
                                        }
                                    }
                                    state.messages.push(ChatMessage::System(
                                        "Resumed saved conversation.".to_string(),
                                    ));
                                    if let Ok(mut preloaded) = preloaded_transcript.lock() {
                                        *preloaded = Some(transcript);
                                    }
                                }
                                state.status = AppStatus::Idle;
                            }
                        }
                        KeyCode::Esc => {
                            exit_code = 0;
                            break;
                        }
                        _ => {}
                    }
                    continue;
                }

                // Approval dialog: selection and direct approve/deny shortcuts
                if state.status == AppStatus::WaitingApproval {
                    match approval_shortcut(*key) {
                        Some(ApprovalShortcut::SelectAllow) => {
                            if let Some(dialog) = &mut state.approval_dialog {
                                dialog.selected = 0;
                            }
                        }
                        Some(ApprovalShortcut::SelectDeny) => {
                            if let Some(dialog) = &mut state.approval_dialog {
                                dialog.selected = 1;
                            }
                        }
                        Some(ApprovalShortcut::ToggleSelection) => {
                            if let Some(dialog) = &mut state.approval_dialog {
                                dialog.selected = 1usize.saturating_sub(dialog.selected);
                            }
                        }
                        Some(ApprovalShortcut::Confirm) => {
                            let approved = state
                                .approval_dialog
                                .as_ref()
                                .map(|d| d.selected == 0)
                                .unwrap_or(false);
                            resolve_approval(&mut state, &action_tx, approved);
                        }
                        Some(ApprovalShortcut::Approve) => {
                            resolve_approval(&mut state, &action_tx, true);
                        }
                        Some(ApprovalShortcut::Deny) => {
                            resolve_approval(&mut state, &action_tx, false);
                        }
                        None => {}
                    }
                    continue;
                }

                // Normal Idle mode input
                if state.status == AppStatus::Idle {
                    match idle_shortcut(*key) {
                        Some(IdleShortcut::Submit) => {
                            let lines: Vec<String> = textarea.lines().to_vec();
                            let text = lines.join("\n").trim().to_string();
                            if !text.is_empty() {
                                if let Some(outcome) = handle_slash_command(
                                    &text,
                                    &mut config,
                                    &shared_config,
                                    &mut state,
                                    &action_tx,
                                ) {
                                    match outcome {
                                        SlashOutcome::Continue => {
                                            textarea = make_textarea();
                                            continue;
                                        }
                                        SlashOutcome::Exit(code) => {
                                            exit_code = code;
                                            break;
                                        }
                                    }
                                }
                                state.record_prompt(text.clone());
                                state.messages.push(ChatMessage::User(text.clone()));
                                state.status = AppStatus::Running;
                                state.scroll_to_bottom();
                                let _ = action_tx.send(UserAction::Submit(text));
                                textarea = make_textarea();
                            }
                        }
                        Some(IdleShortcut::Newline) => {
                            textarea.insert_newline();
                            state.reset_history_navigation();
                        }
                        Some(IdleShortcut::HistoryPrevious) => {
                            let draft = textarea_text(&textarea);
                            if let Some(history) = state.history_previous(draft) {
                                textarea = make_textarea_with_text(&history);
                            }
                        }
                        Some(IdleShortcut::HistoryNext) => {
                            if let Some(history) = state.history_next() {
                                textarea = make_textarea_with_text(&history);
                            }
                        }
                        Some(IdleShortcut::ScrollUp) => {
                            if textarea.lines().len() > 1 {
                                textarea.input(Input::from(ev));
                            } else {
                                state.scroll_up(1);
                            }
                        }
                        Some(IdleShortcut::ScrollDown) => {
                            if textarea.lines().len() > 1 {
                                textarea.input(Input::from(ev));
                            } else {
                                state.scroll_down(1);
                            }
                        }
                        Some(IdleShortcut::PageUp) => {
                            let page = state.visible_height.saturating_sub(2);
                            state.scroll_up(page);
                        }
                        Some(IdleShortcut::PageDown) => {
                            let page = state.visible_height.saturating_sub(2);
                            state.scroll_down(page);
                        }
                        Some(IdleShortcut::HalfPageUp) => {
                            let page = state.visible_height / 2;
                            state.scroll_up(page);
                        }
                        Some(IdleShortcut::HalfPageDown) => {
                            let page = state.visible_height / 2;
                            state.scroll_down(page);
                        }
                        Some(IdleShortcut::Backtrack) => {
                            let _ = action_tx.send(UserAction::Backtrack);
                        }
                        None => {
                            if textarea.input(Input::from(ev)) {
                                state.reset_history_navigation();
                            }
                        }
                    }
                } else if state.status == AppStatus::Running {
                    match running_shortcut(*key) {
                        Some(RunningShortcut::Interrupt) => {
                            cancel_token.cancel();
                            let _ = action_tx.send(UserAction::Interrupt);
                        }
                        Some(RunningShortcut::ScrollUp) => {
                            state.scroll_up(1);
                        }
                        Some(RunningShortcut::ScrollDown) => {
                            state.scroll_down(1);
                        }
                        Some(RunningShortcut::PageUp) => {
                            let page = state.visible_height.saturating_sub(2);
                            state.scroll_up(page);
                        }
                        Some(RunningShortcut::PageDown) => {
                            let page = state.visible_height.saturating_sub(2);
                            state.scroll_down(page);
                        }
                        Some(RunningShortcut::HalfPageUp) => {
                            let page = state.visible_height / 2;
                            state.scroll_up(page);
                        }
                        Some(RunningShortcut::HalfPageDown) => {
                            let page = state.visible_height / 2;
                            state.scroll_down(page);
                        }
                        None => {}
                    }
                }
            }
        }

        while let Ok(tui_event) = event_rx.try_recv() {
            let backtracked_prompt = match &tui_event {
                TuiEvent::Backtracked { prompt } => Some(prompt.clone()),
                _ => None,
            };
            state.update(tui_event);
            if let Some(prompt) = backtracked_prompt {
                textarea = make_textarea_with_text(&prompt);
            }
            if state.auto_scroll {
                state.scroll_to_bottom();
            }
        }
    }

    terminal::disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    Ok(exit_code)
}

fn make_textarea<'a>() -> TextArea<'a> {
    let mut textarea = TextArea::default();
    configure_textarea(&mut textarea);
    textarea
}

fn make_textarea_with_text<'a>(text: &str) -> TextArea<'a> {
    let lines: Vec<String> = if text.is_empty() {
        vec![String::new()]
    } else {
        text.lines().map(str::to_string).collect()
    };
    let mut textarea = TextArea::from(lines);
    configure_textarea(&mut textarea);
    textarea.move_cursor(CursorMove::Bottom);
    textarea.move_cursor(CursorMove::End);
    textarea
}

fn configure_textarea(textarea: &mut TextArea) {
    textarea.set_placeholder_text("Type a message... (Enter send, shift+Enter newline)");
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    textarea.set_block(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .title(" Input "),
    );
}

fn textarea_text(textarea: &TextArea) -> String {
    textarea.lines().join("\n")
}

fn make_setup_textarea<'a>() -> TextArea<'a> {
    let mut textarea = TextArea::default();
    textarea.set_placeholder_text("sk-...");
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    textarea.set_mask_char('*');
    textarea.set_block(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .title(" API Key ")
            .border_style(ratatui::style::Style::default().fg(ratatui::style::Color::Cyan)),
    );
    textarea
}

fn resolve_approval(state: &mut AppState, action_tx: &mpsc::Sender<UserAction>, approved: bool) {
    let _ = action_tx.send(UserAction::Approve(approved));
    if approved {
        state.status = AppStatus::Running;
    } else {
        state.status = AppStatus::Idle;
    }
    state.approval_dialog = None;
}

fn agent_loop_thread(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
    cancel: CancelToken,
) {
    let mut session: Option<bridge::TuiConversationSession> = None;

    loop {
        match action_rx.recv() {
            Ok(UserAction::Submit(prompt)) => {
                cancel.reset();
                let cfg = config.lock().unwrap().clone();
                let cwd = cfg
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                let prompt = match mentions::expand_file_mentions(&prompt, &cwd) {
                    Ok(prompt) => prompt,
                    Err(error) => {
                        let _ = event_tx.send(TuiEvent::Error(error));
                        continue;
                    }
                };
                if session.is_none() {
                    let transcript = preloaded.lock().unwrap().take();
                    session = match bridge::TuiConversationSession::new_with_preloaded(
                        &cfg, &prompt, transcript,
                    ) {
                        Ok(session) => Some(session),
                        Err(error) => {
                            let _ = event_tx.send(TuiEvent::Error(format!(
                                "failed to initialize conversation history: {error}"
                            )));
                            continue;
                        }
                    };
                }
                bridge::run_agent_for_tui(
                    &cfg,
                    session.as_mut().expect("session initialized"),
                    &prompt,
                    &event_tx,
                    &action_rx,
                    &cancel,
                );
            }
            Ok(UserAction::Interrupt) => {
                // Cancel already set by TUI thread; just continue waiting for next Submit
            }
            Ok(UserAction::SetModel(model)) => {
                if let Some(session) = session.as_mut() {
                    session.set_model(Some(&model));
                }
            }
            Ok(UserAction::Compact) => {
                if let Some(session) = session.as_mut() {
                    let cfg = config.lock().unwrap().clone();
                    let cwd = cfg
                        .cwd
                        .clone()
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                    let (before_messages, after_messages) = session.compact(&cwd);
                    let _ = event_tx.send(TuiEvent::Compacted {
                        before_messages,
                        after_messages,
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
            Ok(UserAction::Cancel) | Err(_) => break,
            Ok(UserAction::Approve(_)) => {}
        }
    }
}

enum SlashOutcome {
    Continue,
    Exit(i32),
}

fn handle_slash_command(
    text: &str,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> Option<SlashOutcome> {
    let command = commands::parse(text)?;
    match command {
        SlashCommand::Help => {
            state.messages.push(ChatMessage::System(
                "/help /model <name> /compact /clear /cost /history /mode <suggest|auto-edit|full-auto> /plan [off] /remember <note> /exit"
                    .to_string(),
            ));
        }
        SlashCommand::Model(model) => match commands::validate_model(&model) {
            Ok(()) => {
                config.model = Some(model.clone());
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.model = Some(model.clone());
                }
                state.model_name = model.clone();
                state
                    .messages
                    .push(ChatMessage::System(format!("Model switched to {model}.")));
                let _ = action_tx.send(UserAction::SetModel(model));
            }
            Err(error) => state.messages.push(ChatMessage::Error(error)),
        },
        SlashCommand::Clear => {
            state.messages.clear();
            state.scroll_offset = 0;
            state.auto_scroll = true;
        }
        SlashCommand::Cost => {
            state.messages.push(ChatMessage::System(format!(
                "Session usage: {} input, {} output, {} cache tokens, estimated ${:.6}.",
                state.usage.input_tokens,
                state.usage.output_tokens,
                state.usage.cache_tokens,
                state.usage.estimated_cost_usd
            )));
        }
        SlashCommand::Mode(mode) => match parse_approval_mode(&mode) {
            Some(approval_mode) => {
                config.approval_mode = approval_mode;
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.approval_mode = approval_mode;
                }
                state.messages.push(ChatMessage::System(format!(
                    "Approval mode switched to {mode}."
                )));
            }
            None => state.messages.push(ChatMessage::Error(
                "unsupported mode. Use suggest, auto-edit, or full-auto.".to_string(),
            )),
        },
        SlashCommand::Plan(arg) => match arg.as_deref() {
            Some("off") => {
                config.approval_mode = crate::approval::policy::ApprovalMode::Suggest;
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.approval_mode = crate::approval::policy::ApprovalMode::Suggest;
                }
                state
                    .messages
                    .push(ChatMessage::System("Plan mode disabled.".to_string()));
            }
            None => {
                config.approval_mode = crate::approval::policy::ApprovalMode::Plan;
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.approval_mode = crate::approval::policy::ApprovalMode::Plan;
                }
                state
                    .messages
                    .push(ChatMessage::System("Plan mode enabled.".to_string()));
            }
            Some(_) => state.messages.push(ChatMessage::Error(
                "unsupported plan command. Use /plan or /plan off.".to_string(),
            )),
        },
        SlashCommand::Remember(note) => {
            let result = if let Some(project_note) = note.strip_prefix("project:") {
                let cwd = config
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                crate::runtime::memory::remember_project(&cwd, project_note)
            } else {
                crate::runtime::memory::remember_user(&note)
            };
            match result {
                Ok(path) => state.messages.push(ChatMessage::System(format!(
                    "Remembered in {}.",
                    path.display()
                ))),
                Err(error) => state
                    .messages
                    .push(ChatMessage::Error(format!("failed to remember: {error}"))),
            }
        }
        SlashCommand::Compact => {
            state.status = AppStatus::Running;
            let _ = action_tx.send(UserAction::Compact);
        }
        SlashCommand::History => match history::list_sessions(10) {
            Ok(sessions) if sessions.is_empty() => state
                .messages
                .push(ChatMessage::System("No saved sessions.".to_string())),
            Ok(sessions) => {
                let summary = sessions
                    .into_iter()
                    .map(|session| {
                        format!(
                            "{}  {}  {}",
                            session.session_id,
                            session.updated_at.format("%Y-%m-%d %H:%M"),
                            session.title
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                state
                    .messages
                    .push(ChatMessage::System(format!("Recent sessions:\n{summary}")));
            }
            Err(error) => state.messages.push(ChatMessage::Error(format!(
                "failed to list history: {error}"
            ))),
        },
        SlashCommand::Exit => return Some(SlashOutcome::Exit(0)),
    }
    state.scroll_to_bottom();
    Some(SlashOutcome::Continue)
}

fn parse_approval_mode(mode: &str) -> Option<crate::approval::policy::ApprovalMode> {
    match mode {
        "suggest" => Some(crate::approval::policy::ApprovalMode::Suggest),
        "auto-edit" => Some(crate::approval::policy::ApprovalMode::AutoEdit),
        "full-auto" => Some(crate::approval::policy::ApprovalMode::FullAuto),
        "plan" => Some(crate::approval::policy::ApprovalMode::Plan),
        _ => None,
    }
}

fn chat_message_from_history(
    message: crate::provider::conversation::Message,
) -> Option<ChatMessage> {
    use crate::provider::conversation::Message;

    match message {
        Message::System(_) => None,
        Message::User(content) => Some(ChatMessage::User(content)),
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
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
        } => Some(ChatMessage::ToolCall {
            name: format!("tool:{tool_call_id}"),
            target: None,
            status: "completed".to_string(),
            output: Some(content),
            diff: None,
        }),
    }
}
