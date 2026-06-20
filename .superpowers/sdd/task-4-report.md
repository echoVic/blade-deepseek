# Task 4 Report: GitHub Release Workflow

## Status

Completed.

## Files Changed

- `.github/workflows/release.yml`
- `.superpowers/sdd/task-4-report.md`

## Implementation Summary

Added a `Release` GitHub Actions workflow that:

- runs `cargo test` before release work
- validates the release version against `Cargo.toml`
- builds release binaries for:
  - `aarch64-apple-darwin`
  - `x86_64-apple-darwin`
  - `x86_64-unknown-linux-gnu`
  - `aarch64-unknown-linux-gnu`
- packages each target binary into a tarball plus SHA-256 file
- creates a GitHub Release for tag pushes
- stages npm packages with `scripts/release/stage-npm.mjs`
- smoke-tests the current platform npm package with `scripts/release/smoke-npm.mjs`
- uploads npm tarballs to the GitHub Release
- publishes npm packages only when `secrets.NPM_TOKEN` is present
- clearly skips npm publish when `secrets.NPM_TOKEN` is absent

## GitHub Actions Condition Note

The task brief warned about using `env.NODE_AUTH_TOKEN != ''` without actually setting that environment variable for the steps that test it.

To make that design work correctly in GitHub Actions, the workflow sets:

```yaml
env:
  NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
```

at the `npm` job level, then uses:

- `if: env.NODE_AUTH_TOKEN != ''` for publish
- `if: env.NODE_AUTH_TOKEN == ''` for the explicit skip step

This preserves the intended behavior without exposing the secret.

## Verification

### 1. YAML parse with Ruby

Command:

```bash
ruby -e 'require "yaml"; YAML.load_file(".github/workflows/release.yml"); puts "ok"'
```

Output:

```text
ok
```

### 2. Whitespace / patch hygiene

Command:

```bash
git diff --check
```

Output:

```text
[no output]
```

### 3. Additional local parser check

Command attempted:

```bash
python3 -c 'import yaml, pathlib; yaml.safe_load(pathlib.Path(".github/workflows/release.yml").read_text()); print("ok")'
```

Output:

```text
Traceback (most recent call last):
  File "<string>", line 1, in <module>
ModuleNotFoundError: No module named 'yaml'
```

This was only an optional secondary parser check. The required Ruby YAML validation passed.

### 4. Actions-aware linters / parsers

Checked for locally installed tooling:

- `actionlint` — not installed
- `yamllint` — not installed
- `yq` — not installed

## Limitations

The workflow cannot be fully executed locally from this environment. In particular, I could not locally verify:

- GitHub-hosted runner behavior
- matrix builds across macOS and Ubuntu
- tag-triggered release creation via `softprops/action-gh-release`
- npm publish behavior against real `secrets.NPM_TOKEN`

So local verification is limited to static YAML parsing and patch hygiene, plus manual inspection against the existing release scripts.

## Self-Review

- The artifact upload/download layout matches `scripts/release/stage-npm.mjs`, which looks for binaries under `dist/artifacts/orca-<target>/orca`.
- The manual-dispatch path validates and builds with an explicit `version` input, while GitHub Release and npm publish remain tag-only.
- The npm publish gating is implemented with a job-level `NODE_AUTH_TOKEN` so the step conditions are evaluating a defined environment value.
- The workflow stays within task ownership boundaries and does not modify release scripts, npm package files, README content, or Rust code.

## Concerns

- The macOS matrix uses `macos-latest` for both Apple Silicon and x86_64 targets exactly as specified in the brief, but I could not run that matrix locally to prove hosted-runner compatibility.
- No Actions-specific linter was available locally, so there is some residual risk beyond basic YAML syntax validity.

## Review Finding Fix

- Updated both `actions/download-artifact@v4` steps in `.github/workflows/release.yml` to set `merge-multiple: true`.
- This keeps Task 2's `scripts/release/stage-npm.mjs` contract unchanged while making the workflow download layout match it.

### Artifact Path Reasoning

Before this change, downloading multiple artifacts to `dist/artifacts` would create one subdirectory per artifact name, so the binary for `aarch64-apple-darwin` would land at:

```text
dist/artifacts/orca-aarch64-apple-darwin/orca-aarch64-apple-darwin/orca
```

That does not satisfy `stage-npm.mjs`, which only accepts:

```text
dist/artifacts/orca-aarch64-apple-darwin/orca
```

or the archive fallback:

```text
dist/artifacts/orca-aarch64-apple-darwin.tar.gz
```

With `merge-multiple: true`, `actions/download-artifact` merges the contents of all named artifacts directly into `dist/artifacts`, so each uploaded file lands at the paths the release script already expects:

```text
dist/artifacts/orca-aarch64-apple-darwin/orca
dist/artifacts/orca-aarch64-apple-darwin.tar.gz
dist/artifacts/orca-aarch64-apple-darwin.tar.gz.sha256
```

The same applies for the other targets. This also keeps the release asset collection step working, because `find dist/artifacts -name '*.tar.gz'` and `find dist/artifacts -name '*.sha256'` still locate the packaged archives and checksums after the merged download.

### Verification For This Fix

#### 1. Required Ruby YAML parse

Command:

```bash
ruby -e 'require "yaml"; YAML.load_file(".github/workflows/release.yml"); puts "ok"'
```

Output:

```text
ok
```

#### 2. Required patch hygiene check

Command:

```bash
git diff --check
```

Output:

```text
[no output]
```
