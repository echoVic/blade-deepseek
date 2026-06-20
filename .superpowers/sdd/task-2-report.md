# Task 2 Report: npm Staging Script

## What changed
- Added `scripts/release/stage-npm.mjs` as the local npm staging script from the brief.
- Implemented argument parsing for `--version`, `--artifacts-dir`, `--out-dir`, and `--pack`.
- Added helpers for JSON I/O, Cargo version validation, clean output directories, and artifact discovery from either unpacked binaries or `.tar.gz` archives.
- Staged four platform packages plus the main `@blade-ai/orca` package, with optional `npm pack` output under `tarballs/`.
- Marked the script executable.

## Files changed
- [`scripts/release/stage-npm.mjs`](/Users/qingyun/Documents/GitHub/blade-deepseek/scripts/release/stage-npm.mjs)
- [`.superpowers/sdd/task-2-report.md`](/Users/qingyun/Documents/GitHub/blade-deepseek/.superpowers/sdd/task-2-report.md)

## TDD evidence

### RED

Ran the fixture command from the brief before creating the script:

```bash
tmp="$(mktemp -d)"
for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
  mkdir -p "$tmp/artifacts/orca-$target"
  cp target/release/orca "$tmp/artifacts/orca-$target/orca"
done
node scripts/release/stage-npm.mjs --version 0.1.0 --artifacts-dir "$tmp/artifacts" --out-dir "$tmp/npm" --pack
find "$tmp/npm/tarballs" -name '*.tgz' | wc -l
```

Result:
- Node failed with `Cannot find module '/Users/qingyun/Documents/GitHub/blade-deepseek/scripts/release/stage-npm.mjs'`
- `find` reported the tarball directory missing
- Final count was `0`

### GREEN

After adding the script, reran the same fixture command.

Result:
- The script packed:
  - `@blade-ai/orca-darwin-arm64@0.1.0`
  - `@blade-ai/orca-darwin-x64@0.1.0`
  - `@blade-ai/orca-linux-arm64@0.1.0`
  - `@blade-ai/orca-linux-x64@0.1.0`
  - `@blade-ai/orca@0.1.0`
- Final tarball count was `5`

## Tests and outputs
- Ran `chmod +x scripts/release/stage-npm.mjs`
- Output: command exited 0.
- Ran `node --check scripts/release/stage-npm.mjs`
- Output: command exited 0 with no syntax errors.
- Ran the fixture staging command from the brief
- Output: `npm pack` succeeded for all five packages and `find "$tmp/npm/tarballs" -name '*.tgz' | wc -l` printed `5`.

## Self-review
- Verified the target metadata, argument parser, helper functions, staging flow, and `npm pack` behavior match the brief exactly.
- Verified the script only touches staged output under the requested `outDir` and repo inputs under `npm/`, `README.md`, and `Cargo.toml`.
- Verified the main package gets versioned optional dependencies for all four platform packages.
- Kept scope to the owned files only.

## Concerns
- The archive extraction path intentionally expects the tarball root to contain `orca` directly, matching the brief; if a later workflow nests archive contents differently, this script will need a follow-up adjustment.
- The script eagerly clears both `stage/` and `tarballs/` under the selected output directory, which matches the brief but means callers should not reuse those directories for unrelated artifacts.
