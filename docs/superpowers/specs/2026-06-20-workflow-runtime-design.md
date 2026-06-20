# Workflow Runtime Design

Date: 2026-06-20

## Goal

Implement an Orca workflow system that behaviorally matches the public Claude Code Dynamic workflows surface as closely as possible.

The target is not a generic DAG runner. The target is a background JavaScript orchestration runtime exposed through a `Workflow` tool, with session-scoped runs, resumable `agent()` calls, persisted scripts, progress inspection, and saved workflow commands.

Primary public references:

- Claude Code Dynamic workflows: https://code.claude.com/docs/en/workflows
- Run agents in parallel: https://code.claude.com/docs/en/agents
- Claude Code tools reference: https://code.claude.com/docs/en/tools-reference
- `@anthropic-ai/claude-agent-sdk@0.3.183` `sdk-tools.d.ts`

## Compatibility Contract

Orca will copy Claude Code's public behavior and naming where it is observable:

- Built-in tool name: `Workflow`
- Input fields: `script`, `name`, `description`, `title`, `args`, `scriptPath`, `resumeFromRunId`
- Output fields: `status`, `taskId`, `taskType`, `workflowName`, `runId`, `summary`, `transcriptDir`, `scriptPath`, `sessionUrl`
- Script shape: a JavaScript module beginning with a static `export const meta = { name, description, phases }`
- Runtime helpers: `agent()`, `parallel()`, `pipeline()`, and `phase()`
- Background execution: tool invocation returns after launch rather than waiting for the full run
- Session-scoped resume: `resumeFromRunId` can reuse completed agent results only in the active session
- Limits: at most 16 concurrent workflow agents and at most 1,000 total workflow agents per run
- Workflow script isolation: the script coordinates agents but does not directly read files, write files, or run shell commands
- No mid-run user input: only worker permission decisions can pause progress
- Final result delivery: completion produces one consolidated result that can be surfaced back into the session

Orca will not claim binary-level compatibility with Claude Code internals. Where the implementation details are private, Orca will match the public contract and document any intentional gaps.

## Current Project Fit

The existing workspace already has useful foundations:

- `orca-core` owns shared schemas and config.
- `orca-runtime` owns controller/session/history/subagent execution.
- `orca-tools` owns tool schemas and execution.
- The current `subagent` tool can run child agent loops and already supports parallel subagent batches.
- JSONL events already exist and can be extended without changing the human text mode.

The missing pieces are background task lifecycle, workflow run state, JavaScript host execution, and user-facing workflow management.

## Architecture

Add four new domains:

1. `orca-core::workflow_types`

   Shared types for `WorkflowInput`, `WorkflowOutput`, `WorkflowMeta`, `WorkflowRun`, `WorkflowNode`, `WorkflowPhase`, `WorkflowStatus`, and workflow events.

2. `orca-runtime::tasks`

   A session-scoped background task registry. It tracks task id, task type, status, description, output path, cancellation token, pause state, and optional workflow metadata.

3. `orca-runtime::workflow`

   The workflow runtime. It resolves scripts, persists invocations, runs the JavaScript host, executes workflow agent calls through the existing agent loop, caches completed calls, emits progress, and writes transcripts.

4. `orca-tools::workflow`

   The `Workflow` built-in tool. It validates input, asks launch approval when required, registers a background workflow task, and returns `WorkflowOutput`.

This keeps workflow orchestration out of `controller.rs` except for wiring the tool, event sink, config, and session task registry.

## Workflow Tool

The tool schema will accept:

- `script?: string`
- `name?: string`
- `description?: string`
- `title?: string`
- `args?: object`
- `scriptPath?: string`
- `resumeFromRunId?: string`

Resolution order:

1. `scriptPath`
2. `script`
3. `name`

`description` and `title` are accepted for compatibility but ignored for execution. The script `meta` block is authoritative.

The tool returns:

- `status: "async_launched"`
- `taskId`
- `taskType: "local_workflow"`
- `workflowName`
- `runId`
- `summary`
- `transcriptDir`
- `scriptPath`

`remote_launched` and `sessionUrl` are reserved for future remote execution. First implementation always runs local workflows.

## Script Resolution

`scriptPath`:

- Read the exact file.
- Persist a copy under the current session workflow directory.
- Use the original path as the editable source when the user passes it again.

`script`:

- Validate that it contains a static `meta` export.
- Persist the script to the session workflow directory.
- Return that persisted path in `WorkflowOutput.scriptPath`.

`name`:

- Resolve project workflows before user workflows.
- Project lookup walks from cwd upward to the repo root and checks `.claude/workflows/`.
- If several project workflows share a name, the closest directory to cwd wins.
- User lookup checks `~/.claude/workflows/`.
- Orca will also support `~/.orca/workflows/` as an alias, but `.claude/workflows/` is the compatibility path.

Saved workflow files are JavaScript modules. The slash-command integration comes after the runtime is stable.

## JavaScript Host

First implementation uses an external Node.js process with a small bundled host script. This avoids adding a heavy embedded JS engine while preserving JS semantics and TypeScript-transpilable authoring.

The host:

- Loads the workflow script as an ES module.
- Extracts static `meta`.
- Exposes global `args`.
- Exposes `agent`, `parallel`, `pipeline`, and `phase`.
- Communicates with Rust over JSONL stdin/stdout.
- Does not expose filesystem or shell helpers.
- Does not expose prompts or dialogs for arbitrary mid-run user input.

Workflow scripts can use normal JavaScript control flow, arrays, objects, async/await, and helper functions.

The host protocol is intentionally small:

- JS sends `agent_call` requests with call id, phase, prompt, opts, and a stable call position.
- Rust sends `agent_result`, `agent_error`, `cached_result`, or `cancelled`.
- JS sends `phase_started`, `phase_completed`, `workflow_completed`, and `workflow_failed`.

If Node is unavailable, `Workflow` fails with a clear runtime error. A later phase can embed QuickJS or V8 if the project wants a single static binary.

## Script API

`agent(prompt, opts?)`

- Spawns one workflow agent.
- Returns the agent's final text result.
- `opts` may include `description`, `model`, `agentType`, `tools`, `disallowedTools`, `phase`, and `maxTurns`.
- Cache key is derived from helper name, prompt, normalized opts, script identity, and call path.

`parallel(items)`

- Runs async items concurrently while respecting the 16-agent concurrency cap.
- Preserves input order in the returned array.
- Fails fast only for unrecoverable runtime errors. Agent failures are represented as structured errors and recorded in state.

`pipeline(items)`

- Runs functions or promises sequentially.
- Passes previous result to the next function when the item is callable.

`phase(name, body?)`

- Records progress grouping for `/workflows`.
- If `body` is provided, runs it within that phase context.
- If used as a marker, sets the current phase for following agent calls until changed.

The first version will focus on these APIs. Additional helpers discovered from official behavior can be added behind tests.

## Run State

Each launch creates:

```text
~/.orca/sessions/<session-id>/workflows/<run-id>/
  script.js
  state.json
  events.jsonl
  transcripts/
```

For compatibility with user-visible paths, the returned `transcriptDir` points to `transcripts/` and `scriptPath` points to the persisted `script.js`.

`state.json` records:

- run id
- task id
- session id
- cwd
- workflow name
- meta
- script digest
- args digest
- status
- phase list
- agent call records
- cache records
- total agent count
- timestamps
- final summary or error

Each agent record stores:

- call id
- stable call path
- prompt
- normalized opts
- input hash
- status
- output
- error
- token usage when available
- transcript path
- start and finish timestamps

## Resume Semantics

`resumeFromRunId` works only inside the same active Orca session.

On resume:

- Load the prior run state from the session task registry.
- Re-run the script to reconstruct execution.
- For each `agent()` call, if call path and input hash match a completed prior record, return the cached output immediately.
- If the call is new, changed, failed, interrupted, or incomplete, run it live.
- Preserve the original run id as the resume source and create a new task id for the resumed background task.

If Orca exits and restarts, prior workflow state can be inspected, but a new workflow launch starts fresh unless a later implementation adds cross-session resume. This matches the public same-session resume behavior.

## Background Task Lifecycle

Add a task registry with:

- `TaskCreate`: internal creation for Workflow launches
- `TaskList`: list active and completed tasks
- `TaskGet`: inspect a task
- `TaskStop`: cancel a task
- `TaskUpdate`: pause/resume/delete where applicable

The public tool surface can initially expose only `Workflow`; task commands and tools can be added incrementally, but the registry must exist from the start so workflow launch semantics are correct.

Workflow statuses:

- `queued`
- `running`
- `paused`
- `stopping`
- `stopped`
- `completed`
- `failed`
- `cancelled`

Pause prevents new agents from starting but lets currently running agents finish. Stop cancels pending and running agents through `CancelToken`.

## Agent Execution

Workflow agents reuse the existing `run_agent_loop`.

Differences from the current `subagent` tool:

- Workflow agents are background children, not foreground tool calls.
- Parent conversation receives the workflow launch output immediately.
- When the background run completes, the task registry stores the final consolidated result and emits a completion event so the session can surface that result.
- Intermediate agent results stay in workflow state and transcript files.
- Agents run with accept-edits style permissions and inherit the configured allowlist.
- Agent concurrency is controlled globally per workflow run.

The existing subagent code should be refactored so both the `subagent` tool and workflow runtime call a shared child-agent executor.

## Events

Add runtime JSONL event types:

- `workflow.started`
- `workflow.resumed`
- `workflow.phase.started`
- `workflow.phase.completed`
- `workflow.agent.started`
- `workflow.agent.cached`
- `workflow.agent.completed`
- `workflow.agent.failed`
- `workflow.paused`
- `workflow.stopped`
- `workflow.completed`
- `workflow.failed`
- `workflow.result.available`

Server protocol mapping can initially pass these through as workflow-specific events. The TUI can subscribe to the same event stream for `/workflows`.

## User Interfaces

Phase 1:

- `Workflow` built-in tool
- `orca workflow run <script-or-name>`
- JSONL events
- basic `orca workflow list/show/stop/resume`

Phase 2:

- TUI `/workflows` view
- phase and agent detail panes
- pause/resume/stop/restart keybindings
- save workflow to `.claude/workflows/` or `~/.claude/workflows/`

Phase 3:

- slash command loading from saved workflows
- `ultracode` keyword trigger
- config switches equivalent to `disableWorkflows`, `enableWorkflows`, and `workflowKeywordTriggerEnabled`

## Approval And Permissions

Workflow launch follows the session permission mode:

- interactive modes can ask before launch and show workflow name, description, phases, and script path
- non-interactive JSONL/server modes start immediately if permission rules allow `Workflow`

After launch:

- `agent()` calls do not ask for separate launch approval
- agent tool calls follow configured permission rules
- background prompts that would require unavailable interactive approval are denied or fail according to policy

This mirrors the public distinction between approving the workflow plan and handling worker tool permissions.

## Limits

Hard defaults:

- `max_concurrent_agents = 16`
- `max_agents_per_run = 1000`
- `max_script_size_bytes = 1 MiB`
- `max_agent_prompt_bytes = 64 KiB`

Config can lower the limits. Raising public compatibility limits should require an explicit setting.

## Error Handling

Validation errors fail the `Workflow` tool before task creation:

- no script, name, or scriptPath
- missing script file
- named workflow not found
- invalid static meta block
- unsupported meta shape
- script too large

Runtime errors fail the background task:

- JS host spawn failure
- JS syntax or module load failure
- helper protocol violation
- agent cap exceeded
- all retries exhausted

Agent failures are recorded per agent. The script may catch them if represented as rejected promises. Uncaught failures fail the workflow task.

## Testing

Use mock provider tests for deterministic workflow behavior:

- tool schema accepts official `WorkflowInput` fields
- script is persisted and returned as `scriptPath`
- tool returns `async_launched` with task metadata
- static meta is parsed
- `agent()` launches a child agent and writes transcript state
- `parallel()` respects result ordering and concurrency cap
- `pipeline()` executes sequentially
- `phase()` records phase progress
- completed `agent()` calls are cached on same-session resume
- changed prompt or opts invalidates cache
- stopped run can resume incomplete calls
- exceeding 1,000 agents fails cleanly
- named workflows resolve from nearest project `.claude/workflows/`
- user workflows resolve from `~/.claude/workflows/`

Integration tests should exercise CLI JSONL output without requiring a real DeepSeek API key.

## Implementation Order

1. Add core workflow and task types.
2. Add task registry and cancellation lifecycle.
3. Add workflow script resolver and static meta parser.
4. Add Node-based JS host and JSONL protocol.
5. Refactor child-agent execution into a reusable runtime function.
6. Implement `agent()`, cache records, transcripts, and same-session resume.
7. Implement `parallel()`, `pipeline()`, and `phase()`.
8. Add `Workflow` tool and CLI workflow commands.
9. Add events and server mapping.
10. Add TUI `/workflows` management.
11. Add saved workflow command loading and ultracode trigger.

## Open Risks

- Official workflow helper details may include additional options not visible in public docs or type definitions.
- A Node host means Orca workflow support depends on Node being installed.
- Background approval behavior must be carefully matched to Orca's existing approval policy to avoid surprising auto-denials.
- Cross-session inspection is useful, but cross-session resume should not be implemented by accident because it would diverge from the public same-session contract.

## Non-Goals For The First Implementation

- Remote workflow execution.
- Cloud CCR session URLs.
- Binary-compatible Claude Code session storage.
- Full Desktop app side-pane UI.
- Cross-session resume.
