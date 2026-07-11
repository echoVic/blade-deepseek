# DeepSeek History Replay Validity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `v0.2.15` so TUI session resume rejects new reasoning-only DeepSeek assistant turns and repairs legacy reasoning-only turns before replay.

**Architecture:** Keep one assistant-payload invariant in `orca-core`, enforce it when DeepSeek responses are folded and when runtime history is recorded, then sanitize legacy transcripts during resume. Add a TUI-entry regression test and a real DeepSeek release smoke that resumes a synthetic malformed JSONL transcript.

**Tech Stack:** Rust 2024, Cargo tests, Orca JSONL history, Node.js release scripts, GitHub Actions, npm release verification.

---

## File Map

- `crates/orca-core/src/conversation.rs`: owns the shared replayable assistant-payload invariant and conversation normalization.
- `crates/orca-provider/src/deepseek_http.rs`: applies the invariant to streaming/non-streaming DeepSeek responses and API replay serialization.
- `crates/orca-runtime/src/session.rs`: rejects invalid assistant payloads before conversation/history mutation.
- `crates/orca-runtime/src/history.rs`: verifies legacy transcript resume sanitization.
- `crates/orca-tui/src/agent_runner.rs`: proves the actual `TuiConversationSession` preload path repairs malformed history.
- `scripts/release/real-api-e2e.mjs`: resumes a synthetic malformed transcript against the real DeepSeek API.
- `scripts/release/test-real-api-e2e.mjs`: verifies the release smoke invokes and validates the new resume scenario.
- `Cargo.toml`, `Cargo.lock`, `README.md`, `npm/orca/package.json`, `site/index.html`, `site/src/shared.ts`, `site/src/changelog/Changelog.tsx`: version and public release metadata.
- `docs/production-roadmap.md`, `docs/releases/v0.2.15.md`: architecture baseline, verification record, and release notes.

### Task 1: Audit The Existing Three-Boundary Implementation

**Files:**
- Verify: `crates/orca-core/src/conversation.rs`
- Verify: `crates/orca-provider/src/deepseek_http.rs`
- Verify: `crates/orca-runtime/src/session.rs`
- Verify: `crates/orca-runtime/src/history.rs`

- [ ] **Step 1: Confirm the shared invariant has one definition**

The implementation must keep this behavior in `orca-core`:

```rust
pub fn assistant_message_has_payload(content: Option<&str>, tool_calls: &[RawToolCall]) -> bool {
    content.is_some_and(|text| !text.trim().is_empty()) || !tool_calls.is_empty()
}
```

Run:

```bash
rg -n "assistant_message_has_payload" \
  crates/orca-core/src/conversation.rs \
  crates/orca-provider/src/deepseek_http.rs \
  crates/orca-runtime/src/session.rs
```

Expected: one function definition in `orca-core`; provider and runtime import and call it.

- [ ] **Step 2: Run core invariant tests separately**

Run:

```bash
cargo test -p orca-core assistant_message_payload_requires_content_or_tool_calls -- --nocapture
cargo test -p orca-core normalize_tool_boundaries_drops_reasoning_only_assistant -- --nocapture
```

Expected: both commands pass. Use separate commands because Cargo accepts one filter term.

- [ ] **Step 3: Run DeepSeek response and replay tests**

Run:

```bash
cargo test -p orca-provider reasoning_only -- --nocapture
cargo test -p orca-provider api_replay_preserves_reasoning_content_for_tool_call_turns -- --nocapture
```

Expected: reasoning-only streaming/non-streaming responses are rejected after retry, while valid reasoning plus tool calls still replays.

- [ ] **Step 4: Run runtime persistence and resume tests separately**

Run:

```bash
cargo test -p orca-runtime record_assistant_response_rejects_empty_payload -- --nocapture
cargo test -p orca-runtime resume_drops_reasoning_only_assistant -- --nocapture
```

Expected: both commands pass; the record test leaves the conversation unchanged and resume keeps the later user turn.

- [ ] **Step 5: Confirm the implementation commit is isolated**

Run:

```bash
git show --stat --oneline 9a59d7afb
git diff --check origin/main..9a59d7afb
```

Expected: only the four invariant/provider/runtime/history files are in the feature commit and the diff is whitespace-clean.

### Task 2: Add A TUI Resume Regression Test

**Files:**
- Modify: `crates/orca-tui/src/agent_runner.rs`

- [ ] **Step 1: Add the TUI preload test**

Add this test beside `tui_session_reuses_conversation_across_submits`:

```rust
#[test]
fn tui_resume_drops_reasoning_only_assistant_turn() {
    let config = config();
    let cwd = std::env::current_dir().expect("current dir");
    let transcript = orca_runtime::history::SessionTranscript {
        meta: orca_runtime::history::create_meta(
            &cwd,
            "deepseek",
            None,
            "resume reasoning-only history",
        ),
        messages: vec![
            orca_core::conversation::Message::user("first".to_string()),
            orca_core::conversation::Message::Assistant {
                content: None,
                reasoning_content: Some("synthetic private reasoning".to_string()),
                tool_calls: vec![],
                pinned: false,
            },
            orca_core::conversation::Message::user("second".to_string()),
        ],
        compactions: Vec::new(),
        summaries: Vec::new(),
        usage: None,
        plan: None,
        completion_status: None,
        path: cwd.join("reasoning-only-tui.jsonl"),
    };

    let session = TuiConversationSession::new_with_preloaded(
        &config,
        "resume reasoning-only history",
        Some(transcript),
    )
    .expect("TUI session resumes malformed legacy history");

    assert!(!session.conversation().messages.iter().any(|message| matches!(
        message,
        orca_core::conversation::Message::Assistant {
            content: None,
            tool_calls,
            ..
        } if tool_calls.is_empty()
    )));
    assert!(session.conversation().messages.iter().any(|message| matches!(
        message,
        orca_core::conversation::Message::User { content, .. } if content == "second"
    )));
}
```

- [ ] **Step 2: Prove the regression test is red on the release parent**

Create a patch containing only the new test, apply it to a disposable detached worktree at `v0.2.14`, and run the test:

```bash
git diff -- crates/orca-tui/src/agent_runner.rs > /tmp/history-replay-tui-test.patch
git worktree add --detach /tmp/blade-deepseek-history-replay-red a260e98dd
git -C /tmp/blade-deepseek-history-replay-red apply /tmp/history-replay-tui-test.patch
cargo test --manifest-path /tmp/blade-deepseek-history-replay-red/Cargo.toml \
  -p orca-tui tui_resume_drops_reasoning_only_assistant_turn -- --nocapture
```

Expected: FAIL because the reasoning-only assistant remains in the preloaded conversation.

Clean up only the disposable worktree created in this step:

```bash
git worktree remove --force /tmp/blade-deepseek-history-replay-red
rm -f /tmp/history-replay-tui-test.patch
```

- [ ] **Step 3: Run the test green on the feature branch**

Run:

```bash
cargo test -p orca-tui tui_resume_drops_reasoning_only_assistant_turn -- --nocapture
```

Expected: PASS.

- [ ] **Step 4: Commit the TUI regression coverage**

```bash
git add crates/orca-tui/src/agent_runner.rs
git commit -m "test(tui): cover malformed history resume"
```

### Task 3: Add A Real DeepSeek Malformed-History Resume Gate

**Files:**
- Modify: `scripts/release/test-real-api-e2e.mjs`
- Modify: `scripts/release/real-api-e2e.mjs`

- [ ] **Step 1: Extend the release-script test first**

In the fake Orca executable, choose the emitted sentinel from the requested prompt:

```javascript
if (args[0] === "exec") {
  const text = args.join(" ").includes("ORCA_HISTORY_REPLAY_OK")
    ? "ORCA_HISTORY_REPLAY_OK"
    : "ORCA_REAL_E2E_OK";
  process.stdout.write(JSON.stringify({
    type: "assistant.message.delta",
    payload: { text },
  }) + "\n");
  process.stdout.write('{"type":"session.completed","payload":{"status":"success"}}\n');
  process.exit(0);
}
```

Add `History replay real API e2e verified: ORCA_HISTORY_REPLAY_OK` to the expected output list, and add the expected `orca exec ... --resume latest ...` command to the call-log assertions.

- [ ] **Step 2: Run the release-script test red**

Run:

```bash
node scripts/release/test-real-api-e2e.mjs
```

Expected: FAIL because `real-api-e2e.mjs` does not yet run or report the history replay scenario.

- [ ] **Step 3: Add the synthetic transcript helper and smoke**

Update imports in `real-api-e2e.mjs`:

```javascript
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
```

Add a sentinel:

```javascript
const historyReplaySentinel = "ORCA_HISTORY_REPLAY_OK";
```

Add this function after `runCli`:

```javascript
function runHistoryReplay(args) {
  if (args.skipCli) {
    console.log("History replay real API e2e skipped");
    return;
  }

  const home = mkdtempSync(path.join(os.tmpdir(), "orca-history-replay-e2e-"));
  const sessionDir = path.join(home, "sessions", "2026", "07", "11");
  const sessionPath = path.join(sessionDir, "session-2026-07-11T00-00-00-history-replay-e2e.jsonl");
  mkdirSync(sessionDir, { recursive: true });
  const records = [
    {
      type: "session.meta",
      schema_version: 1,
      session_id: "history-replay-e2e",
      cwd: repoRoot,
      provider: "deepseek",
      model: "deepseek-v4-flash",
      title: "History replay validity e2e",
      created_at: "2026-07-11T00:00:00Z",
    },
    {
      type: "conversation.message",
      message: { role: "user", content: "Legacy valid user context.", pinned: false },
    },
    {
      type: "conversation.message",
      message: {
        role: "assistant",
        content: null,
        reasoning_content: "synthetic incomplete reasoning",
        tool_calls: [],
        pinned: false,
      },
    },
  ];
  writeFileSync(sessionPath, `${records.map((record) => JSON.stringify(record)).join("\n")}\n`);

  try {
    const output = run(
      args.orcaBin,
      [
        "exec",
        "--output-format",
        "jsonl",
        "--mode",
        "suggest",
        "--max-budget",
        args.maxBudget,
        "--resume",
        "latest",
        `Reply with exactly: ${historyReplaySentinel}`,
      ],
      {
        env: { ...process.env, ORCA_HOME: home },
        timeoutMs: args.timeoutMs,
      },
    );
    const events = parseJsonLines(output, "history replay CLI");
    const text = events
      .filter((event) => event.type === "assistant.message.delta")
      .map((event) => event.payload?.text ?? "")
      .join("");
    if (!text.includes(historyReplaySentinel)) {
      throw new Error(`History replay real API e2e missing sentinel ${historyReplaySentinel}:\n${output}`);
    }
    assertStatus(
      events,
      "type",
      "session.completed",
      ["payload", "status"],
      "History replay real API e2e",
    );
    console.log(`History replay real API e2e verified: ${historyReplaySentinel}`);
  } finally {
    rmSync(home, { recursive: true, force: true });
  }
}
```

Call `runHistoryReplay(args)` after `runCli(args)` in `main()`.

- [ ] **Step 4: Run the release-script test green**

Run:

```bash
node scripts/release/test-real-api-e2e.mjs
```

Expected: `real-api-e2e release checks ok`.

- [ ] **Step 5: Run the new real API path**

Run:

```bash
node scripts/release/real-api-e2e.mjs \
  --skip-provider-summary \
  --skip-server \
  --max-budget 0.02
```

Expected output includes:

```text
CLI real API e2e verified: ORCA_REAL_E2E_OK
History replay real API e2e verified: ORCA_HISTORY_REPLAY_OK
```

- [ ] **Step 6: Commit the release verification improvement**

```bash
git add scripts/release/real-api-e2e.mjs scripts/release/test-real-api-e2e.mjs
git commit -m "test(release): verify malformed history resume"
```

### Task 4: Run Focused And Full Verification

**Files:**
- Verify all touched Rust and release-script files.

- [ ] **Step 1: Run focused history/provider/TUI tests**

```bash
cargo test -p orca-core assistant_message_payload_requires_content_or_tool_calls -- --nocapture
cargo test -p orca-core normalize_tool_boundaries_drops_reasoning_only_assistant -- --nocapture
cargo test -p orca-provider reasoning_only -- --nocapture
cargo test -p orca-provider api_replay_preserves_reasoning_content_for_tool_call_turns -- --nocapture
cargo test -p orca-runtime record_assistant_response_rejects_empty_payload -- --nocapture
cargo test -p orca-runtime resume_drops_reasoning_only_assistant -- --nocapture
cargo test -p orca-tui tui_resume_drops_reasoning_only_assistant_turn -- --nocapture
cargo test -p blade-deepseek --test history_contract exec_resume_injects_prior_conversation -- --nocapture
node scripts/release/test-real-api-e2e.mjs
```

Expected: all commands pass.

- [ ] **Step 2: Run formatting and whitespace gates**

```bash
cargo fmt -- --check
git diff --check
```

Expected: exit code 0.

- [ ] **Step 3: Run the complete release gate**

```bash
node scripts/release/real-api-e2e.mjs --max-budget 0.02
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets
npm --prefix site run build
npm --prefix site run check:seo
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
```

Expected: all commands exit 0. Existing clippy warnings may remain, but no new error is allowed.

### Task 5: Prepare The `v0.2.15` Release Commit

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `README.md`
- Modify: `npm/orca/package.json`
- Modify: `site/index.html`
- Modify: `site/src/shared.ts`
- Modify: `site/src/changelog/Changelog.tsx`
- Modify: `docs/production-roadmap.md`
- Create: `docs/releases/v0.2.15.md`

- [ ] **Step 1: Bump public version references to `0.2.15`**

Update the package versions and installer example. Add `v0.2.15` as the first release in `site/src/shared.ts` while retaining `v0.2.14` in release history.

- [ ] **Step 2: Add English and Chinese changelog summaries**

Use this release meaning:

```text
Orca now rejects incomplete DeepSeek assistant turns that contain reasoning but no visible content or tool calls. TUI session resume also removes legacy reasoning-only turns in memory before replay, so an old malformed transcript can continue without rewriting the source JSONL file.
```

The Chinese summary must communicate the same behavior and must not claim that valid reasoning is removed.

- [ ] **Step 3: Update the roadmap baseline**

Add `v0.2.15` above the `v0.2.14` baseline in `docs/production-roadmap.md`. Tie the change to TUI resume reliability and DeepSeek replay validity. Keep the separate compaction-start work out of this release.

- [ ] **Step 4: Write release notes**

Create `docs/releases/v0.2.15.md` with:

- the three assistant-payload enforcement boundaries;
- transparent TUI legacy-session recovery;
- preserved valid reasoning/tool-call replay;
- focused/full/real-API verification commands;
- post-publish verification command;
- installation commands pinned to `0.2.15`.

- [ ] **Step 5: Re-run version-sensitive gates**

```bash
cargo fmt -- --check
git diff --check
npm --prefix site run build
npm --prefix site run check:seo
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
```

Expected: all pass with `0.2.15` metadata.

- [ ] **Step 6: Commit release metadata**

```bash
git add \
  Cargo.toml Cargo.lock README.md npm/orca/package.json \
  site/index.html site/src/shared.ts site/src/changelog/Changelog.tsx \
  docs/production-roadmap.md docs/releases/v0.2.15.md
git commit -m "docs(release): prepare v0.2.15"
```

### Task 6: Publish And Verify Public Artifacts

**Files:**
- No new source files. Operates on the verified commits and remote release state.

- [ ] **Step 1: Confirm the branch is clean and based on current remote main**

```bash
git status --short --branch
git fetch origin main --tags
git merge-base HEAD origin/main
git log --oneline origin/main..HEAD
```

Expected: clean worktree; the feature, spec, TUI test, release-smoke, and release commits are the only commits ahead of `origin/main`.

- [ ] **Step 2: Push main and tag**

```bash
git push origin HEAD:main
git tag v0.2.15
git push origin v0.2.15
```

Expected: both pushes succeed without force.

- [ ] **Step 3: Wait for the tag-driven Release workflow**

```bash
gh run list --repo echoVic/blade-deepseek --workflow Release --limit 5
RUN_ID="$(gh run list --repo echoVic/blade-deepseek --workflow Release \
  --limit 1 --json databaseId --jq '.[0].databaseId')"
gh run watch "$RUN_ID" --repo echoVic/blade-deepseek --exit-status
```

Expected: test, four platform builds, GitHub Release, npm publish, npm verification, and npm release assets all succeed.

- [ ] **Step 4: Verify public GitHub/npm artifacts independently**

```bash
node scripts/release/verify-published.mjs \
  --version 0.2.15 \
  --repo echoVic/blade-deepseek \
  --package @blade-ai/orca \
  --bin orca
gh release view v0.2.15 --repo echoVic/blade-deepseek \
  --json tagName,url,isDraft,isPrerelease,publishedAt,assets
npm view @blade-ai/orca@0.2.15 version --json
```

Expected: GitHub Release is public and non-draft, npm reports `0.2.15`, and `npm exec` reports `orca 0.2.15`.

- [ ] **Step 5: Verify Pages deployment and final refs**

```bash
gh run list --repo echoVic/blade-deepseek --workflow pages.yml --limit 5
git ls-remote origin refs/heads/main refs/tags/v0.2.15
git status --short --branch
```

Expected: Pages run for the release commit succeeds; remote main and tag point to the release commit; local worktree is clean.
