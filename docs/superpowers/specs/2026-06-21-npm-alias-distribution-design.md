# npm Alias Distribution Design

## Goal

Release Orca `0.1.1` using the same npm shape as current OpenAI Codex CLI: one public package name, `@blade-ai/orca`, with platform-specific binary builds published as prerelease versions of that same package.

## Current Problem

`0.1.0` published four real platform packages:

- `@blade-ai/orca-darwin-arm64`
- `@blade-ai/orca-darwin-x64`
- `@blade-ai/orca-linux-arm64`
- `@blade-ai/orca-linux-x64`

This works, but it makes the npm organization look like it owns five separate Orca packages. Codex currently avoids that by using dependency aliases: the installed dependency key is platform-specific, while the registry package name remains the main package.

## Target npm Shape

Publish these versions under the single package name `@blade-ai/orca`:

- `0.1.1`
- `0.1.1-darwin-arm64`
- `0.1.1-darwin-x64`
- `0.1.1-linux-arm64`
- `0.1.1-linux-x64`

The main package `@blade-ai/orca@0.1.1` declares optional dependencies like:

```json
{
  "@blade-ai/orca-darwin-arm64": "npm:@blade-ai/orca@0.1.1-darwin-arm64",
  "@blade-ai/orca-darwin-x64": "npm:@blade-ai/orca@0.1.1-darwin-x64",
  "@blade-ai/orca-linux-arm64": "npm:@blade-ai/orca@0.1.1-linux-arm64",
  "@blade-ai/orca-linux-x64": "npm:@blade-ai/orca@0.1.1-linux-x64"
}
```

`bin/orca.js` can keep resolving platform alias package names. npm installs each alias into `node_modules/<alias-name>`, even though the package metadata inside the tarball is `@blade-ai/orca`.

## Release Order

1. Publish platform prerelease versions first.
2. Publish the main stable version last.
3. Verify `npm install @blade-ai/orca@0.1.1` and `orca --version`.
4. After `0.1.1` is verified, unpublish the `0.1.0` versions:
   - `@blade-ai/orca@0.1.0`
   - `@blade-ai/orca-darwin-arm64@0.1.0`
   - `@blade-ai/orca-darwin-x64@0.1.0`
   - `@blade-ai/orca-linux-arm64@0.1.0`
   - `@blade-ai/orca-linux-x64@0.1.0`

Do not unpublish `@blade-ai/orca@0.1.0` before `0.1.1` exists, because deleting the last version of a package can temporarily block publishing that package name again.

## Out of Scope

- Changing binary lookup names.
- Adding Windows support.
- Rewriting the release workflow around a different package manager.
- Removing historical GitHub Release assets for `v0.1.0`.
