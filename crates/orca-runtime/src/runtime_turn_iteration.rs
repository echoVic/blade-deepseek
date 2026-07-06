use std::io;
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::{ProviderKind, RunConfig};
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::model::ModelSelection;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;
use orca_provider::{ProviderConfig, context};

use crate::agent_child::ChildAgentExecutor;
use crate::cost::CostTracker;
use crate::extension::{ExtensionData, ExtensionRegistry};
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    AgentLoopResult, RuntimePermissionRequestHandler, RuntimeTaskActor, ThreadSteerHandle,
};
use crate::memory::MemoryBlock;
use crate::provider_turn::{
    RuntimeProviderCycleInput, RuntimeTurnProviderCycleResult, RuntimeTurnProviderCycleStep,
};
use crate::runtime_conversation_bootstrap::RuntimePreparedConversation;
use crate::runtime_turn_opening::{
    RuntimeTurnOpeningInput, RuntimeTurnOpeningResult, RuntimeTurnOpeningStep,
};
use crate::tasks::TaskRegistry;
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;

pub(crate) struct RuntimeTurnIterationStep {
    opening_step: RuntimeTurnOpeningStep,
    provider_cycle_step: RuntimeTurnProviderCycleStep,
}

pub(crate) struct RuntimeTurnIterationInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) provider: ProviderKind,
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) runtime_system_messages: &'a [String],
    pub(crate) cwd: &'a Path,
    pub(crate) emit_deltas: bool,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) prompt: &'a str,
    pub(crate) model: &'a ModelSelection,
    pub(crate) subagent_type: &'a SubagentType,
    pub(crate) model_override: Option<&'a str>,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) steer_handle: Option<&'a ThreadSteerHandle>,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) max_budget_usd: Option<f64>,
    pub(crate) config: &'a RunConfig,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) subagent_depth: u32,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) extension_registry: &'a ExtensionRegistry,
    pub(crate) thread_extensions: &'a ExtensionData,
    pub(crate) turn_extensions: &'a ExtensionData,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
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
                provider: input.provider,
                context_config: input.context_config,
                provider_config: input.provider_config,
                cwd: input.cwd,
                emit_deltas: input.emit_deltas,
                hooks: input.hooks,
                events: input.events,
                sink: input.sink,
                conversation,
                history_writer,
                prompt: input.prompt,
                model: input.model,
                subagent_type: input.subagent_type,
                model_override: input.model_override,
                cost_tracker: input.cost_tracker,
                steer_handle: input.steer_handle,
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
                provider: input.provider,
                turn_provider_config: &turn_provider_config,
                runtime_system_messages: input.runtime_system_messages,
                cwd: input.cwd,
                context_config: input.context_config,
                base_provider_config: input.provider_config,
                emit_deltas: input.emit_deltas,
                hooks: input.hooks,
                cancel: input.cancel,
                cost_tracker: input.cost_tracker,
                max_budget_usd: input.max_budget_usd,
                events: input.events,
                sink: input.sink,
                conversation: input.prepared_conversation,
                config: input.config,
                tool_policy: input.tool_policy,
                subagent_depth: input.subagent_depth,
                policy: input.policy,
                instructions: input.instructions,
                memory: input.memory,
                mcp_registry: input.mcp_registry,
                task_registry: input.task_registry,
                extension_registry: input.extension_registry,
                thread_extensions: input.thread_extensions,
                turn_extensions: input.turn_extensions,
                background_workflows: input.background_workflows,
                workflow_ipc: input.workflow_ipc,
                permission_handler: input.permission_handler,
                steer_handle: input.steer_handle,
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
