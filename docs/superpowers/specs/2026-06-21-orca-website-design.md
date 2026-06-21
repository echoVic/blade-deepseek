# Orca Website Design

Date: 2026-06-21

## Goal

Build the first Orca website as a focused product homepage with clear install paths.
The page should help a developer understand Orca quickly, install it with npm or curl,
and jump to GitHub/npm for deeper inspection.

This is not a full documentation site yet. The first version should be polished,
fast, and easy to maintain, while leaving room for docs pages later.

## Audience

- Developers evaluating a local coding agent CLI.
- DeepSeek users who want a native tool-use/runtime experience.
- Existing Orca users who need install commands and release links.

## Positioning

Orca is a DeepSeek-native coding agent runtime by Blade.

The homepage should present Orca as a serious developer tool: terminal-first,
practical, and capable. The visual tone should be hard-edged and technical,
with enough modern AI-product polish to feel credible outside the README.

## Technical Approach

Create a Vite + React site under `site/`.

Reasons:

- GitHub Pages can deploy the static build output.
- React keeps the homepage maintainable as interactive sections grow.
- Vite keeps the setup lightweight and fast.
- Plain CSS avoids committing to a UI framework before the design system exists.

The build output will be static HTML/CSS/JS. No server runtime is needed.

## Deployment

Use GitHub Pages for the first version.

The project should include a Pages workflow that builds `site/` and deploys the
generated static assets. The workflow should run on pushes to `main` when site
files change, and it should also support manual dispatch.

## Install Strategy

The homepage must show two primary installation methods.

### npm

```sh
npm install -g @blade-ai/orca
orca --version
```

This is the best path for users who already have Node.js/npm installed.

### curl

```sh
curl -fsSL https://raw.githubusercontent.com/echoVic/blade-deepseek/main/install.sh | sh
```

The curl path should install the native binary directly from GitHub Releases.
It is the best path for users who do not want Node.js.

`install.sh` should:

- Detect OS: macOS or Linux.
- Detect CPU architecture: arm64 or x64.
- Map platform to the current release asset names:
  - `orca-aarch64-apple-darwin.tar.gz`
  - `orca-x86_64-apple-darwin.tar.gz`
  - `orca-aarch64-unknown-linux-gnu.tar.gz`
  - `orca-x86_64-unknown-linux-gnu.tar.gz`
- Download the latest release by default.
- Support a pinned version through `ORCA_VERSION`.
- Install to `~/.local/bin` by default.
- Support custom install locations through `INSTALL_DIR`.
- Verify the downloaded archive with the published `.sha256` file.
- Print a clear next step if the install directory is not on `PATH`.
- Run `orca --version` after installation when possible.

## Homepage Structure

### Header

- Orca wordmark.
- Navigation anchors: Install, Features, Workflow, GitHub.
- Primary action: GitHub.

### Hero

The first viewport should make the product obvious immediately.

Content:

- Headline: "Orca"
- Subheadline: "A DeepSeek-native coding agent runtime by Blade."
- Short value statement focused on local CLI execution, workflows, subagents,
  approvals, verification, and resumable history.
- Install command block with npm/curl tabs.
- Buttons linking to GitHub and npm.
- A terminal-style product preview showing a realistic Orca command and output.

The hero should not be a marketing card layout. It should feel like a sharp
developer tool surface, with code and command output as first-class visual
material.

### Feature Band

Show a compact grid of the most important capabilities:

- DeepSeek-native reasoning and streaming.
- Orca project and user workflows.
- Subagents for delegated tasks.
- Approval modes for safe edits.
- Verification commands after runs.
- History, resume, fork, and local transcripts.

Each item should be concise and practical, avoiding vague AI language.

### Workflow Section

Explain the workflow feature with a short code sample:

```js
export const meta = {
  name: "audit",
  description: "Audit code",
  phases: ["scan"]
};
```

Pair this with a command:

```sh
orca workflow run audit
```

The section should communicate that workflows are executable, local, and
project/user scoped.

### Install Section

Repeat both install methods lower on the page for users who scroll past the hero.

Include:

- npm install command.
- curl install command.
- Supported platforms.
- Link to GitHub Releases.

### Footer

Include links to:

- GitHub repository.
- npm package.
- Latest GitHub Release.
- License.

## Visual Direction

The site should feel like a hard-core developer tool with restrained product polish.

Guidelines:

- Prefer a dark technical palette with high contrast, but avoid a flat one-hue
  blue/slate look.
- Use code blocks, terminal output, crisp dividers, and compact feature surfaces.
- Avoid oversized marketing cards and decorative blobs.
- Keep layout dense enough for developers, but still readable on mobile.
- Use subtle motion only where it clarifies interactivity, such as install tabs
  or copy feedback.

## Interactions

- Install command tabs switch between npm and curl.
- Copy buttons copy the selected install command.
- External links open normally and should be obvious.
- The page must work without any network calls at runtime.

## Assets

First version can use CSS-driven interface visuals and terminal previews.
No logo asset is required yet. If a logo is added later, it should be simple and
usable at small sizes.

## Testing

Before shipping:

- Run the site build.
- Verify the GitHub Pages workflow YAML parses.
- Use a browser to inspect desktop and mobile viewports.
- Confirm install command copy buttons work.
- Confirm all external links resolve.
- Confirm the page has no overlapping text or broken responsive layout.

## Out of Scope

- Full documentation site.
- Blog.
- Search.
- User accounts.
- Hosted API.
- Download mirror outside GitHub Releases.
- Homebrew formula.
