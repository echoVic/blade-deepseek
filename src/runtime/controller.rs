use std::io;
use std::path::Path;

use crate::approval::confirm;
use crate::approval::policy::{
    ApprovalDecision, ApprovalPolicy, ApprovalRequest, ApprovalResolution,
};
use crate::config::{HistoryMode, OutputFormat, RunConfig};
use crate::event::schema::{EventFactory, RunStatus};
use crate::event::sink::EventSink;
use crate::provider::conversation::Conversation;
use crate::provider::tool_schema::deepseek_tools_schema_for_type;
use crate::provider::{self, ProviderConfig, ProviderStep};
use crate::runtime::agent_common;
use crate::runtime::cancel::CancelToken;
use crate::runtime::history::{self, SessionWriter};
use crate::runtime::session::new_run_id;
use crate::runtime::subagent;
use crate::runtime::subagent_types::SubagentType;
use crate::tools;
use crate::verification;

const DEFAULT_MAX_TURNS: u32 = 128;
const MAX_SUBAGENT_DEPTH: u32 = 1;

#[derive(Clone, Debug)]
struct AgentLoopResult {
    status: RunStatus,
    final_message: Option<String>,
    error: Option<String>,
}

impl AgentLoopResult {
    fn success(final_message: Option<String>) -> Self {
        Self {
            status: RunStatus::Success,
            final_message,
            error: None,
        }
    }

    fn failure(status: RunStatus, error: impl Into<String>) -> Self {
        Self {
            status,
            final_message: None,
            error: Some(error.into()),
        }
    }
}

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
        "(empty prompt)".to_string()
    } else {
        config.prompt.trim().to_string()
    };

    let mut events = EventFactory::new(new_run_id());
    let stdout = io::stdout();
    let mut sink = EventSink::new(stdout.lock(), config.output_format);

    let resumed = match &config.history_mode {
        HistoryMode::Resume(selector) | HistoryMode::Fork(selector) => {
            Some(history::load_session(selector)?)
        }
        HistoryMode::Record | HistoryMode::Disabled => None,
    };

    let mut history_writer = match &config.history_mode {
        HistoryMode::Disabled => None,
        HistoryMode::Record | HistoryMode::Resume(_) => match SessionWriter::start(
            &cwd_path,
            config.provider.as_str(),
            config.model.clone(),
            &prompt,
        ) {
            Ok(writer) => Some(writer),
            Err(error) => {
                eprintln!("orca: warning: failed to initialize history: {error}");
                None
            }
        },
        HistoryMode::Fork(_) => {
            let parent_id = resumed
                .as_ref()
                .map(|transcript| transcript.meta.session_id.clone())
                .unwrap_or_default();
            let meta = history::create_fork_meta(
                &cwd_path,
                config.provider.as_str(),
                config.model.clone(),
                &prompt,
                parent_id,
            );
            match SessionWriter::start_from_meta(meta) {
                Ok(writer) => Some(writer),
                Err(error) => {
                    eprintln!("orca: warning: failed to initialize history: {error}");
                    None
                }
            }
        }
    };

    sink.emit(&events.session_started(
        &cwd,
        config.approval_mode.as_str(),
        config.provider.as_str(),
        config.verifier.as_deref(),
    ))?;

    let cancel = CancelToken::new();
    let result = run_agent_loop(
        &config,
        &cwd_path,
        &mut events,
        &mut sink,
        &prompt,
        resumed.as_ref(),
        history_writer.as_mut(),
        0,
        true,
        &SubagentType::General,
        &cancel,
    )?;
    let status = result.status;

    let status =
        run_verifier_if_needed(status, config.verifier.as_deref(), &mut events, &mut sink)?;

    if let Some(writer) = history_writer.as_mut() {
        writer.complete(status.as_str())?;
    }

    sink.emit(&events.session_completed(status))?;
    Ok(status)
}

#[allow(clippy::too_many_arguments)]
fn run_agent_loop(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    prompt: &str,
    resumed: Option<&history::SessionTranscript>,
    history_writer: Option<&mut SessionWriter>,
    subagent_depth: u32,
    emit_deltas: bool,
    subagent_type: &SubagentType,
    cancel: &CancelToken,
) -> io::Result<AgentLoopResult> {
    let max_turns = DEFAULT_MAX_TURNS;
    let ctx_config = provider::context::ContextConfig::default();
    let tools_override = if subagent_depth > 0 {
        Some(deepseek_tools_schema_for_type(subagent_type))
    } else {
        None
    };
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: config.model.clone(),
        tools_override,
    };

    let system_prompt = agent_common::build_agent_system_prompt(cwd, subagent_depth, subagent_type);
    let mut conversation = if let Some(resumed) = resumed {
        history::resume_conversation(resumed, system_prompt)
    } else {
        let mut conversation = Conversation::new();
        conversation.add_system(system_prompt);
        conversation
    };
    conversation.add_user(prompt.to_string());

    let mut history_writer = history_writer;
    if emit_deltas && let Some(writer) = history_writer.as_deref_mut() {
        if resumed.is_some() {
            for message in &conversation.messages {
                writer.append_message(message)?;
            }
        } else {
            if let Some(system) = conversation.messages.first() {
                writer.append_message(system)?;
            }
            if let Some(user) = conversation.messages.last() {
                writer.append_message(user)?;
            }
        }
    }

    let mut turn: u32 = 0;

    loop {
        turn += 1;

        if turn > max_turns {
            let error = "max turns exhausted";
            if emit_deltas {
                sink.emit(&events.error(error))?;
            }
            return Ok(AgentLoopResult::failure(RunStatus::BudgetExhausted, error));
        }

        if provider::context::needs_compaction(&conversation, &ctx_config) {
            let before_messages = conversation.messages.len();
            conversation = provider::context::compact(&conversation, &ctx_config);
            if emit_deltas && let Some(writer) = history_writer.as_deref_mut() {
                writer.append_compaction(before_messages, conversation.messages.len())?;
            }
        }

        let turn_prompt = if turn == 1 { Some(prompt) } else { None };
        if emit_deltas {
            sink.emit(&events.turn_started(turn, turn_prompt))?;
        }

        let response = provider::call_streaming(
            config.provider,
            &conversation,
            &provider_config,
            cancel,
            &mut |step| {
                if !emit_deltas {
                    return;
                }
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
                    if emit_deltas {
                        sink.emit(&events.provider_replay_updated(replay))?;
                    }
                }
                ProviderStep::Error(message) => {
                    if emit_deltas {
                        sink.emit(&events.error(message))?;
                    }
                    had_error = true;
                    break;
                }
                _ => {}
            }
        }

        if had_error {
            let error = response.steps.iter().find_map(|step| match step {
                ProviderStep::Error(message) => Some(message.clone()),
                _ => None,
            });
            return Ok(AgentLoopResult::failure(
                RunStatus::Failed,
                error.unwrap_or_else(|| "provider error".to_string()),
            ));
        }

        if response.tool_calls.is_empty() {
            let final_message = response.assistant_content.clone();
            conversation.add_assistant(
                response.assistant_content,
                response.assistant_reasoning,
                vec![],
            );
            if emit_deltas
                && let Some(writer) = history_writer.as_deref_mut()
                && let Some(message) = conversation.messages.last()
            {
                writer.append_message(message)?;
            }
            return Ok(AgentLoopResult::success(final_message));
        }

        conversation.add_assistant(
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
        );
        if emit_deltas
            && let Some(writer) = history_writer.as_deref_mut()
            && let Some(message) = conversation.messages.last()
        {
            writer.append_message(message)?;
        }

        for step in &response.steps {
            if let ProviderStep::ToolCall(tool_request) = step {
                let (status, result) = execute_tool_with_approval(
                    config,
                    cwd,
                    events,
                    sink,
                    tool_request,
                    subagent_depth,
                    emit_deltas,
                )?;

                let result_content = agent_common::format_tool_result_for_model(&result);
                conversation.add_tool_result(tool_request.id.clone(), result_content);
                if emit_deltas
                    && let Some(writer) = history_writer.as_deref_mut()
                    && let Some(message) = conversation.messages.last()
                {
                    writer.append_message(message)?;
                }

                if status != RunStatus::Success {
                    return Ok(AgentLoopResult {
                        status,
                        final_message: None,
                        error: result.error.clone(),
                    });
                }
            }
        }
    }
}

fn execute_tool_with_approval(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tools::ToolRequest,
    subagent_depth: u32,
    emit_deltas: bool,
) -> io::Result<(RunStatus, tools::ToolResult)> {
    let policy = ApprovalPolicy::new(config.approval_mode);

    if agent_common::requires_approval(tool_request.action) {
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
        if emit_deltas {
            sink.emit(&events.approval_requested(&approval))?;
        }

        match resolution.decision {
            ApprovalDecision::Allow => {
                if emit_deltas {
                    sink.emit(&events.approval_resolved(&resolution))?;
                }
            }
            ApprovalDecision::Ask => {
                let final_resolution = resolve_interactive(config, &approval, tool_request)?;
                if emit_deltas {
                    sink.emit(&events.approval_resolved(&final_resolution))?;
                }
                if final_resolution.decision == ApprovalDecision::Deny {
                    if emit_deltas {
                        sink.emit(&events.tool_call_requested(tool_request))?;
                    }
                    let result = tools::ToolResult::denied(tool_request, final_resolution.reason);
                    if emit_deltas {
                        sink.emit(&events.tool_call_completed(&result))?;
                    }
                    return Ok((RunStatus::ApprovalRequired, result));
                }
            }
            ApprovalDecision::Deny => {
                if emit_deltas {
                    sink.emit(&events.approval_resolved(&resolution))?;
                    sink.emit(&events.tool_call_requested(tool_request))?;
                }
                let result = tools::ToolResult::denied(tool_request, resolution.reason);
                if emit_deltas {
                    sink.emit(&events.tool_call_completed(&result))?;
                }
                return Ok((RunStatus::ApprovalRequired, result));
            }
        }
    }

    if emit_deltas {
        sink.emit(&events.tool_call_requested(tool_request))?;
    }
    let result = if tool_request.name == tools::ToolName::Subagent {
        execute_subagent_tool(config, cwd, events, sink, tool_request, subagent_depth)?
    } else {
        tools::execute(tool_request, cwd)
    };
    let is_failure = matches!(
        result.status,
        tools::ToolStatus::Failed | tools::ToolStatus::Denied
    );
    if emit_deltas {
        sink.emit(&events.tool_call_completed(&result))?;
    }

    let status = if is_failure {
        RunStatus::Failed
    } else {
        RunStatus::Success
    };

    Ok((status, result))
}

fn execute_subagent_tool(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tools::ToolRequest,
    subagent_depth: u32,
) -> io::Result<tools::ToolResult> {
    let request = subagent::create_subagent_request(tool_request);
    let description = request.description.clone();
    let subagent_type = request.subagent_type;

    sink.emit(&events.subagent_started(&tool_request.id, &description))?;

    if subagent_depth >= MAX_SUBAGENT_DEPTH {
        let error = "nested subagents are disabled in this MVP";
        sink.emit(&events.subagent_completed(
            &tool_request.id,
            &description,
            RunStatus::Failed,
            None,
            Some(error),
        ))?;
        return Ok(tools::ToolResult::failed(tool_request, error, None));
    }

    let child_cancel = CancelToken::new();
    let child = run_agent_loop(
        config,
        cwd,
        events,
        sink,
        &request.prompt,
        None,
        None,
        subagent_depth + 1,
        false,
        &subagent_type,
        &child_cancel,
    )?;

    match child.status {
        RunStatus::Success => {
            let output = child
                .final_message
                .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
            sink.emit(&events.subagent_completed(
                &tool_request.id,
                &description,
                child.status,
                Some(&output),
                None,
            ))?;
            Ok(tools::ToolResult::completed(
                tool_request,
                format!("Subagent status: success\n\n{output}"),
                false,
            ))
        }
        status => {
            let error = child
                .error
                .unwrap_or_else(|| format!("subagent ended with status {status:?}"));
            sink.emit(&events.subagent_completed(
                &tool_request.id,
                &description,
                status,
                child.final_message.as_deref(),
                Some(&error),
            ))?;
            Ok(tools::ToolResult::failed(
                tool_request,
                format!("Subagent status: {status:?}\n\n{error}"),
                None,
            ))
        }
    }
}

fn resolve_interactive(
    config: &RunConfig,
    approval: &ApprovalRequest,
    tool_request: &tools::ToolRequest,
) -> io::Result<ApprovalResolution> {
    if config.output_format == OutputFormat::Jsonl {
        return Ok(ApprovalResolution {
            id: approval.id.clone(),
            decision: ApprovalDecision::Deny,
            reason: "interactive confirmation unavailable in jsonl mode".to_string(),
        });
    }

    let allowed = confirm::prompt_user(tool_request.name.as_str(), tool_request.target.as_deref())?;

    Ok(ApprovalResolution {
        id: approval.id.clone(),
        decision: if allowed {
            ApprovalDecision::Allow
        } else {
            ApprovalDecision::Deny
        },
        reason: if allowed {
            "user approved".to_string()
        } else {
            "user denied".to_string()
        },
    })
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
