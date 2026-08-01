#!/usr/bin/env node

import { existsSync, readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const DEFAULT_MANIFEST =
  "docs/superpowers/specs/2026-07-28-native-windows-platform-foundation.manifest.json";
const SKIPPED_DIRECTORIES = new Set([".git", ".worktrees", "target"]);
const REVIEWED_OPERATION_PATTERNS = new Map([
  [
    "unix_shell_spawn",
    String.raw`(?:std::process::)?(?:Command|ProcessCommand)::new\("(?:sh|bash)"\)`,
  ],
  ["unix_std_api", String.raw`std::os::unix::`],
  [
    "unix_libc_process",
    String.raw`libc::(?:flock|kill|killpg|setsid|setpgid|fcntl|SIGTERM|SIGKILL|LOCK_EX|LOCK_NB|LOCK_UN|O_NONBLOCK|O_CLOEXEC|O_NOFOLLOW)`,
  ],
  [
    "unix_flock_ffi",
    String.raw`fn (?:owner_)?flock\(fd: i32, operation: i32\)`,
  ],
  [
    "non_unix_lock_stub",
    String.raw`#\[cfg\(not\(unix\)\)\][\s\S]{0,160}?fn (?:try_lock_runtime_file|try_lock_exclusive|unlock_exclusive|lock_file_impl|unlock_file_impl)\b`,
  ],
  [
    "direct_temp_rename",
    String.raw`(?:std::fs::rename|fs::rename)\(\s*&?temp_path\b`,
  ],
]);
const REVIEWED_DEFERRED_OWNERS = new Set([
  "conpty-tui-plan",
  "process-ownership-plan",
  "windows-sandbox-plan",
  "windows-test-portability",
]);

function fail(message) {
  throw new Error(`windows platform boundary contract: ${message}`);
}

export function parseManifestText(text) {
  try {
    return JSON.parse(text);
  } catch (error) {
    fail(`malformed manifest JSON: ${error.message}`);
  }
}

function requireString(value, label) {
  if (typeof value !== "string" || value.length === 0) {
    fail(`${label} must be a non-empty string`);
  }
}

function requireRelativePath(value, label) {
  requireString(value, label);
  if (
    path.isAbsolute(value) ||
    value.includes("\\") ||
    value.split("/").includes("..")
  ) {
    fail(`${label} must be a normalized repository-relative path`);
  }
}

function inventoryRows(manifest) {
  return [
    ...manifest.foundation_exceptions.map((row) => ({
      table: "foundation_exceptions",
      row,
    })),
    ...manifest.deferred_boundaries.map((row) => ({
      table: "deferred_boundaries",
      row,
    })),
  ];
}

export function validateManifest(manifest) {
  if (
    manifest === null ||
    typeof manifest !== "object" ||
    Array.isArray(manifest)
  ) {
    fail("manifest root must be an object");
  }
  if (manifest.schema_version !== 1) {
    fail("schema_version must be 1");
  }
  if (manifest.contract !== "native-windows-platform-foundation") {
    fail(
      "contract name is not the reviewed native Windows foundation contract",
    );
  }
  requireRelativePath(manifest.platform_owner, "platform_owner");
  if (
    !Array.isArray(manifest.operation_patterns) ||
    manifest.operation_patterns.length !== REVIEWED_OPERATION_PATTERNS.size
  ) {
    fail("reviewed operation pattern set drift");
  }
  if (
    !Array.isArray(manifest.foundation_exceptions) ||
    !Array.isArray(manifest.deferred_boundaries)
  ) {
    fail("foundation_exceptions and deferred_boundaries must be arrays");
  }

  const operationIds = new Set();
  for (const [index, row] of manifest.operation_patterns.entries()) {
    if (!Array.isArray(row) || row.length !== 2) {
      fail(`operation_patterns row ${index} must have 2 columns`);
    }
    const [id, source] = row;
    requireString(id, `operation_patterns row ${index} id`);
    requireString(source, `operation_patterns row ${index} regex`);
    if (operationIds.has(id)) {
      fail(`duplicate operation id ${id}`);
    }
    const reviewedSource = REVIEWED_OPERATION_PATTERNS.get(id);
    if (reviewedSource === undefined) {
      fail(`unknown operation pattern ${id}`);
    }
    if (source !== reviewedSource) {
      fail(`reviewed regex drift for operation ${id}`);
    }
    operationIds.add(id);
    try {
      new RegExp(source, "g");
    } catch (error) {
      fail(`invalid regex for operation ${id}: ${error.message}`);
    }
  }

  const boundaryIds = new Set();
  const identities = new Set();
  for (const { table, row } of inventoryRows(manifest)) {
    const expectedWidth = table === "foundation_exceptions" ? 4 : 5;
    if (!Array.isArray(row) || row.length !== expectedWidth) {
      fail(`${table} row must have ${expectedWidth} columns`);
    }
    const [id, relativePath, operationId, count, owner] = row;
    requireString(id, `${table} boundary id`);
    requireRelativePath(relativePath, `${table} ${id} path`);
    if (!operationIds.has(operationId)) {
      fail(`${table} ${id} references unknown operation ${operationId}`);
    }
    if (!Number.isInteger(count) || count <= 0) {
      fail(`${table} ${id} count must be a positive integer`);
    }
    if (table === "deferred_boundaries") {
      requireString(owner, `${table} ${id} owner`);
      if (!REVIEWED_DEFERRED_OWNERS.has(owner)) {
        fail(`${table} ${id} has unknown deferred owner ${owner}`);
      }
    }
    if (boundaryIds.has(id)) {
      fail(`duplicate boundary id ${id}`);
    }
    boundaryIds.add(id);
    const identity = `${relativePath}\0${operationId}`;
    if (identities.has(identity)) {
      fail(`duplicate boundary identity ${relativePath} ${operationId}`);
    }
    identities.add(identity);
  }
  return manifest;
}

function collectRustSources(repoRoot) {
  const result = [];
  const visit = (absoluteDirectory, relativeDirectory) => {
    for (const entry of readdirSync(absoluteDirectory, {
      withFileTypes: true,
    })) {
      if (entry.isDirectory() && SKIPPED_DIRECTORIES.has(entry.name)) {
        continue;
      }
      const absolutePath = path.join(absoluteDirectory, entry.name);
      const relativePath = relativeDirectory
        ? `${relativeDirectory}/${entry.name}`
        : entry.name;
      if (entry.isDirectory()) {
        visit(absolutePath, relativePath);
      } else if (entry.isFile() && entry.name.endsWith(".rs")) {
        result.push(relativePath);
      }
    }
  };
  visit(repoRoot, "");
  return result.sort();
}

const NON_PORTABLE_TEST_FIXTURE_PATTERNS = new Map([
  [
    "host-canonical-path",
    String.raw`CanonicalPath::try_new\(\s*(?:std::path::)?PathBuf::from\("/tmp(?:/[^"]*)?"\)\s*\)\s*\.(?:unwrap|expect|is_ok)`,
  ],
  [
    "direct-unix-command-argv",
    String.raw`vec!\[\s*"sh"\.to_string\(\)\s*,\s*"-lc"\.to_string\(\)`,
  ],
]);

function testFixtureSource(relativePath, source) {
  if (
    relativePath.startsWith("tests/") ||
    relativePath.includes("/tests/") ||
    /(?:^|\/)test[^/]*\.rs$/.test(relativePath) ||
    /_tests\.rs$/.test(relativePath)
  ) {
    return { source, lineOffset: 0 };
  }

  const marker = /#\[cfg\(test\)\]\s*mod\s+tests\s*\{/g;
  let match = null;
  for (const candidate of source.matchAll(marker)) {
    match = candidate;
  }
  if (match === null) {
    return null;
  }
  const start = match.index;
  return {
    source: source.slice(start),
    lineOffset: source.slice(0, start).split("\n").length - 1,
  };
}

function matchIsInsidePlatformHelper(source, matchIndex) {
  let functionName = null;
  for (const match of source
    .slice(0, matchIndex)
    .matchAll(/\bfn\s+([A-Za-z0-9_]+)\s*\(/g)) {
    functionName = match[1];
  }
  return (
    functionName?.startsWith("platform_") || functionName === "test_command_argv"
  );
}

function matchHasReviewedFixtureException(source, matchIndex) {
  const lineStart = source.lastIndexOf("\n", matchIndex) + 1;
  const previousLineStart = source.lastIndexOf("\n", lineStart - 2) + 1;
  return source
    .slice(previousLineStart, matchIndex)
    .includes("windows-platform-boundary: protocol-shape-only");
}

export function validatePortableTestFixtures({
  repoRoot,
  sourceOverrides = new Map(),
} = {}) {
  requireString(repoRoot, "repoRoot");
  const rustSources = new Set(collectRustSources(repoRoot));
  for (const relativePath of sourceOverrides.keys()) {
    requireRelativePath(relativePath, "source override path");
    rustSources.add(relativePath);
  }

  const violations = [];
  for (const relativePath of [...rustSources].sort()) {
    const source = sourceOverrides.has(relativePath)
      ? sourceOverrides.get(relativePath)
      : readFileSync(path.join(repoRoot, relativePath), "utf8");
    const fixture = testFixtureSource(relativePath, source);
    if (fixture === null) {
      continue;
    }
    for (const [patternId, regexSource] of NON_PORTABLE_TEST_FIXTURE_PATTERNS) {
      for (const match of fixture.source.matchAll(new RegExp(regexSource, "g"))) {
        if (
          matchIsInsidePlatformHelper(fixture.source, match.index) ||
          matchHasReviewedFixtureException(fixture.source, match.index)
        ) {
          continue;
        }
        const line =
          fixture.lineOffset +
          fixture.source.slice(0, match.index).split("\n").length;
        violations.push(`${relativePath}:${line} ${patternId}`);
      }
    }
  }

  if (violations.length > 0) {
    fail(
      `non-portable test fixtures must use host-platform helpers:\n${violations.join(
        "\n",
      )}`,
    );
  }
  return true;
}

function countMatches(source, regexSource) {
  return Array.from(source.matchAll(new RegExp(regexSource, "g"))).length;
}

export function validateCurrentInventory(
  uncheckedManifest,
  { repoRoot, sourceOverrides = new Map() } = {},
) {
  const manifest = validateManifest(uncheckedManifest);
  requireString(repoRoot, "repoRoot");

  const reviewed = new Map();
  for (const { row } of inventoryRows(manifest)) {
    const [, relativePath, operationId, count] = row;
    const absolutePath = path.resolve(repoRoot, relativePath);
    if (
      !absolutePath.startsWith(`${path.resolve(repoRoot)}${path.sep}`) ||
      !existsSync(absolutePath)
    ) {
      fail(`inventory path does not exist: ${relativePath}`);
    }
    reviewed.set(`${relativePath}\0${operationId}`, count);
  }

  const patterns = new Map(manifest.operation_patterns);
  const rustSources = new Set(collectRustSources(repoRoot));
  for (const relativePath of sourceOverrides.keys()) {
    requireRelativePath(relativePath, "source override path");
    rustSources.add(relativePath);
  }

  const observed = new Map();
  for (const relativePath of [...rustSources].sort()) {
    if (
      relativePath === manifest.platform_owner ||
      relativePath.startsWith(`${manifest.platform_owner}/`)
    ) {
      continue;
    }
    const source = sourceOverrides.has(relativePath)
      ? sourceOverrides.get(relativePath)
      : readFileSync(path.join(repoRoot, relativePath), "utf8");
    for (const [operationId, regexSource] of patterns) {
      const count = countMatches(source, regexSource);
      if (count === 0) {
        continue;
      }
      const identity = `${relativePath}\0${operationId}`;
      observed.set(identity, count);
      const expected = reviewed.get(identity);
      if (expected === undefined) {
        fail(
          `unreviewed direct platform operation ${operationId} in ${relativePath}`,
        );
      }
      if (count !== expected) {
        fail(
          `reviewed platform operation count drift for ${operationId} in ${relativePath}: expected ${expected}, found ${count}`,
        );
      }
    }
  }

  for (const [identity, expected] of reviewed) {
    const observedCount = observed.get(identity) ?? 0;
    if (observedCount !== expected) {
      const [relativePath, operationId] = identity.split("\0");
      fail(
        `stale inventory for ${operationId} in ${relativePath}: expected ${expected}, found ${observedCount}`,
      );
    }
  }
  return true;
}

function main() {
  const repoRoot = path.resolve(
    path.dirname(fileURLToPath(import.meta.url)),
    "..",
  );
  const manifestPath = path.join(repoRoot, process.argv[2] ?? DEFAULT_MANIFEST);
  const manifest = parseManifestText(readFileSync(manifestPath, "utf8"));
  validateCurrentInventory(manifest, { repoRoot });
  validatePortableTestFixtures({ repoRoot });
  console.log("windows platform boundary contract passed");
}

const invokedPath = process.argv[1] ? path.resolve(process.argv[1]) : null;
if (invokedPath === fileURLToPath(import.meta.url)) {
  try {
    main();
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
