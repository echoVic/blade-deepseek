#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { chmodSync, mkdtempSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..", "..");
const harness = path.join(repoRoot, "scripts", "release", "real-api-acp-surface.mjs");
const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-real-acp-harness-test-"));
const fakeBin = path.join(tempDir, "fake-orca-acp.mjs");

writeFileSync(
  fakeBin,
  `#!/usr/bin/env node
const scenario = process.env.ORCA_RELEASE_FAKE_SCENARIO ?? "ok";
if (scenario === "timeout") setInterval(() => {}, 1000);
else {
  const ok = ["initialized connection-1", "new session-1", "prompt_update prompt-1", "prompt_response prompt-1", "load_update session-1", "load_response session-1", "cancel_sent prompt-2", "cancel_response prompt-2", "transport_closed connection-1"];
  let lines = ok;
  if (scenario === "missing") lines = ok.filter((line) => !line.startsWith("load_update"));
  if (scenario === "wrong-order") lines = [ok[0], ok[1], ok[3], ok[2], ...ok.slice(4)];
  if (scenario === "stale") lines = ok.map((line) => line === "cancel_response prompt-2" ? "cancel_response stale-prompt" : line);
  if (scenario === "terminal-before-flush") lines = [ok[0], ok[1], ok[3], ok[2], ...ok.slice(4)];
  for (const line of lines) process.stdout.write("ACP_HARNESS " + line + "\\n");
  if (scenario === "unreaped") setInterval(() => {}, 1000);
}
`,
);
chmodSync(fakeBin, 0o755);

function invoke(scenario, timeoutMs = 10000) {
  try {
    const output = execFileSync(
      process.execPath,
      [harness, "--bin", fakeBin, "--timeout-ms", String(timeoutMs)],
      {
        cwd: repoRoot,
        encoding: "utf8",
        env: { ...process.env, ORCA_RELEASE_FAKE_SCENARIO: scenario },
        timeout: timeoutMs + 1500,
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    return { ok: true, output };
  } catch (error) {
    return { ok: false, output: `${error.stdout ?? ""}${error.stderr ?? ""}` };
  }
}

const success = invoke("ok");
if (!success.ok || !success.output.includes("ORCA_ACP_SURFACE_FAKE_OK")) {
  throw new Error(`ACP harness fake success failed:\n${success.output}`);
}
for (const scenario of ["missing", "wrong-order", "stale", "terminal-before-flush", "timeout", "unreaped"]) {
  const result = invoke(scenario, scenario === "timeout" || scenario === "unreaped" ? 300 : 10000);
  if (result.ok) throw new Error(`ACP harness unexpectedly accepted ${scenario}:\n${result.output}`);
}

try {
  execFileSync(process.execPath, [harness, "--bin", "relative/orca"], { cwd: repoRoot, stdio: "pipe" });
  throw new Error("ACP harness accepted a relative --bin");
} catch (error) {
  if (error.message === "ACP harness accepted a relative --bin") throw error;
}

console.log("real-api ACP surface harness self-test passed");
