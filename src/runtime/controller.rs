use std::io;
use std::path::PathBuf;

use crate::approval::policy::{ActionKind, ApprovalDecision, ApprovalPolicy, ApprovalRequest};
use crate::config::RunConfig;
use crate::event::schema::{EventFactory, RunStatus};
use crate::event::sink::EventSink;
use crate::provider::conversation::Conversation;
use crate::provider::system_prompt::build_system_prompt;
use crate::provider::{self, ProviderStep};
use crate::runtime::session::new_run_id;
use crate::tools;
use crate::verification;

const DEFAULT_MAX_TURNS: u32 = 10;

pub fn run(config: RunConfig) -> i32 {
    match run_inner(config) {
        Ok(status) => status.exit_code(),
        Err(error) => {
            eprintln!("orca: {error}");
            RunStatus::Failed.exit_code()
        }
    }
}

fn run_inner(config: RunConfig) -> io::Result<RunStatus> {
    let cwd_path = config.cwd.clone().unwrap_or(std::env::current_dir()?);
    let cwd = cwd_path.display().to_string();
    let prompt = if config.prompt.trim().is_empty() {
        "(empty prompt)"
    } else {
        config.prompt.trim()
    };

    let mut events = EventFactory::new(new_run_id());
    let stdout = io::stdout();
    let mut sink = EventSink::new(stdout.lock(), config.output_format);

    sink.emit(&events.session_started(
        &cwd,
        config.approval_mode.as_str(),
        config.provider.as_str(),
        config.max_turns,
        config.verifier.as_deref(),
    ))?;

    let status = run_agent_loop(&config, &cwd_path, &mut events, &mut sink, prompt)?;

    let status =
        run_verifier_if_needed(status, config.verifier.as_deref(), &mut events, &mut sink)?;

    sink.emit(&events.session_completed(status))?;
    Ok(status)
}

fn run_agent_loop(
    config: &RunConfig,
    cwd: &PathBuf,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    prompt: &str,
) -> io::Result<RunStatus> {
    let max_turns = config.max_turns.unwrap_or(DEFAULT_MAX_TURNS);
    let ctx_config = provider::context::ContextConfig::default();

    let mut conversation = Conversation::new();
    conversation.add_system(build_system_prompt(cwd));
    conversation.add_user(prompt.to_string());

    let mut turn: u32 = 0;

    loop {
        turn += 1;

        if turn > max_turns {
            sink.emit(&events.error("max turns exhausted"))?;
            return Ok(RunStatus::BudgetExhausted);
        }

        if provider::context::needs_compaction(&conversation, &ctx_config) {
            conversation = provider::context::compact(&conversation, &ctx_config);
        }

        let turn_prompt = if turn == 1 { Some(prompt) } else { None };
        sink.emit(&events.turn_started(turn, turn_prompt))?;

        let response = provider::call_streaming(
            config.provider,
            &conversation,
            &mut |step| {
                match step {
                    ProviderStep::ReasoningDelta(text) => {
                        let _ = sink.emit(&events.assistant_reasoning_delta(text));
                    }
                    ProviderStep::MessageDelta(text) => {
                        let _ = sink.emit(&events.assistant_message_delta(text));
                    }
                    _ => {}
                }
            },
        );

        let mut had_error = false;
        for step in &response.steps {
            match step {
                ProviderStep::ReplayState(replay) => {
                    sink.emit(&events.provider_replay_updated(replay))?;
                }
                ProviderStep::Error(message) => {
                    sink.emit(&events.error(message))?;
                    had_error = true;
                    break;
                }
                _ => {}
            }
        }

        if had_error {
            return Ok(RunStatus::Failed);
        }

        if response.tool_calls.is_empty() {
            conversation.add_assistant(
                response.assistant_content,
                response.assistant_reasoning,
                vec![],
            );
            return Ok(RunStatus::Success);
        }

        conversation.add_assistant(
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
        );

        for step in &response.steps {
            if let ProviderStep::ToolCall(tool_request) = step {
                let (status, result) =
                    execute_tool_with_approval(config, cwd, events, sink, tool_request)?;

                let result_content = format_tool_result_for_model(&result);
                conversation.add_tool_result(tool_request.id.clone(), result_content);

                if status != RunStatus::Success {
                    return Ok(status);
                }
            }
        }
    }
}

fn execute_tool_with_approval(
    config: &RunConfig,
    cwd: &PathBuf,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tools::ToolRequest,
) -> io::Result<(RunStatus, tools::ToolResult)> {
    let policy = ApprovalPolicy::new(config.approval_mode);

    if requires_approval(tool_request.action) {
        let approval = ApprovalRequest {
            id: format!("approval-{}", tool_request.id),
            action: tool_request.action,
            description: format!(
                "{} requested {}",
                tool_request.name.as_str(),
                tool_request.action.as_str()
            ),
        };
        let resolution = policy.resolve(&approval);
        sink.emit(&events.approval_requested(&approval))?;
        sink.emit(&events.approval_resolved(&resolution))?;

        if resolution.decision == ApprovalDecision::Deny {
            sink.emit(&events.tool_call_requested(tool_request))?;
            let result = tools::ToolResult::denied(tool_request, resolution.reason);
            sink.emit(&events.tool_call_completed(&result))?;
            return Ok((RunStatus::ApprovalRequired, result));
        }
    }

    sink.emit(&events.tool_call_requested(tool_request))?;
    let result = tools::execute(tool_request, cwd);
    let is_failure = matches!(
        result.status,
        tools::ToolStatus::Failed | tools::ToolStatus::Denied
    );
    sink.emit(&events.tool_call_completed(&result))?;

    let status = if is_failure {
        RunStatus::Failed
    } else {
        RunStatus::Success
    };

    Ok((status, result))
}

fn format_tool_result_for_model(result: &tools::ToolResult) -> String {
    match (&result.output, &result.error) {
        (Some(output), _) => {
            if result.truncated {
                format!("{output}\n[output truncated]")
            } else {
                output.clone()
            }
        }
        (_, Some(error)) => format!("ERROR: {error}"),
        _ => "(no output)".to_string(),
    }
}

fn requires_approval(action: ActionKind) -> bool {
    matches!(action, ActionKind::Write | ActionKind::Shell)
}

fn run_verifier_if_needed(
    status: RunStatus,
    verifier: Option<&str>,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
) -> io::Result<RunStatus> {
    if status != RunStatus::Success {
        return Ok(status);
    }

    let Some(command) = verifier else {
        return Ok(status);
    };

    sink.emit(&events.verification_started(command))?;
    let result = verification::run(command);
    let success = result.success;
    sink.emit(&events.verification_completed(&result))?;

    if success {
        Ok(RunStatus::Success)
    } else {
        Ok(RunStatus::VerificationFailed)
    }
}
