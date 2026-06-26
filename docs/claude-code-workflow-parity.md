# Claude Code Dynamic Workflows Parity Plan

**Date**: 2026-06-26
**Project**: Orca (blade-deepseek)
**Goal**: Replicate the Claude Code Dynamic Workflows product loop in Orca before extending it with Orca-specific evidence and verifier features.

---

## Executive Summary

The target is not a workflow benchmark harness. The target is a product loop:

```text
user intent
-> Orca authors a JavaScript workflow
-> Orca previews phases, script, risks, and expected scale
-> user approves, edits, saves, or cancels
-> workflow runs in the background
-> /workflows exposes live runs, phase detail, agent detail, transcripts, cost, and controls
-> completed runs can be saved as reusable project or user workflows
-> saved workflows appear as slash commands and can be rerun with args
```

The current Orca runtime already has a meaningful base: JavaScript workflow execution, `phase()`, `agent()`, `parallel()`, background launches, workflow run directories, phase records, agent transcripts, evidence bundles, resume/cache, `/workflows`, `/agents`, workflow IPC tools, and named workflow script resolution. The missing product layer is the authoring, approval, reusable-command, and management experience around that runtime.

P0 should therefore focus on making workflow feel like a first-class agentic procedure, not on adding more stress-test assertions.

---

## What Claude Code Workflow Means

Claude Code Dynamic Workflows are valuable because they move orchestration out of the main conversational context and into a reusable script artifact.

Key product properties to replicate:

1. **Generated orchestration**: the assistant can decide that a task should become a workflow and author the JavaScript workflow script.
2. **Preview before execution**: the user sees the phases and raw script before the workflow starts.
3. **Background execution**: many agents can run while the main conversation stays small.
4. **Observable management**: the user can inspect runs, phases, agents, prompts, tool activity, results, tokens, and status.
5. **Reusable asset**: a good workflow can be saved and later invoked like a command.
6. **Scale by design**: intermediate results live in workflow state/script variables rather than flooding the parent session.

Orca should match that loop first. Evidence-first reporting, deterministic verifiers, and benchmark contracts are important, but they are Orca-specific reliability enhancements layered on top.

---

## Current Orca Baseline

### Already Present

- Workflow tool and input/output types:
  - `crates/orca-core/src/workflow_types.rs`
  - `crates/orca-core/src/tool_types.rs`
  - `crates/orca-runtime/src/controller.rs`
- JavaScript workflow runtime:
  - `crates/orca-runtime/src/workflow/host.mjs`
  - `crates/orca-runtime/src/workflow/host.rs`
  - `crates/orca-runtime/src/workflow/runner.rs`
- Script resolution and keyword detection:
  - `crates/orca-runtime/src/workflow/script.rs`
- Durable run state, evidence, cache, transcripts, and worker records:
  - `crates/orca-runtime/src/workflow/state.rs`
- Workflow events:
  - `crates/orca-core/src/event_schema.rs`
  - `crates/orca-core/src/event_sink.rs`
- TUI workflow surfaces:
  - `/workflows`
  - `/agents`
  - `crates/orca-tui/src/commands/mod.rs`
  - `crates/orca-tui/src/app.rs`
  - `crates/orca-tui/src/ui.rs`
- Workflow shared state tools:
  - `workflow_send_message`
  - `workflow_read_messages`
  - `workflow_clear_messages`
  - `workflow_create_task_list`
  - `workflow_claim_task`
  - `workflow_complete_task`
  - `workflow_list_tasks`

### Missing for Claude Code Workflow Parity

- No first-class workflow authoring flow from a natural-language task.
- No approval preview that shows generated phases and raw JavaScript before launch.
- No edit/save/cancel loop for a generated workflow.
- Existing `/workflows` is useful but not yet a full management panel for script, run controls, and saved commands.
- Named workflows exist, but reusable workflow discovery is not yet treated as a slash-command product surface.
- Final reporting can still over-trust agent text unless grounded in workflow state/evidence.

---

## Product Principles

1. **Workflow is an artifact**
   - A workflow script is a durable, reviewable asset.
   - It can be generated, approved, run, inspected, saved, and rerun.

2. **Runtime owns orchestration**
   - The main assistant should not manually coordinate dozens of agents in conversation.
   - The workflow script owns loops, fan-out, branches, and intermediate results.

3. **User remains in control**
   - Generated scripts need preview and approval.
   - Risky workflows must be editable or cancellable before execution.

4. **Saved workflows become commands**
   - Project workflows live under `.orca/workflows/`.
   - User workflows live under `~/.orca/workflows/`.
   - Saved workflows should be invocable from the slash menu.

5. **Reliability comes after parity, not instead of parity**
   - P0 is parity with the Claude Code product loop.
   - P1/P2 can add stronger evidence, verifier, and benchmark semantics.

---

## Target User Journeys

### Journey A: Generate and Run a One-Off Workflow

```text
User: 用 workflow 审计认证模块，分 8 个 agent 并行看代码，再汇总风险

Orca:
1. Detects workflow intent.
2. Authors a workflow script using phase(), parallel(), and agent().
3. Shows a preview:
   - name
   - description
   - phases
   - expected agent count
   - max concurrency
   - file mutation risk
   - raw script
4. User chooses Run.
5. Workflow runs in background.
6. Main session receives concise progress and completion notifications.
7. User opens /workflows to inspect details.
```

### Journey B: Edit Before Running

```text
User sees generated workflow preview.
User chooses Edit.
Orca opens the generated script in the composer or an edit panel.
User changes target files, agent count, or phase names.
User approves.
Workflow launches from the edited script.
```

### Journey C: Save Successful Workflow as Command

```text
Workflow completes.
Orca asks whether to save it.
User saves as project workflow "security-audit".
Orca writes .orca/workflows/security-audit.js.
Slash menu now offers /workflow:security-audit or /security-audit.
```

### Journey D: Rerun with Arguments

```text
User: /workflow:security-audit target=crates/orca-runtime/src/auth

Orca resolves the named workflow.
Args are validated.
Workflow launches with args passed to the JS runtime.
```

---

## P0 Scope

P0 delivers a usable Claude Code workflow parity loop.

### P0.1 Workflow Authoring

Add an authoring path where Orca can turn a user task into a workflow script.

Trigger conditions:

- User explicitly says `workflow`, `workflows`, `多 agent`, `并行 agent`, `ultracode`, or asks to run a large multi-phase audit/implementation.
- Existing `contains_workflow_keyword()` remains useful, but it should route to authoring preview when there is no explicit script/name/path.

Expected generated script shape:

```js
export const meta = {
  name: "repo-auth-audit",
  description: "Parallel audit of authentication-related code",
  phases: ["scan", "review", "report"]
};

const scanResults = await phase("scan", async () => parallel([
  agent("Inspect auth middleware and report concrete risks.", { team: "scanner" }),
  agent("Inspect session handling and report concrete risks.", { team: "scanner" }),
  agent("Inspect permission checks and report concrete risks.", { team: "scanner" })
]));

const reviewResults = await phase("review", async () => parallel([
  agent(`Review these findings and identify duplicates:\n${JSON.stringify(scanResults)}`, { team: "reviewer" })
]));

export default await phase("report", async () =>
  agent(`Write a concise final report from:\n${JSON.stringify({ scanResults, reviewResults })}`, {
    team: "reporter"
  })
);
```

Implementation options:

- Minimal P0: the main assistant authors the script as tool input and the runtime stores it as pending workflow draft.
- Stronger P0: add a dedicated `WorkflowDraft` tool or controller path that returns a preview object before launch.

Recommendation: start with a dedicated draft path. It creates cleaner UX and avoids overloading `Workflow` launch semantics.

### P0.2 Preview and Approval

Add a pending workflow draft state with these fields:

```rust
pub struct WorkflowDraft {
    pub draft_id: String,
    pub session_id: String,
    pub cwd: String,
    pub name: String,
    pub description: String,
    pub phases: Vec<String>,
    pub script: String,
    pub estimated_agent_count: Option<u32>,
    pub max_configured_concurrent_agents: u32,
    pub source_mutation_risk: WorkflowSourceMutationRisk,
    pub created_at_ms: i64,
}
```

Preview actions:

- `run`: launches the draft.
- `edit`: lets the user edit the JavaScript before launch.
- `save`: writes a reusable workflow without running.
- `cancel`: discards the draft.

TUI preview should show:

- title/name
- description
- phase list
- expected agent count when statically inferable
- configured concurrency cap
- source mutation risk
- raw script with scroll support

### P0.3 Background Run Notifications

Keep existing background execution, but make notifications product-quality:

- On launch: show workflow name, run id, phase count, transcript dir.
- During run: show phase changes and agent count updates without dumping full JSON.
- On completion: show status, failed agents, cost, duration, and how to inspect details.

Final notifications must never print a full 10KB JSON summary.

### P0.4 `/workflows` Management Surface

Upgrade `/workflows` from a task list view into a run manager.

Required views:

- Recent runs list
  - status
  - name
  - run id short hash
  - current phase
  - agent counts
  - elapsed time
  - estimated cost
- Run detail
  - metadata
  - raw script path
  - launch input
  - phase table
  - agent table
  - failures
  - final summary
- Agent detail
  - prompt
  - team
  - status
  - transcript path
  - output
  - usage/cost

Controls:

- stop
- rerun
- resume from run
- save as reusable workflow

Pause/resume can remain P1 if stop/rerun/resume-from-run are solid in P0.

### P0.5 Reusable Workflow Commands

Treat saved workflows as command-like assets.

Locations:

- Project: `.orca/workflows/<name>.js`
- User: `~/.orca/workflows/<name>.js`

Resolution order:

1. Project workflow
2. User workflow
3. Built-in workflow, if added later

Command surface:

- `/workflow:<name>`
- optional alias `/<name>` only when there is no collision with built-in slash commands

Arguments:

Workflow scripts can export an args schema:

```js
export const args = {
  target: { type: "string", required: true },
  maxAgents: { type: "number", required: false, default: 8 }
};
```

Runtime passes parsed args to the workflow script as the existing `args` input.

### P0.6 Report from State, Not Freeform Confidence

The final report can be written by an agent, but the run status line must be deterministic:

```text
Workflow completed
Agents: 18 completed, 1 failed, 0 running
Phases: 4 completed, 1 failed with fallback
Max observed concurrency: 6 / 16
Inspect: /workflows -> <run id>
```

This avoids repeating the previous failure mode where a freeform assistant said "fully resolved" while the evidence showed unresolved caveats.

---

## P1 Scope

P1 deepens parity after the core loop works.

### P1.1 Script Editing UX

Support a first-class edit loop:

- edit generated JavaScript before launch
- re-parse metadata after edit
- re-render preview
- launch edited script

### P1.2 Run Controls

Add richer controls:

- pause workflow
- resume workflow
- restart failed agent
- restart phase
- clone run as draft

### P1.3 Workflow History and Search

Add workflow-specific history:

- search by workflow name
- search by run id
- filter by failed/completed/running
- show saved workflow source
- show associated session id

### P1.4 Better Saved Workflow Metadata

Support optional metadata:

```js
export const meta = {
  name: "security-audit",
  description: "Parallel security audit",
  phases: ["scan", "review", "report"],
  tags: ["audit", "security"],
  version: "1"
};
```

### P1.5 Workflow Authoring Prompt Library

Add internal authoring instructions so generated workflows:

- keep phase count small and meaningful
- avoid dumping huge intermediate results into final summaries
- use teams consistently
- include explicit final report phase
- prefer worktree isolation for source-mutating implementation workflows

---

## P2 Scope: Orca Differentiation

P2 adds reliability features beyond Claude Code parity.

### P2.1 Evidence Contracts

Add structured workflow contracts:

- required tool calls
- expected tool failures
- expected MCP failures
- expected file mutation policy
- expected concurrency threshold

### P2.2 Deterministic Verifier

Add a deterministic verifier that reads `evidence.json`, `task-lists.json`, `mailbox.json`, and transcripts.

Verifier outputs:

- `proven`
- `not_proven`
- `failed`
- `completed_with_failures`

### P2.3 Concurrency Barrier

Add a barrier primitive for true max-concurrency testing.

Example:

```js
await phase("concurrency", () => parallel(
  Array.from({ length: 16 }, (_, index) =>
    agent(`hold-${index}`, { barrier: "concurrency-16", minHoldMs: 3000 })
  )
));
```

### P2.4 Tool and MCP Event Evidence

Record per-agent tool/MCP calls in `WorkflowEvidenceAgent`.

This makes claims like "tool failure tested" verifiable without reading agent self-reports.

---

## Architecture

### New Concepts

#### Workflow Draft

A workflow draft is a generated script that has not been launched yet.

Responsibilities:

- store generated script
- store parsed metadata
- store preview estimates
- allow run/edit/save/cancel

Suggested storage:

```text
.orca/workflow-sessions/<session-id>/workflow-drafts/<draft-id>/
  draft.json
  script.js
```

#### Workflow Command Registry

A registry that loads saved workflows and exposes them to slash completion.

Responsibilities:

- discover `.orca/workflows/*.js`
- discover `~/.orca/workflows/*.js`
- parse `meta`
- parse optional `args`
- handle command-name collisions

#### Workflow Run Manager

An upgraded TUI/controller layer for listing and operating on workflow runs.

Responsibilities:

- load runs from `WorkflowStateStore`
- map run state into TUI summaries
- expose controls
- avoid printing large JSON blobs

---

## File-Level Implementation Map

### Core Types

- Modify `crates/orca-core/src/workflow_types.rs`
  - Add `WorkflowDraft`
  - Add `WorkflowDraftOutput`
  - Add `WorkflowSourceMutationRisk`
  - Add reusable workflow metadata/args types if shared across runtime and TUI

- Modify `crates/orca-core/src/task_types.rs`
  - Add draft/run summary fields needed by `/workflows`

### Runtime

- Modify `crates/orca-runtime/src/workflow/script.rs`
  - Keep current script/name/path resolution.
  - Add reusable workflow discovery helpers if they fit the existing module.
  - Add metadata extraction for optional `args`.

- Create `crates/orca-runtime/src/workflow/draft.rs`
  - Own draft persistence.
  - Write `draft.json` and `script.js`.
  - Load, update, delete, and launch drafts.

- Modify `crates/orca-runtime/src/workflow/runner.rs`
  - Allow launching from a draft.
  - Ensure final summaries are concise.
  - Preserve existing run/evidence behavior.

- Modify `crates/orca-runtime/src/controller.rs`
  - Add draft creation and draft action handling.
  - Continue using `WorkflowRunner` for actual execution.
  - Route named workflow command invocations to existing workflow launch input.

### Tools

- Modify `crates/orca-tools/src/registry.rs`
  - Add or update model-visible workflow tooling for draft creation if needed.
  - Keep the existing `Workflow` tool for actual launch.

Potential tool split:

```text
WorkflowDraft: create previewable draft
Workflow: launch inline/script/name/path workflow
WorkflowDraftAction: run/edit/save/cancel draft
```

If the tool surface should stay smaller, `Workflow` can accept `mode: "draft" | "run"`, but separate tools are easier to reason about.

### TUI

- Modify `crates/orca-tui/src/commands/mod.rs`
  - Keep `/workflows`.
  - Add dynamic saved workflow slash entries or route `/workflow:<name>`.

- Modify `crates/orca-tui/src/app.rs`
  - Add draft preview state.
  - Add run/edit/save/cancel actions.
  - Add `/workflows` navigation state for list/detail/agent detail.

- Modify `crates/orca-tui/src/ui.rs`
  - Render workflow draft preview.
  - Render improved workflow run manager.
  - Render concise completion notifications.

- Modify `crates/orca-tui/src/types.rs`
  - Add view models for draft preview, workflow run detail, and agent detail if current structs are too narrow.

### Tests

- Modify `tests/workflow_runtime_contract.rs`
  - Draft persistence and launch from draft.
  - Named workflow command resolution.
  - Args parsing and validation.
  - Concise summary regression.

- Modify or add TUI command tests
  - Slash parsing for `/workflow:<name>`.
  - Saved workflow collision behavior.

- Add fixtures under `tests/fixtures/workflows/`
  - simple generated workflow
  - workflow with args
  - workflow with phases and multiple agents

---

## P0 Implementation Milestones

### Milestone 1: Draft Persistence and Preview Data

Deliverable:

- Runtime can create a draft from generated script.
- Draft can be loaded from disk.
- Draft preview includes metadata, phases, script, estimated agent count, concurrency cap, and mutation risk.

Validation:

```bash
cargo test --test workflow_runtime_contract workflow_draft -- --nocapture
```

### Milestone 2: Launch Drafts

Deliverable:

- Draft can be launched without copying script by hand.
- Run stores original draft id in launch input or metadata.
- Existing run/evidence/transcript behavior remains unchanged.

Validation:

```bash
cargo test --test workflow_runtime_contract workflow_draft_launch -- --nocapture
```

### Milestone 3: TUI Preview Approval

Deliverable:

- Generated workflow draft renders before launch.
- User can run, save, cancel.
- Edit can initially be implemented as "copy script into composer" if a full editor is too large for P0.

Validation:

```bash
cargo test -p orca-tui workflow -- --nocapture
```

### Milestone 4: Saved Workflow Commands

Deliverable:

- `.orca/workflows/*.js` and `~/.orca/workflows/*.js` are discovered.
- `/workflow:<name>` launches a saved workflow.
- Name collisions with built-in slash commands are handled deterministically.

Validation:

```bash
cargo test --test workflow_runtime_contract named_workflow -- --nocapture
cargo test -p orca-tui commands -- --nocapture
```

### Milestone 5: `/workflows` Run Manager Upgrade

Deliverable:

- `/workflows` shows recent runs and active runs.
- Run detail shows phases, agents, failures, script path, launch input, cost, and duration.
- Agent detail shows prompt, output, transcript path, and usage.

Validation:

```bash
cargo test -p orca-tui workflow -- --nocapture
cargo test -p orca-runtime workflow -- --nocapture
```

### Milestone 6: Concise Completion Reporting

Deliverable:

- Workflow completion notification never dumps full JSON.
- Completion message includes status, agent counts, failures, observed concurrency, and inspection path.

Validation:

```bash
cargo test -p orca-runtime workflow -- --nocapture
```

---

## Acceptance Criteria

P0 is complete when all of these are true:

1. A user can ask Orca to use workflow for a complex task and receive a generated workflow preview before execution.
2. The preview shows phases and raw JavaScript.
3. The user can run or cancel the draft.
4. The launched workflow runs in the background and records state under `.orca/workflow-sessions/`.
5. `/workflows` can show the run, phases, agents, transcripts, usage, failures, and final status without reading raw JSON files.
6. The user can save a workflow under `.orca/workflows/`.
7. Saved workflows can be invoked by slash command.
8. Completion output is concise and evidence-derived at the status line.
9. Existing workflow runtime contract tests continue to pass.
10. Existing named workflow behavior remains backward compatible.

---

## Non-Goals for P0

- Full deterministic verifier.
- Tool/MCP call evidence contracts.
- Barrier-based concurrency benchmark.
- Full in-terminal JavaScript editor.
- Cloud workflow sharing.
- Plugin marketplace for workflows.
- A web UI.

These are valid later projects, but they should not block Claude Code workflow parity.

---

## Risks and Decisions

### Risk: Overloading the Existing `Workflow` Tool

If `Workflow` handles inline launch, named launch, draft creation, and draft actions, the schema will become ambiguous.

Decision:

- Prefer a small draft-specific API or tool path.
- Keep `Workflow` as the launcher.

### Risk: Slash Command Collision

Saved workflow names can collide with built-in commands like `/model` or `/history`.

Decision:

- `/workflow:<name>` is always valid.
- `/<name>` alias is allowed only when there is no built-in collision.

### Risk: Workflow Authoring Hallucinates Unsupported DSL

Generated JavaScript may use non-existent helpers.

Decision:

- Authoring instructions must include the exact DSL surface:
  - `phase`
  - `agent`
  - `parallel`
  - `pipeline`
  - mailbox helpers
  - task-list helpers
- Draft preview must parse metadata before approval.
- Launch must fail fast with a readable script error.

### Risk: Huge Final Summaries

Workflow scripts can export a large object.

Decision:

- Runtime should derive concise summaries from run state.
- Full exported object can remain in run artifacts but should not be printed as the user-facing completion summary.

### Risk: Source Mutation During Preview or Test Runs

Generated workflow may edit source files.

Decision:

- Preview should classify mutation risk from prompt/script heuristics.
- Source-mutating workflow drafts should recommend worktree isolation.
- Hard source-mutation contracts belong to P2.

---

## Recommended Next Step

Start with Milestone 1 and Milestone 2 in one branch:

```text
P0A: workflow draft persistence + launch from draft
```

This creates the missing product primitive without forcing the whole TUI manager redesign into the first patch. Once drafts exist, TUI preview, saved commands, and `/workflows` upgrades become smaller follow-up tasks.
