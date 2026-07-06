use std::io;

use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::model::ModelSelection;
use orca_core::subagent_types::SubagentType;
use orca_provider::ProviderConfig;

use crate::cost::CostTracker;
use crate::lifecycle::{RuntimeModelTurn, RuntimeTaskActor};

pub(crate) struct RuntimeModelRouteStep;

pub(crate) struct RuntimeModelRouteInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) model: &'a ModelSelection,
    pub(crate) subagent_type: &'a SubagentType,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) emit_deltas: bool,
}

impl RuntimeModelRouteStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn route<W: io::Write>(
        &mut self,
        input: RuntimeModelRouteInput<'_, '_, W>,
    ) -> io::Result<RuntimeModelTurn> {
        let routed_model = input.actor.route_model_turn(
            input.model,
            input.subagent_type,
            None,
            input.provider_config,
            input.cost_tracker,
        );
        if input.emit_deltas {
            input
                .sink
                .emit(&input.events.model_routed(&routed_model.decision))?;
        }
        Ok(routed_model)
    }
}
