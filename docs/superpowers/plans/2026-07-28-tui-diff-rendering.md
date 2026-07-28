# TUI Diff Rendering Upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render structured unified diffs with capability-aware row backgrounds, stable dual line-number gutters, quiet hunk separators, and conservative word-level emphasis without weakening Orca's existing parser, syntax refinement, or bounded fallback contracts.

**Architecture:** Extend `Theme` with capability-adapted diff background colors, then keep all structural and inline presentation logic inside `diff_highlight.rs`. The existing `ParsedDiff` parser, first-paint syntax highlighter, background full-file refinement worker, transcript cache, and 80-row budget remain authoritative. Inline emphasis is computed only for fully visible adjacent delete-then-insert clusters and fails closed to whole-row styling.

**Tech Stack:** Rust 2024, ratatui 0.29, similar 3.1.1 inline diff, existing syntect/two-face highlighting, existing `TerminalColorLevel` quantization.

---

## File Map

- Modify `crates/orca-tui/Cargo.toml`
  - Declare the existing workspace `similar` crate as a direct TUI dependency.
- Modify `crates/orca-tui/src/theme.rs`
  - Own four new diff row/emphasis background colors and capability adaptation.
- Modify `crates/orca-tui/src/diff_highlight.rs`
  - Own gutter sizing/formatting, hunk separator formatting, background
    composition, adjacent replacement clustering, bounded inline emphasis, and
    all focused tests.
- Create no new production module.
  - The existing diff module already contains the parser, render path, and
    focused regression suite. Splitting the visual helpers away would create a
    second diff-domain boundary without reducing coupling.

---

### Task 1: Add Capability-Aware Diff Background Colors

**Files:**
- Modify: `crates/orca-tui/src/theme.rs`

- [ ] **Step 1: Write failing palette-shape and exact-color tests**

Extend the test-only palette helper from 16 to 20 values:

```rust
fn theme_colors(theme: Theme) -> [Color; 20] {
    [
        theme.border,
        theme.text,
        theme.muted,
        theme.user,
        theme.success,
        theme.warning,
        theme.error,
        theme.approval,
        theme.plan_mode,
        theme.markdown_h1,
        theme.markdown_h2,
        theme.markdown_h3,
        theme.markdown_inline_code,
        theme.diff_add,
        theme.diff_remove,
        theme.diff_add_bg,
        theme.diff_remove_bg,
        theme.diff_add_emphasis_bg,
        theme.diff_remove_emphasis_bg,
        theme.selection_bg,
    ]
}
```

Add:

```rust
#[test]
fn dark_diff_backgrounds_match_the_review_palette() {
    let theme = Theme::named(ThemeName::Dark);
    assert_eq!(theme.diff_add_bg, Color::Rgb(0x21, 0x3a, 0x2b));
    assert_eq!(theme.diff_remove_bg, Color::Rgb(0x4a, 0x22, 0x1d));
    assert_eq!(theme.diff_add_emphasis_bg, Color::Rgb(0x31, 0x5c, 0x40));
    assert_eq!(theme.diff_remove_emphasis_bg, Color::Rgb(0x71, 0x35, 0x2a));
}
```

Update the existing named-theme expected arrays to include:

```rust
// Dark
Color::Rgb(0x21, 0x3a, 0x2b),
Color::Rgb(0x4a, 0x22, 0x1d),
Color::Rgb(0x31, 0x5c, 0x40),
Color::Rgb(0x71, 0x35, 0x2a),

// Light
Color::Rgb(0xdc, 0xfc, 0xe7),
Color::Rgb(0xfe, 0xe2, 0xe2),
Color::Rgb(0x86, 0xef, 0xac),
Color::Rgb(0xfc, 0xa5, 0xa5),

// Solarized
Color::Rgb(0x16, 0x3c, 0x3a),
Color::Rgb(0x4c, 0x2a, 0x2a),
Color::Rgb(0x24, 0x5b, 0x52),
Color::Rgb(0x71, 0x3c, 0x35),

// Catppuccin
Color::Rgb(0x29, 0x44, 0x36),
Color::Rgb(0x4a, 0x30, 0x3a),
Color::Rgb(0x3d, 0x65, 0x4d),
Color::Rgb(0x70, 0x45, 0x55),
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui dark_diff_backgrounds_match_the_review_palette --lib
cargo test -p orca-tui named_themes_preserve_exact_truecolor_palettes --lib
```

Expected: compilation fails because the four fields do not exist.

- [ ] **Step 3: Add the four theme fields and exact truecolor values**

Add to `Theme` after `diff_remove`:

```rust
pub diff_add_bg: Color,
pub diff_remove_bg: Color,
pub diff_add_emphasis_bg: Color,
pub diff_remove_emphasis_bg: Color,
```

Populate each base theme with the exact values from Step 1.

- [ ] **Step 4: Capability-adapt all new backgrounds**

In `Theme::resolve`, add:

```rust
theme.diff_add_bg = adapt(theme.diff_add_bg);
theme.diff_remove_bg = adapt(theme.diff_remove_bg);
theme.diff_add_emphasis_bg = adapt(theme.diff_add_emphasis_bg);
theme.diff_remove_emphasis_bg = adapt(theme.diff_remove_emphasis_bg);
```

The existing `assert_theme_colors_fit_level` test must now inspect the new
fields through the 20-value helper. Do not add bespoke quantization logic.

- [ ] **Step 5: Run GREEN and commit**

```sh
cargo test -p orca-tui theme::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/theme.rs
git commit -m "feat(tui): add diff review background colors" \
  -m "Define theme-specific row and inline emphasis backgrounds with terminal capability degradation." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 2: Render Stable Dual Line-Number Gutters

**Files:**
- Modify: `crates/orca-tui/src/diff_highlight.rs`

- [ ] **Step 1: Write failing pure gutter tests**

Add test helpers:

```rust
fn span_text(line: &Line<'_>, index: usize) -> &str {
    line.spans[index].content.as_ref()
}
```

Add:

```rust
#[test]
fn structured_diff_uses_one_right_aligned_dual_line_number_gutter() {
    let diff = "\
--- a/value.rs
+++ b/value.rs
@@ -9,2 +99,2 @@ fn value()
 old
-before
+after
";
    let theme = dark_theme();
    let lines = render_unified_diff(diff, &theme, None);

    let context = find_rendered_line(&lines, "old");
    let deletion = find_rendered_line(&lines, "before");
    let insertion = find_rendered_line(&lines, "after");

    assert_eq!(span_text(context, 0), "  9  99   ");
    assert_eq!(span_text(deletion, 0), " 10     - ");
    assert_eq!(span_text(insertion, 0), "    100 + ");
    assert_eq!(context.spans[0].style.fg, Some(theme.muted));
    assert_eq!(deletion.spans[0].style.fg, Some(theme.diff_remove));
    assert_eq!(insertion.spans[0].style.fg, Some(theme.diff_add));
}
```

Add a second fixture with two hunks (`9` and `123`) and assert both use width 3.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui structured_diff_uses_one_right_aligned_dual_line_number_gutter --lib
cargo test -p orca-tui multiple_hunks_share_the_largest_gutter_width --lib
```

Expected: current prefixes are `     ` / `    -` / `    +`.

- [ ] **Step 3: Implement pure gutter width and formatter helpers**

Add:

```rust
fn decimal_width(value: usize) -> usize {
    value.max(1).ilog10() as usize + 1
}

fn parsed_gutter_width(parsed: &ParsedDiff) -> usize {
    parsed
        .hunks
        .iter()
        .flat_map(DiffHunk::source_lines)
        .flat_map(|line| [line.old_line, line.new_line])
        .flatten()
        .map(decimal_width)
        .max()
        .unwrap_or(1)
}

fn source_gutter(line: &DiffSourceLine, width: usize) -> String {
    let old = line.old_line.map_or_else(
        || " ".repeat(width),
        |number| format!("{number:>width$}"),
    );
    let new = line.new_line.map_or_else(
        || " ".repeat(width),
        |number| format!("{number:>width$}"),
    );
    let marker = match line.kind {
        DiffLineKind::Context => ' ',
        DiffLineKind::Insert => '+',
        DiffLineKind::Delete => '-',
    };
    format!("{old} {new} {marker} ")
}
```

Use the parsed-wide value in every structured hunk renderer. Remove
`source_style`'s hard-coded prefix return value; retain only its foreground
choice.

- [ ] **Step 4: Preserve source text, syntax spans, refinement, and row count**

Update `rendered_source_line` to accept `gutter_width`. Its first span is the
gutter and all later spans concatenate to `line.content`.

Run:

```sh
cargo test -p orca-tui parser_tracks_destination_and_old_new_line_numbers --lib
cargo test -p orca-tui parsed_diff --lib
cargo test -p orca-tui refined --lib
```

Expected: all pass after updating only assertions that explicitly encoded the
old five-character prefix.

- [ ] **Step 5: Run GREEN and commit**

```sh
cargo test -p orca-tui gutter --lib
cargo test -p orca-tui diff_highlight --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/diff_highlight.rs
git commit -m "feat(tui): show diff line number gutters" \
  -m "Render stable right-aligned old and new line numbers without changing parsed source text or row bounds." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 3: Replace Raw Hunk Coordinates with Quiet Separators

**Files:**
- Modify: `crates/orca-tui/src/diff_highlight.rs`

- [ ] **Step 1: Write failing separator tests**

Add:

```rust
#[test]
fn hunk_separator_hides_coordinates_and_keeps_scope_context() {
    let diff = "\
--- a/value.rs
+++ b/value.rs
@@ -12,1 +34,1 @@ impl Value
-old
+new
";
    let lines = render_unified_diff(diff, &dark_theme(), None);
    let separator = &lines[2];
    let text = rendered_text(separator);

    assert_eq!(text, " ⋮  ⋮   impl Value");
    assert!(!text.contains("@@"));
    assert!(!text.contains("-12"));
    assert!(!text.contains("+34"));
}

#[test]
fn hunk_separator_without_scope_contains_only_the_dual_ellipsis_gutter() {
    let diff = "--- a/v\n+++ b/v\n@@ -1 +1 @@\n-old\n+new\n";
    let lines = render_unified_diff(diff, &dark_theme(), None);
    assert_eq!(rendered_text(&lines[2]), "⋮ ⋮   ");
}
```

Expected strings must be generated from the actual gutter width:

```rust
format!("{:>width$} {:>width$}   {scope}", "⋮", "⋮")
```

Use that helper in the final test if the literal spacing differs after Task 2.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui hunk_separator --lib
```

Expected: renderer still exposes the raw `@@` header.

- [ ] **Step 3: Implement suffix extraction and separator rendering**

Add:

```rust
fn hunk_scope(header: &str) -> &str {
    header
        .strip_prefix("@@")
        .and_then(|rest| rest.split_once("@@"))
        .map_or("", |(_, suffix)| suffix.trim())
}

fn rendered_hunk_separator(
    hunk: &DiffHunk,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let gutter = format!("{:>width$} {:>width$}   ", "⋮", "⋮");
    let scope = hunk_scope(&hunk.header);
    Line::from(vec![
        Span::styled(gutter, Style::default().fg(theme.border)),
        Span::styled(scope.to_owned(), Style::default().fg(theme.muted)),
    ])
}
```

Use it in `render_parsed_diff_with`. Do not modify `hunk.header` in the parser.

- [ ] **Step 4: Verify malformed/raw fallback remains exact**

```sh
cargo test -p orca-tui malformed --lib
cargo test -p orca-tui raw_fallback --lib
cargo test -p orca-tui headerless_fragment --lib
```

Expected:

- valid structured hunks use `⋮`;
- raw fallback still renders the original `@@` text;
- no raw fragment receives fabricated numbers or separator spans.

- [ ] **Step 5: Run GREEN and commit**

```sh
cargo test -p orca-tui hunk_separator --lib
cargo test -p orca-tui diff_highlight --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/diff_highlight.rs
git commit -m "feat(tui): simplify diff hunk separators" \
  -m "Replace structural coordinates with dual ellipses while preserving optional scope context and raw fallback text." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 4: Apply Whole-Row Backgrounds without Losing Syntax

**Files:**
- Modify: `crates/orca-tui/src/diff_highlight.rs`

- [ ] **Step 1: Write failing truecolor and span-composition tests**

Add:

```rust
#[test]
fn changed_rows_apply_exact_dark_backgrounds_to_gutter_and_content() {
    let theme = dark_theme();
    let lines = render_unified_diff(RUST_DIFF, &theme, None);
    let deletion = find_rendered_line(&lines, "fn old");
    let insertion = find_rendered_line(&lines, "fn new");

    assert!(deletion.spans.iter().all(|span| {
        span.style.bg == Some(Color::Rgb(0x4a, 0x22, 0x1d))
    }));
    assert!(insertion.spans.iter().all(|span| {
        span.style.bg == Some(Color::Rgb(0x21, 0x3a, 0x2b))
    }));
}

#[test]
fn diff_backgrounds_preserve_syntax_and_refined_foregrounds() {
    let theme = dark_theme();
    let overlay = Color::Rgb(1, 2, 3);
    let refined = RefinedDiffStyles::from([(
        1,
        vec![Span::styled(
            "fn new() { let value = \"new\"; }",
            Style::default().fg(overlay).add_modifier(Modifier::ITALIC),
        )],
    )]);
    let lines = render_unified_diff(RUST_DIFF, &theme, Some(&refined));
    let insertion = find_rendered_line(&lines, "fn new");
    let refined_span = insertion
        .spans
        .iter()
        .find(|span| span.style.fg == Some(overlay))
        .expect("refined foreground");

    assert_eq!(refined_span.style.bg, Some(theme.diff_add_bg));
    assert!(refined_span.style.add_modifier.contains(Modifier::ITALIC));
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui changed_rows_apply_exact_dark_backgrounds --lib
cargo test -p orca-tui diff_backgrounds_preserve_syntax --lib
```

Expected: all backgrounds are `None`.

- [ ] **Step 3: Implement style background composition**

Add:

```rust
fn row_background(kind: DiffLineKind, theme: &Theme) -> Option<Color> {
    match (kind, theme.color_level) {
        (_, TerminalColorLevel::Monochrome) | (DiffLineKind::Context, _) => None,
        (DiffLineKind::Insert, _) => Some(theme.diff_add_bg),
        (DiffLineKind::Delete, _) => Some(theme.diff_remove_bg),
    }
}

fn with_background(mut style: Style, background: Option<Color>) -> Style {
    if let Some(background) = background {
        style.bg = Some(background);
    }
    style
}
```

Apply the row background to the gutter and every source content span after
syntax/refined selection. Do not replace foregrounds, modifiers, or underline
colors.

- [ ] **Step 4: Add capability-level behavior tests**

Extend `parsed_diff_styles_obey_terminal_color_level` to inspect both `fg` and
`bg`. Add:

```rust
#[test]
fn monochrome_changed_rows_use_markers_without_backgrounds() {
    let theme = Theme::resolve(
        ThemeName::Dark,
        TerminalProfile {
            background: TerminalBackground::Dark,
            color_level: TerminalColorLevel::Monochrome,
        },
    );
    let lines = render_unified_diff(RUST_DIFF, &theme, None);
    let deletion = find_rendered_line(&lines, "fn old");
    let insertion = find_rendered_line(&lines, "fn new");

    assert!(rendered_text(deletion).contains("- "));
    assert!(rendered_text(insertion).contains("+ "));
    assert!(deletion.spans.iter().all(|span| span.style.bg.is_none()));
    assert!(insertion.spans.iter().all(|span| span.style.bg.is_none()));
}
```

The capability loop must continue to include the exact variants:

```rust
TerminalColorLevel::TrueColor,
TerminalColorLevel::Ansi256,
TerminalColorLevel::Ansi16,
TerminalColorLevel::Monochrome,
```

- [ ] **Step 5: Run GREEN and commit**

```sh
cargo test -p orca-tui diff_background --lib
cargo test -p orca-tui monochrome_changed_rows --lib
cargo test -p orca-tui parsed_diff_styles_obey_terminal_color_level --lib
cargo test -p orca-tui diff_highlight --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/diff_highlight.rs
git commit -m "feat(tui): color changed diff rows" \
  -m "Apply capability-aware added and deleted row backgrounds while preserving syntax and refined foreground styles." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 5: Add Bounded Adjacent Word-Level Emphasis

**Files:**
- Modify: `crates/orca-tui/Cargo.toml`
- Modify: `crates/orca-tui/src/diff_highlight.rs`

- [ ] **Step 1: Declare `similar` directly**

Add to `[dependencies]` in `crates/orca-tui/Cargo.toml`:

```toml
similar = { workspace = true, features = ["inline"] }
```

Do not change the workspace version or lockfile package version. The explicit
feature is required because `similar`'s default features include `text` but not
`inline`.

- [ ] **Step 2: Write failing replacement-cluster classification tests**

Define the intended pure output:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReplacementCluster {
    delete_start: usize,
    delete_end: usize,
    insert_start: usize,
    insert_end: usize,
}
```

Add:

```rust
#[test]
fn replacement_clusters_include_only_adjacent_delete_then_insert_blocks() {
    let entries = vec![
        DiffHunkEntry::Source(DiffSourceLine {
            kind: DiffLineKind::Delete,
            old_line: Some(1),
            new_line: None,
            content: "old one".to_string(),
        }),
        DiffHunkEntry::Source(DiffSourceLine {
            kind: DiffLineKind::Delete,
            old_line: Some(2),
            new_line: None,
            content: "old two".to_string(),
        }),
        DiffHunkEntry::Source(DiffSourceLine {
            kind: DiffLineKind::Insert,
            old_line: None,
            new_line: Some(1),
            content: "new one".to_string(),
        }),
        DiffHunkEntry::Source(DiffSourceLine {
            kind: DiffLineKind::Insert,
            old_line: None,
            new_line: Some(2),
            content: "new two".to_string(),
        }),
        DiffHunkEntry::Source(DiffSourceLine {
            kind: DiffLineKind::Context,
            old_line: Some(3),
            new_line: Some(3),
            content: "context".to_string(),
        }),
        DiffHunkEntry::Source(DiffSourceLine {
            kind: DiffLineKind::Delete,
            old_line: Some(4),
            new_line: None,
            content: "old three".to_string(),
        }),
        DiffHunkEntry::Metadata("\\ No newline at end of file".to_string()),
        DiffHunkEntry::Source(DiffSourceLine {
            kind: DiffLineKind::Insert,
            old_line: None,
            new_line: Some(4),
            content: "new three".to_string(),
        }),
    ];
    assert_eq!(
        replacement_clusters(&entries),
        vec![ReplacementCluster {
            delete_start: 0,
            delete_end: 2,
            insert_start: 2,
            insert_end: 4,
        }]
    );
}
```

- [ ] **Step 3: Run cluster RED and implement the scanner**

```sh
cargo test -p orca-tui replacement_clusters_include_only_adjacent --lib
```

Implement a linear scan:

```rust
fn replacement_clusters(entries: &[DiffHunkEntry]) -> Vec<ReplacementCluster> {
    let mut clusters = Vec::new();
    let mut index = 0;
    while index < entries.len() {
        let delete_start = index;
        while matches!(
            entries.get(index),
            Some(DiffHunkEntry::Source(DiffSourceLine {
                kind: DiffLineKind::Delete,
                ..
            }))
        ) {
            index += 1;
        }
        if index == delete_start {
            index += 1;
            continue;
        }
        let insert_start = index;
        while matches!(
            entries.get(index),
            Some(DiffHunkEntry::Source(DiffSourceLine {
                kind: DiffLineKind::Insert,
                ..
            }))
        ) {
            index += 1;
        }
        if index > insert_start {
            clusters.push(ReplacementCluster {
                delete_start,
                delete_end: insert_start,
                insert_start,
                insert_end: index,
            });
        }
    }
    clusters
}
```

- [ ] **Step 4: Write failing inline-span tests**

Add a fixture:

```rust
const INLINE_DIFF: &str = "\
--- a/value.rs
+++ b/value.rs
@@ -1 +1 @@
-let colour = \"red apple\";
+let color = \"green apple\";
";
```

Add:

```rust
#[test]
fn adjacent_replacement_emphasizes_only_changed_inline_tokens() {
    let theme = dark_theme();
    let lines = render_unified_diff(INLINE_DIFF, &theme, None);
    let deletion = find_rendered_line(&lines, "colour");
    let insertion = find_rendered_line(&lines, "color");

    assert!(deletion.spans.iter().any(|span| {
        span.content.contains("colour")
            && span.style.bg == Some(theme.diff_remove_emphasis_bg)
    }));
    assert!(insertion.spans.iter().any(|span| {
        span.content.contains("color")
            && span.style.bg == Some(theme.diff_add_emphasis_bg)
    }));
    assert!(deletion.spans.iter().any(|span| {
        span.content.contains("apple")
            && span.style.bg == Some(theme.diff_remove_bg)
    }));
    assert!(insertion.spans.iter().any(|span| {
        span.content.contains("apple")
            && span.style.bg == Some(theme.diff_add_bg)
    }));
}
```

Also assert concatenated content after the gutter equals the exact parsed
source line.

- [ ] **Step 5: Implement bounded `similar` mapping**

Import:

```rust
use std::time::{Duration, Instant};
use similar::{ChangeTag, TextDiff};
```

Add:

```rust
const INLINE_DIFF_DEADLINE: Duration = Duration::from_millis(5);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct InlineSegments {
    old: HashMap<usize, Vec<(bool, String)>>,
    new: HashMap<usize, Vec<(bool, String)>>,
}
```

For each fully admitted replacement cluster:

1. collect old/new `line.content` values;
2. reject the cluster if either block violates existing aggregate, line-count,
   or line-byte guardrails;
3. create `TextDiff::from_lines` using joined text with one synthetic newline
   per source line;
4. capture one deadline for the entire cluster:

```rust
let deadline = Some(Instant::now() + INLINE_DIFF_DEADLINE);
```

5. iterate every op with that same deadline:

```rust
diff.iter_inline_changes_deadline(op, deadline)
```

6. map `ChangeTag::Delete` by `old_index` and `ChangeTag::Insert` by
   `new_index`;
7. convert `iter_strings_lossy()` to owned `(emphasized, String)` values;
8. strip only the synthetic final `\n` from each mapped source line;
9. reject the entire cluster unless reconstructed old/new text is exact.

Do not apply `ChangeTag::Equal` rows as changed source entries.

- [ ] **Step 6: Compose inline emphasis with syntax/refined foregrounds**

Add a helper that splits existing syntax/refined spans at byte ranges defined
by the inline segments. It must:

- preserve exact UTF-8 boundaries;
- preserve the original span foreground/modifiers/underline;
- apply emphasis background only where `emphasized == true`;
- use row background for equal segments;
- add `Modifier::BOLD` instead of a background in monochrome.

The helper returns `None` if either inline segment stream or syntax span stream
does not reconstruct `line.content`; caller then uses whole-row styling.

- [ ] **Step 7: Add fallback and boundary tests**

Add tests for:

```text
- delete/context/insert does not pair
- delete/metadata/insert does not pair
- low-similarity replacement has no emphasis background
- Unicode and combining text is byte-identical after rendering
- refined foreground remains under emphasis background
- monochrome emphasis uses BOLD and no background
- a cluster cut by the 80-row budget is not partially emphasized
- a 4097-byte source line gets whole-row styling only
```

Use existing test helpers `rendered_text`, `find_rendered_line`, and
`MAX_HIGHLIGHT_LINE_BYTES`; do not send terminal escape sequences.

- [ ] **Step 8: Run GREEN and commit**

```sh
cargo test -p orca-tui replacement_cluster --lib
cargo test -p orca-tui inline_tokens --lib
cargo test -p orca-tui inline_emphasis --lib
cargo test -p orca-tui low_similarity --lib
cargo test -p orca-tui combining --lib
cargo test -p orca-tui truncated --lib
cargo test -p orca-tui guardrail --lib
cargo test -p orca-tui diff_highlight --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/Cargo.toml crates/orca-tui/src/diff_highlight.rs Cargo.lock
git commit -m "feat(tui): emphasize inline diff changes" \
  -m "Use bounded adjacent replacement refinement to highlight changed words without crossing hunk or fallback boundaries." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

If `Cargo.lock` is byte-identical because `similar` is already present, do not
stage or force-edit it.

---

### Task 6: Regression Audit and Delivery

**Files:**
- Verify every file above.

- [ ] **Step 1: Run focused requirement filters**

```sh
cargo test -p orca-tui gutter --lib
cargo test -p orca-tui hunk_separator --lib
cargo test -p orca-tui diff_background --lib
cargo test -p orca-tui inline_emphasis --lib
cargo test -p orca-tui replacement_cluster --lib
cargo test -p orca-tui malformed --lib
cargo test -p orca-tui raw_fallback --lib
cargo test -p orca-tui truncated --lib
cargo test -p orca-tui guardrail --lib
cargo test -p orca-tui parsed_diff_styles_obey_terminal_color_level --lib
```

- [ ] **Step 2: Run package and workspace gates**

```sh
cargo test -p orca-tui diff_highlight --lib
cargo test -p orca-tui ui::tests --lib
cargo test -p orca-tui -- --test-threads=1
cargo test --workspace --all-targets -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

If the unchanged external-process timeout test exhibits its already proven
timing flake:

1. prove its source is unchanged from the P0 #6 baseline;
2. run its exact serialized filter five times;
3. run the complete workspace suite with only that exact test skipped;
4. do not modify unrelated process-management code.

- [ ] **Step 3: Prompt-to-artifact audit**

| Requirement | Direct evidence |
|---|---|
| Added/deleted row backgrounds | exact dark RGB and rendered-span tests |
| 256/16-color fallback | all rendered foreground/background colors fit capability |
| Monochrome fallback | no colors; gutter markers and bold inline change remain |
| Right-aligned dual line numbers | one-/two-/three-digit and multi-hunk tests |
| `⋮` instead of `@@` | structured separator tests |
| Scope suffix preserved | hunk suffix test |
| Word-level diff | exact changed/equal token background tests |
| Conservative pairing | adjacency/context/metadata boundary tests |
| `similar` direct use | manifest plus `iter_inline_changes_deadline` source audit |
| Syntax/refinement retained | foreground and modifier composition tests |
| 80-row truncation retained | exact count and partial-cluster tests |
| Existing guardrails retained | 512 KiB / 10,000 lines / 4 KiB tests |
| Malformed fallback truthful | exact raw text and unsegmented style tests |
| No parser/worker/cache rewrite | scope diff inspection |
| No P0 #8/P2 leakage | changed-file and symbol audit |

Treat any missing direct evidence as incomplete.

- [ ] **Step 4: Request independent specification and quality reviews**

Review baseline:

```text
docs/superpowers/specs/2026-07-28-tui-diff-rendering-design.md
```

Specification review checks every table row above. Quality review checks:

- UTF-8 split safety;
- inline deadline and input bounds;
- no cross-hunk or metadata pairing;
- style foreground/modifier preservation;
- no fabricated numbers in fallback;
- exact 80-row accounting;
- no per-frame work outside cached transcript construction;
- no Critical or Important findings.

Fix every Critical/Important finding with RED/GREEN evidence and rerun all
gates.

- [ ] **Step 5: Audit commits, trailers, and scope**

Use baseline `4bb642c91bbc1f212740738d6831e4fa5e51e12c`:

```sh
git status --short
git log --format='%H%n%s%n%(trailers:key=Co-authored-by,valueonly)%n---' \
  4bb642c91bbc1f212740738d6831e4fa5e51e12c..HEAD
git diff --check 4bb642c91bbc1f212740738d6831e4fa5e51e12c..HEAD
git diff --name-status 4bb642c91bbc1f212740738d6831e4fa5e51e12c..HEAD
git diff --stat 4bb642c91bbc1f212740738d6831e4fa5e51e12c..HEAD
```

Every commit must contain exactly one final:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

- [ ] **Step 6: Push and verify**

```sh
git push origin feature/tui-syntax-highlighting
local_sha=$(git rev-parse HEAD)
remote_sha=$(git ls-remote --heads origin feature/tui-syntax-highlighting | awk '{print $1}')
test -n "$remote_sha"
test "$local_sha" = "$remote_sha"
git status --short --branch
```

Keep the branch and active checkout for P0 #8.
