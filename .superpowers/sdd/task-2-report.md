# Task 2 Report: Vite React Homepage

## Implementation Summary

Implemented the `site/**` homepage scaffold from the Task 2 brief:

- Added the Vite/React/TypeScript package scaffold in `site/package.json`.
- Generated `site/package-lock.json` with `npm install --prefix site`.
- Added `index.html`, TypeScript configs, and Vite config with `base: "/blade-deepseek/"`.
- Added the React entrypoint, homepage component, and responsive stylesheet with the required `install`, `features`, and `workflow` anchors.

## Compatibility Adjustment

The brief's dependency block was copied verbatim first, but `npm --prefix site run build` failed because TypeScript could not find React type declarations.

To satisfy the brief's "use exact values verbatim unless a build tool requires a compatible adjustment" instruction, I added:

- `@types/react`
- `@types/react-dom`

under `devDependencies` so the required build would pass.

## TDD-Style Verification Notes

### Red

After creating only `site/package.json` and installing dependencies, I ran:

```bash
npm --prefix site run build
```

It failed with:

- `error TS5083: Cannot read file '/Users/qingyun/Documents/GitHub/blade-deepseek/site/tsconfig.json'.`

That confirmed the build was exercising the missing required scaffold.

### Green

After adding the required files and the minimal React type compatibility adjustment, I ran:

```bash
npm --prefix site run build
```

Observed success:

- TypeScript completed.
- Vite built `site/dist/`.
- Output included bundled HTML, CSS, and JS assets.

## Files Added

- `site/package.json`
- `site/package-lock.json`
- `site/index.html`
- `site/tsconfig.json`
- `site/tsconfig.node.json`
- `site/vite.config.ts`
- `site/src/main.tsx`
- `site/src/App.tsx`
- `site/src/styles.css`

## Self-Review

What I checked:

- The homepage content matches the brief's copy and structure.
- Navigation anchors `#install`, `#features`, and `#workflow` exist.
- The install toggle switches between the npm and curl commands and the copy button targets the active command.
- The only requirement-driven deviation from the brief is the addition of React type packages needed for a successful TypeScript build.
- The commit scope is limited to `site/**`; unrelated repository changes were left alone.

## Concerns

- None beyond the documented type-package compatibility adjustment required to make the specified build pass.

## Task 2 Review Fix

Addressed the reviewer feedback in `site/src/App.tsx`:

- Hardened the install command copy flow so it first tries `navigator.clipboard.writeText(command)`, then falls back to a hidden textarea plus `document.execCommand("copy")`, and finally shows a brief `Failed` state if both paths fail.
- Kept the button label stable and professional with `Copy`, `Copied`, and `Failed` states.
- Upgraded the npm/curl switch to proper tab semantics with `role="tab"`, `aria-selected`, stable tab ids, `aria-controls`, and a `role="tabpanel"` wrapper around the command area.

Build verification:

```bash
npm --prefix site run build
```

Relevant output:

```text
vite v7.3.5 building client environment for production...
transforming...
✓ 29 modules transformed.
rendering chunks...
computing gzip size...
dist/index.html                   0.57 kB │ gzip:  0.33 kB
dist/assets/index-DWHWFtbY.css    4.70 kB │ gzip:  1.62 kB
dist/assets/index-aGkLwGGV.js   199.62 kB │ gzip: 62.83 kB
✓ built in 341ms
```

## Task 2 Review Follow-Up

Applied the remaining reviewer fixes in `site/src/App.tsx`:

- Added keyboard navigation for the install method tabs so `ArrowLeft`, `ArrowRight`, `Home`, and `End` move focus and switch the active tab between `npm` and `curl`.
- Reset the copy status immediately when the install mode changes so the button cannot briefly show `Copied` or `Failed` for the wrong command.

Build verification:

```bash
npm --prefix site run build
```

Relevant output:

```text
vite v7.3.5 building client environment for production...
transforming...
✓ 29 modules transformed.
rendering chunks...
computing gzip size...
dist/index.html                   0.57 kB │ gzip:  0.33 kB
dist/assets/index-DWHWFtbY.css    4.70 kB │ gzip:  1.62 kB
dist/assets/index-DvWWfkNA.js   200.03 kB │ gzip: 63.01 kB
✓ built in 289ms
```
