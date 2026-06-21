#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..", "..");
const cargoToml = readFileSync(path.join(repoRoot, "Cargo.toml"), "utf8");
const VERSION = cargoToml.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
if (!VERSION) {
  throw new Error("Unable to read root package version from Cargo.toml");
}

const TARGETS = [
  ["orca-darwin-arm64", "aarch64-apple-darwin", "darwin-arm64"],
  ["orca-darwin-x64", "x86_64-apple-darwin", "darwin-x64"],
  ["orca-linux-arm64", "aarch64-unknown-linux-gnu", "linux-arm64"],
  ["orca-linux-x64", "x86_64-unknown-linux-gnu", "linux-x64"]
];

function readJson(filePath) {
  return JSON.parse(readFileSync(filePath, "utf8"));
}

function assertEqual(actual, expected, message) {
  if (actual !== expected) {
    throw new Error(`${message}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

function assertExists(filePath) {
  if (!existsSync(filePath)) {
    throw new Error(`Expected file to exist: ${filePath}`);
  }
}

function writeFakeBinary(filePath) {
  mkdirSync(path.dirname(filePath), { recursive: true });
  writeFileSync(filePath, `#!/bin/sh\necho orca ${VERSION}\n`);
}

const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-stage-test-"));

try {
  const artifactsDir = path.join(tempDir, "artifacts");
  const outDir = path.join(tempDir, "npm");
  for (const [, triple] of TARGETS) {
    writeFakeBinary(path.join(artifactsDir, `orca-${triple}`, "orca"));
  }

  execFileSync(
    "node",
    [
      path.join(repoRoot, "scripts", "release", "stage-npm.mjs"),
      "--version",
      VERSION,
      "--artifacts-dir",
      artifactsDir,
      "--out-dir",
      outDir,
      "--pack"
    ],
    { cwd: repoRoot, stdio: "pipe" }
  );

  for (const [aliasDir, , suffix] of TARGETS) {
    const packageJson = readJson(path.join(outDir, "stage", aliasDir, "package.json"));
    assertEqual(packageJson.name, "@blade-ai/orca", `${aliasDir} package name`);
    assertEqual(packageJson.version, `${VERSION}-${suffix}`, `${aliasDir} package version`);
    assertExists(path.join(outDir, "tarballs", `blade-ai-orca-${VERSION}-${suffix}.tgz`));
  }

  const mainPackage = readJson(path.join(outDir, "stage", "orca", "package.json"));
  assertEqual(mainPackage.name, "@blade-ai/orca", "main package name");
  assertEqual(mainPackage.version, VERSION, "main package version");

  const expectedOptionalDependencies = Object.fromEntries(
    TARGETS.map(([aliasDir, , suffix]) => [
      `@blade-ai/${aliasDir}`,
      `npm:@blade-ai/orca@${VERSION}-${suffix}`
    ])
  );
  assertEqual(
    JSON.stringify(mainPackage.optionalDependencies),
    JSON.stringify(expectedOptionalDependencies),
    "main package optionalDependencies"
  );

  assertExists(path.join(outDir, "tarballs", `blade-ai-orca-${VERSION}.tgz`));
  console.log("stage-npm alias distribution ok");
} finally {
  rmSync(tempDir, { recursive: true, force: true });
}
