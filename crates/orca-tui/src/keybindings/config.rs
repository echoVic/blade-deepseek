use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::shortcuts::{
    GlobalShortcut, ShortcutAction, ShortcutContext, ShortcutDescriptor, action_for_id,
    configurable_legacy_bindings, normalize_key_parts,
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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct KeySequence(Vec<KeyStroke>);

impl KeySequence {
    fn parse(source: &str) -> Result<Self, KeymapError> {
        let strokes = source
            .split_whitespace()
            .map(KeyStroke::parse)
            .collect::<Result<Vec<_>, _>>()
            .map_err(KeymapError)?;
        if strokes.is_empty() {
            return Err(KeymapError("key sequence cannot be empty".to_string()));
        }
        if strokes.len() > 4 {
            return Err(KeymapError(
                "key sequence may contain at most four strokes".to_string(),
            ));
        }
        Ok(Self(strokes))
    }

    fn first(&self) -> KeyStroke {
        self.0[0]
    }

    fn is_prefix_of(&self, other: &Self) -> bool {
        self.0.len() < other.0.len() && other.0.starts_with(&self.0)
    }
}

impl fmt::Display for KeySequence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self
            .0
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        formatter.write_str(&text)
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
    bindings: HashMap<ShortcutContext, Vec<(KeySequence, ShortcutAction)>>,
}

impl Keymap {
    pub(crate) fn built_in() -> Arc<Self> {
        let mut bindings: HashMap<ShortcutContext, Vec<(KeySequence, ShortcutAction)>> =
            HashMap::new();
        for binding in configurable_legacy_bindings() {
            bindings.entry(binding.context).or_default().push((
                KeySequence(vec![KeyStroke {
                    code: binding.key,
                    modifiers: binding.modifiers,
                }]),
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
            .find(|(sequence, _)| sequence.0.len() == 1 && sequence.first().matches(event))
            .map(|(_, action)| *action)
    }

    pub(crate) fn binding_count(&self) -> usize {
        self.bindings.values().map(Vec::len).sum()
    }

    pub(crate) fn has_action(&self, action: ShortcutAction) -> bool {
        self.bindings
            .values()
            .flatten()
            .any(|(_, candidate)| *candidate == action)
    }

    pub(crate) fn descriptor_keys(&self, descriptor: &ShortcutDescriptor) -> Option<String> {
        if descriptor.actions.is_empty() {
            return Some(descriptor.legacy_keys.to_string());
        }
        if descriptor.actions.iter().all(|action| {
            self.sequences_for_action(*action) == Self::built_in().sequences_for_action(*action)
        }) {
            return Some(descriptor.legacy_keys.to_string());
        }
        let keys = descriptor
            .actions
            .iter()
            .flat_map(|action| self.sequences_for_action(*action))
            .map(|sequence| sequence.to_string())
            .collect::<Vec<_>>();
        (!keys.is_empty()).then(|| keys.join(" / "))
    }

    fn sequences_for_action(&self, action: ShortcutAction) -> Vec<KeySequence> {
        sequences_for(&self.bindings, action)
    }
}

#[derive(Debug)]
pub(crate) struct KeymapError(String);

impl fmt::Display for KeymapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for KeymapError {}

impl From<serde_json::Error> for KeymapError {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeybindingsFile {
    version: u32,
    bindings: StrictBindings,
}

struct StrictBindings(Vec<(String, Vec<String>)>);

impl<'de> Deserialize<'de> for StrictBindings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StrictBindingsVisitor;

        impl<'de> Visitor<'de> for StrictBindingsVisitor {
            type Value = StrictBindings;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an action-to-key-sequences object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::new();
                let mut seen = HashSet::new();
                while let Some((action, sequences)) = map.next_entry::<String, Vec<String>>()? {
                    if !seen.insert(action.clone()) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate action `{action}`"
                        )));
                    }
                    entries.push((action, sequences));
                }
                Ok(StrictBindings(entries))
            }
        }

        deserializer.deserialize_map(StrictBindingsVisitor)
    }
}

pub(crate) fn parse_keymap(bytes: &[u8]) -> Result<Arc<Keymap>, KeymapError> {
    let file: KeybindingsFile = serde_json::from_slice(bytes)?;
    if file.version != 1 {
        return Err(KeymapError(format!(
            "unsupported keybindings version {}",
            file.version
        )));
    }

    let built_in = Keymap::built_in();
    let mut bindings = built_in.bindings.clone();
    let mut replaced = HashSet::new();
    for (id, sources) in file.bindings.0 {
        let action =
            action_for_id(&id).ok_or_else(|| KeymapError(format!("unknown action `{id}`")))?;
        replaced.insert(action);
        if let Some(context_bindings) = bindings.get_mut(&action.context()) {
            context_bindings.retain(|(_, candidate)| *candidate != action);
        }
        for source in sources {
            let sequence = KeySequence::parse(&source).map_err(|error| {
                KeymapError(format!("invalid binding `{source}` for `{id}`: {error}"))
            })?;
            bindings
                .entry(action.context())
                .or_default()
                .push((sequence, action));
        }
    }

    validate_keymap(&bindings, &replaced)?;
    Ok(Arc::new(Keymap { bindings }))
}

fn validate_keymap(
    bindings: &HashMap<ShortcutContext, Vec<(KeySequence, ShortcutAction)>>,
    replaced: &HashSet<ShortcutAction>,
) -> Result<(), KeymapError> {
    let cancel = ShortcutAction::Global(GlobalShortcut::Cancel);
    let cancel_sequences = sequences_for(bindings, cancel);
    if cancel_sequences.is_empty()
        || cancel_sequences
            .iter()
            .any(|sequence| sequence.0.len() != 1)
    {
        return Err(KeymapError(
            "global.cancel must retain at least one single-stroke binding".to_string(),
        ));
    }

    for (context, rows) in bindings {
        for (sequence, action) in rows {
            if (*context == ShortcutContext::Approval || *context == ShortcutContext::Global)
                && sequence
                    .0
                    .iter()
                    .any(|stroke| is_reserved_approval(*stroke))
            {
                return Err(KeymapError(format!(
                    "binding `{sequence}` uses a reserved Approval stroke"
                )));
            }
            if replaced.contains(action) && *context == ShortcutContext::Global {
                for stroke in &sequence.0 {
                    if !is_configurable_global_stroke(*stroke) {
                        return Err(KeymapError(format!(
                            "configurable Global binding `{sequence}` must use function keys or modified characters"
                        )));
                    }
                }
            }
            if *context != ShortcutContext::Approval {
                for stroke in sequence.0.iter().take(sequence.0.len().saturating_sub(1)) {
                    if is_textual(*stroke) {
                        return Err(KeymapError(format!(
                            "non-final stroke `{stroke}` in `{sequence}` must be non-textual"
                        )));
                    }
                }
            }
            if *action != cancel
                && sequence.0.iter().any(|stroke| {
                    cancel_sequences
                        .iter()
                        .any(|cancel| cancel.first() == *stroke)
                })
            {
                return Err(KeymapError(format!(
                    "binding `{sequence}` uses a reserved cancel stroke"
                )));
            }
        }
    }

    for context in [
        ShortcutContext::Global,
        ShortcutContext::Idle,
        ShortcutContext::Running,
        ShortcutContext::Approval,
    ] {
        let mut effective = bindings
            .get(&ShortcutContext::Global)
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        if context != ShortcutContext::Global {
            effective.extend(bindings.get(&context).into_iter().flatten().cloned());
        }
        effective.sort_by_key(|(sequence, action)| {
            (
                sequence.to_string(),
                action.configurable_id().unwrap_or("").to_string(),
            )
        });
        for left in 0..effective.len() {
            for right in (left + 1)..effective.len() {
                let (left_sequence, left_action) = &effective[left];
                let (right_sequence, right_action) = &effective[right];
                if left_sequence == right_sequence {
                    return Err(KeymapError(format!(
                        "binding conflict in {context:?}: `{left_sequence}` maps to `{}` and `{}`",
                        left_action.configurable_id().unwrap_or("fixed"),
                        right_action.configurable_id().unwrap_or("fixed"),
                    )));
                }
                if left_sequence.is_prefix_of(right_sequence)
                    || right_sequence.is_prefix_of(left_sequence)
                {
                    return Err(KeymapError(format!(
                        "binding prefix conflict in {context:?}: `{left_sequence}` and `{right_sequence}`"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn sequences_for(
    bindings: &HashMap<ShortcutContext, Vec<(KeySequence, ShortcutAction)>>,
    action: ShortcutAction,
) -> Vec<KeySequence> {
    bindings
        .get(&action.context())
        .into_iter()
        .flatten()
        .filter(|(_, candidate)| *candidate == action)
        .map(|(sequence, _)| sequence.clone())
        .collect()
}

fn is_textual(stroke: KeyStroke) -> bool {
    matches!(stroke.code, KeyCode::Char(_))
        && !stroke.modifiers.intersects(
            KeyModifiers::CONTROL
                | KeyModifiers::ALT
                | KeyModifiers::SUPER
                | KeyModifiers::HYPER
                | KeyModifiers::META,
        )
}

fn is_configurable_global_stroke(stroke: KeyStroke) -> bool {
    matches!(stroke.code, KeyCode::F(1..=24))
        || matches!(stroke.code, KeyCode::Char(_))
            && stroke.modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            )
}

fn is_reserved_approval(stroke: KeyStroke) -> bool {
    ["1", "2", "3", "4", "y", "a", "shift+a", "n", "d"]
        .into_iter()
        .filter_map(|source| KeyStroke::parse(source).ok())
        .any(|reserved| reserved == stroke)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::shortcuts::{GlobalShortcut, IdleShortcut, ShortcutAction, ShortcutContext};

    use super::{KeyStroke, Keymap, parse_keymap};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn assert_error_contains(source: &[u8], expected: &str) {
        let error = parse_keymap(source).unwrap_err().to_string();
        assert!(
            error.contains(expected),
            "expected {error:?} to contain {expected:?}",
        );
    }

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

    #[test]
    fn omitted_actions_inherit_and_present_actions_replace_defaults() {
        let keymap = parse_keymap(
            br#"{
                "version": 1,
                "bindings": {
                    "idle.submit": ["ctrl+s"],
                    "idle.backtrack": []
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            keymap.resolve_single(
                ShortcutContext::Idle,
                key(KeyCode::Char('s'), KeyModifiers::CONTROL),
            ),
            Some(ShortcutAction::Idle(IdleShortcut::Submit)),
        );
        assert_eq!(
            keymap.resolve_single(
                ShortcutContext::Idle,
                key(KeyCode::Enter, KeyModifiers::NONE),
            ),
            None,
        );
        assert!(!keymap.has_action(ShortcutAction::Idle(IdleShortcut::Backtrack)));
        assert!(keymap.has_action(ShortcutAction::Global(GlobalShortcut::Cancel)));
    }

    #[test]
    fn rejects_unknown_schema_versions_fields_actions_and_duplicate_actions() {
        assert_error_contains(
            br#"{"version":2,"bindings":{}}"#,
            "unsupported keybindings version 2",
        );
        assert_error_contains(
            br#"{"version":1,"extra":true,"bindings":{}}"#,
            "unknown field `extra`",
        );
        assert_error_contains(
            br#"{"version":1,"bindings":{"idle.missing":["x"]}}"#,
            "unknown action `idle.missing`",
        );
        assert_error_contains(
            br#"{"version":1,"bindings":{"idle.submit":["enter"],"idle.submit":["ctrl+s"]}}"#,
            "duplicate action `idle.submit`",
        );
    }

    #[test]
    fn validates_conflicts_prefixes_and_reserved_strokes() {
        for (source, expected) in [
            (
                br#"{"version":1,"bindings":{"idle.submit":["ctrl+x"],"idle.newline":["ctrl+x"]}}"#
                    .as_slice(),
                "conflict",
            ),
            (
                br#"{"version":1,"bindings":{"global.clear-screen":["ctrl+x"],"idle.submit":["ctrl+x"]}}"#,
                "conflict",
            ),
            (
                br#"{"version":1,"bindings":{"idle.submit":["ctrl+x"],"idle.newline":["ctrl+x ctrl+j"]}}"#,
                "prefix",
            ),
            (
                br#"{"version":1,"bindings":{"global.cancel":[]}}"#,
                "single-stroke",
            ),
            (
                br#"{"version":1,"bindings":{"global.cancel":["ctrl+x ctrl+c"]}}"#,
                "single-stroke",
            ),
            (
                br#"{"version":1,"bindings":{"idle.submit":["ctrl+x ctrl+c"]}}"#,
                "reserved cancel",
            ),
            (
                br#"{"version":1,"bindings":{"global.clear-screen":["esc"]}}"#,
                "configurable Global",
            ),
            (
                br#"{"version":1,"bindings":{"global.clear-screen":["shift+x"]}}"#,
                "configurable Global",
            ),
            (
                br#"{"version":1,"bindings":{"approval.confirm":["1"]}}"#,
                "reserved Approval",
            ),
            (
                br#"{"version":1,"bindings":{"global.clear-screen":["ctrl+x a"]}}"#,
                "reserved Approval",
            ),
            (
                br#"{"version":1,"bindings":{"idle.submit":["g g"]}}"#,
                "non-textual",
            ),
            (
                br#"{"version":1,"bindings":{"idle.submit":["ctrl+x ctrl+a ctrl+b ctrl+d ctrl+e"]}}"#,
                "at most four",
            ),
        ] {
            assert_error_contains(source, expected);
        }
    }

    #[test]
    fn permits_cross_context_reuse_approval_text_chords_and_four_strokes() {
        let keymap = parse_keymap(
            br#"{
                "version": 1,
                "bindings": {
                    "idle.submit": ["ctrl+x ctrl+s"],
                    "running.submit-queued": ["ctrl+x ctrl+s"],
                    "approval.confirm": ["g g"],
                    "idle.newline": ["ctrl+x ctrl+a ctrl+b ctrl+n"]
                }
            }"#,
        )
        .unwrap();

        assert!(keymap.has_action(ShortcutAction::Idle(IdleShortcut::Submit)));
    }

    #[test]
    fn built_in_descriptor_keys_match_every_legacy_help_string() {
        let keymap = Keymap::built_in();

        for (descriptor, hint) in
            crate::shortcuts::shortcut_descriptors().zip(crate::shortcuts::SHORTCUT_HINTS)
        {
            assert_eq!(descriptor.scope, hint.scope);
            assert_eq!(
                keymap.descriptor_keys(descriptor).as_deref(),
                Some(descriptor.legacy_keys),
                "{}",
                descriptor.label,
            );
        }
    }

    #[test]
    fn descriptor_keys_change_only_with_their_referenced_actions() {
        let keymap = parse_keymap(
            br#"{
                "version": 1,
                "bindings": {
                    "idle.submit": ["ctrl+s"],
                    "idle.backtrack": []
                }
            }"#,
        )
        .unwrap();

        for descriptor in crate::shortcuts::shortcut_descriptors() {
            let keys = keymap.descriptor_keys(descriptor);
            match descriptor.label {
                "send message" => assert_eq!(keys.as_deref(), Some("ctrl+s")),
                "backtrack previous prompt" => assert_eq!(keys, None),
                _ => assert_eq!(keys.as_deref(), Some(descriptor.legacy_keys)),
            }
        }
    }

    #[test]
    fn descriptor_keys_format_replacements_and_chords_canonically() {
        let keymap = parse_keymap(
            br#"{
                "version": 1,
                "bindings": {
                    "idle.history-previous": ["ctrl+x ctrl+p", "alt+p"],
                    "idle.history-next": ["ctrl+x ctrl+n"]
                }
            }"#,
        )
        .unwrap();
        let descriptor = crate::shortcuts::shortcut_descriptors()
            .find(|descriptor| descriptor.label == "previous or next prompt")
            .unwrap();

        assert_eq!(
            keymap.descriptor_keys(descriptor).as_deref(),
            Some("ctrl+x ctrl+p / alt+p / ctrl+x ctrl+n"),
        );
    }
}
