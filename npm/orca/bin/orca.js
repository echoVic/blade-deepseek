#!/usr/bin/env node

import { spawn } from "node:child_process";
import { existsSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const require = createRequire(import.meta.url);

const TARGETS = {
  "darwin:arm64": {
    packageName: "@blade-ai/orca-darwin-arm64",
    targetTriple: "aarch64-apple-darwin"
  },
  "darwin:x64": {
    packageName: "@blade-ai/orca-darwin-x64",
    targetTriple: "x86_64-apple-darwin"
  },
  "linux:arm64": {
    packageName: "@blade-ai/orca-linux-arm64",
    targetTriple: "aarch64-unknown-linux-gnu"
  },
  "linux:x64": {
    packageName: "@blade-ai/orca-linux-x64",
    targetTriple: "x86_64-unknown-linux-gnu"
  }
};

const target = TARGETS[`${process.platform}:${process.arch}`];
if (!target) {
  throw new Error(`Unsupported platform: ${process.platform} (${process.arch})`);
}

function findExecutable() {
  let vendorRoot;
  try {
    const packageJsonPath = require.resolve(`${target.packageName}/package.json`);
    vendorRoot = path.join(path.dirname(packageJsonPath), "vendor");
  } catch {
    vendorRoot = path.join(__dirname, "..", "vendor");
  }

  const executable = path.join(vendorRoot, target.targetTriple, "bin", "orca");
  if (existsSync(executable)) {
    return executable;
  }

  throw new Error(
    `Missing optional dependency ${target.packageName}. Reinstall with: npm install -g @blade-ai/orca`
  );
}

const binaryPath = findExecutable();
const env = {
  ...process.env,
  ORCA_MANAGED_BY_NPM: "1",
  ORCA_MANAGED_PACKAGE_ROOT: realpathSync(path.join(__dirname, ".."))
};

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
  env
});

child.on("error", (error) => {
  console.error(error);
  process.exit(1);
});

const handledSignals = ["SIGINT", "SIGTERM", "SIGHUP"];
const forwardSignal = (signal) => {
  if (!child.killed) {
    child.kill(signal);
  }
};

for (const signal of handledSignals) {
  process.on(signal, () => forwardSignal(signal));
}

const result = await new Promise((resolve) => {
  child.on("exit", (code, signal) => {
    if (signal) {
      resolve({ type: "signal", signal });
    } else {
      resolve({ type: "code", exitCode: code ?? 1 });
    }
  });
});

if (result.type === "signal") {
  for (const signal of handledSignals) {
    process.removeAllListeners(signal);
  }
  process.kill(process.pid, result.signal);
} else {
  process.exit(result.exitCode);
}
