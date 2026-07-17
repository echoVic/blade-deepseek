# Contributing to Orca

Thank you for contributing to Orca. Bug fixes, documentation improvements, tests,
and focused features are welcome.

## Before You Start

Open an Issue before working on a large feature or a change to the architecture,
public protocol, persisted format, security boundary, or dependencies. This lets
maintainers confirm the direction and avoid duplicated work.

Do not report vulnerabilities in a public Issue. Follow [SECURITY.md](SECURITY.md)
to report them privately.

## Development Setup

1. Fork and clone the repository.
2. Create a branch from `main`.
3. Install stable Rust and [ripgrep](https://github.com/BurntSushi/ripgrep).
4. Build the workspace:

   ```sh
   cargo build --workspace
   ```

Node.js is needed only for workflows, site tooling, or release scripts. When it
is needed, use the Node.js version declared in the relevant GitHub workflow.

Normal tests must not require a real `DEEPSEEK_API_KEY`. Never commit API keys,
tokens, private source code, or sensitive logs.

## Change Guidelines

- Follow existing module and contract boundaries.
- Keep pull requests focused on one concern.
- Add or update tests for behavioral changes.
- Update contracts and documentation when behavior or interfaces change.
- Include screenshots for visible TUI changes.
- Do not change versions, create releases or tags, or publish artifacts unless a
  maintainer explicitly requests it.

## Verification

Run the full contribution gate before submitting a pull request:

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets -- --test-threads=1
cargo clippy --workspace --all-targets
```

If credentials, a platform limitation, or an external service prevents the full
gate from running, run the largest relevant subset and disclose in the pull
request exactly what was not run and why.

## Commits

Use concise Conventional Commit-style messages. Examples:

```text
fix(runtime): preserve event publication order
docs: clarify server protocol
test(tools): cover sandbox denial
```

## Pull Requests

Describe the problem and solution, link any relevant Issue when one exists
(especially for changes that required prior discussion), list verification
performed, and call out compatibility, security, or migration risks. Keep review
feedback in scope and ensure the branch is ready for maintainers to reproduce.

By contributing, you agree that your contribution is licensed under the
[MIT License](LICENSE).

## Release Process

See [docs/release-process.md](docs/release-process.md) for the full release
checklist, including version bumps, site updates, tagging, and post-publish
verification.
