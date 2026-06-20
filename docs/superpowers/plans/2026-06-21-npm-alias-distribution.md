# npm Alias Distribution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Release `0.1.1` using Codex-style npm aliases so all npm tarballs publish under `@blade-ai/orca`.

**Architecture:** Keep the main wrapper and platform binary layout, but change staged platform package metadata to `name: "@blade-ai/orca"` and `version: "<base>-<platform>"`. Main package optional dependencies keep platform alias keys but point to `npm:@blade-ai/orca@<platform-version>`.

**Tech Stack:** Node.js ESM release scripts, npm package aliases, GitHub Actions, Rust/Cargo.

---

## File Structure

- Modify `Cargo.toml`: bump root package version to `0.1.1`.
- Modify `scripts/release/stage-npm.mjs`: stage platform prerelease versions under the main package name.
- Modify `scripts/release/smoke-npm.mjs`: resolve tarballs by package directory metadata and support alias tarballs.
- Create `scripts/release/test-stage-npm.mjs`: assert staged package metadata matches alias distribution.
- Modify `.github/workflows/release.yml`: publish staged package directories in script-discovered order.
- Add `docs/releases/v0.1.1.md`: release notes for the packaging correction.
- Modify `README.md`: installation text remains `npm install -g @blade-ai/orca`, with no platform package install instructions.

## Tasks

### Task 1: Add Alias Distribution Test

- [ ] Create `scripts/release/test-stage-npm.mjs` that stages fake artifacts for `0.1.1`, asserts platform package directories contain `name: "@blade-ai/orca"` and platform prerelease versions, asserts main optional dependencies use `npm:@blade-ai/orca@...`, and asserts tarball names are `blade-ai-orca-0.1.1-<platform>.tgz`.
- [ ] Run `node scripts/release/test-stage-npm.mjs` and confirm it fails against the current independent-package implementation.
- [ ] Commit the failing test only if the implementation is not changed in the same step; otherwise include it with the implementation commit after verifying red/green locally.

### Task 2: Implement Alias Staging and Smoke

- [ ] Change `stage-npm.mjs` target metadata to include `aliasName` and `platformVersionSuffix`.
- [ ] Stage platform package directories with alias directory names but `package.json.name` set to `@blade-ai/orca` and `package.json.version` set to `0.1.1-<platform>`.
- [ ] Set main package optional dependencies to alias specs like `npm:@blade-ai/orca@0.1.1-darwin-arm64`.
- [ ] Update `smoke-npm.mjs` so local tarball smoke installs the main package tarball and current platform tarball through the platform alias dependency key.
- [ ] Run `node scripts/release/test-stage-npm.mjs` and confirm it passes.

### Task 3: Bump Version and Release Docs

- [ ] Bump `Cargo.toml` root package version from `0.1.0` to `0.1.1`.
- [ ] Add `docs/releases/v0.1.1.md` documenting the npm packaging correction.
- [ ] Ensure README install command remains `npm install -g @blade-ai/orca`.

### Task 4: Workflow Publish Order

- [ ] Update `.github/workflows/release.yml` npm publish step to discover package directories from a manifest produced by `stage-npm.mjs` or from deterministic directory order.
- [ ] Ensure platform prerelease directories publish before the main stable directory.
- [ ] Ensure already-published versions are skipped by checking `name@version`.

### Task 5: Verification and Release

- [ ] Run Node syntax checks for all release scripts.
- [ ] Run `cargo test`.
- [ ] Run `cargo build --release`.
- [ ] Run staging and tarball smoke for `0.1.1`.
- [ ] Commit and push `main`.
- [ ] Create and push `v0.1.1`.
- [ ] Verify GitHub Release workflow succeeds.
- [ ] Verify `npm install @blade-ai/orca@0.1.1` prints `orca 0.1.1`.
- [ ] Unpublish the five `0.1.0` npm versions after `0.1.1` verification succeeds.
