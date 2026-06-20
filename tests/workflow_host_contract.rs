use std::fs;

use orca_runtime::workflow::host::{HostEvent, WorkflowHost};
use tempfile::tempdir;

#[test]
fn host_emits_phase_and_agent_call_events() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'host-test', description: 'Host test', phases: ['scan'] };\nconst result = await phase('scan', async () => agent('inspect repo', { description: 'scan repo' }));\nexport default result;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!({"x": 1})).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseStarted { name } if name == "scan"))
    );
    assert!(events.iter().any(
        |event| matches!(event, HostEvent::AgentCall { prompt, .. } if prompt == "inspect repo")
    ));
}

#[test]
fn host_exposes_args_global() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'args-test', description: 'Args test', phases: [] };\nawait agent(args.prompt);\nexport default 'done';",
    )
    .unwrap();

    let events =
        WorkflowHost::run_collecting_events(&script, serde_json::json!({"prompt": "from args"}))
            .unwrap();
    assert!(events.iter().any(
        |event| matches!(event, HostEvent::AgentCall { prompt, .. } if prompt == "from args")
    ));
}

#[test]
fn host_hides_node_globals_from_workflow_scripts() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'restricted-test', description: 'Restricted globals test', phases: [] };\nif (typeof process === 'undefined') {\n  await agent('restricted');\n} else {\n  await agent('process visible');\n}\nexport default null;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::AgentCall { prompt, .. } if prompt == "restricted"))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HostEvent::AgentCall { prompt, .. } if prompt == "process visible"))
    );
}

#[test]
fn host_returns_workflow_failed_event_for_script_exceptions() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'failure-test', description: 'Failure propagation test', phases: [] };\nthrow new Error('boom from script');",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::WorkflowFailed { error } if error.contains("boom from script")
        )
    }));
}
