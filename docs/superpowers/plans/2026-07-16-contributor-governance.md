# Contributor Governance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a complete, actionable contribution and Issue-routing surface for Orca without changing runtime behavior or release automation.

**Architecture:** Keep policy in root Markdown files and GitHub interaction forms under `.github/`. README provides only navigation, while `CONTRIBUTING.md`, `SUPPORT.md`, and `SECURITY.md` own their respective policies. Validate Issue Forms as structured YAML and stage only governance files so concurrent Linux sandbox work remains untouched.

**Tech Stack:** Markdown, GitHub Issue Forms YAML, GitHub pull request templates, Rust workspace verification commands

---

## File Map

- Create `CONTRIBUTING.md`: contribution scope, setup, quality gates, commit and pull request expectations.
- Create `SUPPORT.md`: route bugs, features, docs, questions, and vulnerabilities.
- Create `SECURITY.md`: supported-version and private-reporting policy.
- Create `.github/ISSUE_TEMPLATE/bug_report.yml`: structured defect intake.
- Create `.github/ISSUE_TEMPLATE/feature_request.yml`: structured proposal intake.
- Create `.github/ISSUE_TEMPLATE/documentation.yml`: structured documentation intake.
- Create `.github/ISSUE_TEMPLATE/config.yml`: disable blank Issues and expose private security/community links.
- Create `.github/PULL_REQUEST_TEMPLATE.md`: review checklist and compatibility disclosure.
- Modify `README.md`: add governance navigation beside Community.
- Create `docs/superpowers/plans/2026-07-16-contributor-governance.md`: preserve this implementation plan.
- Update repository setting: enable GitHub Private Vulnerability Reporting.

### Task 1: Contributor, Support, And Security Policies

**Files:**
- Create: `CONTRIBUTING.md`
- Create: `SUPPORT.md`
- Create: `SECURITY.md`

- [ ] **Step 1: Create the contributor guide**

Write `CONTRIBUTING.md` with these sections and rules:

```markdown
# Contributing to Orca

Thanks for helping improve Orca. Bug fixes, documentation improvements, tests,
and focused features are welcome.

## Before You Start

Open an Issue before investing in a large feature or changing architecture,
public protocols, persisted formats, security boundaries, or dependencies.
Small bug fixes, tests, and documentation corrections can go directly to a
pull request.

For security vulnerabilities, follow [SECURITY.md](SECURITY.md). Do not open a
public Issue.

## Development Setup

1. Fork and clone the repository.
2. Create a focused branch from `main`.
3. Install the stable Rust toolchain and `ripgrep`.
4. Build the workspace with `cargo build --workspace`.

Node.js is required only when changing workflows, the website, or release
scripts. Use the version selected by the relevant GitHub workflow.

Normal tests must not require a real `DEEPSEEK_API_KEY`. Never commit API keys,
tokens, private source code, or unredacted sensitive logs.

## Making Changes

- Follow the existing module and contract boundaries.
- Keep pull requests focused; separate unrelated refactors.
- Add focused tests for behavior changes and regression fixes.
- Update public contracts and user documentation when observable behavior
  changes.
- Add screenshots or recordings for meaningful TUI changes.
- Do not bump versions, add release notes, create tags, or publish packages
  unless a maintainer asks you to do so.

## Verification

Run focused tests while developing. Before requesting review, run:

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets -- --test-threads=1
cargo clippy --workspace --all-targets
```

If a full check depends on unavailable credentials, platform features, or
external services, run the largest relevant local subset and explain the gap in
the pull request.

## Commits

Use short, imperative subjects following the repository's Conventional
Commit-style convention, for example:

```text
fix(runtime): preserve event publication order
docs: clarify server protocol
test(tools): cover sandbox denial
```

## Pull Requests

- Explain the problem and the chosen approach.
- Link the relevant Issue when prior discussion is required.
- List the exact verification commands and results.
- Call out public protocol, persistence, security, dependency, and
  documentation impact.
- Respond to review with focused follow-up commits or a clearly explained
  revision.

By contributing, you agree that your contributions are licensed under the
project's [MIT License](LICENSE).
```

- [ ] **Step 2: Create support routing**

Write `SUPPORT.md`:

```markdown
# Support

Choose the channel that matches your request so it reaches the right context.

## Bugs

Use the [Bug Report form](https://github.com/echoVic/blade-deepseek/issues/new?template=bug_report.yml)
for reproducible defects. Include the Orca version, installation method,
platform, execution mode, reproduction steps, and redacted diagnostics.

## Feature Requests

Use the [Feature Request form](https://github.com/echoVic/blade-deepseek/issues/new?template=feature_request.yml)
to describe the user problem and desired outcome before proposing an
implementation.

## Documentation

Use the [Documentation form](https://github.com/echoVic/blade-deepseek/issues/new?template=documentation.yml)
for missing, incorrect, or unclear documentation.

## Usage Questions

For setup and usage questions, use the community channels:

- QQ group: `472309526`
- Telegram: <https://t.me/+11No1w5ZbTMyZTQ1>

## Security

Report vulnerabilities privately through [GitHub Security
Advisories](https://github.com/echoVic/blade-deepseek/security/advisories/new).
Do not disclose an unresolved vulnerability in a public Issue or discussion.

Always remove API keys, access tokens, private source code, personal data, and
other sensitive information from reports and logs.
```

- [ ] **Step 3: Create the security policy**

Write `SECURITY.md`:

```markdown
# Security Policy

## Supported Versions

Security fixes are provided for the latest released version of Orca. Before
reporting a vulnerability, confirm whether it still affects the latest release.

| Version | Supported |
| --- | --- |
| Latest release | Yes |
| Older releases | No |

## Reporting A Vulnerability

Report suspected vulnerabilities privately with [GitHub Security
Advisories](https://github.com/echoVic/blade-deepseek/security/advisories/new).
Do not open a public Issue for an unresolved vulnerability.

Include, when available:

- the affected version and platform;
- the security impact and realistic attack scenario;
- minimal reproduction steps or a proof of concept;
- affected files, commands, or protocol surfaces;
- possible mitigations or fixes;
- whether the issue has been disclosed elsewhere.

Remove API keys, access tokens, personal data, and unrelated private source from
the report. Maintainers will coordinate validation, remediation, and disclosure
through the private advisory. Please allow time for a fix before public
disclosure.
```

- [ ] **Step 4: Check policy links and whitespace**

Run:

```sh
git diff --check -- CONTRIBUTING.md SUPPORT.md SECURITY.md
test -f LICENSE
rg -n 'SECURITY.md|MIT License|cargo fmt|cargo test|cargo clippy' CONTRIBUTING.md
rg -n 'security/advisories/new|Bug Report|Feature Request|Documentation' SUPPORT.md SECURITY.md
```

Expected: all commands exit `0`; `git diff --check` prints nothing.

- [ ] **Step 5: Commit the policy documents**

```sh
git add CONTRIBUTING.md SUPPORT.md SECURITY.md
git commit -m "docs: add contribution and security policies"
```

### Task 2: Structured Issue Intake

**Files:**
- Create: `.github/ISSUE_TEMPLATE/bug_report.yml`
- Create: `.github/ISSUE_TEMPLATE/feature_request.yml`
- Create: `.github/ISSUE_TEMPLATE/documentation.yml`
- Create: `.github/ISSUE_TEMPLATE/config.yml`

- [ ] **Step 1: Create the Bug Report form**

Create `.github/ISSUE_TEMPLATE/bug_report.yml` with `name`, `description`,
`title`, and a `body` containing:

- a Markdown warning to use Security Advisories for vulnerabilities;
- required `version`, `install_method`, `platform`, `mode`, `steps`,
  `expected`, and `actual` fields;
- optional `regression` and `logs` fields;
- a required `terms` checkbox confirming that secrets and sensitive data were
  removed.

Use `dropdown` controls for installation method and execution mode, `input` for
version and platform, and `textarea` for reproduction and diagnostics. Do not
set labels or assignees.

- [ ] **Step 2: Create the Feature Request form**

Create `.github/ISSUE_TEMPLATE/feature_request.yml` with:

- required `problem`, `use_case`, and `proposal` textareas;
- optional `alternatives` textarea;
- an `affected_surfaces` checkbox group containing TUI, `orca exec`, embedded
  server, tools/MCP, subagents/workflows, history/persistence, and other;
- optional `compatibility` textarea for protocol, persistence, security, and
  dependency impact;
- a required `terms` checkbox confirming sensitive data was removed and large
  compatibility-sensitive changes will wait for maintainer alignment.

Do not set labels or assignees.

- [ ] **Step 3: Create the Documentation form**

Create `.github/ISSUE_TEMPLATE/documentation.yml` with required `location`,
`problem`, and `suggested_change` fields, optional `additional_context`, and a
required `terms` checkbox confirming sensitive data was removed.

Do not set labels or assignees.

- [ ] **Step 4: Configure the Issue chooser**

Create `.github/ISSUE_TEMPLATE/config.yml`:

```yaml
blank_issues_enabled: false
contact_links:
  - name: Report a security vulnerability
    url: https://github.com/echoVic/blade-deepseek/security/advisories/new
    about: Report security vulnerabilities privately. Do not open a public issue.
  - name: Ask a usage question
    url: https://t.me/+11No1w5ZbTMyZTQ1
    about: Ask setup and usage questions in the Orca community.
```

- [ ] **Step 5: Validate the YAML structure**

Run:

```sh
ruby -e 'require "yaml"; Dir[".github/ISSUE_TEMPLATE/*.{yml,yaml}"].sort.each { |path| doc = YAML.safe_load(File.read(path), [], [], false); abort("#{path}: expected mapping") unless doc.is_a?(Hash); next if File.basename(path) == "config.yml"; %w[name description body].each { |key| abort("#{path}: missing #{key}") unless doc[key] }; ids = doc["body"].map { |item| item["id"] }.compact; abort("#{path}: duplicate ids") unless ids == ids.uniq; puts "ok #{path}" }'
```

Expected: one `ok` line for each of the three Issue Forms and exit code `0`.

- [ ] **Step 6: Check form safety and commit**

Run:

```sh
rg -n 'Security Advisories|secrets|sensitive' .github/ISSUE_TEMPLATE
git diff --check -- .github/ISSUE_TEMPLATE
```

Expected: each form includes a safety confirmation; whitespace check exits `0`.

Commit:

```sh
git add .github/ISSUE_TEMPLATE
git commit -m "docs: add structured issue forms"
```

### Task 3: Pull Request Template And README Navigation

**Files:**
- Create: `.github/PULL_REQUEST_TEMPLATE.md`
- Modify: `README.md`

- [ ] **Step 1: Create the pull request template**

Write `.github/PULL_REQUEST_TEMPLATE.md`:

```markdown
## Summary

<!-- What problem does this pull request solve? -->

## Approach

<!-- Explain the chosen approach and important trade-offs. -->

## Related Issue

<!-- Link the Issue when prior discussion is required, for example: Closes #123. -->

## Verification

<!-- List the exact commands run and their results. Explain any test gap. -->

## Impact

- Public protocol or CLI compatibility:
- Persisted data or migration:
- Security or permissions:
- Dependencies:
- Documentation:

## User Interface

<!-- Add screenshots or recordings for meaningful TUI changes, or write "Not applicable." -->

## Checklist

- [ ] The change is focused and excludes unrelated refactors.
- [ ] Tests cover new behavior or the reported regression.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] Relevant tests pass; the full workspace gate was run or the gap is explained.
- [ ] `cargo clippy --workspace --all-targets` passes, or the gap is explained.
- [ ] Public behavior and compatibility impact are documented.
- [ ] No API keys, tokens, private source, or sensitive logs are included.
- [ ] Version numbers and release artifacts are unchanged unless requested.
```

- [ ] **Step 2: Add README governance navigation**

Immediately after the existing `## Community` list in `README.md`, add:

```markdown
## Contributing and Support

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) before
starting a large or compatibility-sensitive change.

- Report reproducible defects with the [Bug Report form](https://github.com/echoVic/blade-deepseek/issues/new?template=bug_report.yml).
- Propose product changes with the [Feature Request form](https://github.com/echoVic/blade-deepseek/issues/new?template=feature_request.yml).
- Ask usage questions through the channels in [SUPPORT.md](SUPPORT.md).
- Report vulnerabilities privately according to [SECURITY.md](SECURITY.md).
```

- [ ] **Step 3: Check Markdown links and content**

Run:

```sh
test -f CONTRIBUTING.md
test -f SUPPORT.md
test -f SECURITY.md
test -f .github/PULL_REQUEST_TEMPLATE.md
rg -n 'CONTRIBUTING.md|SUPPORT.md|SECURITY.md|bug_report.yml|feature_request.yml' README.md
git diff --check -- README.md .github/PULL_REQUEST_TEMPLATE.md
```

Expected: all commands exit `0`; whitespace check prints nothing.

- [ ] **Step 4: Commit the PR template and navigation**

```sh
git add README.md .github/PULL_REQUEST_TEMPLATE.md docs/superpowers/plans/2026-07-16-contributor-governance.md
git commit -m "docs: expose contribution and support entry points"
```

### Task 4: Enable Private Vulnerability Reporting

**External state:**
- Update: GitHub repository setting for `echoVic/blade-deepseek`

- [ ] **Step 1: Confirm the current setting**

Run:

```sh
gh api repos/echoVic/blade-deepseek/private-vulnerability-reporting
```

Expected before enablement: `{"enabled":false}`.

- [ ] **Step 2: Enable private vulnerability reporting**

Run:

```sh
gh api \
  --method PUT \
  -H "Accept: application/vnd.github+json" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  repos/echoVic/blade-deepseek/private-vulnerability-reporting
```

Expected: HTTP success with no response body.

- [ ] **Step 3: Verify the repository setting**

Run:

```sh
gh api repos/echoVic/blade-deepseek/private-vulnerability-reporting --jq '.enabled'
```

Expected: `true`.

### Task 5: Final Governance Verification

**Files:**
- Verify: `CONTRIBUTING.md`
- Verify: `SUPPORT.md`
- Verify: `SECURITY.md`
- Verify: `.github/ISSUE_TEMPLATE/*.yml`
- Verify: `.github/PULL_REQUEST_TEMPLATE.md`
- Verify: `README.md`
- Verify: `docs/superpowers/plans/2026-07-16-contributor-governance.md`

- [ ] **Step 1: Verify repository shape**

Run:

```sh
test -f CONTRIBUTING.md
test -f SUPPORT.md
test -f SECURITY.md
test -f .github/PULL_REQUEST_TEMPLATE.md
test "$(find .github/ISSUE_TEMPLATE -maxdepth 1 -type f | wc -l | tr -d ' ')" = "4"
```

Expected: exit code `0`.

- [ ] **Step 2: Re-run structured and whitespace validation**

Run:

```sh
ruby -e 'require "yaml"; Dir[".github/ISSUE_TEMPLATE/*.{yml,yaml}"].sort.each { |path| doc = YAML.safe_load(File.read(path), [], [], false); abort("#{path}: invalid") unless doc.is_a?(Hash); next if File.basename(path) == "config.yml"; abort("#{path}: empty body") unless doc["body"].is_a?(Array) && !doc["body"].empty?; ids = doc["body"].map { |item| item["id"] }.compact; abort("#{path}: duplicate ids") unless ids == ids.uniq; puts "ok #{path}" }'
git diff --check
```

Expected: three `ok` lines, then no whitespace errors.

- [ ] **Step 3: Verify the documented Rust formatting gate**

Run:

```sh
cargo fmt --all -- --check
```

Expected: exit code `0`. If concurrent non-governance Rust changes fail this
check, report their paths without modifying or staging them.

- [ ] **Step 4: Audit security routing**

Run:

```sh
rg -n 'security/advisories/new' README.md SUPPORT.md SECURITY.md .github/ISSUE_TEMPLATE
rg -n 'DEEPSEEK_API_KEY|API keys|tokens|sensitive' CONTRIBUTING.md SUPPORT.md SECURITY.md .github/ISSUE_TEMPLATE .github/PULL_REQUEST_TEMPLATE.md
```

Expected: private Advisory links appear in security routing; secret-handling
warnings appear in contributor-facing surfaces.

- [ ] **Step 5: Confirm private reporting remains enabled**

Run:

```sh
gh api repos/echoVic/blade-deepseek/private-vulnerability-reporting --jq '.enabled'
```

Expected: `true`.

- [ ] **Step 6: Inspect the final scoped diff**

Run:

```sh
git status --short
git diff -- CONTRIBUTING.md SUPPORT.md SECURITY.md README.md .github/ISSUE_TEMPLATE .github/PULL_REQUEST_TEMPLATE.md docs/superpowers/plans/2026-07-16-contributor-governance.md
```

Expected: governance files contain only the approved English policy and
templates. Existing Linux sandbox changes remain untouched and unstaged unless
their owner has committed them independently.
