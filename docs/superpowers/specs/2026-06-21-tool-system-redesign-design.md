# Tool System Redesign Design

Date: 2026-06-21

## Goal

Redesign Orca's tool system so tool behavior is predictable, policy-aware, and easier to align with Codex-style terminal agents.

The immediate trigger is that ordinary exploratory reads, such as listing an optional `.orca/workflows` directory or grepping a missing `tests/fixtures` directory, can currently surface as red tool failures in the TUI. That symptom points to a deeper design issue: model-visible tool names, execution behavior, approval policy, read-only concurrency, result semantics, and TUI rendering are coupled too tightly.

The target is a tool system where:

- Model-visible tools have stable names and clear schemas.
- Tool capabilities drive approval, sandboxing, concurrency, and rendering.
- Empty exploratory reads are not treated as operational failures.
- Shell execution remains available but is not the default way to inspect files.
- Existing sessions and prompts continue to work during migration.
- The design is grounded in the current local Codex CLI source under `/Users/qingyun/Documents/GitHub/codex`.

## Non-Goals

- Do not remove `bash`, `list_files`, `edit`, or `write_file` in the first migration. They remain accepted for transcript and prompt compatibility.
- Do not build Codex's full code-mode or deferred-tool search stack in the first migration.
- Do not replace Orca's existing workflow, subagent, MCP, or external-tool behavior while stabilizing built-in tools.
- Do not change the public JSONL event shape until the typed result model is proven internally.
- Do not make `bash` the only file exploration path. Structured file tools stay first-class.

## Current State

The current stack is split across crates but not cleanly by responsibility:

- `orca-core::tool_types` owns `ToolName`, `ToolRequest`, `ToolStatus`, and `ToolResult`.
- `orca-tools::registry` registers built-in tools with names, schemas, action kinds, and executors.
- `orca-tools::*` modules implement concrete execution.
- `orca-runtime::controller` and `orca-tui::bridge` execute tools, run hooks, batch read-only calls, and emit events.
- `orca-tui::ui` renders tool calls and subagent status.
- `orca-provider::system_prompt` manually describes tools to the model.

Important current coupling points:

- `ToolName` is an enum with special `Mcp(String)` and `External(String)` escape hatches.
- `ToolName::is_read_only()` knows only a few built-ins and cannot express richer tool capabilities.
- `ToolRequest` stores `name`, `action`, `target`, and raw arguments, so policy sees both a semantic name and a caller-supplied action kind.
- `ToolRegistry` registers name, description, schema, `ActionKind`, and executor together, but not aliases, exposure, output semantics, or renderer hints.
- `controller.rs` and `bridge.rs` special-case `Workflow`, `Subagent`, `UpdatePlan`, `UpdateGoal`, read-only batching, and streaming `bash`.
- `system_prompt.rs` manually documents only a subset of tools, so prompt text can drift from registry schemas.

The built-in tool surface currently includes:

- `read_file`
- `list_files`
- `grep`
- `bash`
- `edit`
- `write_file`
- `git_status`
- `web_search`
- `subagent`
- `Workflow`
- `update_plan`
- `update_goal`

The design problem is not that any one tool is wrong. The problem is that each layer has to infer intent from names or `ActionKind`, and each executor decides its own failure semantics.

## Reference Status

Local Codex source path:

- `/Users/qingyun/Documents/GitHub/codex`

The source tree is available and was inspected for this revision.

Key Codex files reviewed:

- `/Users/qingyun/Documents/GitHub/codex/codex-rs/tools/src/tool_spec.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/tools/src/tool_definition.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/tools/src/tool_executor.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/tools/src/tool_search.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/tools/src/responses_api.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/protocol/src/tool_name.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/core/src/tools/spec_plan.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/core/src/tools/handlers/shell_spec.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/core/src/tools/handlers/apply_patch_spec.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/core/src/tools/handlers/plan_spec.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/tools/src/tool_config.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/core/src/exec_policy.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/file-search/src/lib.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/exec-server/src/file_read.rs`

Known nearby references:

- Codex CLI has a first-class `ToolSpec` abstraction for function tools, namespace tools, hosted tools, freeform tools, and deferred tool discovery.
- Claude Code-style tools in `/Users/qingyun/Documents/GitHub/package 3` use `Read`, `Glob`, `Grep`, `Bash`, `Edit`, and `Write`.
- Blade Code documentation similarly exposes `Read`, `Glob`, `Grep`, and `Bash`.

Orca should copy Codex's separation of spec, executor, exposure, namespace, and discovery concepts. It should not copy every Codex tool name blindly because Orca already has provider-facing compatibility and a different existing tool surface.

## Codex Source Findings

Codex's tool system is more structured than a flat built-in tool enum:

- `ToolSpec` serializes model-visible tools for the Responses API. It supports `Function`, `Namespace`, `ToolSearch`, `ImageGeneration`, `WebSearch`, and `Freeform`.
- `ToolDefinition` is lower-level metadata: name, description, input schema, optional output schema, and `defer_loading`.
- `ToolExecutor` ties an executable runtime to `tool_name()`, `spec()`, `exposure()`, optional search metadata, parallel-call support, and `handle()`.
- `ToolExposure` controls visibility as `Direct`, `Deferred`, `DirectModelOnly`, or `Hidden`.
- `ToolName` preserves `{ namespace, name }` instead of collapsing every callable into one flat string.
- `ToolSearchInfo` derives searchable text from specs and converts deferred tools into loadable function or namespace specs.
- `spec_plan.rs` builds the model-visible spec list and runtime registry together, merges namespace specs, appends `tool_search` only when deferred tools exist, and keeps hidden dispatch-only handlers out of the model prompt.
- Shell support is a backend strategy, not just a tool name. Codex can expose classic shell command tools or unified `exec_command`/`write_stdin` tools depending on model and feature flags.
- `apply_patch` is a `Freeform` grammar-backed tool, not just JSON arguments. This gives editing a stricter contract than generic shell execution.
- `update_plan` is a normal function tool spec with its own handler.
- Codex has filesystem services and a `file-search` crate, but the inspected source does not show a primary `list_files` model tool. File discovery is either shell-oriented, file-search-oriented, or provided through other environments/MCP tools.

Implications for Orca:

- Keep structured file tools because Orca already exposes them and users expect them.
- Replace name-based policy and renderer behavior with spec/exposure/capability metadata.
- Make `ToolName` namespace-aware before MCP/dynamic tools become more important.
- Add a deferred-discovery path later, but do not require it for the initial built-in tool cleanup.
- Treat shell as a pluggable execution backend. A future `exec_command` alias or replacement should be planned separately from the first `glob`/result-semantics fix.

## Design Principles

1. Stable model interface first.

   The model should see a small set of durable tools with obvious semantics. Internal executors can change without changing prompts or transcripts.

2. Capability drives policy.

   Approval, concurrency, sandboxing, and UI treatment should be derived from a structured capability model, not string checks against tool names.

3. Shell is powerful, not primary.

   `bash` remains necessary for tests, builds, scripts, and complex inspection. It should not replace structured read/search tools for normal project exploration.

4. Empty is not failed.

   A missing optional search path, no grep matches, or an empty directory should produce a successful read result with explicit empty output. Real permission errors, invalid arguments, and runtime failures should remain failures.

5. Compatibility through aliases.

   Existing names such as `list_files` should remain supported while new preferred names are introduced.

6. No caller-supplied authority.

   The registry owns a tool's capability and action class. Parsed model calls should not be able to upgrade a read tool into a write or shell action by providing a different `ActionKind`.

## Proposed Architecture

Introduce a layered tool model:

1. Tool spec

   A model-visible contract:

   - stable name
   - optional namespace
   - aliases
   - description
   - JSON schema
   - optional output schema
   - capability set
   - exposure mode
   - output contract
   - empty-result behavior
   - renderer hint

   Draft shape:

   ```rust
   struct ToolSpec {
       id: ToolId,
       name: ToolName,
       aliases: Vec<ToolName>,
       description: String,
       input_schema: serde_json::Value,
       output_schema: Option<serde_json::Value>,
       capabilities: CapabilitySet,
       exposure: ToolExposure,
       result_semantics: ResultSemantics,
       renderer: RendererHint,
   }
   ```

2. Capability policy

   Structured capabilities replace scattered checks:

   - `fs.read`
   - `fs.list`
   - `fs.search`
   - `fs.write`
   - `shell.execute`
   - `git.inspect`
   - `network.search`
   - `agent.delegate`
   - `workflow.run`
   - `plan.update`
   - `goal.update`

   Capabilities should be owned by specs, not by model requests. `ActionKind` can remain as a serialized compatibility field during migration, but it should be derived from capability metadata.

3. Executor

   Concrete implementation:

   - filesystem read
   - filesystem glob
   - ripgrep search
   - shell command
   - edit/write
   - git status
   - MCP proxy
   - external command tool

   Executors should expose the same high-level contract Codex uses: name, spec, exposure, parallel support, and `handle()`.

   Draft shape:

   ```rust
   trait ToolExecutor: Send + Sync {
       fn spec(&self) -> &ToolSpec;
       fn supports_parallel_calls(&self) -> bool;
       fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult;
   }
   ```

4. Policy engine

   Uses tool spec plus runtime config to answer:

   - is this read-only?
   - can it run concurrently?
   - does it require approval?
   - should it run in a sandbox?
   - can hooks modify it?
   - is it available to subagents?
   - is it direct, deferred, model-only, or hidden?

5. Renderer

   Turns typed results into UI summaries. Renderers should not decide success or failure.

## Core Architecture Decision

Adopt a Codex-like internal shape without forcing Codex's entire public tool surface:

- Orca keeps `read_file`, `glob`, `grep`, `edit`, `write_file`, `bash`, `git_status`, `web_search`, `subagent`, `Workflow`, `update_plan`, and `update_goal`.
- Internally, all tools are represented by `ToolSpec` plus `ToolExecutor`.
- `ToolName` becomes namespace-aware and string-backed enough to support MCP, external tools, and future plugins without adding enum variants forever.
- `ToolExposure` controls prompt visibility and dispatch visibility.
- Capabilities replace `ToolName::is_read_only()` and most `ActionKind` checks.
- Prompt generation reads specs rather than a hard-coded tool list.

This gives Orca the Codex separation of concerns while preserving existing sessions and users' muscle memory.

## Tool Surface

Preferred model-visible tools:

- `read_file`
- `glob`
- `grep`
- `edit`
- `write_file`
- `bash` for current compatibility, with a future `exec_command` compatibility path
- `git_status`
- `web_search`
- `subagent`
- `Workflow`
- `update_plan`
- `update_goal`

Compatibility aliases:

- `list_files` maps to the same filesystem discovery executor as `glob`, with directory-list input compatibility and legacy `(empty)` display text.
- `read` maps to `read_file` for compatibility with non-Orca prompts.
- `shell_command` and `exec_command` map to the shell backend only after a dedicated shell-tool migration.

`list_files` should stop being a primary prompt recommendation once `glob` exists. It can remain as a narrow helper and transcript-compatible alias.

Codex does not appear to make `list_files` a primary model tool in the inspected source. That supports demoting `list_files` in Orca instead of expanding its role.

Initial model-visible ordering:

1. `read_file`
2. `glob`
3. `grep`
4. `edit`
5. `write_file`
6. `bash`
7. `git_status`
8. `web_search`
9. `subagent`
10. `Workflow`
11. `update_plan`
12. `update_goal`, only when goal context is active

Hidden or compatibility-only names should still parse and dispatch, but should not be recommended in the prompt.

## Tool Identity And Exposure

Replace Orca's flat `ToolName` enum center with a namespace-aware identity:

```rust
struct ToolName {
    namespace: Option<String>,
    name: String,
}
```

Built-in Orca tools can stay plain names. MCP, plugin, dynamic, and future grouped tools should use namespaces such as `mcp__server`, `workflow`, or `agent`.

During migration, preserve current serialization by displaying plain names exactly as today:

- `ToolName { namespace: None, name: "read_file" }` serializes as `"read_file"`.
- `ToolName { namespace: Some("mcp__foo"), name: "exec_command" }` serializes as `"mcp__foo__exec_command"` for legacy event consumers, while retaining the split internally.

Add explicit exposure metadata:

- `direct`: included in the initial model-visible tool list.
- `deferred`: registered for dispatch and discoverable later.
- `model_only`: visible to the model but excluded from nested/code-mode surfaces if Orca adds them.
- `hidden`: dispatch-only and not model-visible.

This mirrors Codex's `ToolExposure` and removes several name checks from prompt generation, subagent filtering, and rendering.

Alias rules:

- Aliases map to a canonical `ToolSpec` before policy and execution.
- Aliases are not independently rendered as separate primary tools.
- The original requested name may be kept in telemetry for debugging, but policy uses the canonical spec.
- Aliases must not change capabilities. `list_files` as an alias of `glob` cannot gain write or shell behavior.
- An alias may keep a legacy display string when that is part of compatibility. For example, `list_files` can render `(empty)` while the canonical discovery result kind is `Empty` or `NoMatches`.

## Deferred Tool Discovery

Do not build Codex-style `tool_search` in the first implementation phase. Orca's built-in tool count is still small enough for direct exposure.

Reserve the design:

- A deferred tool must provide search text derived from name, description, schema fields, and namespace.
- A `tool_search` executor can later return loadable specs with `defer_loading: true`.
- Deferred tools are useful for MCP, plugins, workflows, and large tool catalogs.
- Built-in file tools should remain direct for now because they are high-frequency coding actions.

The first implementation should add `ToolExposure` even if only `direct` and `hidden` are actively used. That keeps the shape compatible with deferred discovery later without introducing a half-migration.

## Glob Tool

Add a first-class `glob` tool.

Schema:

```json
{
  "pattern": "string",
  "path": "string?"
}
```

Behavior:

- `path` defaults to `"."`.
- `pattern` supports common glob patterns such as `**/*.rs`.
- Missing `path` returns completed output `(no matches)`.
- No matches returns completed output `(no matches)`.
- Results are relative to cwd.
- Results are sorted for stability.
- Results are truncated through the shared tool output truncation function.

This is closer to Claude/Blade's `Glob` while preserving Orca's snake_case naming style.

Implementation preference:

- Use the `globset` and `ignore` crates if already available or acceptable in the workspace.
- Respect `.gitignore` by default for broad recursive patterns.
- Avoid traversing hidden implementation directories only if existing Orca conventions already do so; otherwise keep behavior explicit and documented.
- Cap result count and output bytes through shared truncation, not by ad hoc string slicing.

## Result Semantics

Introduce typed result meaning without changing the existing JSONL envelope immediately.

Current:

- `ToolStatus::Completed`
- `ToolStatus::Failed`
- `ToolStatus::Denied`
- `ToolStatus::NotImplemented`

Add an optional result kind:

- `success`
- `empty`
- `no_matches`
- `truncated`
- `permission_denied`
- `invalid_input`
- `runtime_error`

Initial migration can encode this in output text and renderer hints before changing the public event schema. Later, `ToolResult` can add a serialized `kind` field with a compatibility default.

Recommended semantics:

| Tool | Missing target | Empty target | No matches |
| --- | --- | --- | --- |
| `read_file` | failed | completed | not applicable |
| `glob` | completed `(no matches)` | completed `(no matches)` | completed `(no matches)` |
| `list_files` | completed `(empty)` | completed `(empty)` | not applicable |
| `grep` | completed `(no matches)` for missing search path | not applicable | completed `(no matches)` |
| `edit` | failed | failed if old text invalid | not applicable |
| `write_file` | create parent dirs if safe | completed | not applicable |
| `bash` | command-dependent | command-dependent | command-dependent |

This distinction fixes exploratory noise without hiding real write or shell failures.

Internal result shape:

```rust
enum ToolResultKind {
    Success,
    Empty,
    NoMatches,
    Truncated,
    PermissionDenied,
    InvalidInput,
    RuntimeError,
}
```

Compatibility mapping:

- `Success`, `Empty`, `NoMatches`, and `Truncated` map to `ToolStatus::Completed`.
- `PermissionDenied` maps to `ToolStatus::Denied` only when policy denied execution; filesystem permission errors map to `ToolStatus::Failed` with `PermissionDenied` kind.
- `InvalidInput` and `RuntimeError` map to `ToolStatus::Failed`.
- The TUI uses `kind` to choose dim/neutral/red styling; JSONL can keep the existing status fields until a schema version bump.

## Approval And Concurrency

Replace `ToolName::is_read_only()` and ad hoc registry decisions with capabilities.

Rules:

- `fs.read`, `fs.list`, `fs.search`, and `git.inspect` are read-only.
- Read-only tools can run concurrently when marked `concurrent_safe`.
- `fs.write` requires write policy.
- `shell.execute` requires shell policy and optional sandboxing.
- `network.search` requires network policy.
- `agent.delegate` and `workflow.run` use their own depth, concurrency, and task policies.
- `update_plan` and `update_goal` are local state updates, not filesystem writes, and get explicit policy entries.

This makes approval modes easier to reason about:

- `suggest`: read-only auto, write/shell prompt or deny depending mode.
- `auto-edit`: read-only auto, filesystem edits allowed, shell prompt/deny.
- `full-auto`: read-only, edits, and approved shell class allowed under sandbox/policy.

Policy lookup should follow this order:

1. Parse model call into requested name and raw arguments.
2. Resolve aliases to a canonical spec.
3. Derive action class and approval requirement from capabilities.
4. Apply runtime context, such as goal mode, workflow config, MCP availability, and provider policy.
5. Execute only if the canonical spec is available in that context.

This prevents a malformed request from bypassing policy by pairing a safe name with an unsafe action.

## System Prompt Generation

The system prompt should be generated from tool specs rather than manually maintained in `orca-provider::system_prompt`.

Benefits:

- Tool descriptions cannot drift from schemas.
- Aliases can be hidden from the model but accepted by the parser.
- Capability notes can be consistent.
- Subagent tool restrictions can reuse the same spec data.

Prompt ordering should prefer:

1. `read_file`
2. `glob`
3. `grep`
4. `edit`
5. `write_file`
6. `bash`
7. `git_status`
8. task tools
9. planning/goal tools

`bash` prompt guidance should explicitly say it is for tests, builds, project scripts, and complex shell-only tasks.

Prompt generation rules:

- Include only `direct` tools enabled for the current context.
- Hide aliases unless the alias is intentionally in a compatibility transition window.
- Include `update_goal` only when a goal context exists.
- Include `Workflow` only when workflows are enabled.
- Include MCP and external tools from specs, grouped by namespace when available.
- Keep `bash` guidance clear that file inspection should prefer `read_file`, `glob`, and `grep`.

## TUI Rendering

Renderers should receive structured display metadata:

- label
- short target
- status
- output summary
- error summary
- whether empty output should be dim rather than red

Examples:

- `glob`: `✓ glob: **/*.rs (no matches)`
- `list_files`: `✓ list_files: .orca/workflows (empty)`
- `grep`: `✓ grep: tests/fixtures (no matches)`
- `bash`: `✗ bash: cargo test (exit 101)`

Subagent failure should remain red only when the subagent's final run status is failed. A child read tool returning empty should not fail the subagent by itself.

Rendering should consume:

- canonical tool name
- requested alias, when different
- status
- result kind
- short target
- output preview
- error preview
- truncation flag

The renderer should not inspect raw arguments to infer failure class when the executor already returned a kind.

## Migration Plan

Phase 0: Keep current bug fix.

- Keep recent missing-path fixes for `list_files` and `grep`.
- Add regression tests for tool status and output semantics.
- Audit other read-only tools for empty-result behavior.

Phase 1: Add specs without changing behavior.

- Add `ToolSpec` and namespace-aware `ToolName` in `orca-core`.
- Move name, aliases, namespace, schema, optional output schema, capabilities, exposure, and renderer hint into specs.
- Make registry register from specs.
- Keep existing `BuiltinTool` executor shape during this phase.
- Add `ToolExposure` with `direct`, `deferred`, `model_only`, and `hidden`.
- Derive `ActionKind` from specs while preserving the field in `ToolRequest`.

Phase 2: Replace policy lookups.

- Replace `ToolName::is_read_only()` with capability checks.
- Replace read-only batching checks with `capabilities.read_only && concurrent_safe`.
- Route subagent, workflow, goal, and plan availability through specs plus runtime context.
- Keep existing special execution paths where necessary, but make their policy metadata spec-driven.

Phase 3: Add `glob` and compatibility `list_files`.

- Implement `glob` with sorted, relative output.
- Register `list_files` as a compatibility entry pointing at the same filesystem discovery executor, with legacy argument parsing and display text.
- Update system prompt to recommend `glob` over `list_files`.

Phase 4: Add typed result kinds.

- Add internal `ToolResultKind`.
- Keep JSONL `ToolStatus` compatible.
- Update file tools to return `Empty` and `NoMatches`.
- Update TUI rendering to use result kind.

Phase 5: Generate prompts from specs.

- Replace manual tool list in `system_prompt.rs`.
- Add tests asserting prompt contains all enabled model-visible tools and hides aliases.

Phase 6: Add Codex-aligned extensibility.

- Add optional deferred tool discovery for MCP, plugins, and workflows.
- Add namespace grouping for non-built-in tools.
- Evaluate a shell-tool migration from `bash` toward `exec_command`/`write_stdin` semantics.
- Keep `bash` as an alias or provider-specific surface until old sessions and prompts are safely migrated.

## Implementation Notes

Suggested file touch points:

- `crates/orca-core/src/tool_types.rs`: `ToolName`, `ToolSpec`, capabilities, exposure, result kind.
- `crates/orca-tools/src/registry.rs`: spec registration, alias resolution, canonical dispatch.
- `crates/orca-provider/src/system_prompt.rs`: generated prompt from enabled specs.
- `crates/orca-runtime/src/controller.rs`: capability-based policy, read-only batching, workflow/subagent context availability.
- `crates/orca-tui/src/bridge.rs`: mirror runtime changes for TUI execution and goal handling.
- `crates/orca-tui/src/ui.rs`: result-kind-aware rendering.
- `docs/harness-contract.md`: update the public contract after `glob` and result semantics land.

Implementation order should keep each phase shippable. After Phase 1, behavior should be equivalent. After Phase 2, policy should be equivalent but spec-driven. After Phase 3, the model gets the new preferred file discovery tool.

## Tests

Unit tests:

- Tool specs expose expected names, aliases, schemas, and capabilities.
- Namespace-aware `ToolName` round-trips plain and namespaced names.
- Tool exposure controls direct, deferred, model-only, and hidden registration.
- Alias resolution returns canonical specs and preserves capabilities.
- `glob` returns sorted relative paths.
- `glob` returns completed no-match output for missing path.
- `list_files` alias remains accepted.
- `grep` missing path remains completed no-match.
- Policy classifies capabilities correctly.

Integration tests:

- JSONL events for empty read/search tools have `status: completed`.
- Read-only batching includes `read_file`, `glob`, `grep`, and `git_status`.
- Write tools still require write approval.
- `bash` still requires shell approval outside full-auto policy.
- System prompt includes preferred tools and omits hidden aliases.
- Deferred tools are registered for dispatch but omitted from the direct tool list.
- `update_goal` appears only in goal context.
- `Workflow` appears only when workflows are enabled.

TUI tests:

- Empty read/search results render as completed, not failed.
- Subagent messages do not show failed when child tools only returned empty read results.

## Open Questions

- Should `glob` be implemented as an exact glob matcher or as a file-search query tool with glob-compatible syntax?
- Should `ToolResult` grow a public `kind` field immediately, or should result kind remain internal until the next event schema version?
- Should `bash` remain Orca's primary shell name, or should Orca introduce `exec_command` as the Codex-aligned primary name with `bash` as an alias?
- Should `apply_patch` eventually become a grammar-backed freeform tool instead of JSON-style `edit` and `write_file` only?

## Success Criteria

- Ordinary missing optional directories no longer appear as red failures.
- The model sees fewer overlapping file exploration tools.
- Approval behavior can be explained from capabilities without reading executor code.
- TUI output reflects tool semantics instead of raw executor quirks.
- Existing transcripts and provider parsing continue to work.
- Future Codex changes can be incorporated by changing specs and aliases, not rewriting execution architecture.
