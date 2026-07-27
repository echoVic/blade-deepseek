#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..", "..");
const args = { version: null, timeoutMs: 300000, selfTestFakeHarness: false };
for (let index = 2; index < process.argv.length; index += 1) {
  if (process.argv[index] === "--version") args.version = process.argv[++index];
  else if (process.argv[index] === "--timeout-ms") args.timeoutMs = Number.parseInt(process.argv[++index], 10);
  else if (process.argv[index] === "--self-test-fake-harness") args.selfTestFakeHarness = true;
  else throw new Error(`Unknown argument: ${process.argv[index]}`);
}
if (!args.version || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(args.version)) throw new Error("Missing or invalid --version");
if (!process.env.ORCA_API_KEY) throw new Error("ORCA_API_KEY is required for public real-API verification");
if (process.env.ORCA_RELEASE_FAKE_SCENARIO && !args.selfTestFakeHarness) {
  throw new Error("ORCA_RELEASE_FAKE_SCENARIO is forbidden for public real-API verification");
}
const apiKey = process.env.ORCA_API_KEY;
const publicEnv = { ...process.env };
delete publicEnv.ORCA_API_KEY;
delete publicEnv.ORCA_RELEASE_FAKE_SCENARIO;

const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-public-real-api-"));
try {
  writeFileSync(path.join(tempDir, "package.json"), `${JSON.stringify({ private: true }, null, 2)}\n`);
  const spec = `@blade-ai/orca@${args.version}`;
  execFileSync("npm", ["install", "--save-exact", spec], { cwd: tempDir, env: publicEnv, stdio: "inherit" });
  const installed = JSON.parse(readFileSync(path.join(tempDir, "node_modules", "@blade-ai", "orca", "package.json"), "utf8"));
  if (installed.version !== args.version) throw new Error(`Public install resolved ${installed.version}, expected ${args.version}`);
  const bin = path.resolve(tempDir, "node_modules", ".bin", "orca");
  const versionOutput = execFileSync(bin, ["--version"], { cwd: tempDir, env: publicEnv, encoding: "utf8" }).trim();
  if (!versionOutput.includes(`orca ${args.version}`)) throw new Error(`Public binary version mismatch: ${versionOutput}`);
  for (const script of [
    "real-api-tui-approval-recovery.mjs",
    "real-api-acp-surface.mjs",
    "real-api-server-approval-recovery.mjs",
  ]) {
    execFileSync(process.execPath, [path.join(repoRoot, "scripts", "release", script), "--bin", bin, "--timeout-ms", String(args.timeoutMs)], {
      cwd: tempDir,
      env: {
        ...publicEnv,
        ORCA_API_KEY: apiKey,
        ...(args.selfTestFakeHarness ? { ORCA_RELEASE_FAKE_SCENARIO: "ok" } : {}),
      },
      stdio: "inherit",
    });
  }
  console.log(args.selfTestFakeHarness ? `Public real API self-test verified: ${spec}` : `Public real API verified: ${spec}`);
} finally {
  rmSync(tempDir, { recursive: true, force: true });
}
