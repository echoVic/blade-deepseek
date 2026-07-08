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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeCompactionStrategy {
    LocalTruncation,
    RemoteSummary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeCompactionReason {
    ApproachingContextLimit,
    ExceededContextLimit,
    PromptTooLongRecovery,
}

impl RuntimeCompactionReason {
    pub(crate) fn status_text(&self) -> &'static str {
        match self {
            Self::ApproachingContextLimit => "compacted context near token limit",
            Self::ExceededContextLimit => "compacted context at token limit",
            Self::PromptTooLongRecovery => "compacted context after prompt-too-long",
        }
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::ApproachingContextLimit => "approaching_context_limit",
            Self::ExceededContextLimit => "exceeded_context_limit",
            Self::PromptTooLongRecovery => "prompt_too_long_recovery",
        }
    }
}

impl RuntimeCompactionStrategy {
    pub(crate) fn from_compaction_kind(kind: &context::CompactionKind) -> Self {
        match kind {
            context::CompactionKind::LocalTruncation => Self::LocalTruncation,
            context::CompactionKind::RemoteSummary(_) => Self::RemoteSummary,
        }
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::LocalTruncation => "local_truncation",
            Self::RemoteSummary => "remote_summary",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeCompactionDetails {
    pub(crate) trigger: RuntimeCompactionTrigger,
    pub(crate) reason: RuntimeCompactionReason,
    pub(crate) strategy: RuntimeCompactionStrategy,
    pub(crate) before_messages: usize,
    pub(crate) after_messages: usize,
    pub(crate) collapsed_messages: usize,
    pub(crate) status_text: &'static str,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RuntimeCompactionRetryState {
    prompt_too_long_retried: bool,
}

impl RuntimeCompactionRetryState {
    pub(crate) fn record_prompt_too_long_retry(&mut self) {
        self.prompt_too_long_retried = true;
    }

    #[cfg(test)]
    pub(crate) fn has_prompt_too_long_retry(&self) -> bool {
        self.prompt_too_long_retried
    }

    pub(crate) fn reset(&mut self) {
        self.prompt_too_long_retried = false;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeCompactionRetryDecision {
    CompactAndRetry {
        trigger: RuntimeCompactionTrigger,
        reason: RuntimeCompactionReason,
    },
    SurfaceError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeCompactionOutcome {
    trigger: RuntimeCompactionTrigger,
    before_messages: usize,
    after_messages: usize,
    strategy: RuntimeCompactionStrategy,
}

impl RuntimeCompactionOutcome {
    pub(crate) fn trigger(&self) -> RuntimeCompactionTrigger {
        self.trigger
    }

    pub(crate) fn before_messages(&self) -> usize {
        self.before_messages
    }

    pub(crate) fn after_messages(&self) -> usize {
        self.after_messages
    }

    pub(crate) fn strategy(&self) -> RuntimeCompactionStrategy {
        self.strategy
    }

    pub(crate) fn reason(&self) -> RuntimeCompactionReason {
        match self.trigger() {
            RuntimeCompactionTrigger::SoftLimit => RuntimeCompactionReason::ApproachingContextLimit,
            RuntimeCompactionTrigger::HardLimit => RuntimeCompactionReason::ExceededContextLimit,
            RuntimeCompactionTrigger::PromptTooLong => {
                RuntimeCompactionReason::PromptTooLongRecovery
            }
        }
    }

    pub(crate) fn details(&self) -> RuntimeCompactionDetails {
        let reason = self.reason();
        RuntimeCompactionDetails {
            trigger: self.trigger(),
            reason,
            strategy: self.strategy(),
            before_messages: self.before_messages(),
            after_messages: self.after_messages(),
            collapsed_messages: self.before_messages().saturating_sub(self.after_messages()),
            status_text: reason.status_text(),
        }
    }

    pub(crate) fn should_persist_summary_state(&self, emit_deltas: bool) -> bool {
        if !emit_deltas {
            return false;
        }

        match (self.trigger(), self.strategy()) {
            (
                RuntimeCompactionTrigger::SoftLimit
                | RuntimeCompactionTrigger::HardLimit
                | RuntimeCompactionTrigger::PromptTooLong,
                RuntimeCompactionStrategy::LocalTruncation
                | RuntimeCompactionStrategy::RemoteSummary,
            ) => true,
        }
    }
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

    pub(crate) fn decide_for_provider_error(
        error: &str,
        retry_state: &RuntimeCompactionRetryState,
    ) -> RuntimeCompactionRetryDecision {
        if context::is_prompt_too_long_error(error) && !retry_state.prompt_too_long_retried {
            RuntimeCompactionRetryDecision::CompactAndRetry {
                trigger: RuntimeCompactionTrigger::PromptTooLong,
                reason: RuntimeCompactionReason::PromptTooLongRecovery,
            }
        } else {
            RuntimeCompactionRetryDecision::SurfaceError
        }
    }
}

pub(crate) struct RuntimeCompactionTask {
    trigger: RuntimeCompactionTrigger,
    before_messages: usize,
}

impl RuntimeCompactionTask {
    pub(crate) fn start(trigger: RuntimeCompactionTrigger, before_messages: usize) -> Self {
        Self {
            trigger,
            before_messages,
        }
    }

    pub(crate) fn finish(
        &self,
        after_messages: usize,
        kind: &context::CompactionKind,
    ) -> RuntimeCompactionOutcome {
        RuntimeCompactionOutcome {
            trigger: self.trigger(),
            before_messages: self.before_messages(),
            after_messages,
            strategy: RuntimeCompactionStrategy::from_compaction_kind(kind),
        }
    }

    pub(crate) fn trigger(&self) -> RuntimeCompactionTrigger {
        self.trigger
    }

    pub(crate) fn before_messages(&self) -> usize {
        self.before_messages
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

    pub(crate) fn compact_after_provider_error_retry(
        &mut self,
        conversation: &mut Conversation,
        trigger: RuntimeCompactionTrigger,
    ) -> io::Result<()> {
        self.compact_and_persist(conversation, trigger)?;
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
        let task = RuntimeCompactionTask::start(trigger, before_messages);
        let compaction = context::compact_with_summary(
            self.provider,
            conversation,
            self.context_config,
            self.provider_config,
        );
        *conversation = compaction.conversation;
        let after_messages = conversation.messages.len();
        let outcome = task.finish(after_messages, &compaction.kind);
        let details = outcome.details();
        if self.turn_context.emit_deltas {
            self.sink.emit(&self.events.context_compacted(
                details.reason.as_str(),
                details.strategy.as_str(),
                details.before_messages,
                details.after_messages,
                details.collapsed_messages,
                details.status_text,
            ))?;
        }
        if outcome.should_persist_summary_state(self.turn_context.emit_deltas)
            && let Some(writer) = self.history_writer.as_deref_mut()
        {
            writer.append_compaction(details.before_messages, details.after_messages)?;
            if details.strategy == RuntimeCompactionStrategy::RemoteSummary
                && let context::CompactionKind::RemoteSummary(summary) = compaction.kind
            {
                writer.append_summary_state(
                    details.before_messages,
                    details.after_messages,
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
        let task = RuntimeCompactionTask::start(RuntimeCompactionTrigger::PromptTooLong, 11);

        assert_eq!(task.trigger(), RuntimeCompactionTrigger::PromptTooLong);
        assert_eq!(task.before_messages(), 11);

        let outcome = task.finish(4, &context::CompactionKind::LocalTruncation);

        assert_eq!(outcome.trigger(), RuntimeCompactionTrigger::PromptTooLong);
        assert_eq!(outcome.before_messages(), 11);
        assert_eq!(outcome.after_messages(), 4);
        assert_eq!(
            outcome.strategy(),
            RuntimeCompactionStrategy::LocalTruncation
        );
        assert!(outcome.should_persist_summary_state(true));
        assert!(!outcome.should_persist_summary_state(false));
    }

    #[test]
    fn compaction_outcome_records_remote_summary_strategy() {
        let task = RuntimeCompactionTask::start(RuntimeCompactionTrigger::HardLimit, 9);
        let outcome = task.finish(
            3,
            &context::CompactionKind::RemoteSummary("summary".to_string()),
        );

        assert_eq!(outcome.trigger(), RuntimeCompactionTrigger::HardLimit);
        assert_eq!(outcome.before_messages(), 9);
        assert_eq!(outcome.after_messages(), 3);
        assert_eq!(outcome.strategy(), RuntimeCompactionStrategy::RemoteSummary);
    }

    #[test]
    fn compaction_outcome_exposes_reason_and_details() {
        let task = RuntimeCompactionTask::start(RuntimeCompactionTrigger::PromptTooLong, 12);
        let outcome = task.finish(
            5,
            &context::CompactionKind::RemoteSummary("summary".to_string()),
        );

        assert_eq!(
            outcome.reason(),
            RuntimeCompactionReason::PromptTooLongRecovery
        );

        let details = outcome.details();
        assert_eq!(details.trigger, RuntimeCompactionTrigger::PromptTooLong);
        assert_eq!(
            details.reason,
            RuntimeCompactionReason::PromptTooLongRecovery
        );
        assert_eq!(details.strategy, RuntimeCompactionStrategy::RemoteSummary);
        assert_eq!(details.before_messages, 12);
        assert_eq!(details.after_messages, 5);
        assert_eq!(details.collapsed_messages, 7);
        assert_eq!(
            details.status_text,
            "compacted context after prompt-too-long"
        );
    }

    #[test]
    fn compaction_policy_decides_prompt_too_long_retry_once() {
        let mut retry_state = RuntimeCompactionRetryState::default();
        let decision = RuntimeCompactionPolicy::decide_for_provider_error(
            "DeepSeek provider error: prompt_too_long: context length exceeded",
            &retry_state,
        );

        assert_eq!(
            decision,
            RuntimeCompactionRetryDecision::CompactAndRetry {
                trigger: RuntimeCompactionTrigger::PromptTooLong,
                reason: RuntimeCompactionReason::PromptTooLongRecovery,
            }
        );

        retry_state.record_prompt_too_long_retry();
        let decision = RuntimeCompactionPolicy::decide_for_provider_error(
            "DeepSeek provider error: prompt_too_long: context length exceeded",
            &retry_state,
        );

        assert_eq!(decision, RuntimeCompactionRetryDecision::SurfaceError);
    }

    #[test]
    fn compaction_policy_surfaces_non_context_errors() {
        let retry_state = RuntimeCompactionRetryState::default();
        let decision = RuntimeCompactionPolicy::decide_for_provider_error(
            "DeepSeek provider error: quota exhausted",
            &retry_state,
        );

        assert_eq!(decision, RuntimeCompactionRetryDecision::SurfaceError);
    }

    #[test]
    fn compaction_step_emits_context_compacted_event() {
        let context_config = context::ContextConfig {
            max_tokens: 100,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(100),
            soft_compact_token_limit: Some(40),
        };
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("compaction-event-test".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, orca_core::config::OutputFormat::Jsonl);
        let cwd = std::path::Path::new(".");
        let subagent_type = orca_core::subagent_types::SubagentType::General;
        let mut conversation = Conversation::new();
        conversation.add_system("system".to_string());
        for index in 0..20 {
            conversation.add_user(format!("user message {index}: {}", "context ".repeat(8)));
            conversation.add_assistant(
                Some(format!(
                    "assistant message {index}: {}",
                    "details ".repeat(8)
                )),
                None,
                vec![],
            );
        }
        let before_messages = conversation.messages.len();

        RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &context_config,
            &provider_config,
            RuntimeTurnContext::new(cwd, "", 0, true, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        )
        .compact_after_provider_error_retry(
            &mut conversation,
            RuntimeCompactionTrigger::PromptTooLong,
        )
        .expect("compaction should emit event");

        drop(sink);
        let output = String::from_utf8(output).expect("jsonl is utf8");
        let compacted = output
            .lines()
            .find(|line| line.contains("\"type\":\"context.compacted\""))
            .expect("context compacted event emitted");
        let event: serde_json::Value =
            serde_json::from_str(compacted).expect("event should be json");

        assert_eq!(event["payload"]["before_messages"], before_messages);
        assert_eq!(
            event["payload"]["after_messages"],
            conversation.messages.len()
        );
        assert_eq!(event["payload"]["reason"], "prompt_too_long_recovery");
        assert_eq!(event["payload"]["strategy"], "remote_summary");
    }
}
