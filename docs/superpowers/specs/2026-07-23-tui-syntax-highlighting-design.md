# TUI Syntax Highlighting Design

## Objective

Add syntax highlighting to fenced Markdown code blocks and unified diffs in the
Orca TUI using the same `syntect` plus `two-face` combination used by Codex and
grok-build.

The first rendered frame must be useful without waiting for background work:
fenced code blocks are highlighted synchronously, and diffs are highlighted
with hunk-local parser state. Completed file-edit diffs are then eligible for a
background full-file syntax pass that corrects scopes requiring source context
before the first visible hunk.

All highlighted output is stored in `TranscriptRenderCache` together with the
wrapped lines. Scrolling, ordinary redraws, and spinner ticks must not rerun
syntax highlighting.

## Scope

This change includes:

- `syntect` and `two-face` dependencies in the Orca workspace.
- Syntax highlighting for fenced Markdown code blocks.
- Syntax highlighting for unified diffs rendered in transcript tool messages.
- A syntax-theme revision in the transcript render-cache key.
- Codex-compatible highlight safety limits.
- Hunk-local first-paint diff highlighting.
- A single background worker for post-edit full-file scope refinement.
- Focused unit and integration-style TUI tests for rendering, limits, caching,
  stale-result rejection, and progressive refinement.

This change does not include:

- A user-facing syntax-theme picker.
- Custom `.tmTheme` loading.
- Highlighting arbitrary tool stdout as source code.
- Persisting derived highlight spans in session history.
- Changing runtime or event-journal protocols to carry full-file snapshots.

## Reference Behavior

The implementation follows three reference patterns:

- Codex's `render/highlight.rs` for `syntect`/`two-face` initialization,
  language lookup, and the 512 KiB / 10,000-line / 4 KiB-line guardrails.
- Codex's Markdown render cache for explicit `syntax_theme_revision` cache
  invalidation.
- grok-build's edit highlight worker for immediate hunk-local rendering,
  off-draw-thread full-file parsing, latest-job-wins behavior, source-text
  validation, and stale-result rejection.

Claude Code is a visual and behavioral comparison only. It does not define the
Rust highlighting architecture.

## Dependencies and Module Boundary

Add workspace dependencies:

```toml
syntect = { version = "5.3", default-features = false, features = ["default-syntaxes", "dump-load", "parsing", "regex-fancy"] }
two-face = { version = "0.5", default-features = false, features = ["syntect-fancy"] }
```

`orca-tui` consumes both dependencies.

New syntax-specific code belongs in focused modules rather than growing
`ui.rs` or `transcript_view.rs`:

- `syntax_highlight.rs`: syntax and theme initialization, language lookup,
  guardrails, style conversion, code-block highlighting, hunk highlighting,
  and full-file style-map computation.
- `diff_highlight.rs`: unified-diff parsing and conversion into transcript
  lines while preserving Orca's existing diff prefix semantics.
- `edit_highlight_worker.rs`: the single background worker, jobs, results,
  coalescing, and capped file reads.

`ui.rs` remains the message/Markdown renderer and delegates highlighting.
`transcript_view.rs` remains the render-cache owner.

## Syntax Engine

### Syntax database

A process-global `OnceLock<SyntaxSet>` initializes with
`two_face::syntax::extra_newlines`. This gives Orca two-face's extended grammar
set while preserving newline-aware syntect parsing.

Language lookup:

1. Extract the first token from a Markdown fence info string, splitting on
   commas, spaces, and tabs.
2. Normalize known aliases such as `csharp`, `golang`, `python3`, and `shell`.
3. Try `find_syntax_by_token`.
4. Try exact and case-insensitive syntax names.
5. Try the token as a file extension.

Diff syntax selection uses the destination path from the unified diff. For
renames, the `+++` destination path wins. `/dev/null` falls back to the
remaining real path.

Unknown languages and paths return no highlighted output. Callers keep their
plain-text fallback.

### Theme mapping and revision

Orca continues to own its four application themes. Each `ThemeName` maps to a
bundled two-face syntax theme with matching polarity:

- Dark: `OneHalfDark`
- Light: `OneHalfLight`
- Solarized: `SolarizedDark`
- Catppuccin: `CatppuccinMocha`

The syntax subsystem exposes:

- `syntax_theme_revision() -> u64`
- a snapshot containing the selected syntect theme and its revision

The revision changes whenever the selected syntax theme changes. The initial
implementation derives the syntax theme from the immutable TUI `ThemeName`, so
normal runs have one stable revision. The explicit revision remains part of
the cache and worker contracts so a future live syntax-theme picker can safely
invalidate derived spans without redesigning the cache.

Only foreground color and bold are copied from syntect into ratatui styles.
Syntect backgrounds are ignored so Orca's selection and diff backgrounds
remain authoritative. Italic and underline are ignored to avoid inconsistent
terminal rendering.

## Guardrails

Every syntax-highlighting entry point applies the same strict limits before
creating or advancing a syntect parser:

- Total UTF-8 bytes greater than `512 * 1024`: plain text.
- Actual line count greater than `10_000`: plain text.
- Any individual line greater than `4 * 1024` UTF-8 bytes: plain text.

The comparison is strictly greater-than. Inputs exactly at a limit remain
eligible.

Line count uses actual logical lines rather than newline-byte count so a final
line without a trailing newline is counted correctly.

For diffs, the aggregate visible hunk content is checked before any per-hunk
parser is initialized. The background full-file pass applies the same limits
to the complete post-edit file. Any limit failure is a silent, deterministic
fallback to the existing plain-text rendering.

## Markdown Code Blocks

`render_markdown` buffers a fenced code block until `TagEnd::CodeBlock`.
Buffering is required because pulldown-cmark may split one code block across
multiple `Event::Text` events, while syntect state must span all source lines.

On code-block end:

1. Parse the language token from the fence info.
2. Check all guardrails.
3. Highlight the complete block with one `HighlightLines` instance.
4. Convert each highlighted source line to ratatui spans.
5. Prefix the rendered line with the existing two-space code indentation.
6. Fall back to the existing gray code style if lookup, limits, or parsing
   fails.

The renderer preserves source text exactly apart from stripping line endings
that are represented structurally by ratatui `Line` values. Blank source lines
remain blank rendered lines.

Inline code retains its current styling and is outside the syntax parser.

## Diff First Paint

The existing transcript diff view remains compact and does not adopt Codex's
line-number layout in this change. It keeps the unified-diff text, `+`/`-`
prefixes, truncation behavior, and Orca add/remove colors.

The diff parser identifies:

- `---` and `+++` file headers,
- `@@ ... @@` hunk headers,
- old and new line numbers,
- context, insert, and delete lines,
- metadata lines that are not source.

Each hunk owns two syntect parsers:

- The old-side parser advances on delete and context lines.
- The new-side parser advances on insert and context lines.

This prevents a multiline construct on one side from leaking into the other
side. Parser state intentionally resets at each hunk for a fast first paint.

The `+`, `-`, and leading context prefix are never passed into syntect. Syntax
styles apply only to source content. Existing add/remove foreground colors are
the fallback and remain visible on unhighlighted content; successfully
highlighted token foregrounds override the fallback while line classification
remains available to the renderer.

Diff headers and hunk headers use the current muted/default styles.

## Progressive Full-File Refinement

### Eligibility

A background refinement job is submitted only for a newly completed,
successful tool message when all of the following are true:

- The message has a non-empty unified diff.
- The tool target resolves to a file inside the configured workspace.
- The diff has at least one context or inserted new-side source line.
- The destination path resolves to a known syntax.
- This is a live completion, not replayed history.

Historical session replay never queues jobs. Replayed files may have changed,
and queueing every historical edit would create a startup thundering herd.

### Job identity

Each job carries:

- monotonically allocated job ID,
- tool call ID,
- transcript message index,
- message revision,
- syntax-theme revision,
- absolute post-edit path,
- display/destination path used for syntax lookup,
- parsed diff hunks and expected new-side line text.

One worker thread consumes jobs. Before executing queued work, it coalesces
jobs by tool call ID so only the latest job for an entry runs. A replacement
job removes the older in-flight identity from pending state.

### Background computation

The worker:

1. Reads file metadata and rejects non-files or files over 512 KiB.
2. Reads bytes once, verifies the post-read size, and requires UTF-8.
3. Applies the 10,000-line and 4 KiB-line guards.
4. Creates one new-side highlighter for the destination path.
5. Walks the file from line one through the highest requested new-side line.
6. Retains styles only for context and inserted lines visible in the diff.
7. Verifies each retained file line exactly equals the corresponding diff
   new-side text.

If any expected line is missing, duplicated with conflicting text, shifted, or
different from disk, the entire refinement fails. The worker never changes the
displayed text; it returns only foreground style runs keyed by one-based
new-file line number.

Delete lines cannot be derived from the post-edit file and always retain the
hunk-local old-side highlighting.

### Result application

The TUI event loop polls worker results without blocking. While jobs are
pending, the scheduler treats refinement as lightweight animation demand so
results are observed even when the session is otherwise idle.

A result is applied only if all identities still match:

- tool call ID,
- message index and current message revision,
- destination path,
- job ID currently pending for that message,
- current syntax-theme revision.

Ready results are stored in TUI-only derived state associated with the tool
message. Applying a result increments that message's render revision and
invalidates only its `TranscriptRenderCache` entry. The next render overlays
full-file styles on matching context/insert lines, wraps once, and caches the
result.

Failed, disconnected, mismatched, and stale results revert to or retain
hunk-local rendering without user-visible errors.

## Transcript Cache

`CachedMessage` gains `syntax_theme_revision`. The revision participates in:

- `CachedMessage::matches`,
- `CachedMessage::patch_spinner`,
- cache-entry construction.

`TranscriptRenderCache` gains `prepared_syntax_theme_revision`. A revision
change marks all current messages dirty once, parallel to width and theme
identity changes.

The cache stores highlighted spans through the existing
`CompactWrappedLine::style_runs`. No syntect parser runs in `viewport`,
`materialize_rows`, selection painting, or clipboard extraction.

The cache behavior guarantees:

- Scroll-only frames perform zero Markdown parses and zero syntax passes.
- Spinner-only changes patch the spinner in place when the syntax revision is
  unchanged.
- A full-file refinement causes one affected message rebuild, not a transcript
  rebuild.
- Text selection changes backgrounds while preserving syntax foregrounds.

## State Ownership

Highlight refinement is derived presentation state and is not added to
`ChatMessage`'s persisted semantic fields.

`AppState` owns:

- the optional worker runtime,
- pending job identities keyed by tool call ID,
- completed full-file style maps keyed by tool call ID,
- the workspace path needed to resolve edit targets.

Message mutation, truncation, clear, backtrack, and retention operations prune
or invalidate derived entries so a reused message index cannot inherit old
styles.

The worker owns no `AppState` references and communicates with owned job/result
values over channels.

## Error Handling

Syntax highlighting is presentation enhancement, never a correctness
dependency.

- Unknown grammar: plain text.
- Syntect parse error: plain text for that block or hunk.
- Oversized input: plain text.
- File read, UTF-8, or metadata failure: keep hunk-local diff.
- File/diff mismatch: keep hunk-local diff.
- Stale worker result: discard.
- Worker channel disconnect: clear pending jobs and keep hunk-local diff.

No failure adds transcript noise or changes tool status.

## Testing Strategy

Implementation follows test-driven development. Each behavior is introduced by
a focused failing test, the failure is observed, and only then is production
code added.

### Syntax module

- Rust and Python produce multiple token foreground styles.
- Multiline strings/comments preserve parser state.
- Fence info with metadata resolves its first language token.
- Known aliases resolve.
- Unknown languages return fallback.
- Inputs over 512 KiB return fallback.
- Inputs over 10,000 actual lines, including no trailing newline, return
  fallback.
- A line over 4 KiB returns fallback.
- Inputs exactly at each limit remain eligible when otherwise valid.

### Markdown rendering

- A fenced Rust block has syntax-colored spans and unchanged text.
- Split pulldown-cmark text events still share parser state.
- Unknown and oversized blocks preserve the existing gray style.
- Inline code behavior remains unchanged.

### Diff rendering

- Rust diff content is token-colored while prefixes remain intact.
- Old/new hunk parser states are independent.
- Context advances both sides.
- Hunk boundaries reset fast-path state.
- Destination extension selects syntax, including rename and add/delete cases.
- Aggregate guardrail failures retain existing add/remove rendering.
- Wrapping preserves syntax runs and copied text.

### Progressive refinement

- A Python triple-quoted-string fixture demonstrates that hunk-only styles
  differ from full-file styles after a closing delimiter.
- Full-file computation matches a direct full-file syntect pass.
- Only context/insert lines are replaced; delete lines keep hunk styles.
- File text drift rejects the whole result.
- Oversized, too-many-line, long-line, non-UTF-8, and missing files fail
  without changing first-paint output.
- Coalescing keeps the latest job for one tool entry.
- Message-revision, job-ID, path, and syntax-theme mismatches discard results.
- Successful application invalidates only the matching message.
- Replay does not submit background jobs.

### Cache and frame cost

- Changing `syntax_theme_revision` rebuilds cached messages.
- An unchanged revision reuses wrapped highlighted lines.
- Scroll-only frames run zero message builds, Markdown parses, and syntax
  passes.
- Spinner-only frames patch in place without syntax work.
- Selection preserves syntax foreground colors.

## Validation

Run focused checks first:

```sh
cargo test -p orca-tui syntax_highlight
cargo test -p orca-tui diff_highlight
cargo test -p orca-tui edit_highlight_worker
cargo test -p orca-tui transcript_view
```

Then run the full changed crate and workspace checks:

```sh
cargo test -p orca-tui
cargo fmt --all -- --check
cargo clippy -p orca-tui --all-targets -- -D warnings
cargo test --workspace
```

Dependency lockfile changes must be included, and `git diff --check` must pass.

## Completion Criteria

The feature is complete only when:

1. Fenced code blocks and recognized unified diffs visibly use syntax token
   colors.
2. `syntect` and `two-face` are the implemented engine and grammar/theme
   source.
3. All three strict Codex guardrails fall back to plain text.
4. Highlighted wrapped lines are cached with an explicit syntax-theme
   revision, with no per-frame syntax work.
5. Diff first paint uses hunk-local old/new parser state.
6. Eligible live edits can upgrade off-thread to verified full-file-scoped
   context/insert styles.
7. Stale, mismatched, failed, replayed, or over-limit refinement never replaces
   the hunk-local result.
8. Focused tests, the `orca-tui` test suite, formatting, clippy, and workspace
   tests pass.
