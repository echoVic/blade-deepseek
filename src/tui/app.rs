use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tui_textarea::{Input, TextArea};

use crate::config::RunConfig;
use crate::config::file::save_api_key;
use crate::runtime::cancel::CancelToken;
use crate::tui::bridge;
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

    let mut state = AppState::new(action_tx.clone(), model_name);

    let needs_setup = config.api_key.is_none();
    if needs_setup {
        state.status = AppStatus::Setup;
        state.setup_step = 0;
    }

    let initial_prompt = if config.prompt.trim().is_empty() {
        None
    } else {
        Some(config.prompt.clone())
    };

    let shared_config = Arc::new(Mutex::new(config.clone()));
    let agent_config = Arc::clone(&shared_config);
    let agent_event_tx = event_tx.clone();
    let cancel_token = CancelToken::new();
    let agent_cancel = cancel_token.clone();

    let _agent_handle = thread::spawn(move || {
        agent_loop_thread(agent_config, agent_event_tx, action_rx, agent_cancel);
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
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    let _ = action_tx.send(UserAction::Cancel);
                    exit_code = 130;
                    break;
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

                // Approval dialog: Up/Down/Enter
                if state.status == AppStatus::WaitingApproval {
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            if let Some(dialog) = &mut state.approval_dialog {
                                dialog.selected = 0;
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if let Some(dialog) = &mut state.approval_dialog {
                                dialog.selected = 1;
                            }
                        }
                        KeyCode::Enter => {
                            let approved = state
                                .approval_dialog
                                .as_ref()
                                .map(|d| d.selected == 0)
                                .unwrap_or(false);
                            let _ = action_tx.send(UserAction::Approve(approved));
                            if approved {
                                state.status = AppStatus::Running;
                            } else {
                                state.status = AppStatus::Idle;
                            }
                            state.approval_dialog = None;
                        }
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            let _ = action_tx.send(UserAction::Approve(true));
                            state.status = AppStatus::Running;
                            state.approval_dialog = None;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') => {
                            let _ = action_tx.send(UserAction::Approve(false));
                            state.status = AppStatus::Idle;
                            state.approval_dialog = None;
                        }
                        _ => {}
                    }
                    continue;
                }

                // Scrolling — works in Idle and Running
                match key.code {
                    KeyCode::PageUp => {
                        let page = state.visible_height.saturating_sub(2);
                        state.scroll_up(page);
                        continue;
                    }
                    KeyCode::PageDown => {
                        let page = state.visible_height.saturating_sub(2);
                        state.scroll_down(page);
                        continue;
                    }
                    _ => {}
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match key.code {
                        KeyCode::Char('u') => {
                            let page = state.visible_height / 2;
                            state.scroll_up(page);
                            continue;
                        }
                        KeyCode::Char('d') => {
                            let page = state.visible_height / 2;
                            state.scroll_down(page);
                            continue;
                        }
                        _ => {}
                    }
                }

                // Normal Idle mode input
                if state.status == AppStatus::Idle {
                    match key.code {
                        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                            let lines: Vec<String> = textarea.lines().to_vec();
                            let text = lines.join("\n").trim().to_string();
                            if !text.is_empty() {
                                state.messages.push(ChatMessage::User(text.clone()));
                                state.status = AppStatus::Running;
                                state.scroll_to_bottom();
                                let _ = action_tx.send(UserAction::Submit(text));
                                textarea = make_textarea();
                            }
                        }
                        KeyCode::Up => {
                            state.scroll_up(1);
                        }
                        KeyCode::Down => {
                            state.scroll_down(1);
                        }
                        KeyCode::Esc => {
                            exit_code = 0;
                            break;
                        }
                        _ => {
                            textarea.input(Input::from(ev));
                        }
                    }
                } else if state.status == AppStatus::Running {
                    // In running mode, Esc interrupts streaming, Up/Down scroll
                    match key.code {
                        KeyCode::Esc => {
                            cancel_token.cancel();
                            let _ = action_tx.send(UserAction::Interrupt);
                        }
                        KeyCode::Up => {
                            state.scroll_up(1);
                        }
                        KeyCode::Down => {
                            state.scroll_down(1);
                        }
                        _ => {}
                    }
                }
            }
        }

        while let Ok(tui_event) = event_rx.try_recv() {
            state.update(tui_event);
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
    textarea.set_placeholder_text("Type a message... (Enter to send, Esc to quit)");
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    textarea.set_block(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .title(" Input "),
    );
    textarea
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

fn agent_loop_thread(
    config: Arc<Mutex<RunConfig>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
    cancel: CancelToken,
) {
    loop {
        match action_rx.recv() {
            Ok(UserAction::Submit(prompt)) => {
                cancel.reset();
                let cfg = config.lock().unwrap().clone();
                bridge::run_agent_for_tui(&cfg, &prompt, &event_tx, &action_rx, &cancel);
            }
            Ok(UserAction::Interrupt) => {
                // Cancel already set by TUI thread; just continue waiting for next Submit
            }
            Ok(UserAction::Cancel) | Err(_) => break,
            Ok(UserAction::Approve(_)) => {}
        }
    }
}
