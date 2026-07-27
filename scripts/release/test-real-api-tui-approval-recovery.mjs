#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { chmodSync, mkdtempSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..", "..");
const harness = path.join(repoRoot, "scripts", "release", "real-api-tui-approval-recovery.mjs");
const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-real-tui-harness-test-"));
const fakeBin = path.join(tempDir, "fake-orca.mjs");

writeFileSync(
  fakeBin,
  `#!/usr/bin/env node
import { appendFileSync } from "node:fs";

const scenario = process.env.ORCA_RELEASE_FAKE_SCENARIO ?? "ok";
const log = process.env.ORCA_RELEASE_FAKE_LOG;
appendFileSync(log, JSON.stringify({ pid: process.pid, args: process.argv.slice(2) }) + "\\n");

if (scenario === "timeout") {
  setInterval(() => {}, 1000);
} else if (scenario === "unreaped") {
  process.stdout.write("TUI_HARNESS approval_requested request-1\\n");
  process.stdout.write("TUI_HARNESS approval_resolved request-1\\n");
  process.stdout.write("TUI_HARNESS cancel_committed turn-1\\n");
  process.stdout.write("TUI_HARNESS terminal_committed turn-1\\n");
  process.stdout.write("TUI_HARNESS terminal_flushed turn-1\\n");
  process.stdout.write("TUI_HARNESS restart_recovered turn-1\\n");
  setInterval(() => {}, 1000);
} else {
  const lines = scenario === "missing"
    ? ["approval_requested request-1", "approval_resolved request-1", "terminal_committed turn-1", "terminal_flushed turn-1"]
    : scenario === "wrong-order"
      ? ["approval_resolved request-1", "approval_requested request-1", "cancel_committed turn-1", "terminal_committed turn-1", "terminal_flushed turn-1", "restart_recovered turn-1"]
      : scenario === "stale"
        ? ["approval_requested request-1", "approval_resolved stale-request", "cancel_committed turn-1", "terminal_committed turn-1", "terminal_flushed turn-1", "restart_recovered turn-1"]
        : scenario === "terminal-before-flush"
          ? ["approval_requested request-1", "approval_resolved request-1", "cancel_committed turn-1", "terminal_flushed turn-1", "terminal_committed turn-1", "restart_recovered turn-1"]
          : ["approval_requested request-1", "approval_resolved request-1", "cancel_committed turn-1", "terminal_committed turn-1", "terminal_flushed turn-1", "restart_recovered turn-1"];
  for (const line of lines) {
    const rendered = scenario === "ansi"
      ? line.split("").join("\\u001b[;m")
      : line;
    process.stdout.write("TUI_HARNESS " + rendered + "\\n");
  }
}
`,
);
chmodSync(fakeBin, 0o755);

function invoke(scenario, timeoutMs = 10000) {
  try {
    return {
      ok: true,
      output: execFileSync(
        process.execPath,
        [harness, "--bin", fakeBin, "--timeout-ms", String(timeoutMs)],
        {
          cwd: repoRoot,
          encoding: "utf8",
          env: {
            ...process.env,
            ORCA_RELEASE_FAKE_SCENARIO: scenario,
            ORCA_RELEASE_FAKE_LOG: path.join(tempDir, `${scenario}.log`),
          },
          timeout: timeoutMs + 1500,
          stdio: ["ignore", "pipe", "pipe"],
        },
      ),
    };
  } catch (error) {
    return {
      ok: false,
      output: `${error.stdout ?? ""}${error.stderr ?? ""}`,
    };
  }
}

const success = invoke("ok");
if (!success.ok || !success.output.includes("ORCA_TUI_APPROVAL_RECOVERY_FAKE_OK")) {
  throw new Error(`TUI harness fake success failed:\n${success.output}`);
}

const ansi = invoke("ansi");
if (!ansi.ok || !ansi.output.includes("ORCA_TUI_APPROVAL_RECOVERY_FAKE_OK")) {
  throw new Error(`TUI harness ANSI normalization failed:\n${ansi.output}`);
}

for (const scenario of ["missing", "wrong-order", "stale", "terminal-before-flush", "timeout", "unreaped"]) {
  const result = invoke(scenario, scenario === "timeout" || scenario === "unreaped" ? 300 : 10000);
  if (result.ok) {
    throw new Error(`TUI harness unexpectedly accepted ${scenario}:\n${result.output}`);
  }
}

const relative = (() => {
  try {
    execFileSync(process.execPath, [harness, "--bin", "relative/orca"], {
      cwd: repoRoot,
      stdio: "pipe",
    });
    return true;
  } catch {
    return false;
  }
})();
if (relative) throw new Error("TUI harness accepted a relative --bin");

console.log("real-api TUI approval recovery harness self-test passed");
