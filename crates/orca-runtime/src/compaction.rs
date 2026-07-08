use std::io;

use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::hook_types::HookEvent;
use orca_provider::{ProviderConfig, context};

use crate::hooks::{HookContext, HookRunner, conversation_with_hook_context};
use crate::lifecycle::RuntimeTurnContext;
use crate::thread_store::SessionWriter;
use orca_core::conversation::Conversation;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeCompactionTrigger {
    SoftLimit,
    HardLimit,
    PromptTooLong,
}

pub(crate) struct RuntimeCompactionPolicy<'a> {
    context_config: &'a context::ContextConfig,
    provider_config: &'a ProviderConfig,
}

impl<'a> RuntimeCompactionPolicy<'a> {
    pub(crate) fn new(
        context_config: &'a context::ContextConfig,
        provider_config: &'a ProviderConfig,
    ) -> Self {
        Self {
            context_config,
            provider_config,
        }
    }

    pub(crate) fn decide(&self, conversation: &Conversation) -> Option<RuntimeCompactionTrigger> {
        let pressure =
            context::context_pressure(conversation, self.context_config, self.provider_config);
        Self::decide_for_pressure(pressure)
    }

    pub(crate) fn decide_for_pressure(
        pressure: context::ContextPressure,
    ) -> Option<RuntimeCompactionTrigger> {
        if pressure.should_hard_compact {
            Some(RuntimeCompactionTrigger::HardLimit)
        } else if pressure.should_soft_compact {
            Some(RuntimeCompactionTrigger::SoftLimit)
        } else {
            None
        }
    }
}

pub(crate) struct RuntimeCompactionTask {
    trigger: RuntimeCompactionTrigger,
    before_messages: usize,
    after_messages: Option<usize>,
}

impl RuntimeCompactionTask {
    pub(crate) fn start(trigger: RuntimeCompactionTrigger, before_messages: usize) -> Self {
        Self {
            trigger,
            before_messages,
            after_messages: None,
        }
    }

    pub(crate) fn finish(&mut self, after_messages: usize) {
        self.after_messages = Some(after_messages);
    }

    pub(crate) fn trigger(&self) -> RuntimeCompactionTrigger {
        self.trigger
    }

    pub(crate) fn before_messages(&self) -> usize {
        self.before_messages
    }

    pub(crate) fn should_persist_summary_state(&self, emit_deltas: bool) -> bool {
        match self.trigger() {
            RuntimeCompactionTrigger::SoftLimit
            | RuntimeCompactionTrigger::HardLimit
            | RuntimeCompactionTrigger::PromptTooLong => emit_deltas,
        }
    }

    pub(crate) fn after_messages(&self) -> usize {
        self.after_messages
            .expect("runtime compaction task must finish before persistence")
    }
}

pub(crate) struct RuntimeCompactionStep<'a, W: io::Write> {
    provider: orca_core::config::ProviderKind,
    context_config: &'a context::ContextConfig,
    provider_config: &'a ProviderConfig,
    turn_context: RuntimeTurnContext<'a>,
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
        turn_context: RuntimeTurnContext<'a>,
        hooks: &'a HookRunner,
        events: &'a mut EventFactory,
        sink: &'a mut EventSink<W>,
        history_writer: Option<&'a mut SessionWriter>,
    ) -> Self {
        Self {
            provider,
            context_config,
            provider_config,
            turn_context,
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
        let policy = RuntimeCompactionPolicy::new(self.context_config, self.provider_config);
        let Some(trigger) = policy.decide(conversation) else {
            return Ok(false);
        };
        self.compact_with_budget_hooks(conversation, trigger)?;
        Ok(true)
    }

    pub(crate) fn compact_after_prompt_too_long(
        &mut self,
        conversation: &mut Conversation,
    ) -> io::Result<()> {
        self.compact_and_persist(conversation, RuntimeCompactionTrigger::PromptTooLong)?;
        Ok(())
    }

    pub(crate) fn emit_error(&mut self, error: &str) -> io::Result<()> {
        if self.turn_context.emit_deltas {
            self.sink.emit(&self.events.error(error))?;
        }
        Ok(())
    }

    fn compact_with_budget_hooks(
        &mut self,
        conversation: &mut Conversation,
        trigger: RuntimeCompactionTrigger,
    ) -> io::Result<()> {
        let before_messages = conversation.messages.len();
        match self.hooks.run(
            HookEvent::OnBudgetWarning,
            HookContext {
                cwd: &self.turn_context.cwd.display().to_string(),
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
            Err(error) if self.turn_context.emit_deltas => {
                self.sink.emit(
                    &self
                        .events
                        .error(&format!("on_budget_warning hook failed: {error}")),
                )?;
            }
            _ => {}
        }

        if self.turn_context.emit_deltas {
            self.run_compaction_hook(HookEvent::PreCompact, before_messages, None)?;
        }

        let after_messages = self.compact_and_persist(conversation, trigger)?;

        if self.turn_context.emit_deltas {
            self.run_compaction_hook(
                HookEvent::PostCompact,
                before_messages,
                Some(after_messages),
            )?;
        }

        Ok(())
    }

    fn compact_and_persist(
        &mut self,
        conversation: &mut Conversation,
        trigger: RuntimeCompactionTrigger,
    ) -> io::Result<usize> {
        let before_messages = conversation.messages.len();
        let mut task = RuntimeCompactionTask::start(trigger, before_messages);
        let compaction = context::compact_with_summary(
            self.provider,
            conversation,
            self.context_config,
            self.provider_config,
        );
        *conversation = compaction.conversation;
        let after_messages = conversation.messages.len();
        task.finish(after_messages);
        if task.should_persist_summary_state(self.turn_context.emit_deltas)
            && let Some(writer) = self.history_writer.as_deref_mut()
        {
            writer.append_compaction(task.before_messages(), task.after_messages())?;
            if let context::CompactionKind::RemoteSummary(summary) = compaction.kind {
                writer.append_summary_state(
                    task.before_messages(),
                    task.after_messages(),
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
                cwd: &self.turn_context.cwd.display().to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_policy_maps_pressure_to_trigger() {
        let below = context::ContextPressure {
            wire_tokens: 8,
            effective_limit: 20,
            soft_limit: 10,
            should_soft_compact: false,
            should_hard_compact: false,
        };
        let soft = context::ContextPressure {
            wire_tokens: 12,
            effective_limit: 20,
            soft_limit: 10,
            should_soft_compact: true,
            should_hard_compact: false,
        };
        let hard = context::ContextPressure {
            wire_tokens: 24,
            effective_limit: 20,
            soft_limit: 10,
            should_soft_compact: true,
            should_hard_compact: true,
        };

        assert_eq!(RuntimeCompactionPolicy::decide_for_pressure(below), None);
        assert_eq!(
            RuntimeCompactionPolicy::decide_for_pressure(soft),
            Some(RuntimeCompactionTrigger::SoftLimit)
        );
        assert_eq!(
            RuntimeCompactionPolicy::decide_for_pressure(hard),
            Some(RuntimeCompactionTrigger::HardLimit)
        );
    }

    #[test]
    fn compaction_task_records_trigger_and_message_counts() {
        let mut task = RuntimeCompactionTask::start(RuntimeCompactionTrigger::PromptTooLong, 11);

        assert_eq!(task.trigger(), RuntimeCompactionTrigger::PromptTooLong);
        assert_eq!(task.before_messages(), 11);

        task.finish(4);

        assert_eq!(task.after_messages(), 4);
        assert!(task.should_persist_summary_state(true));
        assert!(!task.should_persist_summary_state(false));
    }
}
