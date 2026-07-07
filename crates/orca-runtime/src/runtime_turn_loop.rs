use std::io;
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::config::{ProviderKind, RunConfig};
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::model::ModelSelection;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;
use orca_provider::{ProviderConfig, context};

use crate::agent_child::ChildAgentExecutor;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    AgentLoopResult, RuntimeTaskActor, RuntimeTurnInteractionState, RuntimeTurnLoopState,
    ThreadSteerHandle,
};
use crate::memory::MemoryBlock;
use crate::runtime_conversation_bootstrap::RuntimePreparedConversation;
use crate::runtime_turn_iteration::{
    RuntimeTurnIterationInput, RuntimeTurnIterationResult, RuntimeTurnIterationStep,
};
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;

pub(crate) struct RuntimeTurnLoopStep {
    iteration_step: RuntimeTurnIterationStep,
}

pub(crate) struct RuntimeAgentTurnLoopInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) emit_deltas: bool,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) prompt: &'a str,
    pub(crate) subagent_type: &'a SubagentType,
    pub(crate) loop_state: RuntimeTurnLoopState<'a>,
    pub(crate) steer_handle: Option<&'a ThreadSteerHandle>,
    pub(crate) config: &'a RunConfig,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) subagent_depth: u32,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) turn_interactions: RuntimeTurnInteractionState<'a>,
}

pub(crate) struct RuntimeTurnLoopInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) provider: ProviderKind,
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) emit_deltas: bool,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) prompt: &'a str,
    pub(crate) model: &'a ModelSelection,
    pub(crate) subagent_type: &'a SubagentType,
    pub(crate) loop_state: RuntimeTurnLoopState<'a>,
    pub(crate) steer_handle: Option<&'a ThreadSteerHandle>,
    pub(crate) max_budget_usd: Option<f64>,
    pub(crate) config: &'a RunConfig,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) subagent_depth: u32,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) turn_interactions: RuntimeTurnInteractionState<'a>,
}

pub(crate) struct RuntimeTurnLoopExecutors<W: io::Write> {
    pub(crate) child_executor: ChildAgentExecutor<W>,
    pub(crate) workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    pub(crate) batch_child_executor: ChildAgentExecutor<io::Sink>,
}

impl<'a, 'runtime, W: io::Write> RuntimeTurnLoopInput<'a, 'runtime, W> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        actor: &'a mut RuntimeTaskActor<'runtime>,
        provider: ProviderKind,
        context_config: &'a context::ContextConfig,
        provider_config: &'a ProviderConfig,
        cwd: &'a Path,
        emit_deltas: bool,
        hooks: &'a HookRunner,
        events: &'a mut EventFactory,
        sink: &'a mut EventSink<W>,
        prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
        prompt: &'a str,
        model: &'a ModelSelection,
        subagent_type: &'a SubagentType,
        loop_state: RuntimeTurnLoopState<'a>,
        steer_handle: Option<&'a ThreadSteerHandle>,
        max_budget_usd: Option<f64>,
        config: &'a RunConfig,
        tool_policy: AgentToolPolicyContext<'a>,
        subagent_depth: u32,
        policy: &'a ApprovalPolicy,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
        turn_interactions: RuntimeTurnInteractionState<'a>,
    ) -> Self {
        Self {
            actor,
            provider,
            context_config,
            provider_config,
            cwd,
            emit_deltas,
            hooks,
            events,
            sink,
            prepared_conversation,
            prompt,
            model,
            subagent_type,
            loop_state,
            steer_handle,
            max_budget_usd,
            config,
            tool_policy,
            subagent_depth,
            policy,
            instructions,
            memory,
            mcp_registry,
            background_workflows,
            workflow_ipc,
            turn_interactions,
        }
    }

    pub(crate) fn iteration_input<'iter>(
        &'iter mut self,
    ) -> RuntimeTurnIterationInput<'iter, 'runtime, W> {
        let loop_state = self.loop_state.iteration_state(self.tool_policy);
        RuntimeTurnIterationInput {
            actor: &mut *self.actor,
            provider: self.provider,
            context_config: self.context_config,
            provider_config: self.provider_config,
            runtime_system_messages: loop_state.runtime_system_messages,
            cwd: self.cwd,
            emit_deltas: self.emit_deltas,
            hooks: self.hooks,
            events: &mut *self.events,
            sink: &mut *self.sink,
            prepared_conversation: &mut *self.prepared_conversation,
            prompt: self.prompt,
            model: self.model,
            subagent_type: self.subagent_type,
            model_override: loop_state.model_override,
            cost_tracker: loop_state.cost_tracker,
            steer_handle: self.steer_handle,
            cancel: loop_state.cancel,
            max_budget_usd: self.max_budget_usd,
            config: self.config,
            tool_policy: loop_state.tool_policy,
            subagent_depth: self.subagent_depth,
            policy: self.policy,
            instructions: self.instructions,
            memory: self.memory,
            mcp_registry: self.mcp_registry,
            task_registry: loop_state.task_registry,
            extensions: loop_state.extensions,
            background_workflows: &mut *self.background_workflows,
            workflow_ipc: self.workflow_ipc,
            turn_interactions: self.turn_interactions,
        }
    }
}

impl<W: io::Write> RuntimeTurnLoopExecutors<W> {
    pub(crate) fn new(
        child_executor: ChildAgentExecutor<W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> Self {
        Self {
            child_executor,
            workflow_child_executor,
            batch_child_executor,
        }
    }
}

pub(crate) fn run_agent_turn_loop<W: io::Write>(
    step: &mut RuntimeTurnLoopStep,
    input: RuntimeAgentTurnLoopInput<'_, '_, W>,
    executors: RuntimeTurnLoopExecutors<W>,
) -> io::Result<AgentLoopResult> {
    step.run(input.into_turn_loop_input(), executors)
}

impl RuntimeTurnLoopStep {
    pub(crate) fn new() -> Self {
        Self {
            iteration_step: RuntimeTurnIterationStep::new(),
        }
    }

    pub(crate) fn run<W: io::Write>(
        &mut self,
        mut input: RuntimeTurnLoopInput<'_, '_, W>,
        executors: RuntimeTurnLoopExecutors<W>,
    ) -> io::Result<AgentLoopResult> {
        loop {
            match self.iteration_step.run(
                input.iteration_input(),
                executors.child_executor,
                executors.workflow_child_executor,
                executors.batch_child_executor,
            )? {
                RuntimeTurnIterationResult::ContinueLoop => {
                    continue;
                }
                RuntimeTurnIterationResult::Return(result) => return Ok(result),
            }
        }
    }
}

impl<'a, 'runtime, W: io::Write> RuntimeAgentTurnLoopInput<'a, 'runtime, W> {
    fn into_turn_loop_input(self) -> RuntimeTurnLoopInput<'a, 'runtime, W> {
        RuntimeTurnLoopInput::new(
            self.actor,
            self.config.provider,
            self.context_config,
            self.provider_config,
            self.cwd,
            self.emit_deltas,
            self.hooks,
            self.events,
            self.sink,
            self.prepared_conversation,
            self.prompt,
            &self.config.model,
            self.subagent_type,
            self.loop_state,
            self.steer_handle,
            self.config.max_budget_usd,
            self.config,
            self.tool_policy,
            self.subagent_depth,
            self.policy,
            self.instructions,
            self.memory,
            self.mcp_registry,
            self.background_workflows,
            self.workflow_ipc,
            self.turn_interactions,
        )
    }
}
