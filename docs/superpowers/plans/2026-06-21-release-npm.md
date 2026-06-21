# Release and npm Distribution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a reproducible `v0.1.0` GitHub Release and npm distribution pipeline for Orca as `@blade-ai/orca`.

**Architecture:** Follow the OpenAI Codex CLI pattern: a small npm main package exposes `bin/orca.js`, and native platform packages provide the real Rust `orca` binary via optional dependencies. GitHub Actions builds native archives, stages npm packages, uploads release assets, and publishes to npm only when `NPM_TOKEN` is available.

**Tech Stack:** Rust/Cargo, Node.js ESM scripts without third-party npm dependencies, GitHub Actions, npm scoped packages under `@blade-ai`.

## Global Constraints

- First release version is `0.1.0`.
- Git tag format is `v0.1.0`.
- Main npm package name is `@blade-ai/orca`.
- CLI command name is `orca`.
- Supported `0.1.0` targets are `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`, and `x86_64-unknown-linux-gnu`.
- Windows binaries, Homebrew, crates.io, macOS signing, and notarization are out of scope for `0.1.0`.
- npm publishing must be skipped clearly when `NPM_TOKEN` is absent.
- Scripts must not require third-party npm dependencies.
- Publishing must fail before npm publish if the tag version differs from the root `Cargo.toml` package version.

---

## File Structure

- Create `.github/workflows/release.yml`: tag-triggered release pipeline.
- Create `npm/orca/package.json`: source template for the main npm package.
- Create `npm/orca/bin/orca.js`: Node wrapper that selects and spawns the platform binary.
- Create `npm/platform-package.json`: template metadata for platform packages, read by the staging script.
- Create `scripts/release/stage-npm.mjs`: stages main and platform npm package directories from built artifacts.
- Create `scripts/release/smoke-npm.mjs`: local install smoke test for staged npm packages.
- Modify `.gitignore`: ignore `dist/` if it is not already ignored.
- Modify `README.md`: document npm installation and GitHub Release download path.

---

### Task 1: npm Package Templates and Wrapper

**Files:**
- Create: `npm/orca/package.json`
- Create: `npm/orca/bin/orca.js`
- Create: `npm/platform-package.json`
- Modify: `.gitignore`

**Interfaces:**
- Produces: `npm/orca/bin/orca.js`, executable by Node as the `orca` bin.
- Produces: source package metadata consumed by `scripts/release/stage-npm.mjs`.

- [ ] **Step 1: Write the main package manifest**

Create `npm/orca/package.json`:

```json
{
  "name": "@blade-ai/orca",
  "version": "0.0.0-dev",
  "description": "Orca CLI: a DeepSeek-native coding agent.",
  "license": "MIT",
  "bin": {
    "orca": "bin/orca.js"
  },
  "type": "module",
  "engines": {
    "node": ">=16"
  },
  "files": [
    "bin/orca.js",
    "README.md"
  ],
  "repository": {
    "type": "git",
    "url": "git+https://github.com/echoVic/blade-deepseek.git",
    "directory": "npm/orca"
  }
}
```

- [ ] **Step 2: Write the platform package template**

Create `npm/platform-package.json`:

```json
{
  "license": "MIT",
  "files": [
    "vendor",
    "README.md"
  ],
  "repository": {
    "type": "git",
    "url": "git+https://github.com/echoVic/blade-deepseek.git"
  },
  "engines": {
    "node": ">=16"
  }
}
```

- [ ] **Step 3: Write the Node wrapper**

Create `npm/orca/bin/orca.js`:

```javascript
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

const forwardSignal = (signal) => {
  if (!child.killed) {
    child.kill(signal);
  }
};

for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
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
  process.kill(process.pid, result.signal);
} else {
  process.exit(result.exitCode);
}
```

- [ ] **Step 4: Make the wrapper executable**

Run:

```bash
chmod +x npm/orca/bin/orca.js
```

Expected: command exits 0.

- [ ] **Step 5: Ignore staged release outputs**

If `.gitignore` does not already contain `dist/`, append:

```gitignore
dist/
```

- [ ] **Step 6: Run a syntax check**

Run:

```bash
node --check npm/orca/bin/orca.js
```

Expected: command exits 0 with no syntax errors.

- [ ] **Step 7: Commit**

```bash
git add .gitignore npm/orca/package.json npm/orca/bin/orca.js npm/platform-package.json
git commit -m "feat(release): add npm package wrapper"
```

---

### Task 2: npm Staging Script

**Files:**
- Create: `scripts/release/stage-npm.mjs`
- Test: local script execution with fixture artifacts under a temporary directory.

**Interfaces:**
- Consumes: built artifacts in `<artifacts-dir>/orca-<target>/orca` or `<artifacts-dir>/orca-<target>.tar.gz`.
- Produces: staged package directories under `<out-dir>/stage`.
- Produces: npm tarballs under `<out-dir>/tarballs` when `--pack` is passed.

- [ ] **Step 1: Create target metadata in the script**

Create `scripts/release/stage-npm.mjs` with this header and target map:

```javascript
#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync, copyFileSync, chmodSync } from "node:fs";
import { cp, readdir } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const repoRoot = path.resolve(path.dirname(__filename), "..", "..");

const TARGETS = [
  {
    packageName: "@blade-ai/orca-darwin-arm64",
    targetTriple: "aarch64-apple-darwin",
    os: "darwin",
    cpu: "arm64"
  },
  {
    packageName: "@blade-ai/orca-darwin-x64",
    targetTriple: "x86_64-apple-darwin",
    os: "darwin",
    cpu: "x64"
  },
  {
    packageName: "@blade-ai/orca-linux-arm64",
    targetTriple: "aarch64-unknown-linux-gnu",
    os: "linux",
    cpu: "arm64"
  },
  {
    packageName: "@blade-ai/orca-linux-x64",
    targetTriple: "x86_64-unknown-linux-gnu",
    os: "linux",
    cpu: "x64"
  }
];
```

- [ ] **Step 2: Add argument parsing**

Add this parser:

```javascript
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
```

- [ ] **Step 3: Add file helpers**

Add helpers:

```javascript
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
```

- [ ] **Step 4: Add artifact extraction**

Add this function:

```javascript
function findBinaryForTarget(artifactsDir, targetTriple) {
  const directCandidates = [
    path.join(artifactsDir, `orca-${targetTriple}`, "orca"),
    path.join(artifactsDir, targetTriple, "orca"),
    path.join(artifactsDir, "orca")
  ];
  for (const candidate of directCandidates) {
    if (existsSync(candidate)) {
      return candidate;
    }
  }

  const archive = path.join(artifactsDir, `orca-${targetTriple}.tar.gz`);
  if (!existsSync(archive)) {
    throw new Error(`Missing binary artifact for ${targetTriple}`);
  }

  const tempDir = mkdtempSync(path.join(os.tmpdir(), `orca-${targetTriple}-`));
  execFileSync("tar", ["-xzf", archive, "-C", tempDir], { stdio: "inherit" });
  const extracted = path.join(tempDir, "orca");
  if (!existsSync(extracted)) {
    throw new Error(`Archive ${archive} did not contain orca`);
  }
  return extracted;
}
```

- [ ] **Step 5: Stage platform packages**

Add:

```javascript
async function stagePlatformPackage(target, version, artifactsDir, stageRoot) {
  const packageDir = path.join(stageRoot, target.packageName.replace("@blade-ai/", ""));
  const vendorBin = path.join(packageDir, "vendor", target.targetTriple, "bin");
  mkdirSync(vendorBin, { recursive: true });

  const binary = findBinaryForTarget(artifactsDir, target.targetTriple);
  const dest = path.join(vendorBin, "orca");
  copyFileSync(binary, dest);
  chmodSync(dest, 0o755);

  const template = readJson(path.join(repoRoot, "npm", "platform-package.json"));
  writeJson(path.join(packageDir, "package.json"), {
    ...template,
    name: target.packageName,
    version,
    description: `Native Orca binary for ${target.os}/${target.cpu}.`,
    os: [target.os],
    cpu: [target.cpu]
  });

  await cp(path.join(repoRoot, "README.md"), path.join(packageDir, "README.md"));
  return packageDir;
}
```

- [ ] **Step 6: Stage the main package**

Add:

```javascript
async function stageMainPackage(version, stageRoot) {
  const packageDir = path.join(stageRoot, "orca");
  await cp(path.join(repoRoot, "npm", "orca"), packageDir, { recursive: true });
  await cp(path.join(repoRoot, "README.md"), path.join(packageDir, "README.md"));

  const packageJsonPath = path.join(packageDir, "package.json");
  const packageJson = readJson(packageJsonPath);
  packageJson.version = version;
  packageJson.optionalDependencies = Object.fromEntries(
    TARGETS.map((target) => [target.packageName, version])
  );
  writeJson(packageJsonPath, packageJson);
  return packageDir;
}
```

- [ ] **Step 7: Add npm pack support and main flow**

Add:

```javascript
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
```

- [ ] **Step 8: Make script executable**

Run:

```bash
chmod +x scripts/release/stage-npm.mjs
```

Expected: command exits 0.

- [ ] **Step 9: Test with fixture artifacts**

Run:

```bash
tmp="$(mktemp -d)"
for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
  mkdir -p "$tmp/artifacts/orca-$target"
  cp target/release/orca "$tmp/artifacts/orca-$target/orca"
done
node scripts/release/stage-npm.mjs --version 0.1.0 --artifacts-dir "$tmp/artifacts" --out-dir "$tmp/npm" --pack
find "$tmp/npm/tarballs" -name '*.tgz' | wc -l
```

Expected: final count is `5`.

- [ ] **Step 10: Commit**

```bash
git add scripts/release/stage-npm.mjs
git commit -m "feat(release): stage npm packages"
```

---

### Task 3: npm Smoke Test Script

**Files:**
- Create: `scripts/release/smoke-npm.mjs`

**Interfaces:**
- Consumes: staged package directories from Task 2.
- Produces: a local install verification that runs `orca --version`.

- [ ] **Step 1: Write the smoke script**

Create `scripts/release/smoke-npm.mjs`:

```javascript
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

const args = parseArgs(process.argv.slice(2));
const platformPackageDir = path.join(args.stageDir, TARGETS[`${process.platform}:${process.arch}`] ?? "");
if (!existsSync(platformPackageDir)) {
  throw new Error(`No staged platform package for ${process.platform}/${process.arch}`);
}

const mainPackageDir = path.join(args.stageDir, "orca");
const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-npm-smoke-"));
const platformPackageName = JSON.parse(
  readFileSync(path.join(platformPackageDir, "package.json"), "utf8")
).name;
writeFileSync(path.join(tempDir, "package.json"), JSON.stringify({
  private: true,
  dependencies: {
    "@blade-ai/orca": `file:${mainPackageDir}`,
    [platformPackageName]: `file:${platformPackageDir}`
  }
}, null, 2));

execFileSync("npm", ["install", "--ignore-scripts"], { cwd: tempDir, stdio: "inherit" });
const output = execFileSync("node", ["node_modules/@blade-ai/orca/bin/orca.js", "--version"], {
  cwd: tempDir,
  encoding: "utf8"
}).trim();

if (!output.includes(`orca ${args.version}`)) {
  throw new Error(`Unexpected orca version output: ${output}`);
}
console.log(output);
```

- [ ] **Step 2: Make script executable**

Run:

```bash
chmod +x scripts/release/smoke-npm.mjs
```

Expected: command exits 0.

- [ ] **Step 3: Run smoke test against staged fixture packages**

Run after Task 2 staging:

```bash
node scripts/release/smoke-npm.mjs --version 0.1.0 --stage-dir "$tmp/npm/stage"
```

Expected output includes:

```text
orca 0.1.0
```

- [ ] **Step 4: Commit**

```bash
git add scripts/release/smoke-npm.mjs
git commit -m "test(release): add npm smoke test"
```

---

### Task 4: GitHub Release Workflow

**Files:**
- Create: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: tag `v0.1.0`.
- Produces: GitHub Release assets and optional npm publish.

- [ ] **Step 1: Create workflow skeleton**

Create `.github/workflows/release.yml`:

```yaml
name: Release

on:
  push:
    tags:
      - "v*"
  workflow_dispatch:
    inputs:
      version:
        description: "Version to validate, without leading v"
        required: true
        default: "0.1.0"

permissions:
  contents: write

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo test
```

- [ ] **Step 2: Add version validation job**

Add:

```yaml
  version:
    runs-on: ubuntu-latest
    outputs:
      version: ${{ steps.version.outputs.version }}
    steps:
      - uses: actions/checkout@v4
      - id: version
        shell: bash
        run: |
          if [[ "${GITHUB_REF_TYPE}" == "tag" ]]; then
            version="${GITHUB_REF_NAME#v}"
          else
            version="${{ github.event.inputs.version }}"
          fi
          cargo_version="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "([^"]+)"/\1/')"
          if [[ "$version" != "$cargo_version" ]]; then
            echo "Release version $version does not match Cargo.toml version $cargo_version" >&2
            exit 1
          fi
          echo "version=$version" >> "$GITHUB_OUTPUT"
```

- [ ] **Step 3: Add target build matrix**

Add:

```yaml
  build:
    needs: [test, version]
    strategy:
      fail-fast: false
      matrix:
        include:
          - target: aarch64-apple-darwin
            os: macos-latest
          - target: x86_64-apple-darwin
            os: macos-latest
          - target: x86_64-unknown-linux-gnu
            os: ubuntu-latest
          - target: aarch64-unknown-linux-gnu
            os: ubuntu-latest
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}
      - uses: Swatinem/rust-cache@v2
      - name: Install Linux ARM64 linker
        if: matrix.target == 'aarch64-unknown-linux-gnu'
        run: |
          sudo apt-get update
          sudo apt-get install -y gcc-aarch64-linux-gnu
          echo "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc" >> "$GITHUB_ENV"
      - name: Build
        run: cargo build --release --target ${{ matrix.target }}
      - name: Package
        shell: bash
        run: |
          mkdir -p "dist/orca-${{ matrix.target }}"
          cp "target/${{ matrix.target }}/release/orca" "dist/orca-${{ matrix.target }}/orca"
          tar -C "dist/orca-${{ matrix.target }}" -czf "dist/orca-${{ matrix.target }}.tar.gz" orca
          shasum -a 256 "dist/orca-${{ matrix.target }}.tar.gz" > "dist/orca-${{ matrix.target }}.tar.gz.sha256"
      - uses: actions/upload-artifact@v4
        with:
          name: orca-${{ matrix.target }}
          path: |
            dist/orca-${{ matrix.target }}/orca
            dist/orca-${{ matrix.target }}.tar.gz
            dist/orca-${{ matrix.target }}.tar.gz.sha256
```

- [ ] **Step 4: Add GitHub Release job**

Add:

```yaml
  release:
    if: github.ref_type == 'tag'
    needs: [build, version]
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with:
          path: dist/artifacts
      - name: Collect release assets
        shell: bash
        run: |
          mkdir -p dist/release
          find dist/artifacts -name '*.tar.gz' -exec cp {} dist/release/ \;
          find dist/artifacts -name '*.sha256' -exec cp {} dist/release/ \;
      - uses: softprops/action-gh-release@v2
        with:
          tag_name: ${{ github.ref_name }}
          name: Orca ${{ github.ref_name }}
          generate_release_notes: true
          files: dist/release/*
```

- [ ] **Step 5: Add npm package and publish job**

Add:

```yaml
  npm:
    if: github.ref_type == 'tag'
    needs: [build, version, release]
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 20
          registry-url: https://registry.npmjs.org
      - uses: actions/download-artifact@v4
        with:
          path: dist/artifacts
      - name: Stage npm packages
        run: |
          node scripts/release/stage-npm.mjs \
            --version "${{ needs.version.outputs.version }}" \
            --artifacts-dir dist/artifacts \
            --out-dir dist/npm \
            --pack
      - name: Smoke current platform npm package
        run: node scripts/release/smoke-npm.mjs --version "${{ needs.version.outputs.version }}" --stage-dir dist/npm/stage
      - name: Upload npm tarballs to GitHub Release
        uses: softprops/action-gh-release@v2
        with:
          tag_name: ${{ github.ref_name }}
          files: dist/npm/tarballs/*.tgz
      - name: Publish npm packages
        if: env.NODE_AUTH_TOKEN != ''
        env:
          NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
        shell: bash
        run: |
          for package_dir in dist/npm/stage/orca-darwin-arm64 dist/npm/stage/orca-darwin-x64 dist/npm/stage/orca-linux-arm64 dist/npm/stage/orca-linux-x64 dist/npm/stage/orca; do
            name="$(node -e "console.log(require('./${package_dir}/package.json').name)")"
            version="$(node -e "console.log(require('./${package_dir}/package.json').version)")"
            if npm view "${name}@${version}" version >/dev/null 2>&1; then
              echo "${name}@${version} already published; skipping"
            else
              npm publish "$package_dir" --access public
            fi
          done
      - name: Skip npm publish without token
        if: env.NODE_AUTH_TOKEN == ''
        run: echo "NPM_TOKEN is not configured; npm publish skipped."
        env:
          NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
```

- [ ] **Step 6: Validate workflow syntax locally**

Run:

```bash
ruby -e 'require "yaml"; YAML.load_file(".github/workflows/release.yml"); puts "ok"'
```

Expected:

```text
ok
```

- [ ] **Step 7: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci(release): add github and npm release workflow"
```

---

### Task 5: Documentation and Release Notes

**Files:**
- Modify: `README.md`
- Create: `docs/releases/v0.1.0.md`

**Interfaces:**
- Produces: installation instructions for GitHub Release and npm users.

- [ ] **Step 1: Add README installation section**

Add this section near the top of `README.md` after the project introduction:

```markdown
## Installation

### npm

```bash
npm install -g @blade-ai/orca
orca --version
```

The npm package installs a small Node.js launcher and the native `orca` binary for supported platforms.

Supported platforms for `0.1.0`:

- macOS Apple Silicon (`darwin/arm64`)
- macOS Intel (`darwin/x64`)
- Linux x64 (`linux/x64`)
- Linux ARM64 (`linux/arm64`)

### GitHub Releases

Download the archive for your platform from the latest GitHub Release, extract it, and place `orca` on your `PATH`.
```
```

- [ ] **Step 2: Add release notes draft**

Create `docs/releases/v0.1.0.md`:

```markdown
# Orca v0.1.0

First public release of Orca.

## Highlights

- DeepSeek-native coding agent.
- JSONL event stream for automation.
- Approval modes and permission rules.
- Local tool execution with sandboxing.
- Subagent execution support.
- Claude Code-style local workflow runtime with phases, parallel agents, resume cache, background runs, and TUI visibility.
- npm distribution as `@blade-ai/orca`.

## Install

```bash
npm install -g @blade-ai/orca
orca --version
```
```

- [ ] **Step 3: Commit**

```bash
git add README.md docs/releases/v0.1.0.md
git commit -m "docs(release): add v0.1.0 install notes"
```

---

### Task 6: End-to-End Release Dry Run

**Files:**
- No source files expected.

**Interfaces:**
- Verifies: release scripts, wrapper, Rust build, and npm smoke test.

- [ ] **Step 1: Run Rust verification**

Run:

```bash
cargo test
cargo build --release
./target/release/orca --version
```

Expected final output:

```text
orca 0.1.0
```

- [ ] **Step 2: Run npm staging against local fixture artifacts**

Run:

```bash
tmp="$(mktemp -d)"
for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
  mkdir -p "$tmp/artifacts/orca-$target"
  cp target/release/orca "$tmp/artifacts/orca-$target/orca"
done
node scripts/release/stage-npm.mjs --version 0.1.0 --artifacts-dir "$tmp/artifacts" --out-dir "$tmp/npm" --pack
node scripts/release/smoke-npm.mjs --version 0.1.0 --stage-dir "$tmp/npm/stage"
```

Expected output includes:

```text
orca 0.1.0
```

- [ ] **Step 3: Confirm tag does not already exist**

Run:

```bash
git tag --list 'v0.1.0'
git ls-remote --tags origin 'v0.1.0'
```

Expected: both commands print nothing.

- [ ] **Step 4: Confirm working tree is clean**

Run:

```bash
git status --short
```

Expected: no output.

- [ ] **Step 5: Create and push the release tag**

Run only after the user confirms npm `NPM_TOKEN` is configured in GitHub repository secrets or accepts GitHub-only release assets:

```bash
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin main --tags
```

Expected: GitHub Actions starts the `Release` workflow for `v0.1.0`.

---

## Self-Review Checklist

- [ ] Spec coverage: npm package names, targets, GitHub Release workflow, npm publishing, version checks, and no-token behavior are all covered.
- [ ] Placeholder scan: no `TBD`, `TODO`, or vague implementation placeholders remain.
- [ ] Type consistency: target triples and package names match the design spec.
- [ ] Test coverage: Rust tests, wrapper syntax check, staging fixture test, smoke install test, workflow YAML parse, and final dry run are all included.
