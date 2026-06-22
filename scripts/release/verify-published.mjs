#!/usr/bin/env node

import { execFileSync } from "node:child_process";

function parseArgs(argv) {
  const args = {
    version: null,
    repo: "echoVic/blade-deepseek",
    packageName: "@blade-ai/orca",
    bin: "orca",
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--version") {
      args.version = argv[++index];
    } else if (arg === "--repo") {
      args.repo = argv[++index];
    } else if (arg === "--package") {
      args.packageName = argv[++index];
    } else if (arg === "--bin") {
      args.bin = argv[++index];
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }

  if (!args.version) {
    throw new Error("Missing --version");
  }
  return args;
}

function run(command, args, options = {}) {
  return execFileSync(command, args, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    ...options,
  }).trim();
}

function parseJson(output, label) {
  try {
    return JSON.parse(output);
  } catch (error) {
    throw new Error(`Unable to parse ${label} JSON: ${error.message}\n${output}`);
  }
}

function assertEqual(actual, expected, label) {
  if (actual !== expected) {
    throw new Error(`${label}: expected ${expected}, got ${actual}`);
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const tag = args.version.startsWith("v") ? args.version : `v${args.version}`;
  const version = args.version.replace(/^v/, "");
  const packageSpec = `${args.packageName}@${version}`;

  const release = parseJson(
    run("gh", ["release", "view", tag, "--repo", args.repo, "--json", "tagName,url,isDraft,isPrerelease,publishedAt"]),
    "GitHub Release",
  );
  assertEqual(release.tagName, tag, "GitHub Release tag");
  if (release.isDraft) {
    throw new Error(`GitHub Release ${tag} is still a draft`);
  }
  console.log(`GitHub Release verified: ${release.url ?? tag}`);

  const npmVersion = parseJson(
    run("npm", ["view", packageSpec, "version", "--json"]),
    "npm version",
  );
  assertEqual(npmVersion, version, "npm package version");
  console.log(`npm package verified: ${packageSpec}`);

  const smoke = run("npm", [
    "exec",
    "--yes",
    "--package",
    packageSpec,
    "--",
    args.bin,
    "--version",
  ]);
  if (!smoke.includes(`${args.bin} ${version}`)) {
    throw new Error(`Unexpected npm exec smoke output: ${smoke}`);
  }
  console.log(`npm exec smoke verified: ${smoke}`);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
