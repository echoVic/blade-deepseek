# DeepSeek Unknown Tool Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `v0.2.17` so a DeepSeek call such as `function.name = "wc -l"` becomes a recorded, non-executable tool failure that the model can correct instead of a terminal provider error that pauses Goal mode.

**Architecture:** Preserve every provider tool call as a `ToolRequest`. Registered names retain their existing identity and action; unresolved names become `ToolName::External` with `ActionKind::Read`, the original name as display target, and untouched arguments. Existing registry validation rejects unresolved calls before approval, hooks, task creation, or execution, and the normal tool loop records the failure and asks the model for the next turn.

**Tech Stack:** Rust 2024, Cargo tests, DeepSeek streaming fixtures, Orca mock provider, ratatui TUI loop, TypeScript/Vite site, Node.js release scripts, GitHub Actions, npm.

---

## File Map

- `crates/orca-provider/src/deepseek_http.rs`: preserve unknown streaming and non-streaming provider calls as non-executable tool requests.
- `crates/orca-provider/src/lib.rs`: add a deterministic unknown-call-then-correct mock flow.
- `crates/orca-tui/src/agent_runner.rs`: prove the actual TUI agent loop records the failure and completes the corrective turn.
- `docs/goal-mode.md`: document which failures pause Goal continuation and which malformed tool calls are recoverable.
- `docs/production-roadmap.md`: record the `v0.2.17` provider/runtime baseline.
- `docs/releases/v0.2.17.md`: add incident behavior, safety boundary, and focused verification.
- `site/src/changelog/Changelog.tsx`: update English and Chinese `v0.2.17` summaries.

The base branch already contains the complete cumulative Goal timer feature and
all `0.2.17` version metadata. This slice must not create a second version bump.

### Task 1: Preserve Unknown DeepSeek Tool Calls

**Files:**
- Modify: `crates/orca-provider/src/deepseek_http.rs`

- [ ] **Step 1: Replace the parser error test with the exact incident regression**

Replace `parse_unknown_tool_returns_error` with:

```rust
#[test]
fn parse_unknown_tool_preserves_call_for_model_correction() {
    let tc = make_tc("wc -l", r#"{}"#);
    let request = parse_tool_call(&tc, &[]);

    assert_eq!(
        request.name,
        ToolName::External("wc -l".to_string())
    );
    assert_ne!(request.name, ToolName::Bash);
    assert_eq!(request.action, ActionKind::Read);
    assert_eq!(request.target.as_deref(), Some("wc -l"));
    assert_eq!(request.raw_arguments.as_deref(), Some(r#"{}"#));
}
```

- [ ] **Step 2: Add a streaming response regression**

Add beside `streaming_invalid_tool_arguments_are_returned_for_tool_failure`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_unknown_tool_is_returned_for_model_correction() {
    let unknown = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_wc\",\"function\":{\"name\":\"wc -l\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                   data: [DONE]\n\n";
    let (base_url, bodies) = spawn_streaming_response_sequence_server(vec![unknown]);
    let mut conversation = Conversation::new();
    conversation.add_user("count lines".to_string());
    let config = ProviderConfig {
        api_key: Some("test-key".to_string()),
        base_url: Some(base_url),
        model: Some("deepseek-v4-pro".to_string()),
        reasoning_effort: orca_core::config::ReasoningEffort::default(),
        tools_override: None,
        mcp_registry: None,
        external_tools: Vec::new(),
    };
    let cancel = CancelToken::new();

    let response = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
        .await
        .expect("unknown tool should remain a corrective tool turn");

    assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
    assert!(response.steps.iter().all(|step| !matches!(step, ProviderStep::Error(_))));
    assert!(matches!(
        response.steps.as_slice(),
        [ProviderStep::ToolCall(request)]
            if request.id == "call_wc"
                && request.name == ToolName::External("wc -l".to_string())
                && request.target.as_deref() == Some("wc -l")
    ));
    assert_eq!(response.tool_calls[0].function_name, "wc -l");
    assert_eq!(response.tool_calls[0].arguments, "{}");
}
```

- [ ] **Step 3: Run the tests and verify RED**

Run separately:

```bash
cargo test -p orca-provider parse_unknown_tool_preserves_call_for_model_correction -- --nocapture
cargo test -p orca-provider streaming_unknown_tool_is_returned_for_model_correction -- --nocapture
```

Expected: the parser test fails to compile because `parse_tool_call` still
returns `Result`; the streaming test fails because the response contains
`ProviderStep::Error("failed to parse tool call 'wc -l': unknown tool: wc -l")`.

- [ ] **Step 4: Make provider conversion total and non-executable**

Change `parse_tool_call` to return `ToolRequest` directly. Its identity/action
setup must be:

```rust
let reg = registry::tool_registry_with_mcp_and_external(None, external_tools);
let resolved = reg.resolve(schema_name);
let name = registry::tool_name_from_schema_name(schema_name)
    .expect("provider tool names always map to ToolName");
let action = resolved
    .as_ref()
    .map(|resolved| resolved.spec.capabilities.action_kind())
    .unwrap_or(ActionKind::Read);
```

For target extraction, preserve all existing registered cases, then add the
unknown-name fallback outside JSON parsing so even malformed arguments retain a
useful display target:

```rust
let target = serde_json::from_str::<Value>(&tc.function.arguments)
    .ok()
    .and_then(|args| match schema_name {
        "read_file" => args["path"].as_str().map(String::from),
        "list_files" | "glob" => args["path"]
            .as_str()
            .map(String::from)
            .or(Some(".".to_string())),
        "grep" => args["pattern"].as_str().map(String::from),
        "bash" => args["command"].as_str().map(String::from),
        "edit" | "write_file" => args["path"].as_str().map(String::from),
        "git_status" => Some(".".to_string()),
        "subagent" => args["description"]
            .as_str()
            .map(String::from)
            .or_else(|| args["prompt"].as_str().map(String::from)),
        "web_search" => args["query"].as_str().map(String::from),
        "update_plan" => {
            let count = args["plan"].as_array().map(|plan| plan.len()).unwrap_or(0);
            Some(format!("{count} items"))
        }
        other if other.starts_with("mcp__") => Some(other.to_string()),
        other if external_tools.iter().any(|tool| tool.name == other) => {
            Some(other.to_string())
        }
        _ => None,
    })
    .or_else(|| resolved.is_none().then(|| schema_name.to_string()));
```

Return:

```rust
ToolRequest {
    id: tc.id.clone(),
    name,
    action,
    target,
    raw_arguments: Some(tc.function.arguments.clone()),
}
```

Update streaming and non-streaming call sites to append
`ProviderStep::ToolCall(parse_tool_call(...))` without creating a provider
error branch.

- [ ] **Step 5: Run focused provider tests and verify GREEN**

```bash
cargo test -p orca-provider parse_unknown_tool_preserves_call_for_model_correction -- --nocapture
cargo test -p orca-provider streaming_unknown_tool_is_returned_for_model_correction -- --nocapture
cargo test -p orca-provider deepseek_http::tests -- --test-threads=1
```

Expected: all commands pass; the provider suite contains one more test than the
153-test baseline.

- [ ] **Step 6: Commit the provider fix**

```bash
git add crates/orca-provider/src/deepseek_http.rs
git commit -m "fix(provider): recover unknown tool calls"
```

### Task 2: Prove TUI And Goal-Turn Recovery

**Files:**
- Modify: `crates/orca-provider/src/lib.rs`
- Modify: `crates/orca-tui/src/agent_runner.rs`

- [ ] **Step 1: Add the TUI regression first**

Add beside `tui_child_agent_recovers_from_invalid_tool_arguments`:

```rust
#[test]
fn tui_main_agent_recovers_from_unknown_tool_call() {
    let config = full_auto_config();
    let (event_tx, _event_rx) = mpsc::channel();
    let (_action_tx, action_rx) = mpsc::channel();
    let cancel = CancelToken::new();
    let mut session =
        TuiConversationSession::new_with_preloaded(&config, "recover", None)
            .expect("session");

    let status = run_agent_for_tui(
        &config,
        &mut session,
        "unknown_tool_then_fix",
        &event_tx,
        &action_rx,
        &cancel,
        false,
    );

    assert_eq!(status, "success");
    assert!(session.conversation().messages.iter().any(|message| matches!(
        message,
        orca_core::conversation::Message::Tool { content, .. }
            if content.contains("unknown tool: wc -l")
    )));
    assert!(session.conversation().messages.iter().any(|message| matches!(
        message,
        orca_core::conversation::Message::Assistant { content: Some(content), .. }
            if content.contains("Mock completed after correcting unknown tool call")
    )));
}
```

- [ ] **Step 2: Run the TUI test and verify RED**

```bash
cargo test -p orca-tui tui_main_agent_recovers_from_unknown_tool_call -- --nocapture
```

Expected: FAIL because the generic mock response contains neither the unknown
tool failure nor the corrective final message.

- [ ] **Step 3: Add the deterministic mock correction flow**

In `mock_call`, before generic prompt parsing, return a successful final answer
when `unknown_tool_then_fix` has a recorded `unknown tool: wc -l` result:

```rust
if prompt.trim() == "unknown_tool_then_fix" && has_tool_results {
    let saw_unknown_failure = conversation.messages.iter().any(|message| match message {
        Message::Tool { content, .. } => content.contains("unknown tool: wc -l"),
        _ => false,
    });
    if saw_unknown_failure {
        let message = "Mock completed after correcting unknown tool call.".to_string();
        return ProviderResponse {
            steps: vec![ProviderStep::MessageDelta(message.clone())],
            assistant_content: Some(message),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }
}
```

Before the generic fallback, emit the unresolved request on the first turn:

```rust
if prompt.trim() == "unknown_tool_then_fix" {
    let tool_request = ToolRequest {
        id: "mock-unknown-tool-1".to_string(),
        name: ToolName::External("wc -l".to_string()),
        action: ActionKind::Read,
        target: Some("wc -l".to_string()),
        raw_arguments: Some("{}".to_string()),
    };
    return ProviderResponse {
        steps: vec![ProviderStep::ToolCall(tool_request.clone())],
        assistant_content: None,
        assistant_reasoning: None,
        tool_calls: vec![RawToolCall {
            id: tool_request.id,
            function_name: "wc -l".to_string(),
            arguments: "{}".to_string(),
        }],
        usage: None,
    };
}
```

- [ ] **Step 4: Run focused recovery and safety tests**

```bash
cargo test -p orca-tui tui_main_agent_recovers_from_unknown_tool_call -- --nocapture
cargo test -p orca-tui tui_child_agent_recovers_from_invalid_tool_arguments -- --nocapture
cargo test -p orca-runtime invalid_external_arguments_report_shared_validation_error -- --nocapture
```

Expected: all pass. The TUI conversation contains the original unknown-tool
failure and then the corrective assistant response.

- [ ] **Step 5: Commit end-to-end regression coverage**

```bash
git add crates/orca-provider/src/lib.rs crates/orca-tui/src/agent_runner.rs
git commit -m "test(tui): cover unknown tool correction"
```

### Task 3: Update Goal And Release Documentation

**Files:**
- Modify: `docs/goal-mode.md`
- Modify: `docs/production-roadmap.md`
- Modify: `docs/releases/v0.2.17.md`
- Modify: `site/src/changelog/Changelog.tsx`

- [ ] **Step 1: Document the recovery boundary**

In `docs/goal-mode.md`, retain the rule that actual failed turns stop automatic
continuation, then add that malformed/unknown provider tool names are recorded
as failed tool results and may be corrected within the same turn. State that
Orca never converts an unknown name into shell execution.

- [ ] **Step 2: Extend the current roadmap baseline**

Add to the `v0.2.17` baseline that unknown DeepSeek function names retain call
identity and raw arguments, fail registry validation before any side effect,
and return to the model for correction instead of becoming provider errors.

- [ ] **Step 3: Extend the release note and site summaries**

Add these release-note bullets:

- recoverable unknown tool calls;
- no command-shaped-name-to-bash coercion;
- matching call/result history and Goal continuation;
- incident-specific `wc -l` regression coverage.

Update both English and Chinese `v0.2.17` site summaries with the same user
outcome. Do not change `releaseVersion` or package versions.

- [ ] **Step 4: Verify documentation and site**

```bash
git diff --check
npm --prefix site run build
npm --prefix site run check:seo
```

Expected: all exit 0.

- [ ] **Step 5: Commit documentation**

```bash
git add docs/goal-mode.md docs/production-roadmap.md docs/releases/v0.2.17.md site/src/changelog/Changelog.tsx
git commit -m "docs(release): document unknown tool recovery"
```

### Task 4: Verify, Review, Integrate, And Publish v0.2.17

**Files:**
- Verify all files changed since `origin/main`
- Merge branch: `fix/deepseek-unknown-tool-recovery`
- Publish tag: `v0.2.17`

- [ ] **Step 1: Run the complete local release gate**

```bash
cargo fmt --all -- --check
git diff --check origin/main...HEAD
cargo check --workspace --all-targets
cargo test -p orca-provider -p orca-runtime -p orca-tui -- --test-threads=1
cargo test --workspace --all-targets -- --test-threads=1
cargo clippy --workspace --all-targets
npm --prefix site run build
npm --prefix site run check:seo
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
```

Expected: every command exits 0. Record test counts and any non-blocking
pre-existing warnings.

- [ ] **Step 2: Run the real DeepSeek release smoke**

Reuse the verified target so the smoke budget is spent on API behavior rather
than a cold build:

```bash
RELEASE_TARGET="$PWD/target"
CARGO_TARGET_DIR="$RELEASE_TARGET" \
  node scripts/release/real-api-e2e.mjs \
  --skip-build \
  --orca-bin "$RELEASE_TARGET/debug/orca" \
  --max-budget 0.02 \
  --timeout-ms 300000
```

Expected: provider summary, CLI, malformed-history resume, server thread, and
pagination scenarios all report verified sentinels.

- [ ] **Step 3: Request independent code review**

Review range:

```bash
BASE_SHA=$(git merge-base origin/main HEAD)
HEAD_SHA=$(git rev-parse HEAD)
```

The reviewer must check the incident requirements, no implicit shell mapping,
call/result history pairing, test fidelity, release metadata, and unrelated
worktree preservation. Fix all critical and important findings, then rerun the
affected focused and full gates.

- [ ] **Step 4: Fast-forward local main and verify the merged state**

From the primary checkout:

```bash
git merge --ff-only fix/deepseek-unknown-tool-recovery
cargo test -p orca-provider -p orca-runtime -p orca-tui -- --test-threads=1
git status --short --branch
```

Expected: main contains the Goal timer and unknown-tool commits, tests pass,
and the working tree is clean. Do not modify or delete the other worktrees.

- [ ] **Step 5: Push main and create the release tag**

```bash
git push origin main
git tag -a v0.2.17 -m "Orca v0.2.17"
git push origin v0.2.17
```

Expected: both pushes succeed without force.

- [ ] **Step 6: Watch the tag-driven release workflow**

```bash
RUN_ID=$(gh run list --repo echoVic/blade-deepseek --workflow Release --limit 20 \
  --json databaseId,headBranch,event \
  --jq '.[] | select(.headBranch == "v0.2.17" and .event == "push") | .databaseId' \
  | head -n 1)
gh run watch "$RUN_ID" --repo echoVic/blade-deepseek --exit-status
```

Expected: `test`, `version`, four native builds, `release`, `npm`, and
`npm-release-assets` all succeed.

- [ ] **Step 7: Verify public GitHub and npm artifacts**

```bash
node scripts/release/verify-published.mjs \
  --version 0.2.17 \
  --repo echoVic/blade-deepseek \
  --package @blade-ai/orca \
  --bin orca
gh release view v0.2.17 --repo echoVic/blade-deepseek --json url,assets
npm view @blade-ai/orca@0.2.17 version dist-tags --json
```

Expected: GitHub Release, npm wrapper/platform packages, and `npm exec` smoke
all verify; the release has native archives, checksums, and npm tarballs.

- [ ] **Step 8: Complete the active Goal only after the public audit**

Confirm every acceptance criterion in the design document against current
files, test output, Actions jobs, GitHub Release assets, npm metadata, and the
installed binary version. Only then mark the Goal complete.
