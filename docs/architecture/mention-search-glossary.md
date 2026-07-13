# Mention Search Glossary

This glossary defines the terms used while designing Orca's streaming `@` file search. It is a
living document and will be updated as the design grilling resolves the open decisions in
ADR-0001.

## Candidate

An eligible relative file or directory path that may appear in mention search results. Candidates
include hidden entries but exclude ignored paths, version-control metadata directories, and unsafe
symlinks that resolve outside the search root.

## Best-Effort Exhaustive

A discovery contract with no file-count truncation: every eligible path successfully observed by
the walker may enter the catalog. Filesystem errors, unreadable entries, unsupported path encoding,
cancellation, and explicit timeouts can still prevent absolute coverage.

## Browse Mode

Asynchronous direct-child listing for an empty `@` token or a query ending in `/`. Browse mode
shares eligibility and safety rules with fuzzy discovery and does not destroy the full catalog.

## Catalog

The session-owned collection of candidates already discovered and injected into the matcher. A
catalog may be incomplete while traversal is active.

## Completion

The state in which traversal has stopped and the matcher has incorporated all injected candidates.
Completion is distinct from receiving the first result snapshot.

## Cancellation Terminal State

The state reached after a session generation is invalidated, snapshot publication has stopped, and
all owned walker and matcher workers have terminated and been joined.

## Mention Token

The active composer text range beginning with `@` that currently owns file-search behavior. The
token has a range and query text; both can change as the user edits the composer.

## Path Discovery

Filesystem traversal that identifies eligible candidates. In the proposed architecture, discovery
runs outside the TUI thread and injects candidates incrementally.

## Popup Projection

The visible TUI state derived from accepted snapshots. It owns selection and loading/no-match
presentation but does not own filesystem traversal or fuzzy matching.

## Dirty Event

A lightweight TUI notification that a newer snapshot is available in the shared latest-snapshot
slot. At most one dirty event may be outstanding for a session generation.

## Query Revision

The exact query string currently installed in the matcher. Snapshots are tagged with their query so
the projection can reject results computed for older edits.

## Search Root

The canonical workspace directory that bounds candidate discovery and path selection.

## Workspace-Safe Symlink

A symlink whose canonical target remains inside the canonical search root. The proposed design may
index the link path itself but never recursively traverses a symlinked directory.

## Search Session

The lifecycle boundary that owns path discovery, the Nucleo matcher, worker coordination,
cancellation, and snapshot publication for an active mention search.

## Supported Performance Envelope

The scale at which responsiveness, streaming, cancellation, and resource acceptance criteria must
be demonstrated. The first release targets 1,000,000 eligible paths without treating that number as
a catalog truncation limit. At that scale, completed search infrastructure is limited to 512 MiB of
incremental RSS and construction is limited to a 768 MiB incremental peak.

## Session Generation

An identity that changes whenever active token or root ownership changes, including when a search
session is replaced. It prevents an older token or stopped session from publishing results into the
current popup while allowing the same catalog to be reused across token generations.

## Snapshot

A replaceable, bounded top-N view of the matcher for one session generation and query revision. A
snapshot may also report discovered candidate count and whether traversal is complete.

## Filesystem Snapshot

The eligible path set observed by one completed walk. The first release does not continuously
maintain it with a filesystem watcher; later filesystem changes appear in a subsequent walk.

## Selection Anchor

The candidate path explicitly selected by manual keyboard navigation. Streaming snapshot updates
preserve this path when it remains present instead of resetting selection to the new first result.

## Stale Result

A snapshot that no longer corresponds to the active session, root, mention token, or pending popup
query. Stale results must be ignored without changing selection or loading state.

## Walker

The background producer that traverses one or more search roots and injects eligible paths into the
catalog.

## Warm-Idle Period

The 30-second interval after the active mention token disappears during which the single workspace
catalog remains reusable. Popup and query state are cleared immediately; expiry, cwd change, or TUI
shutdown cancels remaining work and releases the catalog.
