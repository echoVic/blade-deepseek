#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync, copyFileSync, chmodSync } from "node:fs";
import { cp, readdir } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const repoRoot = path.resolve(path.dirname(__filename), "..", "..");

const TARGETS = [
  {
    aliasName: "@blade-ai/orca-darwin-arm64",
    versionSuffix: "darwin-arm64",
    targetTriple: "aarch64-apple-darwin",
    os: "darwin",
    cpu: "arm64"
  },
  {
    aliasName: "@blade-ai/orca-darwin-x64",
    versionSuffix: "darwin-x64",
    targetTriple: "x86_64-apple-darwin",
    os: "darwin",
    cpu: "x64"
  },
  {
    aliasName: "@blade-ai/orca-linux-arm64",
    versionSuffix: "linux-arm64",
    targetTriple: "aarch64-unknown-linux-gnu",
    os: "linux",
    cpu: "arm64"
  },
  {
    aliasName: "@blade-ai/orca-linux-x64",
    versionSuffix: "linux-x64",
    targetTriple: "x86_64-unknown-linux-gnu",
    os: "linux",
    cpu: "x64"
  },
  {
    aliasName: "@blade-ai/orca-win32-arm64",
    versionSuffix: "win32-arm64",
    targetTriple: "aarch64-pc-windows-msvc",
    os: "win32",
    cpu: "arm64",
    binaryName: "orca.exe",
    archiveExtension: "zip",
    helperNames: ["orca-windows-runner.exe", "orca-windows-sandbox-setup.exe"]
  },
  {
    aliasName: "@blade-ai/orca-win32-x64",
    versionSuffix: "win32-x64",
    targetTriple: "x86_64-pc-windows-msvc",
    os: "win32",
    cpu: "x64",
    binaryName: "orca.exe",
    archiveExtension: "zip",
    helperNames: ["orca-windows-runner.exe", "orca-windows-sandbox-setup.exe"]
  }
];

function parseArgs(argv) {
  const args = {
    version: null,
    artifactsDir: null,
    outDir: path.join(repoRoot, "dist", "npm"),
    pack: false
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--version") {
      args.version = argv[++index];
    } else if (arg === "--artifacts-dir") {
      args.artifactsDir = path.resolve(argv[++index]);
    } else if (arg === "--out-dir") {
      args.outDir = path.resolve(argv[++index]);
    } else if (arg === "--pack") {
      args.pack = true;
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }

  if (!args.version) {
    throw new Error("Missing --version");
  }
  if (!args.artifactsDir) {
    throw new Error("Missing --artifacts-dir");
  }
  return args;
}

function readJson(filePath) {
  return JSON.parse(readFileSync(filePath, "utf8"));
}

function writeJson(filePath, value) {
  writeFileSync(filePath, `${JSON.stringify(value, null, 2)}\n`);
}

function assertVersionMatchesCargo(version) {
  const cargoToml = readFileSync(path.join(repoRoot, "Cargo.toml"), "utf8");
  const match = cargoToml.match(/^version\s*=\s*"([^"]+)"/m);
  if (!match) {
    throw new Error("Unable to read root package version from Cargo.toml");
  }
  if (match[1] !== version) {
    throw new Error(`Tag/npm version ${version} does not match Cargo.toml version ${match[1]}`);
  }
}

function ensureCleanDir(dir) {
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
}

function sha256(filePath) {
  return createHash("sha256").update(readFileSync(filePath)).digest("hex");
}

function findBinaryForTarget(artifactsDir, target) {
  const binaryName = target.binaryName ?? "orca";
  const archiveExtension = target.archiveExtension ?? "tar.gz";
  const directBinary = path.join(artifactsDir, `orca-${target.targetTriple}`, binaryName);
  const archive = path.join(artifactsDir, `orca-${target.targetTriple}.${archiveExtension}`);
  const checksum = `${archive}.sha256`;
  for (const [filePath, label] of [
    [directBinary, "binary"],
    [archive, "archive"],
    [checksum, "checksum"],
  ]) {
    if (!existsSync(filePath)) throw new Error(`Missing ${label} artifact for ${target.targetTriple}: ${filePath}`);
  }

  const expectedArchiveHash = readFileSync(checksum, "utf8").trim().split(/\s+/)[0];
  const actualArchiveHash = sha256(archive);
  if (!/^[0-9a-f]{64}$/i.test(expectedArchiveHash) || expectedArchiveHash.toLowerCase() !== actualArchiveHash) {
    throw new Error(`Checksum mismatch for ${target.targetTriple}: expected ${expectedArchiveHash}, got ${actualArchiveHash}`);
  }
  const tempDir = mkdtempSync(path.join(os.tmpdir(), `orca-${target.targetTriple}-`));
  try {
    if (archiveExtension === "zip") {
      execFileSync("unzip", ["-q", archive, "-d", tempDir], { stdio: "inherit" });
    } else {
      execFileSync("tar", ["-xzf", archive, "-C", tempDir], { stdio: "inherit" });
    }
    const extracted = path.join(tempDir, binaryName);
    if (!existsSync(extracted)) throw new Error(`Archive ${archive} did not contain orca`);
    if (sha256(extracted) !== sha256(directBinary)) {
      throw new Error(`Package/archive binary mismatch for ${target.targetTriple}`);
    }
  } finally {
    rmSync(tempDir, { recursive: true, force: true });
  }
  return directBinary;
}

async function stagePlatformPackage(target, version, artifactsDir, stageRoot) {
  const packageDir = path.join(stageRoot, target.aliasName.replace("@blade-ai/", ""));
  const vendorBin = path.join(packageDir, "vendor", target.targetTriple, "bin");
  mkdirSync(vendorBin, { recursive: true });

  const binary = findBinaryForTarget(artifactsDir, target);
  const dest = path.join(vendorBin, target.binaryName ?? "orca");
  copyFileSync(binary, dest);
  if (target.helperNames) {
    const archive = path.join(artifactsDir, `orca-${target.targetTriple}.${target.archiveExtension}`);
    const tempDir = mkdtempSync(path.join(os.tmpdir(), `orca-${target.targetTriple}-bundle-`));
    try {
      execFileSync("unzip", ["-q", archive, "-d", tempDir], { stdio: "inherit" });
      for (const helperName of target.helperNames) {
        const helper = path.join(tempDir, helperName);
        if (!existsSync(helper)) throw new Error(`Archive ${archive} did not contain ${helperName}`);
        copyFileSync(helper, path.join(vendorBin, helperName));
      }
    } finally {
      rmSync(tempDir, { recursive: true, force: true });
    }
  }
  chmodSync(dest, 0o755);

  const template = readJson(path.join(repoRoot, "npm", "platform-package.json"));
  writeJson(path.join(packageDir, "package.json"), {
    ...template,
    name: "@blade-ai/orca",
    version: `${version}-${target.versionSuffix}`,
    description: `Native Orca binary for ${target.os}/${target.cpu}.`,
    os: [target.os],
    cpu: [target.cpu]
  });

  await cp(path.join(repoRoot, "README.md"), path.join(packageDir, "README.md"));
  copyFileSync(path.join(repoRoot, "LICENSE"), path.join(packageDir, "LICENSE"));
  return packageDir;
}

async function stageMainPackage(version, stageRoot) {
  const packageDir = path.join(stageRoot, "orca");
  await cp(path.join(repoRoot, "npm", "orca"), packageDir, { recursive: true });
  await cp(path.join(repoRoot, "README.md"), path.join(packageDir, "README.md"));
  copyFileSync(path.join(repoRoot, "LICENSE"), path.join(packageDir, "LICENSE"));

  const packageJsonPath = path.join(packageDir, "package.json");
  const packageJson = readJson(packageJsonPath);
  packageJson.version = version;
  packageJson.optionalDependencies = Object.fromEntries(
    TARGETS.map((target) => [
      target.aliasName,
      `npm:@blade-ai/orca@${version}-${target.versionSuffix}`
    ])
  );
  writeJson(packageJsonPath, packageJson);
  return packageDir;
}

function npmPack(packageDir, tarballDir) {
  mkdirSync(tarballDir, { recursive: true });
  const output = execFileSync("npm", ["pack", "--pack-destination", tarballDir], {
    cwd: packageDir,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"]
  }).trim();
  return path.join(tarballDir, output.split("\n").at(-1));
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  assertVersionMatchesCargo(args.version);

  const stageRoot = path.join(args.outDir, "stage");
  const tarballDir = path.join(args.outDir, "tarballs");
  ensureCleanDir(stageRoot);
  ensureCleanDir(tarballDir);

  const packageDirs = [];
  for (const target of TARGETS) {
    packageDirs.push(await stagePlatformPackage(target, args.version, args.artifactsDir, stageRoot));
  }
  packageDirs.push(await stageMainPackage(args.version, stageRoot));

  if (args.pack) {
    for (const packageDir of packageDirs) {
      const tarball = npmPack(packageDir, tarballDir);
      console.log(`Packed ${tarball}`);
    }
  } else {
    const entries = await readdir(stageRoot);
    console.log(`Staged npm packages: ${entries.join(", ")}`);
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
