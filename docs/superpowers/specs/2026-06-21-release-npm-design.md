# Release and npm Distribution Design

## Goal

Publish the first public release as `v0.1.0` and make Orca installable from npm as `@blade-ai/orca`, while also attaching native binaries to GitHub Releases.

## References

- OpenAI Codex CLI uses a small npm wrapper package with native platform packages as optional dependencies.
- Codex wrapper reference: `codex-cli/bin/codex.js` in `openai/codex`.
- Codex npm staging reference: `codex-cli/scripts/build_npm_package.py` and `scripts/stage_npm_packages.py` in `openai/codex`.

## Package Names

- Main npm package: `@blade-ai/orca`
- CLI command: `orca`
- Platform packages:
  - `@blade-ai/orca-darwin-arm64`
  - `@blade-ai/orca-darwin-x64`
  - `@blade-ai/orca-linux-arm64`
  - `@blade-ai/orca-linux-x64`

Windows packages are intentionally excluded from `0.1.0`. A future release can add `@blade-ai/orca-win32-x64` and `@blade-ai/orca-win32-arm64`.

## npm Architecture

The main package contains only:

- `package.json`
- `bin/orca.js`
- `README.md`

`bin/orca.js` detects `process.platform` and `process.arch`, resolves the matching optional dependency, and spawns the native `orca` binary with inherited stdio. It forwards `SIGINT`, `SIGTERM`, and `SIGHUP` to the child process and exits with the child's exit code or signal behavior.

Each platform package contains:

- `package.json`
- `vendor/<target-triple>/bin/orca`
- `README.md`

The main package lists the platform packages in `optionalDependencies` using the same version as the release. npm installs only the package matching the user's platform because each platform package declares `os` and `cpu`.

## Supported Targets

| npm package | Rust target | GitHub runner |
| --- | --- | --- |
| `@blade-ai/orca-darwin-arm64` | `aarch64-apple-darwin` | `macos-latest` |
| `@blade-ai/orca-darwin-x64` | `x86_64-apple-darwin` | `macos-latest` |
| `@blade-ai/orca-linux-x64` | `x86_64-unknown-linux-gnu` | `ubuntu-latest` |
| `@blade-ai/orca-linux-arm64` | `aarch64-unknown-linux-gnu` | `ubuntu-latest` |

Linux ARM64 is built with cross compilation. If cross compilation is not reliable in the first workflow run, the release workflow should fail before publishing npm packages.

## GitHub Release Workflow

Create `.github/workflows/release.yml`.

Trigger:

- `push` tags matching `v*`
- `workflow_dispatch` for dry-run validation

Jobs:

1. `test`
   - Checkout repository.
   - Install Rust stable.
   - Run `cargo test`.

2. `build`
   - Matrix over the four supported targets.
   - Build `orca` with `cargo build --release --target <target>`.
   - Package each binary as `orca-<target>.tar.gz`.
   - Generate SHA-256 checksums.
   - Upload build artifacts.

3. `release`
   - Download target artifacts.
   - Create or update the GitHub Release for the tag.
   - Upload all `orca-<target>.tar.gz` archives and checksum files.

4. `npm`
   - Runs only for tag pushes.
   - Downloads target artifacts.
   - Stages the main npm package and four platform packages.
   - Runs `npm pack --dry-run` for every package.
   - If `NPM_TOKEN` is present, publishes platform packages first, then the main package.
   - If `NPM_TOKEN` is absent, uploads npm tarballs to the GitHub Release and skips publishing.

## Local Packaging Scripts

Add scripts under `scripts/release/`:

- `stage-npm.mjs`
  - Inputs: `--version`, `--artifacts-dir`, `--out-dir`.
  - Creates package directories under `dist/npm/stage`.
  - Writes package manifests with exact version.
  - Copies the matching native binary into each platform package.
  - Writes the main package `optionalDependencies`.

- `smoke-npm.mjs`
  - Packs the staged main package.
  - Installs it into a temporary directory with local file dependencies.
  - Runs `node <installed-bin>/orca --version`.
  - Verifies output contains `orca <version>`.

The scripts must not require third-party npm dependencies.

## Release Version Rules

- The first release is `0.1.0`.
- The git tag is `v0.1.0`.
- The Rust crate versions and npm package versions must match `0.1.0`.
- The workflow derives the version from the tag by stripping the leading `v`.
- Publishing must fail if the tag version differs from the root `Cargo.toml` package version.

## Failure Behavior

- If tests fail, no binaries are published.
- If any target build fails, no npm package is published.
- If npm staging or smoke testing fails, GitHub Release binaries may still exist, but npm publish must not run.
- If npm publish partially fails after some platform packages are published, rerunning the workflow should be safe because npm publish for already-existing versions should be detected and skipped or treated as already complete.

## Security and Secrets

- Use GitHub `GITHUB_TOKEN` for GitHub Releases.
- Use npm `NPM_TOKEN` for npm publish.
- Do not echo tokens.
- Do not require npm credentials for local packaging tests.

## Out of Scope for `0.1.0`

- crates.io publication.
- Homebrew formula.
- Windows binaries.
- Signed macOS binaries or notarization.
- Auto-generated changelog beyond the GitHub Release body.

## Acceptance Criteria

- `git tag v0.1.0 && git push origin v0.1.0` starts the release workflow.
- GitHub Release `v0.1.0` contains four native binary archives and checksums.
- `npm install -g @blade-ai/orca` installs the `orca` command on supported platforms after npm publish.
- `orca --version` prints `orca 0.1.0` when installed through npm.
- If `NPM_TOKEN` is missing, the workflow still creates GitHub Release artifacts and npm tarballs, but clearly skips npm publish.
