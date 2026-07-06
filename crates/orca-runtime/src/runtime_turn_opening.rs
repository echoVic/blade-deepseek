use std::io;
use std::path::Path;

use orca_core::config::ProviderKind;
use orca_core::conversation::Conversation;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::model::ModelSelection;
use orca_core::subagent_types::SubagentType;
use orca_provider::{ProviderConfig, context};

use crate::compaction::RuntimeCompactionStep;
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::lifecycle::{AgentLoopResult, RuntimeTaskActor, ThreadSteerHandle};
use crate::runtime_model_route::{RuntimeModelRouteInput, RuntimeModelRouteStep};
use crate::runtime_steer::{RuntimeSteerInput, RuntimeSteerStep};
use crate::runtime_turn_start::{
    RuntimeTurnStartResult, RuntimeTurnStartResultStep, RuntimeTurnStartStep,
};
use crate::thread_store::SessionWriter;

pub(crate) struct RuntimeTurnOpeningStep;

pub(crate) struct RuntimeTurnOpeningInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) provider: ProviderKind,
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) emit_deltas: bool,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
    pub(crate) prompt: &'a str,
    pub(crate) model: &'a ModelSelection,
    pub(crate) subagent_type: &'a SubagentType,
    pub(crate) model_override: Option<&'a str>,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) steer_handle: Option<&'a ThreadSteerHandle>,
}

pub(crate) enum RuntimeTurnOpeningResult {
    Continue { provider_config: ProviderConfig },
    Return(AgentLoopResult),
}

impl RuntimeTurnOpeningStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn open<W: io::Write>(
        &mut self,
        mut input: RuntimeTurnOpeningInput<'_, '_, W>,
    ) -> io::Result<RuntimeTurnOpeningResult> {
        RuntimeCompactionStep::new(
            input.provider,
            input.context_config,
            input.provider_config,
            input.cwd,
            input.emit_deltas,
            input.hooks,
            input.events,
            input.sink,
            input.history_writer.as_deref_mut(),
        )
        .compact_if_needed(input.conversation)?;

        match RuntimeTurnStartResultStep::new().fold(RuntimeTurnStartStep::new().start(
            input.actor,
            input.events,
            input.sink,
            input.prompt,
            input.emit_deltas,
        )?) {
            RuntimeTurnStartResult::Return(result) => {
                return Ok(RuntimeTurnOpeningResult::Return(result));
            }
            RuntimeTurnStartResult::Continue => {}
        }

        let turn_provider_config = RuntimeModelRouteStep::new()
            .route(RuntimeModelRouteInput {
                actor: input.actor,
                model: input.model,
                subagent_type: input.subagent_type,
                model_override: input.model_override,
                provider_config: input.provider_config,
                cost_tracker: input.cost_tracker,
                events: input.events,
                sink: input.sink,
                emit_deltas: input.emit_deltas,
            })?
            .provider_config;

        RuntimeSteerStep::new().apply(RuntimeSteerInput {
            steer_handle: input.steer_handle,
            conversation: input.conversation,
            history_writer: input.history_writer.as_deref_mut(),
        })?;

        Ok(RuntimeTurnOpeningResult::Continue {
            provider_config: turn_provider_config,
        })
    }
}
