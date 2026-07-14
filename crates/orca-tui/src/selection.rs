//! Mouse drag selection over the transcript, in content space.
//!
//! Positions are anchored to the transcript's absolute visual rows (the same
//! wrapped-row space as `TranscriptRenderCache::cumulative_heights`) plus a
//! display column, NOT to screen coordinates. That way a selection stays glued
//! to its content when the transcript scrolls or new messages stream in below.

use ratatui::layout::{Position, Rect};
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// A caret position in transcript content space: absolute visual row across
/// all wrapped message rows, plus display column within that row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SelectionPos {
    pub row: usize,
    pub col: usize,
}

/// An in-progress or settled drag selection. `anchor` is where the drag
/// started (mouse down), `head` follows the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TranscriptSelection {
    pub anchor: SelectionPos,
    pub head: SelectionPos,
    pub dragging: bool,
}

impl TranscriptSelection {
    pub fn begin(pos: SelectionPos) -> Self {
        Self {
            anchor: pos,
            head: pos,
            dragging: true,
        }
    }

    /// True when anchor and head sit on the same cell. The input layer treats
    /// this as a plain click and clears it on mouse-up; as a *selection* it is
    /// still one cell wide (see [`Self::normalized`]), which is what a
    /// double-click word selection of a single character produces.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Ordered endpoints with an EXCLUSIVE end column; the cell under the
    /// later endpoint is included in the selection.
    pub fn normalized(&self) -> (SelectionPos, SelectionPos) {
        let (start, mut end) = if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        end.col = end.col.saturating_add(1);
        (start, end)
    }

    /// The selected column range on absolute row `row`, if any. The start
    /// column is inclusive; a `None` end means "to the end of the row".
    pub fn cols_on_row(&self, row: usize) -> Option<(usize, Option<usize>)> {
        let (start, end) = self.normalized();
        if row < start.row || row > end.row {
            return None;
        }
        if start.row == end.row {
            return Some((start.col, Some(end.col)));
        }
        if row == start.row {
            Some((start.col, None))
        } else if row == end.row {
            Some((0, Some(end.col)))
        } else {
            Some((0, None))
        }
    }
}

/// Map a mouse position to content space. `first_visible_row` is the absolute
/// row of the transcript area's top line. Returns `None` outside `area`.
pub fn screen_to_selection_pos(
    area: Rect,
    first_visible_row: usize,
    column: u16,
    row: u16,
) -> Option<SelectionPos> {
    if !area.contains(Position::new(column, row)) {
        return None;
    }
    Some(SelectionPos {
        row: first_visible_row + (row - area.y) as usize,
        col: (column - area.x) as usize,
    })
}

/// Like [`screen_to_selection_pos`] but clamps out-of-area coordinates onto
/// the nearest cell, so a drag that leaves the transcript keeps tracking.
pub fn screen_to_selection_pos_clamped(
    area: Rect,
    first_visible_row: usize,
    column: u16,
    row: u16,
) -> Option<SelectionPos> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let column = column.clamp(area.x, area.x + area.width - 1);
    let row = row.clamp(area.y, area.y + area.height - 1);
    screen_to_selection_pos(area, first_visible_row, column, row)
}

/// The OSC 52 escape sequence that puts `text` on the system clipboard.
/// Understood by VS Code, iTerm2, kitty, WezTerm, tmux (with `set-clipboard`),
/// and works across SSH because the terminal emulator does the writing.
pub fn osc52_copy_sequence(text: &str) -> String {
    use base64::Engine as _;
    format!(
        "\x1b]52;c;{}\x07",
        base64::engine::general_purpose::STANDARD.encode(text)
    )
}

/// Re-style the display-column range `[col_start, col_end)` of a pre-wrapped
/// line with the theme's selection background, splitting spans at the
/// boundaries. Foreground colors are preserved so highlighted syntax stays
/// readable — only the background changes (like an editor selection). A
/// `None` end highlights through the end of the line. A wide character is
/// selected iff its leading column is inside the range.
pub fn apply_selection_to_line(
    line: Line<'static>,
    col_start: usize,
    col_end: Option<usize>,
    selection_bg: Color,
) -> Line<'static> {
    let col_end = col_end.unwrap_or(usize::MAX);
    if col_start >= col_end {
        return line;
    }

    let alignment = line.alignment;
    let line_style = line.style;
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;

    for span in line.spans {
        let span_width: usize = span.content.chars().map(|ch| ch.width().unwrap_or(0)).sum();
        if col + span_width <= col_start || col >= col_end {
            col += span_width;
            out.push(span);
            continue;
        }

        // The span straddles a boundary: split it into styled runs.
        let style = span.style;
        let mut run = String::new();
        let mut run_selected: Option<bool> = None;
        for ch in span.content.chars() {
            let selected = col >= col_start && col < col_end;
            if run_selected.is_some_and(|current| current != selected) {
                flush_run(
                    &mut out,
                    &mut run,
                    style,
                    run_selected == Some(true),
                    selection_bg,
                );
            }
            run_selected = Some(selected);
            run.push(ch);
            col += ch.width().unwrap_or(0);
        }
        flush_run(
            &mut out,
            &mut run,
            style,
            run_selected == Some(true),
            selection_bg,
        );
    }

    let mut line = Line::from(out);
    line.alignment = alignment;
    line.style = line_style;
    line
}

fn flush_run(
    out: &mut Vec<Span<'static>>,
    run: &mut String,
    style: ratatui::style::Style,
    selected: bool,
    selection_bg: Color,
) {
    if run.is_empty() {
        return;
    }
    let style = if selected {
        style.bg(selection_bg)
    } else {
        style
    };
    out.push(Span::styled(std::mem::take(run), style));
}

/// Slice `text` (one wrapped row) to the display-column range
/// `[col_start, col_end)`. A wide character is included iff its leading
/// column is inside the range.
pub fn slice_row_by_columns(text: &str, col_start: usize, col_end: Option<usize>) -> &str {
    let col_end = col_end.unwrap_or(usize::MAX);
    let mut col = 0usize;
    let mut byte_start = None;
    let mut byte_end = text.len();
    for (offset, ch) in text.char_indices() {
        if col >= col_end {
            byte_end = offset;
            break;
        }
        if col >= col_start && byte_start.is_none() {
            byte_start = Some(offset);
        }
        col += ch.width().unwrap_or(0);
    }
    let byte_start = byte_start.unwrap_or(if col < col_start {
        text.len()
    } else {
        byte_end
    });
    &text[byte_start.min(byte_end)..byte_end]
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    use super::{
        SelectionPos, TranscriptSelection, apply_selection_to_line, osc52_copy_sequence,
        screen_to_selection_pos, screen_to_selection_pos_clamped, slice_row_by_columns,
    };

    const SEL_BG: Color = Color::Rgb(46, 62, 132);

    fn pos(row: usize, col: usize) -> SelectionPos {
        SelectionPos { row, col }
    }

    /// (text, has-selection-bg, foreground) triples for a highlighted line.
    fn rendered(line: &Line<'static>) -> Vec<(String, bool, Option<Color>)> {
        line.spans
            .iter()
            .map(|span| {
                (
                    span.content.to_string(),
                    span.style.bg == Some(SEL_BG),
                    span.style.fg,
                )
            })
            .collect()
    }

    #[test]
    fn normalized_orders_endpoints_and_makes_end_column_exclusive() {
        let forward = TranscriptSelection {
            anchor: pos(1, 4),
            head: pos(3, 2),
            dragging: false,
        };
        assert_eq!(forward.normalized(), (pos(1, 4), pos(3, 3)));

        let backward = TranscriptSelection {
            anchor: pos(3, 2),
            head: pos(1, 4),
            dragging: false,
        };
        assert_eq!(backward.normalized(), (pos(1, 4), pos(3, 3)));
    }

    #[test]
    fn cols_on_row_covers_first_middle_last_and_outside_rows() {
        let sel = TranscriptSelection {
            anchor: pos(1, 4),
            head: pos(3, 2),
            dragging: false,
        };
        assert_eq!(sel.cols_on_row(0), None);
        assert_eq!(sel.cols_on_row(1), Some((4, None)));
        assert_eq!(sel.cols_on_row(2), Some((0, None)));
        assert_eq!(sel.cols_on_row(3), Some((0, Some(3))));
        assert_eq!(sel.cols_on_row(4), None);
    }

    #[test]
    fn single_row_selection_selects_the_cell_under_both_endpoints() {
        let sel = TranscriptSelection {
            anchor: pos(2, 5),
            head: pos(2, 5),
            dragging: false,
        };
        // Same-cell endpoints: "empty" for click detection, but still one
        // selected cell (a single-character word selection).
        assert!(sel.is_empty());
        assert_eq!(sel.cols_on_row(2), Some((5, Some(6))));

        let sel = TranscriptSelection {
            anchor: pos(2, 5),
            head: pos(2, 7),
            dragging: false,
        };
        assert_eq!(sel.cols_on_row(2), Some((5, Some(8))));
    }

    #[test]
    fn screen_mapping_offsets_by_area_origin_and_first_visible_row() {
        let area = Rect::new(2, 3, 10, 5);
        assert_eq!(screen_to_selection_pos(area, 100, 6, 4), Some(pos(101, 4)));
        assert_eq!(screen_to_selection_pos(area, 100, 1, 4), None);
        assert_eq!(screen_to_selection_pos(area, 100, 6, 9), None);
    }

    #[test]
    fn clamped_mapping_snaps_outside_coordinates_to_the_edge() {
        let area = Rect::new(2, 3, 10, 5);
        assert_eq!(
            screen_to_selection_pos_clamped(area, 100, 0, 0),
            Some(pos(100, 0))
        );
        assert_eq!(
            screen_to_selection_pos_clamped(area, 100, 50, 50),
            Some(pos(104, 9))
        );
        assert_eq!(
            screen_to_selection_pos_clamped(Rect::new(0, 0, 0, 0), 0, 1, 1),
            None
        );
    }

    #[test]
    fn osc52_sequence_encodes_text_as_base64() {
        assert_eq!(osc52_copy_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(osc52_copy_sequence(""), "\x1b]52;c;\x07");
    }

    #[test]
    fn apply_selection_splits_spans_and_recolors_only_the_selected_range() {
        let line = Line::from(vec![
            Span::styled("abc", Style::default().fg(Color::Red)),
            Span::styled("def", Style::default().fg(Color::Blue)),
        ]);
        let highlighted = apply_selection_to_line(line, 2, Some(4), SEL_BG);
        assert_eq!(
            rendered(&highlighted),
            vec![
                ("ab".to_string(), false, Some(Color::Red)),
                // Foreground survives; only the background changes.
                ("c".to_string(), true, Some(Color::Red)),
                ("d".to_string(), true, Some(Color::Blue)),
                ("ef".to_string(), false, Some(Color::Blue)),
            ]
        );
    }

    #[test]
    fn apply_selection_open_end_highlights_through_end_of_line() {
        let line = Line::from(Span::raw("abcdef"));
        let highlighted = apply_selection_to_line(line, 3, None, SEL_BG);
        let flags: Vec<bool> = highlighted
            .spans
            .iter()
            .map(|span| span.style.bg == Some(SEL_BG))
            .collect();
        assert_eq!(flags, vec![false, true]);
        assert_eq!(highlighted.spans[1].content.as_ref(), "def");
    }

    #[test]
    fn apply_selection_treats_wide_characters_by_leading_column() {
        // "世" occupies columns 0-1, "界" columns 2-3, "x" column 4.
        let line = Line::from(Span::raw("世界x"));
        let highlighted = apply_selection_to_line(line, 2, Some(4), SEL_BG);
        assert_eq!(
            rendered(&highlighted),
            vec![
                ("世".to_string(), false, None),
                ("界".to_string(), true, None),
                ("x".to_string(), false, None),
            ]
        );
    }

    #[test]
    fn slice_row_by_columns_handles_ascii_wide_chars_and_open_end() {
        assert_eq!(slice_row_by_columns("abcdef", 2, Some(4)), "cd");
        assert_eq!(slice_row_by_columns("abcdef", 3, None), "def");
        assert_eq!(slice_row_by_columns("世界x", 2, Some(4)), "界");
        assert_eq!(slice_row_by_columns("世界x", 0, Some(2)), "世");
        assert_eq!(slice_row_by_columns("abc", 5, Some(9)), "");
        assert_eq!(slice_row_by_columns("", 0, None), "");
    }
}
