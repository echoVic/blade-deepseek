use std::io;

use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    AgentLoopContext, RuntimeCompactionStep, RuntimeProviderErrorOutcome, RuntimeProviderTurnStep,
    RuntimeSessionLifecycle, RuntimeSteerStep, RuntimeTaskActor, RuntimeTurnConfig,
    RuntimeTurnDeps, RuntimeTurnExecution, RuntimeTurnState, TurnPermissionOverlay,
};
use crate::memory::{self, MemoryBlock};
use crate::session::{
    AgentConversationContext, bootstrap_agent_conversation_for_loop,
    record_assistant_response_for_agent, record_initial_history_for_agent,
};
use crate::subagent_execution::{
    SubagentBatchRecordOutcome, collect_subagent_batch, execute_subagent_batch,
    record_subagent_batch_results, should_run_subagent_batch,
};
use crate::tasks::TaskRegistry;
use crate::tool_execution::policy_for_tool_execution;
use crate::tool_invocation::{
    AgentToolPolicyContext, ToolRequestCursor, ToolTurnOutcome, collect_readonly_batch,
    provider_config_for_agent_loop, reject_disallowed_child_tool, run_normal_tool_turn,
    run_readonly_tool_turn, should_run_readonly_batch, terminal_tool_turn,
    tool_requests_from_provider_steps,
};
use crate::workflow_execution::observe_background_workflows;
use orca_core::cancel::CancelToken;
use orca_core::config::{OutputFormat, RunConfig};
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;
use orca_provider;
use orca_provider::context;

const DEFAULT_MAX_TURNS: u32 = 128;

#[derive(Clone, Debug)]
pub(crate) struct AgentLoopResult {
    pub(crate) status: RunStatus,
    pub(crate) final_message: Option<String>,
    pub(crate) error: Option<String>,
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
        Self::terminal(status, Some(error.into()))
    }

    fn terminal(status: RunStatus, error: Option<String>) -> Self {
        Self {
            status,
            final_message: None,
            error,
        }
    }
}

pub(crate) fn run_agent_loop(
    config: &RunConfig,
    loop_context: AgentLoopContext<'_>,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    conversation_context: AgentConversationContext<'_>,
    tool_policy: AgentToolPolicyContext<'_>,
) -> io::Result<AgentLoopResult> {
    let AgentConversationContext {
        resumed,
        history_writer,
        conversation,
    } = conversation_context;
    let AgentLoopContext {
        turn_config:
            RuntimeTurnConfig {
                cwd,
                prompt,
                subagent_depth,
                emit_deltas,
                subagent_type,
            },
        turn_deps,
        turn_state,
        turn_execution,
        steer_handle,
        permission_handler,
    } = loop_context;
    let RuntimeTurnDeps {
        instructions,
        memory,
        mcp_registry,
        hooks,
    } = turn_deps.expect("agent loop turn deps");
    let RuntimeTurnState {
        cost_tracker,
        cancel,
        task_registry,
    } = turn_state.expect("agent loop turn state");
    let RuntimeTurnExecution {
        background_workflows,
        workflow_ipc,
        lifecycle,
    } = turn_execution.expect("agent loop turn execution");
    let max_turns = DEFAULT_MAX_TURNS;
    let budget_model = config.model.as_option();
    let ctx_config = context::ContextConfig::for_model_with_runtime(
        budget_model.as_deref(),
        &config.model_runtime,
    );
    let policy = policy_for_tool_execution(config);
    let provider_config = provider_config_for_agent_loop(
        config,
        subagent_depth,
        subagent_type,
        tool_policy,
        mcp_registry,
    );

    let mut owned_conversation;
    let conversation = if let Some(conversation) = conversation {
        conversation
    } else {
        owned_conversation = bootstrap_agent_conversation_for_loop(
            resumed,
            cwd,
            prompt,
            subagent_depth,
            subagent_type,
            instructions,
            config.approval_mode,
            memory,
        );
        &mut owned_conversation
    };

    let mut history_writer = history_writer;
    record_initial_history_for_agent(
        conversation,
        history_writer.as_deref_mut(),
        resumed.is_some(),
        emit_deltas,
    )?;

    let mut legacy_lifecycle = RuntimeSessionLifecycle::new(events.run_id().to_string());
    let lifecycle = lifecycle.unwrap_or(&mut legacy_lifecycle);
    let mut actor = RuntimeTaskActor::new(lifecycle, max_turns);
    let mut reactive_compacted = false;

    loop {
        RuntimeCompactionStep::new(
            config.provider,
            &ctx_config,
            &provider_config,
            cwd,
            emit_deltas,
            hooks,
            events,
            sink,
            history_writer.as_deref_mut(),
        )
        .compact_if_needed(conversation)?;

        let turn_prompt = if actor
            .active_task()
            .map(|task| task.current_turn())
            .unwrap_or(0)
            == 0
        {
            Some(prompt)
        } else {
            None
        };
        let started_turn = match actor.start_turn(events, turn_prompt, emit_deltas) {
            Ok(started_turn) => started_turn,
            Err(error) => {
                if emit_deltas {
                    sink.emit(&events.error(&error.message))?;
                }
                return Ok(AgentLoopResult::failure(error.status, error.message));
            }
        };
        if let Some(event) = started_turn.into_event() {
            sink.emit(&event)?;
        }

        let routed_model = actor.route_model_turn(
            &config.model,
            subagent_type,
            None,
            &provider_config,
            cost_tracker,
        );
        if emit_deltas {
            sink.emit(&events.model_routed(&routed_model.decision))?;
        }
        let turn_provider_config = routed_model.provider_config;

        RuntimeSteerStep::new().apply(steer_handle, conversation, history_writer.as_deref_mut())?;

        let cwd_display = cwd.display().to_string();
        let provider_turn = RuntimeProviderTurnStep::new().run(
            &mut actor,
            config.provider,
            conversation,
            &turn_provider_config,
            &cwd_display,
            emit_deltas,
            hooks,
            cancel,
            cost_tracker,
            config.max_budget_usd,
            events,
            sink,
            history_writer.as_deref_mut(),
        )?;
        let response = match provider_turn.response {
            Some(response) => response,
            None => {
                let error = provider_turn
                    .terminal_error
                    .expect("provider turn terminal");
                if emit_deltas && error.status != RunStatus::Cancelled {
                    sink.emit(&events.error(&error.message))?;
                }
                return Ok(AgentLoopResult::failure(error.status, error.message));
            }
        };

        let mut provider_turn_step = RuntimeProviderTurnStep::new();
        match provider_turn_step.handle_provider_error(
            &response,
            &mut RuntimeCompactionStep::new(
                config.provider,
                &ctx_config,
                &provider_config,
                cwd,
                emit_deltas,
                hooks,
                events,
                sink,
                history_writer.as_deref_mut(),
            ),
            conversation,
            reactive_compacted,
        )? {
            RuntimeProviderErrorOutcome::ContinueAfterCompaction => {
                reactive_compacted = true;
                continue;
            }
            RuntimeProviderErrorOutcome::Failed(error) => {
                return Ok(AgentLoopResult::failure(RunStatus::Failed, error));
            }
            RuntimeProviderErrorOutcome::NoError => {}
        }

        reactive_compacted = false;

        if response.tool_calls.is_empty() {
            let final_message = response.assistant_content.clone();
            record_assistant_response_for_agent(
                conversation,
                history_writer.as_deref_mut(),
                response.assistant_content,
                response.assistant_reasoning,
                vec![],
                emit_deltas,
            )?;
            if emit_deltas && config.auto_memory {
                memory::extract_project_memory_after_final_response(
                    config,
                    cwd,
                    &conversation.messages,
                    events,
                    sink,
                )?;
            }
            return Ok(AgentLoopResult::success(final_message));
        }

        record_assistant_response_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
            emit_deltas,
        )?;

        let tool_requests = tool_requests_from_provider_steps(&response.steps);
        let mut cursor = ToolRequestCursor::new(&tool_requests);
        let mut permission_overlay = TurnPermissionOverlay::default();
        while let Some(tool_request) = cursor.current() {
            if let Some(result) = reject_disallowed_child_tool(
                tool_request,
                tool_policy,
                mcp_registry,
                &config.external_tools,
            ) {
                if emit_deltas {
                    sink.emit(&events.tool_call_requested(tool_request))?;
                    sink.emit(&events.tool_call_completed(&result))?;
                }
                return Ok(AgentLoopResult::failure(
                    RunStatus::Failed,
                    result.error.clone().unwrap_or_default(),
                ));
            }

            if should_run_subagent_batch(config, tool_request, subagent_depth) {
                let batch_end = collect_subagent_batch(config, &tool_requests, cursor.position());
                let results = execute_subagent_batch(
                    config,
                    cwd,
                    events,
                    sink,
                    &tool_requests[cursor.position()..batch_end],
                    subagent_depth,
                    emit_deltas,
                    instructions,
                    memory,
                    mcp_registry,
                    hooks,
                    cost_tracker,
                    cancel,
                    workflow_ipc,
                    execute_child_agent_loop,
                )?;

                match record_subagent_batch_results(
                    conversation,
                    history_writer.as_deref_mut(),
                    results,
                    emit_deltas,
                )? {
                    SubagentBatchRecordOutcome::Continue => {}
                    SubagentBatchRecordOutcome::Return { status, error } => {
                        match terminal_tool_turn(status, error) {
                            ToolTurnOutcome::Continue => {}
                            ToolTurnOutcome::Return { status, error } => {
                                return Ok(AgentLoopResult::terminal(status, error));
                            }
                        }
                    }
                }
                cursor.advance_to(batch_end);
                continue;
            }

            if should_run_readonly_batch(config.tools.max_read_parallel, tool_request) {
                let batch_end = collect_readonly_batch(
                    config.tools.max_read_parallel,
                    &tool_requests,
                    cursor.position(),
                );
                match run_readonly_tool_turn(
                    cwd,
                    events,
                    sink,
                    conversation,
                    history_writer.as_deref_mut(),
                    &tool_requests[cursor.position()..batch_end],
                    emit_deltas,
                    mcp_registry,
                    hooks,
                    config.tools.output_truncation,
                )? {
                    ToolTurnOutcome::Continue => {}
                    ToolTurnOutcome::Return { status, error } => {
                        return Ok(AgentLoopResult::terminal(status, error));
                    }
                }
                cursor.advance_to(batch_end);
                continue;
            }

            match run_normal_tool_turn(
                config,
                cwd,
                events,
                sink,
                conversation,
                history_writer.as_deref_mut(),
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
                workflow_ipc,
                &mut permission_overlay,
                permission_handler,
                execute_child_agent_loop,
                execute_child_agent_loop,
            )? {
                ToolTurnOutcome::Continue => {}
                ToolTurnOutcome::Return { status, error } => {
                    return Ok(AgentLoopResult::terminal(status, error));
                }
            }
            cursor.advance_one();
        }
    }
}

pub(crate) fn execute_child_agent_loop<W: io::Write>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    runtime: &mut ChildAgentRuntime<'_, W>,
    child_cost_tracker: &mut CostTracker,
) -> io::Result<ChildAgentResult> {
    let task_registry = TaskRegistry::new_for_cwd(runtime.events.run_id().to_string(), runtime.cwd);
    let mut background_workflows = Vec::new();
    let child = run_agent_loop(
        config,
        AgentLoopContext::new(
            runtime.cwd,
            &request.prompt,
            request.depth,
            request.emit_deltas,
            &request.subagent_type,
        )
        .with_services(
            runtime.instructions,
            runtime.memory,
            runtime.mcp_registry,
            runtime.hooks,
        )
        .with_runtime(child_cost_tracker, runtime.cancel, &task_registry)
        .with_execution(
            &mut background_workflows,
            request.workflow_ipc.as_ref(),
            runtime.lifecycle.as_deref_mut(),
        ),
        runtime.events,
        runtime.sink,
        AgentConversationContext::new(),
        AgentToolPolicyContext::new(
            request.allowed_tools.as_deref(),
            request.tool_policy_label.as_deref(),
        ),
    )?;
    observe_background_workflows(
        config.output_format == OutputFormat::Jsonl,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::RuntimeTaskKind;
    use std::path::PathBuf;

    #[test]
    fn runtime_turn_config_snapshots_agent_loop_entry_values() {
        let cwd = PathBuf::from("/tmp/orca-runtime-turn-config");
        let subagent_type = SubagentType::General;

        let config = RuntimeTurnConfig::new(&cwd, "inspect repo", 2, false, &subagent_type);

        assert_eq!(config.cwd(), cwd.as_path());
        assert_eq!(config.prompt(), "inspect repo");
        assert_eq!(config.subagent_depth(), 2);
        assert!(!config.emit_deltas());
        assert_eq!(config.subagent_type(), &SubagentType::General);
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_config() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-config");
        let subagent_type = SubagentType::General;

        let context = AgentLoopContext::new(&cwd, "inspect repo", 1, true, &subagent_type);

        let config = context.turn_config();
        assert_eq!(config.cwd(), cwd.as_path());
        assert_eq!(config.prompt(), "inspect repo");
        assert_eq!(config.subagent_depth(), 1);
        assert!(config.emit_deltas());
        assert_eq!(config.subagent_type(), &SubagentType::General);
    }

    #[test]
    fn agent_loop_context_carries_readonly_services() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-services");
        let subagent_type = SubagentType::General;
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_services(&instructions, &memory, &registry, &hooks);

        assert!(std::ptr::eq(context.instructions(), &instructions));
        assert!(std::ptr::eq(context.memory(), &memory));
        assert!(std::ptr::eq(context.mcp_registry(), &registry));
        assert!(std::ptr::eq(context.hooks(), &hooks));
    }

    #[test]
    fn runtime_turn_deps_group_agent_loop_services() {
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let deps = RuntimeTurnDeps::new(&instructions, &memory, &registry, &hooks);

        assert!(std::ptr::eq(deps.instructions(), &instructions));
        assert!(std::ptr::eq(deps.memory(), &memory));
        assert!(std::ptr::eq(deps.mcp_registry(), &registry));
        assert!(std::ptr::eq(deps.hooks(), &hooks));
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_deps() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-deps");
        let subagent_type = SubagentType::General;
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_services(&instructions, &memory, &registry, &hooks);

        let deps = context.turn_deps();
        assert!(std::ptr::eq(deps.instructions(), &instructions));
        assert!(std::ptr::eq(deps.memory(), &memory));
        assert!(std::ptr::eq(deps.mcp_registry(), &registry));
        assert!(std::ptr::eq(deps.hooks(), &hooks));
    }

    #[test]
    fn agent_loop_context_carries_runtime_refs() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-runtime");
        let subagent_type = SubagentType::General;
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-runtime".to_string());

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_runtime(&mut cost_tracker, &cancel, &task_registry);

        assert_eq!(context.cost_tracker().totals().total_tokens(), 0);
        assert!(std::ptr::eq(context.cancel(), &cancel));
        assert!(std::ptr::eq(context.task_registry(), &task_registry));
    }

    #[test]
    fn runtime_turn_state_groups_mutable_runtime_refs() {
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-state".to_string());

        let state = RuntimeTurnState::new(&mut cost_tracker, &cancel, &task_registry);

        assert_eq!(state.cost_tracker().totals().total_tokens(), 0);
        assert!(std::ptr::eq(state.cancel(), &cancel));
        assert!(std::ptr::eq(state.task_registry(), &task_registry));
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_state() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-state");
        let subagent_type = SubagentType::General;
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-state-context".to_string());

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_runtime(&mut cost_tracker, &cancel, &task_registry);

        let state = context.turn_state();
        assert_eq!(state.cost_tracker().totals().total_tokens(), 0);
        assert!(std::ptr::eq(state.cancel(), &cancel));
        assert!(std::ptr::eq(state.task_registry(), &task_registry));
    }

    #[test]
    fn agent_loop_context_carries_execution_refs() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-execution");
        let subagent_type = SubagentType::General;
        let mut background_workflows = Vec::new();
        let mut lifecycle = RuntimeSessionLifecycle::new("agent-loop-execution");
        lifecycle.start_task(RuntimeTaskKind::Agent);

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_execution(&mut background_workflows, None, Some(&mut lifecycle));

        assert_eq!(context.background_workflow_count(), 0);
        assert!(context.workflow_ipc().is_none());
        assert_eq!(
            context.lifecycle().unwrap().run_id(),
            "agent-loop-execution"
        );
    }

    #[test]
    fn runtime_turn_execution_groups_lifecycle_refs() {
        let mut background_workflows = Vec::new();
        let mut lifecycle = RuntimeSessionLifecycle::new("agent-loop-execution-group");
        lifecycle.start_task(RuntimeTaskKind::Agent);

        let execution =
            RuntimeTurnExecution::new(&mut background_workflows, None, Some(&mut lifecycle));

        assert_eq!(execution.background_workflow_count(), 0);
        assert!(execution.workflow_ipc().is_none());
        assert_eq!(
            execution.lifecycle().unwrap().run_id(),
            "agent-loop-execution-group"
        );
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_execution() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-execution-context");
        let subagent_type = SubagentType::General;
        let mut background_workflows = Vec::new();
        let mut lifecycle = RuntimeSessionLifecycle::new("agent-loop-execution-context");
        lifecycle.start_task(RuntimeTaskKind::Agent);

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_execution(&mut background_workflows, None, Some(&mut lifecycle));

        let execution = context.turn_execution();
        assert_eq!(execution.background_workflow_count(), 0);
        assert!(execution.workflow_ipc().is_none());
        assert_eq!(
            execution.lifecycle().unwrap().run_id(),
            "agent-loop-execution-context"
        );
    }
}
