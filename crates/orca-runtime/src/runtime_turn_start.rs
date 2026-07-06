use std::io;

use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;

use crate::lifecycle::{AgentLoopResult, RuntimeTaskActor, RuntimeTurnStartError};

pub(crate) struct RuntimeTurnStartStep;
pub(crate) struct RuntimeTurnStartResultStep;

pub(crate) struct RuntimeTurnStartStepOutput {
    pub(crate) error: Option<RuntimeTurnStartError>,
}

pub(crate) enum RuntimeTurnStartResult {
    Continue,
    Return(AgentLoopResult),
}

impl RuntimeTurnStartStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn start<W: io::Write>(
        &mut self,
        actor: &mut RuntimeTaskActor<'_>,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        prompt: &str,
        emit_deltas: bool,
    ) -> io::Result<RuntimeTurnStartStepOutput> {
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
                return Ok(RuntimeTurnStartStepOutput { error: Some(error) });
            }
        };
        if let Some(event) = started_turn.into_event() {
            sink.emit(&event)?;
        }
        Ok(RuntimeTurnStartStepOutput { error: None })
    }
}

impl RuntimeTurnStartResultStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn fold(&self, output: RuntimeTurnStartStepOutput) -> RuntimeTurnStartResult {
        match output.error {
            Some(error) => RuntimeTurnStartResult::Return(AgentLoopResult::failure(
                error.status,
                error.message,
            )),
            None => RuntimeTurnStartResult::Continue,
        }
    }
}
