# Contributor Governance Design

Date: 2026-07-16

## Problem

Orca is an MIT-licensed public project with release automation and a substantial
Rust test suite, but it has no contributor guide, Issue forms, pull request
template, security policy, or support-routing document. External contributors
cannot tell which changes are welcome, which checks are required, or where to
report a vulnerability. Maintainers receive unstructured reports without the
environment and reproduction details needed to act on them.

## Governance Policy

Orca will use an open-collaboration policy:

- bug fixes, documentation improvements, tests, and focused features are
  welcome;
- large features and changes to architecture, public protocols, persisted
  formats, security boundaries, or dependencies must begin with an Issue;
- contributors should keep pull requests focused and include tests and contract
  documentation when observable behavior changes;
- maintainers own version bumps, release notes, tags, and package publication
  unless they explicitly request those changes in a pull request;
- security vulnerabilities must be reported privately through GitHub Security
  Advisories, never through a public Issue.

The public governance documents will be written in English to match the README,
code, and release surfaces. Existing QQ and Telegram community links remain
available for general support.

## Artifacts

### `CONTRIBUTING.md`

The contributor guide will explain:

- which contributions are welcome and which require prior discussion;
- how to fork, branch, build, and run focused tests;
- the workspace quality gate:

  ```sh
  cargo fmt --all -- --check
  cargo test --workspace --all-targets -- --test-threads=1
  cargo clippy --workspace --all-targets
  ```

- when Node.js is needed for workflow, site, or release-script changes;
- that normal tests must not require a real DeepSeek API key and secrets must
  never appear in fixtures, logs, Issues, or commits;
- the repository's Conventional Commit-style subject convention;
- pull request expectations for tests, documentation, compatibility, security,
  and user-visible TUI evidence;
- that contributors should not bump versions or publish releases unless asked.

### Issue Forms

Create three YAML Issue forms under `.github/ISSUE_TEMPLATE/`:

- `bug_report.yml` requests Orca version, installation method, operating system
  and architecture, execution mode, reproduction steps, expected and actual
  behavior, regression information, and redacted diagnostics;
- `feature_request.yml` requests the user problem, use case, proposed behavior,
  alternatives, affected surfaces, and compatibility or security concerns;
- `documentation.yml` requires the affected page or section and the problem;
  a proposed correction is optional.

All forms require a confirmation that secrets and sensitive data have been
removed. They will not auto-assign labels because repository label availability
is not defined in source control.

Add `.github/ISSUE_TEMPLATE/config.yml` with blank Issues disabled. Contact
links route vulnerabilities to the repository's GitHub Security Advisory form
and general questions to the existing Telegram community. The QQ group remains
documented in `SUPPORT.md` because GitHub contact links require a URL.

### Pull Request Template

`.github/PULL_REQUEST_TEMPLATE.md` will ask for:

- a concise summary and linked Issue where applicable;
- the problem and chosen approach;
- tests performed;
- compatibility, persistence, security, and documentation impact;
- screenshots or recordings for meaningful TUI changes;
- confirmation that formatting, tests, clippy, secret scanning by inspection,
  and scope checks are complete.

The checklist will not claim that every pull request must run release-only or
real-API tests.

### `SECURITY.md`

The security policy will state that only the latest released version receives
security fixes. Reports must use GitHub's private vulnerability reporting flow
at `/security/advisories/new` and include impact, affected versions,
reproduction details, and suggested mitigations when available.

The policy will avoid promising fixed response or remediation deadlines. It
will ask reporters to allow coordinated remediation and disclosure, and it will
explicitly reject public Issues for unresolved vulnerabilities.

### `SUPPORT.md`

The support guide will route:

- reproducible defects to the Bug Report form;
- product proposals to the Feature Request form;
- documentation defects to the Documentation form;
- usage questions to QQ or Telegram;
- vulnerabilities to GitHub Security Advisories.

It will explain what information makes a support request actionable and warn
users to redact API keys, tokens, private source, and sensitive logs.

### README Navigation

Add a short `Contributing and Support` section near the existing Community
section. It will link to the contributor guide, Issue forms, support guide, and
security policy without duplicating their contents.

### Repository Security Setting

Enable GitHub Private Vulnerability Reporting for
`echoVic/blade-deepseek`. This is required for the Security Advisory links in
the policy and Issue chooser to accept private reports. Apply the setting only
after the repository files are ready, then read the dedicated GitHub API
endpoint and require `enabled: true` as completion evidence.

## Exclusions

This change will not add:

- a CLA, DCO, or sign-off requirement;
- `CODEOWNERS` or automatic reviewer assignment;
- a code of conduct without a private conduct-reporting channel;
- new GitHub labels, branch protections, Discussions, or repository settings
  beyond enabling Private Vulnerability Reporting;
- new CI workflows or changes to runtime behavior.

These require separate maintainer and repository-setting decisions.

## Verification

1. Parse every Issue form and configuration file as YAML.
2. Check required GitHub Issue Form keys and unique field identifiers.
3. Verify that all relative Markdown links resolve in the checkout.
4. Compare documented development commands with the release workflow and
   recent release verification commands.
5. Run `cargo fmt --all -- --check` to prove the documented formatting gate is
   valid for the current checkout.
6. Review all templates for accidental requests for secrets or public security
   disclosures.
7. Enable Private Vulnerability Reporting through the dedicated GitHub API and
   verify that the endpoint returns `enabled: true`.
8. Confirm that no production source or release configuration changed.

## Acceptance Criteria

1. A first-time contributor can set up the project, choose the right Issue
   route, run the required checks, and prepare a reviewable pull request.
2. Bug reports capture version, platform, mode, reproduction, and redacted
   diagnostics.
3. Large or compatibility-sensitive changes are routed through an Issue before
   implementation.
4. Vulnerability reports are routed only to GitHub Security Advisories.
5. Support questions do not need to masquerade as Bug Reports.
6. GitHub templates are syntactically valid and contain no references to
   nonexistent repository-managed labels.
7. README exposes the governance entry points without duplicating policy text.
8. The repository accepts private vulnerability reports through GitHub Security
   Advisories.
