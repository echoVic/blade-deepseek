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
fn host_allows_blocked_words_in_comments_and_prompt_strings() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'string-comment-test', description: 'String and comment handling test', phases: [] };\n// Mentioning child_process here should stay harmless.\nawait agent('inspect process usage and globalThis references');\nexport default 'done';",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, .. }
                if prompt == "inspect process usage and globalThis references"
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowCompleted { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowFailed { .. }))
    );
}

#[test]
fn host_blocks_constructor_process_escape_attempts() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'constructor-escape-test', description: 'Constructor process escape test', phases: [] };\nconst escaped = globalThis.constructor.constructor('return process')();\nawait agent(`escaped ${escaped.version}`);\nexport default null;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowFailed { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HostEvent::AgentCall { prompt, .. } if prompt.starts_with("escaped ")))
    );
    assert!(!events.iter().any(|event| {
        matches!(event, HostEvent::AgentCall { prompt, .. } if prompt == "escaped process")
    }));
}

#[test]
fn host_blocks_bracket_constructor_process_escape_attempts() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'bracket-constructor-escape-test', description: 'Bracket constructor process escape test', phases: [] };\nconst escaped = ({})['constructor']['constructor']('return process')();\nawait agent(`escaped ${escaped.version}`);\nexport default null;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(events.iter().any(|event| matches!(
        event,
        HostEvent::WorkflowFailed { error }
            if error.contains("prohibited computed property: constructor")
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HostEvent::AgentCall { prompt, .. } if prompt.starts_with("escaped ")))
    );
}

#[test]
fn host_blocks_constructor_builtin_module_escape_attempts() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'builtin-escape-test', description: 'Built-in module escape test', phases: [] };\nconst processRef = globalThis.constructor.constructor('return process')();\nconst fsRef = processRef.getBuiltinModule('node:fs');\nawait agent(`escaped fs ${typeof fsRef.readFileSync}`);\nexport default null;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowFailed { .. }))
    );
    assert!(!events.iter().any(|event| {
        matches!(event, HostEvent::AgentCall { prompt, .. } if prompt.starts_with("escaped fs "))
    }));
}

#[test]
fn host_blocks_bracket_builtin_module_escape_attempts() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'bracket-builtin-escape-test', description: 'Bracket built-in module escape test', phases: [] };\nconst processRef = { ['getBuiltinModule']: () => ({ readFileSync() {} }) };\nconst fsRef = processRef['getBuiltinModule']('node:fs');\nawait agent(`escaped fs ${typeof fsRef.readFileSync}`);\nexport default null;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(events.iter().any(|event| matches!(
        event,
        HostEvent::WorkflowFailed { error }
            if error.contains("prohibited computed property: getBuiltinModule")
    )));
    assert!(!events.iter().any(|event| {
        matches!(event, HostEvent::AgentCall { prompt, .. } if prompt.starts_with("escaped fs "))
    }));
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
