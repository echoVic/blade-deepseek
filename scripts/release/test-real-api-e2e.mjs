#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..", "..");
const script = path.join(repoRoot, "scripts", "release", "real-api-e2e.mjs");
const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-real-api-e2e-test-"));

function writeExecutable(filePath, contents) {
  writeFileSync(filePath, contents);
  chmodSync(filePath, 0o755);
}

try {
  const binDir = path.join(tempDir, "bin");
  mkdirSync(binDir, { recursive: true });
  const logPath = path.join(tempDir, "calls.log");

  writeExecutable(
    path.join(binDir, "cargo"),
    `#!/bin/sh
printf 'cargo %s\\n' "$*" >> "${logPath}"
case "$*" in
  "build --bin orca") exit 0 ;;
  "run -p orca-provider --example summary_render_realapi")
    printf '== Acceptance ==\\n'
    printf 'ALL TARGETS MET\\n'
    ;;
  "run -p orca-runtime --example goal_mode_realapi -- --max-budget 0.01")
    printf 'Goal Mode real API e2e verified: status=complete non_goal_tools=1 update_goal_calls=1 continuations=0\\n'
    ;;
  *) exit 42 ;;
esac
`,
  );

  const orcaBin = path.join(binDir, "orca");
  writeExecutable(
    orcaBin,
    `#!/usr/bin/env node
import { appendFileSync, readFileSync, writeFileSync } from "node:fs";
import readline from "node:readline";

const logPath = ${JSON.stringify(logPath)};
const args = process.argv.slice(2);
appendFileSync(logPath, \`orca \${args.join(" ")}\\n\`);

if (process.env.ORCA_FAKE_BAD_CLI === "1" && args[0] === "exec") {
  process.stdout.write('{"type":"assistant.message.delta","payload":{"text":"WRONG"}}\\n');
  process.stdout.write('{"type":"session.completed","payload":{"status":"success"}}\\n');
  process.exit(0);
}

if (args[0] === "exec") {
  const isHistoryReplay = args.join(" ").includes("ORCA_HISTORY_REPLAY_OK");
  if (isHistoryReplay) {
    const historyPath = process.env.ORCA_HOME + "/sessions/2026/07/11/session-2026-07-11T00-00-00-history-replay-e2e.jsonl";
    const records = readFileSync(historyPath, "utf8")
      .trim()
      .split(/\\r?\\n/)
      .map((line) => JSON.parse(line));
    const callId = "legacy-missing-tool-call";
    const hasCall = records.some((record) =>
      record.type === "conversation.message" &&
      record.message?.role === "assistant" &&
      record.message?.tool_calls?.some?.((call) => call.id === callId)
    );
    const hasResult = records.some((record) =>
      record.type === "conversation.message" &&
      record.message?.role === "tool" &&
      record.message?.tool_call_id === callId
    );
    if (!hasCall || hasResult) {
      process.stderr.write("history replay fixture must contain one unmatched tool call\\n");
      process.exit(44);
    }
    appendFileSync(logPath, "history-replay-fixture " + callId + " missing-result\\n");
  }
  const stableIdentity = args.join(" ").match(/ORCA_STABLE_THREAD_IDENTITY_OK_\\d+_\\d+/)?.[0];
  const stableThreadId = "run-stable-thread-test";
  const resumeIndex = args.indexOf("--resume");
  const isStableResume = resumeIndex >= 0 && args[resumeIndex + 1] === stableThreadId;
  if (stableIdentity || isStableResume) {
    const statePath = process.env.ORCA_HOME + "/stable-identity-state.json";
    const state = isStableResume
      ? JSON.parse(readFileSync(statePath, "utf8"))
      : { phase: 1, sentinel: stableIdentity };
    if (isStableResume) {
      state.phase = 2;
    }
    writeFileSync(statePath, JSON.stringify(state));
    const turnId = isStableResume ? "turn_00000000000000000000000002" : "turn_00000000000000000000000001";
    process.stdout.write(JSON.stringify({
      version: "1",
      run_id: stableThreadId,
      seq: 0,
      type: "session.started",
      payload: {}
    }) + "\\n");
    process.stdout.write(JSON.stringify({
      version: "1",
      run_id: stableThreadId,
      seq: 1,
      type: "turn.started",
      payload: { turn_id: turnId }
    }) + "\\n");
    process.stdout.write(JSON.stringify({
      version: "1",
      run_id: stableThreadId,
      seq: 2,
      type: "assistant.message.delta",
      payload: { text: isStableResume ? state.sentinel : "ORCA_STABLE_THREAD_IDENTITY_READY" }
    }) + "\\n");
    process.stdout.write(JSON.stringify({
      version: "1",
      run_id: stableThreadId,
      seq: 3,
      type: "session.completed",
      payload: { status: "success" }
    }) + "\\n");
    process.exit(0);
  }
  const text = isHistoryReplay ? "ORCA_HISTORY_REPLAY_OK" : "ORCA_REAL_E2E_OK";
  process.stdout.write(JSON.stringify({
    type: "assistant.message.delta",
    payload: { text },
  }) + "\\n");
  process.stdout.write('{"type":"session.completed","payload":{"status":"success"}}\\n');
  process.exit(0);
}

if (args[0] === "--mode" && args[1] === "server") {
  const rl = readline.createInterface({ input: process.stdin });
  let serverThreadSentinel = "ORCA_SERVER_THREAD_MEMORY_OK";
  const activeResumeTurnId = "turn_logical_resume_test";
  const activeResumeTaskId = "task-runtime-resume-test";
  rl.on("line", (line) => {
    appendFileSync(logPath, \`server-stdin \${line}\\n\`);
    const request = JSON.parse(line);
    if (request.op === "submit") {
      process.stdout.write(JSON.stringify({ id: request.id, event: "message_delta", text: "ORCA_" }) + "\\n");
      process.stdout.write(JSON.stringify({ id: request.id, event: "message_delta", text: "SERVER_REAL_OK" }) + "\\n");
      process.stdout.write(JSON.stringify({ id: request.id, event: "turn_completed", status: "success" }) + "\\n");
      return;
    }
    if (request.method === "thread/start") {
      const threadId = request.id === "server-thread-extra"
        ? "thread-test-extra"
        : request.id === "server-resume-thread"
          ? "thread-test-resume"
          : "thread-test";
      process.stdout.write(JSON.stringify({ id: request.id, event: "thread_started", threadId }) + "\\n");
      return;
    }
    if (request.method === "turn/start" && request.id === "server-thread-turn-1") {
      const text = request.params.input?.find?.((item) => item.type === "text")?.text ?? "";
      serverThreadSentinel = text.match(/ORCA_SERVER_THREAD_MEMORY_OK_[^ .]+/)?.[0] ?? serverThreadSentinel;
      process.stdout.write(JSON.stringify({ id: request.id, event: "message_delta", text: "READY" }) + "\\n");
      process.stdout.write(JSON.stringify({ id: request.id, event: "turn_completed", status: "success" }) + "\\n");
      return;
    }
    if (request.method === "turn/start" && request.id === "server-thread-turn-2") {
      process.stdout.write(JSON.stringify({ id: request.id, event: "message_delta", text: serverThreadSentinel }) + "\\n");
      process.stdout.write(JSON.stringify({ id: request.id, event: "turn_completed", status: "success" }) + "\\n");
      return;
    }
    if (request.method === "turn/start" && request.id === "server-resume-turn") {
      process.stdout.write(JSON.stringify({
        id: request.id,
        event: "turn_started",
        turnId: activeResumeTurnId,
        task: { task_id: activeResumeTaskId }
      }) + "\\n");
      process.stdout.write(JSON.stringify({ id: request.id, event: "reasoning_delta", text: "first generation" }) + "\\n");
      return;
    }
    if (request.method === "turn/start" && request.id === "server-thread-extra-turn") {
      process.stdout.write(JSON.stringify({ id: request.id, event: "message_delta", text: serverThreadSentinel }) + "\\n");
      process.stdout.write(JSON.stringify({ id: request.id, event: "turn_completed", status: "success" }) + "\\n");
      return;
    }
    if (request.method === "turn/interrupt" || request.method === "turn/resume" || request.method === "turn/steer") {
      const action = request.method.split("/")[1];
      const isActiveResumeControl = request.id === "server-resume-interrupt" || request.id === "server-resume-resume";
      process.stdout.write(JSON.stringify({
        id: request.id,
        event: "turn_controlled",
        action,
        turnId: request.params.turnId,
        status: isActiveResumeControl ? (action === "interrupt" ? "interrupted" : "resumed") : "idle",
        input: request.params.input?.filter?.((item) => item.type === "text").map((item) => item.text).join("\\n") ?? null
      }) + "\\n");
      if (request.id === "server-resume-resume") {
        const text = readFileSync(logPath, "utf8").match(/ORCA_SERVER_RESUME_OK_\\d+_\\d+/)?.[0] ?? "ORCA_SERVER_RESUME_OK";
        process.stdout.write(JSON.stringify({ id: "server-resume-turn", event: "message_delta", text }) + "\\n");
        process.stdout.write(JSON.stringify({ id: "server-resume-turn", event: "turn_completed", status: "success" }) + "\\n");
      }
      return;
    }
    if (request.method === "thread/metadata/update") {
      process.stdout.write(JSON.stringify({
        id: request.id,
        event: "thread_metadata_updated",
        threadId: request.params.threadId,
        title: request.params.title
      }) + "\\n");
      return;
    }
    if (request.method === "thread/list") {
      const isMetadataFilter = request.id === "server-thread-list-metadata-filter";
      const isMetadataFilterMiss = request.id === "server-thread-list-metadata-filter-miss";
      const listData = isMetadataFilterMiss ? [] : isMetadataFilter ? [
        {
          threadId: "thread-test-extra",
          title: \`ORCA server extra thread \${serverThreadSentinel}\`,
          cwd: "/tmp/fake",
          provider: "deepseek",
          model: "deepseek-v4-flash",
          createdAt: "2026-06-27T00:00:02Z",
          updatedAt: "2026-06-27T00:00:03Z",
          archived: false,
          parentId: null,
          forked: false
        },
        {
          threadId: "thread-test",
          title: \`ORCA server thread metadata e2e \${serverThreadSentinel}\`,
          cwd: "/tmp/fake",
          provider: "deepseek",
          model: "deepseek-v4-flash",
          createdAt: "2026-06-27T00:00:00Z",
          updatedAt: "2026-06-27T00:00:01Z",
          archived: false,
          parentId: null,
          forked: false
        }
      ] : request.params.cursor ? [
        {
          threadId: "thread-test",
          title: \`ORCA server thread metadata e2e \${serverThreadSentinel}\`,
          cwd: "/tmp/fake",
          provider: "deepseek",
          model: "deepseek-v4-flash",
          createdAt: "2026-06-27T00:00:00Z",
          updatedAt: "2026-06-27T00:00:01Z",
          archived: false,
          parentId: null,
          forked: false
        }
      ] : [
        {
          threadId: "thread-test-extra",
          title: \`ORCA server extra thread \${serverThreadSentinel}\`,
          cwd: "/tmp/fake",
          provider: "deepseek",
          model: "deepseek-v4-flash",
          createdAt: "2026-06-27T00:00:02Z",
          updatedAt: "2026-06-27T00:00:03Z",
          archived: false,
          parentId: null,
          forked: false
        }
      ];
      process.stdout.write(JSON.stringify({
        id: request.id,
        event: "thread_list",
        data: listData,
        nextCursor: request.params.cursor || isMetadataFilter || isMetadataFilterMiss ? null : "1",
        backwardsCursor: request.params.cursor ?? "0"
      }) + "\\n");
      return;
    }
    if (request.method === "thread/search") {
      const searchData = request.params.cursor ? [
        {
          thread: {
            threadId: "thread-test",
            title: \`ORCA server thread metadata e2e \${serverThreadSentinel}\`,
            cwd: "/tmp/fake",
            provider: "deepseek",
            model: "deepseek-v4-flash",
            createdAt: "2026-06-27T00:00:00Z",
            updatedAt: "2026-06-27T00:00:01Z",
            archived: false,
            parentId: null,
            forked: false
          },
          snippet: \`Remember this exact token for the next turn: \${serverThreadSentinel}.\`
        }
      ] : [
        {
          thread: {
            threadId: "thread-test-extra",
          title: \`ORCA server extra thread \${serverThreadSentinel}\`,
            cwd: "/tmp/fake",
            provider: "deepseek",
            model: "deepseek-v4-flash",
            createdAt: "2026-06-27T00:00:02Z",
            updatedAt: "2026-06-27T00:00:03Z",
            archived: false,
            parentId: null,
            forked: false
          },
          snippet: \`Reply with exactly this token for list pagination coverage: \${serverThreadSentinel}.\`
        }
      ];
      process.stdout.write(JSON.stringify({
        id: request.id,
        event: "thread_search",
        data: searchData,
        nextCursor: request.params.cursor ? null : "1",
        backwardsCursor: request.params.cursor ?? "0"
      }) + "\\n");
      return;
    }
    if (request.method === "thread/turns/list" && request.params.threadId === "run-stable-thread-test") {
      const state = JSON.parse(readFileSync(process.env.ORCA_HOME + "/stable-identity-state.json", "utf8"));
      const data = [
        {
          threadId: "run-stable-thread-test",
          turnId: "turn_00000000000000000000000001",
          index: 0,
          role: "user",
          itemsView: "full",
          items: []
        }
      ];
      if (state.phase === 2) {
        data.push({
          threadId: "run-stable-thread-test",
          turnId: "turn_00000000000000000000000002",
          index: 1,
          role: "user",
          itemsView: "full",
          items: []
        });
      }
      process.stdout.write(JSON.stringify({
        id: request.id,
        event: "thread_turns_list",
        data,
        nextCursor: null,
        backwardsCursor: "0"
      }) + "\\n");
      return;
    }
    if (request.method === "thread/items/list" && request.params.threadId === "run-stable-thread-test") {
      const state = JSON.parse(readFileSync(process.env.ORCA_HOME + "/stable-identity-state.json", "utf8"));
      const data = [
        {
          threadId: "run-stable-thread-test",
          turnId: "turn_00000000000000000000000001",
          itemId: "item_00000000000000000000000001",
          index: 0,
          item: { role: "user", content: "remember " + state.sentinel }
        },
        {
          threadId: "run-stable-thread-test",
          turnId: "turn_00000000000000000000000001",
          itemId: "item_00000000000000000000000002",
          index: 1,
          item: {
            id: "item_00000000000000000000000002",
            type: "agent_message",
            text: "ORCA_STABLE_THREAD_IDENTITY_READY"
          }
        },
        {
          threadId: "run-stable-thread-test",
          turnId: "turn_00000000000000000000000001",
          itemId: "item_00000000000000000000000003",
          index: 2,
          item: {
            id: "item_00000000000000000000000003",
            type: "reasoning",
            summary: "remembering the requested token",
            content: ""
          }
        }
      ];
      if (state.phase === 2) {
        data.push(
          {
            threadId: "run-stable-thread-test",
            turnId: "turn_00000000000000000000000002",
            itemId: "item_00000000000000000000000004",
            index: 3,
            item: { role: "user", content: "recall the prior token" }
          },
          {
            threadId: "run-stable-thread-test",
            turnId: "turn_00000000000000000000000002",
            itemId: "item_00000000000000000000000005",
            index: 4,
            item: {
              id: "item_00000000000000000000000005",
              type: "agent_message",
              text: state.sentinel
            }
          },
          {
            threadId: "run-stable-thread-test",
            turnId: "turn_00000000000000000000000002",
            itemId: "item_00000000000000000000000006",
            index: 5,
            item: {
              id: "item_00000000000000000000000006",
              type: "reasoning",
              summary: "recalling the requested token",
              content: ""
            }
          }
        );
      }
      process.stdout.write(JSON.stringify({
        id: request.id,
        event: "thread_items_list",
        data,
        nextCursor: null,
        backwardsCursor: "0"
      }) + "\\n");
      return;
    }
    if (request.method === "thread/turns/list") {
      const descTurns = request.params.sortDirection === "desc";
      const notLoadedTurns = request.params.itemsView === "notLoaded";
      process.stdout.write(JSON.stringify({
        id: request.id,
        event: "thread_turns_list",
        data: notLoadedTurns ? [
          {
            threadId: "thread-test",
            turnId: "turn-1",
            index: 0,
            role: "user",
            itemsView: "notLoaded",
            items: []
          }
        ] : descTurns ? [
          {
            threadId: "thread-test",
            turnId: "turn-2",
            index: 1,
            role: "user",
            itemsView: "full",
            items: [
              { role: "user", content: "Reply with exactly the token I asked you to remember." },
              { id: "item_00000000000000000000000012", type: "agent_message", text: serverThreadSentinel }
            ]
          }
        ] : request.params.cursor ? [
            {
              threadId: "thread-test",
              turnId: "turn-2",
              index: 1,
              role: "user",
              itemsView: "full",
              items: [
                { role: "user", content: "Reply with exactly the token I asked you to remember." },
                { id: "item_00000000000000000000000012", type: "agent_message", text: serverThreadSentinel }
              ]
            }
          ] : [
            {
            threadId: "thread-test",
            turnId: "turn-1",
            index: 0,
            role: "user",
            itemsView: "full",
            items: [
              { role: "user", content: \`Remember this exact token for the next turn: \${serverThreadSentinel}. Reply with exactly: READY.\` },
              { id: "item_00000000000000000000000011", type: "agent_message", text: "READY" }
            ]
          },
        ],
        nextCursor: request.params.cursor ? null : "1",
        backwardsCursor: request.params.cursor ?? "0"
      }) + "\\n");
      return;
    }
    if (request.method === "thread/items/list") {
      if (request.params.threadId === "history-replay-e2e") {
        process.stdout.write(JSON.stringify({
          id: request.id,
          event: "thread_items_list",
          data: [
            {
              threadId: "history-replay-e2e",
              turnId: "turn-1",
              itemId: "legacy-missing-tool-call",
              index: 1,
              item: {
                id: "legacy-missing-tool-call",
                type: "commandExecution",
                status: "indeterminate",
                kind: "indeterminate",
                terminalSource: "compatibility_repair"
              }
            }
          ],
          nextCursor: null,
          backwardsCursor: "0"
        }) + "\\n");
        return;
      }
      const descItems = request.params.sortDirection === "desc";
      process.stdout.write(JSON.stringify({
        id: request.id,
        event: "thread_items_list",
        data: descItems ? [
          {
            threadId: "thread-test",
            turnId: "turn-2",
            itemId: "item_00000000000000000000000012",
            index: 3,
            item: { id: "item_00000000000000000000000012", type: "agent_message", text: serverThreadSentinel }
          }
        ] : request.params.cursor ? [
          {
            threadId: "thread-test",
            turnId: "turn-2",
            itemId: "item-3",
            index: 2,
            item: { role: "user", content: "Reply with exactly the token I asked you to remember." }
          },
          {
            threadId: "thread-test",
            turnId: "turn-2",
            itemId: "item_00000000000000000000000012",
            index: 3,
            item: { id: "item_00000000000000000000000012", type: "agent_message", text: serverThreadSentinel }
          }
        ] : [
          {
            threadId: "thread-test",
            turnId: "turn-1",
            itemId: "item-1",
            index: 0,
            item: { role: "user", content: \`Remember this exact token for the next turn: \${serverThreadSentinel}. Reply with exactly: READY.\` }
          },
          {
            threadId: "thread-test",
            turnId: "turn-1",
            itemId: "item_00000000000000000000000011",
            index: 1,
            item: { id: "item_00000000000000000000000011", type: "agent_message", text: "READY" }
          }
        ],
        nextCursor: request.params.cursor ? null : "2",
        backwardsCursor: request.params.cursor ?? "0"
      }) + "\\n");
      return;
    }
    if (request.method === "thread/read") {
      process.stdout.write(JSON.stringify({
        id: request.id,
        event: "thread_read",
        threadId: request.params.threadId,
        title: \`ORCA server thread metadata e2e \${serverThreadSentinel}\`,
        cwd: "/tmp/fake",
        messageCount: 5,
        messages: [
          { role: "system", content: "system" },
          { role: "user", content: \`Remember this exact token for the next turn: \${serverThreadSentinel}. Reply with exactly: READY.\` },
          { role: "assistant", content: "READY" },
          { role: "user", content: "Reply with exactly the token I asked you to remember." },
          { role: "assistant", content: serverThreadSentinel }
        ],
        turns: [
          {
            threadId: "thread-test",
            turnId: "turn-1",
            index: 0,
            role: "user",
            items: [
              { role: "user", content: \`Remember this exact token for the next turn: \${serverThreadSentinel}. Reply with exactly: READY.\` },
              { id: "item_00000000000000000000000011", type: "agent_message", text: "READY" }
            ]
          },
          {
            threadId: "thread-test",
            turnId: "turn-2",
            index: 1,
            role: "user",
            items: [
              { role: "user", content: "Reply with exactly the token I asked you to remember." },
              { id: "item_00000000000000000000000012", type: "agent_message", text: serverThreadSentinel }
            ]
          }
        ]
      }) + "\\n");
      return;
    }
    process.stdout.write(JSON.stringify({ id: request.id, event: "error", message: "unexpected request" }) + "\\n");
  });
  rl.on("close", () => process.exit(0));
} else {
  process.exit(43);
}
`,
  );

  const output = execFileSync(
    "node",
    [
      script,
      "--orca-bin",
      orcaBin,
      "--max-budget",
      "0.01",
    ],
    {
      cwd: repoRoot,
      env: {
        ...process.env,
        PATH: `${binDir}${path.delimiter}${process.env.PATH}`,
      },
      encoding: "utf8",
    },
  );

  for (const expected of [
    "Build verified",
    "Provider summary real API e2e verified",
    "Goal Mode real API e2e verified: status=complete non_goal_tools=1 update_goal_calls=1 continuations=0",
    "CLI real API e2e verified: ORCA_REAL_E2E_OK",
    "History replay real API e2e verified: ORCA_HISTORY_REPLAY_OK",
    "History replay repair verified: legacy-missing-tool-call status=indeterminate terminalSource=compatibility_repair",
    "History replay invocation not re-executed: legacy-missing-tool-call",
    "Stable thread identity resume real API e2e verified: ORCA_STABLE_THREAD_IDENTITY_OK_",
    "Stable completed model item objects preserved: 3",
    "Server real API e2e verified: ORCA_SERVER_REAL_OK",
    "Server thread real API e2e verified: ORCA_SERVER_THREAD_MEMORY_OK_",
    "Server active turn resume e2e verified: ORCA_SERVER_RESUME_OK_",
    "Server thread/read e2e verified",
    "Server thread/metadata/update e2e verified",
    "Server turn controls e2e verified",
    "Server thread/list e2e verified",
    "Server thread/list metadata filters e2e verified",
    "Server thread/search e2e verified",
    "Server thread/turns/list e2e verified",
    "Server thread/items/list e2e verified",
  ]) {
    if (!output.includes(expected)) {
      throw new Error(`missing output ${expected}:\n${output}`);
    }
  }

  const log = readFileSync(logPath, "utf8");
  const tokenMatch = log.match(/ORCA_SERVER_THREAD_MEMORY_OK_\d+_\d+/);
  if (!tokenMatch) {
    throw new Error(`missing unique server thread token in log:\n${log}`);
  }
  const serverThreadSentinel = tokenMatch[0];
  const resumeTokenMatch = log.match(/ORCA_SERVER_RESUME_OK_\d+_\d+/);
  if (!resumeTokenMatch) {
    throw new Error(`missing unique server resume token in log:\n${log}`);
  }
  const serverResumeSentinel = resumeTokenMatch[0];
  const stableIdentityTokenMatch = log.match(/ORCA_STABLE_THREAD_IDENTITY_OK_\d+_\d+/);
  if (!stableIdentityTokenMatch) {
    throw new Error(`missing unique stable identity token in log:\n${log}`);
  }
  const stableIdentitySentinel = stableIdentityTokenMatch[0];
  for (const expected of [
    "cargo build --bin orca",
    "cargo run -p orca-provider --example summary_render_realapi",
    "cargo run -p orca-runtime --example goal_mode_realapi -- --max-budget 0.01",
    "orca exec --output-format jsonl --no-history --mode suggest --max-budget 0.01 Reply with exactly: ORCA_REAL_E2E_OK",
    "orca exec --output-format jsonl --mode full-auto --max-budget 0.01 --resume latest Do not call tools or retry prior work. Reply with exactly: ORCA_HISTORY_REPLAY_OK",
    "history-replay-fixture legacy-missing-tool-call missing-result",
    "server-stdin {\"id\":\"history-replay-items\",\"method\":\"thread/items/list\",\"params\":{\"threadId\":\"history-replay-e2e\",\"limit\":20}}",
    `orca exec --output-format jsonl --save-history --mode suggest --max-budget 0.01 Remember this exact token for the next process: ${stableIdentitySentinel}. Reply with exactly: ORCA_STABLE_THREAD_IDENTITY_READY`,
    "server-stdin {\"id\":\"stable-identity-turns\",\"method\":\"thread/turns/list\",\"params\":{\"threadId\":\"run-stable-thread-test\",\"limit\":100}}",
    "server-stdin {\"id\":\"stable-identity-items\",\"method\":\"thread/items/list\",\"params\":{\"threadId\":\"run-stable-thread-test\",\"limit\":100}}",
    "orca exec --output-format jsonl --save-history --mode suggest --max-budget 0.01 --resume run-stable-thread-test Reply with exactly the token I asked you to remember in the previous process.",
    "orca --mode server",
    "server-stdin {\"id\":101,\"op\":\"submit\",\"prompt\":\"Reply with exactly: ORCA_SERVER_REAL_OK\"}",
    "server-stdin {\"id\":\"server-thread\",\"method\":\"thread/start\",\"params\":{}}",
    `server-stdin {"id":"server-thread-turn-1","method":"turn/start","params":{"threadId":"thread-test","input":[{"type":"text","text":"Remember this exact token for the next turn: ${serverThreadSentinel}. Reply with exactly: READY."}]}}`,
    "server-stdin {\"id\":\"server-thread-turn-2\",\"method\":\"turn/start\",\"params\":{\"threadId\":\"thread-test\",\"input\":[{\"type\":\"text\",\"text\":\"Reply with exactly the token I asked you to remember.\"}]}}",
    "server-stdin {\"id\":\"server-resume-thread\",\"method\":\"thread/start\",\"params\":{}}",
    `server-stdin {"id":"server-resume-turn","method":"turn/start","params":{"threadId":"thread-test-resume","approvalPolicy":"never","input":[{"type":"text","text":"Do not call tools or inspect files. This is a text-only streaming test. Write 80 short numbered lines containing STREAM. The final line must be exactly: ${serverResumeSentinel}"}]}}`,
    "server-stdin {\"id\":\"server-resume-interrupt\",\"method\":\"turn/interrupt\",\"params\":{\"threadId\":\"thread-test-resume\",\"turnId\":\"turn_logical_resume_test\"}}",
    "server-stdin {\"id\":\"server-resume-resume\",\"method\":\"turn/resume\",\"params\":{\"threadId\":\"thread-test-resume\",\"turnId\":\"turn_logical_resume_test\"}}",
    "server-stdin {\"id\":\"server-turn-interrupt\",\"method\":\"turn/interrupt\",\"params\":{\"turnId\":\"turn-idle-real-api\"}}",
    "server-stdin {\"id\":\"server-turn-resume\",\"method\":\"turn/resume\",\"params\":{\"turnId\":\"turn-idle-real-api\"}}",
    "server-stdin {\"id\":\"server-turn-steer\",\"method\":\"turn/steer\",\"params\":{\"turnId\":\"turn-idle-real-api\",\"input\":[{\"type\":\"text\",\"text\":\"steer this idle turn\"}]}}",
    `server-stdin {"id":"server-thread-metadata","method":"thread/metadata/update","params":{"threadId":"thread-test","title":"ORCA server thread metadata e2e ${serverThreadSentinel}"}}`,
    "server-stdin {\"id\":\"server-thread-extra\",\"method\":\"thread/start\",\"params\":{}}",
    `server-stdin {"id":"server-thread-extra-turn","method":"turn/start","params":{"threadId":"thread-test-extra","input":[{"type":"text","text":"Reply with exactly this token for list pagination coverage: ${serverThreadSentinel}."}]}}`,
    `server-stdin {"id":"server-thread-extra-metadata","method":"thread/metadata/update","params":{"threadId":"thread-test-extra","title":"ORCA server extra thread ${serverThreadSentinel}"}}`,
    `server-stdin {"id":"server-thread-list","method":"thread/list","params":{"searchTerm":"${serverThreadSentinel}","sortKey":"updatedAt","limit":1}}`,
    `server-stdin {"id":"server-thread-list-page-2","method":"thread/list","params":{"cursor":"1","searchTerm":"${serverThreadSentinel}","sortKey":"updatedAt","limit":10}}`,
    `server-stdin {"id":"server-thread-list-metadata-filter","method":"thread/list","params":{"searchTerm":"${serverThreadSentinel}","cwd":"/tmp/fake","modelProviders":["deepseek"],"model":"deepseek-v4-flash","sortKey":"updatedAt","limit":10}}`,
    `server-stdin {"id":"server-thread-list-metadata-filter-miss","method":"thread/list","params":{"searchTerm":"${serverThreadSentinel}","cwd":"/tmp/fake/missing","modelProviders":["deepseek"],"model":"deepseek-v4-flash","sortKey":"updatedAt","limit":10}}`,
    `server-stdin {"id":"server-thread-search","method":"thread/search","params":{"searchTerm":"${serverThreadSentinel}","sortKey":"updatedAt","limit":1}}`,
    `server-stdin {"id":"server-thread-search-page-2","method":"thread/search","params":{"searchTerm":"${serverThreadSentinel}","cursor":"1","sortKey":"updatedAt","limit":10}}`,
    "server-stdin {\"id\":\"server-thread-turns-list\",\"method\":\"thread/turns/list\",\"params\":{\"threadId\":\"thread-test\",\"limit\":1}}",
    "server-stdin {\"id\":\"server-thread-turns-list-page-2\",\"method\":\"thread/turns/list\",\"params\":{\"threadId\":\"thread-test\",\"cursor\":\"1\",\"limit\":10}}",
    "server-stdin {\"id\":\"server-thread-turns-list-desc\",\"method\":\"thread/turns/list\",\"params\":{\"threadId\":\"thread-test\",\"limit\":1,\"sortDirection\":\"desc\"}}",
    "server-stdin {\"id\":\"server-thread-turns-list-not-loaded\",\"method\":\"thread/turns/list\",\"params\":{\"threadId\":\"thread-test\",\"limit\":1,\"itemsView\":\"notLoaded\"}}",
    "server-stdin {\"id\":\"server-thread-items-list\",\"method\":\"thread/items/list\",\"params\":{\"threadId\":\"thread-test\",\"limit\":2}}",
    "server-stdin {\"id\":\"server-thread-items-list-page-2\",\"method\":\"thread/items/list\",\"params\":{\"threadId\":\"thread-test\",\"cursor\":\"2\",\"limit\":10}}",
    "server-stdin {\"id\":\"server-thread-items-list-desc\",\"method\":\"thread/items/list\",\"params\":{\"threadId\":\"thread-test\",\"limit\":1,\"sortDirection\":\"desc\"}}",
    "server-stdin {\"id\":\"server-thread-read\",\"method\":\"thread/read\",\"params\":{\"threadId\":\"thread-test\",\"includeMessages\":true,\"includeTurns\":true}}",
  ]) {
    if (!log.includes(expected)) {
      throw new Error(`missing command ${expected} in log:\n${log}`);
    }
  }

  try {
    execFileSync(
      "node",
      [script, "--orca-bin", orcaBin],
      {
        cwd: repoRoot,
        env: {
          ...process.env,
          PATH: `${binDir}${path.delimiter}${process.env.PATH}`,
          ORCA_FAKE_BAD_CLI: "1",
        },
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    throw new Error("real-api-e2e should fail when the CLI sentinel is missing");
  } catch (error) {
    if (error.message.includes("real-api-e2e should fail")) {
      throw error;
    }
  }

  console.log("real-api-e2e release checks ok");
} finally {
  rmSync(tempDir, { recursive: true, force: true });
}
