use std::io::{self, BufRead, Write};

pub fn prompt_user(tool_name: &str, target: Option<&str>) -> io::Result<bool> {
    let description = match target {
        Some(t) => format!("{tool_name}: {t}"),
        None => tool_name.to_string(),
    };
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stderr = io::stderr();
    let mut writer = stderr.lock();
    prompt_user_with_io(&description, &mut reader, &mut writer)
}

pub fn prompt_user_with_io(
    description: &str,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<bool> {
    write!(output, "  {description}\n  Allow? [y/n]: ")?;
    output.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    let answer = line.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approves_with_y() {
        let mut input = io::Cursor::new(b"y\n");
        let mut output = Vec::new();
        let result = prompt_user_with_io("edit file.rs", &mut input, &mut output).unwrap();
        assert!(result);
    }

    #[test]
    fn approves_with_yes() {
        let mut input = io::Cursor::new(b"yes\n");
        let mut output = Vec::new();
        let result = prompt_user_with_io("bash echo hi", &mut input, &mut output).unwrap();
        assert!(result);
    }

    #[test]
    fn approves_case_insensitive() {
        let mut input = io::Cursor::new(b"Y\n");
        let mut output = Vec::new();
        let result = prompt_user_with_io("edit file.rs", &mut input, &mut output).unwrap();
        assert!(result);
    }

    #[test]
    fn denies_with_n() {
        let mut input = io::Cursor::new(b"n\n");
        let mut output = Vec::new();
        let result = prompt_user_with_io("bash rm -rf /", &mut input, &mut output).unwrap();
        assert!(!result);
    }

    #[test]
    fn denies_with_empty() {
        let mut input = io::Cursor::new(b"\n");
        let mut output = Vec::new();
        let result = prompt_user_with_io("edit file.rs", &mut input, &mut output).unwrap();
        assert!(!result);
    }

    #[test]
    fn denies_with_arbitrary_text() {
        let mut input = io::Cursor::new(b"maybe\n");
        let mut output = Vec::new();
        let result = prompt_user_with_io("bash echo", &mut input, &mut output).unwrap();
        assert!(!result);
    }

    #[test]
    fn output_contains_description() {
        let mut input = io::Cursor::new(b"n\n");
        let mut output = Vec::new();
        prompt_user_with_io("edit src/main.rs", &mut input, &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("edit src/main.rs"));
        assert!(text.contains("Allow?"));
    }
}
