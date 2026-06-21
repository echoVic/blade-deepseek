# Task 2 Report: Spec-Driven Registry

## Implementation Summary

Implemented the spec-driven registry behavior for Orca's built-in tool system:

- Extended `Tool` to expose `spec() -> &ToolSpec` and derive name, description, schema, action kind, and read-only checks from the spec.
- Added `ToolRegistry::resolve()` with canonical-name and alias lookup, returning `ResolvedTool`.
- Added `ToolRegistry::model_visible_tools()` and updated DeepSeek schema generation to use model-visible tools only.
- Converted built-in tools to own `ToolSpec` values with capability sets, renderer hints, exposure, and concurrency metadata.
- Registered canonical `glob` with the required description, schema, discovery capabilities, direct exposure, and `list_files` alias.
- Updated readonly batching to ignore caller-supplied `ActionKind` and use resolved spec capabilities plus concurrency metadata.
- Preserved MCP compatibility by keeping MCP schema names as `ToolName::Mcp(_)` through proxy specs.
- Updated DeepSeek parser lookup to use `resolve()` so legacy `list_files` calls remain accepted after `list_files` became an alias.

## Scope Notes

The full filesystem glob executor was not implemented. `BuiltinExecutor::Glob` currently routes through the existing list-files executor as a compatibility placeholder, consistent with the brief's instruction that Task 4 owns `crates/orca-tools/src/glob.rs`.

The parser now accepts `glob` and uses its `path` argument, defaulting to `"."`, so the placeholder executor can list a directory without trying to interpret the pattern before Task 4.

## Files Changed

- `crates/orca-tools/src/registry.rs`
- `crates/orca-tools/src/lib.rs`
- `crates/orca-provider/src/tool_schema.rs`
- `crates/orca-provider/src/deepseek_http.rs`

## TDD Evidence

### Red

Added the requested failing tests:

- `registry_resolves_list_files_to_discovery_capabilities`
- `model_visible_tools_hide_list_files_after_glob_exists`
- `readonly_batch_ignores_caller_supplied_write_action_for_read_tool`
- `generated_schema_uses_model_visible_tools_only`

The brief's combined `cargo test -p orca-tools ...` command was rejected by Cargo because it passed multiple test filters. I ran the tool tests individually instead.

Observed failures before implementation:

- `orca-tools` tests failed to compile because `ToolRegistry::resolve` and `ToolRegistry::model_visible_tools` did not exist.
- `generated_schema_uses_model_visible_tools_only` failed because the schema did not contain `glob`.

### Green

After implementing the registry/spec changes, the focused tests passed:

- `cargo test -p orca-tools registry_resolves_list_files_to_discovery_capabilities`
- `cargo test -p orca-tools model_visible_tools_hide_list_files_after_glob_exists`
- `cargo test -p orca-tools readonly_batch_ignores_caller_supplied_write_action_for_read_tool`
- `cargo test -p orca-provider generated_schema_uses_model_visible_tools_only`

## Verification

Required verification commands passed:

- `cargo test -p orca-core`: 76 passed
- `cargo test -p orca-tools`: 39 passed
- `cargo test -p orca-provider tool_schema`: 2 passed

Additional compatibility verification passed:

- `cargo test -p orca-provider`: 38 passed

## Compatibility Adjustment

The full provider suite initially failed two existing parser tests:

- `parse_list_files_with_path`
- `parse_list_files_without_path_defaults_to_dot`

Root cause: parser validation still used `ToolRegistry::get()`, which only checks canonical names, after `list_files` moved to an alias. I changed parser validation/action derivation to use `ToolRegistry::resolve()` so legacy `list_files` tool calls remain compatible.

## Concerns

- `glob` execution is intentionally a placeholder backed by the existing list-files executor until Task 4 implements real glob matching.
- `crates/orca-provider/src/deepseek_http.rs` was changed in addition to the three files listed in the brief because provider-wide tests showed that alias resolution otherwise broke existing `list_files` compatibility.

## Review Fix Follow-Up

Addressed the Task 2 review findings:

- Implemented real `glob` execution in `crates/orca-tools/src/glob.rs` using `globset` and `walkdir`. It honors required `pattern` and optional `path`, returns sorted workspace-relative matches, and returns `(no matches)` for missing paths or empty match sets. This intentionally pulls part of Task 4 forward because `glob` is already model-visible.
- Preserved legacy `list_files` execution by routing alias requests with `ToolName::ListFiles` through the old directory-listing executor.
- Updated typed subagent schema filtering to resolve allowlist entries such as `list_files` through the registry and compare against canonical model-visible tool names, so typed agents receive `glob`.
- Reserved alias names during registry registration so external tools named `list_files` cannot shadow the built-in alias.
- Updated the current system prompt tool list from `list_files` to `glob`.
- Added parser tests for `glob` with `{"pattern":"...","path":"..."}` and pattern-only input.

### Review Fix Red Checks

- `cargo test -p orca-tools external_tool_cannot_shadow_builtin_list_files_alias`: failed before implementation because `list_files` resolved to the external tool instead of canonical `glob`.
- `cargo test -p orca-tools registry_executes_glob_with_pattern_and_path`: failed before implementation because `glob` still listed directory entries and ignored `pattern`.
- `cargo test -p orca-tools registry_executes_glob_with_no_matches`: failed before implementation because missing paths returned `(empty)` through the list-files placeholder.
- `cargo test -p orca-provider typed_subagent_schema_resolves_allowed_list_files_alias_to_glob`: failed before implementation because typed schema filtering compared canonical `glob` against the legacy allowlist name `list_files`.
- `cargo test -p orca-provider parse_glob_with_pattern`: passed before implementation because prior parser work already accepted both new parser cases.

### Review Fix Verification

- `cargo test -p orca-tools external_tool_cannot_shadow_builtin_list_files_alias`: passed.
- `cargo test -p orca-tools registry_executes_glob_with_pattern_and_path`: passed.
- `cargo test -p orca-tools registry_executes_glob_with_no_matches`: passed.
- `cargo test -p orca-provider typed_subagent_schema_resolves_allowed_list_files_alias_to_glob`: passed.
- `cargo test -p orca-provider parse_glob_with_pattern`: passed, 2 tests.
- `cargo fmt`: passed.
- `cargo test -p orca-tools`: passed, 42 tests.
- `cargo test -p orca-provider`: passed, 41 tests.

### Review Fix Concerns

- `glob` now provides minimal real filesystem matching, but Task 4 may still expand semantics such as ignore-file handling or richer output formatting.
