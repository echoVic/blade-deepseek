use crossbeam_channel as mpsc;
use std::sync::{Arc, Mutex};

use orca_core::approval_types::ApprovalMode;
use orca_core::config::RunConfig;
use orca_core::model::ModelSelection;
use orca_runtime::history;

use crate::commands::{
    self, DoctorSlashCommand, GoalSlashCommand, SlashCommand, TrustSlashCommand,
};
use crate::types::{AppState, ChatMessage, UserAction};

pub(crate) enum SlashOutcome {
    Continue,
}

pub(crate) fn handle_slash_command(
    text: &str,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> Option<SlashOutcome> {
    let command = if let Some(command) = commands::parse(text) {
        command
    } else if commands::has_builtin_name(text) {
        return None;
    } else {
        let configured_cwd = config.cwd.as_deref().map(std::path::Path::to_path_buf);
        parse_command_with_cwd(text, || {
            configured_cwd
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
        })?
    };
    match command {
        SlashCommand::Model(Some(model)) => match commands::validate_model(&model) {
            Ok(()) => {
                config.model = ModelSelection::from_unchecked(Some(model.clone()));
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.model = ModelSelection::from_unchecked(Some(model.clone()));
                }
                state.model_name = model.clone();
                state.push_message(ChatMessage::System(format!("Model switched to {model}.")));
                let _ = action_tx.send(UserAction::SetModel(model));
            }
            Err(error) => state.push_message(ChatMessage::Error(error)),
        },
        SlashCommand::Model(None) => {
            state.push_message(ChatMessage::System(format!(
                "Current model: {} (reasoning effort: {}). Use the /model menu to change both.",
                state.model_name,
                state.reasoning_effort.as_str()
            )));
        }
        SlashCommand::Cost => {
            state.push_message(ChatMessage::System(format!(
                "Session usage: {} input, {} output, {} cache tokens, estimated ${:.6}.",
                state.usage.input_tokens,
                state.usage.output_tokens,
                state.usage.cache_tokens,
                state.usage.estimated_cost_usd
            )));
        }
        SlashCommand::ConfigShow => {
            state.push_message(ChatMessage::System(orca_core::config::format_config_show(
                config,
            )));
        }
        SlashCommand::Doctor(command) => match command {
            DoctorSlashCommand::Report => {
                state.push_message(ChatMessage::System(
                    state.doctor_report(std::time::Instant::now()),
                ));
            }
            DoctorSlashCommand::ToggleFps => {
                let enabled = state.toggle_fps_hud();
                state.push_message(ChatMessage::System(fps_hud_message(enabled).to_string()));
            }
            DoctorSlashCommand::SetFps(enabled) => {
                state.set_fps_hud(enabled);
                state.push_message(ChatMessage::System(fps_hud_message(enabled).to_string()));
            }
        },
        SlashCommand::Mode(Some(mode)) => match parse_approval_mode(&mode) {
            Some(approval_mode) => {
                config.approval_mode = approval_mode;
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.approval_mode = approval_mode;
                }
                state.approval_mode = approval_mode;
                state.push_message(ChatMessage::System(format!(
                    "Approval mode switched to {mode}."
                )));
            }
            None => state.push_message(ChatMessage::Error(
                "unsupported mode. Use suggest, auto-edit, full-auto, or plan.".to_string(),
            )),
        },
        SlashCommand::Mode(None) => {
            state.push_message(ChatMessage::System(format!(
                "Current mode: {}",
                config.approval_mode.as_str()
            )));
        }
        SlashCommand::Plan(arg) => match arg.as_deref() {
            Some("off") => {
                config.approval_mode = ApprovalMode::Suggest;
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.approval_mode = ApprovalMode::Suggest;
                }
                state.approval_mode = ApprovalMode::Suggest;
                state.push_message(ChatMessage::System("Plan mode disabled.".to_string()));
            }
            None => {
                config.approval_mode = ApprovalMode::Plan;
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.approval_mode = ApprovalMode::Plan;
                }
                state.approval_mode = ApprovalMode::Plan;
                state.push_message(ChatMessage::System("Plan mode enabled.".to_string()));
            }
            Some(_) => state.push_message(ChatMessage::Error(
                "unsupported plan command. Use /plan or /plan off.".to_string(),
            )),
        },
        SlashCommand::Goal(goal_command) => {
            let action = match goal_command {
                GoalSlashCommand::Show => UserAction::GoalShow,
                GoalSlashCommand::Set(objective) => UserAction::GoalSet(objective),
                GoalSlashCommand::Edit(objective) => UserAction::GoalEdit(objective),
                GoalSlashCommand::Clear => UserAction::GoalClear,
                GoalSlashCommand::Pause => UserAction::GoalPause,
                GoalSlashCommand::Resume => UserAction::GoalResume,
            };
            state.enter_running();
            let _ = action_tx.send(action);
        }
        SlashCommand::SkillRun { id, args } => {
            let prompt = match args {
                Some(a) => format!("${id}:{a}"),
                None => format!("${id}"),
            };
            state.record_prompt(prompt.clone());
            state.push_message(ChatMessage::User(prompt.clone()));
            state.enter_running();
            let _ = action_tx.send(UserAction::Submit(prompt));
        }
        SlashCommand::WorkflowList => {
            state.show_workflows();
        }
        SlashCommand::SkillList => {
            let cwd = config
                .cwd
                .as_deref()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            match orca_tools::skills::discover_from_env(&cwd) {
                Ok(skills) if skills.is_empty() => {
                    state.push_message(ChatMessage::System("No skills found. Add SKILL.md files under .orca/skills/ or .agents/skills/.".to_string()));
                }
                Ok(skills) => {
                    let list = skills
                        .iter()
                        .map(|s| {
                            format!(
                                "${} [{}] — {}",
                                s.id,
                                s.source.as_str(),
                                if s.description.is_empty() {
                                    &s.name
                                } else {
                                    &s.description
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    state.push_message(ChatMessage::System(format!("Available skills:\n{list}")));
                }
                Err(e) => {
                    state.push_message(ChatMessage::Error(format!("failed to list skills: {e}")))
                }
            }
        }
        SlashCommand::WorkflowRun { name, args } => {
            state.enter_running();
            let _ = action_tx.send(UserAction::RunWorkflow { name, args });
        }
        SlashCommand::AgentDashboard => {
            state.show_agents();
        }
        SlashCommand::Remember(note) => {
            let remembered_note = note
                .strip_prefix("project:")
                .map(str::trim)
                .unwrap_or(note.as_str())
                .to_string();
            let result = if let Some(project_note) = note.strip_prefix("project:") {
                let cwd = config
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                orca_runtime::memory::remember_project(&cwd, project_note)
            } else {
                orca_runtime::memory::remember_user(&note)
            };
            match &result {
                Ok(path) => state.push_message(ChatMessage::System(format!(
                    "Remembered in {}.",
                    path.display()
                ))),
                Err(error) => {
                    state.push_message(ChatMessage::Error(format!("failed to remember: {error}")))
                }
            }
            if result.is_ok() {
                let _ = action_tx.send(UserAction::Remember(remembered_note));
            }
        }
        SlashCommand::Compact => {
            state.enter_running();
            let _ = action_tx.send(UserAction::Compact);
        }
        SlashCommand::History => match history::list_sessions(10) {
            Ok(sessions) if sessions.is_empty() => {
                state.push_message(ChatMessage::System("No saved sessions.".to_string()))
            }
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
                state.push_message(ChatMessage::System(format!("Recent sessions:\n{summary}")));
            }
            Err(error) => state.push_message(ChatMessage::Error(format!(
                "failed to list history: {error}"
            ))),
        },
        SlashCommand::Trust(trust_command) => {
            use orca_core::config::folder_trust::{self, TrustLevel};
            let cwd = config
                .cwd
                .as_deref()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            match trust_command {
                TrustSlashCommand::Show => {
                    if folder_trust::is_trusted(&cwd) {
                        state.push_message(ChatMessage::System(format!(
                            "{} is trusted; the OS sandbox honors the configured write and network policy.",
                            cwd.display()
                        )))
                    } else {
                        state.push_message(ChatMessage::System(format!(
                            "{} is not trusted; commands run read-only with no network. Use /trust add to trust it.",
                            cwd.display()
                        )))
                    }
                }
                TrustSlashCommand::Add => {
                    match folder_trust::set_trust(&cwd, TrustLevel::Trusted) {
                        Ok(()) => state.push_message(ChatMessage::System(format!(
                            "Trusted {}. Restart Orca to load project config from this folder.",
                            cwd.display()
                        ))),
                        Err(error) => state.push_message(ChatMessage::Error(format!(
                            "failed to trust folder: {error}"
                        ))),
                    }
                }
                TrustSlashCommand::Remove => {
                    match folder_trust::set_trust(&cwd, TrustLevel::Untrusted) {
                        Ok(()) => state.push_message(ChatMessage::System(format!(
                            "Removed trust for {}; commands now run read-only with no network.",
                            cwd.display()
                        ))),
                        Err(error) => state.push_message(ChatMessage::Error(format!(
                            "failed to update trust: {error}"
                        ))),
                    }
                }
            }
        }
    }
    state.scroll_to_bottom();
    Some(SlashOutcome::Continue)
}

fn parse_command_with_cwd(
    text: &str,
    cwd: impl FnOnce() -> std::path::PathBuf,
) -> Option<SlashCommand> {
    if commands::has_builtin_name(text) {
        return None;
    }
    commands::parse_with_cwd(text, &cwd())
}

pub(crate) fn parse_approval_mode(mode: &str) -> Option<ApprovalMode> {
    match mode {
        "suggest" => Some(ApprovalMode::Suggest),
        "auto-edit" => Some(ApprovalMode::AutoEdit),
        "full-auto" => Some(ApprovalMode::FullAuto),
        "plan" => Some(ApprovalMode::Plan),
        _ => None,
    }
}

fn fps_hud_message(enabled: bool) -> &'static str {
    if enabled {
        "FPS HUD enabled."
    } else {
        "FPS HUD disabled."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use crate::types::{AppStatus, PanelMode};
    use crate::vim::{VimMode, VimState};
    use orca_core::config::ThemeName;

    fn diagnostic_state() -> (
        AppState,
        mpsc::Receiver<UserAction>,
        RunConfig,
        Arc<Mutex<RunConfig>>,
        mpsc::Sender<UserAction>,
    ) {
        let (action_tx, action_rx) = mpsc::unbounded();
        let state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let config = crate::test_support::test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        (state, action_rx, config, shared, action_tx)
    }

    #[test]
    fn doctor_report_pushes_one_message_and_preserves_session_state() {
        let (mut state, _rx, mut config, shared, action_tx) = diagnostic_state();
        state.status = AppStatus::Running;
        state.panel_mode = PanelMode::Agents;
        state.vim_mode = Some(VimMode::Visual);
        let before = (state.status, state.panel_mode, state.vim_mode);

        assert!(
            handle_slash_command("/doctor", &mut config, &shared, &mut state, &action_tx,)
                .is_some()
        );

        assert_eq!((state.status, state.panel_mode, state.vim_mode), before);
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::System(report)) if report.starts_with("Orca diagnostics\n")
        ));
    }

    #[test]
    fn doctor_fps_toggle_and_explicit_forms_are_session_only_and_idempotent() {
        let (mut state, _rx, mut config, shared, action_tx) = diagnostic_state();
        let before = orca_core::config::format_config_show(&config);

        for (command, expected, message) in [
            ("/doctor fps", true, "FPS HUD enabled."),
            ("/doctor fps on", true, "FPS HUD enabled."),
            ("/doctor fps off", false, "FPS HUD disabled."),
            ("/doctor fps off", false, "FPS HUD disabled."),
        ] {
            handle_slash_command(command, &mut config, &shared, &mut state, &action_tx)
                .expect("recognized doctor command");
            assert_eq!(state.fps_hud_enabled, expected);
            assert!(matches!(
                state.messages.last(),
                Some(ChatMessage::System(text)) if text == message
            ));
            assert_eq!(orca_core::config::format_config_show(&config), before);
            assert_eq!(
                orca_core::config::format_config_show(&shared.lock().unwrap()),
                before,
            );
        }
    }

    #[test]
    fn submitted_doctor_command_is_cleared_by_existing_submit_contract() {
        use crate::composer_textarea::{make_textarea_with_text, textarea_text};
        use crate::idle_submit_actions::handle_idle_submit;

        let (mut state, _rx, mut config, shared, action_tx) = diagnostic_state();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("/doctor", &vim, &theme);

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim,
            &theme,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
        ));

        assert_eq!(textarea_text(&textarea), "");
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::System(report)) if report.starts_with("Orca diagnostics\n")
        ));
    }

    #[test]
    fn built_in_slash_commands_are_parsed_before_cwd_discovery() {
        let source = include_str!("slash_command_actions.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("production slash command source");
        let parse_builtin = source
            .find("commands::parse(text)")
            .expect("built-in parser must run first");
        let current_dir = source
            .find("std::env::current_dir()")
            .expect("dynamic aliases may still need cwd discovery");
        assert!(parse_builtin < current_dir);
    }

    #[test]
    fn malformed_builtin_commands_do_not_invoke_cwd_resolver() {
        let mut resolver_called = false;
        let command = parse_command_with_cwd("/doctor extra", || {
            resolver_called = true;
            std::path::PathBuf::from("/tmp")
        });

        assert_eq!(command, None);
        assert!(!resolver_called);
    }
}
