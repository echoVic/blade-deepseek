#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..", "..");
const script = path.join(repoRoot, "scripts", "release", "verify-public-real-api.mjs");
const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-public-real-api-test-"));
try {
  const binDir = path.join(tempDir, "bin");
  const log = path.join(tempDir, "npm.log");
  mkdirSync(binDir);
  const npm = path.join(binDir, "npm");
  writeFileSync(npm, `#!/usr/bin/env node
import { appendFileSync, chmodSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
appendFileSync(${JSON.stringify(log)}, process.argv.slice(2).join(" ") + "\\n");
if (process.env.ORCA_API_KEY) { console.error("npm received ORCA_API_KEY"); process.exit(91); }
const version = process.argv.at(-1).slice(process.argv.at(-1).lastIndexOf("@") + 1), cwd = process.cwd();
const packageDir = path.join(cwd, "node_modules", "@blade-ai", "orca"), binDir = path.join(cwd, "node_modules", ".bin");
mkdirSync(packageDir, { recursive: true }); mkdirSync(binDir, { recursive: true });
writeFileSync(path.join(packageDir, "package.json"), JSON.stringify({ name: "@blade-ai/orca", version }));
const bin = path.join(binDir, "orca");
  writeFileSync(bin, ${JSON.stringify(`#!/bin/sh
if [ -n "$ORCA_API_KEY" ] && [ -z "$ORCA_RELEASE_FAKE_SCENARIO" ]; then echo key leaked to version probe >&2; exit 92; fi
echo orca 9.8.7
echo TUI_HARNESS approval_requested request-1
echo TUI_HARNESS approval_resolved request-1
echo TUI_HARNESS cancel_committed turn-1
echo TUI_HARNESS terminal_committed turn-1
echo TUI_HARNESS terminal_flushed turn-1
echo TUI_HARNESS restart_recovered turn-1
echo ACP_HARNESS initialized connection-1
echo ACP_HARNESS new session-1
echo ACP_HARNESS prompt_update prompt-1
echo ACP_HARNESS prompt_response prompt-1
echo ACP_HARNESS load_update session-1
echo ACP_HARNESS load_response session-1
echo ACP_HARNESS cancel_sent prompt-2
echo ACP_HARNESS cancel_response prompt-2
echo ACP_HARNESS transport_closed connection-1
echo SERVER_HARNESS thread_started thread-1
echo SERVER_HARNESS permission_requested request-1
echo SERVER_HARNESS permission_resolved request-1
echo SERVER_HARNESS output_flushed turn-1
echo SERVER_HARNESS turn_terminal turn-1
echo SERVER_HARNESS eof_settled request-2
echo SERVER_HARNESS restart_resumed thread-1
echo SERVER_HARNESS replay_visible turn-1
echo SERVER_HARNESS eof_restart_recovered thread-2
echo SERVER_HARNESS shutdown_complete connection-2
`)}); chmodSync(bin, 0o755);
`);
  chmodSync(npm, 0o755);
  const baseEnv = { ...process.env, PATH: `${binDir}${path.delimiter}${process.env.PATH}`, ORCA_RELEASE_FAKE_SCENARIO: "ok" };
  delete baseEnv.ORCA_API_KEY;
  try {
    execFileSync(process.execPath, [script, "--version", "9.8.7"], { cwd: repoRoot, env: baseEnv, stdio: "pipe" });
    throw new Error("public real API verifier accepted a missing key");
  } catch (error) {
    if (error.message.includes("accepted a missing key")) throw error;
    if (!`${error.stdout ?? ""}${error.stderr ?? ""}`.includes("ORCA_API_KEY is required")) throw error;
  }
  try {
    execFileSync(process.execPath, [script, "--version", "9.8.7"], {
      cwd: repoRoot,
      env: { ...baseEnv, ORCA_API_KEY: "fixture-key" },
      stdio: "pipe",
    });
    throw new Error("public real API verifier accepted ambient fake mode");
  } catch (error) {
    if (error.message.includes("accepted ambient fake mode")) throw error;
    if (!`${error.stdout ?? ""}${error.stderr ?? ""}`.includes("ORCA_RELEASE_FAKE_SCENARIO is forbidden")) throw error;
  }
  const output = execFileSync(process.execPath, [script, "--version", "9.8.7", "--self-test-fake-harness"], {
    cwd: repoRoot,
    env: { ...baseEnv, ORCA_API_KEY: "fixture-key" },
    encoding: "utf8",
  });
  if (!output.includes("Public real API self-test verified") || output.includes("Public real API verified:")) throw new Error(`public verifier self-test failed: ${output}`);
  if (!readFileSync(log, "utf8").includes("install --save-exact @blade-ai/orca@9.8.7")) throw new Error("public verifier did not install the exact version");
  console.log("verify-public-real-api release checks ok");
} finally {
  rmSync(tempDir, { recursive: true, force: true });
}
