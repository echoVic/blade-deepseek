## Summary

<!-- Briefly describe what this pull request changes and why. -->

## Approach

<!-- Explain the implementation approach and any important tradeoffs. -->

## Related Issue

<!-- Write `Closes #...` or `Not required`. -->

## Verification

<!-- List the exact commands run and their results. Call out any checks not run or remaining gaps. -->

## Impact

- Public protocol/CLI:
- Persisted data/migration:
- Security/permissions:
- Dependencies:
- Documentation:

## User Interface

<!-- Add screenshots or recordings for user interface changes, or write "Not applicable." -->

## Checklist

- [ ] The change is focused and contains no unrelated refactors.
- [ ] Tests cover the change where appropriate.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo test --workspace --all-targets -- --test-threads=1` passes.
- [ ] If credentials, platform limits, or external services block the full test gate, the Verification section lists the largest relevant test subset run and explains the blocker.
- [ ] `cargo clippy --workspace --all-targets` passes.
- [ ] If credentials, platform limits, or external services block the full clippy gate, the Verification section lists the largest relevant subset run and explains the blocker.
- [ ] Public behavior changes are reflected in documentation.
- [ ] No secrets or sensitive data are included.
- [ ] No version or release artifacts are included unless explicitly requested by a maintainer.
