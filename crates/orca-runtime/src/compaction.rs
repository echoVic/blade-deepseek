use std::io;
use std::path::Path;

use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::hook_types::HookEvent;
use orca_provider::{ProviderConfig, context};

use crate::hooks::{HookContext, HookRunner, conversation_with_hook_context};
use crate::thread_store::SessionWriter;
use orca_core::conversation::Conversation;

pub(crate) struct RuntimeCompactionStep<'a, W: io::Write> {
    provider: orca_core::config::ProviderKind,
    context_config: &'a context::ContextConfig,
    provider_config: &'a ProviderConfig,
    cwd: &'a Path,
    emit_deltas: bool,
    hooks: &'a HookRunner,
    events: &'a mut EventFactory,
    sink: &'a mut EventSink<W>,
    history_writer: Option<&'a mut SessionWriter>,
}

impl<'a, W: io::Write> RuntimeCompactionStep<'a, W> {
    pub(crate) fn new(
        provider: orca_core::config::ProviderKind,
        context_config: &'a context::ContextConfig,
        provider_config: &'a ProviderConfig,
        cwd: &'a Path,
        emit_deltas: bool,
        hooks: &'a HookRunner,
        events: &'a mut EventFactory,
        sink: &'a mut EventSink<W>,
        history_writer: Option<&'a mut SessionWriter>,
    ) -> Self {
        Self {
            provider,
            context_config,
            provider_config,
            cwd,
            emit_deltas,
            hooks,
            events,
            sink,
            history_writer,
        }
    }

    pub(crate) fn compact_if_needed(
        &mut self,
        conversation: &mut Conversation,
    ) -> io::Result<bool> {
        if !context::needs_compaction_wire(conversation, self.context_config, self.provider_config)
        {
            return Ok(false);
        }

        self.compact_with_budget_hooks(conversation)?;
        Ok(true)
    }

    pub(crate) fn compact_after_prompt_too_long(
        &mut self,
        conversation: &mut Conversation,
    ) -> io::Result<()> {
        self.compact_and_persist(conversation)?;
        Ok(())
    }

    pub(crate) fn emit_error(&mut self, error: &str) -> io::Result<()> {
        if self.emit_deltas {
            self.sink.emit(&self.events.error(error))?;
        }
        Ok(())
    }

    fn compact_with_budget_hooks(&mut self, conversation: &mut Conversation) -> io::Result<()> {
        let before_messages = conversation.messages.len();
        match self.hooks.run(
            HookEvent::OnBudgetWarning,
            HookContext {
                cwd: &self.cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: Some(before_messages),
                after_messages: None,
                usage: None,
            },
        ) {
            Ok(outcome) if !outcome.injected_context.is_empty() => {
                *conversation = conversation_with_hook_context(conversation, &outcome);
            }
            Err(error) if self.emit_deltas => {
                self.sink.emit(
                    &self
                        .events
                        .error(&format!("on_budget_warning hook failed: {error}")),
                )?;
            }
            _ => {}
        }

        if self.emit_deltas {
            self.run_compaction_hook(HookEvent::PreCompact, before_messages, None)?;
        }

        let after_messages = self.compact_and_persist(conversation)?;

        if self.emit_deltas {
            self.run_compaction_hook(
                HookEvent::PostCompact,
                before_messages,
                Some(after_messages),
            )?;
        }

        Ok(())
    }

    fn compact_and_persist(&mut self, conversation: &mut Conversation) -> io::Result<usize> {
        let before_messages = conversation.messages.len();
        let compaction = context::compact_with_summary(
            self.provider,
            conversation,
            self.context_config,
            self.provider_config,
        );
        *conversation = compaction.conversation;
        let after_messages = conversation.messages.len();
        if self.emit_deltas
            && let Some(writer) = self.history_writer.as_deref_mut()
        {
            writer.append_compaction(before_messages, after_messages)?;
            if let context::CompactionKind::RemoteSummary(summary) = compaction.kind {
                writer.append_summary_state(
                    before_messages,
                    after_messages,
                    summary,
                    &conversation.summary,
                )?;
            }
        }
        Ok(after_messages)
    }

    fn run_compaction_hook(
        &mut self,
        event: HookEvent,
        before_messages: usize,
        after_messages: Option<usize>,
    ) -> io::Result<()> {
        if let Err(error) = self.hooks.run(
            event,
            HookContext {
                cwd: &self.cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: Some(before_messages),
                after_messages,
                usage: None,
            },
        ) {
            self.sink.emit(
                &self
                    .events
                    .error(&format!("{} hook failed: {error}", event.as_str())),
            )?;
        }
        Ok(())
    }
}
