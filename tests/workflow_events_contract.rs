use orca_core::config::WorkflowConfig;
use orca_core::config::file::FileConfig;
use orca_core::event_schema::{EventFactory, EventType};

#[test]
fn workflow_events_serialize_with_expected_names() {
    assert_eq!(
        serde_json::to_string(&EventType::WorkflowStarted).unwrap(),
        "\"workflow.started\""
    );
    assert_eq!(
        serde_json::to_string(&EventType::WorkflowResultAvailable).unwrap(),
        "\"workflow.result.available\""
    );
}

#[test]
fn workflow_event_factory_includes_run_and_task_ids() {
    let mut factory = EventFactory::new("run-outer".to_string());
    let event =
        factory.workflow_started("task-1", "workflow-run-1", "audit", &["scan".to_string()]);

    assert_eq!(event.event_type, EventType::WorkflowStarted);
    assert_eq!(event.payload["taskId"], "task-1");
    assert_eq!(event.payload["runId"], "workflow-run-1");
    assert_eq!(event.payload["workflowName"], "audit");
    assert_eq!(event.payload["phases"][0], "scan");
}

#[test]
fn workflow_config_defaults_match_public_limits() {
    let config = WorkflowConfig::default();
    assert!(config.enabled);
    assert_eq!(config.max_concurrent_agents, 16);
    assert_eq!(config.max_agents_per_run, 1000);
    assert!(config.keyword_trigger_enabled);
}

#[test]
fn workflow_config_parses_enable_disable_aliases() {
    let disabled: FileConfig = toml::from_str(
        r#"
[workflows]
disableWorkflows = true
"#,
    )
    .unwrap();
    assert!(!disabled.workflows.resolved().enabled);

    let enabled: FileConfig = toml::from_str(
        r#"
[workflows]
enableWorkflows = false
"#,
    )
    .unwrap();
    assert!(!enabled.workflows.resolved().enabled);
}

#[test]
fn workflow_config_parses_keyword_trigger_alias() {
    let config: FileConfig = toml::from_str(
        r#"
[workflows]
workflowKeywordTriggerEnabled = false
"#,
    )
    .unwrap();

    assert!(!config.workflows.resolved().keyword_trigger_enabled);
}
