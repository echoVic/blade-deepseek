# Workflow Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an Orca workflow runtime that matches Claude Code Dynamic workflows' public behavior: `Workflow` tool launch, JavaScript orchestration, background tasks, script persistence, same-session resume, and workflow management.

**Architecture:** Add shared workflow/task types in `orca-core`, session-scoped background task and workflow runtime modules in `orca-runtime`, and wire the `Workflow` tool through the existing agent controller as a runtime-special tool like `subagent`. Use an external Node.js host for JavaScript workflow scripts and keep agent execution in Rust by reusing the existing child-agent loop.

**Tech Stack:** Rust 2024, serde/serde_json, chrono, uuid, std threads/channels/process, Node.js ES modules for the workflow host, existing mock provider contract tests.

## Global Constraints

- Built-in tool name: `Workflow`.
- `WorkflowInput` fields: `script`, `name`, `description`, `title`, `args`, `scriptPath`, `resumeFromRunId`.
- `WorkflowOutput` fields: `status`, `taskId`, `taskType`, `workflowName`, `runId`, `summary`, `transcriptDir`, `scriptPath`, `sessionUrl`.
- Script shape: a JavaScript module beginning with a static `export const meta = { name, description, phases }`.
- Runtime helpers: `agent()`, `parallel()`, `pipeline()`, `phase()`.
- Workflow launch returns immediately with `status: "async_launched"`.
- `resumeFromRunId` reuses completed `agent()` calls only within the active session.
- Maximum concurrent workflow agents: 16.
- Maximum agents per workflow run: 1,000.
- Workflow scripts must not directly expose filesystem, shell, prompt, or dialog helpers.
- Workflow agents run with accept-edits style permissions and inherit the configured allowlist.
- Integration tests must run with `--provider mock` or `--provider deepseek-fixture`; no real API key required.

---

## File Structure

- Create `crates/orca-core/src/workflow_types.rs`: public workflow input/output/state/event structs and enums.
- Create `crates/orca-core/src/task_types.rs`: background task metadata and status structs shared by CLI/TUI/runtime.
- Modify `crates/orca-core/src/tool_types.rs`: add `ToolName::Workflow`.
- Modify `crates/orca-core/src/event_schema.rs`: add workflow event variants and factory helpers.
- Modify `crates/orca-core/src/config/mod.rs` and `crates/orca-core/src/config/file.rs`: add workflow config defaults and settings.
- Create `crates/orca-runtime/src/tasks.rs`: session-scoped in-memory task registry with cancellation/pause handles.
- Create `crates/orca-runtime/src/workflow/mod.rs`: runtime facade and launch/resume orchestration.
- Create `crates/orca-runtime/src/workflow/script.rs`: script/name/path resolution and persisted run directories.
- Create `crates/orca-runtime/src/workflow/host.rs`: Rust side of the Node host protocol.
- Create `crates/orca-runtime/src/workflow/host.mjs`: JavaScript workflow host.
- Create `crates/orca-runtime/src/workflow/state.rs`: run state persistence, cache lookup, and transcript paths.
- Create `crates/orca-runtime/src/workflow/agent.rs`: workflow child-agent executor wrapper.
- Modify `crates/orca-runtime/src/controller.rs`: add runtime context, special-case `Workflow`, emit workflow events, and append final workflow result messages.
- Modify `crates/orca-runtime/src/server.rs`: pass workflow events through to JSONL protocol.
- Modify `crates/orca-runtime/src/lib.rs`: export `tasks` and `workflow`.
- Modify `crates/orca-tools/src/registry.rs`: register the `Workflow` schema.
- Modify `crates/orca-provider/src/lib.rs`: teach the mock provider to request `Workflow`.
- Modify `src/cli.rs`: add `orca workflow run/list/show/stop/resume`.
- Modify `README.md`: document workflow commands and the `Workflow` tool.
- Create tests:
  - `tests/workflow_types_contract.rs`
  - `tests/workflow_events_contract.rs`
  - `tests/workflow_script_contract.rs`
  - `tests/workflow_host_contract.rs`
  - `tests/workflow_runtime_contract.rs`
  - `tests/workflow_tool_contract.rs`
  - `tests/workflow_cli_contract.rs`

---

### Task 1: Core Workflow And Task Types

**Files:**
- Create: `crates/orca-core/src/workflow_types.rs`
- Create: `crates/orca-core/src/task_types.rs`
- Modify: `crates/orca-core/src/lib.rs`
- Modify: `crates/orca-core/src/tool_types.rs`
- Test: `tests/workflow_types_contract.rs`

**Interfaces:**
- Produces: `WorkflowInput`, `WorkflowOutput`, `WorkflowMeta`, `WorkflowRunStatus`, `WorkflowAgentStatus`, `WorkflowRunState`, `TaskStatus`, `TaskType`, `BackgroundTaskSummary`.
- Produces: `ToolName::Workflow` with `as_str() == "Workflow"` and `from_str("Workflow")`.
- Consumes: existing `serde`, `serde_json`, `chrono`, `uuid`.

- [ ] **Step 1: Write failing type serialization tests**

Create `tests/workflow_types_contract.rs`:

```rust
use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};
use orca_core::tool_types::ToolName;
use orca_core::workflow_types::{WorkflowInput, WorkflowOutput, WorkflowRunStatus};

#[test]
fn workflow_input_accepts_official_fields() {
    let input: WorkflowInput = serde_json::from_value(serde_json::json!({
        "script": "export const meta = { name: 'audit', description: 'Audit code', phases: [] };",
        "name": "audit",
        "description": "ignored",
        "title": "ignored",
        "args": { "paths": ["src"] },
        "scriptPath": "/tmp/workflow.js",
        "resumeFromRunId": "workflow-run-1"
    }))
    .unwrap();

    assert!(input.script.unwrap().contains("export const meta"));
    assert_eq!(input.name.as_deref(), Some("audit"));
    assert_eq!(input.args.unwrap()["paths"][0], "src");
    assert_eq!(input.script_path.as_deref(), Some("/tmp/workflow.js"));
    assert_eq!(input.resume_from_run_id.as_deref(), Some("workflow-run-1"));
}

#[test]
fn workflow_output_serializes_claude_compatible_shape() {
    let output = WorkflowOutput {
        status: "async_launched".to_string(),
        task_id: "task-1".to_string(),
        task_type: Some("local_workflow".to_string()),
        workflow_name: Some("audit".to_string()),
        run_id: Some("workflow-run-1".to_string()),
        summary: Some("Workflow launched".to_string()),
        transcript_dir: Some("/tmp/transcripts".to_string()),
        script_path: Some("/tmp/script.js".to_string()),
        session_url: None,
    };

    let value = serde_json::to_value(output).unwrap();
    assert_eq!(value["status"], "async_launched");
    assert_eq!(value["taskId"], "task-1");
    assert_eq!(value["taskType"], "local_workflow");
    assert_eq!(value["workflowName"], "audit");
    assert_eq!(value["runId"], "workflow-run-1");
    assert_eq!(value["scriptPath"], "/tmp/script.js");
    assert!(value.get("sessionUrl").is_none());
}

#[test]
fn workflow_tool_name_round_trips() {
    assert_eq!(ToolName::Workflow.as_str(), "Workflow");
    assert_eq!(ToolName::from_str("Workflow"), Some(ToolName::Workflow));
}

#[test]
fn background_task_summary_matches_sdk_names() {
    let summary = BackgroundTaskSummary {
        id: "task-1".to_string(),
        task_type: TaskType::Workflow,
        status: TaskStatus::Running,
        description: "Audit codebase".to_string(),
        command: None,
        agent_type: None,
        server: None,
        tool: None,
        name: Some("audit".to_string()),
    };

    let value = serde_json::to_value(summary).unwrap();
    assert_eq!(value["type"], "workflow");
    assert_eq!(value["status"], "running");
    assert_eq!(value["name"], "audit");
}

#[test]
fn workflow_status_serializes_snake_case() {
    assert_eq!(
        serde_json::to_value(WorkflowRunStatus::AsyncLaunched).unwrap(),
        "async_launched"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test workflow_types_contract`

Expected: compile fails with unresolved imports for `workflow_types`, `task_types`, and `ToolName::Workflow`.

- [ ] **Step 3: Implement the types**

Add to `crates/orca-core/src/lib.rs`:

```rust
pub mod task_types;
pub mod workflow_types;
```

Create `crates/orca-core/src/workflow_types.rs` with serde field renames:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_from_run_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowOutput {
    pub status: String,
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowMeta {
    pub name: String,
    pub description: String,
    pub phases: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Queued,
    Running,
    Paused,
    Stopping,
    Stopped,
    Completed,
    Failed,
    Cancelled,
    AsyncLaunched,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAgentStatus {
    Pending,
    Running,
    Cached,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunState {
    pub run_id: String,
    pub task_id: String,
    pub session_id: String,
    pub cwd: String,
    pub workflow_name: String,
    pub meta: WorkflowMeta,
    pub script_digest: String,
    pub args_digest: String,
    pub status: WorkflowRunStatus,
    pub total_agent_count: u32,
    pub final_summary: Option<String>,
    pub error: Option<String>,
}
```

Create `crates/orca-core/src/task_types.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Paused,
    Stopping,
    Stopped,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    Workflow,
    Subagent,
    Shell,
    Monitor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskSummary {
    pub id: String,
    #[serde(rename = "type")]
    pub task_type: TaskType,
    pub status: TaskStatus,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}
```

Update `ToolName` in `crates/orca-core/src/tool_types.rs`:

```rust
Workflow,
```

Add match arms:

```rust
Self::Workflow => "Workflow",
"Workflow" | "workflow" => Self::Workflow,
```

Keep `Workflow` out of `is_read_only()`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test workflow_types_contract`

Expected: all 5 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-core/src/lib.rs crates/orca-core/src/workflow_types.rs crates/orca-core/src/task_types.rs crates/orca-core/src/tool_types.rs tests/workflow_types_contract.rs
git commit -m "feat(workflow): add core workflow types"
```

---

### Task 2: Workflow Tool Schema And Mock Provider Trigger

**Files:**
- Modify: `crates/orca-tools/src/registry.rs`
- Modify: `crates/orca-provider/src/lib.rs`
- Test: `tests/workflow_tool_contract.rs`

**Interfaces:**
- Consumes: `ToolName::Workflow` from Task 1.
- Produces: a registered `Workflow` schema with official input fields.
- Produces: mock prompt prefix `workflow ` that creates a `ToolRequest` named `Workflow`.

- [ ] **Step 1: Write failing schema and mock tests**

Create `tests/workflow_tool_contract.rs`:

```rust
use std::process::Command;

use serde_json::Value;

#[test]
fn workflow_schema_is_registered_with_official_fields() {
    let registry = orca_tools::registry::default_tool_registry();
    let tool = registry.get("Workflow").expect("Workflow tool registered");
    let schema = tool.schema();
    let properties = &schema["function"]["parameters"]["properties"];

    assert_eq!(schema["function"]["name"], "Workflow");
    assert!(properties.get("script").is_some());
    assert!(properties.get("name").is_some());
    assert!(properties.get("description").is_some());
    assert!(properties.get("title").is_some());
    assert!(properties.get("args").is_some());
    assert!(properties.get("scriptPath").is_some());
    assert!(properties.get("resumeFromRunId").is_some());
}

#[test]
fn mock_provider_can_request_workflow_tool() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "workflow inline",
        ])
        .output()
        .expect("run orca");

    let events = parse_jsonl(&output.stdout);
    let requested = events
        .iter()
        .find(|event| event["type"] == "tool.call.requested")
        .expect("tool requested");
    assert_eq!(requested["payload"]["name"], "Workflow");
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test workflow_tool_contract`

Expected: first test fails because `Workflow` is not registered; second test fails because mock provider does not emit `Workflow`.

- [ ] **Step 3: Register the schema**

In `crates/orca-tools/src/registry.rs`, add a builtin tool:

```rust
registry.register(BuiltinTool::new(
    "Workflow",
    "Run a dynamic workflow: a JavaScript script that orchestrates many subagents in the background and returns one consolidated result.",
    ActionKind::Agent,
    json!({
        "type": "object",
        "properties": {
            "script": {
                "type": "string",
                "description": "Self-contained workflow script beginning with export const meta = { name, description, phases }."
            },
            "name": {
                "type": "string",
                "description": "Name of a predefined workflow from .claude/workflows/ or the user workflow directory."
            },
            "description": {
                "type": "string",
                "description": "Compatibility field ignored by the runtime; use meta.description in the script."
            },
            "title": {
                "type": "string",
                "description": "Compatibility field ignored by the runtime; use meta.name in the script."
            },
            "args": {
                "type": "object",
                "description": "Structured input exposed to the workflow script as the global args value."
            },
            "scriptPath": {
                "type": "string",
                "description": "Path to a workflow script file. Takes precedence over script and name."
            },
            "resumeFromRunId": {
                "type": "string",
                "description": "Run id of a prior same-session workflow invocation to resume from."
            }
        },
        "required": []
    }),
    BuiltinExecutor::Workflow,
));
```

Add `BuiltinExecutor::Workflow` and make direct execution return:

```rust
ToolResult::failed(request, "Workflow must be executed by the runtime controller", None)
```

- [ ] **Step 4: Add mock provider workflow prompt**

In `crates/orca-provider/src/lib.rs`, extend `parse_mock_prompt`:

```rust
if let Some(rest) = prompt.strip_prefix("workflow ") {
    let mode = rest.trim();
    let script = "export const meta = { name: 'mock-workflow', description: 'Mock workflow', phases: ['main'] };\\nconst result = await phase('main', async () => agent('inspect repo'));\\nexport default result;";
    return Some(ToolRequest {
        id: "mock-tool-1".to_string(),
        name: ToolName::Workflow,
        action: ActionKind::Agent,
        target: Some(mode.to_string()),
        raw_arguments: Some(
            serde_json::json!({
                "script": script,
                "args": { "mode": mode }
            })
            .to_string(),
        ),
    });
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --test workflow_tool_contract`

Expected: schema test passes; mock request test may still fail with a runtime execution error after `tool.call.requested`, which is acceptable for this task.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-tools/src/registry.rs crates/orca-provider/src/lib.rs tests/workflow_tool_contract.rs
git commit -m "feat(workflow): register workflow tool schema"
```

---

### Task 3: Workflow Events And Config

**Files:**
- Modify: `crates/orca-core/src/event_schema.rs`
- Modify: `crates/orca-core/src/config/mod.rs`
- Modify: `crates/orca-core/src/config/file.rs`
- Test: `tests/workflow_events_contract.rs`

**Interfaces:**
- Produces: `EventType` variants for workflow lifecycle.
- Produces: `EventFactory::workflow_started`, `workflow_agent_completed`, `workflow_completed`, and `workflow_result_available`.
- Produces: `WorkflowConfig` in `RunConfig`.

- [ ] **Step 1: Write failing event/config tests**

Create `tests/workflow_events_contract.rs`:

```rust
use orca_core::config::WorkflowConfig;
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
    let event = factory.workflow_started("task-1", "workflow-run-1", "audit", &["scan".to_string()]);

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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test workflow_events_contract`

Expected: unresolved event variants and `WorkflowConfig`.

- [ ] **Step 3: Add event variants and factory helpers**

In `crates/orca-core/src/event_schema.rs`, add variants:

```rust
#[serde(rename = "workflow.started")]
WorkflowStarted,
#[serde(rename = "workflow.resumed")]
WorkflowResumed,
#[serde(rename = "workflow.phase.started")]
WorkflowPhaseStarted,
#[serde(rename = "workflow.phase.completed")]
WorkflowPhaseCompleted,
#[serde(rename = "workflow.agent.started")]
WorkflowAgentStarted,
#[serde(rename = "workflow.agent.cached")]
WorkflowAgentCached,
#[serde(rename = "workflow.agent.completed")]
WorkflowAgentCompleted,
#[serde(rename = "workflow.agent.failed")]
WorkflowAgentFailed,
#[serde(rename = "workflow.paused")]
WorkflowPaused,
#[serde(rename = "workflow.stopped")]
WorkflowStopped,
#[serde(rename = "workflow.completed")]
WorkflowCompleted,
#[serde(rename = "workflow.failed")]
WorkflowFailed,
#[serde(rename = "workflow.result.available")]
WorkflowResultAvailable,
```

Add factory helpers with camelCase payload keys:

```rust
pub fn workflow_started(
    &mut self,
    task_id: &str,
    run_id: &str,
    workflow_name: &str,
    phases: &[String],
) -> EventEnvelope {
    self.make(
        EventType::WorkflowStarted,
        json!({
            "taskId": task_id,
            "runId": run_id,
            "workflowName": workflow_name,
            "phases": phases
        }),
    )
}
```

Implement similar helpers for agent completion and workflow completion:

```rust
pub fn workflow_result_available(
    &mut self,
    task_id: &str,
    run_id: &str,
    result: &str,
) -> EventEnvelope {
    self.make(
        EventType::WorkflowResultAvailable,
        json!({
            "taskId": task_id,
            "runId": run_id,
            "result": result
        }),
    )
}
```

- [ ] **Step 4: Add workflow config**

In `crates/orca-core/src/config/mod.rs`:

```rust
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowConfig {
    #[serde(default = "default_workflows_enabled")]
    pub enabled: bool,
    #[serde(default = "default_max_workflow_concurrent_agents")]
    pub max_concurrent_agents: usize,
    #[serde(default = "default_max_workflow_agents_per_run")]
    pub max_agents_per_run: u32,
    #[serde(default = "default_workflow_keyword_trigger_enabled")]
    pub keyword_trigger_enabled: bool,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent_agents: 16,
            max_agents_per_run: 1000,
            keyword_trigger_enabled: true,
        }
    }
}
```

Add `pub workflows: WorkflowConfig` to `RunConfig` and fill all constructors/tests with `WorkflowConfig::default()`.

In `crates/orca-core/src/config/file.rs`, map:

- `disableWorkflows = true` to `enabled = false`
- `enableWorkflows = false` to `enabled = false`
- `workflowKeywordTriggerEnabled = false` to `keyword_trigger_enabled = false`

Use serde aliases:

```rust
#[serde(alias = "disableWorkflows")]
pub disable_workflows: Option<bool>,
#[serde(alias = "enableWorkflows")]
pub enable_workflows: Option<bool>,
#[serde(alias = "workflowKeywordTriggerEnabled")]
pub workflow_keyword_trigger_enabled: Option<bool>,
```

- [ ] **Step 5: Run tests**

Run: `cargo test --test workflow_events_contract`

Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-core/src/event_schema.rs crates/orca-core/src/config/mod.rs crates/orca-core/src/config/file.rs tests/workflow_events_contract.rs src/cli.rs
git commit -m "feat(workflow): add events and config"
```

---

### Task 4: Session Task Registry

**Files:**
- Create: `crates/orca-runtime/src/tasks.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Test: `crates/orca-runtime/src/tasks.rs`

**Interfaces:**
- Consumes: `TaskStatus`, `TaskType`, `BackgroundTaskSummary`.
- Produces: `TaskRegistry`, `TaskHandle`, `TaskRecord`, `TaskControl`.
- Produces: `TaskRegistry::create_workflow`, `list`, `get`, `mark_running`, `complete`, `fail`, `request_stop`, `request_pause`, `request_resume`.

- [ ] **Step 1: Write failing unit tests**

In new `crates/orca-runtime/src/tasks.rs`, start with tests at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_creates_and_lists_workflow_tasks() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
        );

        assert!(task.id.starts_with("task-"));
        assert_eq!(task.workflow_run_id.as_deref(), Some("workflow-run-1"));

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task_type, orca_core::task_types::TaskType::Workflow);
        assert_eq!(list[0].status, orca_core::task_types::TaskStatus::Queued);
        assert_eq!(list[0].name.as_deref(), Some("audit"));
    }

    #[test]
    fn stop_sets_cancel_flag_and_status() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
        );

        registry.request_stop(&task.id).unwrap();
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, orca_core::task_types::TaskStatus::Stopping);
        assert!(record.control.cancel.is_cancelled());
    }

    #[test]
    fn complete_stores_result() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
        );

        registry.complete(&task.id, "done".to_string()).unwrap();
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, orca_core::task_types::TaskStatus::Completed);
        assert_eq!(record.result.as_deref(), Some("done"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p orca-runtime tasks::tests`

Expected: `TaskRegistry` types are missing.

- [ ] **Step 3: Implement registry**

Use `Arc<Mutex<HashMap<String, TaskRecord>>>` and existing `orca_core::cancel::CancelToken`.

Key structs:

```rust
#[derive(Clone)]
pub struct TaskRegistry {
    session_id: String,
    inner: Arc<Mutex<TaskRegistryInner>>,
}

#[derive(Clone, Debug)]
pub struct TaskRecord {
    pub id: String,
    pub task_type: TaskType,
    pub status: TaskStatus,
    pub description: String,
    pub name: Option<String>,
    pub workflow_run_id: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub control: TaskControl,
}

#[derive(Clone, Debug)]
pub struct TaskControl {
    pub cancel: CancelToken,
    pub pause: Arc<AtomicBool>,
}
```

Implement ids with `uuid::Uuid::new_v4()`:

```rust
fn new_task_id() -> String {
    format!("task-{}", uuid::Uuid::new_v4())
}
```

Implement `list()` by converting records to `BackgroundTaskSummary`.

- [ ] **Step 4: Export module**

In `crates/orca-runtime/src/lib.rs`:

```rust
pub mod tasks;
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p orca-runtime tasks::tests`

Expected: all task registry tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-runtime/src/tasks.rs crates/orca-runtime/src/lib.rs
git commit -m "feat(workflow): add background task registry"
```

---

### Task 5: Script Resolution And Run State Store

**Files:**
- Create: `crates/orca-runtime/src/workflow/mod.rs`
- Create: `crates/orca-runtime/src/workflow/script.rs`
- Create: `crates/orca-runtime/src/workflow/state.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Add dependency: `sha2 = "0.10"` in workspace and `orca-runtime`
- Test: `tests/workflow_script_contract.rs`

**Interfaces:**
- Consumes: `WorkflowInput`, `WorkflowMeta`, `WorkflowRunState`.
- Produces: `ResolvedWorkflowScript { source_kind, original_path, persisted_path, meta, script, script_digest }`.
- Produces: `WorkflowStateStore::create_run`, `load_run`, `write_state`, `record_agent_completed`, `cached_agent_result`.

- [ ] **Step 1: Write failing script resolution tests**

Create `tests/workflow_script_contract.rs`:

```rust
use std::fs;

use orca_core::workflow_types::WorkflowInput;
use orca_runtime::workflow::script::resolve_workflow_script;
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
    assert!(fs::read_to_string(resolved.persisted_path).unwrap().contains("export const meta"));
    assert_eq!(resolved.script_digest.len(), 64);
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
        script: Some("export const meta = { name: 'ignored', description: 'Ignored', phases: [] };".to_string()),
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
    let cwd = temp.path().join("repo/packages/api");
    fs::create_dir_all(cwd.join(".claude/workflows")).unwrap();
    fs::create_dir_all(temp.path().join("home/.claude/workflows")).unwrap();

    fs::write(
        cwd.join(".claude/workflows/audit.js"),
        "export const meta = { name: 'audit', description: 'Project audit', phases: [] };\nexport default 'project';",
    )
    .unwrap();
    fs::write(
        temp.path().join("home/.claude/workflows/audit.js"),
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
        &temp.path().join("home/.claude/workflows"),
    )
    .unwrap();

    assert_eq!(resolved.meta.description, "Project audit");
}
```

Add the needed import in the test:

```rust
use orca_runtime::workflow::script::resolve_workflow_script_with_user_dir;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test workflow_script_contract`

Expected: unresolved workflow module and resolver functions.

- [ ] **Step 3: Add digest dependency**

In root `Cargo.toml` workspace dependencies:

```toml
sha2 = "0.10"
```

In `crates/orca-runtime/Cargo.toml`:

```toml
sha2 = { workspace = true }
```

- [ ] **Step 4: Implement script resolver**

In `crates/orca-runtime/src/workflow/mod.rs`:

```rust
pub mod script;
pub mod state;
```

In `script.rs`, implement:

```rust
pub struct ResolvedWorkflowScript {
    pub source_kind: WorkflowScriptSource,
    pub original_path: Option<PathBuf>,
    pub persisted_path: PathBuf,
    pub meta: WorkflowMeta,
    pub script: String,
    pub script_digest: String,
}
```

Use resolution order `scriptPath`, `script`, `name`. Persist to:

```rust
session_dir.join("workflows").join("scripts").join(format!("{}.js", meta.name))
```

Implement `sha256_hex(input: &[u8]) -> String` with `sha2::Sha256`.

For meta parsing in this task, use a narrow parser that accepts official static literal shape:

```rust
export const meta = { name: 'audit', description: 'Audit code', phases: ['scan'] };
```

The parser must support single or double quoted strings and `phases: []`. It must return an `io::ErrorKind::InvalidData` error when `name`, `description`, or `phases` is missing.

- [ ] **Step 5: Implement state store skeleton**

In `state.rs`, define `WorkflowStateStore` with paths and JSON read/write:

```rust
pub struct WorkflowStateStore {
    root: PathBuf,
}

impl WorkflowStateStore {
    pub fn new(root: PathBuf) -> Self;
    pub fn run_dir(&self, run_id: &str) -> PathBuf;
    pub fn transcript_dir(&self, run_id: &str) -> PathBuf;
    pub fn write_state(&self, state: &WorkflowRunState) -> io::Result<()>;
    pub fn load_state(&self, run_id: &str) -> io::Result<WorkflowRunState>;
}
```

Write `state.json` using `serde_json::to_string_pretty`.

- [ ] **Step 6: Export workflow module**

In `crates/orca-runtime/src/lib.rs`:

```rust
pub mod workflow;
```

- [ ] **Step 7: Run tests**

Run: `cargo test --test workflow_script_contract`

Expected: all script resolver tests pass.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock crates/orca-runtime/Cargo.toml crates/orca-runtime/src/lib.rs crates/orca-runtime/src/workflow tests/workflow_script_contract.rs
git commit -m "feat(workflow): resolve and persist workflow scripts"
```

---

### Task 6: Node Workflow Host Protocol

**Files:**
- Create: `crates/orca-runtime/src/workflow/host.rs`
- Create: `crates/orca-runtime/src/workflow/host.mjs`
- Modify: `crates/orca-runtime/src/workflow/mod.rs`
- Test: `tests/workflow_host_contract.rs`

**Interfaces:**
- Consumes: persisted workflow script path and `args`.
- Produces: `HostEvent` and `HostCommand` enums.
- Produces: `WorkflowHost::run_collecting_events(script_path, args)`.
- JS helpers emit JSONL protocol events.

- [ ] **Step 1: Write failing host protocol tests**

Create `tests/workflow_host_contract.rs`:

```rust
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

    assert!(events.iter().any(|event| matches!(event, HostEvent::PhaseStarted { name } if name == "scan")));
    assert!(events.iter().any(|event| matches!(event, HostEvent::AgentCall { prompt, .. } if prompt == "inspect repo")));
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

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!({"prompt": "from args"})).unwrap();
    assert!(events.iter().any(|event| matches!(event, HostEvent::AgentCall { prompt, .. } if prompt == "from args")));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test workflow_host_contract`

Expected: unresolved `workflow::host`.

- [ ] **Step 3: Implement host enums**

In `host.rs`:

```rust
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostEvent {
    PhaseStarted { name: String },
    PhaseCompleted { name: String },
    AgentCall {
        call_id: String,
        call_path: String,
        phase: Option<String>,
        prompt: String,
        opts: serde_json::Value,
    },
    WorkflowCompleted { result: serde_json::Value },
    WorkflowFailed { error: String },
}
```

`WorkflowHost::run_collecting_events` spawns:

```rust
node <temp-host-file> <script-path> <args-json>
```

Write `include_str!("host.mjs")` to a temp file under `std::env::temp_dir()/orca-workflow-host.mjs` before spawning.

- [ ] **Step 4: Implement JavaScript host**

Create `host.mjs`:

```javascript
const scriptPath = process.argv[2];
const argsJson = process.argv[3] ?? "null";
globalThis.args = JSON.parse(argsJson);

let callSeq = 0;
let currentPhase = null;

function emit(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

globalThis.agent = async function agent(prompt, opts = {}) {
  callSeq += 1;
  const callId = `agent-${callSeq}`;
  const callPath = `${currentPhase ?? "root"}:${callSeq}`;
  emit({
    type: "agent_call",
    call_id: callId,
    call_path: callPath,
    phase: currentPhase,
    prompt,
    opts,
  });
  return { callId, prompt, cached: false };
};

globalThis.parallel = async function parallel(items) {
  return Promise.all(items);
};

globalThis.pipeline = async function pipeline(items) {
  let previous;
  for (const item of items) {
    previous = typeof item === "function" ? await item(previous) : await item;
  }
  return previous;
};

globalThis.phase = async function phase(name, body) {
  const prior = currentPhase;
  currentPhase = name;
  emit({ type: "phase_started", name });
  try {
    const result = typeof body === "function" ? await body() : undefined;
    emit({ type: "phase_completed", name });
    return result;
  } finally {
    currentPhase = prior;
  }
};

try {
  const module = await import(`file://${scriptPath}`);
  emit({ type: "workflow_completed", result: module.default ?? null });
} catch (error) {
  emit({ type: "workflow_failed", error: error?.stack ?? String(error) });
  process.exitCode = 1;
}
```

This first host returns synthetic agent results. Task 8 replaces them with Rust-fed results over stdin.

- [ ] **Step 5: Run tests**

Run: `cargo test --test workflow_host_contract`

Expected: tests pass when Node is installed; tests skip when Node is missing.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-runtime/src/workflow/host.rs crates/orca-runtime/src/workflow/host.mjs crates/orca-runtime/src/workflow/mod.rs tests/workflow_host_contract.rs
git commit -m "feat(workflow): add javascript workflow host"
```

---

### Task 7: Reusable Child Agent Executor

**Files:**
- Create: `crates/orca-runtime/src/agent_child.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/controller.rs`
- Test: `tests/subagent_contract.rs`

**Interfaces:**
- Consumes: current private `run_agent_loop`.
- Produces: `ChildAgentRequest`, `ChildAgentResult`, `run_child_agent`.
- Preserves current `subagent` behavior.

- [ ] **Step 1: Add regression command**

Run existing tests before changing:

Run: `cargo test --test subagent_contract`

Expected: all tests pass before refactor.

- [ ] **Step 2: Extract child executor interface**

Create `crates/orca-runtime/src/agent_child.rs`:

```rust
use orca_core::event_schema::RunStatus;
use orca_core::subagent_types::SubagentType;

#[derive(Clone, Debug)]
pub struct ChildAgentRequest {
    pub prompt: String,
    pub subagent_type: SubagentType,
    pub model: Option<String>,
    pub depth: u32,
    pub emit_deltas: bool,
}

#[derive(Clone, Debug)]
pub struct ChildAgentResult {
    pub status: RunStatus,
    pub final_message: Option<String>,
    pub error: Option<String>,
}
```

Move the minimum shared code out of `controller.rs` without changing behavior. Keep `run_agent_loop` in `controller.rs` if moving it would make the diff too large; expose a small internal wrapper that workflow can call in a later task.

- [ ] **Step 3: Replace subagent batch internals with the interface**

In `execute_subagent_batch`, construct `ChildAgentRequest` from `subagent::create_subagent_request` and call the shared child executor. Keep all event emission exactly as today:

- `subagent.started`
- `subagent.completed`
- `tool.call.completed`

- [ ] **Step 4: Run regression tests**

Run: `cargo test --test subagent_contract`

Expected: all tests pass unchanged.

- [ ] **Step 5: Run broader controller tests**

Run: `cargo test --test agent_loop_contract --test tool_contract`

Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-runtime/src/agent_child.rs crates/orca-runtime/src/lib.rs crates/orca-runtime/src/controller.rs
git commit -m "refactor(runtime): share child agent execution"
```

---

### Task 8: Workflow Runner With Agent Cache

**Files:**
- Create: `crates/orca-runtime/src/workflow/runner.rs`
- Modify: `crates/orca-runtime/src/workflow/host.rs`
- Modify: `crates/orca-runtime/src/workflow/host.mjs`
- Modify: `crates/orca-runtime/src/workflow/state.rs`
- Modify: `crates/orca-runtime/src/workflow/mod.rs`
- Test: `tests/workflow_runtime_contract.rs`

**Interfaces:**
- Consumes: `TaskRegistry`, `WorkflowStateStore`, `WorkflowHost`, child agent executor.
- Produces: `WorkflowRunner::launch`, `WorkflowRunner::resume`.
- Produces: cached `agent()` behavior keyed by call path and input hash.

- [ ] **Step 1: Write failing runtime tests**

Create `tests/workflow_runtime_contract.rs`:

```rust
use std::fs;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::{HistoryMode, OutputFormat, ProviderKind, RunConfig, ToolConfig, WorkflowConfig};
use orca_core::model::ModelSelection;
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::workflow::{WorkflowLaunchRequest, WorkflowRunner};
use tempfile::tempdir;

#[test]
fn workflow_runner_executes_agent_and_writes_state() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'audit', description: 'Audit code', phases: ['scan'] };\nconst result = await phase('scan', async () => agent('inspect repo'));\nexport default result;",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));

    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(script.display().to_string()))
        .unwrap();

    let record = tasks.get(&launched.task_id).unwrap();
    assert_eq!(record.status, orca_core::task_types::TaskStatus::Completed);
    assert!(record.result.unwrap().contains("inspect repo"));
    assert!(launched.output.script_path.unwrap().ends_with(".js"));
    assert!(launched.output.transcript_dir.unwrap().contains("transcripts"));
}

#[test]
fn workflow_resume_uses_completed_agent_cache() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'cache', description: 'Cache test', phases: [] };\nconst result = await agent('inspect repo');\nexport default result;",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));

    let first = runner
        .launch(WorkflowLaunchRequest::from_script_path(script.display().to_string()))
        .unwrap();
    let second = runner
        .launch(
            WorkflowLaunchRequest::from_script_path(script.display().to_string())
                .with_resume_from(first.output.run_id.clone().unwrap()),
        )
        .unwrap();

    assert!(second.summary.contains("cached 1 agent"));
}

fn mock_run_config(cwd: &std::path::Path) -> RunConfig {
    RunConfig {
        prompt: String::new(),
        cwd: Some(cwd.to_path_buf()),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::FullAuto,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::from_unchecked(Some("auto".to_string())),
        api_key: None,
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode: HistoryMode::Disabled,
        show_session_picker: false,
        permission_rules: Default::default(),
        max_budget_usd: None,
        subagents: Default::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: Default::default(),
        vim_mode: false,
        update_check: false,
        desktop_notifications: false,
        auto_memory: false,
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test workflow_runtime_contract`

Expected: unresolved `WorkflowRunner`.

- [ ] **Step 3: Upgrade host to request Rust agent results**

Change `host.mjs` so `agent()` emits `agent_call`, reads one JSON line from stdin, and returns `message.result` or throws on `agent_error`.

Rust `WorkflowHost` must:

- spawn Node with piped stdin/stdout
- read each host event line
- call a callback for `AgentCall`
- write `{"type":"agent_result","call_id":"...","result":"..."}` to host stdin

- [ ] **Step 4: Implement state cache**

In `state.rs`, add agent records:

```rust
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgentRecord {
    pub call_id: String,
    pub call_path: String,
    pub prompt: String,
    pub opts: serde_json::Value,
    pub input_hash: String,
    pub status: WorkflowAgentStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub transcript_path: Option<String>,
}
```

Add methods:

```rust
pub fn input_hash(prompt: &str, opts: &serde_json::Value) -> String;
pub fn find_cached_agent(&self, run_id: &str, call_path: &str, input_hash: &str) -> Option<String>;
pub fn record_agent_completed(&self, run_id: &str, record: WorkflowAgentRecord) -> io::Result<()>;
```

- [ ] **Step 5: Implement runner**

`WorkflowRunner::launch` should:

1. Resolve script.
2. Create task and run id.
3. Create run directory and state.
4. Execute host.
5. On every `agent_call`, check cache when resuming.
6. If no cache hit, run child agent with prompt and opts.
7. Write transcript output file under `transcripts/<call_id>.json`.
8. Mark task completed or failed.

For this task, run synchronously inside `launch` after creating the task. Task 10 changes controller launch to background thread while preserving the same runner API.

- [ ] **Step 6: Run tests**

Run: `cargo test --test workflow_runtime_contract`

Expected: both runtime tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/orca-runtime/src/workflow tests/workflow_runtime_contract.rs
git commit -m "feat(workflow): run workflow agents with cache"
```

---

### Task 9: `parallel`, `pipeline`, `phase`, And Limits

**Files:**
- Modify: `crates/orca-runtime/src/workflow/host.mjs`
- Modify: `crates/orca-runtime/src/workflow/runner.rs`
- Modify: `crates/orca-runtime/src/workflow/state.rs`
- Test: `tests/workflow_runtime_contract.rs`

**Interfaces:**
- Consumes: Task 8 host protocol.
- Produces: ordered `parallel()` results, sequential `pipeline()` behavior, phase records, 16 concurrency cap, 1,000 total agent cap.

- [ ] **Step 1: Add failing tests**

Append to `tests/workflow_runtime_contract.rs`:

```rust
#[test]
fn parallel_preserves_order_and_records_phase() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'parallel', description: 'Parallel test', phases: ['fanout'] };\nconst result = await phase('fanout', async () => parallel([agent('first'), agent('second')]));\nexport default result.map(item => item.prompt).join(',');",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(script.display().to_string()))
        .unwrap();

    assert!(launched.summary.contains("first,second"));
}

#[test]
fn agent_cap_failure_is_recorded() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'cap', description: 'Cap test', phases: [] };\nfor (let i = 0; i < 1001; i++) await agent(`agent ${i}`);\nexport default 'unreachable';",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));
    let err = runner
        .launch(WorkflowLaunchRequest::from_script_path(script.display().to_string()))
        .unwrap_err();

    assert!(err.to_string().contains("maximum workflow agent count 1000 exceeded"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test workflow_runtime_contract parallel_preserves_order_and_records_phase agent_cap_failure_is_recorded`

Expected: cap test fails until runner enforces total count; order test fails if host returns synthetic objects without Rust results.

- [ ] **Step 3: Enforce limits**

In `WorkflowRunner`, maintain:

```rust
struct WorkflowExecutionCounters {
    total_agents: u32,
    active_agents: usize,
}
```

Before starting an agent:

```rust
if counters.total_agents >= config.workflows.max_agents_per_run {
    return Err(io::Error::new(
        io::ErrorKind::Other,
        format!(
            "maximum workflow agent count {} exceeded",
            config.workflows.max_agents_per_run
        ),
    ));
}
```

Use a `Condvar` or channel semaphore to keep `active_agents <= max_concurrent_agents`.

- [ ] **Step 4: Persist phase records**

In state, add:

```rust
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPhaseRecord {
    pub name: String,
    pub status: WorkflowRunStatus,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub agent_count: u32,
}
```

Record `phase_started` and `phase_completed` events from host.

- [ ] **Step 5: Run tests**

Run: `cargo test --test workflow_runtime_contract`

Expected: all workflow runtime tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-runtime/src/workflow tests/workflow_runtime_contract.rs
git commit -m "feat(workflow): support workflow phases and limits"
```

---

### Task 10: Controller Integration And Background Launch

**Files:**
- Modify: `crates/orca-runtime/src/controller.rs`
- Modify: `crates/orca-runtime/src/server.rs`
- Test: `tests/workflow_tool_contract.rs`

**Interfaces:**
- Consumes: `WorkflowRunner`, `TaskRegistry`, workflow events.
- Produces: actual `Workflow` tool execution from the agent loop.
- Produces: `tool.call.completed` output containing `WorkflowOutput`.

- [ ] **Step 1: Add failing end-to-end workflow tool test**

Append to `tests/workflow_tool_contract.rs`:

```rust
#[test]
fn workflow_tool_launches_background_task_and_returns_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "workflow inline",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    let events = parse_jsonl(&output.stdout);

    let completed = events
        .iter()
        .find(|event| event["type"] == "tool.call.completed" && event["payload"]["name"] == "Workflow")
        .expect("workflow tool completed");
    assert_eq!(completed["payload"]["status"], "completed");

    let output_text = completed["payload"]["output"].as_str().unwrap();
    let workflow_output: Value = serde_json::from_str(output_text).unwrap();
    assert_eq!(workflow_output["status"], "async_launched");
    assert_eq!(workflow_output["taskType"], "local_workflow");
    assert!(workflow_output["taskId"].as_str().unwrap().starts_with("task-"));
    assert!(workflow_output["runId"].as_str().unwrap().starts_with("workflow-run-"));

    assert!(events.iter().any(|event| event["type"] == "workflow.started"));
    assert!(events.iter().any(|event| event["type"] == "workflow.result.available"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test workflow_tool_contract workflow_tool_launches_background_task_and_returns_output`

Expected: `Workflow` tool fails with "must be executed by runtime controller".

- [ ] **Step 3: Add runtime context to controller**

In `run_inner`, create:

```rust
let session_id = events.run_id().to_string();
let task_registry = TaskRegistry::new(session_id.clone());
```

Expose `EventFactory::run_id()` if needed.

Pass `task_registry.clone()` into `run_agent_loop` and `execute_tool_with_approval`.

- [ ] **Step 4: Special-case `Workflow` execution**

In `execute_tool_with_approval`, add:

```rust
let result = if execution_request.name == tool_types::ToolName::Workflow {
    execute_workflow_tool(
        config,
        cwd,
        events,
        sink,
        execution_request,
        task_registry,
        emit_deltas,
    )?
} else if execution_request.name == tool_types::ToolName::Subagent {
    ...
}
```

`execute_workflow_tool` parses `WorkflowInput`, launches a background thread, emits `workflow.started`, and returns `ToolResult::completed` with serialized `WorkflowOutput`.

For JSONL `exec`, join the background thread before `session.completed` so tests can see `workflow.result.available`. For TUI/server, keep the task running in the registry.

- [ ] **Step 5: Map workflow events in server**

In `map_runtime_event`, pass through workflow events as:

```rust
"workflow.started" => Some(json!({
    "event": "workflow_started",
    "taskId": payload["taskId"],
    "runId": payload["runId"],
    "workflowName": payload["workflowName"]
})),
```

Add mappings for `workflow.result.available`, `workflow.completed`, and `workflow.failed`.

- [ ] **Step 6: Run tests**

Run: `cargo test --test workflow_tool_contract`

Expected: all workflow tool tests pass.

- [ ] **Step 7: Run agent regression tests**

Run: `cargo test --test subagent_contract --test agent_loop_contract --test session_server_contract`

Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add crates/orca-runtime/src/controller.rs crates/orca-runtime/src/server.rs tests/workflow_tool_contract.rs
git commit -m "feat(workflow): execute workflow tool in runtime"
```

---

### Task 11: Workflow CLI Commands

**Files:**
- Modify: `src/cli.rs`
- Test: `tests/workflow_cli_contract.rs`

**Interfaces:**
- Consumes: `WorkflowRunner`, `TaskRegistry`.
- Produces: `orca workflow run <script-or-name>`, `list`, `show`, `stop`, `resume`.

- [ ] **Step 1: Write failing CLI tests**

Create `tests/workflow_cli_contract.rs`:

```rust
use std::fs;
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

#[test]
fn workflow_run_command_executes_script() {
    let temp = tempdir().unwrap();
    let script = temp.path().join("audit.js");
    fs::write(
        &script,
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default await agent('inspect repo');",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            "--cwd",
            temp.path().to_str().unwrap(),
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");

    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "async_launched");
    assert_eq!(value["workflowName"], "audit");
}

#[test]
fn workflow_run_named_script_resolves_project_workflow() {
    let temp = tempdir().unwrap();
    let dir = temp.path().join(".claude/workflows");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("audit.js"),
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default await agent('inspect repo');",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            "--cwd",
            temp.path().to_str().unwrap(),
            "audit",
        ])
        .output()
        .expect("run workflow");

    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["workflowName"], "audit");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test workflow_cli_contract`

Expected: clap reports unknown `workflow` subcommand.

- [ ] **Step 3: Add CLI subcommands**

In `src/cli.rs`, add:

```rust
Workflow(WorkflowArgs),
```

Define:

```rust
#[derive(Debug, Parser)]
struct WorkflowArgs {
    #[command(subcommand)]
    command: WorkflowCommand,
}

#[derive(Debug, Subcommand)]
enum WorkflowCommand {
    Run(WorkflowRunArgs),
    List,
    Show { task_id: String },
    Stop { task_id: String },
    Resume { run_id: String },
}
```

`WorkflowRunArgs` should accept `--cwd`, `--provider`, `--model`, `--api-key`, `--base-url`, `--args <json>`, `--resume-from-run-id`, and a required `script_or_name`.

Print `WorkflowOutput` as JSON to stdout.

- [ ] **Step 4: Run CLI tests**

Run: `cargo test --test workflow_cli_contract`

Expected: both CLI tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs tests/workflow_cli_contract.rs
git commit -m "feat(workflow): add workflow cli commands"
```

---

### Task 12: Saved Workflow Commands And Config Switches

**Files:**
- Modify: `src/cli.rs`
- Modify: `crates/orca-runtime/src/workflow/script.rs`
- Modify: `README.md`
- Test: `tests/workflow_cli_contract.rs`

**Interfaces:**
- Consumes: project/user workflow resolution.
- Produces: slash-like saved workflow execution for command names.
- Produces: disabled workflows behavior.

- [ ] **Step 1: Add failing disabled workflow test**

Append to `tests/workflow_cli_contract.rs`:

```rust
#[test]
fn disable_workflows_setting_blocks_launch() {
    let temp = tempdir().unwrap();
    fs::write(temp.path().join("config.toml"), "disableWorkflows = true\n").unwrap();
    let script = temp.path().join("audit.js");
    fs::write(
        &script,
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default 'blocked';",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", temp.path())
        .args(["workflow", "run", script.to_str().unwrap()])
        .output()
        .expect("run workflow");

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("workflows are disabled"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test workflow_cli_contract disable_workflows_setting_blocks_launch`

Expected: workflow still runs or config field is ignored.

- [ ] **Step 3: Enforce config switches**

Before CLI or tool launch:

```rust
if !config.workflows.enabled {
    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "workflows are disabled"));
}
```

For prompt keyword trigger, add detection but do not auto-launch until Task 13:

```rust
pub fn contains_workflow_keyword(prompt: &str, config: &WorkflowConfig) -> bool {
    config.keyword_trigger_enabled && prompt.split_whitespace().any(|word| word == "ultracode")
}
```

- [ ] **Step 4: Document saved workflow paths**

Update `README.md` with:

````markdown
## Workflows

`orca workflow run <script-or-name>` runs a Claude Code-style dynamic workflow.
Named workflows resolve from the nearest `.claude/workflows/` directory first,
then `~/.claude/workflows/`. Project workflows win over user workflows.

Workflow scripts are JavaScript modules beginning with:

```js
export const meta = { name: "audit", description: "Audit code", phases: ["scan"] };
```
````

- [ ] **Step 5: Run tests**

Run: `cargo test --test workflow_cli_contract`

Expected: all CLI workflow tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/cli.rs crates/orca-runtime/src/workflow/script.rs README.md tests/workflow_cli_contract.rs
git commit -m "feat(workflow): honor workflow settings"
```

---

### Task 13: Workflow Management View Foundation

**Files:**
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/commands/mod.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Test: existing TUI compile tests via `cargo test -p orca-tui`

**Interfaces:**
- Consumes: `BackgroundTaskSummary`.
- Produces: `/workflows` command route and basic list view showing workflow name, status, run id, phase count.

- [ ] **Step 1: Add TUI command type**

Add a command variant:

```rust
WorkflowList,
```

Map `/workflows` to this command in `commands/mod.rs`.

- [ ] **Step 2: Add app state**

In TUI app state:

```rust
pub enum PanelMode {
    Conversation,
    Workflows,
}

pub struct WorkflowPanelState {
    pub selected: usize,
    pub tasks: Vec<BackgroundTaskSummary>,
}
```

- [ ] **Step 3: Render basic list**

In `ui.rs`, render a full-width panel when `PanelMode::Workflows`:

```text
Workflows
audit  running  task-...
```

Avoid implementing drill-down in this task; the list must compile and display task summaries.

- [ ] **Step 4: Run TUI tests/compile**

Run: `cargo test -p orca-tui`

Expected: crate compiles and tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-tui/src/types.rs crates/orca-tui/src/app.rs crates/orca-tui/src/commands/mod.rs crates/orca-tui/src/ui.rs
git commit -m "feat(workflow): add workflow tui list view"
```

---

### Task 14: Final Verification And Documentation

**Files:**
- Modify: `README.md`
- Modify: `docs/superpowers/specs/2026-06-20-workflow-runtime-design.md` only if the implementation intentionally differs from spec.

**Interfaces:**
- Consumes: all previous tasks.
- Produces: verified workflow runtime and documented commands.

- [ ] **Step 1: Run targeted workflow tests**

Run:

```bash
cargo test --test workflow_types_contract --test workflow_events_contract --test workflow_script_contract --test workflow_host_contract --test workflow_runtime_contract --test workflow_tool_contract --test workflow_cli_contract
```

Expected: all workflow contract tests pass.

- [ ] **Step 2: Run existing contract tests**

Run:

```bash
cargo test --test subagent_contract --test approval_contract --test tool_contract --test agent_loop_contract --test session_server_contract
```

Expected: all existing contract tests pass.

- [ ] **Step 3: Run full workspace tests**

Run:

```bash
cargo test --workspace
```

Expected: all tests pass. If Node is missing, host tests must skip cleanly and report no failure.

- [ ] **Step 4: Run formatting and lint checks**

Run:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: formatting is clean and clippy reports no warnings.

- [ ] **Step 5: Manual smoke test**

Create `/tmp/orca-workflow-smoke.js`:

```js
export const meta = { name: "smoke", description: "Smoke workflow", phases: ["scan"] };
const result = await phase("scan", async () => agent("inspect repo"));
export default result;
```

Run:

```bash
cargo run -- workflow run --provider mock /tmp/orca-workflow-smoke.js
```

Expected: stdout is a JSON object with `status: "async_launched"`, `taskType: "local_workflow"`, `workflowName: "smoke"`, a `runId`, a `taskId`, a `scriptPath`, and a `transcriptDir`.

- [ ] **Step 6: Update README examples**

Ensure README includes:

```markdown
orca workflow run ./audit.js
orca workflow run audit --args '{"paths":["src"]}'
orca exec --approval-mode full-auto "ultracode: audit API endpoints"
```

Also state:

```markdown
Workflow resume is same-session scoped. Completed `agent()` calls with unchanged prompt and options are cached when resuming a stopped run.
```

- [ ] **Step 7: Commit**

```bash
git add README.md docs/superpowers/specs/2026-06-20-workflow-runtime-design.md
git commit -m "docs(workflow): document workflow runtime"
```

---

## Self-Review

Spec coverage:

- `Workflow` tool compatibility is covered by Tasks 1, 2, and 10.
- JavaScript script host and helpers are covered by Tasks 5, 6, 8, and 9.
- Background tasks are covered by Tasks 4, 10, 11, and 13.
- `scriptPath`, `script`, `name`, and saved workflow resolution are covered by Tasks 5, 11, and 12.
- Same-session resume and cache are covered by Task 8.
- 16 concurrent agents and 1,000 total agents are covered by Task 9.
- Events and server mapping are covered by Tasks 3 and 10.
- CLI management commands are covered by Task 11.
- TUI `/workflows` foundation is covered by Task 13.
- Documentation and final verification are covered by Task 14.

Type consistency:

- The plan consistently uses `WorkflowInput`, `WorkflowOutput`, `WorkflowMeta`, `WorkflowRunState`, `WorkflowRunner`, `WorkflowLaunchRequest`, `TaskRegistry`, and `BackgroundTaskSummary`.
- JSON compatibility fields use camelCase through serde: `taskId`, `taskType`, `workflowName`, `runId`, `transcriptDir`, `scriptPath`, `resumeFromRunId`.

Risk notes:

- Node-dependent tests skip when Node is unavailable.
- The first runtime implementation uses a Node host instead of embedding a JS engine.
- Cross-session resume is intentionally excluded to match the public same-session contract.
