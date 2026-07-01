use super::wire::{WireInputParam, WireParams, WireUserInput};

pub(super) fn prompt_from_turn_start_params(params: Option<WireParams>) -> String {
    params
        .map(|params| {
            let Some(WireInputParam::Items(input)) = params.input else {
                return String::new();
            };
            input
                .into_iter()
                .filter_map(|input| match input {
                    WireUserInput::Text { text } => Some(text),
                    WireUserInput::Image {}
                    | WireUserInput::LocalImage {}
                    | WireUserInput::Skill {}
                    | WireUserInput::Mention {} => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}
