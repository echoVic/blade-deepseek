use std::io;

use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
use crate::cost::CostTracker;
use crate::lifecycle::{
    AgentLoopContext, AgentLoopResult, RuntimeSessionLifecycle, RuntimeTaskActor,
    RuntimeTurnConfig, RuntimeTurnDeps, RuntimeTurnExecution, RuntimeTurnState,
};
use crate::runtime_conversation_bootstrap::RuntimeConversationBootstrapStep;
use crate::runtime_directive::RuntimeDirectiveState;
use crate::runtime_turn_loop::{
    RuntimeTurnLoopExecutors, RuntimeTurnLoopInput, RuntimeTurnLoopStep,
};
use crate::runtime_turn_setup::RuntimeTurnSetupStep;
use crate::session::AgentConversationContext;
use crate::tasks::TaskRegistry;
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow_execution::observe_background_workflows;
use orca_core::config::{OutputFormat, RunConfig};
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;

const DEFAULT_MAX_TURNS: u32 = 128;

pub(crate) fn run_agent_loop(
    config: &RunConfig,
    loop_context: AgentLoopContext<'_>,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    conversation_context: AgentConversationContext<'_>,
    tool_policy: AgentToolPolicyContext<'_>,
) -> io::Result<AgentLoopResult> {
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
    let turn_state = turn_state.expect("agent loop turn state");
    let RuntimeTurnState {
        cost_tracker,
        cancel,
        task_registry,
        ref directive_state,
    } = turn_state;
    let RuntimeTurnExecution {
        background_workflows,
        workflow_ipc,
        lifecycle,
    } = turn_execution.expect("agent loop turn execution");
    let tool_policy = tool_policy_for_directive_state(tool_policy, directive_state);
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

    let mut prepared_conversation = RuntimeConversationBootstrapStep::new().prepare(
        conversation_context,
        cwd,
        prompt,
        subagent_depth,
        subagent_type,
        instructions,
        config.approval_mode,
        memory,
        emit_deltas,
    )?;

    let mut legacy_lifecycle = RuntimeSessionLifecycle::new(events.run_id().to_string());
    let lifecycle = lifecycle.unwrap_or(&mut legacy_lifecycle);
    let mut actor = RuntimeTaskActor::new(lifecycle, max_turns);
    let mut turn_loop_step = RuntimeTurnLoopStep::new();

    turn_loop_step.run(
        RuntimeTurnLoopInput::new(
            &mut actor,
            config.provider,
            &ctx_config,
            &provider_config,
            directive_state.pending_system_messages(),
            cwd,
            emit_deltas,
            hooks,
            events,
            sink,
            &mut prepared_conversation,
            prompt,
            &config.model,
            subagent_type,
            directive_state.model_override(),
            cost_tracker,
            steer_handle,
            cancel,
            config.max_budget_usd,
            config,
            tool_policy,
            subagent_depth,
            &policy,
            instructions,
            memory,
            mcp_registry,
            task_registry,
            background_workflows,
            workflow_ipc,
            permission_handler,
        ),
        RuntimeTurnLoopExecutors::new(
            execute_child_agent_loop,
            execute_child_agent_loop,
            execute_child_agent_loop,
        ),
    )
}

fn tool_policy_for_directive_state<'a>(
    tool_policy: AgentToolPolicyContext<'a>,
    directive_state: &'a RuntimeDirectiveState,
) -> AgentToolPolicyContext<'a> {
    tool_policy.replace_allowed_tools(
        directive_state.allowed_tools(),
        "runtime directive tool policy",
    )
}

#[cfg(test)]
fn tool_policy_for_runtime_directives<'a>(
    tool_policy: AgentToolPolicyContext<'a>,
    turn_state: &'a RuntimeTurnState<'_>,
) -> AgentToolPolicyContext<'a> {
    tool_policy_for_directive_state(tool_policy, &turn_state.directive_state)
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
    use crate::hooks::HookRunner;
    use crate::instructions::ProjectInstructions;
    use crate::lifecycle::RuntimeTaskKind;
    use crate::memory::MemoryBlock;
    use orca_core::cancel::CancelToken;
    use orca_core::subagent_types::SubagentType;
    use orca_mcp::McpRegistry;
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
    fn runtime_turn_state_applies_runtime_directives_in_order() {
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-directives".to_string());
        let mut state = RuntimeTurnState::new(&mut cost_tracker, &cancel, &task_registry);

        state.apply_directive(crate::runtime_directive::RuntimeDirective::SwitchModel {
            model: orca_core::model::FLASH_MODEL.to_string(),
            reason: "skill requested cheaper execution".to_string(),
        });
        state.apply_directive(
            crate::runtime_directive::RuntimeDirective::ReplaceAllowedTools {
                tool_names: vec!["read_file".to_string(), "grep".to_string()],
                reason: "skill narrowed tool surface".to_string(),
            },
        );
        state.apply_directive(
            crate::runtime_directive::RuntimeDirective::InjectSystemMessage {
                message: "Prefer focused repository evidence.".to_string(),
                reason: "skill added runtime instruction".to_string(),
            },
        );

        let directives = &state.directive_state;
        assert_eq!(
            directives.model_override(),
            Some(orca_core::model::FLASH_MODEL)
        );
        assert_eq!(
            directives.allowed_tools(),
            Some(&["read_file".to_string(), "grep".to_string()][..])
        );
        assert_eq!(
            directives.pending_system_messages(),
            &["Prefer focused repository evidence.".to_string()]
        );
        assert_eq!(
            directives.transition_reasons(),
            &[
                "switch_model: skill requested cheaper execution".to_string(),
                "replace_allowed_tools: skill narrowed tool surface".to_string(),
                "inject_system_message: skill added runtime instruction".to_string(),
            ]
        );
    }

    #[test]
    fn runtime_directives_replace_agent_loop_tool_policy() {
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-tool-directives".to_string());
        let mut state = RuntimeTurnState::new(&mut cost_tracker, &cancel, &task_registry);
        state.apply_directive(
            crate::runtime_directive::RuntimeDirective::ReplaceAllowedTools {
                tool_names: vec!["read_file".to_string()],
                reason: "narrow current turn".to_string(),
            },
        );

        let policy =
            tool_policy_for_runtime_directives(AgentToolPolicyContext::unrestricted(), &state);

        assert_eq!(
            policy.allowed_tools().unwrap(),
            &["read_file".to_string()][..]
        );
        assert_eq!(policy.label(), Some("runtime directive tool policy"));
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
