use crossbeam_channel as mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::RunConfig;
use orca_runtime::surface::RuntimeSurfaceHostHandle;

use crate::commands::{self, GoalSlashCommand, SlashCommand, TrustSlashCommand};
use crate::surface_actions::TuiHostActions;
use crate::types::{AppState, AppStatus, ChatMessage, TuiMemoryScope, UserAction};

pub(crate) enum SlashOutcome {
    Continue,
    Prefill(String),
}

pub(crate) fn handle_slash_command(
    text: &str,
    config: &mut RunConfig,
    _shared_config: &Arc<Mutex<RunConfig>>,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> Option<SlashOutcome> {
    let cwd = config
        .cwd
        .as_deref()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let command = commands::parse_with_cwd(text, &cwd)?;
    let mut pending_settings_action = None;
    match command {
        SlashCommand::New => {
            if state.status == AppStatus::Idle {
                state.enter_running();
                let _ = action_tx.send(UserAction::NewSession);
            } else {
                state.push_message(ChatMessage::Error(
                    "finish or cancel the current work before starting a new conversation"
                        .to_string(),
                ));
            }
        }
        SlashCommand::Model(Some(model)) => match commands::validate_model(&model) {
            Ok(()) => {
                pending_settings_action = Some(UserAction::SetModel(model));
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
        SlashCommand::Mode(Some(mode)) => match parse_approval_mode(&mode) {
            Some(approval_mode) => {
                pending_settings_action = Some(UserAction::SetModel(encode_settings_intent(
                    None,
                    None,
                    Some(approval_mode),
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
                pending_settings_action = Some(UserAction::SetModel(encode_settings_intent(
                    None,
                    None,
                    Some(ApprovalMode::Suggest),
                )));
            }
            None => {
                pending_settings_action = Some(UserAction::SetModel(encode_settings_intent(
                    None,
                    None,
                    Some(ApprovalMode::Plan),
                )));
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
            let (scope, note) = if let Some(project_note) = note.strip_prefix("project:") {
                (TuiMemoryScope::Project, project_note.trim().to_string())
            } else {
                (TuiMemoryScope::User, note)
            };
            let _ = action_tx.send(UserAction::Remember { scope, note });
        }
        SlashCommand::Compact => {
            state.enter_running();
            let _ = action_tx.send(UserAction::Compact);
        }
        SlashCommand::Resume => match RuntimeSurfaceHostHandle::list_saved_sessions(20) {
            Ok(sessions) if !sessions.is_empty() => {
                state.reset_queued_user_messages();
                state.session_picker_sessions = sessions;
                state.session_picker_selected = 0;
                state.session_picker_phase = crate::types::SessionPickerPhase::Browsing;
                state.session_picker_error = None;
                state.status = AppStatus::SessionPicker;
            }
            Ok(_) => state.push_message(ChatMessage::System("No saved conversations.".to_string())),
            Err(error) => state.push_message(ChatMessage::Error(format!(
                "failed to list saved conversations: {error}"
            ))),
        },
        SlashCommand::Fork(title) => {
            if state.status == AppStatus::Idle {
                state.enter_running();
                let _ = action_tx.send(UserAction::ForkCurrentSession { title });
            } else {
                state.push_message(ChatMessage::Error(
                    "finish or cancel the current work before forking this conversation"
                        .to_string(),
                ));
            }
        }
        SlashCommand::Rename(None) => return Some(SlashOutcome::Prefill("/rename ".to_string())),
        SlashCommand::Rename(Some(title)) => {
            state.enter_running();
            let _ = action_tx.send(UserAction::RenameCurrentSession { title });
        }
        SlashCommand::Status => {
            state.push_message(ChatMessage::System(format_status(state, config)));
        }
        SlashCommand::Copy(argument) => {
            let position = match argument.as_deref() {
                None => Some(1),
                Some(value) => value.parse::<usize>().ok().filter(|value| *value > 0),
            };
            match position.and_then(|position| {
                state
                    .nth_final_assistant_response(position)
                    .map(str::to_string)
            }) {
                Some(text) => state.stage_clipboard_copy(text, Instant::now()),
                None => state.push_message(ChatMessage::Error(
                    "usage: /copy [N], where N selects a completed assistant response from newest to oldest"
                        .to_string(),
                )),
            }
        }
        SlashCommand::CancelOperation => {
            if let Some(operation_id) = state.recoverable_operation_id.clone() {
                state.enter_running();
                let _ = action_tx.send(UserAction::CancelOperation { operation_id });
            } else {
                state.push_message(ChatMessage::Error(
                    "no recoverable operation is available".to_string(),
                ));
            }
        }
        SlashCommand::Trust(trust_command) => match trust_command {
            TrustSlashCommand::Show => {
                if TuiHostActions::folder_is_trusted(&cwd) {
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
            TrustSlashCommand::Add => match TuiHostActions::set_folder_trust(&cwd, true) {
                Ok(()) => state.push_message(ChatMessage::System(format!(
                    "Trusted {}. Restart Orca to load project config from this folder.",
                    cwd.display()
                ))),
                Err(error) => state.push_message(ChatMessage::Error(format!(
                    "failed to trust folder: {error}"
                ))),
            },
            TrustSlashCommand::Remove => match TuiHostActions::set_folder_trust(&cwd, false) {
                Ok(()) => state.push_message(ChatMessage::System(format!(
                    "Removed trust for {}; commands now run read-only with no network.",
                    cwd.display()
                ))),
                Err(error) => state.push_message(ChatMessage::Error(format!(
                    "failed to update trust: {error}"
                ))),
            },
        },
    }
    if let Some(action) = pending_settings_action {
        let _ = action_tx.send(action);
    }
    state.scroll_to_bottom();
    Some(SlashOutcome::Continue)
}

fn format_status(state: &AppState, config: &RunConfig) -> String {
    let session_id = state.current_session_id.as_deref().unwrap_or("-");
    let title = state.current_session_title.as_deref().unwrap_or("-");
    let context = if state.context_limit_tokens == 0 {
        "-".to_string()
    } else {
        format!(
            "{} / {}",
            state.context_used_tokens, state.context_limit_tokens
        )
    };
    let active_tasks = state
        .workflow_panel
        .tasks
        .iter()
        .filter(|task| {
            matches!(
                task.status,
                orca_core::task_types::TaskStatus::Queued
                    | orca_core::task_types::TaskStatus::Running
                    | orca_core::task_types::TaskStatus::Paused
                    | orca_core::task_types::TaskStatus::Stopping
                    | orca_core::task_types::TaskStatus::ApprovalRequired
            )
        })
        .count();
    let goal = state.current_goal.as_ref().map_or("-", |goal| {
        orca_core::goal_types::goal_status_label(goal.status)
    });
    format!(
        "Session status\n\
         title: {title}\n\
         id: {session_id}\n\
         model: {} ({})\n\
         mode: {}\n\
         cwd: {}\n\
         context: {context}\n\
         usage: {} input, {} output, {} cache\n\
         cost: ${:.6}\n\
         goal: {goal}\n\
         active tasks: {active_tasks}\n\
         recoverable: {}",
        state.model_name,
        state.reasoning_effort.as_str(),
        config.approval_mode.as_str(),
        state.cwd,
        state.usage.input_tokens,
        state.usage.output_tokens,
        state.usage.cache_tokens,
        state.usage.estimated_cost_usd,
        if state.recoverable_operation_id.is_some() {
            "yes"
        } else {
            "no"
        },
    )
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

const SETTINGS_INTENT_PREFIX: &str = "__orca_runtime_settings__:";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SettingsIntent {
    pub model: Option<String>,
    pub reasoning_effort: Option<orca_core::config::ReasoningEffort>,
    pub approval_mode: Option<ApprovalMode>,
}

pub(crate) fn encode_settings_intent(
    model: Option<&str>,
    reasoning_effort: Option<orca_core::config::ReasoningEffort>,
    approval_mode: Option<ApprovalMode>,
) -> String {
    format!(
        "{SETTINGS_INTENT_PREFIX}{}|{}|{}",
        model.unwrap_or("-"),
        reasoning_effort.map_or("-", orca_core::config::ReasoningEffort::as_str),
        approval_mode.map_or("-", ApprovalMode::as_str),
    )
}

pub(crate) fn decode_settings_intent(value: &str) -> Option<SettingsIntent> {
    let fields = value
        .strip_prefix(SETTINGS_INTENT_PREFIX)?
        .split('|')
        .collect::<Vec<_>>();
    if fields.len() != 3 {
        return None;
    }
    let model = match fields[0] {
        "-" => None,
        model if orca_core::model::validate_model(model).is_ok() => Some(model.to_string()),
        _ => return None,
    };
    let reasoning_effort = match fields[1] {
        "-" => None,
        "low" => Some(orca_core::config::ReasoningEffort::Low),
        "high" => Some(orca_core::config::ReasoningEffort::High),
        "max" => Some(orca_core::config::ReasoningEffort::Max),
        _ => return None,
    };
    let approval_mode = match fields[2] {
        "-" => None,
        "suggest" => Some(ApprovalMode::Suggest),
        "auto-edit" => Some(ApprovalMode::AutoEdit),
        "full-auto" => Some(ApprovalMode::FullAuto),
        "plan" => Some(ApprovalMode::Plan),
        _ => return None,
    };
    (model.is_some() || reasoning_effort.is_some() || approval_mode.is_some()).then_some(
        SettingsIntent {
            model,
            reasoning_effort,
            approval_mode,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_run_config;

    fn state() -> AppState {
        let (action_tx, _) = mpsc::unbounded();
        AppState::new(
            action_tx,
            "test".to_string(),
            "deepseek-v4-pro".to_string(),
            "/tmp/project".to_string(),
        )
    }

    #[test]
    fn low_reasoning_effort_round_trips_through_settings_intent() {
        let encoded = encode_settings_intent(
            Some("deepseek-v4-flash"),
            Some(orca_core::config::ReasoningEffort::Low),
            None,
        );

        let decoded = decode_settings_intent(&encoded).expect("decode low effort intent");

        assert_eq!(
            decoded.reasoning_effort,
            Some(orca_core::config::ReasoningEffort::Low)
        );
    }

    #[test]
    fn copy_slash_command_stages_nth_final_response() {
        let mut state = state();
        state.push_message(ChatMessage::Assistant("older".to_string()));
        state.push_message(ChatMessage::AssistantChunk {
            text: "unfinished".to_string(),
            trailing_blank: false,
        });
        state.push_message(ChatMessage::Assistant("latest".to_string()));
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, _) = mpsc::unbounded();

        handle_slash_command("/copy 2", &mut config, &shared, &mut state, &action_tx);

        assert_eq!(state.pending_clipboard_copy.as_deref(), Some("older"));
    }

    #[test]
    fn copy_slash_command_rejects_invalid_or_missing_indices() {
        for command in ["/copy 0", "/copy nope", "/copy 2"] {
            let mut state = state();
            state.push_message(ChatMessage::Assistant("only".to_string()));
            let mut config = test_run_config();
            let shared = Arc::new(Mutex::new(config.clone()));
            let (action_tx, _) = mpsc::unbounded();

            handle_slash_command(command, &mut config, &shared, &mut state, &action_tx);

            assert!(state.pending_clipboard_copy.is_none(), "accepted {command}");
            assert!(matches!(state.messages.last(), Some(ChatMessage::Error(_))));
        }
    }

    #[test]
    fn status_slash_command_reports_session_snapshot() {
        let mut state = state();
        state.current_session_id = Some("session-1".to_string());
        state.current_session_title = Some("Release triage".to_string());
        state.context_used_tokens = 250;
        state.context_limit_tokens = 1_000;
        state.usage.input_tokens = 100;
        state.usage.output_tokens = 50;
        state.usage.cache_tokens = 25;
        state.usage.estimated_cost_usd = 0.125;
        state.recoverable_operation_id = Some(
            orca_runtime::surface::SurfaceOperationId::try_from_bytes([
                0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 3,
            ])
            .unwrap(),
        );
        let mut config = test_run_config();
        config.approval_mode = ApprovalMode::Plan;
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, _) = mpsc::unbounded();

        handle_slash_command("/status", &mut config, &shared, &mut state, &action_tx);

        let Some(ChatMessage::System(status)) = state.messages.last() else {
            panic!("status output was not appended");
        };
        for expected in [
            "Release triage",
            "session-1",
            "deepseek-v4-pro",
            "plan",
            "/tmp/project",
            "250 / 1000",
            "100 input, 50 output, 25 cache",
            "$0.125000",
            "recoverable: yes",
        ] {
            assert!(status.contains(expected), "missing {expected}: {status}");
        }
    }

    #[test]
    fn fork_slash_command_dispatches_typed_action_only_while_idle() {
        for (status, should_dispatch) in [(AppStatus::Idle, true), (AppStatus::Running, false)] {
            let mut state = state();
            state.status = status;
            let mut config = test_run_config();
            let shared = Arc::new(Mutex::new(config.clone()));
            let (action_tx, action_rx) = mpsc::unbounded();

            handle_slash_command(
                "/fork auth experiment",
                &mut config,
                &shared,
                &mut state,
                &action_tx,
            );

            if should_dispatch {
                assert!(matches!(
                    action_rx.try_recv(),
                    Ok(UserAction::ForkCurrentSession { title: Some(title) })
                        if title == "auth experiment"
                ));
                assert_eq!(state.status, AppStatus::Running);
            } else {
                assert!(action_rx.try_recv().is_err());
                assert!(matches!(state.messages.last(), Some(ChatMessage::Error(_))));
            }
        }
    }

    #[test]
    fn rename_slash_command_prefills_or_dispatches_typed_action() {
        let mut state = state();
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();

        assert!(matches!(
            handle_slash_command(
                "/rename",
                &mut config,
                &shared,
                &mut state,
                &action_tx,
            ),
            Some(SlashOutcome::Prefill(value)) if value == "/rename "
        ));
        assert!(action_rx.try_recv().is_err());

        handle_slash_command(
            "/rename release triage",
            &mut config,
            &shared,
            &mut state,
            &action_tx,
        );
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::RenameCurrentSession { title }) if title == "release triage"
        ));
        assert_eq!(state.status, AppStatus::Running);
    }
}
