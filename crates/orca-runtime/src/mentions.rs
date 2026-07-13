use std::fs;
use std::path::{Path, PathBuf};

const MAX_MENTION_BYTES: usize = 32 * 1024;

pub fn expand_file_mentions(input: &str, cwd: &Path) -> Result<String, String> {
    let mentions = find_mentions(input);
    if mentions.is_empty() {
        return Ok(input.to_string());
    }

    let mut blocks = Vec::new();
    for mention in mentions {
        let target = MentionTarget::parse(&mention)?;
        let path = match resolve_mention_path(cwd, target.path) {
            Ok(path) => path,
            Err(_) if is_plain_at_word(target.path) => continue,
            Err(error) => return Err(error),
        };
        if is_binary_file(&path) {
            return Err(format!("@{mention} appears to be a binary file"));
        }
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("failed to read @{}: {error}", mention))?;
        let content = select_lines(&content, target.range, &mention)?;
        let (content, truncated) = truncate_content(content);
        let marker = if truncated {
            "\n[... truncated ...]"
        } else {
            ""
        };
        let line_attr = target
            .range
            .map(|range| format!(r#" range="{}""#, escape_attr(&range.display())))
            .unwrap_or_default();
        blocks.push(format!(
            r#"<file path="{}"{}>
{}{}</file>"#,
            escape_attr(target.path),
            line_attr,
            content,
            marker
        ));
    }

    if blocks.is_empty() {
        return Ok(input.to_string());
    }

    Ok(format!("{}\n\n{}", input, blocks.join("\n\n")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionToken {
    pub start: usize,
    pub end: usize,
    pub query: String,
    pub quoted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionEdit {
    pub text: String,
    pub cursor: usize,
}

pub fn current_mention_token(input: &str) -> Option<MentionToken> {
    mention_token_at_cursor(input, input.len())
}

pub fn mention_token_at_cursor(input: &str, cursor: usize) -> Option<MentionToken> {
    if cursor > input.len() || !input.is_char_boundary(cursor) {
        return None;
    }

    let mut active = None;
    for (start, ch) in input.char_indices() {
        if ch != '@'
            || (start > 0
                && !input[..start]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace))
        {
            continue;
        }

        let query_start = start + 1;
        if input[query_start..].starts_with('"') {
            let query_start = query_start + 1;
            let closing_quote = input[query_start..]
                .find('"')
                .map(|offset| query_start + offset);
            let query_end = closing_quote.unwrap_or(input.len());
            if (query_start..=query_end).contains(&cursor) {
                active = Some(MentionToken {
                    start,
                    end: closing_quote.map_or(query_end, |end| end + 1),
                    query: input[query_start..query_end].to_string(),
                    quoted: true,
                });
            }
            continue;
        }

        let end = input[query_start..]
            .find(char::is_whitespace)
            .map(|offset| query_start + offset)
            .unwrap_or(input.len());
        if !(query_start..=end).contains(&cursor) {
            continue;
        }
        let query = &input[query_start..end];
        if query.starts_with('@') || query.starts_with("http://") || query.starts_with("https://") {
            continue;
        }
        active = Some(MentionToken {
            start,
            end,
            query: query.to_string(),
            quoted: false,
        });
    }
    active
}

pub fn complete_file_mention_from_candidates(input: &str, candidates: &[String]) -> Option<String> {
    complete_file_mention_from_candidates_at_cursor(input, input.len(), candidates)
        .map(|edit| edit.text)
}

pub fn complete_file_mention_from_candidates_at_cursor(
    input: &str,
    cursor: usize,
    candidates: &[String],
) -> Option<MentionEdit> {
    let token = mention_token_at_cursor(input, cursor)?;
    let replacement = if candidates.len() == 1 {
        candidates[0].clone()
    } else {
        let common = common_prefix(candidates)?;
        if !common.starts_with(&token.query) {
            return None;
        }
        common
    };
    if replacement == token.query {
        return None;
    }
    let mut completed = String::new();
    completed.push_str(&input[..token.start]);
    if token.quoted {
        completed.push_str("@\"");
    } else {
        completed.push('@');
    }
    completed.push_str(&replacement);
    let completed_cursor = completed.len();
    if token.quoted && input[token.start..token.end].ends_with('"') {
        completed.push('"');
    }
    completed.push_str(&input[token.end..]);
    Some(MentionEdit {
        text: completed,
        cursor: completed_cursor,
    })
}

pub fn apply_mention_selection(input: &str, candidate: &str) -> String {
    apply_mention_selection_at_cursor(input, input.len(), candidate)
        .map_or_else(|| input.to_string(), |edit| edit.text)
}

pub fn apply_mention_selection_at_cursor(
    input: &str,
    cursor: usize,
    candidate: &str,
) -> Option<MentionEdit> {
    let token = mention_token_at_cursor(input, cursor)?;
    let has_space = candidate.contains(' ');
    let mut result = String::new();
    result.push_str(&input[..token.start]);
    if token.quoted || has_space {
        result.push_str("@\"");
        result.push_str(candidate);
        if !candidate.ends_with('/') {
            result.push('"');
        }
    } else {
        result.push('@');
        result.push_str(candidate);
    }
    let inserted_end = result.len();
    let suffix = &input[token.end..];
    let mut result_cursor = inserted_end;
    if !candidate.ends_with('/') {
        if let Some(whitespace) = suffix.chars().next().filter(|ch| ch.is_whitespace()) {
            result_cursor += whitespace.len_utf8();
        } else {
            result.push(' ');
            result_cursor += 1;
        }
    }
    result.push_str(suffix);
    Some(MentionEdit {
        text: result,
        cursor: result_cursor,
    })
}

fn find_mentions(input: &str) -> Vec<String> {
    let mut seen = Vec::new();
    let tokens = extract_mention_tokens(input);
    for mention in tokens {
        if !seen.contains(&mention) {
            seen.push(mention);
        }
    }
    seen
}

fn extract_mention_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();

    let mut chars = input.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '@' {
            continue;
        }
        if i > 0 && !input[..i].ends_with(char::is_whitespace) {
            continue;
        }
        if chars.peek().map(|(_, c)| *c) == Some('"') {
            chars.next();
            let start = chars.peek().map(|(i, _)| *i).unwrap_or(input.len());
            let mut end = start;
            for (j, ch) in chars.by_ref() {
                if ch == '"' {
                    break;
                }
                end = j + ch.len_utf8();
            }
            let path = &input[start..end];
            if !path.is_empty() {
                tokens.push(path.to_string());
            }
        } else {
            let start = i + 1;
            let end = input[start..]
                .find(char::is_whitespace)
                .map(|j| start + j)
                .unwrap_or(input.len());
            let raw = &input[start..end];
            let mention = raw.trim_end_matches(|c: char| {
                matches!(c, ',' | '.' | ':' | ';' | ')' | ']' | '}' | '"' | '\'')
            });
            if mention.is_empty()
                || mention.starts_with('@')
                || mention.starts_with("http://")
                || mention.starts_with("https://")
            {
                continue;
            }
            tokens.push(mention.to_string());
        }
    }
    tokens
}

fn is_plain_at_word(value: &str) -> bool {
    !value.contains('/')
        && !value.contains('\\')
        && !value.contains('.')
        && !value.contains('#')
        && !value.starts_with('~')
}

fn common_prefix(values: &[String]) -> Option<String> {
    let first = values.first()?.as_str();
    let mut end = first.len();
    for value in values.iter().skip(1) {
        end = end.min(value.len());
        while end > 0 && !value.starts_with(&first[..end]) {
            end -= 1;
            while !first.is_char_boundary(end) {
                end -= 1;
            }
        }
    }
    Some(first[..end].to_string())
}

fn resolve_mention_path(cwd: &Path, mention: &str) -> Result<PathBuf, String> {
    let cwd = cwd
        .canonicalize()
        .map_err(|error| format!("failed to resolve cwd: {error}"))?;
    let candidate = cwd.join(mention);
    let path = candidate
        .canonicalize()
        .map_err(|error| format!("failed to resolve @{mention}: {error}"))?;
    if !path.starts_with(&cwd) {
        return Err(format!("@{mention} is outside the workspace"));
    }
    if !path.is_file() {
        return Err(format!("@{mention} is not a file"));
    }
    Ok(path)
}

#[derive(Clone, Copy)]
struct MentionTarget<'a> {
    path: &'a str,
    range: Option<LineRange>,
}

impl<'a> MentionTarget<'a> {
    fn parse(mention: &'a str) -> Result<Self, String> {
        let Some((path, suffix)) = mention.rsplit_once("#L") else {
            return Ok(Self {
                path: mention,
                range: None,
            });
        };
        if path.is_empty() {
            return Err(format!("@{mention} is missing a file path"));
        }
        let range = LineRange::parse(suffix).ok_or_else(|| {
            format!("@{mention} has an invalid line reference; use #L10 or #L10-L20")
        })?;
        Ok(Self {
            path,
            range: Some(range),
        })
    }
}

#[derive(Clone, Copy)]
struct LineRange {
    start: usize,
    end: usize,
}

impl LineRange {
    fn parse(value: &str) -> Option<Self> {
        let (start, end) = value.split_once('-').unwrap_or((value, value));
        let start = start.parse::<usize>().ok()?;
        let end = end.strip_prefix('L').unwrap_or(end);
        let end = end.parse::<usize>().ok()?;
        if start == 0 || end == 0 || end < start {
            return None;
        }
        Some(Self { start, end })
    }

    fn display(self) -> String {
        if self.start == self.end {
            format!("L{}", self.start)
        } else {
            format!("L{}-L{}", self.start, self.end)
        }
    }
}

fn select_lines<'a>(
    content: &'a str,
    range: Option<LineRange>,
    mention: &str,
) -> Result<&'a str, String> {
    let Some(range) = range else {
        return Ok(content);
    };

    let line_spans = line_spans(content);
    let total = line_spans.len();
    if range.start > total {
        return Err(format!(
            "@{mention} starts past the end of the file (only {total} lines)"
        ));
    }
    if range.end > total {
        return Err(format!(
            "@{mention} ends past the end of the file (only {total} lines)"
        ));
    }

    let start = line_spans[range.start - 1].0;
    let end = line_spans[range.end - 1].1;
    Ok(&content[start..end])
}

fn line_spans(content: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for segment in content.split_inclusive('\n') {
        let raw_end = start + segment.len();
        let end = if segment.ends_with('\n') {
            let before_lf = raw_end - 1;
            if before_lf > start && content.as_bytes()[before_lf - 1] == b'\r' {
                before_lf - 1
            } else {
                before_lf
            }
        } else {
            raw_end
        };
        spans.push((start, end));
        start = raw_end;
    }
    spans
}

fn truncate_content(content: &str) -> (&str, bool) {
    if content.len() <= MAX_MENTION_BYTES {
        return (content, false);
    }
    let mut end = MAX_MENTION_BYTES;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    (&content[..end], true)
}

fn is_binary_file(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    use std::io::Read;
    let mut buf = [0u8; 512];
    let n = match file.take(512).read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    buf[..n].contains(&0)
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_relative_file_mentions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "hello").unwrap();

        let expanded = expand_file_mentions("read @notes.txt", dir.path()).unwrap();

        assert!(expanded.contains("read @notes.txt"));
        assert!(expanded.contains(r#"<file path="notes.txt">"#));
        assert!(expanded.contains("hello</file>"));
    }

    #[test]
    fn expands_line_mentions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "one\ntwo\nthree\n").unwrap();

        let expanded = expand_file_mentions("read @notes.txt#L2-L3", dir.path()).unwrap();

        assert!(expanded.contains(r#"<file path="notes.txt" range="L2-L3">"#));
        assert!(expanded.contains("two\nthree</file>"));
        assert!(!expanded.contains("\none\ntwo"));
    }

    #[test]
    fn ignores_urls_and_plain_at_words_without_files() {
        let dir = tempfile::tempdir().unwrap();
        let expanded =
            expand_file_mentions("see @https://example.com and @alice", dir.path()).unwrap();
        assert_eq!(expanded, "see @https://example.com and @alice");
    }

    #[test]
    fn rejects_mentions_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let outside_path = dir.path().join("orca-outside-mention.txt");
        fs::write(&outside_path, "outside").unwrap();

        let err =
            expand_file_mentions("read @../orca-outside-mention.txt", &workspace).unwrap_err();

        let _ = fs::remove_file(outside_path);
        assert!(err.contains("outside the workspace"));
    }

    #[test]
    fn completes_unique_file_mention() {
        let completed =
            complete_file_mention_from_candidates("read @no", &["notes.txt".to_string()]).unwrap();

        assert_eq!(completed, "read @notes.txt");
    }

    #[test]
    fn completes_common_prefix_for_multiple_mentions() {
        let completed = complete_file_mention_from_candidates(
            "read @a",
            &["alpha-one.txt".to_string(), "alpha-two.txt".to_string()],
        )
        .unwrap();

        assert_eq!(completed, "read @alpha-");
    }

    #[test]
    fn handles_crlf_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("win.txt"), "one\r\ntwo\r\nthree\r\n").unwrap();

        let expanded = expand_file_mentions("read @win.txt#L2", dir.path()).unwrap();

        assert!(expanded.contains("two</file>"));
        assert!(!expanded.contains("\r"));
    }

    #[test]
    fn rejects_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("image.png"), b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();

        let err = expand_file_mentions("read @image.png", dir.path()).unwrap_err();

        assert!(err.contains("binary file"));
    }

    #[test]
    fn preserves_mention_appearance_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b.txt"), "beta").unwrap();
        fs::write(dir.path().join("a.txt"), "alpha").unwrap();

        let expanded = expand_file_mentions("see @b.txt and @a.txt", dir.path()).unwrap();

        let b_pos = expanded.find(r#"<file path="b.txt">"#).unwrap();
        let a_pos = expanded.find(r#"<file path="a.txt">"#).unwrap();
        assert!(b_pos < a_pos);
    }

    #[test]
    fn expands_quoted_path_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("my dir");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("file.txt"), "content").unwrap();

        let expanded = expand_file_mentions(r#"read @"my dir/file.txt""#, dir.path()).unwrap();

        assert!(expanded.contains(r#"<file path="my dir/file.txt">"#));
        assert!(expanded.contains("content</file>"));
    }

    #[test]
    fn line_range_error_includes_line_count() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("short.txt"), "one\ntwo\n").unwrap();

        let err = expand_file_mentions("read @short.txt#L5", dir.path()).unwrap_err();

        assert!(err.contains("only 2 lines"));
    }

    #[test]
    fn completes_quoted_mention() {
        let completed = complete_file_mention_from_candidates(
            r#"read @"my dir/no"#,
            &["my dir/notes.txt".to_string()],
        )
        .unwrap();

        assert!(completed.contains("my dir/notes.txt"));
    }

    #[test]
    fn current_token_exposes_range_query_and_quote_state() {
        let token = current_mention_token(r#"review @"src/ma"#).unwrap();

        assert_eq!(
            token,
            MentionToken {
                start: 7,
                end: 15,
                query: "src/ma".to_string(),
                quoted: true,
            }
        );
    }

    #[test]
    fn token_at_cursor_owns_the_earlier_mention_instead_of_the_final_token() {
        let input = "compare @src/lib.rs with @README.md";
        let cursor = input.find("lib.rs").unwrap() + 3;

        let token = mention_token_at_cursor(input, cursor).unwrap();

        assert_eq!(token.start, 8);
        assert_eq!(token.query, "src/lib.rs");
    }

    #[test]
    fn closed_quoted_mention_is_inactive_after_the_closing_quote() {
        let input = r#"review @"my dir/file.rs" "#;

        assert!(mention_token_at_cursor(input, input.len()).is_none());
        assert_eq!(
            mention_token_at_cursor(input, input.find("file.rs").unwrap() + 2)
                .unwrap()
                .query,
            "my dir/file.rs"
        );
    }

    #[test]
    fn selection_at_an_earlier_cursor_preserves_the_remaining_composer_text() {
        let input = "compare @sr with @README.md";
        let edit = apply_mention_selection_at_cursor(input, 11, "src/lib.rs").unwrap();

        assert_eq!(edit.text, "compare @src/lib.rs with @README.md");
        assert_eq!(&edit.text[..edit.cursor], "compare @src/lib.rs ");
    }

    #[test]
    fn completes_from_existing_snapshot_without_searching() {
        let completed = complete_file_mention_from_candidates(
            "review @src/m",
            &["src/main.rs".to_string(), "src/match.rs".to_string()],
        )
        .unwrap();

        assert_eq!(completed, "review @src/ma");
    }

    #[test]
    fn selecting_a_directory_keeps_the_mention_open_for_browsing() {
        assert_eq!(apply_mention_selection("review @s", "src/"), "review @src/");
        assert_eq!(
            apply_mention_selection(r#"review @"my"#, "my dir/"),
            r#"review @"my dir/"#
        );
    }

    #[test]
    fn selecting_a_quoted_file_closes_the_token_and_moves_past_whitespace() {
        let edit = apply_mention_selection_at_cursor(r#"review @"my" later"#, 11, "my dir/file.rs")
            .unwrap();

        assert_eq!(edit.text, r#"review @"my dir/file.rs" later"#);
        assert!(mention_token_at_cursor(&edit.text, edit.cursor).is_none());
    }
}
