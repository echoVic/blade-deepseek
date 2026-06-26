# Workflow Harness Evidence Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Orca workflow benchmark and harness output evidence-bound, fail-closed, and safe to persist before increasing workflow scale claims.

**Architecture:** Runtime owns the canonical workflow evidence bundle, report rendering consumes only that bundle, and missing evidence is a blocker instead of a soft downgrade. Session transcript persistence gets a redaction layer so harness runs can be stored and inspected without leaking provider keys or tool secrets.

**Tech Stack:** Rust workspace, `serde` JSON artifacts, existing `WorkflowRunner`, `WorkflowStateStore`, `SessionWriter`, integration tests under `tests/`.

## Global Constraints

- Preserve DeepSeek-native behavior; Codex and Claude Code are references, not designs to copy verbatim.
- Use TDD for P0: write failing tests first, verify RED, implement minimal runtime support, verify GREEN.
- Do not mutate unrelated dirty or untracked files, including `docs/reports/workflow-limit-benchmark.*`.
- Treat `state.json`, `agent_cache.json`, transcript files, and final runtime status as the source of truth for workflow reports.
- A workflow/benchmark report must never claim success when no verified workflow evidence exists.

---

## Priority Overview

| Priority | Theme | Why It Comes First | Delivery Bar |
| --- | --- | --- | --- |
| P0 | Evidence truth chain and secret safety | The current harness can produce persuasive text without a runtime-owned proof artifact. That makes benchmark results hard to trust. | Reports are generated from runtime evidence only; missing evidence blocks success; transcripts are redacted before persistence. |
| P1 | Reliability under higher fan-out | Once evidence is trustworthy, push concurrency/retry/resume behavior harder and make partial failures diagnosable. | Stress runs prove bounded concurrency, retry semantics, resume/cache behavior, and fail-open vs fail-closed phase policy. |
| P2 | Productized workflow UX | After correctness and scale, improve operator ergonomics and comparison against Claude Code workflow. | `/workflows`, `/agents`, docs, and benchmark prompts show clear live state, evidence links, and next actions. |

## Existing Anchors

- `crates/orca-core/src/config/mod.rs:63-67` defines current workflow limits: 8 read-parallel tools, 16 concurrent workflow agents, 1000 agents per run, one retry by default, retry cap 5.
- `crates/orca-core/src/workflow_types.rs:91-108` defines `WorkflowRunState`; it has status, phases, counts, digests, summary, and error, but no immutable evidence bundle.
- `crates/orca-runtime/src/workflow/state.rs:149-235` owns per-run paths and persistence for `state.json`, launch input, worker state, mailbox, task lists, and agent cache.
- `crates/orca-runtime/src/history.rs:89-112` defines persisted session JSONL record variants; `SessionWriter` currently writes messages directly via `write_record`.
- `docs/agent-workflow-benchmark.md:16-31` contains benchmark claims that need to become evidence-derived instead of hand-synthesized.

## P0: Evidence Truth Chain

### Task 1: Add Runtime-Owned `WorkflowEvidenceBundle`

**Files:**
- Modify: `crates/orca-core/src/workflow_types.rs`
- Modify: `crates/orca-runtime/src/workflow/state.rs`
- Test: `tests/workflow_script_contract.rs`

**Interfaces:**
- Produces: `WorkflowEvidenceBundle`, `WorkflowEvidenceIdentity`, `WorkflowEvidenceAgent`, `WorkflowEvidencePhase`.
- Produces: `WorkflowStateStore::evidence_path(run_id: &str) -> PathBuf`.
- Produces: `WorkflowStateStore::write_evidence_bundle(bundle: &WorkflowEvidenceBundle) -> io::Result<()>`.
- Produces: `WorkflowStateStore::load_evidence_bundle(run_id: &str) -> io::Result<WorkflowEvidenceBundle>`.

- [ ] **Step 1: Write failing round-trip test**

Add a test in `tests/workflow_script_contract.rs` that creates a `WorkflowRunState`, records one completed agent and one failed/retried agent in the existing agent cache, writes a `WorkflowEvidenceBundle`, then loads it back and asserts:

- `run_id`, `task_id`, `session_id`, `cwd`, `workflow_name`, `script_digest`, `args_digest`, and final `status` match `WorkflowRunState`.
- `total_agent_count` matches the runtime state.
- phase status/error/fallback fields are present.
- agent rows include `call_id`, `call_path`, `team`, `status`, `attempt`, `max_attempts`, `previous_errors`, `input_hash`, `transcript_path`, `started_at_ms`, `completed_at_ms`, and usage when available.

Run:

```bash
cargo test --test workflow_script_contract workflow_evidence_bundle_round_trips_state_and_agent_rows
```

Expected RED: compile failure because the evidence structs and store methods do not exist.

- [ ] **Step 2: Implement the evidence structs**

Add serializable structs in `crates/orca-core/src/workflow_types.rs` with `#[serde(rename_all = "camelCase")]`. Keep fields factual and derived from existing runtime records; do not add scored or interpretive fields like "trustScore".

- [ ] **Step 3: Implement evidence persistence**

In `WorkflowStateStore`, add `evidence.json` beside `state.json` under each run directory. Use existing pretty JSON helpers and make `load_evidence_bundle` fail normally with `io::ErrorKind::NotFound` when evidence is absent.

- [ ] **Step 4: Verify GREEN**

Run:

```bash
cargo test --test workflow_script_contract workflow_evidence_bundle_round_trips_state_and_agent_rows
```

Expected GREEN.

### Task 2: Emit Evidence At Workflow Completion And Resume

**Files:**
- Modify: `crates/orca-runtime/src/workflow/runner.rs`
- Modify: `crates/orca-runtime/src/workflow/state.rs`
- Test: `tests/workflow_runtime_contract.rs`

**Interfaces:**
- Consumes: Task 1 evidence structs and store methods.
- Produces: `WorkflowStateStore::build_evidence_bundle(state: &WorkflowRunState, identity: WorkflowEvidenceIdentity) -> io::Result<WorkflowEvidenceBundle>`.
- Runner writes evidence after terminal workflow status is persisted and after agent rows have been updated.

- [ ] **Step 1: Write failing integration test**

Extend an existing `WorkflowRunner` test in `tests/workflow_runtime_contract.rs` so it captures `session_dir`, loads `WorkflowStateStore::new(session_dir.join("workflow-runs"))`, loads `evidence.json` by `run_id`, and asserts the evidence contains the completed runtime status and observed agent rows.

Run:

```bash
cargo test --test workflow_runtime_contract workflow_runner_executes_agent_and_writes_evidence
```

Expected RED: evidence is not written by the runner.

- [ ] **Step 2: Build evidence from current run state**

Use `WorkflowRunState`, existing phase data, and agent summaries/cache records. Include identity fields that are cheap and stable:

- `app_version` from `RunConfig.app_version`.
- `binary_path` from `std::env::current_exe()` when available.
- `generated_at_ms` from runtime clock.
- `evidence_version = 1`.

- [ ] **Step 3: Write evidence on every terminal path**

Ensure evidence is written for `completed`, `failed`, and `cancelled` terminal states. If evidence writing fails, surface that as a runtime error in the caller path used for benchmark/report execution.

- [ ] **Step 4: Verify GREEN**

Run:

```bash
cargo test --test workflow_runtime_contract workflow_runner_executes_agent_and_writes_evidence
```

Expected GREEN.

### Task 3: Make Reports Evidence-Bound And Fail Closed

**Files:**
- Create: `crates/orca-runtime/src/workflow/report.rs`
- Modify: `crates/orca-runtime/src/workflow/mod.rs`
- Test: `tests/workflow_script_contract.rs`

**Interfaces:**
- Produces: `render_evidence_markdown(bundle: &WorkflowEvidenceBundle) -> String`.
- Produces: `render_evidence_json(bundle: &WorkflowEvidenceBundle) -> serde_json::Value`.
- Produces: `render_report_for_run(store: &WorkflowStateStore, run_id: &str) -> io::Result<WorkflowEvidenceReport>`.
- Missing evidence returns an error containing `no verified workflow evidence`.

- [ ] **Step 1: Write failing report contract tests**

Add tests that:

- build a bundle with `total_agent_count = 3` and assert Markdown/JSON report shows `3`, not an externally supplied or requested number.
- call `render_report_for_run` for a run without `evidence.json` and assert it returns an error containing `no verified workflow evidence`.

Run:

```bash
cargo test --test workflow_script_contract workflow_report_is_bound_to_evidence
cargo test --test workflow_script_contract workflow_report_blocks_without_verified_evidence
```

Expected RED: report module does not exist.

- [ ] **Step 2: Implement report rendering**

Render only facts present in `WorkflowEvidenceBundle`: run identity, status, phase table, agent status counts, retry counts, elapsed timestamps, usage totals when available, and evidence file identity.

- [ ] **Step 3: Add no-silent-downgrade gate**

Any workflow benchmark/report code path that cannot load evidence must return a blocker result. It may include remediation text, but it must not produce a success report.

- [ ] **Step 4: Verify GREEN**

Run both report contract tests again.

### Task 4: Redact Secrets Before Session JSONL Persistence

**Files:**
- Modify: `crates/orca-runtime/src/history.rs`
- Test: `crates/orca-runtime/src/history.rs`

**Interfaces:**
- Produces: `redact_session_record(record: &SessionRecord) -> SessionRecord`.
- Produces: string redaction helper for persisted text fields.
- Redaction affects persisted JSONL only; in-memory model context remains unchanged for the active turn.

- [ ] **Step 1: Write failing redaction test**

Add a test that writes a message containing fake values like:

```text
ORCA_API_KEY=sk-test-redaction-1234567890
"DEEPSEEK_API_KEY": "sk-test-redaction-json-1234567890"
password=super-secret-test-password
```

Then read the raw session JSONL and assert:

- raw secret values are absent.
- `<redacted>` appears.
- non-secret prose remains intact.

Run:

```bash
cargo test -p orca-runtime history::tests::writer_redacts_secrets_before_persisting_transcript
```

Expected RED: secrets are currently serialized by `write_record`.

- [ ] **Step 2: Redact all persistence paths**

Apply redaction inside `write_record_line` and the append path used by `write_record`, so normal appends and rewrites/compression share the same protection.

- [ ] **Step 3: Verify GREEN**

Run:

```bash
cargo test -p orca-runtime history::tests::writer_redacts_secrets_before_persisting_transcript
cargo test -p orca-runtime history
```

Expected GREEN.

## P1: Reliability Under Higher Fan-Out

### Task 5: Stress-Test Concurrency, Retries, And Resume

**Files:**
- Modify: `tests/workflow_runtime_contract.rs`
- Create: `.orca/workflows/stress-evidence.js`
- Modify: `docs/agent-workflow-benchmark.md`

**Delivery:**
- A deterministic stress workflow launches at least 16 short agents and proves max observed concurrency never exceeds the configured limit.
- A second run proves completed/cached rows are reused on resume instead of re-running every child.
- Retry rows include previous errors and attempt counts in evidence.

**Validation Commands:**

```bash
cargo test --test workflow_runtime_contract workflow_respects_configured_concurrency_in_evidence
cargo test --test workflow_runtime_contract workflow_resume_reuses_evidence_bound_cached_agents
```

### Task 6: Separate Phase Failure Policy From Agent Failure Policy

**Files:**
- Modify: `crates/orca-runtime/src/workflow/runner.rs`
- Modify: `crates/orca-core/src/workflow_types.rs`
- Test: `tests/workflow_runtime_contract.rs`

**Delivery:**
- Evidence distinguishes `agent_failed`, `phase_failed_continue`, `phase_failed_blocked`, and `workflow_failed`.
- A failed child with `fallback: "continue"` does not masquerade as an all-green run.
- Final report shows both final workflow status and contained failures.

### Task 7: MCP And Tool Failure Diagnostics For Child Agents

**Files:**
- Modify: `crates/orca-runtime/src/workflow/runner.rs`
- Modify: `crates/orca-mcp/src/lib.rs` or existing MCP client files after inspection.
- Test: focused MCP/workflow tests under existing test layout.

**Delivery:**
- Child-agent evidence records tool/MCP failure class, retryability, and whether the retry was attempted.
- Tool failure does not terminate unrelated independent workflow branches unless phase policy requires fail-closed behavior.

## P2: Productized Workflow UX And Competitive Positioning

### Task 8: Evidence Links In `/workflows` And `/agents`

**Files:**
- Modify TUI workflow/agent surfaces after locating the current command modules.
- Test current TUI state rendering tests.

**Delivery:**
- Each workflow run exposes evidence path, report status, agent counts, and warning badges for partial failure.
- Users can tell "finished with continued failures" apart from "fully succeeded".

### Task 9: Benchmark Prompt And Harness Pack

**Files:**
- Modify: `docs/agent-workflow-benchmark.md`
- Create: `docs/workflow-harness-prompts.md`
- Create or modify benchmark scripts after locating current harness entrypoints.

**Delivery:**
- Prompts explicitly require evidence bundle path and run id.
- Benchmark output includes reproducibility commands and refuses to summarize from memory.
- Claude Code comparison remains framed as reference behavior; Orca claims are backed by local runtime artifacts.

### Task 10: Operator Controls For Scale Tests

**Files:**
- Modify config docs and workflow config parsing after inspecting current config schema.
- Test: config parsing and runtime limit tests.

**Delivery:**
- Documented knobs for `max_concurrent_agents`, `max_agents_per_run`, retry caps, team budgets, and workflow isolation.
- Benchmark mode can set stricter fail-closed behavior without changing normal user workflow defaults.

## Execution Order

1. Finish P0 Tasks 1-4 in order. Do not start P1 until all P0 focused tests are green.
2. Run final P0 validation:

```bash
cargo test --test workflow_script_contract --test workflow_runtime_contract
cargo test -p orca-runtime history
```

3. Commit P0 as one coherent runtime hardening commit.
4. Start P1 stress work in a separate commit so evidence correctness and scale experiments are reviewable independently.

## Completion Criteria

- `evidence.json` exists for terminal workflow runs and is loadable by run id.
- Markdown/JSON workflow reports are generated from `WorkflowEvidenceBundle`.
- Missing evidence blocks success reports with a clear error.
- Persisted session JSONL does not contain fake API keys, tokens, passwords, or secrets from the redaction tests.
- Existing benchmark docs no longer make unsupported "completed" claims unless a runtime evidence path is cited.

