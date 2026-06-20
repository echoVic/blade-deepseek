# Task 3 Report

## Summary

Implemented the npm smoke test script that installs the staged `orca` packages into a throwaway directory and verifies the installed wrapper reports the requested version.

## Files changed

- `scripts/release/smoke-npm.mjs`
- `.superpowers/sdd/task-3-report.md`

## Verification

Executed:

- `node --check scripts/release/smoke-npm.mjs`
- staged fixture packages under a temporary `.../npm/stage` directory by first running `scripts/release/stage-npm.mjs` against a throwaway artifact tree built from `target/release/orca`
- `node scripts/release/smoke-npm.mjs --version 0.1.0 --stage-dir "$tmp/npm/stage"`

Result:

- `node --check` passed
- smoke test printed `orca 0.1.0`

## Self-review

The script now:

- validates `--version` and `--stage-dir`
- maps the current `process.platform` / `process.arch` to the expected staged platform package
- reads the staged platform package name directly from JSON
- creates a throwaway `package.json`, runs `npm install --ignore-scripts`, and launches the installed wrapper through Node's npm-managed `.bin` path with symlink preservation so package resolution happens from the installed tree

## Concerns

No known functional issues remain from the required verification path. The smoke test is intentionally scoped to the current host platform, matching how the wrapper itself resolves its native dependency.
