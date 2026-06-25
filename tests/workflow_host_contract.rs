use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

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
fn host_phase_marker_applies_to_following_agents_until_changed() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'marker-test', description: 'Marker phase test', phases: ['scan', 'review'] };\nphase('scan');\nawait agent('inspect repo');\nphase('review');\nawait agent('review findings');\nexport default 'done';",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, phase, .. }
                if prompt == "inspect repo" && phase.as_deref() == Some("scan")
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, phase, .. }
                if prompt == "review findings" && phase.as_deref() == Some("review")
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseCompleted { name } if name == "scan"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseCompleted { name } if name == "review"))
    );
}

#[test]
fn host_parallel_routes_out_of_order_agent_results_by_call_id() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'parallel-host-test', description: 'Parallel host test', phases: [] };\nconst results = await parallel([agent('slow'), agent('fast')]);\nexport default results.map(item => item.prompt).join(',');",
    )
    .unwrap();

    let events =
        WorkflowHost::run_collecting_events_with_agent(&script, serde_json::json!(null), |call| {
            if call.prompt == "slow" {
                thread::sleep(Duration::from_millis(150));
            }
            Ok(orca_runtime::workflow::host::HostCommand::AgentResult {
                call_id: call.call_id.clone(),
                result: serde_json::json!({
                    "prompt": call.prompt,
                }),
            })
        })
        .unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::WorkflowCompleted { result }
                if result.as_str() == Some("slow,fast")
        )
    }));
}

#[test]
fn host_phase_fallback_continue_emits_failed_phase_and_continues() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'fallback-host-test', description: 'Fallback host test', phases: ['scan', 'review'] };\n\
         const scan = await phase('scan', async () => agent('fail scan'), { fallback: 'continue' });\n\
         const review = await phase('review', async () => agent('review anyway'));\n\
         export default { scan, review };",
    )
    .unwrap();

    let events =
        WorkflowHost::run_collecting_events_with_agent(&script, serde_json::json!(null), |call| {
            if call.prompt == "fail scan" {
                Ok(orca_runtime::workflow::host::HostCommand::AgentError {
                    call_id: call.call_id.clone(),
                    error: "scan failed".to_string(),
                })
            } else {
                Ok(orca_runtime::workflow::host::HostCommand::AgentResult {
                    call_id: call.call_id.clone(),
                    result: serde_json::json!({ "prompt": call.prompt }),
                })
            }
        })
        .unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::PhaseFailed { name, error, .. }
                if name == "scan" && error.contains("scan failed")
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, phase, .. }
                if prompt == "review anyway" && phase.as_deref() == Some("review")
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowCompleted { .. }))
    );
}

#[test]
fn host_phase_fallback_value_returns_value_to_following_phase() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'fallback-value-test', description: 'Fallback value test', phases: ['scan', 'review'] };\n\
         const scan = await phase('scan', async () => agent('fail scan'), { fallback: { value: { recovered: true, source: 'fallback' } } });\n\
         const review = await phase('review', async () => agent(`review recovered=${scan.recovered}`));\n\
         export default { scan, review };",
    )
    .unwrap();

    let events =
        WorkflowHost::run_collecting_events_with_agent(&script, serde_json::json!(null), |call| {
            if call.prompt == "fail scan" {
                Ok(orca_runtime::workflow::host::HostCommand::AgentError {
                    call_id: call.call_id.clone(),
                    error: "scan failed".to_string(),
                })
            } else {
                Ok(orca_runtime::workflow::host::HostCommand::AgentResult {
                    call_id: call.call_id.clone(),
                    result: serde_json::json!({ "prompt": call.prompt }),
                })
            }
        })
        .unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::PhaseFailed { name, error, fallback }
                if name == "scan" && error.contains("scan failed") && fallback.as_deref() == Some("value")
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, phase, .. }
                if prompt == "review recovered=true" && phase.as_deref() == Some("review")
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::WorkflowCompleted { result }
                if result["scan"]["recovered"] == true && result["scan"]["source"] == "fallback"
        )
    }));
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
fn host_ignores_export_mentions_in_comments_and_strings_when_loading_workflow_module() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "/* export const meta = { fake: true }; */\nconst prompt = 'Prompt mentioning export default before the real workflow body';\nexport const meta = { name: 'rewrite-guard-test', description: 'Syntax-aware export rewrite test', phases: ['scan'] };\nconst result = await phase('scan', async () => agent(prompt, { description: 'scan repo' }));\nexport default result;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseStarted { name } if name == "scan"))
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, .. }
                if prompt == "Prompt mentioning export default before the real workflow body"
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
fn host_executes_top_level_phase_task_definitions() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'dsl-test', description: 'DSL test' };\nexport const phases = [{ name: 'scan', tasks: [{ type: 'agent', description: 'scan repo', prompt: 'inspect repo', model: 'deepseek-v4-flash' }] }, { name: 'review', tasks: [{ type: 'agent', description: 'review scan', prompt: 'review previous output' }] }];",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseStarted { name } if name == "scan"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseStarted { name } if name == "review"))
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, opts, .. }
                if prompt == "inspect repo" && opts["description"] == "scan repo"
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, .. }
                if prompt.contains("[Previous phase outputs]") && prompt.contains("review previous output")
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowCompleted { .. }))
    );
}

#[test]
fn host_executes_meta_phase_task_definitions() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'dsl-test', description: 'DSL test', phases: [{ name: 'scan', tasks: [{ description: 'scan repo', prompt: 'inspect repo' }] }] };",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseStarted { name } if name == "scan"))
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, opts, .. }
                if prompt == "inspect repo" && opts["description"] == "scan repo"
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowCompleted { .. }))
    );
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

#[test]
fn host_reports_workflow_failed_when_stdin_closes_before_agent_result() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'stdin-eof-test', description: 'stdin eof', phases: [] };\nawait agent('inspect repo');\nexport default 'done';",
    )
    .unwrap();

    let host = temp.path().join("host.mjs");
    fs::write(
        &host,
        include_str!("../crates/orca-runtime/src/workflow/host.mjs"),
    )
    .unwrap();

    let mut child = Command::new("node")
        .arg(&host)
        .arg(&script)
        .arg("null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).unwrap();
    assert!(first_line.contains("\"type\":\"agent_call\""));

    let stdin = child.stdin.take().unwrap();
    drop(stdin);

    let mut remaining = Vec::new();
    for line in reader.lines() {
        remaining.push(line.unwrap());
    }

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "expected host to exit with workflow failure, status={:?}, stderr={stderr}",
        output.status.code()
    );
    assert!(
        remaining
            .iter()
            .any(|line| line.contains("\"type\":\"workflow_failed\""))
    );
}

#[test]
fn host_reports_workflow_failed_for_partial_trailing_json_on_stdin_eof() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'stdin-partial-json-test', description: 'stdin partial json', phases: [] };\nawait agent('inspect repo');\nexport default 'done';",
    )
    .unwrap();

    let host = temp.path().join("host.mjs");
    fs::write(
        &host,
        include_str!("../crates/orca-runtime/src/workflow/host.mjs"),
    )
    .unwrap();

    let mut child = Command::new("node")
        .arg(&host)
        .arg(&script)
        .arg("null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).unwrap();
    assert!(first_line.contains("\"type\":\"agent_call\""));

    let mut stdin = child.stdin.take().unwrap();
    use std::io::Write;
    stdin.write_all(br#"{"type":"agent_result""#).unwrap();
    drop(stdin);

    let mut remaining = Vec::new();
    for line in reader.lines() {
        remaining.push(line.unwrap());
    }

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "expected host to exit with workflow failure, status={:?}, stderr={stderr}",
        output.status.code()
    );
    assert!(remaining.iter().any(|line| {
        line.contains("\"type\":\"workflow_failed\"") && line.contains("partial JSON")
    }));
}
