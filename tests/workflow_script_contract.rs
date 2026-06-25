use std::fs;

use orca_core::workflow_types::{
    WorkflowAgentStatus, WorkflowInput, WorkflowMeta, WorkflowRunState, WorkflowRunStatus,
};
use orca_runtime::workflow::script::{
    resolve_workflow_script, resolve_workflow_script_to_path, resolve_workflow_script_with_user_dir,
};
use orca_runtime::workflow::state::{
    WorkflowAgentCacheRecord, WorkflowAgentRecord, WorkflowStateStore,
};
use serde_json::json;
use tempfile::tempdir;

#[test]
fn inline_script_is_persisted_and_meta_is_extracted() {
    let temp = tempdir().unwrap();
    let session_dir = temp.path().join("session");
    let input = WorkflowInput {
        script: Some("export const meta = { name: 'audit', description: 'Audit code', phases: ['scan', 'review'] };\nexport default await agent('inspect repo');".to_string()),
        ..Default::default()
    };

    let resolved = resolve_workflow_script(&input, temp.path(), &session_dir).unwrap();

    assert_eq!(resolved.meta.name, "audit");
    assert_eq!(resolved.meta.description, "Audit code");
    assert_eq!(resolved.meta.phases, vec!["scan", "review"]);
    assert!(resolved.persisted_path.exists());
    assert!(
        fs::read_to_string(&resolved.persisted_path)
            .unwrap()
            .contains("export const meta")
    );
    assert_eq!(resolved.script_digest.len(), 64);
}

#[test]
fn inline_script_accepts_top_level_phase_objects() {
    let temp = tempdir().unwrap();
    let session_dir = temp.path().join("session");
    let input = WorkflowInput {
        script: Some(
            "export const meta = { name: 'audit', description: 'Audit code' };\nexport const phases = [{ name: 'scan', tasks: [{ type: 'agent', prompt: 'inspect repo' }] }, { name: 'review', tasks: [] }];".to_string(),
        ),
        ..Default::default()
    };

    let resolved = resolve_workflow_script(&input, temp.path(), &session_dir).unwrap();

    assert_eq!(resolved.meta.name, "audit");
    assert_eq!(resolved.meta.description, "Audit code");
    assert_eq!(resolved.meta.phases, vec!["scan", "review"]);
}

#[test]
fn workflow_scripts_can_be_persisted_per_run_with_same_meta_name() {
    let temp = tempdir().unwrap();
    let input = WorkflowInput {
        script: Some("export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default 'ok';".to_string()),
        ..Default::default()
    };
    let first_path = temp.path().join("session/workflow-runs/run-1/script.js");
    let second_path = temp.path().join("session/workflow-runs/run-2/script.js");

    let first = resolve_workflow_script_to_path(&input, temp.path(), &first_path).unwrap();
    let second = resolve_workflow_script_to_path(&input, temp.path(), &second_path).unwrap();

    assert_eq!(first.persisted_path, first_path);
    assert_eq!(second.persisted_path, second_path);
    assert_ne!(first.persisted_path, second.persisted_path);
    assert!(first.persisted_path.exists());
    assert!(second.persisted_path.exists());
}

#[test]
fn script_path_takes_precedence_over_inline_script() {
    let temp = tempdir().unwrap();
    let session_dir = temp.path().join("session");
    let source = temp.path().join("chosen.js");
    fs::write(
        &source,
        "export const meta = { name: 'chosen', description: 'Chosen script', phases: [] };\nexport default 'ok';",
    )
    .unwrap();

    let input = WorkflowInput {
        script: Some(
            "export const meta = { name: 'ignored', description: 'Ignored', phases: [] };"
                .to_string(),
        ),
        script_path: Some(source.display().to_string()),
        ..Default::default()
    };

    let resolved = resolve_workflow_script(&input, temp.path(), &session_dir).unwrap();
    assert_eq!(resolved.meta.name, "chosen");
    assert_eq!(resolved.original_path.as_deref(), Some(source.as_path()));
}

#[test]
fn nearest_project_workflow_wins_over_user_workflow() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cwd = repo_root.join("packages/api");
    fs::create_dir_all(repo_root.join(".orca/workflows")).unwrap();
    fs::create_dir_all(temp.path().join("home/.orca/workflows")).unwrap();

    fs::write(
        repo_root.join(".orca/workflows/audit.js"),
        "export const meta = { name: 'audit', description: 'Project audit', phases: [] };\nexport default 'project';",
    )
    .unwrap();
    fs::write(
        temp.path().join("home/.orca/workflows/audit.js"),
        "export const meta = { name: 'audit', description: 'User audit', phases: [] };\nexport default 'user';",
    )
    .unwrap();

    let input = WorkflowInput {
        name: Some("audit".to_string()),
        ..Default::default()
    };

    let resolved = resolve_workflow_script_with_user_dir(
        &input,
        &cwd,
        &temp.path().join("session"),
        &temp.path().join("home/.orca/workflows"),
    )
    .unwrap();

    assert_eq!(resolved.meta.description, "Project audit");
}

#[test]
fn missing_meta_fields_return_invalid_data_error() {
    let temp = tempdir().unwrap();
    let session_dir = temp.path().join("session");
    let input = WorkflowInput {
        script: Some(
            "export const meta = { name: 'audit', description: 'Audit code' };\nexport default 'x';"
                .to_string(),
        ),
        ..Default::default()
    };

    let error = resolve_workflow_script(&input, temp.path(), &session_dir).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn state_store_round_trips_run_state_and_agent_cache() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-1".to_string(),
        task_id: "task-1".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Queued,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: None,
        error: None,
    };

    store.create_run(&state).unwrap();
    assert!(store.transcript_dir(&state.run_id).exists());

    let loaded = store.load_run(&state.run_id).unwrap();
    assert_eq!(loaded.workflow_name, "audit");

    let updated_state = WorkflowRunState {
        status: WorkflowRunStatus::Completed,
        final_summary: Some("done".to_string()),
        ..loaded
    };
    store.write_state(&updated_state).unwrap();

    let record = WorkflowAgentCacheRecord {
        call_path: "phases.scan".to_string(),
        input_hash: "1234".to_string(),
        output: json!({
            "status": WorkflowAgentStatus::Completed,
            "summary": "cached"
        }),
    };
    store
        .record_agent_completed(&updated_state.run_id, record.clone())
        .unwrap();

    let cached = store
        .cached_agent_result(&updated_state.run_id, "phases.scan", "1234")
        .unwrap();
    assert_eq!(cached, Some(record));
}

#[test]
fn state_store_reads_legacy_agent_cache_shape() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-legacy".to_string(),
        task_id: "task-legacy".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let legacy_record = serde_json::json!({
        "phases.scan:1234": {
            "call_path": "phases.scan",
            "input_hash": "1234",
            "output": {
                "status": "completed",
                "summary": "cached"
            }
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&legacy_record).unwrap(),
    )
    .unwrap();

    let cached = store
        .cached_agent_result(&state.run_id, "phases.scan", "1234")
        .unwrap();
    assert_eq!(
        cached,
        Some(WorkflowAgentCacheRecord {
            call_path: "phases.scan".to_string(),
            input_hash: "1234".to_string(),
            output: json!({
                "status": "completed",
                "summary": "cached"
            }),
        })
    );

    let found = store.find_cached_agent_value(&state.run_id, "phases.scan", "1234");
    assert_eq!(
        found,
        Some(json!({
            "status": "completed",
            "summary": "cached"
        }))
    );
}

#[test]
fn state_store_reads_legacy_string_agent_cache_without_json_quoting() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-legacy-string".to_string(),
        task_id: "task-legacy-string".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let legacy_record = serde_json::json!({
        "phases.scan:1234": {
            "call_path": "phases.scan",
            "input_hash": "1234",
            "output": "legacy result"
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&legacy_record).unwrap(),
    )
    .unwrap();

    let found = store.find_cached_agent_value(&state.run_id, "phases.scan", "1234");
    assert_eq!(found, Some(json!("legacy result")));
}

#[test]
fn state_store_preserves_legacy_json_looking_string_cache_values() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-legacy-json-string".to_string(),
        task_id: "task-legacy-json-string".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    for (input_hash, output) in [
        ("1234", "123"),
        ("false", "false"),
        ("null", "null"),
        ("array", "[1]"),
        ("object", "{\"k\":1}"),
    ] {
        let legacy_record = serde_json::json!({
            format!("phases.scan:{input_hash}"): {
                "call_path": "phases.scan",
                "input_hash": input_hash,
                "output": output
            }
        });
        fs::write(
            store.run_dir(&state.run_id).join("agent-cache.json"),
            serde_json::to_string_pretty(&legacy_record).unwrap(),
        )
        .unwrap();

        let found = store.find_cached_agent_value(&state.run_id, "phases.scan", input_hash);
        assert_eq!(found, Some(json!(output)), "case {output}");
    }
}

#[test]
fn state_store_reads_legacy_null_agent_cache_as_null_value() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-legacy-null".to_string(),
        task_id: "task-legacy-null".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let legacy_record = serde_json::json!({
        "phases.scan:1234": {
            "call_path": "phases.scan",
            "input_hash": "1234",
            "output": null
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&legacy_record).unwrap(),
    )
    .unwrap();

    let found = store.find_cached_agent_value(&state.run_id, "phases.scan", "1234");
    assert_eq!(found, Some(serde_json::Value::Null));
}

#[test]
fn state_store_returns_legacy_object_cache_as_object_value() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-legacy-object-value".to_string(),
        task_id: "task-legacy-object-value".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let legacy_record = serde_json::json!({
        "phases.scan:1234": {
            "call_path": "phases.scan",
            "input_hash": "1234",
            "output": {
                "kind": "legacy-object"
            }
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&legacy_record).unwrap(),
    )
    .unwrap();

    let found = store.find_cached_agent_value(&state.run_id, "phases.scan", "1234");
    assert_eq!(found, Some(json!({ "kind": "legacy-object" })));
}

#[test]
fn state_store_reads_current_null_output_as_null_value() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-current-null".to_string(),
        task_id: "task-current-null".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let current_record = serde_json::json!({
        "phases.scan:1234": {
            "callId": "call-1",
            "callPath": "phases.scan",
            "prompt": "inspect repo",
            "opts": null,
            "inputHash": "1234",
            "status": "completed",
            "output": null,
            "error": null,
            "transcriptPath": null
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&current_record).unwrap(),
    )
    .unwrap();

    let found = store.find_cached_agent_value(&state.run_id, "phases.scan", "1234");
    assert_eq!(found, Some(serde_json::Value::Null));

    let cached = store
        .cached_agent_result(&state.run_id, "phases.scan", "1234")
        .unwrap();
    assert_eq!(
        cached,
        Some(WorkflowAgentCacheRecord {
            call_path: "phases.scan".to_string(),
            input_hash: "1234".to_string(),
            output: serde_json::Value::Null,
        })
    );
}

#[test]
fn state_store_reads_current_string_agent_output_as_string_value() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-current-string".to_string(),
        task_id: "task-current-string".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let current_record = serde_json::json!({
        "phases.scan:1234": {
            "callId": "call-1",
            "callPath": "phases.scan",
            "prompt": "inspect repo",
            "opts": null,
            "inputHash": "1234",
            "status": "completed",
            "output": "current result",
            "error": null,
            "transcriptPath": null
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&current_record).unwrap(),
    )
    .unwrap();

    let cached = store
        .cached_agent_result(&state.run_id, "phases.scan", "1234")
        .unwrap();
    assert_eq!(
        cached,
        Some(WorkflowAgentCacheRecord {
            call_path: "phases.scan".to_string(),
            input_hash: "1234".to_string(),
            output: json!("current result"),
        })
    );
}

#[test]
fn state_store_preserves_current_json_looking_string_outputs() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-current-json-string".to_string(),
        task_id: "task-current-json-string".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    for (index, input_hash, output) in [
        (1usize, "1234", "123"),
        (2, "false", "false"),
        (3, "null", "null"),
        (4, "array", "[1]"),
        (5, "object", "{\"k\":1}"),
    ] {
        store
            .record_agent_completed(
                &state.run_id,
                WorkflowAgentRecord {
                    call_id: format!("call-{index}"),
                    call_path: "phases.scan".to_string(),
                    prompt: "inspect repo".to_string(),
                    opts: json!(null),
                    input_hash: input_hash.to_string(),
                    status: WorkflowAgentStatus::Completed,
                    attempt: 1,
                    max_attempts: 1,
                    previous_errors: Vec::new(),
                    output: Some(json!(output)),
                    error: None,
                    transcript_path: None,
                    usage: None,
                },
            )
            .unwrap();

        let found = store.find_cached_agent_value(&state.run_id, "phases.scan", input_hash);
        assert_eq!(found, Some(json!(output)), "find case {output}");

        let cached = store
            .cached_agent_result(&state.run_id, "phases.scan", input_hash)
            .unwrap();
        assert_eq!(
            cached,
            Some(WorkflowAgentCacheRecord {
                call_path: "phases.scan".to_string(),
                input_hash: input_hash.to_string(),
                output: json!(output),
            }),
            "cached case {output}"
        );
    }
}

#[test]
fn state_store_preserves_missing_output_field_when_appending_completed_record() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-missing-output-fidelity".to_string(),
        task_id: "task-missing-output-fidelity".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 2,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let cache_path = store.run_dir(&state.run_id).join("agent-cache.json");
    let current_records = serde_json::json!({
        "phases.scan:missing-output": {
            "callId": "call-1",
            "callPath": "phases.scan",
            "prompt": "inspect repo",
            "opts": null,
            "inputHash": "missing-output",
            "status": "completed",
            "error": null,
            "transcriptPath": null
        }
    });
    fs::write(
        &cache_path,
        serde_json::to_string_pretty(&current_records).unwrap(),
    )
    .unwrap();

    store
        .record_agent_completed(
            &state.run_id,
            WorkflowAgentRecord {
                call_id: "call-2".to_string(),
                call_path: "phases.scan".to_string(),
                prompt: "inspect repo again".to_string(),
                opts: json!(null),
                input_hash: "with-output".to_string(),
                status: WorkflowAgentStatus::Completed,
                attempt: 1,
                max_attempts: 1,
                previous_errors: Vec::new(),
                output: Some(json!("cached result")),
                error: None,
                transcript_path: None,
                usage: None,
            },
        )
        .unwrap();

    assert_eq!(
        store.find_cached_agent_value(&state.run_id, "phases.scan", "missing-output"),
        None
    );
    assert_eq!(
        store
            .cached_agent_result(&state.run_id, "phases.scan", "missing-output")
            .unwrap(),
        None
    );

    let rewritten_cache: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_path).unwrap()).unwrap();
    let original = &rewritten_cache["phases.scan:missing-output"];
    assert!(
        original.get("output").is_none(),
        "missing output field should stay omitted after cache rewrite: {original}"
    );
    assert_eq!(
        rewritten_cache["phases.scan:with-output"]["output"],
        json!("cached result")
    );
}

#[test]
fn state_store_cached_agent_result_ignores_incomplete_or_outputless_current_records() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-current-incomplete".to_string(),
        task_id: "task-current-incomplete".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 2,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let current_records = serde_json::json!({
        "phases.scan:in-progress": {
            "callId": "call-1",
            "callPath": "phases.scan",
            "prompt": "inspect repo",
            "opts": null,
            "inputHash": "in-progress",
            "status": "running",
            "output": "result",
            "error": null,
            "transcriptPath": null
        },
        "phases.scan:missing-output": {
            "callId": "call-2",
            "callPath": "phases.scan",
            "prompt": "inspect repo",
            "opts": null,
            "inputHash": "missing-output",
            "status": "completed",
            "error": null,
            "transcriptPath": null
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&current_records).unwrap(),
    )
    .unwrap();

    assert_eq!(
        store
            .cached_agent_result(&state.run_id, "phases.scan", "in-progress")
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .cached_agent_result(&state.run_id, "phases.scan", "missing-output")
            .unwrap(),
        None
    );
    assert_eq!(
        store.find_cached_agent_value(&state.run_id, "phases.scan", "missing-output"),
        None
    );
}
