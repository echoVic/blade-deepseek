#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..", "..");
const script = path.join(repoRoot, "scripts", "release", "verify-published.mjs");
const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-verify-published-test-"));

function writeExecutable(filePath, contents) {
  writeFileSync(filePath, contents);
  chmodSync(filePath, 0o755);
}

try {
  const binDir = path.join(tempDir, "bin");
  mkdirSync(binDir, { recursive: true });
  const logPath = path.join(tempDir, "calls.log");

  writeExecutable(
    path.join(binDir, "gh"),
    `#!/bin/sh
printf 'gh %s\\n' "$*" >> "${logPath}"
case "$*" in
  *"release view v9.8.7"*) printf '{"tagName":"v9.8.7","url":"https://example.test/releases/v9.8.7","isDraft":false}\\n' ;;
  *) exit 42 ;;
esac
`,
  );

  writeExecutable(
    path.join(binDir, "npm"),
    `#!/bin/sh
printf 'npm %s\\n' "$*" >> "${logPath}"
case "$1 $2" in
  "view @blade-ai/orca@9.8.7") printf '"9.8.7"\\n' ;;
  "exec --yes") printf 'orca 9.8.7\\n' ;;
  *) exit 43 ;;
esac
`,
  );

  const output = execFileSync(
    "node",
    [
      script,
      "--version",
      "9.8.7",
      "--repo",
      "echoVic/blade-deepseek",
      "--package",
      "@blade-ai/orca",
      "--bin",
      "orca",
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

  if (!output.includes("GitHub Release verified")) {
    throw new Error(`missing GitHub verification output: ${output}`);
  }
  if (!output.includes("npm package verified")) {
    throw new Error(`missing npm verification output: ${output}`);
  }
  if (!output.includes("npm exec smoke verified")) {
    throw new Error(`missing npm smoke output: ${output}`);
  }

  const log = readFileSync(logPath, "utf8");
  for (const expected of [
    "gh release view v9.8.7 --repo echoVic/blade-deepseek",
    "npm view @blade-ai/orca@9.8.7 version --json",
    "npm exec --yes --package @blade-ai/orca@9.8.7 -- orca --version",
  ]) {
    if (!log.includes(expected)) {
      throw new Error(`missing command ${expected} in log:\n${log}`);
    }
  }

  console.log("verify-published release checks ok");

  try {
    execFileSync(
      "node",
      [
        script,
        "--version",
        "9.8.8",
        "--repo",
        "echoVic/blade-deepseek",
        "--package",
        "@blade-ai/orca",
        "--bin",
        "orca",
      ],
      {
        cwd: repoRoot,
        env: {
          ...process.env,
          PATH: `${binDir}${path.delimiter}${process.env.PATH}`,
        },
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    throw new Error("verify-published should fail when the GitHub Release is missing");
  } catch (error) {
    if (error.message.includes("verify-published should fail")) {
      throw error;
    }
  }

  console.log("verify-published failure checks ok");
} finally {
  rmSync(tempDir, { recursive: true, force: true });
}
