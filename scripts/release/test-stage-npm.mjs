#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
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
  ["orca-linux-x64", "x86_64-unknown-linux-gnu", "linux-x64"],
  ["orca-win32-arm64", "aarch64-pc-windows-msvc", "win32-arm64", "orca.exe", "zip", ["orca-windows-runner.exe", "orca-windows-sandbox-setup.exe"]],
  ["orca-win32-x64", "x86_64-pc-windows-msvc", "win32-x64", "orca.exe", "zip", ["orca-windows-runner.exe", "orca-windows-sandbox-setup.exe"]]
];
const HOMEPAGE = "https://orcaagent.dev/";

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
  chmodSync(filePath, 0o755);
}

function writeArtifactSet(artifactsDir, triple, binaryName = "orca", archiveExtension = "tar.gz", helpers = []) {
  const packageDir = path.join(artifactsDir, `orca-${triple}`);
  const binary = path.join(packageDir, binaryName);
  writeFakeBinary(binary);
  for (const helper of helpers) writeFakeBinary(path.join(packageDir, helper));
  const archive = path.join(artifactsDir, `orca-${triple}.${archiveExtension}`);
  if (archiveExtension === "zip") {
    execFileSync("zip", ["-j", "-q", archive, binary, ...helpers.map((helper) => path.join(packageDir, helper))]);
  } else {
    execFileSync("tar", ["-C", packageDir, "-czf", archive, binaryName]);
  }
  const digest = createHash("sha256").update(readFileSync(archive)).digest("hex");
  writeFileSync(`${archive}.sha256`, `${digest}  ${path.basename(archive)}\n`);
}

const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-stage-test-"));

try {
  const artifactsDir = path.join(tempDir, "artifacts");
  const outDir = path.join(tempDir, "npm");
  for (const [, triple, , binaryName, archiveExtension, helpers] of TARGETS) {
    writeArtifactSet(artifactsDir, triple, binaryName, archiveExtension, helpers);
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

  for (const [aliasDir, triple, suffix, binaryName = "orca", , helpers = []] of TARGETS) {
    const packageJson = readJson(path.join(outDir, "stage", aliasDir, "package.json"));
    assertEqual(packageJson.name, "@blade-ai/orca", `${aliasDir} package name`);
    assertEqual(packageJson.version, `${VERSION}-${suffix}`, `${aliasDir} package version`);
    assertEqual(packageJson.homepage, HOMEPAGE, `${aliasDir} homepage`);
    assertExists(path.join(outDir, "stage", aliasDir, "vendor", triple, "bin", binaryName));
    for (const helper of helpers) assertExists(path.join(outDir, "stage", aliasDir, "vendor", triple, "bin", helper));
    assertExists(path.join(outDir, "tarballs", `blade-ai-orca-${VERSION}-${suffix}.tgz`));
  }

  const mainPackage = readJson(path.join(outDir, "stage", "orca", "package.json"));
  assertEqual(mainPackage.name, "@blade-ai/orca", "main package name");
  assertEqual(mainPackage.version, VERSION, "main package version");
  assertEqual(mainPackage.homepage, HOMEPAGE, "main package homepage");

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

  const stageScript = path.join(repoRoot, "scripts", "release", "stage-npm.mjs");
  const expectFailure = (label, mutate, expected) => {
    const [, triple, , binaryName = "orca", archiveExtension = "tar.gz"] = TARGETS[0];
    writeArtifactSet(artifactsDir, triple, binaryName, archiveExtension, TARGETS[0][5]);
    mutate(triple, binaryName, archiveExtension);
    try {
      execFileSync("node", [stageScript, "--version", VERSION, "--artifacts-dir", artifactsDir, "--out-dir", path.join(tempDir, label)], {
        cwd: repoRoot,
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      });
      throw new Error(`stage-npm accepted ${label}`);
    } catch (error) {
      if (error.message === `stage-npm accepted ${label}`) throw error;
      const output = `${error.stdout ?? ""}${error.stderr ?? ""}`;
      if (!output.includes(expected)) throw new Error(`${label} missing ${expected}: ${output}`);
    }
  };
  expectFailure("missing-checksum", (triple, _binary, extension) => rmSync(path.join(artifactsDir, `orca-${triple}.${extension}.sha256`)), "Missing checksum artifact");
  expectFailure("bad-checksum", (triple, _binary, extension) => writeFileSync(path.join(artifactsDir, `orca-${triple}.${extension}.sha256`), `${"0".repeat(64)}  bad\n`), "Checksum mismatch");
  expectFailure("binary-mismatch", (triple, binary) => writeFileSync(path.join(artifactsDir, `orca-${triple}`, binary), "different"), "Package/archive binary mismatch");
  console.log("stage-npm alias distribution ok");
} finally {
  rmSync(tempDir, { recursive: true, force: true });
}
