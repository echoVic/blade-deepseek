#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StreamingMarkdownAction {
    UpdateTail(String),
    FreezeTail { text: String, trailing_blank: bool },
    AppendFrozen { text: String, trailing_blank: bool },
    ClearTail,
    FinishTail(String),
}

#[derive(Default)]
pub(crate) struct StreamingMarkdownAssembler {
    partial_line: String,
    current_block: String,
    fence: Option<FenceState>,
    finished: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FenceState {
    marker: char,
    run_len: usize,
}

impl StreamingMarkdownAssembler {
    pub(crate) fn push(&mut self, text: &str) -> Vec<StreamingMarkdownAction> {
        if self.finished || text.is_empty() {
            return Vec::new();
        }
        self.partial_line.push_str(text);
        let Some(last_newline) = self.partial_line.rfind('\n') else {
            return Vec::new();
        };
        let complete = self.partial_line[..=last_newline].to_owned();
        self.partial_line.drain(..=last_newline);
        let mut actions = Vec::new();
        for line in complete.split_inclusive('\n') {
            self.current_block.push_str(line);
            if let Some(fence) = self.fence {
                if fence_closes(line, fence) {
                    self.fence = None;
                    self.freeze_current_block(&mut actions, false);
                }
            } else if let Some(fence) = fence_open(line) {
                self.fence = Some(fence);
            } else if line_is_blank(line) {
                self.freeze_current_block(&mut actions, true);
            }
        }
        if !self.current_block.is_empty() {
            actions.push(StreamingMarkdownAction::UpdateTail(
                self.current_block.clone(),
            ));
        }
        actions
    }

    pub(crate) fn finish(&mut self) -> Vec<StreamingMarkdownAction> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        if self.current_block.is_empty() && self.partial_line.is_empty() {
            Vec::new()
        } else {
            self.current_block.clear();
            vec![StreamingMarkdownAction::FinishTail(std::mem::take(
                &mut self.partial_line,
            ))]
        }
    }

    #[cfg(test)]
    fn held_text_for_test(&self) -> &str {
        &self.partial_line
    }

    fn freeze_current_block(
        &mut self,
        actions: &mut Vec<StreamingMarkdownAction>,
        trailing_blank: bool,
    ) {
        let text = std::mem::take(&mut self.current_block);
        actions.push(StreamingMarkdownAction::UpdateTail(text.clone()));
        actions.push(StreamingMarkdownAction::FreezeTail {
            text,
            trailing_blank,
        });
        actions.push(StreamingMarkdownAction::ClearTail);
    }
}

fn fence_open(line: &str) -> Option<FenceState> {
    let content = line.strip_suffix('\n').unwrap_or(line);
    let trimmed = content.trim_start_matches(' ');
    if content.len() - trimmed.len() > 3 {
        return None;
    }
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let run_len = trimmed
        .chars()
        .take_while(|character| *character == marker)
        .count();
    (run_len >= 3).then_some(FenceState { marker, run_len })
}

fn fence_closes(line: &str, fence: FenceState) -> bool {
    let content = line.strip_suffix('\n').unwrap_or(line);
    let trimmed = content.trim_start_matches(' ');
    if content.len() - trimmed.len() > 3 {
        return false;
    }
    let run_len = trimmed
        .chars()
        .take_while(|character| *character == fence.marker)
        .count();
    run_len >= fence.run_len && trimmed[run_len..].trim().is_empty()
}

fn line_is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::{StreamingMarkdownAction, StreamingMarkdownAssembler};

    fn reconstructed_action_text(actions: &[StreamingMarkdownAction]) -> String {
        let mut frozen = String::new();
        let mut tail = String::new();
        for action in actions {
            match action {
                StreamingMarkdownAction::UpdateTail(text) => tail.clone_from(text),
                StreamingMarkdownAction::FreezeTail { text, .. } => {
                    frozen.push_str(text);
                    tail.clear();
                }
                StreamingMarkdownAction::AppendFrozen { text, .. } => frozen.push_str(text),
                StreamingMarkdownAction::ClearTail => tail.clear(),
                StreamingMarkdownAction::FinishTail(text) => tail.push_str(text),
            }
        }
        frozen.push_str(&tail);
        frozen
    }

    #[test]
    fn partial_source_line_stays_hidden_until_newline_or_finish() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert!(assembler.push("hello").is_empty());
        assert_eq!(assembler.held_text_for_test(), "hello");
        assert_eq!(
            assembler.push(" world\n"),
            vec![StreamingMarkdownAction::UpdateTail(
                "hello world\n".to_string()
            )]
        );
        assert_eq!(assembler.held_text_for_test(), "");

        assert!(assembler.push("final").is_empty());
        assert_eq!(
            assembler.finish(),
            vec![StreamingMarkdownAction::FinishTail("final".to_string())]
        );
        assert!(assembler.finish().is_empty());
    }

    #[test]
    fn newline_gate_reconstructs_cjk_emoji_and_combining_text_exactly() {
        let mut assembler = StreamingMarkdownAssembler::default();
        let input = ["中", "文👍🏽e\u{301}", "\n尾", "行"];
        let mut actions = Vec::new();
        for piece in input {
            actions.extend(assembler.push(piece));
        }
        actions.extend(assembler.finish());
        assert_eq!(reconstructed_action_text(&actions), "中文👍🏽e\u{301}\n尾行");
    }

    #[test]
    fn blank_line_freezes_the_visible_tail_and_starts_a_fresh_block() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert_eq!(
            assembler.push("first paragraph\n\n"),
            vec![
                StreamingMarkdownAction::UpdateTail("first paragraph\n\n".to_string()),
                StreamingMarkdownAction::FreezeTail {
                    text: "first paragraph\n\n".to_string(),
                    trailing_blank: true,
                },
                StreamingMarkdownAction::ClearTail,
            ]
        );
        assert_eq!(
            assembler.push("second paragraph\n"),
            vec![StreamingMarkdownAction::UpdateTail(
                "second paragraph\n".to_string()
            )]
        );
    }

    #[test]
    fn consecutive_blank_lines_preserve_source_with_one_display_blank() {
        let mut assembler = StreamingMarkdownAssembler::default();
        let actions = assembler.push("paragraph\n\n\n");
        assert_eq!(
            actions,
            vec![
                StreamingMarkdownAction::UpdateTail("paragraph\n\n".to_string()),
                StreamingMarkdownAction::FreezeTail {
                    text: "paragraph\n\n".to_string(),
                    trailing_blank: true,
                },
                StreamingMarkdownAction::ClearTail,
                StreamingMarkdownAction::UpdateTail("\n".to_string()),
                StreamingMarkdownAction::FreezeTail {
                    text: "\n".to_string(),
                    trailing_blank: true,
                },
                StreamingMarkdownAction::ClearTail,
            ]
        );
        assert_eq!(reconstructed_action_text(&actions), "paragraph\n\n\n");
    }

    #[test]
    fn fenced_block_freezes_only_after_matching_close() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert_eq!(
            assembler.push("before\n\n```rust\nfn main() {\n"),
            vec![
                StreamingMarkdownAction::UpdateTail("before\n\n".to_string()),
                StreamingMarkdownAction::FreezeTail {
                    text: "before\n\n".to_string(),
                    trailing_blank: true,
                },
                StreamingMarkdownAction::ClearTail,
                StreamingMarkdownAction::UpdateTail("```rust\nfn main() {\n".to_string()),
            ]
        );
        assert_eq!(
            assembler.push("}\n```\n"),
            vec![
                StreamingMarkdownAction::UpdateTail("```rust\nfn main() {\n}\n```\n".to_string()),
                StreamingMarkdownAction::FreezeTail {
                    text: "```rust\nfn main() {\n}\n```\n".to_string(),
                    trailing_blank: false,
                },
                StreamingMarkdownAction::ClearTail,
            ]
        );
    }

    #[test]
    fn fences_require_matching_marker_and_sufficient_closing_run() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert_eq!(
            assembler.push("   ~~~~text\n~~~\n````\n~~~~\n"),
            vec![
                StreamingMarkdownAction::UpdateTail("   ~~~~text\n~~~\n````\n~~~~\n".to_string()),
                StreamingMarkdownAction::FreezeTail {
                    text: "   ~~~~text\n~~~\n````\n~~~~\n".to_string(),
                    trailing_blank: false,
                },
                StreamingMarkdownAction::ClearTail,
            ]
        );
    }

    #[test]
    fn unfinished_fence_is_emitted_by_finish() {
        let mut assembler = StreamingMarkdownAssembler::default();
        let mut actions = assembler.push("```rust\nlet value = 1;\n");
        actions.extend(assembler.push("unfinished"));
        actions.extend(assembler.finish());
        assert_eq!(
            reconstructed_action_text(&actions),
            "```rust\nlet value = 1;\nunfinished"
        );
    }

    #[test]
    fn action_stream_and_held_suffix_reconstruct_every_input_prefix() {
        let fixtures = [
            vec!["# Head", "ing\n\n- one\n", "- two"],
            vec!["前文\n\n", "~~~text\n中", "文\n~~~\n", "尾"],
            vec!["e\u{301}", "moji 👍🏽\n", "\n````\n", "code\n```", "`\n"],
        ];

        for pieces in fixtures {
            let mut assembler = StreamingMarkdownAssembler::default();
            let mut actions = Vec::new();
            let mut prefix = String::new();
            for piece in pieces {
                prefix.push_str(piece);
                actions.extend(assembler.push(piece));
                let mut reconstructed = reconstructed_action_text(&actions);
                reconstructed.push_str(assembler.held_text_for_test());
                assert_eq!(reconstructed, prefix);
            }
            actions.extend(assembler.finish());
            assert_eq!(reconstructed_action_text(&actions), prefix);
        }
    }
}
