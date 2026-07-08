use std::io;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;

use crate::agent_child::ChildAgentExecutor;
use crate::cost::CostTracker;
use crate::extension::RuntimeExtensionContext;
use crate::lifecycle::{AgentLoopResult, RuntimeTaskActor, RuntimeTurnDeps};
use crate::provider_turn::{
    RuntimeProviderCycleInput, RuntimeTurnProviderCycleResult, RuntimeTurnProviderCycleStep,
};
use crate::runtime_conversation_bootstrap::RuntimePreparedConversation;
use crate::runtime_turn_loop::{
    RuntimeTurnOutputContext, RuntimeTurnProviderContext, RuntimeTurnRequestContext,
    RuntimeTurnWorkflowContext,
};
use crate::runtime_turn_opening::{
    RuntimeTurnOpeningInput, RuntimeTurnOpeningResult, RuntimeTurnOpeningStep,
};
use crate::step_context::RuntimeStepCapabilitySnapshot;
use crate::tasks::TaskRegistry;
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow::runner::SharedEventBuffer;

pub(crate) struct RuntimeTurnIterationStep {
    opening_step: RuntimeTurnOpeningStep,
    provider_cycle_step: RuntimeTurnProviderCycleStep,
}

pub(crate) struct RuntimeTurnIterationInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) provider_context: RuntimeTurnProviderContext<'a>,
    pub(crate) runtime_system_messages: &'a [String],
    pub(crate) request: RuntimeTurnRequestContext<'a>,
    pub(crate) deps: RuntimeTurnDeps<'a>,
    pub(crate) output: RuntimeTurnOutputContext<'a, 'a, W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) model_override: Option<&'a str>,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) config: &'a RunConfig,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) extensions: RuntimeExtensionContext<'a>,
    pub(crate) workflow: RuntimeTurnWorkflowContext<'a, 'a>,
}

pub(crate) enum RuntimeTurnIterationResult {
    ContinueLoop,
    Return(AgentLoopResult),
}

impl RuntimeTurnIterationStep {
    pub(crate) fn new() -> Self {
        Self {
            opening_step: RuntimeTurnOpeningStep::new(),
            provider_cycle_step: RuntimeTurnProviderCycleStep::new(),
        }
    }

    pub(crate) fn run<W: io::Write>(
        &mut self,
        input: RuntimeTurnIterationInput<'_, '_, W>,
        child_executor: ChildAgentExecutor<W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> io::Result<RuntimeTurnIterationResult> {
        let turn_provider_config = {
            let (conversation, history_writer) = input.prepared_conversation.parts_mut();
            match self.opening_step.open(RuntimeTurnOpeningInput {
                actor: input.actor,
                provider: input.provider_context.provider,
                context_config: input.provider_context.context_config,
                provider_config: input.provider_context.provider_config,
                cwd: input.request.cwd,
                emit_deltas: input.request.emit_deltas,
                hooks: input.deps.hooks,
                events: input.output.events,
                sink: input.output.sink,
                conversation,
                history_writer,
                prompt: input.request.prompt,
                model: input.provider_context.model,
                subagent_type: input.request.subagent_type,
                model_override: input.model_override,
                cost_tracker: input.cost_tracker,
                steer_handle: input.request.steer_handle,
            })? {
                RuntimeTurnOpeningResult::Continue { provider_config } => provider_config,
                RuntimeTurnOpeningResult::Return(result) => {
                    return Ok(RuntimeTurnIterationResult::Return(result));
                }
            }
        };

        match self.provider_cycle_step.run(
            RuntimeProviderCycleInput {
                actor: input.actor,
                provider: input.provider_context.provider,
                continuation: input.request.continuation,
                turn_provider_config: &turn_provider_config,
                runtime_system_messages: input.runtime_system_messages,
                cwd: input.request.cwd,
                context_config: input.provider_context.context_config,
                base_provider_config: input.provider_context.provider_config,
                emit_deltas: input.request.emit_deltas,
                capabilities: RuntimeStepCapabilitySnapshot::new(
                    input.deps.instructions,
                    input.deps.memory,
                    input.deps.mcp_registry,
                    input.deps.hooks,
                    input.cancel,
                    input.task_registry,
                    input.workflow.workflow_ipc,
                    input.deps.turn_interactions.permission_handler(),
                    input.deps.turn_interactions.user_input_handler(),
                ),
                cost_tracker: input.cost_tracker,
                max_budget_usd: input.provider_context.max_budget_usd,
                events: input.output.events,
                sink: input.output.sink,
                conversation: input.prepared_conversation,
                config: input.config,
                tool_policy: input.tool_policy,
                subagent_depth: input.request.subagent_depth,
                policy: input.policy,
                extensions: input.extensions,
                background_workflows: input.workflow.background_workflows,
                steer_handle: input.request.steer_handle,
            },
            child_executor,
            workflow_child_executor,
            batch_child_executor,
        )? {
            RuntimeTurnProviderCycleResult::ContinueLoop
            | RuntimeTurnProviderCycleResult::ContinueTurn => {
                Ok(RuntimeTurnIterationResult::ContinueLoop)
            }
            RuntimeTurnProviderCycleResult::Return(result) => {
                Ok(RuntimeTurnIterationResult::Return(result))
            }
        }
    }
}
