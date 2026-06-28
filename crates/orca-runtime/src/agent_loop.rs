use std::io;

use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    AgentLoopContext, RuntimeCompactionStep, RuntimeModelRouteStep, RuntimeProviderErrorStep,
    RuntimeProviderErrorStepOutcome, RuntimeProviderResponseOutcome, RuntimeProviderResponseStep,
    RuntimeProviderTurnResultOutcome, RuntimeProviderTurnResultStep, RuntimeProviderTurnStep,
    RuntimeSessionLifecycle, RuntimeSteerStep, RuntimeTaskActor, RuntimeTurnConfig,
    RuntimeTurnDeps, RuntimeTurnExecution, RuntimeTurnSetupStep, RuntimeTurnStartStep,
    RuntimeTurnState,
};
use crate::memory::MemoryBlock;
use crate::session::{
    AgentConversationContext, bootstrap_agent_conversation_for_loop,
    record_initial_history_for_agent,
};
use crate::tasks::TaskRegistry;
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow_execution::observe_background_workflows;
use orca_core::cancel::CancelToken;
use orca_core::config::{OutputFormat, RunConfig};
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;

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
    let setup = RuntimeTurnSetupStep::new().prepare(
        config,
        subagent_depth,
        subagent_type,
        tool_policy,
        mcp_registry,
    );
    let ctx_config = setup.context_config;
    let policy = setup.policy;
    let provider_config = setup.provider_config;

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
    let mut provider_error_step = RuntimeProviderErrorStep::new();

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

        if let Some(error) = RuntimeTurnStartStep::new()
            .start(&mut actor, events, sink, prompt, emit_deltas)?
            .error
        {
            return Ok(AgentLoopResult::failure(error.status, error.message));
        }

        let turn_provider_config = RuntimeModelRouteStep::new()
            .route(
                &mut actor,
                &config.model,
                subagent_type,
                &provider_config,
                cost_tracker,
                events,
                sink,
                emit_deltas,
            )?
            .provider_config;

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
        let response = match RuntimeProviderTurnResultStep::new().fold(
            provider_turn,
            events,
            sink,
            emit_deltas,
        )? {
            RuntimeProviderTurnResultOutcome::Response(response) => response,
            RuntimeProviderTurnResultOutcome::Failed(error) => {
                return Ok(AgentLoopResult::failure(error.status, error.message));
            }
        };

        match provider_error_step.handle(
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
        )? {
            RuntimeProviderErrorStepOutcome::ContinueAfterCompaction => {
                continue;
            }
            RuntimeProviderErrorStepOutcome::Failed(error) => {
                return Ok(AgentLoopResult::failure(error.status, error.message));
            }
            RuntimeProviderErrorStepOutcome::NoError => {}
        }

        match RuntimeProviderResponseStep::new().handle(
            response,
            config,
            cwd,
            events,
            sink,
            conversation,
            history_writer.as_deref_mut(),
            tool_policy,
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
            permission_handler,
            execute_child_agent_loop,
            execute_child_agent_loop,
            execute_child_agent_loop,
        )? {
            RuntimeProviderResponseOutcome::Continue => {}
            RuntimeProviderResponseOutcome::Success { final_message } => {
                return Ok(AgentLoopResult::success(final_message));
            }
            RuntimeProviderResponseOutcome::Return { status, error } => {
                return Ok(AgentLoopResult::terminal(status, error));
            }
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
