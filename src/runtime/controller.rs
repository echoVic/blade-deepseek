use std::io;
use std::path::Path;
use std::thread;

use crate::approval::confirm;
use crate::approval::policy::{
    ApprovalDecision, ApprovalPolicy, ApprovalRequest, ApprovalResolution,
};
use crate::config::{HistoryMode, OutputFormat, RunConfig};
use crate::event::schema::{EventFactory, RunStatus};
use crate::event::sink::EventSink;
use crate::mcp::McpRegistry;
use crate::model::ModelRouteContext;
use crate::provider::conversation::Conversation;
use crate::provider::tool_schema::{
    deepseek_tools_schema_for_type_with_mcp, deepseek_tools_schema_with_mcp,
};
use crate::provider::{self, ProviderConfig, ProviderStep};
use crate::runtime::agent_common;
use crate::runtime::cancel::CancelToken;
use crate::runtime::cost::CostTracker;
use crate::runtime::history::{self, SessionWriter};
use crate::runtime::hooks::{HookContext, HookEvent, HookRunner};
use crate::runtime::instructions::{self, ProjectInstructions};
use crate::runtime::memory::{self, MemoryBlock};
use crate::runtime::session::new_run_id;
use crate::runtime::subagent;
use crate::runtime::subagent_types::SubagentType;
use crate::tools;
use crate::verification;

const DEFAULT_MAX_TURNS: u32 = 128;

#[derive(Clone, Debug)]
struct AgentLoopResult {
    status: RunStatus,
    final_message: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct SubagentExecutionResult {
    tool_request: tools::ToolRequest,
    description: String,
    child: AgentLoopResult,
    cost_tracker: CostTracker,
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
    let instructions = load_project_instructions(&cwd_path);
    let memory = memory::load_for_cwd(&cwd_path);
    let hooks = HookRunner::new(config.hooks.clone());
    let mcp_registry = crate::mcp::initialize_registry(&config.mcp_servers);
    for error in mcp_registry.errors() {
        eprintln!("orca: warning: {error}");
    }

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
            config.model.as_history_value(),
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
                config.model.as_history_value(),
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
    if let Err(error) = hooks.run(
        HookEvent::SessionStart,
        HookContext {
            cwd: &cwd,
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
        },
    ) {
        sink.emit(&events.error(&format!("session_start hook failed: {error}")))?;
    }

    let cancel = CancelToken::new();
    let mut cost_tracker = CostTracker::new(config.model.as_deref());
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
        &instructions,
        &memory,
        &mcp_registry,
        &hooks,
        &mut cost_tracker,
        &cancel,
    )?;
    let status = result.status;

    let status =
        run_verifier_if_needed(status, config.verifier.as_deref(), &mut events, &mut sink)?;

    if let Some(writer) = history_writer.as_mut() {
        writer.complete(status.as_str())?;
    }
    if let Err(error) = hooks.run(
        HookEvent::SessionEnd,
        HookContext {
            cwd: &cwd,
            session_status: Some(status.as_str()),
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
        },
    ) {
        sink.emit(&events.error(&format!("session_end hook failed: {error}")))?;
    }

    sink.emit(&events.session_completed(status))?;
    if config.desktop_notifications {
        let _ = crate::runtime::notify::notify("Orca", &format!("Session {}", status.as_str()));
    }
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
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
) -> io::Result<AgentLoopResult> {
    let max_turns = DEFAULT_MAX_TURNS;
    let ctx_config = provider::context::ContextConfig::default();
    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());
    let tools_override = if subagent_depth > 0 {
        Some(deepseek_tools_schema_for_type_with_mcp(
            subagent_type,
            Some(mcp_registry),
        ))
    } else {
        Some(deepseek_tools_schema_with_mcp(Some(mcp_registry)))
    };
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: config.model.as_option(),
        tools_override,
        mcp_registry: Some(mcp_registry.clone()),
    };

    let system_prompt = agent_common::build_agent_system_prompt(
        cwd,
        subagent_depth,
        subagent_type,
        Some(instructions),
        config.approval_mode,
        Some(memory),
    );
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
            if emit_deltas
                && let Err(error) = hooks.run(
                    HookEvent::PreCompact,
                    HookContext {
                        cwd: &cwd.display().to_string(),
                        session_status: None,
                        tool_request: None,
                        tool_result: None,
                        before_messages: Some(before_messages),
                        after_messages: None,
                    },
                )
            {
                sink.emit(&events.error(&format!("pre_compact hook failed: {error}")))?;
            }
            let compaction = provider::context::compact_with_summary(
                config.provider,
                &conversation,
                &ctx_config,
                &provider_config,
            );
            conversation = compaction.conversation;
            let after_messages = conversation.messages.len();
            if emit_deltas && let Some(writer) = history_writer.as_deref_mut() {
                writer.append_compaction(before_messages, after_messages)?;
                if let provider::context::CompactionKind::RemoteSummary(summary) = compaction.kind {
                    writer.append_summary(before_messages, after_messages, summary)?;
                }
            }
            if emit_deltas
                && let Err(error) = hooks.run(
                    HookEvent::PostCompact,
                    HookContext {
                        cwd: &cwd.display().to_string(),
                        session_status: None,
                        tool_request: None,
                        tool_result: None,
                        before_messages: Some(before_messages),
                        after_messages: Some(after_messages),
                    },
                )
            {
                sink.emit(&events.error(&format!("post_compact hook failed: {error}")))?;
            }
        }

        let turn_prompt = if turn == 1 { Some(prompt) } else { None };
        if emit_deltas {
            sink.emit(&events.turn_started(turn, turn_prompt))?;
        }

        let route_decision = config.model.route(ModelRouteContext {
            subagent_type,
            subagent_model: None,
        });
        cost_tracker.set_model(Some(&route_decision.actual_model));
        if emit_deltas {
            sink.emit(&events.model_routed(&route_decision))?;
        }
        let mut turn_provider_config = provider_config.clone();
        turn_provider_config.model = Some(route_decision.actual_model.clone());

        let response = provider::call_streaming(
            config.provider,
            &conversation,
            &turn_provider_config,
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

        if emit_deltas
            && let Some(usage) = response.usage
            && !usage.is_empty()
        {
            let totals = cost_tracker.add_usage(usage);
            sink.emit(&events.usage_updated(totals))?;
            if let Some(writer) = history_writer.as_deref_mut() {
                writer.append_usage(totals)?;
            }
            if let Some(max_budget) = config.max_budget_usd
                && totals.estimated_cost_usd > max_budget
            {
                let error = format!(
                    "budget exhausted: estimated cost ${:.6} exceeded limit ${:.6}",
                    totals.estimated_cost_usd, max_budget
                );
                sink.emit(&events.error(&error))?;
                return Ok(AgentLoopResult::failure(RunStatus::BudgetExhausted, error));
            }
        }

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
            if emit_deltas && config.auto_memory {
                let provider_config = ProviderConfig {
                    api_key: config.api_key.clone(),
                    base_url: config.base_url.clone(),
                    model: Some(crate::model::auxiliary_model().to_string()),
                    tools_override: Some(Vec::new()),
                    mcp_registry: None,
                };
                if let Err(error) = memory::extract_project_memory(
                    config.provider,
                    &provider_config,
                    cwd,
                    &conversation.messages,
                ) {
                    sink.emit(&events.error(&format!("memory extraction failed: {error}")))?;
                }
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

        let tool_requests: Vec<tools::ToolRequest> = response
            .steps
            .iter()
            .filter_map(|step| match step {
                ProviderStep::ToolCall(tool_request) => Some(tool_request.clone()),
                _ => None,
            })
            .collect();
        let mut index = 0;
        while index < tool_requests.len() {
            if should_run_subagent_batch(config, &tool_requests[index], subagent_depth) {
                let batch_end = collect_subagent_batch(config, &tool_requests, index);
                let results = execute_subagent_batch(
                    config,
                    cwd,
                    events,
                    sink,
                    &tool_requests[index..batch_end],
                    subagent_depth,
                    emit_deltas,
                    instructions,
                    memory,
                    mcp_registry,
                    hooks,
                    cost_tracker,
                    cancel,
                )?;

                for (status, result) in results {
                    let result_content = agent_common::format_tool_result_for_model(&result);
                    conversation.add_tool_result(result.id.clone(), result_content);
                    if emit_deltas
                        && let Some(writer) = history_writer.as_deref_mut()
                        && let Some(message) = conversation.messages.last()
                    {
                        writer.append_message(message)?;
                    }

                    if status == RunStatus::ApprovalRequired {
                        return Ok(AgentLoopResult {
                            status,
                            final_message: None,
                            error: result.error.clone(),
                        });
                    }
                    if status == RunStatus::Failed {
                        return Ok(AgentLoopResult::failure(
                            RunStatus::Failed,
                            result.error.clone().unwrap_or_default(),
                        ));
                    }
                }
                index = batch_end;
                continue;
            }

            let tool_request = &tool_requests[index];
            let (status, result) = execute_tool_with_approval(
                config,
                cwd,
                events,
                sink,
                tool_request,
                subagent_depth,
                emit_deltas,
                &policy,
                instructions,
                memory,
                mcp_registry,
                hooks,
                cost_tracker,
                cancel,
            )?;

            if tool_request.name == tools::ToolName::UpdatePlan
                && result.status == tools::ToolStatus::Completed
            {
                if let Ok(update) = tools::update_plan::parse_args(tool_request) {
                    if let Some(writer) = history_writer.as_deref_mut() {
                        let _ = writer.append_plan_state(update.explanation, update.plan);
                    }
                }
            }

            let result_content = agent_common::format_tool_result_for_model(&result);
            conversation.add_tool_result(tool_request.id.clone(), result_content);
            if emit_deltas
                && let Some(writer) = history_writer.as_deref_mut()
                && let Some(message) = conversation.messages.last()
            {
                writer.append_message(message)?;
            }

            if status == RunStatus::ApprovalRequired {
                return Ok(AgentLoopResult {
                    status,
                    final_message: None,
                    error: result.error.clone(),
                });
            }
            if status == RunStatus::Failed && tool_request.name == tools::ToolName::Subagent {
                return Ok(AgentLoopResult::failure(
                    RunStatus::Failed,
                    result.error.clone().unwrap_or_default(),
                ));
            }
            index += 1;
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
    policy: &ApprovalPolicy,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
) -> io::Result<(RunStatus, tools::ToolResult)> {
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
        let resolution = policy.resolve_for_tool(
            &approval,
            tool_request.name.as_str(),
            tool_request.target.as_deref(),
        );
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
    if let Err(error) = hooks.run(
        HookEvent::PreToolUse,
        HookContext {
            cwd: &cwd.display().to_string(),
            session_status: None,
            tool_request: Some(tool_request),
            tool_result: None,
            before_messages: None,
            after_messages: None,
        },
    ) {
        let result = tools::ToolResult::failed(
            tool_request,
            format!("pre_tool_use hook blocked tool: {error}"),
            None,
        );
        if emit_deltas {
            sink.emit(&events.tool_call_completed(&result))?;
        }
        return Ok((RunStatus::Failed, result));
    }
    let result = if tool_request.name == tools::ToolName::Subagent {
        execute_subagent_tool(
            config,
            cwd,
            events,
            sink,
            tool_request,
            subagent_depth,
            instructions,
            memory,
            mcp_registry,
            hooks,
            emit_deltas,
            cost_tracker,
            cancel,
        )?
    } else {
        tools::execute_with_mcp(tool_request, cwd, mcp_registry)
    };
    let is_failure = matches!(
        result.status,
        tools::ToolStatus::Failed | tools::ToolStatus::Denied
    );
    if emit_deltas {
        sink.emit(&events.tool_call_completed(&result))?;
        if tool_request.name == tools::ToolName::UpdatePlan
            && result.status == tools::ToolStatus::Completed
        {
            match tools::update_plan::parse_args(tool_request) {
                Ok(update) => sink.emit(&events.plan_updated(&update))?,
                Err(error) => {
                    sink.emit(&events.error(&format!("failed to render plan update: {error}")))?
                }
            }
        }
        if let Err(error) = hooks.run(
            HookEvent::PostToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: Some(&result),
                before_messages: None,
                after_messages: None,
            },
        ) {
            sink.emit(&events.error(&format!("post_tool_use hook failed: {error}")))?;
        }
    }

    let status = if is_failure {
        RunStatus::Failed
    } else {
        RunStatus::Success
    };

    Ok((status, result))
}

fn should_run_subagent_batch(
    config: &RunConfig,
    tool_request: &tools::ToolRequest,
    subagent_depth: u32,
) -> bool {
    tool_request.name == tools::ToolName::Subagent
        && subagent_depth < config.subagents.max_depth
        && config.subagents.max_parallel > 1
}

fn collect_subagent_batch(
    config: &RunConfig,
    tool_requests: &[tools::ToolRequest],
    start: usize,
) -> usize {
    let max_end = (start + config.subagents.max_parallel).min(tool_requests.len());
    let mut end = start;
    while end < max_end && tool_requests[end].name == tools::ToolName::Subagent {
        end += 1;
    }
    end
}

#[allow(clippy::too_many_arguments)]
fn execute_subagent_batch(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_requests: &[tools::ToolRequest],
    subagent_depth: u32,
    emit_deltas: bool,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
) -> io::Result<Vec<(RunStatus, tools::ToolResult)>> {
    let mut handles = Vec::new();
    let mut results: Vec<Option<(RunStatus, tools::ToolResult)>> = vec![None; tool_requests.len()];

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        if emit_deltas {
            sink.emit(&events.tool_call_requested(tool_request))?;
        }
        if let Err(error) = hooks.run(
            HookEvent::PreToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: None,
                before_messages: None,
                after_messages: None,
            },
        ) {
            let result = tools::ToolResult::failed(
                tool_request,
                format!("pre_tool_use hook blocked tool: {error}"),
                None,
            );
            if emit_deltas {
                sink.emit(&events.tool_call_completed(&result))?;
            }
            results[idx] = Some((RunStatus::Failed, result));
            continue;
        }

        let request = subagent::create_subagent_request(tool_request);
        if emit_deltas {
            sink.emit(&events.subagent_started(&tool_request.id, &request.description))?;
        }

        let child_tool_request = tool_request.clone();
        let child_config = config.clone();
        let child_cwd = cwd.to_path_buf();
        let child_instructions = instructions.clone();
        let child_memory = memory.clone();
        let child_mcp_registry = mcp_registry.clone();
        let child_hooks = hooks.clone();
        let child_cancel = cancel.clone();
        handles.push((
            idx,
            thread::spawn(move || {
                let mut child_events =
                    EventFactory::new(format!("subagent-{}", child_tool_request.id));
                let mut child_sink = EventSink::new(io::sink(), child_config.output_format);
                let mut child_config = child_config;
                child_config.model = child_config
                    .model
                    .with_subagent_override(request.model.clone());
                let mut child_cost_tracker = CostTracker::new(child_config.model.as_deref());
                let child = run_agent_loop(
                    &child_config,
                    &child_cwd,
                    &mut child_events,
                    &mut child_sink,
                    &request.prompt,
                    None,
                    None,
                    subagent_depth + 1,
                    false,
                    &request.subagent_type,
                    &child_instructions,
                    &child_memory,
                    &child_mcp_registry,
                    &child_hooks,
                    &mut child_cost_tracker,
                    &child_cancel,
                )
                .unwrap_or_else(|error| {
                    AgentLoopResult::failure(RunStatus::Failed, error.to_string())
                });

                SubagentExecutionResult {
                    tool_request: child_tool_request,
                    description: request.description,
                    child,
                    cost_tracker: child_cost_tracker,
                }
            }),
        ));
    }

    for (idx, handle) in handles {
        let execution = match handle.join() {
            Ok(execution) => execution,
            Err(_) => {
                let tool_request = &tool_requests[idx];
                let result =
                    tools::ToolResult::failed(tool_request, "subagent thread panicked", None);
                if emit_deltas {
                    sink.emit(&events.tool_call_completed(&result))?;
                }
                results[idx] = Some((RunStatus::Failed, result));
                continue;
            }
        };

        cost_tracker.merge(&execution.cost_tracker);

        let (status, result) =
            subagent_execution_to_tool_result(events, sink, &execution, emit_deltas)?;
        if emit_deltas {
            sink.emit(&events.tool_call_completed(&result))?;
            if let Err(error) = hooks.run(
                HookEvent::PostToolUse,
                HookContext {
                    cwd: &cwd.display().to_string(),
                    session_status: None,
                    tool_request: Some(&execution.tool_request),
                    tool_result: Some(&result),
                    before_messages: None,
                    after_messages: None,
                },
            ) {
                sink.emit(&events.error(&format!("post_tool_use hook failed: {error}")))?;
            }
        }
        results[idx] = Some((status, result));
    }

    Ok(results
        .into_iter()
        .map(|result| result.expect("each subagent batch item has a result"))
        .collect())
}

fn subagent_execution_to_tool_result(
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    execution: &SubagentExecutionResult,
    emit_deltas: bool,
) -> io::Result<(RunStatus, tools::ToolResult)> {
    match execution.child.status {
        RunStatus::Success => {
            let output = execution
                .child
                .final_message
                .clone()
                .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
            if emit_deltas {
                sink.emit(&events.subagent_completed(
                    &execution.tool_request.id,
                    &execution.description,
                    execution.child.status,
                    Some(&output),
                    None,
                ))?;
            }
            Ok((
                RunStatus::Success,
                tools::ToolResult::completed(
                    &execution.tool_request,
                    format!("Subagent status: success\n\n{output}"),
                    false,
                ),
            ))
        }
        status => {
            let error = execution
                .child
                .error
                .clone()
                .unwrap_or_else(|| format!("subagent ended with status {status:?}"));
            if emit_deltas {
                sink.emit(&events.subagent_completed(
                    &execution.tool_request.id,
                    &execution.description,
                    status,
                    execution.child.final_message.as_deref(),
                    Some(&error),
                ))?;
            }
            Ok((
                RunStatus::Failed,
                tools::ToolResult::failed(
                    &execution.tool_request,
                    format!("Subagent status: {status:?}\n\n{error}"),
                    None,
                ),
            ))
        }
    }
}

fn execute_subagent_tool(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tools::ToolRequest,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    emit_deltas: bool,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
) -> io::Result<tools::ToolResult> {
    let request = subagent::create_subagent_request(tool_request);
    let description = request.description.clone();
    let subagent_type = request.subagent_type;

    if emit_deltas {
        sink.emit(&events.subagent_started(&tool_request.id, &description))?;
    }

    if subagent_depth >= config.subagents.max_depth {
        let error = format!("subagent max depth {} reached", config.subagents.max_depth);
        if emit_deltas {
            sink.emit(&events.subagent_completed(
                &tool_request.id,
                &description,
                RunStatus::Failed,
                None,
                Some(&error),
            ))?;
        }
        return Ok(tools::ToolResult::failed(tool_request, error, None));
    }

    let mut child_config = config.clone();
    child_config.model = child_config
        .model
        .with_subagent_override(request.model.clone());
    let mut child_cost_tracker = CostTracker::new(child_config.model.as_deref());
    let child = run_agent_loop(
        &child_config,
        cwd,
        events,
        sink,
        &request.prompt,
        None,
        None,
        subagent_depth + 1,
        false,
        &subagent_type,
        instructions,
        memory,
        mcp_registry,
        hooks,
        &mut child_cost_tracker,
        cancel,
    )?;

    cost_tracker.merge(&child_cost_tracker);

    match child.status {
        RunStatus::Success => {
            let output = child
                .final_message
                .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
            if emit_deltas {
                sink.emit(&events.subagent_completed(
                    &tool_request.id,
                    &description,
                    child.status,
                    Some(&output),
                    None,
                ))?;
            }
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
            if emit_deltas {
                sink.emit(&events.subagent_completed(
                    &tool_request.id,
                    &description,
                    status,
                    child.final_message.as_deref(),
                    Some(&error),
                ))?;
            }
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

fn load_project_instructions(cwd: &Path) -> ProjectInstructions {
    instructions::load_for_cwd_or_default(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::policy::{ActionKind, ApprovalMode};
    use crate::config::{HistoryMode, OutputFormat, ProviderKind};
    use crate::model::ModelSelection;
    use crate::runtime::subagent_config::SubagentConfig;

    fn config(subagents: SubagentConfig) -> RunConfig {
        RunConfig {
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            api_key: None,
            base_url: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            permission_rules: Default::default(),
            max_budget_usd: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            subagents,
            theme: crate::config::ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn subagent_request(id: &str) -> tools::ToolRequest {
        tools::ToolRequest {
            id: id.to_string(),
            name: tools::ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("task".to_string()),
            raw_arguments: None,
        }
    }

    #[test]
    fn subagent_batch_respects_parallel_limit() {
        let config = config(SubagentConfig::default());
        let requests = vec![
            subagent_request("a"),
            subagent_request("b"),
            subagent_request("c"),
            subagent_request("d"),
            subagent_request("e"),
        ];

        assert!(should_run_subagent_batch(&config, &requests[0], 0));
        assert_eq!(collect_subagent_batch(&config, &requests, 0), 4);
    }

    #[test]
    fn max_parallel_one_uses_sequential_subagent_path() {
        let config = config(
            SubagentConfig {
                max_depth: 2,
                max_parallel: 1,
            }
            .normalized(),
        );
        let request = subagent_request("a");

        assert!(!should_run_subagent_batch(&config, &request, 0));
    }

    #[test]
    fn subagent_batch_stops_at_first_non_subagent_tool() {
        let config = config(SubagentConfig::default());
        let mut requests = vec![subagent_request("a"), subagent_request("b")];
        requests.push(tools::ToolRequest {
            id: "read".to_string(),
            name: tools::ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("src/main.rs".to_string()),
            raw_arguments: None,
        });
        requests.push(subagent_request("c"));

        assert_eq!(collect_subagent_batch(&config, &requests, 0), 2);
    }
}
