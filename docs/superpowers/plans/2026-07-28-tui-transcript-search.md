# TUI Transcript Search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add smart-case full-transcript search with `Ctrl+F`, Vim `/ n N`, highlighted visible matches, wrapped-coordinate navigation, and zero steady-frame rescans.

**Architecture:** `TranscriptRenderCache` exposes read-only rendered-logical-line search and a monotonic content generation. A focused `TranscriptSearchState` stores only the query, cursor, match coordinates, and active result; `AppState` coordinates refresh and scrolling. Search and mouse-selection styles are applied after viewport materialization, so cache entries remain search-agnostic.

**Tech Stack:** Rust 2024, ratatui 0.29, crossterm, tui-textarea, Unicode display-width utilities, existing `TranscriptRenderCache`.

---

## File Map

- Create `crates/orca-tui/src/transcript_search.rs`
  - Smart-case query matching, query editing, active-match preservation,
    navigation, row-range projection, and pure tests.
- Modify `crates/orca-tui/src/lib.rs`
  - Register the focused module.
- Modify `crates/orca-tui/src/transcript_view.rs`
  - Cache content generation, logical rendered-text search, UTF-8/display
    coordinate mapping, reveal-offset calculation, and performance tests.
- Modify `crates/orca-tui/src/types.rs`
  - Own search state, refresh results, navigate, scroll, and reconcile
    transcript mutations.
- Modify `crates/orca-tui/src/shortcuts.rs`
  - Register `Ctrl+F` and expose the shortcut hint.
- Modify `crates/orca-tui/src/key_event_actions.rs`
  - Give active search input priority while preserving global `Ctrl+C`.
- Modify `crates/orca-tui/src/status_key_actions.rs`
  - Route Vim normal `/`, `n`, and `N` in Idle, Running, and WaitingUserInput.
- Modify `crates/orca-tui/src/input_event_actions.rs`
  - Route bracketed paste to the search query.
- Modify `crates/orca-tui/src/ui.rs`
  - Search-row layout, hardware cursor, result counter, search overlay, and
    completed-frame tests.
- Modify `crates/orca-tui/src/theme.rs`
  - Capability-safe active and inactive search styles.
- Modify `crates/orca-tui/src/app.rs`
  - Event-loop integration tests; production flow uses the focused input
    handlers.
- Modify `crates/orca-tui/src/vim.rs`
  - A small pure normal-mode search-intent resolver only.

Baseline for final scope audit:

```text
c2999f9c509067ef1f8426047cd26e87514404b6
```

Written design:

```text
docs/superpowers/specs/2026-07-28-tui-transcript-search-design.md
```

---

### Task 1: Build Smart-Case Query and Search State

**Files:**
- Create: `crates/orca-tui/src/transcript_search.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [ ] **Step 1: Register the module and intended types**

Add to `crates/orca-tui/src/lib.rs` in alphabetical order:

```rust
mod transcript_search;
```

Create `crates/orca-tui/src/transcript_search.rs` with:

```rust
use std::ops::Range;

use crate::selection::SelectionPos;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SearchQuery {
    original: String,
    needle: String,
    case_sensitive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct TranscriptLineIdentity {
    pub(crate) message_revision: u64,
    pub(crate) line_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptSearchMatch {
    pub(crate) start: SelectionPos,
    pub(crate) end: SelectionPos,
    pub(crate) line_identity: TranscriptLineIdentity,
    pub(crate) byte_range: Range<usize>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TranscriptSearchState {
    pub(crate) open: bool,
    query: String,
    cursor: usize,
    matches: Vec<TranscriptSearchMatch>,
    active: Option<usize>,
    prepared_generation: Option<u64>,
    prepared_query: String,
    #[cfg(test)]
    scan_count: usize,
}
```

- [ ] **Step 2: Write failing smart-case and UTF-8 tests**

Add:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_query_is_case_insensitive_and_uppercase_query_is_sensitive() {
        let insensitive = SearchQuery::new("error");
        assert_eq!(insensitive.find_ranges("ERROR error"), vec![0..5, 6..11]);

        let sensitive = SearchQuery::new("Error");
        assert_eq!(sensitive.find_ranges("ERROR Error error"), vec![6..11]);
    }

    #[test]
    fn unicode_case_folding_maps_matches_back_to_original_boundaries() {
        let query = SearchQuery::new("ä");
        let text = "Ärger ä";
        let ranges = query.find_ranges(text);
        assert_eq!(ranges.len(), 2);
        assert_eq!(&text[ranges[0].clone()], "Ä");
        assert_eq!(&text[ranges[1].clone()], "ä");
        assert!(ranges
            .iter()
            .all(|range| text_boundary_or_empty(text, range)));
    }

    #[test]
    fn repeated_matches_are_non_overlapping() {
        assert_eq!(SearchQuery::new("aa").find_ranges("aaaa"), vec![0..2, 2..4]);
        assert!(SearchQuery::new("").find_ranges("anything").is_empty());
    }

    fn text_boundary_or_empty(text: &str, range: &Range<usize>) -> bool {
        range.is_empty()
            || (range.end <= text.len()
                && text.is_char_boundary(range.start)
                && text.is_char_boundary(range.end))
    }
}
```

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-tui lowercase_query_is_case_insensitive --lib
cargo test -p orca-tui unicode_case_folding_maps --lib
```

Expected: `SearchQuery::new` and `find_ranges` are missing.

- [ ] **Step 4: Implement linear per-line smart-case matching**

Use:

```rust
impl SearchQuery {
    pub(crate) fn new(query: &str) -> Self {
        let case_sensitive = query.chars().any(char::is_uppercase);
        let needle = if case_sensitive {
            query.to_string()
        } else {
            query.chars().flat_map(char::to_lowercase).collect()
        };
        Self {
            original: query.to_string(),
            needle,
            case_sensitive,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.original.is_empty()
    }

    pub(crate) fn find_ranges(&self, text: &str) -> Vec<Range<usize>> {
        if self.is_empty() {
            return Vec::new();
        }
        if self.case_sensitive {
            return text
                .match_indices(&self.needle)
                .map(|(start, matched)| start..start + matched.len())
                .collect();
        }

        let mut folded = String::new();
        let mut boundaries = Vec::with_capacity(text.chars().count() + 1);
        for (original_offset, character) in text.char_indices() {
            boundaries.push((folded.len(), original_offset));
            folded.extend(character.to_lowercase());
        }
        boundaries.push((folded.len(), text.len()));

        folded
            .match_indices(&self.needle)
            .filter_map(|(start, matched)| {
                let end = start + matched.len();
                let original_start = boundaries
                    .binary_search_by_key(&start, |(folded, _)| *folded)
                    .ok()
                    .map(|index| boundaries[index].1)?;
                let original_end = boundaries
                    .binary_search_by_key(&end, |(folded, _)| *folded)
                    .ok()
                    .map(|index| boundaries[index].1)?;
                Some(original_start..original_end)
            })
            .collect()
    }
}
```

This allocates one folded logical line, never one folded transcript.

- [ ] **Step 5: Write failing query-edit and navigation tests**

Add:

```rust
fn match_at(row: usize, col: usize, revision: u64, bytes: Range<usize>) -> TranscriptSearchMatch {
    TranscriptSearchMatch {
        start: SelectionPos { row, col },
        end: SelectionPos {
            row,
            col: col + (bytes.end - bytes.start),
        },
        line_identity: TranscriptLineIdentity {
            message_revision: revision,
            line_index: 0,
        },
        byte_range: bytes,
    }
}

#[test]
fn query_editing_uses_utf8_byte_cursor_and_paste_normalizes_lines() {
    let mut search = TranscriptSearchState::default();
    search.open_new();
    search.insert_char('中');
    search.insert_char('a');
    search.move_left();
    search.insert_char('文');
    assert_eq!(search.query(), "中文a");
    assert_eq!(search.cursor(), "中文".len());
    assert!(search.backspace());
    assert_eq!(search.query(), "中a");
    search.insert_paste("one\r\ntwo\nthree");
    assert_eq!(search.query(), "中one two threea");
}

#[test]
fn refresh_preserves_active_identity_and_selects_nearest_following_match() {
    let mut search = TranscriptSearchState::default();
    search.open_new();
    search.replace_query("hit");
    let first = match_at(2, 0, 10, 0..3);
    let second = match_at(8, 0, 20, 0..3);
    search.refresh_with(1, 5, |_| vec![first.clone(), second.clone()]);
    assert_eq!(search.active_match(), Some(&second));

    search.next();
    assert_eq!(search.active_match(), Some(&first));
    search.refresh_with(2, 0, |_| vec![second.clone()]);
    assert_eq!(search.active_match(), Some(&second));
}

#[test]
fn next_and_previous_wrap_without_rescanning() {
    let mut search = TranscriptSearchState::default();
    search.open_new();
    search.replace_query("x");
    search.refresh_with(1, 0, |_| {
        vec![match_at(1, 0, 1, 0..1), match_at(4, 0, 2, 0..1)]
    });
    let scans = search.scan_count;
    assert_eq!(search.next().map(|found| found.start.row), Some(4));
    assert_eq!(search.next().map(|found| found.start.row), Some(1));
    assert_eq!(search.previous().map(|found| found.start.row), Some(4));
    assert_eq!(search.scan_count, scans);
}
```

- [ ] **Step 6: Run RED**

```sh
cargo test -p orca-tui query_editing_uses_utf8 --lib
cargo test -p orca-tui refresh_preserves_active_identity --lib
cargo test -p orca-tui next_and_previous_wrap --lib
```

Expected: state methods are missing.

- [ ] **Step 7: Implement minimal search state**

Implement:

```rust
impl TranscriptSearchMatch {
    pub(crate) fn cols_on_row(&self, row: usize) -> Option<(usize, Option<usize>)> {
        let last_row = self.last_covered_row();
        if row < self.start.row || row > last_row {
            return None;
        }
        if self.start.row == self.end.row {
            return (self.start.col < self.end.col)
                .then_some((self.start.col, Some(self.end.col)));
        }
        if row == self.start.row {
            Some((self.start.col, None))
        } else if row == last_row {
            if self.end.col == 0 {
                Some((0, None))
            } else {
                Some((0, Some(self.end.col)))
            }
        } else {
            Some((0, None))
        }
    }
}

impl TranscriptSearchState {
    pub(crate) fn open_new(&mut self) {
        if self.open {
            return;
        }
        self.open = true;
        self.query.clear();
        self.cursor = 0;
        self.matches.clear();
        self.active = None;
        self.invalidate_prepared();
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn has_query(&self) -> bool {
        !self.query.is_empty()
    }

    pub(crate) fn insert_char(&mut self, character: char) {
        self.query.insert(self.cursor, character);
        self.cursor += character.len_utf8();
        self.invalidate_prepared();
    }

    pub(crate) fn insert_paste(&mut self, pasted: &str) {
        let normalized = pasted
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .split('\n')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        self.query.insert_str(self.cursor, &normalized);
        self.cursor += normalized.len();
        self.invalidate_prepared();
    }

    pub(crate) fn backspace(&mut self) -> bool {
        let Some(previous) = self.query[..self.cursor].char_indices().next_back().map(|(i, _)| i)
        else {
            return false;
        };
        self.query.drain(previous..self.cursor);
        self.cursor = previous;
        self.invalidate_prepared();
        true
    }

    pub(crate) fn move_left(&mut self) {
        self.cursor = self.query[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    pub(crate) fn move_right(&mut self) {
        self.cursor = self.query[self.cursor..]
            .chars()
            .next()
            .map_or(self.query.len(), |character| {
                self.cursor + character.len_utf8()
            });
    }

    pub(crate) fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn move_end(&mut self) {
        self.cursor = self.query.len();
    }

    pub(crate) fn clear_query(&mut self) {
        self.query.clear();
        self.cursor = 0;
        self.matches.clear();
        self.active = None;
        self.invalidate_prepared();
    }

    pub(crate) fn replace_query(&mut self, query: &str) {
        self.query.clear();
        self.query.push_str(query);
        self.cursor = self.query.len();
        self.invalidate_prepared();
    }

    fn invalidate_prepared(&mut self) {
        self.prepared_generation = None;
    }
}
```

Add:

```rust
impl TranscriptSearchState {
    pub(crate) fn refresh_with(
        &mut self,
        generation: u64,
        viewport_base: usize,
        search: impl FnOnce(&SearchQuery) -> Vec<TranscriptSearchMatch>,
    ) {
        if self.prepared_generation == Some(generation)
            && self.prepared_query == self.query
        {
            return;
        }

        let same_query = self.prepared_query == self.query;
        let previous = same_query
            .then(|| self.active_match().cloned())
            .flatten();
        let query = SearchQuery::new(&self.query);
        let matches = if query.is_empty() {
            Vec::new()
        } else {
            #[cfg(test)]
            {
                self.scan_count += 1;
            }
            search(&query)
        };

        let active = if matches.is_empty() {
            None
        } else if let Some(previous) = previous.as_ref() {
            matches
                .iter()
                .position(|found| {
                    found.line_identity == previous.line_identity
                        && found.byte_range == previous.byte_range
                })
                .or_else(|| {
                    let next = matches.partition_point(|found| {
                        found.start < previous.start
                    });
                    Some(if next == matches.len() { 0 } else { next })
                })
        } else {
            let next = matches.partition_point(|found| {
                found.start.row < viewport_base
            });
            Some(if next == matches.len() { 0 } else { next })
        };

        self.matches = matches;
        self.active = active;
        self.prepared_generation = Some(generation);
        self.prepared_query.clone_from(&self.query);
    }

    pub(crate) fn next(&mut self) -> Option<&TranscriptSearchMatch> {
        if self.matches.is_empty() {
            self.active = None;
            return None;
        }
        self.active = Some(match self.active {
            Some(index) => (index + 1) % self.matches.len(),
            None => 0,
        });
        self.active_match()
    }

    pub(crate) fn previous(&mut self) -> Option<&TranscriptSearchMatch> {
        if self.matches.is_empty() {
            self.active = None;
            return None;
        }
        self.active = Some(match self.active {
            Some(0) | None => self.matches.len() - 1,
            Some(index) => index - 1,
        });
        self.active_match()
    }

    pub(crate) fn active_match(&self) -> Option<&TranscriptSearchMatch> {
        self.active.and_then(|index| self.matches.get(index))
    }

    pub(crate) fn active_index(&self) -> Option<usize> {
        self.active
    }

    pub(crate) fn active_ordinal(&self) -> Option<usize> {
        self.active.map(|index| index + 1)
    }

    pub(crate) fn match_count(&self) -> usize {
        self.matches.len()
    }

    pub(crate) fn visible_matches(
        &self,
        start_row: usize,
        end_row: usize,
    ) -> impl Iterator<Item = (usize, &TranscriptSearchMatch)> {
        self.matches
            .iter()
            .enumerate()
            .skip_while(move |(_, found)| found.last_covered_row() < start_row)
            .take_while(move |(_, found)| found.start.row < end_row)
    }

    #[cfg(test)]
    pub(crate) fn scan_count_for_test(&self) -> usize {
        self.scan_count
    }
}

impl TranscriptSearchMatch {
    pub(crate) fn last_covered_row(&self) -> usize {
        if self.end.row > self.start.row && self.end.col == 0 {
            self.end.row - 1
        } else {
            self.end.row
        }
    }
}
```

This pins:

1. zero scans when generation and query are unchanged;
2. exact preservation by `line_identity + byte_range`;
3. nearest following match after removal;
4. first match at or below `viewport_base` for a new query;
5. wrap only when no following match exists.

- [ ] **Step 8: Run GREEN and commit**

```sh
cargo test -p orca-tui transcript_search --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/lib.rs crates/orca-tui/src/transcript_search.rs
git commit -m "feat(tui): add transcript search state" \
  -m "Implement smart-case literal matching, UTF-8 query editing, stable active-match preservation, and wraparound navigation." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 2: Search Cached Logical Lines and Map Visual Coordinates

**Files:**
- Modify: `crates/orca-tui/src/transcript_view.rs`

- [ ] **Step 1: Write failing cache-generation tests**

Add:

```rust
#[test]
fn content_generation_changes_only_when_searchable_cache_content_changes() {
    let messages = vec![ChatMessage::Assistant("alpha".to_string())];
    let mut revisions = vec![1];
    let mut cache = TranscriptRenderCache::default();
    let theme = theme();
    assert_eq!(cache.content_generation(), 0);

    cache.prepare(
        &messages,
        &revisions,
        TranscriptRenderContext::new(&theme, 40, 0, false),
        |_, message, theme, width, tick, force_expand| {
            build_lines_for_messages(
                std::slice::from_ref(message),
                theme,
                width,
                tick,
                force_expand,
            )
        },
    );
    let built = cache.content_generation();
    assert_ne!(built, 0);

    cache.prepare(
        &messages,
        &revisions,
        TranscriptRenderContext::new(&theme, 40, 0, false),
        |_, _, _, _, _, _| unreachable!(),
    );
    let _ = cache.viewport(0, 0, 10);
    assert_eq!(cache.content_generation(), built);

    revisions[0] += 1;
    cache.invalidate(0);
    cache.prepare(
        &messages,
        &revisions,
        TranscriptRenderContext::new(&theme, 40, 0, false),
        |_, message, theme, width, tick, force_expand| {
            build_lines_for_messages(
                std::slice::from_ref(message),
                theme,
                width,
                tick,
                force_expand,
            )
        },
    );
    assert_ne!(cache.content_generation(), built);
}
```

Add:

```rust
#[test]
fn spinner_only_patch_does_not_change_search_generation() {
    let messages = vec![ChatMessage::ToolCall {
        id: "running".to_string(),
        name: "read".to_string(),
        target: None,
        status: "running".to_string(),
        output: None,
        diff: None,
        kind: None,
        expanded: false,
    }];
    let revisions = vec![1];
    let mut cache = TranscriptRenderCache::default();
    prepare_exact(&mut cache, &messages, &revisions, 40, 0);
    let generation = cache.content_generation();

    prepare_exact(&mut cache, &messages, &revisions, 40, 2);

    assert_eq!(cache.content_generation(), generation);
}

#[test]
fn structural_cache_mutations_bump_generation_only_when_effective() {
    let messages = vec![
        ChatMessage::System("one".to_string()),
        ChatMessage::System("two".to_string()),
    ];
    let revisions = vec![1, 2];
    let mut cache = TranscriptRenderCache::default();
    prepare_exact(&mut cache, &messages, &revisions, 40, 0);

    let initial = cache.content_generation();
    cache.retain(&[true, true]);
    assert_eq!(cache.content_generation(), initial);

    cache.retain(&[false, true]);
    let retained = cache.content_generation();
    assert_ne!(retained, initial);

    cache.truncate(0);
    let truncated = cache.content_generation();
    assert_ne!(truncated, retained);

    cache.clear();
    assert_ne!(cache.content_generation(), truncated);
}
```

Define in the test module:

```rust
fn prepare_exact(
    cache: &mut TranscriptRenderCache,
    messages: &[ChatMessage],
    revisions: &[u64],
    width: usize,
    tick: u64,
) {
    let theme = theme();
    cache.prepare(
        messages,
        revisions,
        TranscriptRenderContext::new(&theme, width, tick, false),
        |_, message, theme, width, tick, force_expand| {
            build_lines_for_messages(
                std::slice::from_ref(message),
                theme,
                width,
                tick,
                force_expand,
            )
        },
    );
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui content_generation_changes_only --lib
cargo test -p orca-tui spinner_only_search_generation --lib
```

Expected: `content_generation` API is missing.

- [ ] **Step 3: Implement monotonic content generation**

Add:

```rust
content_generation: u64,
```

and:

```rust
fn bump_content_generation(&mut self) {
    self.content_generation = self.content_generation.wrapping_add(1);
    if self.content_generation == 0 {
        self.content_generation = 1;
    }
}

pub(crate) fn content_generation(&self) -> u64 {
    self.content_generation
}
```

Rules:

- `prepare` uses `rebuilt_any`; bump once after at least one full entry rebuild;
- successful `patch_spinner` does not set `rebuilt_any`;
- `truncate` bumps only if length decreased;
- `retain` bumps only if an entry was removed/reindexed;
- `clear` preserves the previous generation long enough to bump it after
  resetting fields;
- `reconcile_len` alone does not bump because new empty entries are not
  searchable until prepared.

- [ ] **Step 4: Write failing logical-line coordinate tests**

Add this fixture before the tests:

```rust
fn prepared_search_cache(
    lines_per_message: &[Vec<Line<'static>>],
    width: usize,
) -> TranscriptRenderCache {
    let messages = (0..lines_per_message.len())
        .map(|index| ChatMessage::System(index.to_string()))
        .collect::<Vec<_>>();
    let revisions = (1..=messages.len() as u64).collect::<Vec<_>>();
    let mut cache = TranscriptRenderCache::default();
    let theme = theme();
    cache.prepare(
        &messages,
        &revisions,
        TranscriptRenderContext::new(&theme, width, 0, false),
        |_, message, _, _, _, _| {
            let ChatMessage::System(index) = message else {
                unreachable!();
            };
            lines_per_message[index.parse::<usize>().unwrap()].clone()
        },
    );
    cache
}
```

Add:

```rust
#[test]
fn search_maps_span_and_soft_wrap_matches_to_absolute_rows() {
    let messages = vec![ChatMessage::System("fixture".to_string())];
    let revisions = vec![7];
    let mut cache = TranscriptRenderCache::default();
    let theme = theme();
    cache.prepare(
        &messages,
        &revisions,
        TranscriptRenderContext::new(&theme, 6, 0, false),
        |_, _, _, _, _, _| {
            vec![Line::from(vec![
                Span::styled("alpha ", Style::default().fg(Color::Red)),
                Span::styled("beta", Style::default().fg(Color::Blue)),
            ])]
        },
    );

    let matches = cache.search(0, &SearchQuery::new("alpha beta"));
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].start, SelectionPos { row: 0, col: 0 });
    assert_eq!(matches[0].end, SelectionPos { row: 1, col: 4 });
    assert_eq!(matches[0].line_identity.message_revision, 7);
}

#[test]
fn search_does_not_cross_hard_lines_or_message_boundaries() {
    let cache = prepared_search_cache(
        &[
            vec![Line::from("alpha"), Line::from("beta")],
            vec![Line::from("gamma")],
        ],
        80,
    );
    assert!(cache.search(0, &SearchQuery::new("alpha beta")).is_empty());
    assert!(cache.search(0, &SearchQuery::new("beta gamma")).is_empty());
}

#[test]
fn search_maps_cjk_combining_and_emoji_to_display_columns() {
    let cache = prepared_search_cache(
        &[vec![Line::from("A中e\u{301}👍🏽Z")]],
        80,
    );
    let cjk = cache.search(0, &SearchQuery::new("中"));
    assert_eq!((cjk[0].start.col, cjk[0].end.col), (1, 3));
    let combining = cache.search(0, &SearchQuery::new("e\u{301}"));
    assert_eq!((combining[0].start.col, combining[0].end.col), (3, 4));
    let emoji = cache.search(0, &SearchQuery::new("👍🏽"));
    assert!(emoji[0].end.col > emoji[0].start.col);
}
```

Add:

```rust
#[test]
fn search_coordinates_remain_usize_above_u16_max() {
    let lines = (0..70_000)
        .map(|index| Line::from(format!("line {index}")))
        .collect::<Vec<_>>();
    let cache = prepared_search_cache(&[lines], 80);
    let found = cache.search(0, &SearchQuery::new("line 69999"));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].start.row, 69_999);
}

#[test]
fn search_skips_unprepared_entries_and_keeps_repeated_matches() {
    let mut cache = TranscriptRenderCache::default();
    cache.reconcile_len(1);
    assert!(cache.search(0, &SearchQuery::new("x")).is_empty());

    let cache = prepared_search_cache(&[vec![Line::from("x x x")]], 80);
    assert_eq!(cache.search(0, &SearchQuery::new("x")).len(), 3);
}

#[test]
fn search_skips_flushed_prefix_entries() {
    let cache = prepared_search_cache(
        &[
            vec![Line::from("old target")],
            vec![Line::from("live target")],
        ],
        80,
    );
    let found = cache.search(1, &SearchQuery::new("target"));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].line_identity.message_revision, 2);
}

#[test]
fn search_excludes_spinner_glyph_but_keeps_stable_tool_text() {
    let messages = vec![ChatMessage::ToolCall {
        id: "running".to_string(),
        name: "read".to_string(),
        target: Some("src/lib.rs".to_string()),
        status: "running".to_string(),
        output: None,
        diff: None,
        kind: None,
        expanded: false,
    }];
    let revisions = vec![1];
    let mut cache = TranscriptRenderCache::default();
    prepare_exact(&mut cache, &messages, &revisions, 80, 0);

    assert!(cache.search(0, &SearchQuery::new(SPINNER_FRAMES[0])).is_empty());
    assert_eq!(cache.search(0, &SearchQuery::new("read")).len(), 1);
    assert_eq!(cache.search(0, &SearchQuery::new("running")).len(), 1);
}
```

- [ ] **Step 5: Run RED**

```sh
cargo test -p orca-tui search_maps_span_and_soft_wrap --lib
cargo test -p orca-tui search_does_not_cross_hard --lib
cargo test -p orca-tui search_maps_cjk_combining --lib
```

Expected: cache search API and coordinate model are missing.

- [ ] **Step 6: Implement one-logical-line-at-a-time search**

Import:

```rust
use crate::transcript_search::{
    SearchQuery, TranscriptLineIdentity, TranscriptSearchMatch,
};
```

Add a private builder:

```rust
struct SearchableLogicalLine {
    text: String,
    boundaries: Vec<(usize, SelectionPos)>,
}
```

For each `CachedMessage.wrapped_lines[line_index]`:

1. compute the absolute first row from message and line cumulative heights;
2. append every visible row in order;
3. append `wrap_gaps[row]` between soft-wrapped rows;
4. record every UTF-8 character boundary and its absolute display position;
5. for an animated tool first line, omit only the spinner glyph from
   searchable text and preserve the remaining label/status;
6. call `SearchQuery::find_ranges`;
7. resolve exact start/end boundaries with binary search;
8. emit ordered `TranscriptSearchMatch` values.

Use the cached message revision and line index for `TranscriptLineIdentity`.
Do not concatenate messages or hard lines.

Add:

```rust
pub(crate) fn search(
    &self,
    first_retained_message: usize,
    query: &SearchQuery,
) -> Vec<TranscriptSearchMatch>;
```

Under `#[cfg(test)]`, track logical lines visited with `Cell<usize>` and expose:

```rust
pub(crate) fn last_search_lines_visited(&self) -> usize;
```

- [ ] **Step 7: Write failing reveal-offset tests**

Add:

```rust
#[test]
fn reveal_offset_keeps_visible_match_and_uses_nearest_edge() {
    let cache = prepared_search_cache(
        &(0..30)
            .map(|index| vec![Line::from(format!("line {index}"))])
            .collect::<Vec<_>>(),
        80,
    );
    let visible = TranscriptSearchMatch::new(
        SelectionPos { row: 12, col: 0 },
        SelectionPos { row: 12, col: 4 },
        TranscriptLineIdentity {
            message_revision: 13,
            line_index: 0,
        },
        0..4,
    );
    assert_eq!(cache.reveal_offset(0, 10, 5, &visible), 10);

    let above = TranscriptSearchMatch::new(
        SelectionPos { row: 3, col: 0 },
        SelectionPos { row: 3, col: 4 },
        TranscriptLineIdentity {
            message_revision: 4,
            line_index: 0,
        },
        0..4,
    );
    assert_eq!(cache.reveal_offset(0, 10, 5, &above), 3);

    let below = TranscriptSearchMatch::new(
        SelectionPos { row: 20, col: 0 },
        SelectionPos { row: 20, col: 4 },
        TranscriptLineIdentity {
            message_revision: 21,
            line_index: 0,
        },
        0..4,
    );
    assert_eq!(cache.reveal_offset(0, 10, 5, &below), 16);
}
```

Add this production constructor in Task 1 and use it in cache tests:

```rust
impl TranscriptSearchMatch {
    pub(crate) fn new(
        start: SelectionPos,
        end: SelectionPos,
        line_identity: TranscriptLineIdentity,
        byte_range: Range<usize>,
    ) -> Self {
        Self {
            start,
            end,
            line_identity,
            byte_range,
        }
    }
}
```

- [ ] **Step 8: Implement reveal offset and run GREEN**

Add:

```rust
pub(crate) fn reveal_offset(
    &self,
    first_retained_message: usize,
    current_offset: usize,
    visible_height: usize,
    found: &TranscriptSearchMatch,
) -> usize;
```

Treat `end` as exclusive. If `end.row > start.row && end.col == 0`, the last
covered row is `end.row - 1`; otherwise it is `end.row`.

Use saturating arithmetic and clamp to the same max scroll as `viewport`.

```sh
cargo test -p orca-tui search_maps --lib
cargo test -p orca-tui search_does_not_cross --lib
cargo test -p orca-tui reveal_offset --lib
cargo test -p orca-tui content_generation --lib
cargo test -p orca-tui transcript_view --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 9: Commit**

```sh
git add crates/orca-tui/src/transcript_view.rs crates/orca-tui/src/transcript_search.rs
git commit -m "feat(tui): search cached transcript lines" \
  -m "Map smart-case rendered-text matches to absolute wrapped coordinates and expose bounded reveal offsets without rebuilding cache entries." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 3: Integrate Search Lifecycle with AppState

**Files:**
- Modify: `crates/orca-tui/src/types.rs`

- [ ] **Step 1: Write failing lifecycle tests**

Add this helper to `types.rs` tests:

```rust
fn prepare_transcript_cache(state: &mut AppState, width: usize) {
    let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
    let messages = &state.messages;
    let revisions = &state.message_revisions;
    state.transcript_render_cache.prepare(
        messages,
        revisions,
        TranscriptRenderContext::new(&theme, width, 0, false),
        |_, message, theme, width, tick, force_expand| {
            crate::ui::build_lines_for_messages(
                std::slice::from_ref(message),
                theme,
                width,
                tick,
                force_expand,
            )
        },
    );
}
```

Add:

```rust
#[test]
fn opening_search_preserves_scroll_and_refresh_selects_viewport_match() {
    let mut state = state();
    state.push_message(ChatMessage::Assistant(
        "first hit\nsecond\nthird hit".to_string(),
    ));
    prepare_transcript_cache(&mut state, 20);
    state.scroll_offset = 1;
    state.viewport_base_row = 1;
    state.open_transcript_search();
    state.replace_transcript_search_query("hit");
    state.refresh_transcript_search();

    assert!(state.transcript_search.open);
    assert_eq!(state.scroll_offset, 1);
    assert_eq!(
        state.transcript_search.active_match().map(|found| found.start.row),
        Some(2)
    );
}

#[test]
fn explicit_search_jump_disables_follow_and_reveals_match() {
    let mut state = state();
    for index in 0..30 {
        state.push_message(ChatMessage::System(format!("line {index} target")));
    }
    prepare_transcript_cache(&mut state, 80);
    state.visible_height = 5;
    state.scroll_offset = 20;
    state.auto_scroll = true;
    state.open_transcript_search();
    state.replace_transcript_search_query("target");
    state.refresh_transcript_search();

    state.search_next();

    assert!(!state.auto_scroll);
    let active = state.transcript_search.active_match().unwrap();
    assert!(active.start.row >= state.scroll_offset);
    assert!(active.start.row < state.scroll_offset + state.visible_height);
}

#[test]
fn clear_resets_search_but_other_mutations_reconcile_lazily() {
    let mut state = state();
    state.open_transcript_search();
    state.replace_transcript_search_query("x");
    state.push_message(ChatMessage::System("x".to_string()));
    prepare_transcript_cache(&mut state, 40);
    state.refresh_transcript_search();
    assert_eq!(state.transcript_search.match_count(), 1);

    state.truncate_messages(0);
    state.refresh_transcript_search();
    assert_eq!(state.transcript_search.match_count(), 0);
    assert_eq!(state.transcript_search.query(), "x");

    state.clear_messages();
    assert!(!state.transcript_search.open);
    assert_eq!(state.transcript_search.query(), "");
}
```

Add:

```rust
#[test]
fn append_and_retain_preserve_active_revision_identity() {
    let mut state = state();
    state.push_message(ChatMessage::System("remove".to_string()));
    state.push_message(ChatMessage::System("target".to_string()));
    prepare_transcript_cache(&mut state, 40);
    state.open_transcript_search();
    state.replace_transcript_search_query("target");
    state.refresh_transcript_search();
    let identity = state
        .transcript_search
        .active_match()
        .unwrap()
        .line_identity;

    state.push_message(ChatMessage::System("later target".to_string()));
    prepare_transcript_cache(&mut state, 40);
    state.refresh_transcript_search();
    assert_eq!(
        state.transcript_search.active_match().unwrap().line_identity,
        identity
    );

    state.retain_messages(
        |message| !matches!(message, ChatMessage::System(text) if text == "remove"),
    );
    prepare_transcript_cache(&mut state, 40);
    state.refresh_transcript_search();
    assert_eq!(
        state.transcript_search.active_match().unwrap().line_identity,
        identity
    );
}

#[test]
fn removal_chooses_nearest_following_match_and_open_does_not_disable_follow() {
    let mut state = state();
    for text in ["target one", "middle", "target two"] {
        state.push_message(ChatMessage::System(text.to_string()));
    }
    prepare_transcript_cache(&mut state, 40);
    state.auto_scroll = true;
    state.open_transcript_search();
    assert!(state.auto_scroll);
    state.replace_transcript_search_query("target");
    state.refresh_transcript_search();
    let first_revision = state.transcript_search.active_match().unwrap().line_identity;

    state.retain_messages(|message| {
        !matches!(message, ChatMessage::System(text) if text == "target one")
    });
    prepare_transcript_cache(&mut state, 40);
    state.refresh_transcript_search();
    assert_ne!(
        state.transcript_search.active_match().unwrap().line_identity,
        first_revision
    );
    assert_eq!(state.transcript_search.match_count(), 1);
}

#[test]
fn approval_closes_search_but_preserves_query() {
    let mut state = state();
    state.open_transcript_search();
    state.replace_transcript_search_query("target");
    state.update(TuiEvent::ApprovalNeeded {
        key: interaction_key(TuiInteractionKind::Approval, "approval"),
        tool: "bash".to_string(),
        target: None,
        preview: None,
    });

    assert!(!state.transcript_search.open);
    assert_eq!(state.transcript_search.query(), "target");
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui opening_search_preserves_scroll --lib
cargo test -p orca-tui explicit_search_jump_disables_follow --lib
cargo test -p orca-tui clear_resets_search --lib
```

Expected: `AppState` search ownership and methods are missing.

- [ ] **Step 3: Add state ownership and refresh**

Import `TranscriptSearchState` and add:

```rust
pub(crate) transcript_search: TranscriptSearchState,
```

Initialize with `TranscriptSearchState::default()`.

Add:

```rust
pub(crate) fn open_transcript_search(&mut self) {
    if self.panel_mode == PanelMode::Conversation
        && matches!(
            self.status,
            AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
        )
    {
        self.transcript_search.open_new();
    }
}

pub(crate) fn close_transcript_search(&mut self) {
    self.transcript_search.close();
}

pub(crate) fn replace_transcript_search_query(&mut self, query: &str) {
    self.transcript_search.replace_query(query);
}

pub(crate) fn refresh_transcript_search(&mut self) {
    let generation = self.transcript_render_cache.content_generation();
    let viewport_base = self.viewport_base_row;
    let live_start = self.flushed_count.min(self.messages.len());
    let cache = &self.transcript_render_cache;
    self.transcript_search
        .refresh_with(generation, viewport_base, |query| {
            cache.search(live_start, query)
        });
}
```

Add next/previous methods that:

1. refresh;
2. change active index without scanning;
3. compute offset with `reveal_offset`;
4. set `auto_scroll = false`;
5. leave offset unchanged when no match exists.

Search uses `flushed_count` as `first_retained_message`.

- [ ] **Step 4: Reconcile mutation and modal paths**

Rules:

- `clear_messages` calls `transcript_search.reset()`;
- `replace_messages`, `truncate_messages`, `retain_messages`, and backtrack do
  not erase the query; cache generation makes results stale;
- `ApprovalNeeded` and `PermissionApprovalNeeded` close search input before
  showing the modal but retain query/matches;
- SessionPicker and Setup never open transcript search;
- clear-screen behavior is covered through `clear_messages`;
- selection invalidation remains independent.

- [ ] **Step 5: Run GREEN and commit**

```sh
cargo test -p orca-tui transcript_search --lib
cargo test -p orca-tui search_jump --lib
cargo test -p orca-tui search_reconcile --lib
cargo test -p orca-tui types::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/types.rs crates/orca-tui/src/transcript_search.rs
git commit -m "feat(tui): integrate transcript search state" \
  -m "Refresh rendered-text matches by cache generation, preserve active results across mutations, and reveal explicit jumps without disturbing message revisions." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 4: Route Shortcuts, Search Input, and Paste

**Files:**
- Modify: `crates/orca-tui/src/shortcuts.rs`
- Modify: `crates/orca-tui/src/key_event_actions.rs`
- Modify: `crates/orca-tui/src/global_actions.rs`
- Modify: `crates/orca-tui/src/input_event_actions.rs`

- [ ] **Step 1: Write failing shortcut tests**

Add:

```rust
#[test]
fn global_ctrl_f_opens_transcript_search() {
    assert_eq!(
        global_shortcut(key(KeyCode::Char('f'), KeyModifiers::CONTROL)),
        Some(GlobalShortcut::OpenTranscriptSearch)
    );
}

#[test]
fn search_shortcut_hint_is_backed_by_a_binding() {
    assert!(shortcut_hints().any(|hint| {
        hint.scope == ShortcutScope::Global
            && hint.keys == "ctrl+f"
            && hint.has_registered_binding
    }));
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui global_ctrl_f_opens --lib
```

Expected: shortcut variant and binding are missing.

- [ ] **Step 3: Register and handle open-search action**

Add:

```rust
GlobalShortcut::OpenTranscriptSearch
```

with:

```rust
KeyBinding::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
```

and hint:

```rust
ShortcutHint {
    scope: ShortcutScope::Global,
    keys: "ctrl+f",
    action: "find in transcript",
},
```

In `handle_global_shortcut`:

```rust
GlobalShortcut::OpenTranscriptSearch => state.open_transcript_search(),
```

- [ ] **Step 4: Write failing active-search routing tests**

In `key_event_actions.rs`, add:

```rust
#[test]
fn active_search_key_table_edits_closes_and_navigates_without_fallthrough() {
    let mut state = state_with_search_matches();
    let cases = [
        (
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            "alphaz",
        ),
        (
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            "alphaz",
        ),
        (
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            "alphz",
        ),
        (
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            "",
        ),
    ];
    for (key, expected_query) in cases {
        assert_eq!(
            handle_transcript_search_key(key, &mut state),
            SearchKeyFlow::Handled
        );
        assert_eq!(state.transcript_search.query(), expected_query);
        assert!(state.transcript_search.open);
    }

    state.replace_transcript_search_query("alpha");
    state.refresh_transcript_search();
    let first = state.transcript_search.active_ordinal();
    handle_transcript_search_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut state,
    );
    assert_ne!(state.transcript_search.active_ordinal(), first);
    handle_transcript_search_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        &mut state,
    );
    assert_eq!(state.transcript_search.active_ordinal(), first);

    handle_transcript_search_key(
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        &mut state,
    );
    assert_ne!(state.transcript_search.active_ordinal(), first);
    handle_transcript_search_key(
        KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ),
        &mut state,
    );
    assert_eq!(state.transcript_search.active_ordinal(), first);

    handle_transcript_search_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut state,
    );
    assert!(!state.transcript_search.open);
}

#[test]
fn search_ctrl_g_precedes_running_interrupt_and_ctrl_c_stays_global() {
    let (action_tx, action_rx) = crossbeam_channel::unbounded();
    let mut state = state_with_search_matches();
    state.enter_running();
    let operation = TestOperationInterrupt::default();
    let mut config = config();
    let shared = Arc::new(Mutex::new(config.clone()));

    let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert!(matches!(
        handle_key_event_preflight(
            ctrl_g,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            || Ok(()),
        )
        .unwrap(),
        KeyEventFlow::Continue
    ));
    assert_eq!(operation.call_count(), 0);
    assert!(action_rx.try_recv().is_err());

    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    handle_key_event_preflight(
        ctrl_c,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(operation.call_count(), 1);
    assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
}

#[test]
fn release_and_unknown_search_keys_are_handled_without_query_mutation() {
    let mut state = state_with_search_matches();
    let before = state.transcript_search.query().to_string();
    let release = KeyEvent {
        kind: KeyEventKind::Release,
        ..KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
    };
    assert_eq!(
        handle_transcript_search_key(release, &mut state),
        SearchKeyFlow::NotSearch
    );
    assert_eq!(
        handle_transcript_search_key(
            KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE),
            &mut state,
        ),
        SearchKeyFlow::Handled
    );
    assert_eq!(state.transcript_search.query(), before);
}
```

Define in the test module:

```rust
fn state_with_search_matches() -> AppState {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut state = AppState::new(
        tx,
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.push_message(ChatMessage::System("alpha one".to_string()));
    state.push_message(ChatMessage::System("alpha two".to_string()));
    let theme = Theme::named(ThemeName::Dark);
    let messages = &state.messages;
    let revisions = &state.message_revisions;
    state.transcript_render_cache.prepare(
        messages,
        revisions,
        TranscriptRenderContext::new(&theme, 40, 0, false),
        |_, message, theme, width, tick, force_expand| {
            build_lines_for_messages(
                std::slice::from_ref(message),
                theme,
                width,
                tick,
                force_expand,
            )
        },
    );
    state.open_transcript_search();
    state.replace_transcript_search_query("alpha");
    state.refresh_transcript_search();
    state
}
```

Use a focused function:

```rust
pub(crate) enum SearchKeyFlow {
    NotSearch,
    Handled,
}

pub(crate) fn handle_transcript_search_key(
    key: KeyEvent,
    state: &mut AppState,
) -> SearchKeyFlow;
```

- [ ] **Step 5: Run RED**

```sh
cargo test -p orca-tui search_esc_closes_before --lib
cargo test -p orca-tui search_ctrl_g_navigates --lib
cargo test -p orca-tui search_printable_input --lib
```

- [ ] **Step 6: Implement routing priority**

In preflight:

1. reject non-press/non-repeat;
2. resolve and handle only `GlobalShortcut::Cancel`;
3. if search is open and input is allowed, call
   `handle_transcript_search_key` and consume handled input;
4. resolve other global shortcuts, including `Ctrl+F`;
5. continue existing selection, BackTab, and panel behavior.

Unknown keys while search owns input are consumed rather than forwarded to the
composer.

Implement query editing through `TranscriptSearchState` methods. After a query
mutation, call `state.refresh_transcript_search()`. Navigation calls AppState
jump methods.

`handle_key_event_preflight` must not borrow `AppState.transcript_search` and
`AppState` mutably at the same time. Put query-edit methods that need refresh
on `AppState`, or return a small `SearchInputEffect` and apply refresh/jump
after the search-state borrow ends.

- [ ] **Step 7: Write failing paste tests**

Add:

```rust
#[test]
fn search_paste_updates_query_without_touching_composer() {
    let mut state = state();
    state.open_transcript_search();
    let config = config();
    let mut textarea = TextArea::from(["composer"]);

    assert!(handle_paste_event(
        &Event::Paste("one\r\ntwo".to_string()),
        &mut state,
        &config,
        &mut textarea,
    ));

    assert_eq!(state.transcript_search.query(), "one two");
    assert_eq!(textarea.lines(), &["composer".to_string()]);
}
```

- [ ] **Step 8: Implement paste routing and run GREEN**

At the start of the `Event::Paste` path:

```rust
if state.transcript_search.open {
    state.transcript_search.insert_paste(pasted);
    state.refresh_transcript_search();
    return true;
}
```

Do not add large-paste placeholders to search.

```sh
cargo test -p orca-tui search_shortcut --lib
cargo test -p orca-tui transcript_search_key --lib
cargo test -p orca-tui search_paste --lib
cargo test -p orca-tui key_event_actions --lib
cargo test -p orca-tui input_event_actions --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 9: Commit**

```sh
git add crates/orca-tui/src/shortcuts.rs \
  crates/orca-tui/src/key_event_actions.rs \
  crates/orca-tui/src/global_actions.rs \
  crates/orca-tui/src/input_event_actions.rs
git commit -m "feat(tui): route transcript search input" \
  -m "Open search with Ctrl+F, prioritize query editing and match navigation, and keep paste and Ctrl+C ownership explicit." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 5: Add Capability-Safe Search Styles and Visible Overlay

**Files:**
- Modify: `crates/orca-tui/src/theme.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/selection.rs`

- [ ] **Step 1: Write failing theme tests**

Add two colors to `Theme` only after tests are red:

```rust
search_match_bg: Color,
search_match_active_bg: Color,
```

Tests:

```rust
#[test]
fn search_styles_are_distinct_and_capability_safe() {
    for name in [
        ThemeName::Dark,
        ThemeName::Light,
        ThemeName::Solarized,
        ThemeName::Catppuccin,
    ] {
        for level in [
            TerminalColorLevel::TrueColor,
            TerminalColorLevel::Ansi256,
            TerminalColorLevel::Ansi16,
            TerminalColorLevel::Monochrome,
        ] {
            let theme = Theme::resolve(
                name,
                TerminalProfile {
                    background: TerminalBackground::Unknown,
                    color_level: level,
                },
            );
            assert_ne!(theme.search_match_style(), theme.search_match_active_style());
            if level == TerminalColorLevel::Monochrome {
                assert!(theme
                    .search_match_style()
                    .add_modifier
                    .contains(Modifier::UNDERLINED));
                assert!(theme
                    .search_match_active_style()
                    .add_modifier
                    .contains(Modifier::REVERSED));
            }
        }
    }
}
```

Extend `theme_colors` from `[Color; 20]` to `[Color; 22]` by appending
`search_match_bg` and `search_match_active_bg`. Append the two exact values to
every array in `named_themes_preserve_exact_truecolor_palettes`.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui search_styles_are_distinct --lib
```

- [ ] **Step 3: Implement theme styles**

Use exact truecolor backgrounds:

| Theme | Inactive | Active |
|---|---|---|
| Dark | `Rgb(78, 67, 31)` | `Rgb(77, 107, 254)` |
| Light | `Rgb(255, 235, 153)` | `Rgb(166, 188, 255)` |
| Solarized | `Rgb(88, 73, 0)` | `Rgb(38, 139, 210)` |
| Catppuccin | `Rgb(88, 91, 112)` | `Rgb(137, 180, 250)` |

Adapt both colors in `Theme::resolve`.

Add:

```rust
pub(crate) fn search_match_style(self) -> Style {
    match self.color_level {
        TerminalColorLevel::Monochrome => {
            Style::default().add_modifier(Modifier::UNDERLINED)
        }
        _ => Style::default().bg(self.search_match_bg),
    }
}

pub(crate) fn search_match_active_style(self) -> Style {
    match self.color_level {
        TerminalColorLevel::Monochrome => Style::default()
            .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        _ => Style::default()
            .bg(self.search_match_active_bg)
            .add_modifier(Modifier::BOLD),
    }
}
```

- [ ] **Step 4: Generalize the range-overlay helper with RED tests**

Rename or wrap `apply_selection_to_line` as:

```rust
pub(crate) fn apply_style_to_line_range(
    line: Line<'static>,
    col_start: usize,
    col_end: Option<usize>,
    overlay: Style,
) -> Line<'static>;
```

Keep `apply_selection_to_line` as a compatibility wrapper.

Add:

```rust
#[test]
fn generic_range_overlay_preserves_foregrounds_and_selection_wins() {
    let search_bg = Color::Rgb(78, 67, 31);
    let selection_bg = Color::Rgb(46, 62, 132);
    let line = Line::from(vec![
        Span::styled("let", Style::default().fg(Color::Magenta)),
        Span::styled(" 中", Style::default().fg(Color::Green)),
    ]);

    let searched = apply_style_to_line_range(
        line,
        0,
        Some(3),
        Style::default().bg(search_bg),
    );
    assert!(searched.spans.iter().all(|span| span.style.fg.is_some()));
    assert!(searched
        .spans
        .iter()
        .any(|span| span.style.bg == Some(search_bg)));

    let selected = apply_style_to_line_range(
        searched,
        1,
        Some(4),
        Style::default().bg(selection_bg),
    );
    assert!(selected
        .spans
        .iter()
        .any(|span| span.style.bg == Some(selection_bg)));
}

#[test]
fn generic_range_overlay_uses_wide_character_leading_columns() {
    let highlighted = apply_style_to_line_range(
        Line::from("A中B"),
        1,
        Some(3),
        Style::default().bg(Color::Blue),
    );
    assert!(highlighted.spans.iter().any(|span| {
        span.content == "中" && span.style.bg == Some(Color::Blue)
    }));
}
```

- [ ] **Step 5: Write failing search-overlay tests**

Add:

```rust
#[test]
fn search_overlay_styles_only_visible_matches_and_selection_wins() {
    let theme = Theme::named(ThemeName::Dark);
    let lines = vec![Line::from("alpha beta"), Line::from("tail")];
    let mut search = TranscriptSearchState::default();
    search.open_new();
    search.replace_query("alpha");
    search.refresh_with(1, 0, |_| {
        vec![
            TranscriptSearchMatch::new(
                SelectionPos { row: 0, col: 0 },
                SelectionPos { row: 0, col: 5 },
                TranscriptLineIdentity {
                    message_revision: 1,
                    line_index: 0,
                },
                0..5,
            ),
            TranscriptSearchMatch::new(
                SelectionPos { row: 100, col: 0 },
                SelectionPos { row: 100, col: 4 },
                TranscriptLineIdentity {
                    message_revision: 2,
                    line_index: 0,
                },
                0..4,
            ),
        ]
    });
    let mut selection = TranscriptSelection::unit(
        SelectionGranularity::Cell,
        SelectionPos { row: 0, col: 1 },
        SelectionPos { row: 0, col: 2 },
    );
    selection.dragging = false;

    let overlaid = apply_transcript_overlays(lines, &search, Some(selection), 0, &theme);
    assert_eq!(search.visible_matches(0, 2).count(), 1);
    assert!(overlaid[0].spans.iter().any(|span| {
        span.style.bg == theme.search_match_active_style().bg
    }));
    assert!(overlaid[0].spans.iter().any(|span| {
        span.style.bg == theme.selection_style().bg
    }));
}
```

Add this production iterator in Task 1:

```rust
pub(crate) fn visible_matches(
    &self,
    start_row: usize,
    end_row: usize,
) -> impl Iterator<Item = (usize, &TranscriptSearchMatch)> {
    self.matches
        .iter()
        .enumerate()
        .skip_while(move |(_, found)| found.end.row < start_row)
        .take_while(move |(_, found)| found.start.row < end_row)
}
```

- [ ] **Step 6: Implement search then selection overlay**

Replace `apply_selection_overlay` with:

```rust
fn apply_transcript_overlays(
    mut lines: Vec<Line<'static>>,
    search: &TranscriptSearchState,
    selection: Option<TranscriptSelection>,
    base_row: usize,
    theme: &Theme,
) -> Vec<Line<'static>>;
```

For visible search matches:

1. patch inactive matches;
2. patch the active match with active style;
3. patch mouse selection last.

Use ordered match partitioning so off-screen matches do not enter span
splitting.

- [ ] **Step 7: Run GREEN and commit**

```sh
cargo test -p orca-tui search_styles --lib
cargo test -p orca-tui search_overlay --lib
cargo test -p orca-tui selection --lib
cargo test -p orca-tui ui::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/theme.rs crates/orca-tui/src/ui.rs crates/orca-tui/src/selection.rs
git commit -m "feat(tui): highlight transcript search matches" \
  -m "Apply capability-safe active and inactive match styles after viewport materialization while preserving syntax colors and mouse-selection priority." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 6: Render the Search Bar and Hardware Cursor

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Write failing layout tests**

Update `main_layout` intended signature:

```rust
fn main_layout(
    area: Rect,
    goal_height: u16,
    plan_height: u16,
    activity_height: u16,
    search_height: u16,
    input_height: u16,
) -> Rc<[Rect]>;
```

Add:

```rust
#[test]
fn open_search_reserves_one_row_without_squeezing_composer_or_status() {
    let area = Rect::new(0, 0, 80, 20);
    let chunks = main_layout(area, 0, 0, 2, 1, 3);
    assert_eq!(chunks[4].height, 1);
    assert_eq!(chunks[5].height, 3);
    assert_eq!(chunks[6].height, 1);
    assert_eq!(chunks[4].bottom(), chunks[5].y);
}

#[test]
fn compact_search_layout_preserves_fixed_chrome_before_transcript() {
    let chunks = main_layout(Rect::new(0, 0, 20, 6), 0, 0, 0, 1, 3);
    assert_eq!(chunks[1].height, 1);
    assert_eq!(chunks[4].height, 1);
    assert_eq!(chunks[5].height, 3);
    assert_eq!(chunks[6].height, 1);
}
```

- [ ] **Step 2: Write failing completed-frame search-bar tests**

Add:

```rust
#[test]
fn search_frame_shows_query_count_and_hardware_cursor() {
    let mut state = test_state();
    state.push_message(ChatMessage::Assistant(
        "alpha beta alpha".to_string(),
    ));
    state.open_transcript_search();
    state.replace_transcript_search_query("alpha");
    let theme = Theme::named(ThemeName::Dark);
    let textarea = TextArea::default();
    let mut terminal =
        Terminal::new(TestBackend::new(50, 12)).expect("test backend");

    terminal
        .draw(|frame| render(frame, &mut state, &textarea, &theme))
        .expect("draw");

    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("Find:"));
    assert!(rendered.contains("alpha"));
    assert!(rendered.contains("1/2"));
    let cursor = terminal.get_cursor_position().expect("hardware cursor");
    assert!(state.search_area.expect("search area").contains(cursor));
}

#[test]
fn search_frame_status_matrix_and_zero_counts_are_stable() {
    for status in [
        AppStatus::Idle,
        AppStatus::Running,
        AppStatus::WaitingUserInput,
    ] {
        let mut state = test_state();
        state.set_status(status);
        state.push_message(ChatMessage::System("alpha".to_string()));
        state.open_transcript_search();
        let theme = Theme::named(ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal =
            Terminal::new(TestBackend::new(24, 8)).expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("empty query draw");
        let empty = format!("{:?}", terminal.backend().buffer());
        assert!(empty.contains("0/0"), "{status:?}: {empty}");

        state.replace_transcript_search_query("missing");
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("missing query draw");
        let missing = format!("{:?}", terminal.backend().buffer());
        assert!(missing.contains("0/0"), "{status:?}: {missing}");
        assert!(state.search_area.is_some());
    }
}

#[test]
fn narrow_search_frame_keeps_count_and_cursor_segment_without_composer_cursor() {
    let mut state = test_state();
    state.push_message(ChatMessage::System(
        "long-query-tail long-query-tail".to_string(),
    ));
    state.open_transcript_search();
    state.replace_transcript_search_query("prefix-long-query-tail");
    let theme = Theme::named(ThemeName::Dark);
    let textarea = TextArea::from(["COMPOSER_CURSOR_SENTINEL"]);
    let mut terminal =
        Terminal::new(TestBackend::new(18, 8)).expect("test backend");

    terminal
        .draw(|frame| render(frame, &mut state, &textarea, &theme))
        .expect("narrow draw");

    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("0/0"));
    assert!(rendered.contains("tail"));
    let cursor = terminal.get_cursor_position().expect("search cursor");
    assert!(state.search_area.unwrap().contains(cursor));
    assert!(!state.input_area.unwrap().contains(cursor));
}

#[test]
fn shortcuts_and_approval_hide_search_hardware_cursor() {
    let theme = Theme::named(ThemeName::Dark);
    let textarea = TextArea::default();
    let mut shortcuts = test_state();
    shortcuts.open_transcript_search();
    shortcuts.show_shortcuts = true;
    let mut terminal =
        Terminal::new(TestBackend::new(50, 12)).expect("test backend");
    terminal
        .draw(|frame| render(frame, &mut shortcuts, &textarea, &theme))
        .expect("shortcuts draw");
    assert!(!terminal.backend().is_cursor_visible());

    let mut approval = test_state();
    approval.open_transcript_search();
    approval.set_status(AppStatus::WaitingApproval);
    terminal
        .draw(|frame| render(frame, &mut approval, &textarea, &theme))
        .expect("approval draw");
    assert!(!terminal.backend().is_cursor_visible());
}
```

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-tui open_search_reserves_one_row --lib
cargo test -p orca-tui search_frame_shows_query_count --lib
```

- [ ] **Step 4: Implement search row geometry**

Add to `AppState`:

```rust
pub(crate) search_area: Option<Rect>,
```

Reset it at frame start.

In `render`:

- `search_height = usize::from(search_visible(state)) as u16`;
- add the search row immediately before composer;
- suppress main composer hardware cursor while search is open;
- call `render_search_bar` before `render_input`;
- do not render slash/mention popups while search is open.

Use:

```rust
fn search_visible(state: &AppState) -> bool {
    state.transcript_search.open
        && state.panel_mode == PanelMode::Conversation
        && matches!(
            state.status,
            AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
        )
}
```

- [ ] **Step 5: Implement the shared search textarea surface**

`render_search_bar`:

1. renders a compact ` Find: ` or `F:` prefix;
2. reserves the right-aligned ` current/total ` count;
3. builds `TextArea::from([query])`;
4. moves its cursor from the UTF-8 byte cursor to a character column;
5. uses no block and one visual row;
6. uses `render_textarea_surface` for content and hardware cursor;
7. hides hardware cursor when shortcuts/modal own the foreground.

The query field receives remaining width. If it has zero width, render only
prefix/count and do not set a cursor.

- [ ] **Step 6: Refresh search after cache prepare**

In `render_live_messages`, scope the mutable cache borrow around `prepare`,
release it, then refresh search, then borrow the cache immutably for
`viewport`:

```rust
{
    let cache = &mut state.transcript_render_cache;
    cache.prepare(
        messages,
        revisions,
        TranscriptRenderContext::new(theme, width, state.tick, false),
        build_message,
    );
}
state.refresh_transcript_search();
let viewport = state.transcript_render_cache.viewport(
    live_start,
    requested_scroll,
    visible_height,
);
```

Then call `apply_transcript_overlays`.

The empty welcome-screen path refreshes to zero transcript matches without
searching welcome text.

- [ ] **Step 7: Run GREEN and commit**

```sh
cargo test -p orca-tui search_frame --lib
cargo test -p orca-tui search_cursor --lib
cargo test -p orca-tui overflowing_transcript --lib
cargo test -p orca-tui ui::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/ui.rs crates/orca-tui/src/types.rs
git commit -m "feat(tui): render transcript search bar" \
  -m "Reserve fixed search chrome, position the terminal cursor on the query, and refresh rendered-text matches before viewport overlays." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 7: Add Vim `/`, `n`, and `N` Search Intents

**Files:**
- Modify: `crates/orca-tui/src/vim.rs`
- Modify: `crates/orca-tui/src/status_key_actions.rs`

- [ ] **Step 1: Write failing pure Vim-intent tests**

Add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimTranscriptSearchIntent {
    Open,
    Next,
    Previous,
}

#[test]
fn vim_normal_mode_resolves_transcript_search_intents() {
    let state = VimState::new(true);
    assert_eq!(
        state.transcript_search_intent(KeyCode::Char('/')),
        Some(VimTranscriptSearchIntent::Open)
    );
    assert_eq!(
        state.transcript_search_intent(KeyCode::Char('n')),
        Some(VimTranscriptSearchIntent::Next)
    );
    assert_eq!(
        state.transcript_search_intent(KeyCode::Char('N')),
        Some(VimTranscriptSearchIntent::Previous)
    );
}

#[test]
fn vim_insert_and_visual_modes_do_not_resolve_search_intents() {
    let mut state = VimState::new(true);
    state.mode = VimMode::Insert;
    assert_eq!(state.transcript_search_intent(KeyCode::Char('/')), None);
    state.mode = VimMode::Visual;
    assert_eq!(state.transcript_search_intent(KeyCode::Char('n')), None);
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui vim_normal_mode_resolves_transcript --lib
```

- [ ] **Step 3: Implement the pure resolver**

Import `crossterm::event::KeyCode` in `vim.rs`.

```rust
pub(crate) fn transcript_search_intent(
    &self,
    key: KeyCode,
) -> Option<VimTranscriptSearchIntent> {
    if !self.enabled || self.mode != VimMode::Normal {
        return None;
    }
    match key {
        KeyCode::Char('/') => Some(VimTranscriptSearchIntent::Open),
        KeyCode::Char('n') => Some(VimTranscriptSearchIntent::Next),
        KeyCode::Char('N') => Some(VimTranscriptSearchIntent::Previous),
        _ => None,
    }
}
```

- [ ] **Step 4: Write failing status-routing tests**

Add:

```rust
#[test]
fn vim_slash_opens_search_in_every_conversation_status_without_composer_edit() {
    for status in [
        AppStatus::Idle,
        AppStatus::Running,
        AppStatus::WaitingUserInput,
    ] {
        let (mut state, mut config, shared, action_tx, preloaded, operation) =
            status_harness(status);
        let mut textarea = TextArea::from(["draft"]);
        let mut vim = VimState::new(true);
        let theme = Theme::named(ThemeName::Dark);
        let key = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);

        handle_status_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &preloaded,
            &mut textarea,
            &mut vim,
            &theme,
            None,
            || Ok(()),
        )
        .unwrap();

        assert!(state.transcript_search.open, "{status:?}");
        assert_eq!(textarea.lines(), &["draft".to_string()]);
        assert_eq!(operation.call_count(), 0);
    }
}

#[test]
fn vim_n_and_shift_n_navigate_closed_search_but_no_query_falls_through() {
    let (mut state, mut config, shared, action_tx, preloaded, operation) =
        status_harness(AppStatus::Running);
    prepare_two_search_matches(&mut state);
    state.close_transcript_search();
    let mut textarea = TextArea::from(["draft"]);
    let mut vim = VimState::new(true);
    let theme = Theme::named(ThemeName::Dark);
    let first = state.transcript_search.active_ordinal();

    for code in [KeyCode::Char('n'), KeyCode::Char('N')] {
        let key = KeyEvent::new(code, KeyModifiers::NONE);
        handle_status_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &preloaded,
            &mut textarea,
            &mut vim,
            &theme,
            None,
            || Ok(()),
        )
        .unwrap();
        if code == KeyCode::Char('n') {
            assert_ne!(state.transcript_search.active_ordinal(), first);
        } else {
            assert_eq!(state.transcript_search.active_ordinal(), first);
        }
    }
    assert_eq!(operation.call_count(), 0);

    state.transcript_search.reset();
    let key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
    handle_status_key(
        &Event::Key(key),
        &key,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &preloaded,
        &mut textarea,
        &mut vim,
        &theme,
        None,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(textarea.lines(), &["draft".to_string()]);
}

#[test]
fn vim_insert_slash_remains_composer_text() {
    let (mut state, mut config, shared, action_tx, preloaded, operation) =
        status_harness(AppStatus::Idle);
    let mut textarea = TextArea::default();
    let mut vim = VimState::new(true);
    vim.mode = VimMode::Insert;
    let theme = Theme::named(ThemeName::Dark);
    let key = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);
    handle_status_key(
        &Event::Key(key),
        &key,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &preloaded,
        &mut textarea,
        &mut vim,
        &theme,
        None,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(textarea.lines(), &["/".to_string()]);
    assert!(!state.transcript_search.open);
}
```

Define:

```rust
fn status_harness(
    status: AppStatus,
) -> (
    AppState,
    RunConfig,
    Arc<Mutex<RunConfig>>,
    mpsc::Sender<UserAction>,
    Arc<Mutex<Option<SessionTranscript>>>,
    TestOperationInterrupt,
) {
    let (action_tx, _action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.set_status(status);
    let config = config();
    let shared = Arc::new(Mutex::new(config.clone()));
    (
        state,
        config,
        shared,
        action_tx,
        Arc::new(Mutex::new(None)),
        TestOperationInterrupt::default(),
    )
}

fn prepare_two_search_matches(state: &mut AppState) {
    state.push_message(ChatMessage::System("alpha one".to_string()));
    state.push_message(ChatMessage::System("alpha two".to_string()));
    let theme = Theme::named(ThemeName::Dark);
    let messages = &state.messages;
    let revisions = &state.message_revisions;
    state.transcript_render_cache.prepare(
        messages,
        revisions,
        TranscriptRenderContext::new(&theme, 40, 0, false),
        |_, message, theme, width, tick, force_expand| {
            build_lines_for_messages(
                std::slice::from_ref(message),
                theme,
                width,
                tick,
                force_expand,
            )
        },
    );
    state.open_transcript_search();
    state.replace_transcript_search_query("alpha");
    state.refresh_transcript_search();
}
```

- [ ] **Step 5: Route intents before status-specific handlers**

After Setup/SessionPicker/WaitingApproval early returns and before Idle/Running:

```rust
if matches!(
    state.status,
    AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
) && let Some(intent) = vim_state.transcript_search_intent(key.code)
{
    match intent {
        VimTranscriptSearchIntent::Open => state.open_transcript_search(),
        VimTranscriptSearchIntent::Next if state.transcript_search.has_query() => {
            state.search_next();
        }
        VimTranscriptSearchIntent::Previous if state.transcript_search.has_query() => {
            state.search_previous();
        }
        _ => {}
    }
    if matches!(intent, VimTranscriptSearchIntent::Open)
        || state.transcript_search.has_query()
    {
        return Ok(StatusKeyFlow::Continue);
    }
}
```

For `n/N` without a query, do not consume the key; allow existing Vim composer
handling, which currently leaves them unhandled. `/` is always consumed in
normal mode.

- [ ] **Step 6: Run GREEN and commit**

```sh
cargo test -p orca-tui vim_ --lib
cargo test -p orca-tui search_intent --lib
cargo test -p orca-tui status_key_actions --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/vim.rs crates/orca-tui/src/status_key_actions.rs
git commit -m "feat(tui): add Vim transcript search keys" \
  -m "Route normal-mode slash and n/N through the shared transcript search state without moving search ownership into the composer Vim engine." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 8: Prove Bounded Search Work and End-to-End Frames

**Files:**
- Modify: `crates/orca-tui/src/transcript_view.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [ ] **Step 1: Add the 10,000-message performance test**

Construct real cached messages:

```rust
#[test]
fn ten_thousand_messages_search_once_then_steady_scroll_and_navigation_scan_zero() {
    let messages = (0..10_000)
        .map(|index| ChatMessage::System(format!("message {index} needle")))
        .collect::<Vec<_>>();
    let revisions = (1..=10_000).collect::<Vec<u64>>();
    let mut cache = TranscriptRenderCache::default();
    let theme = theme();
    cache.prepare(
        &messages,
        &revisions,
        TranscriptRenderContext::new(&theme, 80, 0, false),
        |_, message, _, _, _, _| match message {
            ChatMessage::System(text) => vec![Line::from(text.clone())],
            _ => unreachable!(),
        },
    );

    let mut search = TranscriptSearchState::default();
    search.open_new();
    search.replace_query("needle");
    search.refresh_with(cache.content_generation(), 0, |query| cache.search(0, query));
    assert_eq!(search.match_count(), 10_000);
    assert_eq!(cache.last_search_lines_visited(), 10_000);

    search.refresh_with(cache.content_generation(), 0, |_| unreachable!());
    let _ = cache.viewport(0, 5_000, 20);
    search.refresh_with(cache.content_generation(), 5_000, |_| unreachable!());
    search.next();
    search.previous();
    assert_eq!(search.scan_count_for_test(), 1);
}
```

- [ ] **Step 2: Add append and visible-overlay work evidence**

Add:

```rust
#[test]
fn one_append_rebuilds_one_message_then_rescans_without_render_rebuilds() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut state = AppState::new(
        tx,
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    for index in 0..1_000 {
        state.push_message(ChatMessage::System(format!("item {index} needle")));
    }
    prepare_state_cache(&mut state, 80);
    state.open_transcript_search();
    state.replace_transcript_search_query("needle");
    state.refresh_transcript_search();
    let scans = state.transcript_search.scan_count_for_test();

    state.push_message(ChatMessage::System("last needle".to_string()));
    prepare_state_cache(&mut state, 80);
    assert_eq!(state.transcript_render_cache.last_prepare_visited(), 1);
    let render_generation = state.transcript_render_cache.content_generation();
    state.refresh_transcript_search();
    assert_eq!(
        state.transcript_search.scan_count_for_test(),
        scans + 1
    );
    assert_eq!(
        state.transcript_render_cache.content_generation(),
        render_generation
    );
    assert_eq!(state.transcript_render_cache.last_prepare_visited(), 1);
}

#[test]
fn visible_match_iterator_bounds_overlay_work_to_viewport() {
    let mut search = TranscriptSearchState::default();
    search.open_new();
    search.replace_query("needle");
    search.refresh_with(1, 0, |_| {
        (0..10_001)
            .map(|row| {
                TranscriptSearchMatch::new(
                    SelectionPos { row, col: 0 },
                    SelectionPos { row, col: 6 },
                    TranscriptLineIdentity {
                        message_revision: row as u64 + 1,
                        line_index: 0,
                    },
                    0..6,
                )
            })
            .collect()
    });
    assert_eq!(search.visible_matches(5_000, 5_020).count(), 20);
}
```

Define `prepare_state_cache` in `types.rs` tests exactly like Task 3's
`prepare_transcript_cache`; use the existing helper if Task 3 made it shared.

- [ ] **Step 3: Add completed event-loop/frame tests**

Add:

```rust
#[test]
fn search_keyboard_frames_move_active_match_without_composer_mutation() {
    let mut state = test_state();
    for index in 0..30 {
        state.push_message(ChatMessage::System(format!(
            "row {index:02} alpha"
        )));
    }
    let theme = Theme::named(ThemeName::Dark);
    let textarea = TextArea::from(["composer draft"]);
    let mut terminal =
        Terminal::new(TestBackend::new(40, 10)).expect("test backend");
    terminal
        .draw(|frame| render(frame, &mut state, &textarea, &theme))
        .expect("initial draw");

    state.open_transcript_search();
    state.replace_transcript_search_query("alpha");
    terminal
        .draw(|frame| render(frame, &mut state, &textarea, &theme))
        .expect("search draw");
    let first = state.transcript_search.active_ordinal();
    assert!(format!("{:?}", terminal.backend().buffer()).contains("1/30"));

    handle_transcript_search_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut state,
    );
    terminal
        .draw(|frame| render(frame, &mut state, &textarea, &theme))
        .expect("next draw");
    assert_ne!(state.transcript_search.active_ordinal(), first);
    assert!(!state.auto_scroll);
    assert_eq!(textarea.lines(), &["composer draft".to_string()]);

    handle_transcript_search_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        &mut state,
    );
    assert_eq!(state.transcript_search.active_ordinal(), first);
}

#[test]
fn running_search_esc_closes_before_interrupt_and_paste_never_touches_composer() {
    let (action_tx, action_rx) = crossbeam_channel::unbounded();
    let mut state = test_state();
    state.enter_running();
    state.open_transcript_search();
    let mut textarea = TextArea::from(["composer"]);
    let operation = TestOperationInterrupt::default();
    let mut config = config();
    let shared = Arc::new(Mutex::new(config.clone()));

    assert!(handle_paste_event(
        &Event::Paste("alpha\r\nbeta".to_string()),
        &mut state,
        &config,
        &mut textarea,
    ));
    assert_eq!(state.transcript_search.query(), "alpha beta");
    assert_eq!(textarea.lines(), &["composer".to_string()]);
    assert!(state.pending_pastes.is_empty());

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    handle_key_event_preflight(
        esc,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        || Ok(()),
    )
    .unwrap();
    assert!(!state.transcript_search.open);
    assert_eq!(operation.call_count(), 0);

    let preloaded = Arc::new(Mutex::new(None));
    let mut vim = VimState::new(false);
    let theme = Theme::named(ThemeName::Dark);
    handle_status_key(
        &Event::Key(esc),
        &esc,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &preloaded,
        &mut textarea,
        &mut vim,
        &theme,
        None,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(operation.call_count(), 1);
    assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
}

#[test]
fn mouse_selection_over_search_match_wins_and_copy_stays_exact() {
    let mut state = test_state();
    state.push_message(ChatMessage::System("alpha beta".to_string()));
    state.open_transcript_search();
    state.replace_transcript_search_query("alpha");
    let theme = Theme::named(ThemeName::Dark);
    let textarea = TextArea::default();
    let mut terminal =
        Terminal::new(TestBackend::new(40, 8)).expect("test backend");
    terminal
        .draw(|frame| render(frame, &mut state, &textarea, &theme))
        .expect("search draw");

    state.selection = Some(TranscriptSelection::unit(
        SelectionGranularity::Cell,
        SelectionPos { row: 0, col: 1 },
        SelectionPos { row: 0, col: 3 },
    ));
    terminal
        .draw(|frame| render(frame, &mut state, &textarea, &theme))
        .expect("selection draw");
    assert_eq!(
        state.transcript_render_cache.extract_text(
            state.selection.as_ref().unwrap()
        ),
        "lph"
    );
    let selected_cells = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .filter(|cell| cell.style().bg == theme.selection_style().bg)
        .count();
    assert!(selected_cells >= 3);
}

#[test]
fn streaming_and_resize_refresh_matches_without_stealing_active_identity() {
    let mut state = test_state();
    state.update(TuiEvent::MessageDelta(
        "prefix long words before alpha\n\nhidden alpha".to_string(),
    ));
    state.open_transcript_search();
    state.replace_transcript_search_query("alpha");
    let theme = Theme::named(ThemeName::Dark);
    let textarea = TextArea::default();
    let mut terminal =
        Terminal::new(TestBackend::new(20, 8)).expect("test backend");
    terminal
        .draw(|frame| render(frame, &mut state, &textarea, &theme))
        .expect("held draw");
    assert_eq!(state.transcript_search.match_count(), 1);
    let identity = state
        .transcript_search
        .active_match()
        .unwrap()
        .line_identity;

    state.update(TuiEvent::MessageDelta("\n".to_string()));
    terminal
        .draw(|frame| render(frame, &mut state, &textarea, &theme))
        .expect("released draw");
    assert_eq!(state.transcript_search.match_count(), 2);
    assert_eq!(
        state.transcript_search.active_match().unwrap().line_identity,
        identity
    );
    let before = state.transcript_search.active_match().unwrap().start;

    let mut resized =
        Terminal::new(TestBackend::new(8, 8)).expect("resized backend");
    resized
        .draw(|frame| render(frame, &mut state, &textarea, &theme))
        .expect("resized draw");
    assert_eq!(
        state.transcript_search.active_match().unwrap().line_identity,
        identity
    );
    assert_ne!(state.transcript_search.active_match().unwrap().start, before);
}
```

The running-Esc test intentionally verifies preflight closes search first and
the next dispatch reaches the existing status router's interrupt path.

- [ ] **Step 4: Run focused gates**

```sh
cargo test -p orca-tui transcript_search --lib
cargo test -p orca-tui search_match --lib
cargo test -p orca-tui search_shortcut --lib
cargo test -p orca-tui search_frame --lib
cargo test -p orca-tui search_paste --lib
cargo test -p orca-tui search_intent --lib
cargo test -p orca-tui ten_thousand_messages_search --lib
cargo test -p orca-tui transcript_view --lib
cargo test -p orca-tui ui::tests --lib
cargo test -p orca-tui app::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 5: Commit integration evidence**

```sh
git add crates/orca-tui/src/transcript_view.rs \
  crates/orca-tui/src/types.rs \
  crates/orca-tui/src/ui.rs \
  crates/orca-tui/src/app.rs
git commit -m "test(tui): verify transcript search integration" \
  -m "Cover 10,000-message bounded scans, steady and scroll-only zero work, streaming refresh, input priority, selection, resize, and completed search frames." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 9: Final Review, Audit, Push, and Remote Verification

**Files:**
- Verify every file above.

- [ ] **Step 1: Prompt-to-artifact audit**

| Requirement | Direct evidence |
|---|---|
| `Ctrl+F` opens search | shortcut resolver and completed input test |
| Vim `/` opens same surface | pure intent and status-routing tests |
| Vim `n/N` after close | closed-search navigation test |
| smart-case literal semantics | query unit tests |
| rendered text, not raw Markdown | cache builder fixtures |
| no hard-line/message crossing | cache boundary tests |
| soft-wrap crossing | wrapped phrase coordinate test |
| CJK/combining/emoji coordinates | Unicode mapping tests |
| hidden partial/table absent | streaming frame tests |
| spinner glyph excluded | tool spinner search test |
| active/inactive highlights | overlay and theme tests |
| selection wins | overlay order and copy tests |
| match jump and wraparound | state/navigation tests |
| nearest-edge scrolling | reveal-offset tests |
| auto-follow only changes on jump | AppState tests |
| mutation reconciliation | clear/replace/truncate/retain/backtrack tests |
| search hardware cursor | completed frame cursor test |
| fixed chrome does not overlap | compact layout tests |
| paste ownership | search paste test |
| `Ctrl+C` preserved | preflight test |
| steady and scroll zero scan | 10,000-message performance test |
| navigation zero scan | scan counter test |
| no second transcript cache | changed-file and symbol audit |
| no later P2 leakage | scope audit |

Treat every missing row as incomplete.

- [ ] **Step 2: Request independent reviews**

Specification review against:

```text
docs/superpowers/specs/2026-07-28-tui-transcript-search-design.md
```

Quality review checks:

- Unicode folding and original-range mapping;
- display-column mapping across wrap gaps;
- zero-width and wide-character behavior;
- generation bump omissions or excess invalidation;
- spinner search instability;
- stale active identity after retain/truncate;
- input-priority regressions;
- Running `Ctrl+G` ownership;
- hardware cursor ownership;
- overlay order and syntax-style preservation;
- performance evidence versus proxy counts;
- scope creep into keybindings/Vim enhancement.

Fix every Critical or Important finding with RED/GREEN evidence.

- [ ] **Step 3: Run package and workspace gates**

```sh
cargo test -p orca-tui -- --test-threads=1
cargo test --workspace --all-targets -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

If an unchanged process-cleanup timing test flakes:

1. prove its blob matches baseline `c2999f9c509067ef1f8426047cd26e87514404b6`;
2. run the exact serialized test five times;
3. skip only proven flaky tests in the workspace rerun;
4. do not alter unrelated process code.

- [ ] **Step 4: Audit commits, trailers, and scope**

```sh
git status --short
git log --format='%H%n%s%n%(trailers:key=Co-authored-by,valueonly)%n---' \
  c2999f9c509067ef1f8426047cd26e87514404b6..HEAD
git diff --check c2999f9c509067ef1f8426047cd26e87514404b6..HEAD
git diff --name-status c2999f9c509067ef1f8426047cd26e87514404b6..HEAD
git diff --stat c2999f9c509067ef1f8426047cd26e87514404b6..HEAD
```

Every commit must contain exactly one final:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

- [ ] **Step 5: Push and verify remote SHA**

```sh
git push origin feature/tui-syntax-highlighting
local_sha=$(git rev-parse HEAD)
remote_sha=$(git ls-remote --heads origin feature/tui-syntax-highlighting | awk '{print $1}')
test -n "$remote_sha"
test "$local_sha" = "$remote_sha"
git status --short --branch
```

Keep the branch and worktree for the remaining P2 roadmap.

