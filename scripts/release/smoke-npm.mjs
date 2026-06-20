#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
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
    stageDir: null,
    tarballsDir: null
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--version") {
      args.version = argv[++index];
    } else if (arg === "--stage-dir") {
      args.stageDir = path.resolve(argv[++index]);
    } else if (arg === "--tarballs-dir") {
      args.tarballsDir = path.resolve(argv[++index]);
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

function packageTarballPath(packageName, version, tarballsDir) {
  const fileName = `${packageName.replace(/^@/, "").replace("/", "-")}-${version}.tgz`;
  const tarball = path.join(tarballsDir, fileName);
  if (!existsSync(tarball)) {
    throw new Error(`Missing packed tarball for ${packageName} at ${tarball}`);
  }
  return tarball;
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

  const mainPackageName = readJson(path.join(mainPackageDir, "package.json")).name;
  const platformPackageName = readJson(path.join(platformPackageDir, "package.json")).name;
  const mainPackageSpec = args.tarballsDir
    ? `file:${packageTarballPath(mainPackageName, args.version, args.tarballsDir)}`
    : `file:${mainPackageDir}`;
  const platformPackageSpec = args.tarballsDir
    ? `file:${packageTarballPath(platformPackageName, args.version, args.tarballsDir)}`
    : `file:${platformPackageDir}`;
  const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-npm-smoke-"));
  writeFileSync(path.join(tempDir, "package.json"), JSON.stringify({
    private: true,
    dependencies: {
      [mainPackageName]: mainPackageSpec,
      [platformPackageName]: platformPackageSpec
    }
  }, null, 2));

  execFileSync("npm", ["install", "--ignore-scripts"], {
    cwd: tempDir,
    stdio: "inherit"
  });

  const output = execFileSync(
    "node",
    [
      "--preserve-symlinks-main",
      path.join(tempDir, "node_modules", ".bin", "orca"),
      "--version"
    ],
    {
      cwd: tempDir,
      encoding: "utf8"
    }
  ).trim();

  if (!output.includes(`orca ${args.version}`)) {
    throw new Error(`Unexpected orca version output: ${output}`);
  }

  console.log(output);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
