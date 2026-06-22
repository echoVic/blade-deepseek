use std::fs;
use std::path::{Path, PathBuf};

const MAX_MENTION_BYTES: usize = 32 * 1024;
const MAX_FUZZY_MENTION_CANDIDATES: usize = 8;
const MAX_FUZZY_MENTION_VISITS: usize = 2000;

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

pub fn complete_file_mention(input: &str, cwd: &Path) -> Option<String> {
    let (start, prefix) = current_mention_prefix(input)?;
    let matches = mention_matches(cwd, prefix);
    if matches.is_empty() {
        return None;
    }
    let replacement = if matches.len() == 1 {
        matches[0].clone()
    } else {
        common_prefix(&matches)?
    };
    if replacement == prefix {
        return None;
    }
    let token = &input[start..];
    let is_quoted = token.starts_with("@\"");
    let prefix_len_in_input = if is_quoted {
        2 + prefix.len()
    } else {
        1 + prefix.len()
    };
    let mut completed = String::new();
    completed.push_str(&input[..start]);
    if is_quoted {
        completed.push_str("@\"");
    } else {
        completed.push('@');
    }
    completed.push_str(&replacement);
    completed.push_str(&input[start + prefix_len_in_input..]);
    Some(completed)
}

pub fn list_mention_candidates(input: &str, cwd: &Path) -> Vec<String> {
    let Some((_, prefix)) = current_mention_prefix(input) else {
        return Vec::new();
    };
    mention_matches(cwd, prefix)
}

pub fn apply_mention_selection(input: &str, candidate: &str) -> String {
    let Some((start, _prefix)) = current_mention_prefix(input) else {
        return input.to_string();
    };
    let token = &input[start..];
    let is_quoted = token.starts_with("@\"");
    let has_space = candidate.contains(' ');
    let mut result = String::new();
    result.push_str(&input[..start]);
    if is_quoted || has_space {
        result.push_str("@\"");
        result.push_str(candidate);
        if !candidate.ends_with('/') {
            result.push('"');
        }
    } else {
        result.push('@');
        result.push_str(candidate);
    }
    if !candidate.ends_with('/') {
        result.push(' ');
    }
    result
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

fn current_mention_prefix(input: &str) -> Option<(usize, &str)> {
    let cursor = input.len();
    if let Some(at_quote) = input[..cursor].rfind("@\"") {
        let prefix_start = at_quote + 2;
        let prefix = &input[prefix_start..cursor];
        return Some((at_quote, prefix));
    }
    let token_start = input[..cursor]
        .rfind(char::is_whitespace)
        .map(|index| index + 1)
        .unwrap_or(0);
    let token = &input[token_start..cursor];
    let after_at = token.strip_prefix('@')?;
    if after_at.starts_with('@')
        || after_at.starts_with("http://")
        || after_at.starts_with("https://")
    {
        return None;
    }
    Some((token_start, after_at))
}

fn mention_matches(cwd: &Path, prefix: &str) -> Vec<String> {
    let exact_matches = prefix_mention_matches(cwd, prefix);
    if !exact_matches.is_empty() || prefix.contains('/') || prefix.is_empty() {
        return exact_matches;
    }
    fuzzy_mention_matches(cwd, prefix)
}

fn prefix_mention_matches(cwd: &Path, prefix: &str) -> Vec<String> {
    let (dir_prefix, file_prefix) = prefix.rsplit_once('/').unwrap_or(("", prefix));
    let search_dir = if dir_prefix.is_empty() {
        cwd.to_path_buf()
    } else {
        match resolve_mention_dir(cwd, dir_prefix) {
            Ok(dir) => dir,
            Err(_) => return Vec::new(),
        }
    };
    let entries = match fs::read_dir(search_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let matches_prefix = if file_prefix.chars().any(|c| c.is_uppercase()) {
            name.starts_with(file_prefix)
        } else {
            name.to_lowercase().starts_with(&file_prefix.to_lowercase())
        };
        if !matches_prefix {
            continue;
        }
        let mut candidate = String::new();
        if !dir_prefix.is_empty() {
            candidate.push_str(dir_prefix);
            candidate.push('/');
        }
        candidate.push_str(name);
        if entry.path().is_dir() {
            candidate.push('/');
        }
        matches.push(candidate);
    }
    matches.sort();
    matches
}

fn fuzzy_mention_matches(cwd: &Path, query: &str) -> Vec<String> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let Ok(cwd) = cwd.canonicalize() else {
        return Vec::new();
    };
    let mut scored = Vec::new();
    collect_fuzzy_mention_matches(&cwd, &cwd, query, &mut scored, &mut 0);
    scored.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.len().cmp(&right.1.len()))
            .then_with(|| left.1.cmp(&right.1))
    });
    scored
        .into_iter()
        .take(MAX_FUZZY_MENTION_CANDIDATES)
        .map(|(_, candidate)| candidate)
        .collect()
}

fn collect_fuzzy_mention_matches(
    root: &Path,
    dir: &Path,
    query: &str,
    scored: &mut Vec<(usize, String)>,
    visited: &mut usize,
) {
    if *visited >= MAX_FUZZY_MENTION_VISITS {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if *visited >= MAX_FUZZY_MENTION_VISITS {
            return;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        *visited += 1;
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let candidate = relative.to_string_lossy().replace('\\', "/");
        let candidate = if path.is_dir() {
            format!("{candidate}/")
        } else {
            candidate
        };
        if let Some(score) = fuzzy_score(&candidate, query) {
            scored.push((score, candidate));
        }
        if path.is_dir() {
            collect_fuzzy_mention_matches(root, &path, query, scored, visited);
        }
    }
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<usize> {
    let candidate_lower = candidate.to_lowercase();
    let query_lower = query.to_lowercase();
    if candidate_lower.contains(&query_lower) {
        return candidate_lower.find(&query_lower);
    }
    subsequence_score(&candidate_lower, &query_lower)
}

fn subsequence_score(candidate: &str, query: &str) -> Option<usize> {
    let mut score = 0;
    let mut last_match = 0;
    let mut chars = candidate.char_indices();
    for query_char in query.chars() {
        let mut matched = None;
        for (index, candidate_char) in chars.by_ref() {
            if candidate_char == query_char {
                matched = Some(index);
                break;
            }
        }
        let index = matched?;
        score += index.saturating_sub(last_match);
        last_match = index + query_char.len_utf8();
    }
    Some(score)
}

fn resolve_mention_dir(cwd: &Path, mention: &str) -> Result<PathBuf, String> {
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
    if !path.is_dir() {
        return Err(format!("@{mention} is not a directory"));
    }
    Ok(path)
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
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "hello").unwrap();

        let completed = complete_file_mention("read @no", dir.path()).unwrap();

        assert_eq!(completed, "read @notes.txt");
    }

    #[test]
    fn completes_common_prefix_for_multiple_mentions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha-one.txt"), "hello").unwrap();
        fs::write(dir.path().join("alpha-two.txt"), "hello").unwrap();

        let completed = complete_file_mention("read @a", dir.path()).unwrap();

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
    fn hidden_files_excluded_from_completion() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".hidden"), "secret").unwrap();
        fs::write(dir.path().join("visible.txt"), "hello").unwrap();

        let candidates = list_mention_candidates("@", dir.path());

        assert!(!candidates.iter().any(|c| c.contains(".hidden")));
        assert!(candidates.iter().any(|c| c.contains("visible")));
    }

    #[test]
    fn completes_quoted_mention() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("my dir");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("notes.txt"), "hello").unwrap();

        let completed = complete_file_mention(r#"read @"my dir/no"#, dir.path()).unwrap();

        assert!(completed.contains("my dir/notes.txt"));
    }

    #[test]
    fn fuzzy_mention_candidates_match_path_initials() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src/runtime/config");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("mod.rs"), "hello").unwrap();

        let candidates = list_mention_candidates("@rcm", dir.path());

        assert!(
            candidates
                .iter()
                .any(|candidate| { candidate == "src/runtime/config/mod.rs" })
        );
    }
}
