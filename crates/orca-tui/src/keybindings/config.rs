use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::shortcuts::{
    ShortcutAction, ShortcutContext, configurable_legacy_bindings, normalize_key_parts,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct KeyStroke {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyStroke {
    pub(crate) fn parse(source: &str) -> Result<Self, String> {
        if source.is_empty() || source.chars().any(char::is_whitespace) {
            return Err(format!("invalid key stroke `{source}`"));
        }
        let parts = source.split('+').collect::<Vec<_>>();
        let (key_name, modifier_names) = parts
            .split_last()
            .ok_or_else(|| format!("invalid key stroke `{source}`"))?;
        if key_name.is_empty() {
            return Err(format!("invalid key stroke `{source}`"));
        }

        let mut modifiers = KeyModifiers::NONE;
        for modifier in modifier_names {
            let flag = match modifier.to_ascii_lowercase().as_str() {
                "ctrl" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                "super" => KeyModifiers::SUPER,
                "hyper" => KeyModifiers::HYPER,
                "meta" => KeyModifiers::META,
                _ => return Err(format!("unknown modifier `{modifier}`")),
            };
            if modifiers.contains(flag) {
                return Err(format!("duplicate modifier `{modifier}`"));
            }
            modifiers.insert(flag);
        }

        let code = parse_key_code(key_name)?;
        Ok(Self { code, modifiers })
    }

    pub(crate) fn matches(self, event: KeyEvent) -> bool {
        if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return false;
        }
        normalize_key_parts(self.code, self.modifiers)
            == normalize_key_parts(event.code, event.modifiers)
    }
}

impl fmt::Display for KeyStroke {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (modifier, label) in [
            (KeyModifiers::CONTROL, "ctrl+"),
            (KeyModifiers::ALT, "alt+"),
            (KeyModifiers::SHIFT, "shift+"),
            (KeyModifiers::SUPER, "super+"),
            (KeyModifiers::HYPER, "hyper+"),
            (KeyModifiers::META, "meta+"),
        ] {
            if self.modifiers.contains(modifier) {
                formatter.write_str(label)?;
            }
        }
        formatter.write_str(&format_key_code(self.code))
    }
}

fn parse_key_code(source: &str) -> Result<KeyCode, String> {
    let normalized = source.to_ascii_lowercase();
    let named = match normalized.as_str() {
        "backspace" => Some(KeyCode::Backspace),
        "enter" => Some(KeyCode::Enter),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "up" => Some(KeyCode::Up),
        "down" => Some(KeyCode::Down),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        "pageup" => Some(KeyCode::PageUp),
        "pagedown" => Some(KeyCode::PageDown),
        "tab" => Some(KeyCode::Tab),
        "backtab" => Some(KeyCode::BackTab),
        "delete" => Some(KeyCode::Delete),
        "insert" => Some(KeyCode::Insert),
        "esc" => Some(KeyCode::Esc),
        "space" => Some(KeyCode::Char(' ')),
        _ => None,
    };
    if let Some(code) = named {
        return Ok(code);
    }
    if let Some(number) = normalized
        .strip_prefix('f')
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|number| (1..=24).contains(number))
    {
        return Ok(KeyCode::F(number));
    }
    let mut characters = source.chars();
    match (characters.next(), characters.next()) {
        (Some(character), None) if !character.is_whitespace() && !character.is_control() => {
            Ok(KeyCode::Char(character.to_ascii_lowercase()))
        }
        _ => Err(format!("unknown key `{source}`")),
    }
}

fn format_key_code(code: KeyCode) -> String {
    match code {
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "backtab".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::F(number) => format!("f{number}"),
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(character) => character.to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Null
        | KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::KeypadBegin
        | KeyCode::Media(_)
        | KeyCode::Modifier(_) => {
            unreachable!("unsupported key code in configurable stroke")
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Keymap {
    bindings: HashMap<ShortcutContext, Vec<(KeyStroke, ShortcutAction)>>,
}

impl Keymap {
    pub(crate) fn built_in() -> Arc<Self> {
        let mut bindings: HashMap<ShortcutContext, Vec<(KeyStroke, ShortcutAction)>> =
            HashMap::new();
        for binding in configurable_legacy_bindings() {
            bindings.entry(binding.context).or_default().push((
                KeyStroke {
                    code: binding.key,
                    modifiers: binding.modifiers,
                },
                binding.action,
            ));
        }
        Arc::new(Self { bindings })
    }

    pub(crate) fn resolve_single(
        &self,
        context: ShortcutContext,
        event: KeyEvent,
    ) -> Option<ShortcutAction> {
        self.resolve_in(ShortcutContext::Global, event)
            .or_else(|| self.resolve_in(context, event))
    }

    fn resolve_in(&self, context: ShortcutContext, event: KeyEvent) -> Option<ShortcutAction> {
        self.bindings
            .get(&context)?
            .iter()
            .find(|(stroke, _)| stroke.matches(event))
            .map(|(_, action)| *action)
    }

    pub(crate) fn binding_count(&self) -> usize {
        self.bindings.values().map(Vec::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{KeyStroke, Keymap};

    #[test]
    fn built_in_keymap_matches_all_configurable_legacy_bindings() {
        let keymap = Keymap::built_in();
        for binding in crate::shortcuts::configurable_legacy_bindings() {
            assert_eq!(
                keymap.resolve_single(binding.context, binding.as_key_event()),
                Some(binding.action),
                "{:?} {:?} must keep {binding:?}",
                binding.context,
                binding.action,
            );
        }
        assert_eq!(
            keymap.binding_count(),
            crate::shortcuts::configurable_legacy_bindings().count(),
        );
    }

    #[test]
    fn key_strokes_parse_and_format_canonically() {
        for (source, canonical) in [
            ("CTRL+X", "ctrl+x"),
            ("alt+SHIFT+enter", "alt+shift+enter"),
            ("BackTab", "backtab"),
            ("space", "space"),
            ("f24", "f24"),
            ("ctrl+界", "ctrl+界"),
        ] {
            assert_eq!(KeyStroke::parse(source).unwrap().to_string(), canonical);
        }
    }

    #[test]
    fn invalid_key_strokes_are_rejected() {
        for source in [
            "",
            "ctrl",
            "ctrl+",
            "ctrl+ctrl+x",
            "wat+x",
            "a+b",
            "f25",
            "\n",
        ] {
            assert!(KeyStroke::parse(source).is_err(), "{source:?}");
        }
    }

    #[test]
    fn normalization_keeps_c0_and_shifted_character_compatibility() {
        let ctrl_j = KeyStroke::parse("ctrl+j").unwrap();
        assert!(ctrl_j.matches(KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE,)));
        let shift_a = KeyStroke::parse("shift+a").unwrap();
        assert!(shift_a.matches(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE,)));
    }
}
