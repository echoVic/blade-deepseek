use std::io;
use std::path::PathBuf;

use crate::approval::policy::{ActionKind, ApprovalDecision, ApprovalPolicy, ApprovalRequest};
use crate::config::RunConfig;
use crate::event::schema::{EventFactory, RunStatus};
use crate::event::sink::EventSink;
use crate::provider::{self, ProviderStep};
use crate::runtime::session::new_run_id;
use crate::tools;
use crate::verification;

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
    sink.emit(&events.turn_started(1, prompt))?;

    let status = if config.provider != crate::config::ProviderKind::Mock {
        run_provider_plan(&config, &cwd_path, &mut events, &mut sink, prompt)?
    } else if let Some(tool_request) = tools::request_from_prompt(prompt) {
        sink.emit(&events.assistant_reasoning_delta(
            "Mock runtime is preserving the DeepSeek reasoning channel.",
        ))?;
        run_tool_request(
            &config,
            &cwd_path,
            &mut events,
            &mut sink,
            tool_request,
            true,
        )?
    } else if prompt_requests_write(prompt) {
        sink.emit(&events.assistant_reasoning_delta(
            "Mock runtime is preserving the DeepSeek reasoning channel.",
        ))?;
        run_mock_write_request(&config, &mut events, &mut sink)?
    } else {
        run_provider_plan(&config, &cwd_path, &mut events, &mut sink, prompt)?
    };

    let status =
        run_verifier_if_needed(status, config.verifier.as_deref(), &mut events, &mut sink)?;

    sink.emit(&events.session_completed(status))?;
    Ok(status)
}

fn run_provider_plan(
    config: &RunConfig,
    cwd: &PathBuf,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    prompt: &str,
) -> io::Result<RunStatus> {
    let mut status = RunStatus::Success;

    for step in provider::plan(config.provider, prompt) {
        match step {
            ProviderStep::ReasoningDelta(text) => {
                sink.emit(&events.assistant_reasoning_delta(&text))?;
            }
            ProviderStep::MessageDelta(text) => {
                sink.emit(&events.assistant_message_delta(&text))?;
            }
            ProviderStep::ReplayState(replay) => {
                sink.emit(&events.provider_replay_updated(&replay))?;
            }
            ProviderStep::ToolCall(tool_request) => {
                status = run_tool_request(config, cwd, events, sink, tool_request, false)?;
                if status != RunStatus::Success {
                    break;
                }
            }
        }
    }

    Ok(status)
}

fn run_tool_request(
    config: &RunConfig,
    cwd: &PathBuf,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: tools::ToolRequest,
    emit_summary: bool,
) -> io::Result<RunStatus> {
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
            sink.emit(&events.tool_call_requested(&tool_request))?;
            let result = tools::ToolResult::denied(&tool_request, resolution.reason);
            sink.emit(&events.tool_call_completed(&result))?;
            return Ok(RunStatus::ApprovalRequired);
        }
    }

    sink.emit(&events.tool_call_requested(&tool_request))?;
    let result = tools::execute(&tool_request, cwd);
    let is_failure = matches!(
        result.status,
        tools::ToolStatus::Failed | tools::ToolStatus::Denied
    );
    sink.emit(&events.tool_call_completed(&result))?;

    if is_failure {
        Ok(RunStatus::Failed)
    } else {
        if emit_summary {
            sink.emit(&events.assistant_message_delta("Mock runtime completed one tool request."))?;
        }
        Ok(RunStatus::Success)
    }
}

fn run_mock_write_request(
    config: &RunConfig,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
) -> io::Result<RunStatus> {
    let request = ApprovalRequest {
        id: "approval-1".to_string(),
        action: ActionKind::Write,
        description: "mock write action requested by prompt".to_string(),
    };
    let policy = ApprovalPolicy::new(config.approval_mode);
    let resolution = policy.resolve(&request);
    sink.emit(&events.approval_requested(&request))?;
    sink.emit(&events.approval_resolved(&resolution))?;

    if resolution.decision == ApprovalDecision::Deny {
        Ok(RunStatus::ApprovalRequired)
    } else {
        sink.emit(&events.assistant_message_delta("Mock runtime accepted the write request."))?;
        Ok(RunStatus::Success)
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

fn prompt_requests_write(prompt: &str) -> bool {
    let prompt = prompt.to_ascii_lowercase();
    [
        "write", "edit", "modify", "create", "delete", "写", "改", "删", "创建",
    ]
    .iter()
    .any(|needle| prompt.contains(needle))
}
