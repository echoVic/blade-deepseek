#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { cp, mkdir, symlink } from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const TARGETS = {
  "darwin:arm64": "orca-darwin-arm64",
  "darwin:x64": "orca-darwin-x64",
  "linux:arm64": "orca-linux-arm64",
  "linux:x64": "orca-linux-x64"
};

function parseArgs(argv) {
  const args = {
    version: null,
    stageDir: null
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--version") {
      args.version = argv[++index];
    } else if (arg === "--stage-dir") {
      args.stageDir = path.resolve(argv[++index]);
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }

  if (!args.version) {
    throw new Error("Missing --version");
  }
  if (!args.stageDir) {
    throw new Error("Missing --stage-dir");
  }
  return args;
}

function readJson(filePath) {
  return JSON.parse(readFileSync(filePath, "utf8"));
}

function ensurePackageExists(dir, label) {
  if (!existsSync(dir)) {
    throw new Error(`Missing staged ${label} package at ${dir}`);
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const targetKey = `${process.platform}:${process.arch}`;
  const targetPackage = TARGETS[targetKey];

  if (!targetPackage) {
    throw new Error(`Unsupported platform: ${targetKey}`);
  }

  const mainPackageDir = path.join(args.stageDir, "orca");
  const platformPackageDir = path.join(args.stageDir, targetPackage);
  ensurePackageExists(mainPackageDir, "main");
  ensurePackageExists(platformPackageDir, "platform");

  const platformPackageName = readJson(path.join(platformPackageDir, "package.json")).name;
  const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-npm-smoke-"));
  const nodeModulesDir = path.join(tempDir, "node_modules", "@blade-ai");
  const binDir = path.join(tempDir, "node_modules", ".bin");
  await mkdir(nodeModulesDir, { recursive: true });
  await mkdir(binDir, { recursive: true });
  await cp(mainPackageDir, path.join(nodeModulesDir, "orca"), { recursive: true });
  await cp(platformPackageDir, path.join(nodeModulesDir, platformPackageName.slice("@blade-ai/".length)), {
    recursive: true
  });
  await symlink(
    path.join("..", "@blade-ai", "orca", "bin", "orca.js"),
    path.join(binDir, "orca")
  );

  const output = execFileSync(path.join("node_modules", ".bin", "orca"), ["--version"], {
    cwd: tempDir,
    encoding: "utf8"
  }).trim();

  if (!output.includes(`orca ${args.version}`)) {
    throw new Error(`Unexpected orca version output: ${output}`);
  }

  console.log(output);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
