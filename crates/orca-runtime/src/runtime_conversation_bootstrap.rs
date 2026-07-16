use std::io;
use std::path::Path;

use orca_core::approval_types::ApprovalMode;
use orca_core::conversation::Conversation;
use orca_core::subagent_types::SubagentType;
use orca_core::thread_identity::TurnId;

use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;
use crate::session::{
    AgentConversationContext, bootstrap_agent_conversation_for_loop,
    record_initial_history_for_agent,
};
use crate::thread_store::SessionWriter;

pub(crate) struct RuntimeConversationBootstrapStep;

pub(crate) struct RuntimePreparedConversation<'a> {
    conversation: RuntimePreparedConversationStorage<'a>,
    history_writer: Option<&'a mut SessionWriter>,
}

enum RuntimePreparedConversationStorage<'a> {
    Borrowed(&'a mut Conversation),
    Owned(Conversation),
}

impl RuntimeConversationBootstrapStep {
    pub(crate) fn new() -> Self {
        Self
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare<'a>(
        &mut self,
        conversation_context: AgentConversationContext<'a>,
        cwd: &Path,
        prompt: &str,
        subagent_depth: u32,
        subagent_type: &SubagentType,
        instructions: &ProjectInstructions,
        approval_mode: ApprovalMode,
        memory: &MemoryBlock,
        turn_id: &TurnId,
        emit_deltas: bool,
    ) -> io::Result<RuntimePreparedConversation<'a>> {
        let AgentConversationContext {
            resumed,
            history_writer,
            conversation,
        } = conversation_context;

        let mut prepared = RuntimePreparedConversation {
            conversation: match conversation {
                Some(conversation) => RuntimePreparedConversationStorage::Borrowed(conversation),
                None => RuntimePreparedConversationStorage::Owned(
                    bootstrap_agent_conversation_for_loop(
                        resumed,
                        cwd,
                        prompt,
                        subagent_depth,
                        subagent_type,
                        instructions,
                        approval_mode,
                        memory,
                    ),
                ),
            },
            history_writer,
        };

        if let Some(writer) = prepared.history_writer.as_deref_mut() {
            writer.enter_turn(turn_id.clone());
        }

        let (conversation, history_writer) = prepared.parts_mut();
        record_initial_history_for_agent(
            conversation,
            history_writer,
            resumed.is_some(),
            emit_deltas,
        )?;

        Ok(prepared)
    }
}

impl RuntimePreparedConversation<'_> {
    #[cfg(test)]
    pub(crate) fn conversation_mut(&mut self) -> &mut Conversation {
        match &mut self.conversation {
            RuntimePreparedConversationStorage::Borrowed(conversation) => conversation,
            RuntimePreparedConversationStorage::Owned(conversation) => conversation,
        }
    }

    #[cfg(test)]
    pub(crate) fn history_writer_mut(&mut self) -> Option<&mut SessionWriter> {
        self.history_writer.as_deref_mut()
    }

    pub(crate) fn parts_mut(&mut self) -> (&mut Conversation, Option<&mut SessionWriter>) {
        let conversation = match &mut self.conversation {
            RuntimePreparedConversationStorage::Borrowed(conversation) => conversation,
            RuntimePreparedConversationStorage::Owned(conversation) => conversation,
        };
        (conversation, self.history_writer.as_deref_mut())
    }
}
