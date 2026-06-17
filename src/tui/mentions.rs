use std::collections::BTreeSet;
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
        let path = resolve_mention_path(cwd, &mention)?;
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("failed to read @{}: {error}", mention))?;
        let (content, truncated) = truncate_content(&content);
        let marker = if truncated {
            "\n[... truncated ...]"
        } else {
            ""
        };
        blocks.push(format!(
            r#"<file path="{}">
{}{}</file>"#,
            escape_attr(&mention),
            content,
            marker
        ));
    }

    Ok(format!("{}\n\n{}", input, blocks.join("\n\n")))
}

fn find_mentions(input: &str) -> Vec<String> {
    let mut mentions = BTreeSet::new();
    for token in input.split_whitespace() {
        let Some(raw) = token.strip_prefix('@') else {
            continue;
        };
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
        mentions.insert(mention.to_string());
    }
    mentions.into_iter().collect()
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
    fn ignores_urls_and_plain_at_words_without_files() {
        let dir = tempfile::tempdir().unwrap();
        let expanded = expand_file_mentions("see @https://example.com", dir.path()).unwrap();
        assert_eq!(expanded, "see @https://example.com");
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
}
