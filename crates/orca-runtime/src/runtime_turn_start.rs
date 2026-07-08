use std::io;

use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;

use crate::lifecycle::{
    AgentLoopResult, RuntimeTaskActor, RuntimeTurnContext, RuntimeTurnStartError,
};

pub(crate) struct RuntimeTurnStartStep;
pub(crate) struct RuntimeTurnStartResultStep;

pub(crate) struct RuntimeTurnStartInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) turn_context: RuntimeTurnContext<'a>,
}

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
        input: RuntimeTurnStartInput<'_, '_, W>,
    ) -> io::Result<RuntimeTurnStartStepOutput> {
        let RuntimeTurnContext {
            cwd: _,
            prompt,
            subagent_depth: _,
            emit_deltas,
            subagent_type: _,
            continuation: _,
            steer_handle: _,
        } = input.turn_context;

        let turn_prompt = if input
            .actor
            .active_task()
            .map(|task| task.current_turn())
            .unwrap_or(0)
            == 0
        {
            Some(prompt)
        } else {
            None
        };
        let started_turn = match input
            .actor
            .start_turn(input.events, turn_prompt, emit_deltas)
        {
            Ok(started_turn) => started_turn,
            Err(error) => {
                if emit_deltas {
                    input.sink.emit(&input.events.error(&error.message))?;
                }
                return Ok(RuntimeTurnStartStepOutput { error: Some(error) });
            }
        };
        if let Some(event) = started_turn.into_event() {
            input.sink.emit(&event)?;
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
