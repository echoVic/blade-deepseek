use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{
    ContextCompactedPayload, ContextCompactionStartedPayload, EventEnvelope, EventType,
};

use crate::types::{TuiEvent, TuiTaskLifecycle};

pub(crate) fn tui_event_from_runtime_event(event: &EventEnvelope) -> Option<TuiEvent> {
    match event.event_type {
        EventType::TurnStarted => Some(TuiEvent::TurnStarted {
            turn: event.payload["turn"].as_u64()?.try_into().ok()?,
            task: event.payload.get("task").and_then(tui_task_lifecycle),
        }),
        EventType::AssistantReasoningDelta => Some(TuiEvent::ReasoningDelta(
            event.payload["text"].as_str()?.to_string(),
        )),
        EventType::AssistantMessageDelta => Some(TuiEvent::MessageDelta(
            event.payload["text"].as_str()?.to_string(),
        )),
        EventType::UsageUpdated => Some(TuiEvent::UsageUpdated(UsageTotals {
            input_tokens: event.payload["input_tokens"].as_u64()?,
            output_tokens: event.payload["output_tokens"].as_u64()?,
            cache_tokens: event.payload["cache_tokens"].as_u64().unwrap_or_default(),
            estimated_cost_usd: event.payload["estimated_cost_usd"].as_f64()?,
        })),
        EventType::ContextUpdated => Some(TuiEvent::ContextUpdated {
            used_tokens: event.payload["used_tokens"].as_u64()? as usize,
            limit_tokens: event.payload["limit_tokens"].as_u64()? as usize,
        }),
        EventType::ContextCompactionStarted => {
            let _: ContextCompactionStartedPayload =
                serde_json::from_value(event.payload.clone()).ok()?;
            Some(TuiEvent::CompactionStarted)
        }
        EventType::ContextCompacted => {
            let payload: ContextCompactedPayload =
                serde_json::from_value(event.payload.clone()).ok()?;
            Some(TuiEvent::Compacted {
                before_messages: payload.before_messages,
                after_messages: payload.after_messages,
                reason: payload.reason,
                strategy: payload.strategy,
                collapsed_messages: payload.collapsed_messages,
                status_text: payload.status_text,
            })
        }
        EventType::ModelRouted => Some(TuiEvent::Notice(format!(
            "Model routed to {} ({})",
            event.payload["actual_model"].as_str()?,
            event.payload["reason"].as_str()?
        ))),
        EventType::ToolCallRequested => Some(TuiEvent::ToolRequested {
            id: event.payload["id"].as_str()?.to_string(),
            name: event.payload["name"].as_str()?.to_string(),
            target: event
                .payload
                .get("target")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        }),
        EventType::ToolCallProgress => Some(TuiEvent::ToolCallProgress {
            id: event.payload["id"].as_str()?.to_string(),
            name: event
                .payload
                .get("name")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            arguments_bytes: event.payload["arguments_bytes"].as_u64()? as usize,
        }),
        EventType::ToolOutputDelta => Some(TuiEvent::ToolOutputDelta {
            id: event.payload["id"].as_str()?.to_string(),
            chunk: event.payload["chunk"].as_str()?.to_string(),
        }),
        EventType::ToolCallCompleted => {
            let output = event
                .payload
                .get("output")
                .and_then(|value| value.as_str())
                .or_else(|| event.payload.get("error").and_then(|value| value.as_str()))
                .unwrap_or_default()
                .to_string();
            Some(TuiEvent::ToolCompleted {
                id: event.payload["id"].as_str()?.to_string(),
                name: event.payload["name"].as_str()?.to_string(),
                status: event.payload["status"].as_str()?.to_string(),
                output,
                diff: event
                    .payload
                    .get("diff")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                kind: event
                    .payload
                    .get("kind")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            })
        }
        EventType::PlanUpdated => Some(TuiEvent::PlanUpdated {
            explanation: serde_json::from_value(event.payload["explanation"].clone()).ok()?,
            plan: serde_json::from_value(event.payload["plan"].clone()).ok()?,
        }),
        // An actionable approval must carry the active operation fence. The
        // surface interaction handler emits that typed event directly.
        EventType::ApprovalRequested => None,
        EventType::ApprovalResolved => Some(TuiEvent::Notice(format!(
            "Approval {} resolved: {} ({})",
            event.payload["id"].as_str()?,
            event.payload["decision"].as_str()?,
            event.payload["reason"].as_str()?
        ))),
        EventType::SubagentStarted => Some(TuiEvent::SubagentStarted {
            id: event.payload["id"].as_str()?.to_string(),
            description: event.payload["description"].as_str()?.to_string(),
        }),
        EventType::SubagentCompleted => Some(TuiEvent::SubagentCompleted {
            id: event.payload["id"].as_str()?.to_string(),
            description: event.payload["description"].as_str()?.to_string(),
            status: match event.payload["status"].as_str()? {
                "success" => "completed",
                status => status,
            }
            .to_string(),
            output: event
                .payload
                .get("output")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            error: event
                .payload
                .get("error")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        }),
        EventType::WorkflowResultAvailable | EventType::WorkflowFailed => {
            let status = event.payload["status"]
                .as_str()
                .unwrap_or(if event.event_type == EventType::WorkflowFailed {
                    "failed"
                } else {
                    "completed"
                })
                .to_string();
            let summary = event
                .payload
                .get("result")
                .and_then(|value| value.as_str())
                .or_else(|| event.payload.get("error").and_then(|value| value.as_str()))
                .unwrap_or_default()
                .to_string();
            let notification = WorkflowTerminalNotification {
                task_id: event.payload["taskId"].as_str()?.to_string(),
                run_id: event.payload["runId"].as_str()?.to_string(),
                tool_use_id: event
                    .payload
                    .get("toolUseId")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                status: status.clone(),
                summary: summary.clone(),
            };
            let workflow_name = event
                .payload
                .get("workflowName")
                .and_then(|value| value.as_str())
                .unwrap_or("workflow");
            Some(TuiEvent::WorkflowNotification {
                id: notification.id(),
                prompt: notification.to_prompt(),
                status,
                summary: format!("{workflow_name}: {summary}"),
            })
        }
        EventType::WorkflowTasksUpdated => Some(TuiEvent::WorkflowTasksUpdated {
            tasks: serde_json::from_value(event.payload["tasks"].clone()).ok()?,
        }),
        EventType::TaskStatusUpdated => Some(TuiEvent::WorkflowTaskUpdated {
            task: serde_json::from_value(event.payload["task"].clone()).ok()?,
        }),
        EventType::WorkflowResumed => Some(TuiEvent::Notice(format!(
            "Workflow resumed: {}",
            workflow_name_from_payload(&event.payload)
        ))),
        EventType::WorkflowPhaseStarted => Some(TuiEvent::Notice(format!(
            "Workflow phase started: {}",
            event.payload["phase"].as_str()?
        ))),
        EventType::WorkflowPhaseCompleted => Some(TuiEvent::Notice(format!(
            "Workflow phase completed: {} ({}){}",
            event.payload["phase"].as_str()?,
            event.payload["status"].as_str().unwrap_or("completed"),
            optional_detail_suffix(&event.payload, "summary")
        ))),
        EventType::WorkflowAgentStarted => Some(TuiEvent::Notice(format!(
            "Workflow agent started: {} ({})",
            event.payload["agentId"].as_str()?,
            event.payload["phase"].as_str().unwrap_or("workflow")
        ))),
        EventType::WorkflowAgentCached => Some(TuiEvent::Notice(format!(
            "Workflow agent cached: {} ({}){}",
            event.payload["agentId"].as_str()?,
            event.payload["phase"].as_str().unwrap_or("workflow"),
            optional_detail_suffix(&event.payload, "output")
        ))),
        EventType::WorkflowAgentCompleted => Some(TuiEvent::Notice(format!(
            "Workflow agent completed: {} ({}){}",
            event.payload["agentId"].as_str()?,
            event.payload["phase"].as_str().unwrap_or("workflow"),
            optional_detail_suffix(&event.payload, "output")
        ))),
        EventType::WorkflowAgentFailed => Some(TuiEvent::Notice(format!(
            "Workflow agent failed: {} ({}): {}",
            event.payload["agentId"].as_str()?,
            event.payload["phase"].as_str().unwrap_or("workflow"),
            event.payload["error"].as_str().unwrap_or("failed")
        ))),
        EventType::WorkflowPaused => Some(TuiEvent::Notice(format!(
            "Workflow paused: {} ({})",
            workflow_name_from_payload(&event.payload),
            event.payload["reason"].as_str().unwrap_or("paused")
        ))),
        EventType::WorkflowStopped => Some(TuiEvent::Notice(format!(
            "Workflow stopped: {} ({})",
            workflow_name_from_payload(&event.payload),
            event.payload["reason"].as_str().unwrap_or("stopped")
        ))),
        EventType::VerificationStarted => Some(TuiEvent::Notice(format!(
            "Verification started: {}",
            event.payload["command"].as_str()?
        ))),
        EventType::VerificationCompleted => {
            let status = if event.payload["success"].as_bool()? {
                "passed"
            } else {
                "failed"
            };
            let exit = event
                .payload
                .get("exit_code")
                .and_then(|value| value.as_i64())
                .map(|code| format!(" (exit {code})"))
                .unwrap_or_default();
            Some(TuiEvent::Notice(format!(
                "Verification {status}: {}{exit}",
                event.payload["command"].as_str()?
            )))
        }
        EventType::Error => Some(TuiEvent::Error(
            event.payload["message"].as_str()?.to_string(),
        )),
        EventType::SessionCompleted => Some(TuiEvent::SessionCompleted {
            status: event.payload["status"].as_str()?.to_string(),
        }),
        _ => None,
    }
}

fn tui_task_lifecycle(value: &serde_json::Value) -> Option<TuiTaskLifecycle> {
    Some(TuiTaskLifecycle {
        id: value["task_id"].as_str()?.to_string(),
        kind: value["kind"].as_str()?.to_string(),
        status: value["status"].as_str()?.to_string(),
        turn: value["turn"].as_u64()?.try_into().ok()?,
    })
}

fn workflow_name_from_payload(payload: &serde_json::Value) -> String {
    payload
        .get("workflowName")
        .and_then(|value| value.as_str())
        .unwrap_or("workflow")
        .to_string()
}

fn optional_detail_suffix(payload: &serde_json::Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(|value| format!(": {value}"))
        .unwrap_or_default()
}

struct WorkflowTerminalNotification {
    task_id: String,
    run_id: String,
    tool_use_id: String,
    status: String,
    summary: String,
}

impl WorkflowTerminalNotification {
    fn id(&self) -> String {
        format!("{}:{}:{}", self.run_id, self.task_id, self.tool_use_id)
    }

    fn to_prompt(&self) -> String {
        format!(
            "<task-notification>\n<task-id>{}</task-id>\n<tool-use-id>{}</tool-use-id>\n<run-id>{}</run-id>\n<status>{}</status>\n<summary>{}</summary>\n</task-notification>\n\nA background workflow finished. Use this result to continue the current task.",
            xml_escape(&self.task_id),
            xml_escape(&self.tool_use_id),
            xml_escape(&self.run_id),
            xml_escape(&self.status),
            xml_escape(&self.summary)
        )
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use orca_core::event_schema::{EventDraft, EventFactory, RunStatus};
    use orca_core::event_sink::{EventObserver, observe_event};
    use orca_core::tool_types;

    fn materialize(draft: EventDraft) -> EventEnvelope {
        let event = Arc::new(Mutex::new(None));
        let observer = {
            let event = Arc::clone(&event);
            move |published: &EventEnvelope| {
                *event.lock().unwrap() = Some(published.clone());
                Ok(())
            }
        };
        observe_event(Some(&observer as &dyn EventObserver), draft).unwrap();
        drop(observer);
        Arc::try_unwrap(event)
            .unwrap()
            .into_inner()
            .unwrap()
            .expect("published event")
    }

    fn project(draft: EventDraft) -> Option<TuiEvent> {
        tui_event_from_runtime_event(&materialize(draft))
    }

    #[test]
    fn runtime_turn_started_event_maps_task_lifecycle_to_tui() {
        let mut events = EventFactory::new("tui-runtime-turn".to_string());
        let task = orca_runtime::lifecycle::RuntimeTaskLifecycle::new_snapshot(
            "main-session-1",
            orca_runtime::lifecycle::RuntimeTaskKind::Agent,
            orca_runtime::lifecycle::RuntimeTaskStatus::Running,
            3,
        );
        let event = task.attach_to_event(events.turn_started(3, Some("continue")));

        let projected = project(event).expect("turn started event");

        assert!(matches!(
            projected,
            TuiEvent::TurnStarted {
                turn: 3,
                task: Some(TuiTaskLifecycle {
                    id,
                    kind,
                    status,
                    turn: 3,
                }),
            } if id == "main-session-1" && kind == "agent" && status == "running"
        ));
    }

    #[test]
    fn runtime_tool_requested_event_maps_to_tui_tool_requested() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());
        let request = tool_types::ToolRequest {
            id: "tool-call-1".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "echo hi" }).to_string()),
        };

        let tui_event = project(events.tool_call_requested(&request)).expect("tui event");

        match tui_event {
            TuiEvent::ToolRequested { id, name, target } => {
                assert_eq!(id, "tool-call-1");
                assert_eq!(name, "bash");
                assert_eq!(target, Some("echo hi".to_string()));
            }
            other => panic!("expected tool requested event, got {other:?}"),
        }
    }

    #[test]
    fn runtime_failed_tool_completed_event_maps_error_to_tui_output() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());
        let request = tool_types::ToolRequest {
            id: "tool-call-2".to_string(),
            name: tool_types::ToolName::External("deploy_preview".to_string()),
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("preview".to_string()),
            raw_arguments: None,
        };
        let result = tool_types::ToolResult::failed(&request, "preview failed", Some(42));

        let tui_event = project(events.tool_call_completed(&result)).expect("tui event");

        match tui_event {
            TuiEvent::ToolCompleted {
                id,
                name,
                status,
                output,
                diff,
                kind,
            } => {
                assert_eq!(id, "tool-call-2");
                assert_eq!(name, "deploy_preview");
                assert_eq!(status, "failed");
                assert_eq!(output, "preview failed");
                assert_eq!(diff, None);
                assert_eq!(kind, Some("runtime_error".to_string()));
            }
            other => panic!("expected tool completed event, got {other:?}"),
        }
    }

    #[test]
    fn runtime_context_updated_event_maps_to_tui_context_budget() {
        let mut events = EventFactory::new("tui-context-budget".to_string());

        let event = project(events.context_updated(4_096, 96_000)).expect("context event");

        assert!(matches!(
            event,
            TuiEvent::ContextUpdated {
                used_tokens: 4_096,
                limit_tokens: 96_000
            }
        ));
    }

    #[test]
    fn runtime_tool_output_delta_maps_to_tui_live_output() {
        let mut events = EventFactory::new("tui-tool-output".to_string());

        let event = project(events.tool_output_delta("shell-call", "streamed output\n"))
            .expect("tool output event");

        assert!(matches!(
            event,
            TuiEvent::ToolOutputDelta { id, chunk }
                if id == "shell-call" && chunk == "streamed output\n"
        ));
    }

    #[test]
    fn runtime_tool_completed_event_maps_committed_diff() {
        let request = tool_types::ToolRequest {
            id: "tool-call-diff".to_string(),
            name: tool_types::ToolName::Edit,
            action: orca_core::approval_types::ActionKind::Write,
            target: Some("notes.txt".to_string()),
            raw_arguments: None,
        };
        let result =
            tool_types::ToolResult::completed(&request, "edited notes.txt".to_string(), false)
                .with_file_change_preview(tool_types::FileChangePreview::UnifiedDiff {
                    text: "--- a/notes.txt\n+++ b/notes.txt\n-old\n+new\n".to_string(),
                    truncated: false,
                });
        let mut events = EventFactory::new("tui-tool-diff".to_string());

        let event = project(events.tool_call_completed(&result)).expect("tool completion event");

        assert!(matches!(
            event,
            TuiEvent::ToolCompleted { diff: Some(diff), .. }
                if diff.contains("-old") && diff.contains("+new")
        ));
    }

    #[test]
    fn runtime_cancelled_tool_completed_event_keeps_cancelled_status() {
        let mut events = EventFactory::new("tui-runtime-cancelled".to_string());
        let request = tool_types::ToolRequest {
            id: "cancelled-bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("sleep 30".to_string()),
            raw_arguments: None,
        };
        let result = tool_types::ToolResult::cancelled(&request, "turn interrupted", Some(130));

        let tui_event = project(events.tool_call_completed(&result)).expect("tui event");

        assert!(matches!(
            tui_event,
            TuiEvent::ToolCompleted { status, kind, .. }
                if status == "cancelled" && kind.as_deref() == Some("cancelled")
        ));
    }

    #[test]
    fn runtime_assistant_delta_events_map_to_tui_streaming_events() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());

        let reasoning =
            project(events.assistant_reasoning_delta("thinking")).expect("reasoning event");
        let message = project(events.assistant_message_delta("hello")).expect("message event");

        assert!(matches!(reasoning, TuiEvent::ReasoningDelta(text) if text == "thinking"));
        assert!(matches!(message, TuiEvent::MessageDelta(text) if text == "hello"));
    }

    #[test]
    fn runtime_tool_call_progress_event_maps_to_tui_progress() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());
        let progress = orca_core::provider_types::ToolCallProgress {
            id: "call-1".to_string(),
            function_name: Some("write_file".to_string()),
            arguments_bytes: 12_345,
        };

        let event = project(events.tool_call_progress(&progress)).expect("tool progress event");

        match event {
            TuiEvent::ToolCallProgress {
                id,
                name,
                arguments_bytes,
            } => {
                assert_eq!(id, "call-1");
                assert_eq!(name.as_deref(), Some("write_file"));
                assert_eq!(arguments_bytes, 12_345);
            }
            other => panic!("expected tool progress event, got {other:?}"),
        }
    }

    #[test]
    fn runtime_usage_error_and_completion_events_map_to_tui_events() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());

        let usage = project(events.usage_updated(UsageTotals {
            input_tokens: 10,
            output_tokens: 5,
            cache_tokens: 2,
            estimated_cost_usd: 0.001,
        }))
        .expect("usage event");
        let error = project(events.error("boom")).expect("error event");
        let completed = project(events.session_completed(RunStatus::BudgetExhausted))
            .expect("completion event");

        match usage {
            TuiEvent::UsageUpdated(totals) => {
                assert_eq!(totals.input_tokens, 10);
                assert_eq!(totals.output_tokens, 5);
                assert_eq!(totals.cache_tokens, 2);
                assert_eq!(totals.estimated_cost_usd, 0.001);
            }
            other => panic!("expected usage event, got {other:?}"),
        }
        assert!(matches!(error, TuiEvent::Error(message) if message == "boom"));
        assert!(
            matches!(completed, TuiEvent::SessionCompleted { status } if status == "budget_exhausted")
        );
    }

    #[test]
    fn runtime_context_compacted_event_maps_to_tui_compacted() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());

        let compacted = project(events.context_compacted(
            "prompt_too_long_recovery",
            "remote_summary",
            12,
            5,
            7,
            "compacted context after prompt-too-long",
        ))
        .expect("compaction event");

        match compacted {
            TuiEvent::Compacted {
                before_messages,
                after_messages,
                reason,
                strategy,
                collapsed_messages,
                status_text,
            } => {
                assert_eq!(before_messages, 12);
                assert_eq!(after_messages, 5);
                assert_eq!(reason, "prompt_too_long_recovery");
                assert_eq!(strategy, "remote_summary");
                assert_eq!(collapsed_messages, 7);
                assert_eq!(status_text, "compacted context after prompt-too-long");
            }
            other => panic!("expected compacted event, got {other:?}"),
        }
    }

    #[test]
    fn legacy_context_compacted_event_keeps_tui_projection_compatible() {
        let event = EventEnvelope {
            version: orca_core::event_schema::EVENT_SCHEMA_VERSION.to_string(),
            run_id: "legacy-compaction".to_string(),
            seq: 1,
            timestamp_ms: 1,
            event_type: EventType::ContextCompacted,
            payload: serde_json::json!({
                "before_messages": 12,
                "after_messages": 5
            }),
        };

        let compacted = tui_event_from_runtime_event(&event).expect("legacy compaction event");

        assert!(matches!(
            compacted,
            TuiEvent::Compacted {
                before_messages: 12,
                after_messages: 5,
                ..
            }
        ));
    }

    #[test]
    fn runtime_context_compaction_started_event_maps_to_tui_compacting_status() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());

        let started = project(events.context_compaction_started("approaching_context_limit", 12))
            .expect("compaction started event");

        assert!(matches!(started, TuiEvent::CompactionStarted));
    }

    #[test]
    fn runtime_control_events_map_to_tui_notices() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());
        let route = orca_core::model::ModelRouteDecision {
            requested_model: Some("auto".to_string()),
            actual_model: "deepseek-v4-pro".to_string(),
            reason: orca_core::model::ModelRouteReason::DefaultPro,
        };
        let approval = orca_core::approval_types::ApprovalResolution {
            id: "approval-1".to_string(),
            decision: orca_core::approval_types::ApprovalDecision::Allow,
            reason: "user approved".to_string(),
        };
        let verifier = orca_core::verification::VerificationResult {
            command: "cargo test".to_string(),
            success: false,
            exit_code: Some(101),
            stdout: String::new(),
            stderr: "failed".to_string(),
        };

        let routed = project(events.model_routed(&route)).expect("model routed");
        let resolved = project(events.approval_resolved(&approval)).expect("approval resolved");
        let verification_started =
            project(events.verification_started("cargo test")).expect("verification started");
        let verification_completed =
            project(events.verification_completed(&verifier)).expect("verification completed");

        assert!(matches!(routed, TuiEvent::Notice(message)
                if message == "Model routed to deepseek-v4-pro (default_pro)"));
        assert!(matches!(resolved, TuiEvent::Notice(message)
                if message == "Approval approval-1 resolved: allow (user approved)"));
        assert!(matches!(verification_started, TuiEvent::Notice(message)
                if message == "Verification started: cargo test"));
        assert!(matches!(verification_completed, TuiEvent::Notice(message)
                if message == "Verification failed: cargo test (exit 101)"));
    }

    #[test]
    fn runtime_plan_approval_and_subagent_events_map_to_tui_events() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());
        let plan_update = orca_core::plan_types::UpdatePlanArgs {
            explanation: Some("next steps".to_string()),
            plan: vec![orca_core::plan_types::PlanItem {
                step: "wire adapter".to_string(),
                status: orca_core::plan_types::PlanStatus::InProgress,
            }],
        };
        let approval = orca_core::approval_types::ApprovalRequest {
            id: "approval-1".to_string(),
            action: orca_core::approval_types::ActionKind::Shell,
            description: "run cargo test".to_string(),
            tool: Some("bash".to_string()),
            target: Some("cargo test".to_string()),
            preview: Some("$ cargo test".to_string()),
        };

        let plan = project(events.plan_updated(&plan_update)).expect("plan event");
        let approval = project(events.approval_requested(&approval));
        let subagent_started =
            project(events.subagent_started("agent-1", "review code")).expect("subagent started");
        let subagent_completed = project(events.subagent_completed(
            "agent-1",
            "review code",
            RunStatus::Success,
            Some("looks good"),
            None,
        ))
        .expect("subagent completed");

        match plan {
            TuiEvent::PlanUpdated { explanation, plan } => {
                assert_eq!(explanation, Some("next steps".to_string()));
                assert_eq!(plan.len(), 1);
                assert_eq!(plan[0].step, "wire adapter");
                assert_eq!(
                    plan[0].status,
                    orca_core::plan_types::PlanStatus::InProgress
                );
            }
            other => panic!("expected plan event, got {other:?}"),
        }
        assert!(
            approval.is_none(),
            "unfenced approval events are not actionable"
        );
        assert!(
            matches!(subagent_started, TuiEvent::SubagentStarted { id, description }
                if id == "agent-1" && description == "review code")
        );
        assert!(
            matches!(subagent_completed, TuiEvent::SubagentCompleted { id, description, status, output, error }
                if id == "agent-1"
                    && description == "review code"
                    && status == "completed"
                    && output == Some("looks good".to_string())
                    && error.is_none())
        );
    }

    #[test]
    fn runtime_workflow_result_event_maps_to_tui_notification() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());

        let notification = project(events.workflow_result_available(
            "task-1",
            "workflow-run-1",
            "mock-workflow",
            Some("workflow-tool-1"),
            "completed",
            "all phases passed",
        ))
        .expect("workflow notification");

        match notification {
            TuiEvent::WorkflowNotification {
                id,
                prompt,
                status,
                summary,
            } => {
                assert_eq!(id, "workflow-run-1:task-1:workflow-tool-1");
                assert_eq!(status, "completed");
                assert_eq!(summary, "mock-workflow: all phases passed");
                assert!(prompt.contains("<task-id>task-1</task-id>"));
                assert!(prompt.contains("<tool-use-id>workflow-tool-1</tool-use-id>"));
                assert!(prompt.contains("<run-id>workflow-run-1</run-id>"));
                assert!(prompt.contains("<status>completed</status>"));
                assert!(prompt.contains("<summary>all phases passed</summary>"));
            }
            other => panic!("expected workflow notification, got {other:?}"),
        }
    }

    #[test]
    fn runtime_workflow_lifecycle_events_map_to_tui_notices() {
        let mut events = EventFactory::new("run-1".to_string());

        let phase_started =
            project(events.workflow_phase_started("task-1", "workflow-run-1", "scan"))
                .expect("phase started");
        let agent_failed = project(events.workflow_agent_failed(
            "task-1",
            "workflow-run-1",
            "scan",
            "agent-1",
            "boom",
        ))
        .expect("agent failed");
        let paused =
            project(events.workflow_paused("task-1", "workflow-run-1", "audit", "manual pause"))
                .expect("paused");

        assert!(matches!(phase_started, TuiEvent::Notice(message)
                if message == "Workflow phase started: scan"));
        assert!(matches!(agent_failed, TuiEvent::Notice(message)
                if message == "Workflow agent failed: agent-1 (scan): boom"));
        assert!(matches!(paused, TuiEvent::Notice(message)
                if message == "Workflow paused: audit (manual pause)"));
    }

    #[test]
    fn runtime_workflow_tasks_event_maps_to_tui_tasks_updated() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());
        let task = orca_core::task_types::BackgroundTaskSummary {
            id: "task-1".to_string(),
            task_type: orca_core::task_types::TaskType::Workflow,
            status: orca_core::task_types::TaskStatus::Running,
            is_backgrounded: false,
            description: "demo workflow".to_string(),
            created_at_ms: 10,
            started_at_ms: Some(20),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: Some("workflow".to_string()),
            pending_tool_call: None,
            name: Some("demo".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(2),
            workflow_progress: Some(orca_core::task_types::WorkflowTaskProgress {
                total_agents: 3,
                running_agents: 1,
                completed_agents: 2,
                failed_agents: 0,
                completed_phases: 1,
                running_phases: 1,
                failed_phases: 0,
            }),
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: Some("workflow.md".to_string()),
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            result: None,
            error: None,
        };

        let tui_event =
            project(events.workflow_tasks_updated(&[task])).expect("workflow tasks updated event");

        match tui_event {
            TuiEvent::WorkflowTasksUpdated { tasks } => {
                assert_eq!(tasks.len(), 1);
                assert_eq!(tasks[0].id, "task-1");
                assert_eq!(tasks[0].workflow_run_id, Some("workflow-run-1".to_string()));
                assert_eq!(
                    tasks[0]
                        .workflow_progress
                        .as_ref()
                        .map(|progress| progress.completed_agents),
                    Some(2)
                );
            }
            other => panic!("expected workflow tasks updated event, got {other:?}"),
        }
    }

    #[test]
    fn runtime_task_status_event_maps_to_tui_task_updated() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());
        let task = orca_core::task_types::BackgroundTaskSummary {
            id: "main-session-1".to_string(),
            task_type: orca_core::task_types::TaskType::MainSession,
            status: orca_core::task_types::TaskStatus::ApprovalRequired,
            is_backgrounded: true,
            description: "background turn".to_string(),
            created_at_ms: 10,
            started_at_ms: Some(20),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: Some("shell".to_string()),
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(30),
            result: None,
            error: None,
        };

        let tui_event =
            project(events.task_status_updated(&task)).expect("task status updated event");

        match tui_event {
            TuiEvent::WorkflowTaskUpdated { task } => {
                assert_eq!(task.id, "main-session-1");
                assert_eq!(
                    task.status,
                    orca_core::task_types::TaskStatus::ApprovalRequired
                );
                assert!(task.is_backgrounded);
                assert_eq!(task.tool.as_deref(), Some("shell"));
            }
            other => panic!("expected task status updated event, got {other:?}"),
        }
    }
}
