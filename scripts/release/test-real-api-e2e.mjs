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
  *) exit 42 ;;
esac
`,
  );

  const orcaBin = path.join(binDir, "orca");
  writeExecutable(
    orcaBin,
    `#!/bin/sh
printf 'orca %s\\n' "$*" >> "${logPath}"
if [ "$ORCA_FAKE_BAD_CLI" = "1" ] && [ "$1" = "exec" ]; then
  printf '{"type":"assistant.message.delta","payload":{"text":"WRONG"}}\\n'
  printf '{"type":"session.completed","payload":{"status":"success"}}\\n'
  exit 0
fi
if [ "$1" = "exec" ]; then
  printf '{"type":"assistant.message.delta","payload":{"text":"ORCA_"}}\\n'
  printf '{"type":"assistant.message.delta","payload":{"text":"REAL_E2E_OK"}}\\n'
  printf '{"type":"session.completed","payload":{"status":"success"}}\\n'
  exit 0
fi
if [ "$1" = "--mode" ] && [ "$2" = "server" ]; then
  server_input="$(cat)"
  printf 'server-stdin %s\\n' "$server_input" >> "${logPath}"
  printf '{"id":101,"event":"message_delta","text":"ORCA_"}\\n'
  printf '{"id":101,"event":"message_delta","text":"SERVER_REAL_OK"}\\n'
  printf '{"id":101,"event":"turn_completed","status":"success"}\\n'
  exit 0
fi
exit 43
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
    "CLI real API e2e verified: ORCA_REAL_E2E_OK",
    "Server real API e2e verified: ORCA_SERVER_REAL_OK",
  ]) {
    if (!output.includes(expected)) {
      throw new Error(`missing output ${expected}:\n${output}`);
    }
  }

  const log = readFileSync(logPath, "utf8");
  for (const expected of [
    "cargo build --bin orca",
    "cargo run -p orca-provider --example summary_render_realapi",
    "orca exec --output-format jsonl --no-history --mode suggest --max-budget 0.01 Reply with exactly: ORCA_REAL_E2E_OK",
    "orca --mode server",
    "server-stdin {\"id\":101,\"op\":\"submit\",\"prompt\":\"Reply with exactly: ORCA_SERVER_REAL_OK\"}",
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
