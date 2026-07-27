#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const TARGETS = [
  ["darwin-arm64", "aarch64-apple-darwin"],
  ["darwin-x64", "x86_64-apple-darwin"],
  ["linux-arm64", "aarch64-unknown-linux-gnu"],
  ["linux-x64", "x86_64-unknown-linux-gnu"],
];

function parseArgs(argv) {
  const args = { version: null, repo: "echoVic/blade-deepseek", packageName: "@blade-ai/orca", bin: "orca", retries: 12, retryDelayMs: 10000 };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--version") args.version = argv[++index];
    else if (arg === "--repo") args.repo = argv[++index];
    else if (arg === "--package") args.packageName = argv[++index];
    else if (arg === "--bin") args.bin = argv[++index];
    else if (arg === "--retries") args.retries = Number.parseInt(argv[++index], 10);
    else if (arg === "--retry-delay-ms") args.retryDelayMs = Number.parseInt(argv[++index], 10);
    else throw new Error(`Unknown argument: ${arg}`);
  }
  if (!args.version) throw new Error("Missing --version");
  if (!Number.isInteger(args.retries) || args.retries < 1) throw new Error("--retries must be a positive integer");
  if (!Number.isInteger(args.retryDelayMs) || args.retryDelayMs < 0) throw new Error("--retry-delay-ms must be a non-negative integer");
  return args;
}

function run(command, args, options = {}) {
  return execFileSync(command, args, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], ...options }).trim();
}

function json(command, args, label, options) {
  const output = run(command, args, options);
  try { return JSON.parse(output); } catch (error) { throw new Error(`Unable to parse ${label} JSON: ${error.message}\n${output}`); }
}

function digest(filePath, algorithm = "sha256", encoding = "hex") {
  return createHash(algorithm).update(readFileSync(filePath)).digest(encoding);
}

async function retry(label, args, operation) {
  let lastError;
  for (let attempt = 1; attempt <= args.retries; attempt += 1) {
    try { return operation(); } catch (error) {
      lastError = error;
      if (attempt === args.retries) break;
      console.log(`${label}: attempt ${attempt}/${args.retries} failed: ${error.message}`);
      await new Promise((resolve) => setTimeout(resolve, args.retryDelayMs));
    }
  }
  throw lastError;
}

function extract(tarball, destination) {
  mkdirSync(destination, { recursive: true });
  run("tar", ["-xzf", tarball, "-C", destination]);
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const version = args.version.replace(/^v/, "");
  const tag = `v${version}`;
  const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-verify-published-"));
  try {
    const release = await retry("GitHub Release verification", args, () => json("gh", ["release", "view", tag, "--repo", args.repo, "--json", "tagName,url,isDraft,isPrerelease,assets"], "GitHub Release"));
    if (release.tagName !== tag || release.isDraft) throw new Error(`GitHub Release ${tag} is missing, mismatched, or draft`);
    const tagSha = run("gh", ["api", `repos/${args.repo}/commits/${tag}`, "--jq", ".sha"]);
    const mainSha = run("gh", ["api", `repos/${args.repo}/commits/main`, "--jq", ".sha"]);
    if (tagSha !== mainSha) throw new Error(`Release tag target ${tagSha} does not match main ${mainSha}`);

    const expectedAssets = [
      ...TARGETS.flatMap(([, triple]) => [`orca-${triple}.tar.gz`, `orca-${triple}.tar.gz.sha256`]),
      ...TARGETS.map(([suffix]) => `blade-ai-orca-${version}-${suffix}.tgz`),
      `blade-ai-orca-${version}.tgz`,
    ];
    const assetNames = new Set((release.assets ?? []).map((asset) => asset.name));
    for (const name of expectedAssets) if (!assetNames.has(name)) throw new Error(`GitHub Release missing asset ${name}`);
    run("gh", ["release", "download", tag, "--repo", args.repo, "--dir", tempDir]);
    for (const [, triple] of TARGETS) {
      const archive = path.join(tempDir, `orca-${triple}.tar.gz`);
      const checksum = readFileSync(`${archive}.sha256`, "utf8").trim().split(/\s+/)[0];
      if (checksum !== digest(archive)) throw new Error(`Checksum failure for ${path.basename(archive)}`);
    }

    const npmDir = path.join(tempDir, "npm");
    mkdirSync(npmDir);
    const packed = new Map();
    for (const [suffix] of [...TARGETS, [null]]) {
      const packageVersion = suffix ? `${version}-${suffix}` : version;
      const spec = `${args.packageName}@${packageVersion}`;
      const metadata = await retry(`npm metadata ${spec}`, args, () => json("npm", ["view", spec, "--json"], `npm metadata ${spec}`));
      if (metadata.name !== args.packageName || metadata.version !== packageVersion) throw new Error(`npm version mismatch for ${spec}`);
      if (!metadata.dist?.integrity?.startsWith("sha512-")) throw new Error(`npm registry integrity missing for ${spec}`);
      if (!suffix) {
        const expectedAliases = Object.fromEntries(TARGETS.map(([alias]) => [`@blade-ai/orca-${alias}`, `npm:@blade-ai/orca@${version}-${alias}`]));
        if (JSON.stringify(metadata.optionalDependencies) !== JSON.stringify(expectedAliases)) throw new Error(`wrong optional-dependency aliases for ${spec}`);
      }
      const fileName = await retry(`npm pack ${spec}`, args, () => run("npm", ["pack", spec, "--pack-destination", npmDir]));
      const tarball = path.join(npmDir, fileName.split(/\r?\n/).at(-1));
      const actualIntegrity = `sha512-${digest(tarball, "sha512", "base64")}`;
      if (actualIntegrity !== metadata.dist.integrity) throw new Error(`registry integrity mismatch for ${spec}`);
      const releaseTarball = path.join(tempDir, path.basename(tarball));
      if (!existsSync(releaseTarball) || digest(releaseTarball) !== digest(tarball)) {
        throw new Error(`GitHub/npm tarball mismatch for ${spec}`);
      }
      packed.set(packageVersion, tarball);
    }

    for (const [suffix, triple] of TARGETS) {
      const releaseExtract = path.join(tempDir, `release-${suffix}`);
      const npmExtract = path.join(tempDir, `npm-${suffix}`);
      extract(path.join(tempDir, `orca-${triple}.tar.gz`), releaseExtract);
      extract(packed.get(`${version}-${suffix}`), npmExtract);
      const releaseBinary = path.join(releaseExtract, "orca");
      const npmBinary = path.join(npmExtract, "package", "vendor", triple, "bin", "orca");
      if (!existsSync(npmBinary) || digest(releaseBinary) !== digest(npmBinary)) throw new Error(`package/archive binary mismatch for ${triple}`);
    }

    const installDir = path.join(tempDir, "install");
    mkdirSync(installDir);
    writeFileSync(path.join(installDir, "package.json"), `${JSON.stringify({ private: true }, null, 2)}\n`);
    run("npm", ["install", "--save-exact", `${args.packageName}@${version}`], { cwd: installDir });
    const installed = JSON.parse(readFileSync(path.join(installDir, "node_modules", "@blade-ai", "orca", "package.json"), "utf8"));
    if (installed.version !== version) throw new Error(`clean install resolved ${installed.version}, expected ${version}`);
    const smoke = run(path.join(installDir, "node_modules", ".bin", args.bin), ["--version"], { cwd: installDir });
    if (!smoke.includes(`${args.bin} ${version}`)) throw new Error(`Unexpected installed binary output: ${smoke}`);
    console.log(`Published release verified: ${tag} ${mainSha}`);
  } finally {
    rmSync(tempDir, { recursive: true, force: true });
  }
}

main().catch((error) => { console.error(error); process.exit(1); });
