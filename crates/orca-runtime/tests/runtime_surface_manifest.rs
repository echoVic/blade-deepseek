use std::collections::BTreeSet;

const MANIFEST: &str = include_str!(
    "../../../docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json"
);
const EVENT_SCHEMA: &str = include_str!("../../orca-core/src/event_schema.rs");

fn rust_char_literal_len(source: &str) -> Option<usize> {
    let mut chars = source.char_indices();
    if chars.next()?.1 != '\'' {
        return None;
    }
    let (_, character) = chars.next()?;
    if character == '\\' {
        let (_, escape) = chars.next()?;
        match escape {
            '0' | 'n' | 'r' | 't' | '\\' | '\'' | '"' => {}
            'x' => {
                for _ in 0..2 {
                    if !chars.next()?.1.is_ascii_hexdigit() {
                        return None;
                    }
                }
            }
            'u' => {
                if chars.next()?.1 != '{' {
                    return None;
                }
                let mut digits = 0;
                loop {
                    match chars.next()?.1 {
                        '}' if (1..=6).contains(&digits) => break,
                        '_' => {}
                        digit if digit.is_ascii_hexdigit() => digits += 1,
                        _ => return None,
                    }
                }
            }
            _ => return None,
        }
    } else if character == '\'' || character.is_control() {
        return None;
    }
    let (closing_index, closing) = chars.next()?;
    (closing == '\'').then_some(closing_index + closing.len_utf8())
}

fn enum_variants(source: &str, declaration: &str) -> Vec<String> {
    let uncommented = strip_rust_comments(source);
    let body = uncommented
        .split(declaration)
        .nth(1)
        .unwrap_or_else(|| panic!("missing {declaration}"));
    let mut variants = Vec::new();
    let mut chunk = String::new();
    let mut braces = 0_i32;
    let mut parentheses = 0_i32;
    let mut brackets = 0_i32;
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0;
    while index < body.len() {
        let ch = body[index..].chars().next().expect("character at boundary");
        let ch_len = ch.len_utf8();
        if let Some(delimiter) = quote {
            chunk.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == delimiter {
                quote = None;
            }
            index += ch_len;
            continue;
        }
        if ch == '"' {
            quote = Some(ch);
            chunk.push(ch);
            index += ch_len;
            continue;
        }
        if ch == '\'' {
            if let Some(len) = rust_char_literal_len(&body[index..]) {
                chunk.push_str(&body[index..index + len]);
                index += len;
                continue;
            }
        }
        if ch == '}' && braces == 0 && parentheses == 0 && brackets == 0 {
            push_variant(&mut variants, &mut chunk);
            break;
        }
        match ch {
            '{' => braces += 1,
            '}' => braces -= 1,
            '(' => parentheses += 1,
            ')' => parentheses -= 1,
            '[' => brackets += 1,
            ']' => brackets -= 1,
            ',' if braces == 0 && parentheses == 0 && brackets == 0 => {
                push_variant(&mut variants, &mut chunk);
                index += ch_len;
                continue;
            }
            _ => {}
        }
        chunk.push(ch);
        index += ch_len;
    }
    variants
}

fn strip_rust_comments(source: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        Line,
        Block(usize),
        String,
    }

    let mut output = String::new();
    let mut state = State::Code;
    let mut escaped = false;
    let mut index = 0;
    while index < source.len() {
        let ch = source[index..]
            .chars()
            .next()
            .expect("character at boundary");
        let ch_len = ch.len_utf8();
        let next = source[index + ch_len..].chars().next();
        match state {
            State::Code if ch == '/' && next == Some('/') => {
                index += 2;
                state = State::Line;
                continue;
            }
            State::Code if ch == '/' && next == Some('*') => {
                index += 2;
                state = State::Block(1);
                continue;
            }
            State::Code => {
                if ch == '\'' {
                    if let Some(len) = rust_char_literal_len(&source[index..]) {
                        output.push_str(&source[index..index + len]);
                        index += len;
                        continue;
                    }
                }
                output.push(ch);
                if ch == '"' {
                    escaped = false;
                    state = State::String;
                }
            }
            State::Line if ch == '\n' => {
                output.push(ch);
                state = State::Code;
            }
            State::Line => {}
            State::Block(depth) if ch == '/' && next == Some('*') => {
                index += 2;
                state = State::Block(depth + 1);
                continue;
            }
            State::Block(depth) if ch == '*' && next == Some('/') => {
                index += 2;
                state = if depth == 1 {
                    State::Code
                } else {
                    State::Block(depth - 1)
                };
                continue;
            }
            State::Block(depth) => {
                if ch == '\n' {
                    output.push(ch);
                }
                state = State::Block(depth);
            }
            State::String => {
                output.push(ch);
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    state = State::Code;
                }
            }
        }
        index += ch_len;
    }
    output
}

fn push_variant(variants: &mut Vec<String>, chunk: &mut String) {
    let mut remaining = chunk.trim();
    while remaining.starts_with("#[") {
        let mut depth = 0_i32;
        let mut end = None;
        let mut quote = None;
        let mut escaped = false;
        let mut index = 1;
        while index < remaining.len() {
            let ch = remaining[index..]
                .chars()
                .next()
                .expect("character at boundary");
            let ch_len = ch.len_utf8();
            if let Some(delimiter) = quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == delimiter {
                    quote = None;
                }
                index += ch_len;
                continue;
            }
            match ch {
                '"' => quote = Some(ch),
                '\'' => {
                    if let Some(len) = rust_char_literal_len(&remaining[index..]) {
                        index += len;
                        continue;
                    }
                }
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
            index += ch_len;
        }
        let end = end.expect("terminated Rust enum attribute");
        remaining = remaining[end..].trim_start();
    }
    let name: String = remaining
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect();
    if !name.is_empty() {
        variants.push(name);
    }
    chunk.clear();
}

#[test]
fn enum_inventory_parser_handles_rust_syntax_and_final_variant_without_comma() {
    let source = r#"
pub enum Fixture {
    #[serde(rename = "tuple,renamed")]
    Tuple(String, Vec<u8>), // line comment, with comma
    /*
    FakeVariant,
    */
    /// doc comment, with comma
    Struct { value: Option<(u8, u8)> },
    #[cfg_attr(feature = "nested", serde(rename = "right]bracket"))]
    #[serde(rename = "right]bracket")]
    RightBracket,
    Final
}
"#;

    assert_eq!(
        enum_variants(source, "pub enum Fixture {"),
        ["Tuple", "Struct", "RightBracket", "Final"]
    );
    assert_enum_inventory_parser_distinguishes_lifetimes_from_character_literals();
}

fn assert_enum_inventory_parser_distinguishes_lifetimes_from_character_literals() {
    let source = r##"
pub enum LifetimeFixture<'a> {
    Existing,
    #[doc = "borrowed, static"]
    Hidden(&'static str),
    Named { value: &'a str },
    Character(char),
    Plain = 'x' as isize,
    Newline = '\n' as isize,
    Backslash = '\\' as isize,
    Quote = '\'' as isize,
    Unicode = '界' as isize,
}
"##;

    assert_eq!(
        enum_variants(source, "pub enum LifetimeFixture<'a> {"),
        [
            "Existing",
            "Hidden",
            "Named",
            "Character",
            "Plain",
            "Newline",
            "Backslash",
            "Quote",
            "Unicode",
        ]
    );
}

#[test]
fn manifest_source_facts_exactly_match_current_event_type() {
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("manifest JSON");
    let declared: Vec<String> = manifest["source_facts"]
        .as_array()
        .expect("source_facts")
        .iter()
        .map(|row| row[0].as_str().expect("source fact id").to_string())
        .collect();
    let current = enum_variants(EVENT_SCHEMA, "pub enum EventType {");

    assert_eq!(declared.len(), 53, "the reviewed baseline has 53 events");
    assert_eq!(
        declared.iter().collect::<BTreeSet<_>>().len(),
        declared.len(),
        "source fact ids must be unique"
    );
    assert_eq!(
        declared, current,
        "EventType drift requires contract review"
    );
}
