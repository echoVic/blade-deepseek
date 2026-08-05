# ask_user_question Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a DeepSeek-visible `ask_user_question` tool that collects one to four structured answers through Orca's existing runtime-owned interaction path.

**Architecture:** Keep `request_user_input` intact and add a distinct `ToolName::AskUserQuestion` contract whose model-visible name is `ask_user_question`. Parse and validate the Claude-compatible questionnaire in `orca-runtime`, then issue one typed `RuntimeUserInputRequest` per question through the existing broker so TUI, surface routing, cancellation, and recovery keep a single owner. Aggregate accepted answers into deterministic JSON for the model.

**Tech Stack:** Rust, serde/serde_json, Orca tool registry, runtime interaction broker, ratatui TUI, Cargo contract tests.

---

### Task 1: Freeze the public tool contract

**Files:**
- Modify: `crates/orca-core/src/tool_types.rs`
- Modify: `crates/orca-tools/src/registry.rs`
- Test: `crates/orca-tools/src/lib.rs`

- [x] **Step 1: Write a failing registry test**

Assert that `ask_user_question` is registered, read-only, model-visible, runtime-owned, and exposes `questions` with 1-4 items, `options` with 2-4 items, plus `header`, `description`, and `multiSelect` fields.

- [x] **Step 2: Run the focused test and verify RED**

Run: `cargo test -p orca-tools ask_user_question_tool -- --nocapture`

Expected: FAIL because `ask_user_question` is not registered.

- [x] **Step 3: Add the typed name and registry specification**

Add `ToolName::AskUserQuestion`, accepting and serializing only the model-facing `ask_user_question` name. Register a conservative direct tool using the `UserInputRequest` capability and an `AskUserQuestion` runtime-only executor.

- [x] **Step 4: Run the focused test and verify GREEN**

Run: `cargo test -p orca-tools ask_user_question_tool -- --nocapture`

Expected: PASS.

### Task 2: Parse, validate, execute, and aggregate questionnaires

**Files:**
- Modify: `crates/orca-runtime/src/runtime_user_input.rs`
- Modify: `crates/orca-runtime/src/runtime_special.rs`
- Modify: `crates/orca-runtime/src/tool_router.rs`
- Modify: `crates/orca-runtime/src/lifecycle.rs`
- Test: `crates/orca-runtime/src/runtime_user_input.rs`
- Test: `tests/runtime_lifecycle_contract.rs`

- [x] **Step 1: Write failing parser and execution tests**

Cover one-to-four questions, camelCase and snake_case `multiSelect`, ordered per-question broker calls, label/description projection, JSON answer aggregation, cancellation, empty questions, too many questions, invalid options, duplicate labels, and unchanged legacy `request_user_input` behavior.

- [x] **Step 2: Run the focused tests and verify RED**

Run: `cargo test -p orca-runtime runtime_user_input::tests -- --nocapture`

Expected: FAIL because questionnaire parsing/execution does not exist.

- [x] **Step 3: Implement the minimal runtime behavior**

Deserialize `questions` into typed question/option structs, validate the public bounds and required text, derive unique interaction IDs as `<tool-call-id>:<index>`, expose readable choices as `<label> - <description>`, collect raw user answers, and return compact JSON shaped as `{"answers":{"<question>":"<answer>"}}`. Return a cancelled tool result if any question is dismissed.

- [x] **Step 4: Route the new tool through the existing special interaction dispatch**

Classify both `RequestUserInput` and `AskUserQuestion` as user-input interactions, but select the correct parser from the typed `ToolName`.

- [x] **Step 5: Run focused runtime tests and verify GREEN**

Run: `cargo test -p orca-runtime runtime_user_input::tests -- --nocapture`

Run: `cargo test --test runtime_lifecycle_contract ask_user_question -- --nocapture`

Expected: PASS.

### Task 3: Preserve structured choices through the TUI projection

**Files:**
- Test: `crates/orca-tui/src/runtime_interaction_adapter_tests.rs`

- [x] **Step 1: Write a TUI projection regression test**

Assert that an `ask_user_question` runtime request produces `UserInputRequested` with the full question and readable label/description choices, and that an answer returns through the existing typed broker.

- [x] **Step 2: Run the focused projection test**

Run: `cargo test -p orca-tui runtime_interaction_adapter_tests::ask_user_question -- --nocapture`

Expected: PASS because the questionnaire reuses the existing typed request and broker; no TUI production change is required.

- [x] **Step 3: Propagate the shared request fields without adding a second broker**

Keep `TuiInteractionResponse::UserInput(String)` and the current composer workflow. Ensure the transcript clearly lists the available choices and custom text remains accepted for the implicit Other path.

- [x] **Step 4: Run focused TUI tests and verify GREEN**

Run: `cargo test -p orca-tui runtime_interaction_adapter_tests::ask_user_question -- --nocapture`

Expected: PASS.

### Task 4: Document and verify the feature

**Files:**
- Modify: `README.md`
- Modify: `docs/reference/tools.md` if this repository's tool catalog uses that path

- [x] **Step 1: Document when and how the model uses `ask_user_question`**

Document the structured schema, 1-4 and 2-4 bounds, multi-select answer convention, custom text, cancellation semantics, and headless failure behavior. Do not change the legacy `request_user_input` contract.

- [x] **Step 2: Run formatting and static validation**

Run: `cargo fmt --all -- --check`

Run: `cargo clippy --workspace --all-targets -- -D warnings`

Result: formatting passed. Non-strict workspace Clippy exited 0 with the
repository's existing warnings. The strict `-D warnings` variant remains blocked
by 17 pre-existing `orca-core` warnings before it reaches the changed runtime
paths.

- [x] **Step 3: Run affected and full test gates**

Run: `cargo test -p orca-tools`

Run: `cargo test -p orca-runtime`

Run: `cargo test -p orca-tui`

Run: `cargo test --workspace --all-targets`

Result: focused registry, runtime, lifecycle, and TUI tests passed. The serial
workspace gate passed with one explicitly skipped pre-existing test,
`bash_commands_receive_eof_on_stdin_instead_of_inheriting_terminal`, which was
also reproduced in isolation and is unchanged by this patch. The remaining
workspace suite passed, including 998 runtime tests, 219 tools tests, and 1,028
TUI tests.

- [x] **Step 4: Inspect the final patch**

Run: `git diff --check`

Run: `git status --short`

Expected: no whitespace errors; only intended source, test, plan, and documentation files are modified in addition to pre-existing untracked artifacts.
