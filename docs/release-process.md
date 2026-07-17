# Release Process

## Checklist

Every release must complete these steps in order.

### 1. Code changes

Make and commit all code changes on `main`.

### 2. Bump versions

Update the version in **all three places** — they must match:

- `Cargo.toml` — `version = "x.y.z"`
- `npm/orca/package.json` — `"version": "x.y.z"`
- `Cargo.lock` — updated automatically by `cargo build` or `cargo check`

Commit all three together:

```sh
cargo check  # updates Cargo.lock
git add Cargo.toml npm/orca/package.json Cargo.lock
git commit -m "release: prepare vX.Y.Z"
```

### 3. Write release notes

Create `docs/releases/vX.Y.Z.md` following the format of an existing release note.
Include: summary sentence, Changes, Compatibility, Verification commands, Upgrade commands.

```sh
git add docs/releases/vX.Y.Z.md
git commit -m "docs: add vX.Y.Z release notes"  # or include in step 2 commit
```

### 4. Run pre-release checks

```sh
cargo fmt -- --check
cargo test -p orca-tools sandbox -- --nocapture
cargo clippy --workspace --all-targets
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
```

### 5. Update the website

Edit `site/src/shared.ts`:
- Set `releaseVersion` to `"vX.Y.Z"`
- Add a new entry at the top of the `releases` array

Edit `site/src/changelog/Changelog.tsx`:
- Add a `"vX.Y.Z": "..."` entry at the top of **both** the English and Chinese `summaries` objects

Verify the site builds:

```sh
npm --prefix site run build
npm --prefix site run check:seo
```

Commit:

```sh
git add site/src/shared.ts site/src/changelog/Changelog.tsx
git commit -m "chore(site): add vX.Y.Z to changelog and release list"
```

### 6. Tag and push

```sh
git push origin main
git tag vX.Y.Z
git push origin vX.Y.Z
```

**Never `--force` push `main` or an existing tag.** If a tag already exists at the wrong commit, delete it locally and remotely before re-tagging:

```sh
git tag -d vX.Y.Z
git push origin :refs/tags/vX.Y.Z
git tag vX.Y.Z
git push origin vX.Y.Z
```

### 7. CI publishes automatically

The `release.yml` workflow triggers on the tag push and:
1. Runs tests
2. Builds binaries for all four targets
3. Creates a GitHub Release with binary assets
4. Stages, smoke-tests, and publishes npm packages

Monitor progress:

```sh
gh run list --repo echoVic/blade-deepseek --limit 5
```

### 8. Post-publish verification

```sh
node scripts/release/verify-published.mjs \
  --version X.Y.Z \
  --repo echoVic/blade-deepseek \
  --package @blade-ai/orca \
  --bin orca
```

## Common mistakes

| Mistake | Fix |
|---|---|
| Forgot `Cargo.lock` | `cargo check && git add Cargo.lock && git commit --amend` |
| Forgot site update | Push a follow-up commit to `site/src/` — pages workflow re-deploys automatically |
| Force-pushed `main` | Restore with `git push origin <last-good-sha>:refs/heads/main --force` |
| Tag points to wrong commit | Delete and re-create the tag (see step 6) |
| `summaries` missing new version in Changelog.tsx | TypeScript build fails — add entry to both EN and ZH summaries objects |
