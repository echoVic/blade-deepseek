# ADR-0002: Unified Atomic Mention System

## Status

Accepted on 2026-07-14 as the completion layer above ADR-0001's streaming file-search engine.

## Context

ADR-0001 moved file discovery and fuzzy matching off the TUI input path, introduced an owned
`SearchSession`, and validated bounded streaming behavior up to one million paths. That solved the
search-latency and worker-lifecycle problem, but the first implementation still had three product
gaps:

1. one search session represented only one workspace root;
2. file search was available only through the TUI;
3. visible `@name` text was treated as enough identity to resolve a selection at submission time.

The third gap becomes unsafe as soon as multiple roots or multiple candidate kinds are present.
Two roots may both contain `src/config.ts`; a Skill and Plugin may share a display name; two MCP
servers may expose Resources with the same name. Re-resolving visible text at submit time can
silently attach the wrong object even though the user selected the correct row.

Orca also had separate interaction paths for files (`@file`), Skills (`$skill`), Plugins, and MCP
Resources. A complete Mention system needs one candidate protocol while allowing each source to
keep the discovery strategy appropriate for its scale.

## Decision

### Multi-root file identity

`orca-file-search::SearchSession::start_roots` accepts one or more workspace roots. Roots are
canonicalized and deduplicated before discovery. Every `SearchMatch` carries both the canonical
owning `root` and the root-relative `path`; `root + path` is the file identity.

Browse and fuzzy modes scan all roots. Equal relative paths from different roots remain separate
candidates, and overlapping roots retain one identity per root traversal rather than assigning a
path to whichever root happens to be deepest. Exclude globs, Git-ignore behavior, result limits,
cancellation, progress, and snapshot semantics apply to the whole multi-root session.

### Two app-server search contracts

The Codex-compatible file-only contract remains available:

- `fuzzyFileSearch/sessionStart`
- `fuzzyFileSearch/sessionUpdate`
- `fuzzyFileSearch/sessionStop`

It accepts explicit roots and streams file results with root identity. Existing clients can use it
without understanding Orca's richer Mention model.

Orca additionally exposes a thread-bound unified contract:

- `mention/search/start`
- `mention/search/update`
- `mention/search/stop`

The start request names a live `threadId`. The server takes workspace roots and the MCP registry
from that thread rather than accepting an unrelated client-supplied context. This guarantees that
a candidate discovered by the Mention session is validated and expanded against the same runtime
boundary when it is later submitted.

Both contracts use owned `SearchSession` workers, bounded wakeups, latest-wins query state,
replaceable snapshots, stop guards, and asynchronous reaping. A manager retains every reaper
handle created by an individual stop; `stop_all` and `Drop` join those handles before the server
returns. Stopping a session prevents late relay output from being written after the stopped event.

### Unified candidates, source-specific discovery

The shared candidate model is:

```rust
pub struct MentionCandidate {
    pub id: String,
    pub kind: MentionKind,
    pub display: String,
    pub description: String,
    pub score: u32,
    pub indices: Vec<u32>,
    pub target: MentionTarget,
}
```

`MentionKind` is `file`, `skill`, `plugin`, or `resource`.

Files remain streaming because their catalog may contain millions of paths. Skills, Plugins, MCP
Resources, and Resource Templates are discovered into a smaller `MentionCatalog` and fuzzy-scored
with Nucleo atoms. TUI and app-server projections merge static candidates with the current file
snapshot, deduplicate by stable id, and apply one result limit.

Skill discovery uses the existing environment-aware Skill loader for every workspace root. Plugin
discovery checks workspace `.orca/plugins`, workspace `.codex/plugins`, and `$ORCA_HOME/plugins`
for `.codex-plugin/plugin.json`. MCP discovery calls `resources/list` and
`resources/templates/list` on the session registry and preserves partial errors. TUI MCP
initialization and this catalog discovery run on background workers; the first frame does not wait
for a configured server's connect or resource listing call. Catalog results return through a
generation-tagged event and are ignored if roots or the active search have already changed.

### Atomic binding

Selecting a final candidate inserts natural visible text into the composer and records a hidden
`MentionBinding`:

```rust
pub struct MentionBinding {
    pub start: usize,
    pub end: usize,
    pub visible: String,
    pub target: MentionTarget,
}
```

`MentionTarget` stores the real identity:

- File: canonical root, relative path, file/directory kind;
- Skill: Skill id and `SKILL.md` path;
- Plugin: manifest name and manifest path;
- Resource: MCP server and URI;
- Resource Template: MCP server and URI template.

Stable ids are opaque hashes derived from the fully typed target, never from display text alone.
Each component is length-delimited before SHA-256 hashing, so path, server, and URI separators
cannot create an alias. Selecting a directory keeps browse mode open and does not create an
attachable binding.

### Editing and stale bindings

Bindings are reconciled whenever composer text changes:

- an edit entirely before a binding rebases its byte range;
- an edit entirely after a binding leaves it unchanged;
- an edit overlapping the binding invalidates it;
- a binding whose visible slice no longer matches is discarded.

Submission therefore cannot attach a previously selected object after the user has edited its
visible mention into a different value.

### Expansion and validation

TUI submission uses `SubmitWithMentions { prompt, bindings }`. App-server `turn/start` accepts
structured `{ "type": "mention", "name": ..., "target": ... }` input and constructs the same
binding model.

Before the prompt enters model history, the runtime expands each bound target and revalidates its
current state:

- bound files must still resolve inside the bound active workspace root;
- Skills must still match both id and canonical path discovered from an active root;
- Plugin manifests must remain inside configured plugin roots and retain the bound manifest name;
- MCP Resources are read through the same registry used for discovery;
- Resource Templates are injected as typed descriptors.

The visible prompt is preserved and expanded context is appended in typed blocks. Duplicate targets
are injected once.

### Compatibility

Unbound legacy `@file` mentions continue to use the existing workspace-safe parser and expansion
rules. Explicit `$skill` injection remains supported. Structured clients and TUI selections gain
atomic identity without breaking shell scripts, historical prompts, or older clients.

## Required invariants

1. Equal display text never collapses candidates with different targets.
2. A bound file expands from its selected root, not from cwd or candidate order.
3. A candidate found through a thread-bound MCP registry is read through that same registry.
4. Text edits cannot leave a binding pointing at content different from its visible slice.
5. TUI and app-server submissions use the same target expansion and workspace validation code.
6. File-only app-server compatibility remains independent from the unified Mention protocol.
7. Search stop and server shutdown retain owned cancellation and join responsibility.
8. Legacy unbound `@file` and `$skill` inputs remain valid.

## Verification contract

Automated tests cover:

- multi-root browse and fuzzy results with stable root identity;
- overlapping-root traversal and component-separator stable-id collision regression cases;
- exclude and `respectGitignore` options;
- Codex-compatible streaming file-search start/update/stop events;
- unified app-server file/Skill/Plugin candidates and distinct stable ids;
- binding rebase before edits and invalidation on overlapping edits;
- exact-root expansion when two roots contain the same relative path;
- same-name Skill/Plugin expansion selected by target rather than display text;
- MCP Resource discovery and `resources/read` expansion through one registry;
- TUI file selection, directory browsing, and mention-aware submit actions;
- structured app-server mention input and a two-turn model-history echo proving exact expansion;
- server shutdown waiting for retained search reaper ownership;
- legacy file and Skill compatibility.

Final release verification includes formatting, diff checks, workspace checking, the full Rust test
suite with one test thread, npm staging/verifier tests, site build and SEO checks, GitHub Release
verification, npm registry verification, and a clean installed-binary smoke test.

## Consequences

Mention discovery is now a runtime capability rather than a TUI-only file picker. Adding future
candidate types requires implementing discovery, a stable target, serialization, validation, and
expansion without changing the composer contract.

The cost is more state than plain text: clients that want atomic behavior must preserve the target
selected by the user. This complexity is intentional. Plain display names are not sufficient
identity in a multi-root, plugin-enabled, MCP-enabled Agent runtime.
