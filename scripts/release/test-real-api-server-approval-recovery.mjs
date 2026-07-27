#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { chmodSync, mkdtempSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..", "..");
const harness = path.join(repoRoot, "scripts", "release", "real-api-server-approval-recovery.mjs");
const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-real-server-harness-test-"));
const fakeBin = path.join(tempDir, "fake-orca-server.mjs");

writeFileSync(fakeBin, `#!/usr/bin/env node
const scenario = process.env.ORCA_RELEASE_FAKE_SCENARIO ?? "ok";
if (scenario === "timeout") setInterval(() => {}, 1000);
else {
  const ok = ["thread_started thread-1", "permission_requested request-1", "permission_resolved request-1", "output_flushed turn-1", "turn_terminal turn-1", "eof_settled request-2", "restart_resumed thread-1", "replay_visible turn-1", "eof_restart_recovered thread-2", "shutdown_complete connection-2"];
  let lines = ok;
  if (scenario === "missing") lines = ok.filter((line) => !line.startsWith("eof_settled"));
  if (scenario === "wrong-order") lines = [ok[0], ok[2], ok[1], ...ok.slice(3)];
  if (scenario === "stale") lines = ok.map((line) => line === "permission_resolved request-1" ? "permission_resolved stale-request" : line);
  if (scenario === "terminal-before-flush") lines = [ok[0], ok[1], ok[2], ok[4], ok[3], ...ok.slice(5)];
  for (const line of lines) process.stdout.write("SERVER_HARNESS " + line + "\\n");
  if (scenario === "unreaped") setInterval(() => {}, 1000);
}
`);
chmodSync(fakeBin, 0o755);

function invoke(scenario, timeoutMs = 3000) {
  try {
    const output = execFileSync(process.execPath, [harness, "--bin", fakeBin, "--timeout-ms", String(timeoutMs)], {
      cwd: repoRoot,
      encoding: "utf8",
      env: { ...process.env, ORCA_RELEASE_FAKE_SCENARIO: scenario },
      timeout: timeoutMs + 1500,
      stdio: ["ignore", "pipe", "pipe"],
    });
    return { ok: true, output };
  } catch (error) {
    return { ok: false, output: `${error.stdout ?? ""}${error.stderr ?? ""}` };
  }
}

const success = invoke("ok");
if (!success.ok || !success.output.includes("ORCA_SERVER_APPROVAL_RECOVERY_FAKE_OK")) {
  throw new Error(`server harness fake success failed:\n${success.output}`);
}
for (const scenario of ["missing", "wrong-order", "stale", "terminal-before-flush", "timeout", "unreaped"]) {
  const result = invoke(scenario, scenario === "timeout" || scenario === "unreaped" ? 300 : 3000);
  if (result.ok) throw new Error(`server harness unexpectedly accepted ${scenario}:\n${result.output}`);
}
try {
  execFileSync(process.execPath, [harness, "--bin", "relative/orca"], { cwd: repoRoot, stdio: "pipe" });
  throw new Error("server harness accepted a relative --bin");
} catch (error) {
  if (error.message === "server harness accepted a relative --bin") throw error;
}
console.log("real-api server approval recovery harness self-test passed");
