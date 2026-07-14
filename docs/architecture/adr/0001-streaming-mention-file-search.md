# ADR-0001: Session-Scoped Streaming Mention File Search

## Status

Accepted on 2026-07-13 after design grilling against the local Claude Code and Codex reference
implementations.

Extended on 2026-07-14 by
[ADR-0002](0002-unified-atomic-mention-system.md), which adds canonical multi-root identity,
file-only and unified app-server sessions, files/Skills/Plugins/MCP Resources candidate merging,
and atomic submission bindings. This ADR remains authoritative for the underlying file-search
performance and lifecycle contract.

## Context

Orca currently resolves non-empty fuzzy `@` queries synchronously in
`crates/orca-runtime/src/mentions.rs`. The first query builds a workspace index with two
`git ls-files` commands or an `ignore` walk, caps the index at 100,000 files, clones the cached
path vector, and scores the whole snapshot before returning to the TUI. The cache is rebuilt
synchronously when its five-second freshness window expires.

The TUI already has a runtime event channel and a bounded frame scheduler, so streaming search
results can be projected without replacing the event-loop skeleton. However, a full streaming
design needs explicit ownership for search sessions, cancellation, query revisions, root changes,
worker lifetime, and stale-result rejection.

The local Codex reference implementation separates these concerns:

- one search session owns a parallel `ignore` walker and a long-lived `nucleo::Nucleo` matcher;
- discovered paths are injected while the walk is still running;
- query changes reparse the existing matcher, using append optimization when possible;
- matcher ticks publish bounded top-N snapshots approximately every 10 ms when results change;
- session generation, the current mention token, and the popup's pending query independently
  reject stale results.

## Direction

Replace the synchronous fuzzy-index lookup with a session-scoped streaming search subsystem.
Entering a searchable mention token creates a session for the active workspace root. Query edits
update that session. The session streams changing top-N snapshots while paths are discovered and
matched. Clearing or leaving the token enters warm-idle state; changing the search root, warm-idle
expiry, or shutting down the TUI cancels and destroys the session.

The candidate catalog has no file-count hard limit. Discovery is best-effort exhaustive: every
eligible path successfully observed by the walker may enter the catalog, while unreadable entries,
unsupported path encoding, traversal errors, and cancellation may be skipped. Result snapshots,
worker count, event frequency, and cancellation latency remain bounded; the popup displays only a
fixed top-N subset.

The walker does not recursively follow symlinked directories. A symlink path may become a candidate
only when its canonical target remains inside the canonical search root. Links resolving outside
the workspace are excluded from the catalog, matching Orca's existing mention-expansion safety
boundary and preventing searches from escaping into arbitrarily large external trees.

The eligible corpus contains regular files and directories, including hidden entries. Discovery
respects `.gitignore`, `.ignore`, Git exclude rules, and equivalent repository ignore semantics by
default. Ignored paths and version-control metadata directories such as `.git`, `.svn`, and `.hg`
are excluded. Directories are candidates so users can progressively navigate a path as well as
fuzzy-match a deep file.

The first release's supported performance envelope is one canonical workspace containing up to
1,000,000 eligible candidate paths. The implementation must remain interactive, stream partial
results, and support bounded cancellation throughout discovery and matching at that scale. This
envelope is an acceptance and benchmarking target, not a candidate-count truncation rule.

At the 1,000,000-path envelope, completed search infrastructure may add at most 512 MiB to the
process resident set, and construction may peak at no more than 768 MiB above the idle baseline.
Exceeding either limit fails acceptance. The implementation must not satisfy the budget by silently
truncating the candidate catalog.

Each TUI instance owns at most one mention-search session and one candidate catalog. Only the
mention token at the active composer cursor may control that session. Moving to a different token
advances the session generation and updates or replaces the current query context; multiple
catalogs must never coexist for different tokens in the same TUI.

When no active mention token remains, the popup and installed query are cleared immediately, but
the catalog enters a 30-second warm-idle period. A traversal already in progress may continue during
that window so a quickly repeated `@` can reuse progressively accumulated or completed state. Warm
state is destroyed and its workers are cancelled when the idle deadline expires, the cwd changes,
or the TUI exits. Reactivation checks repository freshness before reuse. When tracked state changed,
the existing catalog is reset and rebuilt without creating a second catalog; the last top-N popup
projection may remain visible as stale `Refreshing…` state until the first replacement snapshot.

Search concurrency is adaptive but capped. Walker concurrency is
`min(4, max(1, available_parallelism / 2))`; Nucleo matcher concurrency is
`min(4, max(1, ceil(available_parallelism / 3)))`. The subsystem therefore uses no more than eight
compute-heavy workers regardless of machine size. Coordinator threads may handle channels, matcher
ticks, and snapshot publication but must not add unbounded compute parallelism.

Implementation validation amended the original two-matcher cap to four: with one million synthetic
paths, two workers remained above both accepted query-latency SLOs even after replacing Nucleo's
full result sort with bounded top-12 selection. Three workers passed isolated runs but remained
borderline under a 25-sample p95 gate; four workers passed with stable margin using the same
catalog, ranking semantics, and memory budget. Four remains a hard cap.

The popup has explicit streaming states. Before the first snapshot it displays `Searching files…`.
Partial snapshots are displayed immediately with `Scanning… N paths`; successful completion removes
the scanning indicator, and a completed empty result displays `No matches`. Traversal failure keeps
any accepted partial results visible and marks the projection `Search incomplete`.

Selection follows the highest-ranked result until the user navigates manually. After manual
navigation, selection is anchored by candidate path across snapshot replacement. If that path
disappears, selection moves to the candidate nearest its previous index. Streaming updates must not
unconditionally reset selection to the first row.

The first release uses snapshot discovery rather than a filesystem watcher. A completed catalog
represents the eligible paths observed by that walk; external additions, removals, and renames are
not guaranteed to appear while the same token remains active. Warm-state reactivation checks the
Git index identity and modification time and rebuilds when tracked state changed. Untracked and
non-Git changes are guaranteed to receive a fresh walk after the 30-second warm-idle state expires.
Selecting a path deleted after its snapshot was emitted returns the existing path-resolution error.

Live filesystem watching, incremental candidate deletion, and cross-platform watcher overflow
recovery are non-goals for the first release.

Cancellation invalidates the visible session generation immediately. Walker and matcher loops
check cancellation at least every ten milliseconds or every 256 processed entries, whichever
comes first. On supported local filesystems, cancellation stops snapshot publication within 50 ms
and all owned workers terminate and are joined within 500 ms. Worker handles must remain owned;
detached search threads are forbidden.

The TUI thread never blocks while joining. It may project a transient stopping/searching state, but
a replacement catalog for a new cwd does not start until the previous catalog's workers have
terminated, preventing million-path memory overlap. The 500 ms join deadline does not apply while
the operating system has a worker blocked inside an uninterruptible or remote-filesystem directory
operation. On TUI exit, cancellation is immediate and an owned reaper thread takes responsibility
for joining the session's worker handles off the TUI thread.

Performance acceptance uses a 1,000,000-path workspace on a supported local SSD with a warm OS
cache. While scanning, key input reaches a visible frame within 32 ms at p95. Session start emits
its first progress snapshot within 100 ms at p95. On a warm catalog, an appended query character
produces an updated snapshot within 50 ms at p95; deletion or arbitrary query replacement completes
a full reparse within 150 ms at p95.

The worker side publishes no more than 60 snapshots per second and suppresses snapshots whose
visible top-N and status metadata did not change. Full walk completion has no fixed duration SLO;
the contract is continuous progress, responsive input, bounded resources, cancellation, and
eventual completion.

Streaming search lives in a new workspace crate, `orca-file-search`. That crate owns eligible-path
discovery, the full `nucleo` dependency, matcher and walker workers, session lifecycle, snapshots,
cancellation, benchmarks, and concurrency tests. It has no dependency on `orca-tui` or terminal
types so later CLI or app-server surfaces can reuse it.

`orca-runtime::mentions` retains mention-token parsing, selected-path insertion, file-content
expansion, line-range handling, and canonical workspace safety. Its synchronous candidate index and
fuzzy scoring are removed along with runtime-only `ignore` and `nucleo-matcher` dependencies.
`orca-tui::mention_search_manager` owns the single TUI session and projects accepted snapshots into
popup state. Tab completion consumes the current accepted snapshot and must not start synchronous
candidate discovery.

Query commands use a bounded latest-wins slot: a newer composer query replaces an unprocessed older
query instead of queuing behind it. Nucleo wakeups are coalesced so at most one wakeup is pending.
Workers publish snapshots by replacing a shared latest-snapshot slot. Only the transition from “no
pending snapshot” to “pending snapshot” sends a lightweight
`TuiEvent::MentionSearchDirty { generation }`; the TUI consumes the newest slot value, so obsolete
intermediate snapshots never accumulate in the existing unbounded event channel.

Snapshot application validates the session generation, active mention-token identity and query,
and popup pending query. Search phase and sanitized failure state travel inside the snapshot. Query,
wakeup, and snapshot backlog therefore remain bounded independently of typing speed, traversal
rate, or temporary TUI stalls.

Search behavior has two modes. An empty token after `@` asynchronously browses the workspace root,
and a query ending in `/` asynchronously browses that resolved directory's direct children. Browse
results apply the same ignore, hidden-entry, symlink, and workspace-boundary rules as catalog
discovery. Direct-child traversal advances in bounded batches of at most 256 entries or five
milliseconds, retains only the best 12 paths, and publishes progressive snapshots instead of
collecting and sorting an unbounded directory. Any other non-empty query uses global Nucleo fuzzy
matching.

Creating the empty `@` token also starts the full catalog walk, so directory browsing doubles as
search prewarming. Switching between browse and fuzzy modes does not destroy the catalog. Directory
reads, ignore evaluation, canonicalization, and candidate construction run outside the TUI input
thread.

Snapshots contain at most 12 candidates, matching the popup's existing maximum visible row count.
Fuzzy matching uses Nucleo path matching, smart normalization, and smart case: all-lowercase queries
are case-insensitive, while any uppercase query is case-sensitive. Orca adds no product-specific
penalties or boosts beyond Nucleo scoring.

Fuzzy results sort by descending Nucleo score, then files before directories, shorter relative path,
and lexical path order. Browse results sort directories before files and then use smart-case lexical
order. Snapshots include matched character indices for highlighting. Indices are Unicode character
positions, never UTF-8 byte offsets.

## Verification Contract

`orca-file-search` unit tests cover ignore rules, hidden paths, workspace-safe symlinks, browse mode,
smart case, deterministic ranking, and Unicode match indices. Controlled concurrency tests cover
latest-wins query replacement, dirty-event coalescing, cancellation and joining, and the 30-second
warm-idle lifecycle.

Stale-result tests independently exercise old session generation, old mention-token identity or
query, and old popup pending query. Snapshot tests prove scanned count is monotonic and completion is
reported only after traversal has stopped and the matcher has processed all injected candidates.
TUI tests cover streaming states, manual selection anchoring, and snapshot replacement without
selection reset.

Concurrency tests use barriers, channels, injected clocks, and deterministic fake path sources.
Sleeping is allowed only as a final deadlock watchdog, never as the synchronization mechanism that
makes a test pass.

A separate 1,000,000-synthetic-path benchmark enforces the memory and latency SLOs without slowing
ordinary tests. A medium real temporary tree validates filesystem ignore, symlink, browse, and
cancellation behavior. Final validation runs focused `orca-file-search`, mention-runtime, and TUI
tests, followed by workspace checking and tests, formatting, and diff checks.

## Domain Objects

- **Mention token**: the active composer range beginning with `@`, plus its current query text.
- **Search root**: the canonical workspace directory whose paths may become candidates.
- **Search session**: the lifecycle owner for one active token and root.
- **Session generation**: a monotonically changing identity used to reject output from an older
  session.
- **Query revision**: the query value currently installed in the matcher.
- **Path catalog**: the set of eligible paths discovered by the walker and injected into Nucleo.
- **Search snapshot**: a replaceable top-N result set for one generation and query revision,
  including scan progress and completion state.
- **Popup projection**: the TUI state that accepts only snapshots relevant to the current token.

## Required Invariants

1. TUI input handling never performs filesystem walking, Git subprocess execution, or full-catalog
   fuzzy scoring.
2. A snapshot is applied only when its session generation, root, and query remain relevant to the
   current mention token.
3. Destroying a session prevents late worker output from mutating visible popup state.
4. A root change destroys the old session before results for the new root can be accepted.
5. Result count, worker count, channel backlog, and event frequency are bounded.
6. Paths exposed to selection remain relative to and resolvable inside the active search root.
7. Appending to a query may use incremental matcher optimization; deletion or arbitrary editing
   must produce results equivalent to a full reparse.
8. Completion means the walker has stopped and the matcher has processed all injected paths, not
   merely that one snapshot was emitted.

## Reference Behavior

Both reference implementations separate catalog coverage from result presentation. Neither places
a file-count cap on the internal candidate catalog; both bound the top-N results returned to the
popup.

| Concern | Claude Code checkout | Codex checkout |
| --- | --- | --- |
| Discovery | `git ls-files` for tracked paths, untracked paths fetched separately in the background, `ripgrep --files` fallback | Parallel `ignore::WalkBuilder` traversal |
| Catalog population | Collects a path list, then builds a progressively queryable index in event-loop-sized chunks | Injects each discovered path directly into a live `nucleo::Nucleo` session |
| Candidate count cap | No explicit file-count cap; source comments and tests account for roughly 270k-346k path lists | No explicit file-count cap |
| Visible result cap | 15 suggestions | 20 by default in the TUI-oriented library API; 50 in app-server search |
| First-query behavior | Prewarms on mount; an unfinished build exposes the ready prefix and re-runs the current query after completion | Starts a session for the first non-empty query and streams changing snapshots during traversal |
| Query execution | Synchronously searches the ready catalog, debounced by 50 ms in the UI | Reuses the live matcher and reparses on each query update; prefix appends use the incremental optimization |
| Refresh behavior | `.git/index` mtime invalidates tracked paths immediately; a five-second floor checks untracked paths | The TUI session walks once and remains live while the mention token is non-empty; clearing the token drops the catalog |
| Failure bound | Git operations and the overall discovery path are time-bounded; failures retain or return the currently available index | Traversal errors and non-UTF-8 paths are skipped; cancellation or session drop stops further work |
| Hidden and symlink behavior | Git mode follows Git semantics; ripgrep fallback includes hidden paths and follows symlinks | Includes hidden entries, follows symlinks, and respects Git ignore rules by default |

Claude Code's current local source imports a TypeScript `FileIndex` port that preserves the native
Nucleo-facing API shape. It stores every deduplicated path, lowercased path, character bitmap, and
path length, yields index construction approximately every four milliseconds, and searches only
the indexed prefix while construction continues. It is therefore progressive index construction,
not streaming filesystem discovery.

Codex provides the stronger streaming model selected for Orca's direction: walker and matcher are
independent workers, the matcher ticks approximately every ten milliseconds when data changes,
and each snapshot reports scanned candidate count and walk completion. Its TUI rejects stale
output at the session-generation, current-token, and popup-pending-query boundaries.

The practical reference contract is **best-effort exhaustive without count truncation**. Every
eligible path successfully discovered within the implementation's filesystem, encoding, timeout,
and cancellation rules may enter the catalog. Neither implementation promises that filesystem
errors or timeouts are impossible, so “complete” cannot mean an absolute guarantee over every
directory entry.

## Implementation Stages

1. Add the unused `orca-file-search` crate with session, Nucleo, snapshot, bounded command and
   notification primitives, plus deterministic unit and concurrency tests.
2. Add the real walker, browse mode, eligibility and symlink policy, warm-idle and cancellation
   lifecycle, and the million-path benchmark.
3. Add the TUI manager, dirty event, three stale-result guards, streaming popup projection, match
   highlighting, and selection anchoring while retaining the synchronous path.
4. Cut composer and Tab completion over to accepted snapshots, remove the synchronous runtime
   index and cache, then run PTY, performance, focused, and full-workspace verification.

Every stage must compile, format, and pass its focused tests. The old synchronous path is removed
only after the asynchronous TUI path is proven, keeping the first three stages independently
reversible.

## Consequences

The TUI no longer performs repository indexing or full-catalog scoring on the input path, and Orca
can return useful matches before a million-path traversal completes. The search subsystem becomes
reusable outside the TUI and has explicit ownership, cancellation, memory, concurrency, and stale
result contracts.

The cost is a materially larger change than the original cache optimization: approximately
1,500-2,200 changed lines including tests and benchmark infrastructure, with an estimated four to
six focused development days. A used million-path catalog may retain hundreds of MiB during its
active and 30-second warm-idle lifetime. The first release intentionally accepts snapshot staleness
instead of adding filesystem-watcher complexity.

## Rejected Alternatives

### Serve-Stale Cache Refresh Only

This removes synchronous rebuilds but does not provide partial cold-start results or complete
million-path coverage. Rejected because the selected product target explicitly requires the full
streaming architecture.

### Rebuild-Complete TUI Event Only

This refreshes the popup after a background cache completes but still waits for complete discovery
before producing useful fuzzy results. Rejected as the final architecture, though the dirty-event
coalescing pattern is retained.

### Multiple Concurrent Token Catalogs

This simplifies token switching but can multiply the million-path memory footprint. Rejected in
favor of one catalog and generation-controlled token ownership per TUI.

### Filesystem Watcher In The First Release

This could keep a completed catalog live but introduces cross-platform overflow, rename, ignore,
and incremental deletion semantics. Rejected until snapshot behavior produces concrete freshness
complaints.

## Reference Evidence

- Orca synchronous cache and build path: `crates/orca-runtime/src/mentions.rs:280-469`.
- Orca runtime event drain: `crates/orca-tui/src/app.rs:239-305`.
- Codex session orchestration: `codex-rs/tui/src/file_search.rs` in the local Codex checkout.
- Codex walker and matcher workers: `codex-rs/file-search/src/lib.rs:399-603` in the local Codex
  checkout.
