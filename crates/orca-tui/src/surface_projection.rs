#![allow(dead_code)]

use std::collections::BTreeMap;

use orca_core::cost_types::UsageTotals;
use orca_core::plan_types::{PlanItem, PlanStatus};
use orca_runtime::surface::{
    AssistantChannel, AssistantPatch, OperationPatch, OperationTerminal, SurfaceAssistantStream,
    SurfaceAssistantStreamState, SurfaceCommitBatch, SurfaceCompletedModelResponse, SurfaceCursor,
    SurfaceEvent, SurfaceFileChange, SurfaceHistoryMessage, SurfaceInputPresentation, SurfaceItem,
    SurfaceOperationId, SurfaceStreamId, SurfaceToolResultKind, SurfaceUserInputState, ToolPatch,
};

use crate::types::{TuiEvent, TuiTaskLifecycle};

pub(crate) fn history_messages_from_surface_snapshot(
    snapshot: &orca_runtime::surface::SurfaceSnapshot,
) -> Vec<crate::types::ChatMessage> {
    history_messages_from_surface_items(&snapshot.items)
}

pub(crate) fn history_messages_from_surface_history(
    messages: &[SurfaceHistoryMessage],
) -> Vec<crate::types::ChatMessage> {
    messages
        .iter()
        .flat_map(|message| match message {
            SurfaceHistoryMessage::System { .. } => Vec::new(),
            SurfaceHistoryMessage::User { content, .. } => {
                vec![crate::types::ChatMessage::User(
                    content.as_str().to_string(),
                )]
            }
            SurfaceHistoryMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
                ..
            } => {
                let mut projected = Vec::new();
                if let Some(reasoning) = reasoning_content
                    .as_ref()
                    .filter(|text| !text.as_str().trim().is_empty())
                {
                    projected.push(crate::types::ChatMessage::Reasoning(
                        reasoning.as_str().to_string(),
                    ));
                }
                if let Some(content) = content
                    .as_ref()
                    .filter(|text| !text.as_str().trim().is_empty())
                {
                    projected.push(crate::types::ChatMessage::Assistant(
                        content.as_str().to_string(),
                    ));
                }
                if content
                    .as_ref()
                    .is_none_or(|text| text.as_str().trim().is_empty())
                    && reasoning_content
                        .as_ref()
                        .is_none_or(|text| text.as_str().trim().is_empty())
                    && !tool_calls.is_empty()
                {
                    projected.push(crate::types::ChatMessage::System(
                        "Previous assistant requested tools".to_string(),
                    ));
                }
                projected
            }
            SurfaceHistoryMessage::Tool {
                tool_call_id,
                content,
                ..
            } => vec![crate::types::ChatMessage::ToolCall {
                id: tool_call_id.as_str().to_string(),
                name: format!("tool:{}", tool_call_id.as_str()),
                target: None,
                status: "completed".to_string(),
                output: (!content.as_str().is_empty()).then(|| content.as_str().to_string()),
                diff: None,
                kind: None,
                expanded: false,
            }],
        })
        .collect()
}

fn history_messages_from_surface_items(items: &[SurfaceItem]) -> Vec<crate::types::ChatMessage> {
    items
        .iter()
        .filter_map(history_message_from_surface_item)
        .collect()
}

fn history_message_from_surface_item(item: &SurfaceItem) -> Option<crate::types::ChatMessage> {
    match item {
        SurfaceItem::UserMessage { input, .. } => match input {
            SurfaceUserInputState::Pending { presentation, .. }
            | SurfaceUserInputState::ResolutionFailed { presentation, .. } => {
                visible_input_text(presentation).map(crate::types::ChatMessage::User)
            }
            SurfaceUserInputState::Resolved { fact } => match fact {
                orca_runtime::surface::SurfaceResolvedInputFact::Replayable { input, .. } => Some(
                    crate::types::ChatMessage::User(input.canonical_text.as_str().to_string()),
                ),
                orca_runtime::surface::SurfaceResolvedInputFact::NonReplayable {
                    presentation,
                    ..
                } => visible_input_text(presentation).map(crate::types::ChatMessage::User),
            },
        },
        SurfaceItem::SystemMessage { content, .. } => Some(crate::types::ChatMessage::System(
            content.as_str().to_string(),
        )),
        SurfaceItem::AssistantMessage { text, .. } => (!text.as_str().trim().is_empty())
            .then(|| crate::types::ChatMessage::Assistant(text.as_str().to_string())),
        SurfaceItem::AssistantReasoning {
            content, summary, ..
        } => {
            let text = if content.as_str().trim().is_empty() {
                summary.as_str()
            } else {
                content.as_str()
            };
            (!text.trim().is_empty())
                .then(|| crate::types::ChatMessage::Reasoning(text.to_string()))
        }
        SurfaceItem::AssistantPlan { text, .. } => (!text.as_str().trim().is_empty())
            .then(|| crate::types::ChatMessage::ProposedPlan(text.as_str().to_string())),
        SurfaceItem::ToolResultMessage {
            tool_call_id,
            content,
            terminal,
            ..
        } => {
            let output = (!content.as_str().is_empty()).then(|| content.as_str().to_string());
            Some(crate::types::ChatMessage::ToolCall {
                id: tool_call_id.as_str().to_string(),
                name: format!("tool:{}", tool_call_id.as_str()),
                target: None,
                status: tool_result_status(terminal.kind).to_string(),
                output,
                diff: None,
                kind: None,
                expanded: false,
            })
        }
    }
}

fn visible_input_text(presentation: &SurfaceInputPresentation) -> Option<String> {
    match presentation {
        SurfaceInputPresentation::Visible { text } => Some(text.as_str().to_string()),
        SurfaceInputPresentation::Redacted => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceProjectionError {
    CursorGap {
        expected: SurfaceCursor,
        observed: SurfaceCursor,
    },
    UnknownAssistantStream {
        stream_id: SurfaceStreamId,
    },
}

pub(crate) struct TuiSurfaceProjection {
    cursor: SurfaceCursor,
    assistant_streams: BTreeMap<SurfaceStreamId, SurfaceAssistantStream>,
    focused_operation: Option<SurfaceOperationId>,
    pending_turn_started: Option<TuiTaskLifecycle>,
}

impl TuiSurfaceProjection {
    pub(crate) fn from_snapshot(cursor: SurfaceCursor, streams: &[SurfaceAssistantStream]) -> Self {
        Self {
            cursor,
            assistant_streams: streams
                .iter()
                .map(|stream| (stream.stream_id.clone(), stream.clone()))
                .collect(),
            focused_operation: None,
            pending_turn_started: None,
        }
    }

    pub(crate) fn from_surface_snapshot(snapshot: &orca_runtime::surface::SurfaceSnapshot) -> Self {
        let mut projection = Self::from_snapshot(
            snapshot.cursor.clone(),
            snapshot.assistant_streams.as_slice(),
        );
        projection.focused_operation = snapshot
            .foreground_operation
            .as_ref()
            .map(|operation| operation.operation_id.clone());
        projection.pending_turn_started = snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| operation.terminal.is_none())
            .and_then(|operation| operation.agent_loop_turns.last())
            .map(|turn| TuiTaskLifecycle {
                id: turn.task_id.as_str().to_string(),
                kind: "agent".to_string(),
                status: "running".to_string(),
                turn: turn.ordinal,
            });
        projection
    }

    pub(crate) fn hydrate_open_streams(&mut self) -> Vec<TuiEvent> {
        let mut projected = self
            .pending_turn_started
            .take()
            .map(|task| {
                vec![TuiEvent::TurnStarted {
                    turn: task.turn,
                    task: Some(task),
                }]
            })
            .unwrap_or_default();
        projected.extend(
            self.assistant_streams
                .values()
                .filter(|stream| {
                    stream.state == SurfaceAssistantStreamState::Open
                        && !stream.text.as_str().is_empty()
                })
                .map(|stream| match stream.channel {
                    AssistantChannel::Message => {
                        TuiEvent::MessageDelta(stream.text.as_str().to_string())
                    }
                    AssistantChannel::Reasoning => {
                        TuiEvent::ReasoningDelta(stream.text.as_str().to_string())
                    }
                    AssistantChannel::Plan => TuiEvent::Notice(stream.text.as_str().to_string()),
                })
                .collect::<Vec<_>>(),
        );
        projected
    }

    #[allow(dead_code)]
    pub(crate) fn focus_operation(&mut self, operation_id: SurfaceOperationId) {
        self.focused_operation = Some(operation_id);
    }

    pub(crate) fn cursor(&self) -> &SurfaceCursor {
        &self.cursor
    }

    pub(crate) fn reduce_typed_batch(
        &mut self,
        batch: &SurfaceCommitBatch,
    ) -> Result<Vec<TuiEvent>, SurfaceProjectionError> {
        if batch.cursor_before != self.cursor {
            return Err(SurfaceProjectionError::CursorGap {
                expected: self.cursor.clone(),
                observed: batch.cursor_before.clone(),
            });
        }

        let mut assistant_streams = self.assistant_streams.clone();
        let mut focused_operation = self.focused_operation.clone();
        let mut projected = Vec::new();
        for envelope in batch.events.as_slice() {
            match &envelope.event {
                SurfaceEvent::Assistant(AssistantPatch::StreamOpened { stream }) => {
                    assistant_streams
                        .entry(stream.stream_id.clone())
                        .and_modify(|current| *current = stream.clone())
                        .or_insert_with(|| stream.clone());
                }
                SurfaceEvent::Assistant(AssistantPatch::Delta {
                    stream_id,
                    offset,
                    text,
                }) => {
                    let stream = assistant_streams.get_mut(stream_id).ok_or_else(|| {
                        SurfaceProjectionError::UnknownAssistantStream {
                            stream_id: stream_id.clone(),
                        }
                    })?;
                    if stream.state != SurfaceAssistantStreamState::Open
                        || stream.next_offset != *offset
                    {
                        return Err(SurfaceProjectionError::UnknownAssistantStream {
                            stream_id: stream_id.clone(),
                        });
                    }
                    stream.text = orca_runtime::surface::DisplayText::new(format!(
                        "{}{}",
                        stream.text.as_str(),
                        text.as_str()
                    ));
                    stream.next_offset = orca_runtime::surface::ByteOffset::new(
                        offset.get().saturating_add(text.as_str().len() as u64),
                    );
                    match stream.channel {
                        AssistantChannel::Message => {
                            projected.push(TuiEvent::MessageDelta(text.as_str().to_string()));
                        }
                        AssistantChannel::Reasoning => {
                            projected.push(TuiEvent::ReasoningDelta(text.as_str().to_string()));
                        }
                        AssistantChannel::Plan => {}
                    }
                }
                SurfaceEvent::Assistant(AssistantPatch::StreamDiscarded { stream_id, .. }) => {
                    if let Some(stream) = assistant_streams.get_mut(stream_id) {
                        stream.state = SurfaceAssistantStreamState::Discarded;
                    }
                }
                SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted { response }) => {
                    for stream in assistant_streams.values_mut().filter(|stream| {
                        stream.turn_id == response.turn_id
                            && stream.state == SurfaceAssistantStreamState::Open
                    }) {
                        stream.state = SurfaceAssistantStreamState::Completed;
                    }
                    projected.push(response_completed_event(response));
                }
                SurfaceEvent::Tool(ToolPatch::Requested { request }) => {
                    projected.push(TuiEvent::ToolRequested {
                        id: request.tool_call_id.as_str().to_string(),
                        name: request.name.as_str().to_string(),
                        target: request
                            .target
                            .as_ref()
                            .map(|target| target.as_str().to_string()),
                    });
                }
                SurfaceEvent::Tool(ToolPatch::ArgumentsProgress {
                    tool_call_id,
                    arguments_bytes,
                }) => projected.push(TuiEvent::ToolCallProgress {
                    id: tool_call_id.as_str().to_string(),
                    name: None,
                    arguments_bytes: usize::try_from(arguments_bytes.get()).unwrap_or(usize::MAX),
                }),
                SurfaceEvent::Tool(ToolPatch::OutputDelta {
                    tool_call_id,
                    chunk,
                    ..
                }) => projected.push(TuiEvent::ToolOutputDelta {
                    id: tool_call_id.as_str().to_string(),
                    chunk: chunk.as_str().to_string(),
                }),
                SurfaceEvent::Tool(ToolPatch::Completed { result }) => {
                    let (diff, kind) = match &result.file_change {
                        Some(SurfaceFileChange::UnifiedDiff { text, .. }) => (
                            Some(text.as_str().to_string()),
                            Some("file_change".to_string()),
                        ),
                        Some(SurfaceFileChange::PreviewOmitted { .. }) => {
                            (None, Some("file_change".to_string()))
                        }
                        None => (None, None),
                    };
                    projected.push(TuiEvent::ToolCompleted {
                        id: result.tool_call_id.as_str().to_string(),
                        name: result.name.as_str().to_string(),
                        status: tool_result_status(result.terminal.kind).to_string(),
                        output: result
                            .output
                            .as_ref()
                            .or(result.error.as_ref())
                            .map(|text| text.as_str().to_string())
                            .unwrap_or_default(),
                        diff,
                        kind,
                    });
                }
                SurfaceEvent::Plan(plan) => projected.push(TuiEvent::PlanUpdated {
                    explanation: plan
                        .explanation
                        .as_ref()
                        .map(|value| value.as_str().to_string()),
                    plan: plan
                        .items
                        .iter()
                        .map(|item| PlanItem {
                            step: item.step.as_str().to_string(),
                            status: match item.status {
                                orca_runtime::surface::SurfacePlanStatus::Pending => {
                                    PlanStatus::Pending
                                }
                                orca_runtime::surface::SurfacePlanStatus::InProgress => {
                                    PlanStatus::InProgress
                                }
                                orca_runtime::surface::SurfacePlanStatus::Completed => {
                                    PlanStatus::Completed
                                }
                            },
                        })
                        .collect(),
                }),
                SurfaceEvent::Usage(usage) => {
                    projected.push(TuiEvent::UsageUpdated(UsageTotals {
                        input_tokens: usage.thread_total.input_tokens,
                        output_tokens: usage.thread_total.output_tokens,
                        cache_tokens: usage.thread_total.cache_tokens,
                        estimated_cost_usd: usage.thread_total.estimated_cost_usd_micros as f64
                            / 1_000_000.0,
                    }));
                }
                SurfaceEvent::Context(context) => {
                    projected.push(TuiEvent::ContextUpdated {
                        used_tokens: usize::try_from(context.used_tokens).unwrap_or(usize::MAX),
                        limit_tokens: usize::try_from(context.limit_tokens).unwrap_or(usize::MAX),
                    });
                    match &context.compaction {
                        orca_runtime::surface::CompactionState::Running { .. } => {
                            projected.push(TuiEvent::CompactionStarted);
                        }
                        orca_runtime::surface::CompactionState::Completed {
                            reason,
                            strategy,
                            before_messages,
                            after_messages,
                            collapsed_messages,
                            status_text,
                            ..
                        } => {
                            projected.push(TuiEvent::Compacted {
                                before_messages: usize::try_from(*before_messages)
                                    .unwrap_or(usize::MAX),
                                after_messages: usize::try_from(*after_messages)
                                    .unwrap_or(usize::MAX),
                                reason: match reason {
                                    orca_runtime::surface::CompactionReason::Manual => {
                                        "manual".to_string()
                                    }
                                    orca_runtime::surface::CompactionReason::Automatic => {
                                        "automatic".to_string()
                                    }
                                },
                                strategy: strategy.as_str().to_string(),
                                collapsed_messages: usize::try_from(*collapsed_messages)
                                    .unwrap_or(usize::MAX),
                                status_text: status_text.as_str().to_string(),
                            });
                        }
                        orca_runtime::surface::CompactionState::Idle => {}
                    }
                }
                SurfaceEvent::Operation(OperationPatch::AgentLoopTurnStarted { turn })
                    if focused_operation.as_ref() == Some(&turn.fence.operation_id) =>
                {
                    projected.push(TuiEvent::TurnStarted {
                        turn: turn.ordinal,
                        task: Some(TuiTaskLifecycle {
                            id: turn.task_id.as_str().to_string(),
                            kind: "agent".to_string(),
                            status: "running".to_string(),
                            turn: turn.ordinal,
                        }),
                    });
                }
                SurfaceEvent::Operation(OperationPatch::Terminal { record })
                    if focused_operation.as_ref() == Some(&record.operation_id) =>
                {
                    if let Some(status) = operation_terminal_status(&record.terminal) {
                        projected.push(TuiEvent::SessionCompleted {
                            status: status.to_string(),
                        });
                    }
                    focused_operation = None;
                }
                _ => {}
            }
        }
        self.assistant_streams = assistant_streams;
        self.focused_operation = focused_operation;
        self.cursor = batch.cursor_after.clone();
        Ok(projected)
    }
}

fn tool_result_status(kind: SurfaceToolResultKind) -> &'static str {
    match kind {
        SurfaceToolResultKind::Success => "completed",
        SurfaceToolResultKind::Denied => "denied",
        SurfaceToolResultKind::Cancelled => "cancelled",
        SurfaceToolResultKind::TimedOut | SurfaceToolResultKind::InvalidArguments => "failed",
        SurfaceToolResultKind::ExternalEffectAmbiguous
        | SurfaceToolResultKind::ObservationUnavailable
        | SurfaceToolResultKind::CleanupAmbiguous => "indeterminate",
        SurfaceToolResultKind::Failed => "failed",
    }
}

fn operation_terminal_status(terminal: &OperationTerminal) -> Option<&'static str> {
    match terminal {
        OperationTerminal::Succeeded { .. } => Some("success"),
        OperationTerminal::Cancelled { .. } => Some("cancelled"),
        OperationTerminal::BudgetExhausted { .. } => Some("budget_exhausted"),
        OperationTerminal::NotAdmitted { .. } => Some("not_admitted"),
        OperationTerminal::Failed { class, .. } => match class {
            orca_runtime::surface::FailureClass::Verification => Some("verification_failed"),
            _ => Some("failed"),
        },
        OperationTerminal::Panicked { .. }
        | OperationTerminal::JoinFailed { .. }
        | OperationTerminal::AbortedByRuntimeRestart { .. } => Some("failed"),
        OperationTerminal::Shutdown { .. } => Some("cancelled"),
    }
}

fn response_completed_event(response: &SurfaceCompletedModelResponse) -> TuiEvent {
    TuiEvent::AssistantResponseCompleted(
        response
            .message_item
            .as_ref()
            .map(|item| item.text.as_str().to_string()),
        response
            .reasoning_item
            .as_ref()
            .map(|item| item.content.as_str().to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_runtime::surface::{
        ByteOffset, CommitClass, CursorSourceRevision, DisplayText, DurableRevision, NonEmptyVec,
        SequenceNumber, Sha256Digest, SurfaceCommitId, SurfaceEventEnvelope, SurfaceEventId,
        SurfaceIncarnation, SurfaceInputCorrelationId, SurfaceItemId, SurfaceScope,
        SurfaceThreadId, SurfaceTurnId,
    };

    fn uuid_v7_bytes(seed: u8) -> [u8; 16] {
        let mut bytes = [seed; 16];
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        bytes
    }

    fn cursor(next_seq: u64, revision: u64) -> SurfaceCursor {
        SurfaceCursor {
            thread_id: SurfaceThreadId::try_from_bytes(uuid_v7_bytes(1)).unwrap(),
            incarnation: SurfaceIncarnation::try_from_bytes(uuid_v7_bytes(2)).unwrap(),
            next_seq: SequenceNumber::new(next_seq),
            source_revision: CursorSourceRevision::Recorded {
                durable_revision: DurableRevision::try_new(revision).unwrap(),
            },
        }
    }

    #[test]
    fn typed_history_projection_preserves_visible_items_and_redacts_secrets() {
        let turn_id = SurfaceTurnId::new();
        let items = vec![
            SurfaceItem::UserMessage {
                id: SurfaceItemId::new(),
                turn_id: turn_id.clone(),
                input: SurfaceUserInputState::Pending {
                    presentation: SurfaceInputPresentation::Visible {
                        text: DisplayText::new("visible prompt"),
                    },
                    correlation_id: SurfaceInputCorrelationId::try_from_bytes(uuid_v7_bytes(8))
                        .unwrap(),
                },
                pinned: false,
                origin: orca_runtime::surface::SurfaceItemOrigin::UserInput,
            },
            SurfaceItem::UserMessage {
                id: SurfaceItemId::new(),
                turn_id: turn_id.clone(),
                input: SurfaceUserInputState::Pending {
                    presentation: SurfaceInputPresentation::Redacted,
                    correlation_id: SurfaceInputCorrelationId::try_from_bytes(uuid_v7_bytes(9))
                        .unwrap(),
                },
                pinned: false,
                origin: orca_runtime::surface::SurfaceItemOrigin::UserInput,
            },
            SurfaceItem::SystemMessage {
                id: SurfaceItemId::new(),
                content: DisplayText::new("system"),
                pinned: false,
                origin: orca_runtime::surface::SurfaceItemOrigin::RuntimeContext,
            },
            SurfaceItem::AssistantMessage {
                id: SurfaceItemId::new(),
                turn_id: turn_id.clone(),
                text: DisplayText::new("answer"),
                pinned: false,
            },
            SurfaceItem::AssistantReasoning {
                id: SurfaceItemId::new(),
                turn_id: turn_id.clone(),
                summary: DisplayText::new("summary"),
                content: DisplayText::new("reasoning"),
                pinned: false,
            },
            SurfaceItem::AssistantPlan {
                id: SurfaceItemId::new(),
                turn_id,
                text: DisplayText::new("plan"),
                pinned: false,
            },
        ];

        let messages = history_messages_from_surface_items(&items);
        assert!(matches!(
            messages.as_slice(),
            [
                crate::types::ChatMessage::User(prompt),
                crate::types::ChatMessage::System(system),
                crate::types::ChatMessage::Assistant(answer),
                crate::types::ChatMessage::Reasoning(reasoning),
                crate::types::ChatMessage::ProposedPlan(plan),
            ] if prompt == "visible prompt"
                && system == "system"
                && answer == "answer"
                && reasoning == "reasoning"
                && plan == "plan"
        ));
    }

    #[test]
    fn runtime_history_projection_preserves_message_order_and_tool_identity() {
        let tool_id = orca_runtime::surface::SurfaceHistoryId::try_new("call-1").unwrap();
        let history = vec![
            SurfaceHistoryMessage::User {
                role: orca_runtime::surface::SurfaceHistoryUserRole::User,
                content: DisplayText::new("hello"),
            },
            SurfaceHistoryMessage::Assistant {
                role: orca_runtime::surface::SurfaceHistoryAssistantRole::Assistant,
                content: Some(DisplayText::new("hi")),
                reasoning_content: Some(DisplayText::new("reason")),
                tool_calls: Vec::new(),
            },
            SurfaceHistoryMessage::Tool {
                role: orca_runtime::surface::SurfaceHistoryToolRole::Tool,
                tool_call_id: tool_id,
                content: DisplayText::new("done"),
            },
        ];

        let messages = history_messages_from_surface_history(&history);
        assert!(matches!(
            messages[0],
            crate::types::ChatMessage::User(ref text) if text == "hello"
        ));
        assert!(matches!(
            messages[1],
            crate::types::ChatMessage::Reasoning(ref text) if text == "reason"
        ));
        assert!(matches!(
            messages[2],
            crate::types::ChatMessage::Assistant(ref text) if text == "hi"
        ));
        assert!(matches!(
            messages[3],
            crate::types::ChatMessage::ToolCall { ref id, .. } if id == "call-1"
        ));
    }

    #[test]
    fn typed_assistant_delta_projects_only_after_stream_identity_is_known() {
        let before = cursor(0, 1);
        let stream_id = SurfaceStreamId::try_from_bytes(uuid_v7_bytes(3)).unwrap();
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: orca_runtime::surface::ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(2).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(4)).unwrap(),
        };
        let after = SurfaceCursor {
            next_seq: SequenceNumber::new(1),
            source_revision: CursorSourceRevision::Recorded {
                durable_revision: DurableRevision::try_new(2).unwrap(),
            },
            ..before.clone()
        };
        let event = SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(5)).unwrap(),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: SurfaceEvent::Assistant(AssistantPatch::Delta {
                stream_id: stream_id.clone(),
                offset: ByteOffset::new(0),
                text: DisplayText::new("hello"),
            }),
        };
        let batch = SurfaceCommitBatch {
            cursor_before: before.clone(),
            cursor_after: after,
            commit_class,
            event_count: 1,
            batch_digest: Sha256Digest::new([0; 32]),
            events: NonEmptyVec::try_new(vec![event]).unwrap(),
        };
        let mut projection = TuiSurfaceProjection::from_snapshot(before, &[]);

        assert!(matches!(
            projection.reduce_typed_batch(&batch),
            Err(SurfaceProjectionError::UnknownAssistantStream { stream_id: observed })
                if observed == stream_id
        ));
    }

    #[test]
    fn cursor_gap_is_rejected_without_advancing_projection() {
        let expected = cursor(3, 2);
        let observed = cursor(4, 3);
        let mut projection = TuiSurfaceProjection::from_snapshot(expected.clone(), &[]);
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: orca_runtime::surface::ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(4).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(6)).unwrap(),
        };
        let event = SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(7)).unwrap(),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: SurfaceEvent::Context(orca_runtime::surface::SurfaceContextSnapshot {
                revision: orca_runtime::surface::ContextRevision::try_new(2).unwrap(),
                used_tokens: 1,
                limit_tokens: 2,
                compaction: orca_runtime::surface::CompactionState::Idle,
                fragments: Vec::new(),
                provider_replay: orca_runtime::surface::ProviderReplayHealth::None,
            }),
        };
        let batch = SurfaceCommitBatch {
            cursor_before: observed.clone(),
            cursor_after: SurfaceCursor {
                next_seq: SequenceNumber::new(5),
                source_revision: CursorSourceRevision::Recorded {
                    durable_revision: DurableRevision::try_new(4).unwrap(),
                },
                ..observed.clone()
            },
            commit_class,
            event_count: 1,
            batch_digest: Sha256Digest::new([1; 32]),
            events: NonEmptyVec::try_new(vec![event]).unwrap(),
        };

        assert!(matches!(
            projection.reduce_typed_batch(&batch),
            Err(SurfaceProjectionError::CursorGap {
                expected: gap_expected,
                observed: gap_observed,
            }) if gap_expected == expected && gap_observed == observed
        ));
        assert_eq!(projection.cursor(), &expected);
    }

    #[test]
    fn typed_tool_statuses_use_existing_tui_vocabulary() {
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::Success),
            "completed"
        );
        assert_eq!(tool_result_status(SurfaceToolResultKind::Denied), "denied");
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::Cancelled),
            "cancelled"
        );
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::TimedOut),
            "failed"
        );
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::InvalidArguments),
            "failed"
        );
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::ExternalEffectAmbiguous),
            "indeterminate"
        );
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::ObservationUnavailable),
            "indeterminate"
        );
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::CleanupAmbiguous),
            "indeterminate"
        );
        assert_eq!(tool_result_status(SurfaceToolResultKind::Failed), "failed");
    }

    #[test]
    fn snapshot_hydration_emits_running_turn_once() {
        let mut projection = TuiSurfaceProjection {
            cursor: cursor(0, 1),
            assistant_streams: BTreeMap::new(),
            focused_operation: None,
            pending_turn_started: Some(TuiTaskLifecycle {
                id: "task-1".to_string(),
                kind: "agent".to_string(),
                status: "running".to_string(),
                turn: 4,
            }),
        };

        assert!(matches!(
            projection.hydrate_open_streams().as_slice(),
            [TuiEvent::TurnStarted {
                turn: 4,
                task: Some(TuiTaskLifecycle { id, status, .. }),
            }] if id == "task-1" && status == "running"
        ));
        assert!(projection.hydrate_open_streams().is_empty());
    }
}
