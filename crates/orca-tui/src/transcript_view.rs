use std::{
    collections::{BTreeSet, VecDeque},
    mem,
};

use ratatui::layout::Alignment;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::{Line, Span, StyledGrapheme};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;
use crate::types::ChatMessage;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, Debug)]
struct StyleRun {
    start: usize,
    style: Style,
}

#[derive(Clone, Debug)]
struct CompactWrappedLine {
    text: String,
    row_boundaries: Vec<usize>,
    style_runs: Vec<StyleRun>,
    alignment: Option<Alignment>,
}

impl CompactWrappedLine {
    fn new(alignment: Option<Alignment>) -> Self {
        Self {
            text: String::new(),
            row_boundaries: vec![0],
            style_runs: Vec::new(),
            alignment,
        }
    }

    fn push_row(&mut self, graphemes: Vec<StyledGrapheme<'_>>) {
        for grapheme in graphemes {
            if self
                .style_runs
                .last()
                .is_none_or(|run| run.style != grapheme.style)
            {
                self.style_runs.push(StyleRun {
                    start: self.text.len(),
                    style: grapheme.style,
                });
            }
            self.text.push_str(grapheme.symbol);
        }
        self.row_boundaries.push(self.text.len());
    }

    fn row_count(&self) -> usize {
        self.row_boundaries.len().saturating_sub(1)
    }

    fn materialize_rows(&self, start: usize, end: usize) -> Vec<Line<'static>> {
        let start = start.min(self.row_count());
        let end = end.min(self.row_count()).max(start);
        (start..end)
            .map(|row| {
                let row_start = self.row_boundaries[row];
                let row_end = self.row_boundaries[row + 1];
                let mut spans = Vec::new();
                if row_start < row_end {
                    let mut run_index = self
                        .style_runs
                        .partition_point(|run| run.start <= row_start)
                        .saturating_sub(1);
                    while let Some(run) = self.style_runs.get(run_index) {
                        let next_start = self
                            .style_runs
                            .get(run_index + 1)
                            .map(|next| next.start)
                            .unwrap_or(self.text.len());
                        let segment_start = row_start.max(run.start);
                        let segment_end = row_end.min(next_start);
                        if segment_start < segment_end {
                            spans.push(Span::styled(
                                self.text[segment_start..segment_end].to_owned(),
                                run.style,
                            ));
                        }
                        if next_start >= row_end {
                            break;
                        }
                        run_index += 1;
                    }
                }
                let mut line = Line::from(spans);
                line.alignment = self.alignment;
                line
            })
            .collect()
    }
}

fn wrap_line_ratatui_compatible(line: &Line<'_>, width: u16) -> CompactWrappedLine {
    let mut wrapped = CompactWrappedLine::new(line.alignment);
    if width == 0 {
        return wrapped;
    }

    // This mirrors ratatui 0.29's WordWrapper with trim=false. Paragraph's
    // scroll offset is u16, so exceptionally tall logical lines get a compact
    // row index and only visible rows are materialized as ratatui Lines.
    let mut pending_line: Vec<StyledGrapheme<'_>> = Vec::new();
    let mut pending_word: Vec<StyledGrapheme<'_>> = Vec::new();
    let mut pending_whitespace: VecDeque<StyledGrapheme<'_>> = VecDeque::new();
    let mut line_width = 0u16;
    let mut word_width = 0u16;
    let mut whitespace_width = 0u16;
    let mut non_whitespace_previous = false;

    for grapheme in line.styled_graphemes(Style::default()) {
        let is_whitespace = grapheme_is_whitespace(&grapheme);
        let symbol_width = grapheme.symbol.width() as u16;
        if symbol_width > width {
            continue;
        }

        let word_found = non_whitespace_previous && is_whitespace;
        let untrimmed_overflow = pending_line.is_empty()
            && word_width
                .saturating_add(whitespace_width)
                .saturating_add(symbol_width)
                > width;

        if word_found || untrimmed_overflow {
            pending_line.extend(pending_whitespace.drain(..));
            line_width = line_width.saturating_add(whitespace_width);
            pending_line.append(&mut pending_word);
            line_width = line_width.saturating_add(word_width);
            whitespace_width = 0;
            word_width = 0;
        }

        let line_full = line_width >= width;
        let pending_word_overflow = symbol_width > 0
            && line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width)
                >= width;

        if line_full || pending_word_overflow {
            let mut remaining_width = width.saturating_sub(line_width);
            wrapped.push_row(mem::take(&mut pending_line));
            line_width = 0;

            while let Some(grapheme) = pending_whitespace.front() {
                let grapheme_width = grapheme.symbol.width() as u16;
                if grapheme_width > remaining_width {
                    break;
                }
                whitespace_width = whitespace_width.saturating_sub(grapheme_width);
                remaining_width = remaining_width.saturating_sub(grapheme_width);
                pending_whitespace.pop_front();
            }

            if is_whitespace && pending_whitespace.is_empty() {
                non_whitespace_previous = false;
                continue;
            }
        }

        if is_whitespace {
            whitespace_width = whitespace_width.saturating_add(symbol_width);
            pending_whitespace.push_back(grapheme);
        } else {
            word_width = word_width.saturating_add(symbol_width);
            pending_word.push(grapheme);
        }
        non_whitespace_previous = !is_whitespace;
    }

    if pending_line.is_empty() && pending_word.is_empty() && !pending_whitespace.is_empty() {
        wrapped.push_row(Vec::new());
    }
    pending_line.extend(pending_whitespace);
    pending_line.append(&mut pending_word);
    if !pending_line.is_empty() {
        wrapped.push_row(pending_line);
    }
    if wrapped.row_count() == 0 {
        wrapped.push_row(Vec::new());
    }

    wrapped
}

fn grapheme_is_whitespace(grapheme: &StyledGrapheme<'_>) -> bool {
    const NBSP: &str = "\u{00a0}";
    const ZWSP: &str = "\u{200b}";

    grapheme.symbol == ZWSP
        || (grapheme.symbol.chars().all(char::is_whitespace) && grapheme.symbol != NBSP)
}

pub(crate) fn viewport_paragraph(lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ThemeIdentity {
    border: Color,
    text: Color,
    muted: Color,
    user: Color,
    success: Color,
    warning: Color,
    error: Color,
    approval: Color,
    diff_add: Color,
    diff_remove: Color,
}

impl From<&Theme> for ThemeIdentity {
    fn from(theme: &Theme) -> Self {
        Self {
            border: theme.border,
            text: theme.text,
            muted: theme.muted,
            user: theme.user,
            success: theme.success,
            warning: theme.warning,
            error: theme.error,
            approval: theme.approval,
            diff_add: theme.diff_add,
            diff_remove: theme.diff_remove,
        }
    }
}

#[derive(Clone, Debug)]
struct CachedMessage {
    revision: u64,
    width: usize,
    theme: ThemeIdentity,
    force_expand: bool,
    spinner_phase: Option<u8>,
    wrapped_lines: Vec<CompactWrappedLine>,
    line_cumulative_heights: Vec<usize>,
    visual_height: usize,
}

impl CachedMessage {
    fn matches(
        &self,
        revision: u64,
        width: usize,
        theme: ThemeIdentity,
        force_expand: bool,
        spinner_phase: Option<u8>,
    ) -> bool {
        self.revision == revision
            && self.width == width
            && self.theme == theme
            && self.force_expand == force_expand
            && self.spinner_phase == spinner_phase
    }

    fn patch_spinner(
        &mut self,
        revision: u64,
        width: usize,
        theme: ThemeIdentity,
        force_expand: bool,
        spinner_phase: Option<u8>,
    ) -> bool {
        let Some(spinner_phase) = spinner_phase else {
            return false;
        };
        if self.revision != revision
            || self.width != width
            || self.theme != theme
            || self.force_expand != force_expand
            || self.spinner_phase.is_none()
            || self.spinner_phase == Some(spinner_phase)
        {
            return false;
        }

        let Some(content) = self.wrapped_lines.first_mut().map(|line| &mut line.text) else {
            return false;
        };
        let Some(old_icon) = content.get(2..).and_then(|rest| rest.chars().next()) else {
            return false;
        };
        let icon_end = 2 + old_icon.len_utf8();
        if content.get(..2) != Some("  ")
            || !SPINNER_FRAMES.contains(&content.get(2..icon_end).unwrap_or_default())
        {
            return false;
        }
        content.replace_range(
            2..icon_end,
            SPINNER_FRAMES[spinner_phase as usize % SPINNER_FRAMES.len()],
        );
        self.spinner_phase = Some(spinner_phase);
        true
    }
}

#[derive(Debug, Default)]
pub(crate) struct TranscriptRenderCache {
    entries: Vec<Option<CachedMessage>>,
    cumulative_heights: Vec<usize>,
    dirty_indices: BTreeSet<usize>,
    spinner_indices: BTreeSet<usize>,
    prepared_width: Option<usize>,
    prepared_theme: Option<ThemeIdentity>,
    prepared_force_expand: Option<bool>,
    prepared_spinner_phase: Option<u8>,
    #[cfg(test)]
    last_prepare_visited: usize,
}

#[derive(Debug, Default)]
pub(crate) struct TranscriptViewport {
    pub lines: Vec<Line<'static>>,
    pub total_height: usize,
    pub scroll_offset: usize,
    #[cfg(test)]
    pub first_message: usize,
    #[cfg(test)]
    pub last_message: usize,
    #[cfg(test)]
    pub rendered_message_count: usize,
}

impl TranscriptRenderCache {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn populated_len(&self) -> usize {
        self.entries.iter().filter(|entry| entry.is_some()).count()
    }

    #[cfg(test)]
    fn oversized_storage_segments(&self, message_index: usize, line_index: usize) -> usize {
        self.entries
            .get(message_index)
            .and_then(Option::as_ref)
            .and_then(|entry| entry.wrapped_lines.get(line_index))
            .map(|line| line.style_runs.len())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn last_prepare_visited(&self) -> usize {
        self.last_prepare_visited
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn reconcile_len(&mut self, len: usize) {
        if self.entries.len() == len {
            return;
        }
        if len < self.entries.len() {
            self.entries.truncate(len);
            self.dirty_indices.retain(|index| *index < len);
            self.spinner_indices.retain(|index| *index < len);
            self.rebuild_cumulative_heights();
            return;
        }
        if self.cumulative_heights.is_empty() {
            self.cumulative_heights.push(0);
        }
        while self.entries.len() < len {
            let index = self.entries.len();
            self.entries.push(None);
            self.dirty_indices.insert(index);
            self.cumulative_heights
                .push(self.cumulative_heights.last().copied().unwrap_or_default());
        }
    }

    pub fn truncate(&mut self, len: usize) {
        self.entries.truncate(len);
        self.dirty_indices.retain(|index| *index < len);
        self.spinner_indices.retain(|index| *index < len);
        self.rebuild_cumulative_heights();
    }

    pub fn retain(&mut self, keep: &[bool]) {
        let old_entries = mem::take(&mut self.entries);
        let old_dirty = mem::take(&mut self.dirty_indices);
        let old_spinners = mem::take(&mut self.spinner_indices);
        self.entries.reserve(old_entries.len().min(keep.len()));

        for (old_index, entry) in old_entries.into_iter().enumerate() {
            if !keep.get(old_index).copied().unwrap_or(false) {
                continue;
            }
            let new_index = self.entries.len();
            if old_dirty.contains(&old_index) || entry.is_none() {
                self.dirty_indices.insert(new_index);
            }
            if old_spinners.contains(&old_index) {
                self.spinner_indices.insert(new_index);
            }
            self.entries.push(entry);
        }
        self.rebuild_cumulative_heights();
    }

    pub fn invalidate(&mut self, index: usize) {
        if index < self.entries.len() {
            self.dirty_indices.insert(index);
        }
    }

    pub fn prepare<F>(
        &mut self,
        messages: &[ChatMessage],
        revisions: &[u64],
        width: usize,
        theme: &Theme,
        tick: u64,
        force_expand: bool,
        mut build_message: F,
    ) where
        F: FnMut(&ChatMessage, &Theme, usize, u64, bool) -> Vec<Line<'static>>,
    {
        let width = width.max(1);
        let theme_identity = ThemeIdentity::from(theme);
        self.reconcile_len(messages.len());
        if self.prepared_width != Some(width)
            || self.prepared_theme != Some(theme_identity)
            || self.prepared_force_expand != Some(force_expand)
        {
            self.dirty_indices.extend(0..messages.len());
            self.prepared_width = Some(width);
            self.prepared_theme = Some(theme_identity);
            self.prepared_force_expand = Some(force_expand);
        }
        let spinner_phase = ((tick / 2) % 10) as u8;
        if self.prepared_spinner_phase != Some(spinner_phase) {
            self.dirty_indices
                .extend(self.spinner_indices.iter().copied());
            self.prepared_spinner_phase = Some(spinner_phase);
        }
        let dirty_indices = mem::take(&mut self.dirty_indices);
        let mut first_height_change: Option<usize> = None;
        #[cfg(test)]
        {
            self.last_prepare_visited = 0;
        }

        for index in dirty_indices {
            let Some(message) = messages.get(index) else {
                continue;
            };
            #[cfg(test)]
            {
                self.last_prepare_visited += 1;
            }
            let revision = revisions.get(index).copied().unwrap_or_default();
            let spinner_phase = message_spinner_phase(message, tick);
            if self.entries[index].as_mut().is_some_and(|cached| {
                cached.patch_spinner(revision, width, theme_identity, force_expand, spinner_phase)
            }) {
                continue;
            }
            let matches = self.entries[index].as_ref().is_some_and(|cached| {
                cached.matches(revision, width, theme_identity, force_expand, spinner_phase)
            });
            if matches {
                continue;
            }

            let lines = build_message(message, theme, width, tick, force_expand);
            let ratatui_width = width.min(u16::MAX as usize) as u16;
            let wrapped_lines = lines
                .iter()
                .map(|line| wrap_line_ratatui_compatible(line, ratatui_width))
                .collect::<Vec<_>>();
            let mut line_cumulative_heights: Vec<usize> =
                Vec::with_capacity(wrapped_lines.len() + 1);
            line_cumulative_heights.push(0);
            for line in &wrapped_lines {
                let next = line_cumulative_heights
                    .last()
                    .copied()
                    .unwrap_or_default()
                    .saturating_add(line.row_count());
                line_cumulative_heights.push(next);
            }
            let visual_height = line_cumulative_heights.last().copied().unwrap_or_default();
            if spinner_phase.is_some() {
                self.spinner_indices.insert(index);
            } else {
                self.spinner_indices.remove(&index);
            }
            if self.entries[index]
                .as_ref()
                .is_none_or(|cached| cached.visual_height != visual_height)
            {
                first_height_change = Some(
                    first_height_change
                        .map(|earliest| earliest.min(index))
                        .unwrap_or(index),
                );
            }
            self.entries[index] = Some(CachedMessage {
                revision,
                width,
                theme: theme_identity,
                force_expand,
                spinner_phase,
                wrapped_lines,
                line_cumulative_heights,
                visual_height,
            });
        }

        if self.cumulative_heights.len() != self.entries.len() + 1 {
            first_height_change = Some(0);
        }
        if let Some(index) = first_height_change {
            self.rebuild_cumulative_heights_from(index);
        }
    }

    pub fn viewport(
        &self,
        first_retained_message: usize,
        requested_scroll: usize,
        visible_height: usize,
    ) -> TranscriptViewport {
        let message_count = self.entries.len();
        let live_start = first_retained_message.min(message_count);
        let base_height = self
            .cumulative_heights
            .get(live_start)
            .copied()
            .unwrap_or_default();
        let absolute_total = self.cumulative_heights.last().copied().unwrap_or_default();
        let total_height = absolute_total.saturating_sub(base_height);
        let max_scroll = total_height.saturating_sub(visible_height);
        let scroll_offset = requested_scroll.min(max_scroll);
        if live_start == message_count || visible_height == 0 {
            return TranscriptViewport {
                total_height,
                scroll_offset,
                #[cfg(test)]
                first_message: live_start,
                #[cfg(test)]
                last_message: live_start,
                ..TranscriptViewport::default()
            };
        }

        let absolute_scroll = base_height.saturating_add(scroll_offset);
        let absolute_end = absolute_scroll
            .saturating_add(visible_height)
            .min(absolute_total);
        let first_visible = self.cumulative_heights[..message_count]
            .partition_point(|height| *height <= absolute_scroll)
            .saturating_sub(1)
            .max(live_start)
            .min(message_count - 1);
        let last_visible = self.cumulative_heights[..message_count]
            .partition_point(|height| *height < absolute_end)
            .max(first_visible + 1)
            .min(message_count);

        let first_message = first_visible;
        let last_message = last_visible;
        let mut lines = Vec::new();
        for message_index in first_message..last_message {
            let Some(cached) = self.entries[message_index].as_ref() else {
                continue;
            };
            if cached.wrapped_lines.is_empty() {
                continue;
            }

            let message_start = self
                .cumulative_heights
                .get(message_index)
                .copied()
                .unwrap_or_default();
            let relative_start = absolute_scroll
                .saturating_sub(message_start)
                .min(cached.visual_height);
            let relative_end = absolute_end
                .saturating_sub(message_start)
                .min(cached.visual_height);
            if relative_start >= relative_end {
                continue;
            }

            let line_count = cached.wrapped_lines.len();
            let first_line = cached.line_cumulative_heights[..line_count]
                .partition_point(|height| *height <= relative_start)
                .saturating_sub(1)
                .min(line_count - 1);
            let last_line = cached.line_cumulative_heights[..line_count]
                .partition_point(|height| *height < relative_end)
                .max(first_line + 1)
                .min(line_count);

            for line_index in first_line..last_line {
                let line_start = cached.line_cumulative_heights[line_index];
                let line_end = cached.line_cumulative_heights[line_index + 1];
                let line_height = line_end.saturating_sub(line_start);
                let visual_start = relative_start.saturating_sub(line_start).min(line_height);
                let visual_end = relative_end.saturating_sub(line_start).min(line_height);
                if visual_start >= visual_end {
                    continue;
                }

                let visual_line = &cached.wrapped_lines[line_index];
                let visual_start = visual_start.min(visual_line.row_count());
                let visual_end = visual_end.min(visual_line.row_count());
                lines.extend(visual_line.materialize_rows(visual_start, visual_end));
            }
        }

        TranscriptViewport {
            lines,
            total_height,
            scroll_offset,
            #[cfg(test)]
            first_message,
            #[cfg(test)]
            last_message,
            #[cfg(test)]
            rendered_message_count: last_message.saturating_sub(first_message),
        }
    }

    fn rebuild_cumulative_heights(&mut self) {
        self.cumulative_heights.clear();
        self.cumulative_heights.reserve(self.entries.len() + 1);
        self.cumulative_heights.push(0);
        for entry in &self.entries {
            let height = entry
                .as_ref()
                .map(|cached| cached.visual_height)
                .unwrap_or_default();
            let next = self
                .cumulative_heights
                .last()
                .copied()
                .unwrap_or_default()
                .saturating_add(height);
            self.cumulative_heights.push(next);
        }
    }

    fn rebuild_cumulative_heights_from(&mut self, start: usize) {
        let start = start.min(self.entries.len());
        if self.cumulative_heights.len() < start + 1 {
            self.rebuild_cumulative_heights();
            return;
        }
        self.cumulative_heights.truncate(start + 1);
        for entry in &self.entries[start..] {
            let height = entry
                .as_ref()
                .map(|cached| cached.visual_height)
                .unwrap_or_default();
            let next = self
                .cumulative_heights
                .last()
                .copied()
                .unwrap_or_default()
                .saturating_add(height);
            self.cumulative_heights.push(next);
        }
    }
}

fn message_spinner_phase(message: &ChatMessage, tick: u64) -> Option<u8> {
    match message {
        ChatMessage::ToolCall { status, .. }
            if matches!(status.as_str(), "running" | "receiving") =>
        {
            Some(((tick / 2) % 10) as u8)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use ratatui::buffer::Buffer;
    use ratatui::layout::{Alignment, Rect};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Paragraph, Widget, Wrap};
    use unicode_width::UnicodeWidthStr;

    use super::{TranscriptRenderCache, viewport_paragraph, wrap_line_ratatui_compatible};
    use crate::theme::Theme;
    use crate::types::ChatMessage;
    use crate::ui::build_lines_for_messages;

    fn theme() -> Theme {
        Theme::named(orca_core::config::ThemeName::Dark)
    }

    fn render_lines(lines: Vec<Line<'static>>, width: u16, height: u16) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, &mut buffer);
        buffer
    }

    #[test]
    fn compact_wrapper_matches_ratatui_cells_for_unicode_whitespace_and_styles() {
        let line = Line::from(vec![
            Span::styled("alpha  ", Style::default().fg(Color::Red)),
            Span::styled(
                "世界\u{00a0}wide\u{200b}word ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("tail-tail-tail", Style::default().fg(Color::Blue)),
        ])
        .alignment(Alignment::Center);
        let width = 9;
        let paragraph = Paragraph::new(line.clone()).wrap(Wrap { trim: false });
        let height = paragraph.line_count(width) as u16;
        let expected = render_lines(vec![line.clone()], width, height);

        let compact = wrap_line_ratatui_compatible(&line, width);
        assert_eq!(compact.row_count(), height as usize);
        let actual = render_lines(
            compact.materialize_rows(0, compact.row_count()),
            width,
            height,
        );

        assert_eq!(actual, expected);
    }

    #[test]
    fn whitespace_heavy_tail_matches_original_paragraph_without_double_wrapping() {
        let source_lines = vec![Line::from(" ".repeat(2_050)), Line::from("tail")];
        let width = 1;
        let visible_height = 10usize;
        let source = Paragraph::new(source_lines.clone()).wrap(Wrap { trim: false });
        let total_height = source.line_count(width);
        let mut expected = Buffer::empty(Rect::new(0, 0, width, visible_height as u16));
        source
            .scroll(((total_height - visible_height) as u16, 0))
            .render(expected.area, &mut expected);

        let messages = vec![ChatMessage::System("whitespace".to_string())];
        let revisions = vec![1];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();
        cache.prepare(
            &messages,
            &revisions,
            width as usize,
            &theme,
            0,
            false,
            |_, _, _, _, _| source_lines.clone(),
        );
        let viewport = cache.viewport(0, usize::MAX, visible_height);
        let mut actual = Buffer::empty(expected.area);
        viewport_paragraph(viewport.lines).render(actual.area, &mut actual);

        assert_eq!(actual, expected);
    }

    fn prepare_with_counters(
        cache: &mut TranscriptRenderCache,
        messages: &[ChatMessage],
        revisions: &[u64],
        width: usize,
        tick: u64,
        message_builds: &Cell<usize>,
        markdown_parses: &Cell<usize>,
    ) {
        let theme = theme();
        cache.prepare(
            messages,
            revisions,
            width,
            &theme,
            tick,
            false,
            |message, theme, width, tick, force_expand| {
                message_builds.set(message_builds.get() + 1);
                if matches!(message, ChatMessage::Assistant(_)) {
                    markdown_parses.set(markdown_parses.get() + 1);
                }
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

    #[test]
    fn scroll_only_second_frame_builds_and_parses_zero_messages() {
        let messages = vec![
            ChatMessage::Assistant("# Cached\n\nMarkdown body".to_string()),
            ChatMessage::User("next prompt".to_string()),
            ChatMessage::Assistant("final answer".to_string()),
        ];
        let revisions = vec![1, 2, 3];
        let builds = Cell::new(0);
        let parses = Cell::new(0);
        let mut cache = TranscriptRenderCache::default();

        prepare_with_counters(&mut cache, &messages, &revisions, 40, 0, &builds, &parses);
        let _ = cache.viewport(0, 0, 4);
        assert_eq!(builds.get(), 3);
        assert_eq!(parses.get(), 2);

        builds.set(0);
        parses.set(0);
        prepare_with_counters(&mut cache, &messages, &revisions, 40, 0, &builds, &parses);
        let _ = cache.viewport(0, 2, 4);

        assert_eq!(builds.get(), 0);
        assert_eq!(parses.get(), 0);
        assert_eq!(cache.last_prepare_visited(), 0);
    }

    #[test]
    fn assistant_delta_rebuilds_only_the_final_message() {
        let mut messages = vec![
            ChatMessage::User("question".to_string()),
            ChatMessage::Assistant("first".to_string()),
            ChatMessage::Assistant("stream".to_string()),
        ];
        let mut revisions = vec![1, 2, 3];
        let builds = Cell::new(0);
        let parses = Cell::new(0);
        let mut cache = TranscriptRenderCache::default();

        prepare_with_counters(&mut cache, &messages, &revisions, 32, 0, &builds, &parses);
        builds.set(0);
        parses.set(0);

        let ChatMessage::Assistant(text) = &mut messages[2] else {
            unreachable!();
        };
        text.push_str("ing delta");
        revisions[2] = 4;
        cache.invalidate(2);
        prepare_with_counters(&mut cache, &messages, &revisions, 32, 0, &builds, &parses);

        assert_eq!(builds.get(), 1);
        assert_eq!(parses.get(), 1);
    }

    #[test]
    fn tick_patches_running_or_receiving_spinners_without_rebuilding_messages() {
        let tool = |id: &str, status: &str| ChatMessage::ToolCall {
            id: id.to_string(),
            name: "read".to_string(),
            target: None,
            status: status.to_string(),
            output: None,
            diff: None,
            kind: None,
            expanded: false,
        };
        let messages = vec![
            ChatMessage::Assistant("stable markdown".to_string()),
            tool("running", "running"),
            tool("receiving", "receiving"),
            tool("completed", "completed"),
        ];
        let revisions = vec![1, 2, 3, 4];
        let builds = Cell::new(0);
        let parses = Cell::new(0);
        let mut cache = TranscriptRenderCache::default();

        prepare_with_counters(&mut cache, &messages, &revisions, 40, 0, &builds, &parses);
        let before = cache.entries[1].as_ref().unwrap().wrapped_lines[0]
            .text
            .clone();
        builds.set(0);
        parses.set(0);
        prepare_with_counters(&mut cache, &messages, &revisions, 40, 2, &builds, &parses);
        let after = cache.entries[1].as_ref().unwrap().wrapped_lines[0]
            .text
            .clone();

        assert_eq!(builds.get(), 0);
        assert_eq!(parses.get(), 0);
        assert_ne!(before, after);
    }

    #[test]
    fn thousands_of_messages_render_a_bounded_viewport_window() {
        let messages = (0..5_000)
            .map(|index| ChatMessage::System(format!("message {index}")))
            .collect::<Vec<_>>();
        let revisions = (1..=messages.len() as u64).collect::<Vec<_>>();
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            80,
            &theme,
            0,
            false,
            |message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone()), Line::from("")],
                _ => unreachable!(),
            },
        );
        let viewport = cache.viewport(0, 7_000, 20);

        assert_eq!(cache.len(), 5_000);
        assert!(viewport.rendered_message_count <= 12);
        assert!(viewport.last_message <= viewport.first_message + 12);
    }

    #[test]
    fn offsets_above_u16_max_remain_representable_and_navigable() {
        let messages = (0..40_000)
            .map(|index| ChatMessage::System(format!("message {index}")))
            .collect::<Vec<_>>();
        let revisions = (1..=messages.len() as u64).collect::<Vec<_>>();
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            80,
            &theme,
            0,
            false,
            |message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone()), Line::from("")],
                _ => unreachable!(),
            },
        );
        let viewport = cache.viewport(0, 70_000, 20);

        assert_eq!(viewport.total_height, 80_000);
        assert_eq!(viewport.scroll_offset, 70_000);
        assert!(viewport.first_message > u16::MAX as usize / 2);
    }

    #[test]
    fn retained_prefix_rebases_total_height_and_visible_message_indices() {
        let messages = (0..100)
            .map(|index| ChatMessage::System(format!("message {index}")))
            .collect::<Vec<_>>();
        let revisions = (1..=messages.len() as u64).collect::<Vec<_>>();
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            80,
            &theme,
            0,
            false,
            |message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone())],
                _ => unreachable!(),
            },
        );
        let viewport = cache.viewport(50, 0, 10);

        assert_eq!(viewport.total_height, 50);
        assert_eq!(viewport.first_message, 50);
        assert_eq!(viewport.last_message, 60);
        assert_eq!(viewport.lines[0].spans[0].content, "message 50");
        assert_eq!(viewport.lines[9].spans[0].content, "message 59");
    }

    #[test]
    fn tall_message_discards_complete_logical_lines_before_materializing_rows() {
        let messages = vec![
            ChatMessage::System("tall".to_string()),
            ChatMessage::System("tail".to_string()),
        ];
        let revisions = vec![1, 2];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            80,
            &theme,
            0,
            false,
            |message, _, _, _, _| match message {
                ChatMessage::System(text) if text == "tall" => (0..70_000)
                    .map(|index| Line::from(format!("line {index}")))
                    .collect(),
                ChatMessage::System(text) => vec![Line::from(text.clone())],
                _ => unreachable!(),
            },
        );
        let viewport = cache.viewport(0, 69_980, 20);

        assert_eq!(viewport.scroll_offset, 69_980);
        assert!(viewport.lines.len() <= 21);
        assert_eq!(viewport.lines[0].spans[0].content, "line 69980");
    }

    #[test]
    fn tall_message_bounds_logical_lines_at_the_top_of_the_viewport() {
        let messages = vec![ChatMessage::System("tall".to_string())];
        let revisions = vec![1];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            80,
            &theme,
            0,
            false,
            |_, _, _, _, _| {
                (0..70_000)
                    .map(|index| Line::from(format!("line {index}")))
                    .collect()
            },
        );
        let viewport = cache.viewport(0, 0, 20);

        assert!(viewport.lines.len() <= 20);
        assert_eq!(viewport.lines[0].spans[0].content, "line 0");
        assert_eq!(viewport.lines[19].spans[0].content, "line 19");
    }

    #[test]
    fn trimming_stops_after_the_first_partially_visible_wrapped_line() {
        let messages = vec![
            ChatMessage::System("wrapped".to_string()),
            ChatMessage::System("later".to_string()),
        ];
        let revisions = vec![1, 2];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            5,
            &theme,
            0,
            false,
            |message, _, _, _, _| match message {
                ChatMessage::System(text) if text == "wrapped" => {
                    vec![Line::from("abcdefghijklmnopqrstuvwxy")]
                }
                ChatMessage::System(_) => vec![Line::from("b"), Line::from("c")],
                _ => unreachable!(),
            },
        );
        let viewport = cache.viewport(0, 3, 4);

        assert_eq!(viewport.lines.len(), 4);
        assert_eq!(viewport.lines[0].spans[0].content, "pqrst");
        assert_eq!(viewport.lines[1].spans[0].content, "uvwxy");
        assert_eq!(viewport.lines[2].spans[0].content, "b");
        assert_eq!(viewport.lines[3].spans[0].content, "c");
    }

    #[test]
    fn one_logical_line_above_u16_rows_rebases_to_the_requested_row() {
        let body = format!("{}Z{}", "a".repeat(69_980), "b".repeat(19));
        let messages = vec![ChatMessage::System("huge".to_string())];
        let revisions = vec![1];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            1,
            &theme,
            0,
            false,
            |_, _, _, _, _| vec![Line::from(body.clone())],
        );
        let viewport = cache.viewport(0, 69_980, 20);

        assert_eq!(viewport.total_height, 70_000);
        assert_eq!(viewport.scroll_offset, 69_980);
        assert_eq!(viewport.lines[0].spans[0].content, "Z");
    }

    #[test]
    fn oversized_logical_line_bounds_materialized_content_at_the_top() {
        let body = "a".repeat(70_000);
        let messages = vec![ChatMessage::System("huge".to_string())];
        let revisions = vec![1];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            1,
            &theme,
            0,
            false,
            |_, _, _, _, _| vec![Line::from(body.clone())],
        );
        let viewport = cache.viewport(0, 0, 20);
        let materialized_width = viewport
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.width())
            .sum::<usize>();

        assert_eq!(viewport.lines.len(), 20);
        assert_eq!(materialized_width, 20);
        assert!(
            cache.oversized_storage_segments(0, 0) <= 2,
            "the cache must not retain one Line/Span/String allocation per visual row"
        );
    }

    #[test]
    fn terminal_width_change_recomputes_layout_and_clamps_scroll() {
        let messages = vec![ChatMessage::Assistant(
            "a deliberately long markdown paragraph that wraps differently".repeat(8),
        )];
        let revisions = vec![1];
        let builds = Cell::new(0);
        let parses = Cell::new(0);
        let mut cache = TranscriptRenderCache::default();

        prepare_with_counters(&mut cache, &messages, &revisions, 80, 0, &builds, &parses);
        let wide_height = cache.viewport(0, usize::MAX, 5).total_height;
        builds.set(0);
        parses.set(0);

        prepare_with_counters(&mut cache, &messages, &revisions, 20, 0, &builds, &parses);
        let narrow = cache.viewport(0, usize::MAX, 5);

        assert_eq!(builds.get(), 1);
        assert_eq!(parses.get(), 1);
        assert!(narrow.total_height > wide_height);
        assert_eq!(narrow.scroll_offset, narrow.total_height.saturating_sub(5));
    }
}
