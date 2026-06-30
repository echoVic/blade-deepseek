use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventEnvelope, EventType};

use crate::types::TuiEvent;

pub(crate) fn tui_event_from_runtime_event(event: &EventEnvelope) -> Option<TuiEvent> {
    match event.event_type {
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
                diff: None,
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
        EventType::ApprovalRequested => Some(TuiEvent::ApprovalNeeded {
            id: event.payload["id"].as_str()?.to_string(),
            tool: event
                .payload
                .get("tool")
                .and_then(|value| value.as_str())
                .or_else(|| event.payload["action"].as_str())?
                .to_string(),
            target: event
                .payload
                .get("target")
                .and_then(|value| value.as_str())
                .or_else(|| {
                    event
                        .payload
                        .get("description")
                        .and_then(|value| value.as_str())
                })
                .map(str::to_string),
            preview: event
                .payload
                .get("preview")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        }),
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
                prompt: notification.to_prompt(),
                status,
                summary: format!("{workflow_name}: {summary}"),
            })
        }
        EventType::WorkflowTasksUpdated => Some(TuiEvent::WorkflowTasksUpdated {
            tasks: serde_json::from_value(event.payload["tasks"].clone()).ok()?,
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
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::tool_types;

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

        let tui_event =
            tui_event_from_runtime_event(&events.tool_call_requested(&request)).expect("tui event");

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

        let tui_event =
            tui_event_from_runtime_event(&events.tool_call_completed(&result)).expect("tui event");

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
    fn runtime_assistant_delta_events_map_to_tui_streaming_events() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());

        let reasoning = tui_event_from_runtime_event(&events.assistant_reasoning_delta("thinking"))
            .expect("reasoning event");
        let message = tui_event_from_runtime_event(&events.assistant_message_delta("hello"))
            .expect("message event");

        assert!(matches!(reasoning, TuiEvent::ReasoningDelta(text) if text == "thinking"));
        assert!(matches!(message, TuiEvent::MessageDelta(text) if text == "hello"));
    }

    #[test]
    fn runtime_usage_error_and_completion_events_map_to_tui_events() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());

        let usage = tui_event_from_runtime_event(&events.usage_updated(UsageTotals {
            input_tokens: 10,
            output_tokens: 5,
            cache_tokens: 2,
            estimated_cost_usd: 0.001,
        }))
        .expect("usage event");
        let error = tui_event_from_runtime_event(&events.error("boom")).expect("error event");
        let completed =
            tui_event_from_runtime_event(&events.session_completed(RunStatus::BudgetExhausted))
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

        let routed =
            tui_event_from_runtime_event(&events.model_routed(&route)).expect("model routed");
        let resolved = tui_event_from_runtime_event(&events.approval_resolved(&approval))
            .expect("approval resolved");
        let verification_started =
            tui_event_from_runtime_event(&events.verification_started("cargo test"))
                .expect("verification started");
        let verification_completed =
            tui_event_from_runtime_event(&events.verification_completed(&verifier))
                .expect("verification completed");

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

        let plan =
            tui_event_from_runtime_event(&events.plan_updated(&plan_update)).expect("plan event");
        let approval =
            tui_event_from_runtime_event(&events.approval_requested(&approval)).expect("approval");
        let subagent_started =
            tui_event_from_runtime_event(&events.subagent_started("agent-1", "review code"))
                .expect("subagent started");
        let subagent_completed = tui_event_from_runtime_event(&events.subagent_completed(
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
            matches!(approval, TuiEvent::ApprovalNeeded { id, tool, target, preview }
                if id == "approval-1"
                    && tool == "bash"
                    && target == Some("cargo test".to_string())
                    && preview == Some("$ cargo test".to_string()))
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

        let notification = tui_event_from_runtime_event(&events.workflow_result_available(
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
                prompt,
                status,
                summary,
            } => {
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

        let phase_started = tui_event_from_runtime_event(&events.workflow_phase_started(
            "task-1",
            "workflow-run-1",
            "scan",
        ))
        .expect("phase started");
        let agent_failed = tui_event_from_runtime_event(&events.workflow_agent_failed(
            "task-1",
            "workflow-run-1",
            "scan",
            "agent-1",
            "boom",
        ))
        .expect("agent failed");
        let paused = tui_event_from_runtime_event(&events.workflow_paused(
            "task-1",
            "workflow-run-1",
            "audit",
            "manual pause",
        ))
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
            description: "demo workflow".to_string(),
            created_at_ms: 10,
            started_at_ms: Some(20),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: Some("workflow".to_string()),
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
        };

        let tui_event = tui_event_from_runtime_event(&events.workflow_tasks_updated(&[task]))
            .expect("workflow tasks updated event");

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
}
