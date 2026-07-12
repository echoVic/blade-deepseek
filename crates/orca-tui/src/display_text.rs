use unicode_width::UnicodeWidthStr;

pub(crate) fn truncate_to_display_width(text: &str, max_width: usize) -> String {
    let text_width = UnicodeWidthStr::width(text);
    if text_width <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }

    let ellipsis = "…";
    let content_width = max_width.saturating_sub(UnicodeWidthStr::width(ellipsis));
    let mut truncated = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthStr::width(ch.to_string().as_str());
        if width + ch_width > content_width {
            break;
        }
        truncated.push(ch);
        width += ch_width;
    }
    truncated.push_str(ellipsis);
    truncated
}

pub(crate) fn compact_long_text(text: &str, line_width: usize, max_lines: usize) -> String {
    const LONG_TEXT_CHAR_THRESHOLD: usize = 1000;
    const LONG_TEXT_LINE_THRESHOLD: usize = 8;

    let char_count = text.chars().count();
    if char_count <= LONG_TEXT_CHAR_THRESHOLD && text.lines().count() <= LONG_TEXT_LINE_THRESHOLD {
        return text.to_string();
    }

    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let suffix = format!(" [{} chars]", char_count);
    let total_width = line_width.saturating_mul(max_lines);
    let body_width = total_width.saturating_sub(UnicodeWidthStr::width(suffix.as_str()));
    format!(
        "{}{}",
        truncate_to_display_width(&normalized, body_width),
        suffix
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_handles_ascii_and_cjk_display_width() {
        assert_eq!(truncate_to_display_width("abcdef", 4), "abc…");
        assert_eq!(truncate_to_display_width("目标内容很长", 5), "目标…");
        assert_eq!(truncate_to_display_width("目标", 4), "目标");
        assert_eq!(truncate_to_display_width("anything", 0), "");
    }

    #[test]
    fn compact_long_text_bounds_transcript_rows_and_keeps_size() {
        let text = "目标内容".repeat(300);
        let compact = compact_long_text(&text, 40, 3);

        assert!(UnicodeWidthStr::width(compact.as_str()) <= 120);
        assert!(compact.contains("[1200 chars]"));
        assert!(compact.contains('…'));
    }
}
