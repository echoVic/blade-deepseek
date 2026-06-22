use std::io;
use std::path::Path;
use std::thread;

use orca_approval::{ApprovalPolicy, prompt_user};
use orca_core::approval_types::{ApprovalDecision, ApprovalRequest, ApprovalResolution};
use orca_core::cancel::CancelToken;
use orca_core::config::{HistoryMode, OutputFormat, RunConfig};
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::model::ModelRouteContext;
use orca_core::provider_types::ProviderStep;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types;
use orca_core::workflow_types::WorkflowInput;
use orca_mcp::McpRegistry;
use orca_provider::context;
use orca_provider::tool_schema::{
    deepseek_tools_schema_for_type_with_mcp_and_external,
    deepseek_tools_schema_with_mcp_and_external,
};
use orca_provider::{self, ProviderConfig};
use orca_tools;

use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime, run_child_agent};
use crate::agent_common;
use crate::cost::CostTracker;
use crate::history::{self, SessionWriter};
use crate::hooks::{
    HookContext, HookRunner, conversation_with_hook_context, tool_request_with_hook_outcome,
};
use crate::instructions::{self, ProjectInstructions};
use crate::memory::{self, MemoryBlock};
use crate::session::new_run_id;
use crate::subagent;
use crate::tasks::TaskRegistry;
use crate::workflow::{WorkflowBackgroundLaunch, WorkflowLaunchRequest, WorkflowRunner};
use orca_core::hook_types::HookEvent;

const DEFAULT_MAX_TURNS: u32 = 128;

#[derive(Clone, Copy, Debug)]
pub struct ControllerRunOptions {
    pub wait_for_background_workflows: bool,
}

impl ControllerRunOptions {
    fn for_run_config(config: &RunConfig) -> Self {
        Self {
            wait_for_background_workflows: config.output_format == OutputFormat::Jsonl,
        }
    }
}

#[derive(Clone, Debug)]
struct AgentLoopResult {
    status: RunStatus,
    final_message: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct SubagentExecutionResult {
    tool_request: tool_types::ToolRequest,
    description: String,
    child: ChildAgentResult,
    cost_tracker: CostTracker,
}

#[derive(Debug)]
struct BackgroundWorkflowRun {
    task_id: String,
    run_id: String,
    workflow_name: String,
    handle: WorkflowBackgroundLaunch,
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
    let stdout = io::stdout();
    let options = ControllerRunOptions::for_run_config(&config);
    match run_inner(config, stdout.lock(), options) {
        Ok(status) => status.exit_code(),
        Err(error) => {
            eprintln!("orca: {error}");
            RunStatus::Failed.exit_code()
        }
    }
}

pub fn run_to_writer<W: io::Write>(config: RunConfig, writer: W) -> i32 {
    let options = ControllerRunOptions::for_run_config(&config);
    run_to_writer_with_options(config, writer, options)
}

pub fn run_to_writer_with_options<W: io::Write>(
    config: RunConfig,
    writer: W,
    options: ControllerRunOptions,
) -> i32 {
    match run_inner(config, writer, options) {
        Ok(status) => status.exit_code(),
        Err(error) => {
            eprintln!("orca: {error}");
            RunStatus::Failed.exit_code()
        }
    }
}

fn run_inner<W: io::Write>(
    config: RunConfig,
    writer: W,
    options: ControllerRunOptions,
) -> io::Result<RunStatus> {
    let cwd_path = config.cwd.clone().unwrap_or(std::env::current_dir()?);
    let cwd = cwd_path.display().to_string();
    let prompt = if config.prompt.trim().is_empty() {
        "(empty prompt)".to_string()
    } else {
        config.prompt.trim().to_string()
    };

    let mut events = EventFactory::new(new_run_id());
    let task_registry = TaskRegistry::new(events.run_id().to_string());
    let mut background_workflows = Vec::new();
    let mut sink = EventSink::new(writer, config.output_format);
    let instructions = load_project_instructions(&cwd_path);
    let memory = memory::load_for_cwd(&cwd_path);
    let hooks = HookRunner::new(config.hooks.clone());
    let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
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
            usage: None,
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
        &task_registry,
        &mut background_workflows,
    )?;
    let status = result.status;

    observe_background_workflows(options, &mut events, &mut sink, &mut background_workflows)?;

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
            usage: None,
        },
    ) {
        sink.emit(&events.error(&format!("session_end hook failed: {error}")))?;
    }

    sink.emit(&events.session_completed(status))?;
    if config.desktop_notifications {
        let _ = crate::notify::notify("Orca", &format!("Session {}", status.as_str()));
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
    task_registry: &TaskRegistry,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
) -> io::Result<AgentLoopResult> {
    let max_turns = DEFAULT_MAX_TURNS;
    let budget_model = config.model.as_option();
    let ctx_config = context::ContextConfig::for_model_with_runtime(
        budget_model.as_deref(),
        &config.model_runtime,
    );
    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());
    let tools_override = if subagent_depth > 0 {
        Some(deepseek_tools_schema_for_type_with_mcp_and_external(
            subagent_type,
            Some(mcp_registry),
            &config.external_tools,
        ))
    } else {
        Some(deepseek_tools_schema_with_mcp_and_external(
            Some(mcp_registry),
            &config.external_tools,
        ))
    };
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: config.model.as_option(),
        tools_override,
        mcp_registry: Some(mcp_registry.clone()),
        external_tools: config.external_tools.clone(),
    };

    let mut system_prompt = agent_common::build_agent_system_prompt(
        cwd,
        subagent_depth,
        subagent_type,
        Some(instructions),
        config.approval_mode,
        Some(memory),
    );
    agent_common::append_explicit_skill_context(&mut system_prompt, cwd, prompt);
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
    let mut reactive_compacted = false;

    loop {
        turn += 1;

        if turn > max_turns {
            let error = "max turns exhausted";
            if emit_deltas {
                sink.emit(&events.error(error))?;
            }
            return Ok(AgentLoopResult::failure(RunStatus::BudgetExhausted, error));
        }

        if context::needs_compaction(&conversation, &ctx_config) {
            let before_messages = conversation.messages.len();
            match hooks.run(
                HookEvent::OnBudgetWarning,
                HookContext {
                    cwd: &cwd.display().to_string(),
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: Some(before_messages),
                    after_messages: None,
                    usage: None,
                },
            ) {
                Ok(outcome) if !outcome.injected_context.is_empty() => {
                    conversation = conversation_with_hook_context(&conversation, &outcome);
                }
                Err(error) if emit_deltas => {
                    sink.emit(&events.error(&format!("on_budget_warning hook failed: {error}")))?;
                }
                _ => {}
            }
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
                        usage: None,
                    },
                )
            {
                sink.emit(&events.error(&format!("pre_compact hook failed: {error}")))?;
            }
            let compaction = context::compact_with_summary(
                config.provider,
                &conversation,
                &ctx_config,
                &provider_config,
            );
            conversation = compaction.conversation;
            let after_messages = conversation.messages.len();
            if emit_deltas && let Some(writer) = history_writer.as_deref_mut() {
                writer.append_compaction(before_messages, after_messages)?;
                if let context::CompactionKind::RemoteSummary(summary) = compaction.kind {
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
                        usage: None,
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
        let turn_ctx_config = context::ContextConfig::for_model_with_runtime(
            Some(&route_decision.actual_model),
            &config.model_runtime,
        );
        context::apply_context_budget_hint_with_config(&mut conversation, &turn_ctx_config);

        let pre_model_outcome = match hooks.run(
            HookEvent::PreModelCall,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: None,
            },
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let error = format!("pre_model_call hook failed: {error}");
                if emit_deltas {
                    sink.emit(&events.error(&error))?;
                }
                return Ok(AgentLoopResult::failure(RunStatus::Failed, error));
            }
        };
        let model_conversation = conversation_with_hook_context(&conversation, &pre_model_outcome);

        let response = orca_provider::call_streaming(
            config.provider,
            &model_conversation,
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

        if let Err(error) = hooks.run(
            HookEvent::PostModelCall,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: response.usage.as_ref(),
            },
        ) && emit_deltas
        {
            sink.emit(&events.error(&format!("post_model_call hook failed: {error}")))?;
        }

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

        let provider_error = response.steps.iter().find_map(|step| match step {
            ProviderStep::Error(message) => Some(message.clone()),
            _ => None,
        });

        if let Some(error) = provider_error {
            if context::is_prompt_too_long_error(&error) && !reactive_compacted {
                let before_messages = conversation.messages.len();
                let compaction = context::compact_with_summary(
                    config.provider,
                    &conversation,
                    &ctx_config,
                    &provider_config,
                );
                conversation = compaction.conversation;
                let after_messages = conversation.messages.len();
                if emit_deltas && let Some(writer) = history_writer.as_deref_mut() {
                    writer.append_compaction(before_messages, after_messages)?;
                    if let context::CompactionKind::RemoteSummary(summary) = compaction.kind {
                        writer.append_summary(before_messages, after_messages, summary)?;
                    }
                }
                reactive_compacted = true;
                continue;
            }
            if emit_deltas {
                sink.emit(&events.error(&error))?;
            }
            return Ok(AgentLoopResult::failure(RunStatus::Failed, error));
        }

        reactive_compacted = false;

        for step in &response.steps {
            match step {
                ProviderStep::ReplayState(replay) => {
                    if emit_deltas {
                        sink.emit(&events.provider_replay_updated(replay))?;
                    }
                }
                _ => {}
            }
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
                    model: Some(orca_core::model::auxiliary_model().to_string()),
                    tools_override: Some(Vec::new()),
                    mcp_registry: None,
                    external_tools: Vec::new(),
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

        let tool_requests: Vec<tool_types::ToolRequest> = response
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

            if orca_tools::should_run_readonly_batch(
                config.tools.max_read_parallel,
                &tool_requests[index],
            ) {
                let batch_end = orca_tools::collect_readonly_batch(
                    config.tools.max_read_parallel,
                    &tool_requests,
                    index,
                );
                let results = execute_readonly_batch(
                    cwd,
                    events,
                    sink,
                    &tool_requests[index..batch_end],
                    emit_deltas,
                    mcp_registry,
                    hooks,
                    config.tools.output_truncation,
                )?;

                for result in results {
                    let result_content = agent_common::format_tool_result_for_model(&result);
                    conversation.add_tool_result(result.id.clone(), result_content);
                    if emit_deltas
                        && let Some(writer) = history_writer.as_deref_mut()
                        && let Some(message) = conversation.messages.last()
                    {
                        writer.append_message(message)?;
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
                task_registry,
                background_workflows,
            )?;

            if tool_request.name == tool_types::ToolName::UpdatePlan
                && result.status == tool_types::ToolStatus::Completed
            {
                if let Ok(update) = orca_tools::update_plan::parse_args(tool_request) {
                    conversation.replace_plan_state(
                        orca_tools::update_plan::format_context_message(&update),
                    );
                    if emit_deltas
                        && let Some(writer) = history_writer.as_deref_mut()
                        && let Some(message) = conversation.messages.last()
                    {
                        writer.append_message(message)?;
                    }
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
            if status == RunStatus::Failed && tool_request.name == tool_types::ToolName::Subagent {
                return Ok(AgentLoopResult::failure(
                    RunStatus::Failed,
                    result.error.clone().unwrap_or_default(),
                ));
            }
            index += 1;
        }
    }
}

pub(crate) fn execute_child_agent_loop<W: io::Write>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    runtime: &mut ChildAgentRuntime<'_, W>,
    child_cost_tracker: &mut CostTracker,
) -> io::Result<ChildAgentResult> {
    let task_registry = TaskRegistry::new(runtime.events.run_id().to_string());
    let mut background_workflows = Vec::new();
    let child = run_agent_loop(
        config,
        runtime.cwd,
        runtime.events,
        runtime.sink,
        &request.prompt,
        None,
        None,
        request.depth,
        request.emit_deltas,
        &request.subagent_type,
        runtime.instructions,
        runtime.memory,
        runtime.mcp_registry,
        runtime.hooks,
        child_cost_tracker,
        runtime.cancel,
        &task_registry,
        &mut background_workflows,
    )?;
    observe_background_workflows(
        ControllerRunOptions::for_run_config(config),
        runtime.events,
        runtime.sink,
        &mut background_workflows,
    )?;
    Ok(ChildAgentResult {
        status: child.status,
        final_message: child.final_message,
        error: child.error,
    })
}

fn approval_action_for_tool(
    tool_request: &tool_types::ToolRequest,
    subagent_depth: u32,
    mcp_registry: &McpRegistry,
    config: &RunConfig,
) -> Option<orca_core::approval_types::ActionKind> {
    if tool_request.name == tool_types::ToolName::Subagent
        && subagent_depth >= config.subagents.max_depth
    {
        return None;
    }
    Some(orca_tools::canonical_action_kind_with_mcp_and_external(
        tool_request,
        Some(mcp_registry),
        &config.external_tools,
    ))
}

#[cfg(test)]
fn canonical_action_for_tool(
    tool_request: &tool_types::ToolRequest,
    mcp_registry: &McpRegistry,
    external_tools: &[orca_core::external_config::ExternalToolConfig],
) -> orca_core::approval_types::ActionKind {
    orca_tools::canonical_action_kind_with_mcp_and_external(
        tool_request,
        Some(mcp_registry),
        external_tools,
    )
}

fn execute_tool_with_approval(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tool_types::ToolRequest,
    subagent_depth: u32,
    emit_deltas: bool,
    policy: &ApprovalPolicy,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
    task_registry: &TaskRegistry,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
) -> io::Result<(RunStatus, tool_types::ToolResult)> {
    if let Some(action) =
        approval_action_for_tool(tool_request, subagent_depth, mcp_registry, config)
        && agent_common::requires_approval(action)
    {
        let approval = ApprovalRequest {
            id: format!("approval-{}", tool_request.id),
            action,
            description: format!(
                "{} requested {}",
                tool_request.name.as_str(),
                action.as_str()
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
                    let result =
                        tool_types::ToolResult::denied(tool_request, final_resolution.reason);
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
                let result = tool_types::ToolResult::denied(tool_request, resolution.reason);
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
    let pre_tool_outcome = match hooks.run(
        HookEvent::PreToolUse,
        HookContext {
            cwd: &cwd.display().to_string(),
            session_status: None,
            tool_request: Some(tool_request),
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        },
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            let result = tool_types::ToolResult::failed(
                tool_request,
                format!("pre_tool_use hook blocked tool: {error}"),
                None,
            );
            if emit_deltas {
                sink.emit(&events.tool_call_completed(&result))?;
            }
            return Ok((RunStatus::Failed, result));
        }
    };
    let effective_tool_request = tool_request_with_hook_outcome(tool_request, &pre_tool_outcome);
    let execution_request = &effective_tool_request;
    let result = if execution_request.name == tool_types::ToolName::Workflow {
        execute_workflow_tool(
            config,
            cwd,
            events,
            sink,
            execution_request,
            emit_deltas,
            task_registry,
            background_workflows,
        )?
    } else if execution_request.name == tool_types::ToolName::Subagent {
        execute_subagent_tool(
            config,
            cwd,
            events,
            sink,
            execution_request,
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
        orca_tools::execute_with_mcp_external_and_policy(
            execution_request,
            cwd,
            mcp_registry,
            &config.external_tools,
            config.tools.output_truncation,
        )
    };
    let is_failure = matches!(
        result.status,
        tool_types::ToolStatus::Failed | tool_types::ToolStatus::Denied
    );
    if emit_deltas {
        sink.emit(&events.tool_call_completed(&result))?;
        if execution_request.name == tool_types::ToolName::UpdatePlan
            && result.status == tool_types::ToolStatus::Completed
        {
            match orca_tools::update_plan::parse_args(execution_request) {
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
                tool_request: Some(execution_request),
                tool_result: Some(&result),
                before_messages: None,
                after_messages: None,
                usage: None,
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

fn execute_workflow_tool(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tool_types::ToolRequest,
    emit_deltas: bool,
    task_registry: &TaskRegistry,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
) -> io::Result<tool_types::ToolResult> {
    if !config.workflows.enabled {
        return Ok(tool_types::ToolResult::failed(
            tool_request,
            "workflows are disabled",
            None,
        ));
    }

    let input = parse_workflow_input(tool_request)?;
    let session_dir = cwd
        .join(".orca")
        .join("workflow-sessions")
        .join(task_registry.session_id());
    let runner = WorkflowRunner::new(config.clone(), task_registry.clone(), session_dir);
    let launch = runner.launch_background(WorkflowLaunchRequest::from(input))?;
    if emit_deltas {
        sink.emit(&events.workflow_started(
            &launch.task_id,
            &launch.run_id,
            &launch.workflow_name,
            &launch.phases,
        ))?;
    }
    let output = serde_json::to_string(&launch.output)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    background_workflows.push(BackgroundWorkflowRun {
        task_id: launch.task_id.clone(),
        run_id: launch.run_id.clone(),
        workflow_name: launch.workflow_name.clone(),
        handle: launch,
    });

    Ok(tool_types::ToolResult::completed(
        tool_request,
        output,
        false,
    ))
}

fn parse_workflow_input(tool_request: &tool_types::ToolRequest) -> io::Result<WorkflowInput> {
    let raw_arguments = tool_request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn observe_background_workflows(
    options: ControllerRunOptions,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
) -> io::Result<()> {
    if !options.wait_for_background_workflows {
        return Ok(());
    }

    for workflow in background_workflows.drain(..) {
        match workflow.handle.join() {
            Ok(Ok(result)) => {
                sink.emit(&events.workflow_completed(
                    &workflow.task_id,
                    &workflow.run_id,
                    &workflow.workflow_name,
                ))?;
                sink.emit(&events.workflow_result_available(
                    &workflow.task_id,
                    &workflow.run_id,
                    &result.summary,
                ))?;
            }
            Ok(Err(error)) => {
                sink.emit(&events.workflow_failed(
                    &workflow.task_id,
                    &workflow.run_id,
                    &error.to_string(),
                ))?;
            }
            Err(_) => {
                sink.emit(&events.workflow_failed(
                    &workflow.task_id,
                    &workflow.run_id,
                    "workflow thread panicked",
                ))?;
            }
        }
    }

    Ok(())
}

fn should_run_subagent_batch(
    config: &RunConfig,
    tool_request: &tool_types::ToolRequest,
    subagent_depth: u32,
) -> bool {
    tool_request.name == tool_types::ToolName::Subagent
        && subagent_depth < config.subagents.max_depth
        && config.subagents.max_parallel > 1
}

fn collect_subagent_batch(
    config: &RunConfig,
    tool_requests: &[tool_types::ToolRequest],
    start: usize,
) -> usize {
    let max_end = (start + config.subagents.max_parallel).min(tool_requests.len());
    let mut end = start;
    while end < max_end && tool_requests[end].name == tool_types::ToolName::Subagent {
        end += 1;
    }
    end
}

fn execute_readonly_batch(
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_requests: &[tool_types::ToolRequest],
    emit_deltas: bool,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    output_truncation: tool_types::ToolOutputTruncation,
) -> io::Result<Vec<tool_types::ToolResult>> {
    let mut hook_failed: Vec<Option<tool_types::ToolResult>> = vec![None; tool_requests.len()];
    let mut runnable = Vec::new();

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        if emit_deltas {
            sink.emit(&events.tool_call_requested(tool_request))?;
        }
        match hooks.run(
            HookEvent::PreToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: None,
            },
        ) {
            Ok(outcome) => {
                runnable.push((idx, tool_request_with_hook_outcome(tool_request, &outcome)));
            }
            Err(error) => {
                hook_failed[idx] = Some(tool_types::ToolResult::failed(
                    tool_request,
                    format!("pre_tool_use hook blocked tool: {error}"),
                    None,
                ));
            }
        }
    }

    let mut results = orca_tools::run_readonly_batch_parallel_with_policy(
        tool_requests,
        runnable,
        cwd,
        mcp_registry,
        output_truncation,
    );

    for (idx, failed) in hook_failed.into_iter().enumerate() {
        if let Some(result) = failed {
            results[idx] = result;
        }
    }

    for (tool_request, result) in tool_requests.iter().zip(results.iter()) {
        if emit_deltas {
            sink.emit(&events.tool_call_completed(result))?;
            if let Err(error) = hooks.run(
                HookEvent::PostToolUse,
                HookContext {
                    cwd: &cwd.display().to_string(),
                    session_status: None,
                    tool_request: Some(tool_request),
                    tool_result: Some(result),
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            ) {
                sink.emit(&events.error(&format!("post_tool_use hook failed: {error}")))?;
            }
        }
    }

    Ok(results)
}

#[allow(clippy::too_many_arguments)]
fn execute_subagent_batch(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_requests: &[tool_types::ToolRequest],
    subagent_depth: u32,
    emit_deltas: bool,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
) -> io::Result<Vec<(RunStatus, tool_types::ToolResult)>> {
    let mut handles = Vec::new();
    let mut results: Vec<Option<(RunStatus, tool_types::ToolResult)>> =
        vec![None; tool_requests.len()];

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        if emit_deltas {
            sink.emit(&events.tool_call_requested(tool_request))?;
        }
        let pre_tool_outcome = match hooks.run(
            HookEvent::PreToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: None,
            },
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let result = tool_types::ToolResult::failed(
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
        };
        let effective_tool_request =
            tool_request_with_hook_outcome(tool_request, &pre_tool_outcome);

        let request = subagent::create_subagent_request(&effective_tool_request);
        if emit_deltas {
            sink.emit(&events.subagent_started(&tool_request.id, &request.description))?;
        }

        let child_request = ChildAgentRequest {
            prompt: request.prompt.clone(),
            subagent_type: request.subagent_type,
            model: request.model.clone(),
            depth: subagent_depth + 1,
            emit_deltas: false,
        };
        let child_tool_request = effective_tool_request;
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
                let mut child_runtime = ChildAgentRuntime::new(
                    &child_cwd,
                    &mut child_events,
                    &mut child_sink,
                    &child_instructions,
                    &child_memory,
                    &child_mcp_registry,
                    &child_hooks,
                    &child_cancel,
                    execute_child_agent_loop,
                );
                let (child, child_cost_tracker) =
                    run_child_agent(&child_config, &child_request, &mut child_runtime);

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
                    tool_types::ToolResult::failed(tool_request, "subagent thread panicked", None);
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
                    usage: None,
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
) -> io::Result<(RunStatus, tool_types::ToolResult)> {
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
                tool_types::ToolResult::completed(
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
                tool_types::ToolResult::failed(
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
    tool_request: &tool_types::ToolRequest,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    emit_deltas: bool,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
) -> io::Result<tool_types::ToolResult> {
    let request = subagent::create_subagent_request(tool_request);
    let description = request.description.clone();

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
        return Ok(tool_types::ToolResult::failed(tool_request, error, None));
    }

    let child_request = ChildAgentRequest {
        prompt: request.prompt,
        subagent_type: request.subagent_type,
        model: request.model,
        depth: subagent_depth + 1,
        emit_deltas: false,
    };
    let mut runtime = ChildAgentRuntime::new(
        cwd,
        events,
        sink,
        instructions,
        memory,
        mcp_registry,
        hooks,
        cancel,
        execute_child_agent_loop,
    );
    let (child, child_cost_tracker) = run_child_agent(config, &child_request, &mut runtime);

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
            Ok(tool_types::ToolResult::completed(
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
            Ok(tool_types::ToolResult::failed(
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
    tool_request: &tool_types::ToolRequest,
) -> io::Result<ApprovalResolution> {
    if config.output_format == OutputFormat::Jsonl {
        return Ok(ApprovalResolution {
            id: approval.id.clone(),
            decision: ApprovalDecision::Deny,
            reason: "interactive confirmation unavailable in jsonl mode".to_string(),
        });
    }

    let allowed = prompt_user(tool_request.name.as_str(), tool_request.target.as_deref())?;

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
    let result = orca_core::verification::run(command);
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
    use crate::hooks::HookOutcome;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{HistoryMode, OutputFormat, ProviderKind};
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;

    fn config(subagents: SubagentConfig) -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: Default::default(),
            api_key: None,
            base_url: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            permission_rules: Default::default(),
            max_budget_usd: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            subagents,
            tools: Default::default(),
            workflows: Default::default(),
            theme: orca_core::config::ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn subagent_request(id: &str) -> tool_types::ToolRequest {
        tool_types::ToolRequest {
            id: id.to_string(),
            name: tool_types::ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("task".to_string()),
            raw_arguments: None,
        }
    }

    fn tool_request(
        id: &str,
        name: tool_types::ToolName,
        action: ActionKind,
    ) -> tool_types::ToolRequest {
        tool_types::ToolRequest {
            id: id.to_string(),
            name,
            action,
            target: Some("target".to_string()),
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
        requests.push(tool_types::ToolRequest {
            id: "read".to_string(),
            name: tool_types::ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("src/main.rs".to_string()),
            raw_arguments: None,
        });
        requests.push(subagent_request("c"));

        assert_eq!(collect_subagent_batch(&config, &requests, 0), 2);
    }

    #[test]
    fn readonly_batch_respects_parallel_limit() {
        let mut config = config(SubagentConfig::default());
        config.tools.max_read_parallel = 2;
        let requests = vec![
            tool_request("a", tool_types::ToolName::ReadFile, ActionKind::Read),
            tool_request("b", tool_types::ToolName::Grep, ActionKind::Read),
            tool_request("c", tool_types::ToolName::ListFiles, ActionKind::Read),
        ];

        assert!(orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &requests[0]
        ));
        assert_eq!(
            orca_tools::collect_readonly_batch(config.tools.max_read_parallel, &requests, 0),
            2
        );
    }

    #[test]
    fn readonly_batch_stops_at_first_mutating_tool() {
        let config = config(SubagentConfig::default());
        let requests = vec![
            tool_request("a", tool_types::ToolName::ReadFile, ActionKind::Read),
            tool_request("b", tool_types::ToolName::Bash, ActionKind::Shell),
            tool_request("c", tool_types::ToolName::Grep, ActionKind::Read),
        ];

        assert_eq!(
            orca_tools::collect_readonly_batch(config.tools.max_read_parallel, &requests, 0),
            1
        );
        assert!(!orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &requests[1]
        ));
    }

    #[test]
    fn readonly_batch_uses_spec_not_request_action() {
        let config = config(SubagentConfig::default());
        let request = tool_request("a", tool_types::ToolName::ReadFile, ActionKind::Write);

        assert!(orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &request
        ));
    }

    #[test]
    fn readonly_batch_rejects_shell_by_capability() {
        let config = config(SubagentConfig::default());
        let request = tool_request("bash", tool_types::ToolName::Bash, ActionKind::Read);

        assert!(!orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &request
        ));
    }

    #[test]
    fn approval_action_rejects_caller_supplied_read_for_shell() {
        let request = tool_request("bash", tool_types::ToolName::Bash, ActionKind::Read);
        let registry = McpRegistry::default();

        assert_eq!(
            canonical_action_for_tool(&request, &registry, &[]),
            ActionKind::Shell
        );
    }

    #[test]
    fn readonly_batch_skips_network_actions() {
        let config = config(SubagentConfig::default());
        let request = tool_request("a", tool_types::ToolName::WebSearch, ActionKind::Network);

        assert!(!orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &request
        ));
    }

    #[test]
    fn readonly_batch_executes_results_in_request_order() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(dir.path().join("b.txt"), "bravo").unwrap();
        let requests = vec![
            tool_types::ToolRequest {
                target: Some("a.txt".to_string()),
                ..tool_request("first", tool_types::ToolName::ReadFile, ActionKind::Read)
            },
            tool_types::ToolRequest {
                target: Some("b.txt".to_string()),
                ..tool_request("second", tool_types::ToolName::ReadFile, ActionKind::Read)
            },
        ];
        let mut events = EventFactory::new("test-run".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let results = execute_readonly_batch(
            dir.path(),
            &mut events,
            &mut sink,
            &requests,
            true,
            &registry,
            &hooks,
            tool_types::ToolOutputTruncation::default(),
        )
        .unwrap();

        assert_eq!(
            results
                .iter()
                .map(|result| result.id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert_eq!(results[0].output.as_deref(), Some("alpha"));
        assert_eq!(results[1].output.as_deref(), Some("bravo"));
    }

    #[test]
    fn pre_model_hook_context_is_added_as_pinned_system_message() {
        let mut conversation = Conversation::new();
        conversation.add_system("base system".to_string());
        conversation.add_user("do work".to_string());
        let outcome = HookOutcome {
            modified_target: None,
            injected_context: vec!["policy hint".to_string(), "repo hint".to_string()],
        };

        let model_conversation = conversation_with_hook_context(&conversation, &outcome);

        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(model_conversation.messages.len(), 3);
        assert!(matches!(
            model_conversation.messages.last(),
            Some(orca_core::conversation::Message::System { content, pinned: true })
                if content.contains("policy hint") && content.contains("repo hint")
        ));
    }
}
