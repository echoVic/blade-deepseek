use std::io;

use orca_core::conversation::Conversation;

use crate::lifecycle::RuntimeTurnContext;
use crate::thread_store::SessionWriter;

pub(crate) struct RuntimeSteerStep;

pub(crate) struct RuntimeSteerInput<'a> {
    pub(crate) turn_context: RuntimeTurnContext<'a>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
}

impl RuntimeSteerStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn apply(&mut self, mut input: RuntimeSteerInput<'_>) -> io::Result<usize> {
        let Some(steer_handle) = input.turn_context.steer_handle else {
            return Ok(0);
        };

        let mut injected = 0;
        for steer_input in steer_handle.drain() {
            input.conversation.add_user(steer_input);
            injected += 1;
            if let Some(writer) = input.history_writer.as_deref_mut()
                && let Some(message) = input.conversation.messages.last()
            {
                writer.append_message(message)?;
            }
        }
        Ok(injected)
    }
}
