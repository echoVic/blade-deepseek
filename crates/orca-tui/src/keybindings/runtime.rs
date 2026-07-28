use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyEvent, KeyEventKind};

use crate::shortcuts::{ShortcutAction, ShortcutContext};
use crate::types::PanelMode;
use crate::vim::VimMode;

use super::config::{KeyStroke, Keymap};
use super::reload::FileObservation;

const CHORD_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ModalOwner {
    None,
    TranscriptSearch,
    Shortcuts,
    SlashMenu,
    MentionMenu,
    WorkflowPanel,
    Setup,
    SessionPicker,
    Approval,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct InputOwnerFingerprint {
    pub(crate) context: ShortcutContext,
    pub(crate) modal: ModalOwner,
    pub(crate) panel: PanelMode,
    pub(crate) vim_mode: Option<VimMode>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InvocationOrigin {
    Key(KeyEvent),
    Chord,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShortcutInvocation {
    pub(crate) action: ShortcutAction,
    pub(crate) origin: InvocationOrigin,
}

impl ShortcutInvocation {
    pub(crate) const fn key(action: ShortcutAction, event: KeyEvent) -> Self {
        Self {
            action,
            origin: InvocationOrigin::Key(event),
        }
    }

    pub(crate) const fn chord(action: ShortcutAction) -> Self {
        Self {
            action,
            origin: InvocationOrigin::Chord,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShortcutResolution {
    NoMatch,
    RetryCurrentKey,
    Pending,
    Action(ShortcutInvocation),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReloadOutcome {
    Unchanged,
    Applied,
    RestoredDefaults,
    Rejected(String),
}

#[derive(Clone, Debug)]
struct PendingChord {
    owner: InputOwnerFingerprint,
    strokes: Vec<KeyStroke>,
    deadline: Instant,
}

pub(crate) struct KeymapRuntime {
    keymap: Arc<Keymap>,
    generation: u64,
    pending: Option<PendingChord>,
    last_observation: Option<FileObservation>,
    active_bytes: Option<Vec<u8>>,
}

impl KeymapRuntime {
    pub(crate) fn new(keymap: Arc<Keymap>) -> Self {
        Self {
            keymap,
            generation: 0,
            pending: None,
            last_observation: None,
            active_bytes: None,
        }
    }

    pub(crate) fn resolve(
        &mut self,
        owner: InputOwnerFingerprint,
        event: KeyEvent,
        now: Instant,
    ) -> ShortcutResolution {
        if matches!(event.kind, KeyEventKind::Release) {
            return ShortcutResolution::NoMatch;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.owner != owner)
        {
            self.pending = None;
        }
        if let Some(action) = self.keymap.cancel_action(event) {
            self.pending = None;
            return ShortcutResolution::Action(ShortcutInvocation::key(action, event));
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| now > pending.deadline)
        {
            self.pending = None;
        }
        let Some(stroke) = KeyStroke::from_event(event) else {
            return ShortcutResolution::NoMatch;
        };

        if let Some(pending) = self.pending.as_mut() {
            let mut prefix = pending.strokes.clone();
            prefix.push(stroke);
            let matches = self.keymap.matching_sequences(owner.context, &prefix);
            if matches.is_empty() {
                self.pending = None;
                return ShortcutResolution::RetryCurrentKey;
            }
            if let Some((_, action)) = matches
                .iter()
                .find(|(sequence, _)| sequence.len() == prefix.len())
            {
                let action = *action;
                self.pending = None;
                return ShortcutResolution::Action(ShortcutInvocation::chord(action));
            }
            pending.strokes = prefix;
            pending.deadline = now + CHORD_TIMEOUT;
            return ShortcutResolution::Pending;
        }

        let matches = self.keymap.matching_sequences(owner.context, &[stroke]);
        if let Some((_, action)) = matches.iter().find(|(sequence, _)| sequence.len() == 1) {
            return ShortcutResolution::Action(ShortcutInvocation::key(*action, event));
        }
        if matches.is_empty() {
            return ShortcutResolution::NoMatch;
        }
        self.pending = Some(PendingChord {
            owner,
            strokes: vec![stroke],
            deadline: now + CHORD_TIMEOUT,
        });
        ShortcutResolution::Pending
    }

    pub(crate) fn clear_for_non_key(&mut self) {
        self.pending = None;
    }

    pub(crate) fn clear_for_suspend(&mut self) {
        self.pending = None;
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|pending| pending.deadline)
    }

    pub(crate) fn has_pending_chord(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn install(&mut self, keymap: Arc<Keymap>) {
        self.keymap = keymap;
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn apply_observation(&mut self, observation: FileObservation) -> ReloadOutcome {
        if self.last_observation.as_ref() == Some(&observation) {
            return ReloadOutcome::Unchanged;
        }
        self.last_observation = Some(observation.clone());
        match observation {
            FileObservation::Missing => {
                if self.active_bytes.is_none() {
                    return ReloadOutcome::Unchanged;
                }
                self.active_bytes = None;
                self.install(Keymap::built_in());
                ReloadOutcome::RestoredDefaults
            }
            FileObservation::Rejected(error) => {
                ReloadOutcome::Rejected(format!("keybindings reload rejected: {error}"))
            }
            FileObservation::Bytes(bytes) => {
                if self.active_bytes.as_ref() == Some(&bytes) {
                    return ReloadOutcome::Unchanged;
                }
                match super::config::parse_keymap(&bytes) {
                    Ok(keymap) => {
                        self.active_bytes = Some(bytes);
                        self.install(keymap);
                        ReloadOutcome::Applied
                    }
                    Err(error) => ReloadOutcome::Rejected(format!(
                        "keybindings reload rejected: {}",
                        stable_parse_error(&error.to_string())
                    )),
                }
            }
        }
    }
}

fn stable_parse_error(error: &str) -> &str {
    error.split(" at line ").next().unwrap_or(error)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use crate::shortcuts::{GlobalShortcut, IdleShortcut, ShortcutAction, ShortcutContext};
    use crate::types::PanelMode;
    use crate::vim::VimMode;

    use super::{
        InputOwnerFingerprint, KeymapRuntime, ModalOwner, ShortcutInvocation, ShortcutResolution,
    };

    fn ctrl(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    fn idle_owner() -> InputOwnerFingerprint {
        InputOwnerFingerprint {
            context: ShortcutContext::Idle,
            modal: ModalOwner::None,
            panel: PanelMode::Conversation,
            vim_mode: None,
        }
    }

    fn runtime_with(bindings: &str) -> KeymapRuntime {
        let source = format!(r#"{{"version":1,"bindings":{bindings}}}"#);
        KeymapRuntime::new(crate::keybindings::config::parse_keymap(source.as_bytes()).unwrap())
    }

    #[test]
    fn exact_chord_emits_once_and_resets() {
        let mut runtime = runtime_with(r#"{"idle.submit":["ctrl+x ctrl+s"]}"#);
        let now = Instant::now();
        let owner = idle_owner();

        assert_eq!(
            runtime.resolve(owner, ctrl('x'), now),
            ShortcutResolution::Pending,
        );
        assert_eq!(
            runtime.resolve(owner, ctrl('s'), now + Duration::from_millis(10)),
            ShortcutResolution::Action(ShortcutInvocation::chord(ShortcutAction::Idle(
                IdleShortcut::Submit,
            ))),
        );
        assert!(!runtime.has_pending_chord());
    }

    #[test]
    fn two_three_and_four_stroke_chords_complete() {
        for (sequence, keys) in [
            ("ctrl+x ctrl+s", vec!['x', 's']),
            ("ctrl+x ctrl+a ctrl+s", vec!['x', 'a', 's']),
            ("ctrl+x ctrl+a ctrl+b ctrl+s", vec!['x', 'a', 'b', 's']),
        ] {
            let source = format!(r#"{{"idle.submit":["{sequence}"]}}"#);
            let mut runtime = runtime_with(&source);
            let now = Instant::now();
            for (index, key) in keys.iter().enumerate() {
                let resolution = runtime.resolve(
                    idle_owner(),
                    ctrl(*key),
                    now + Duration::from_millis(index as u64),
                );
                if index + 1 == keys.len() {
                    assert!(matches!(resolution, ShortcutResolution::Action(_)));
                } else {
                    assert_eq!(resolution, ShortcutResolution::Pending);
                }
            }
        }
    }

    #[test]
    fn mismatch_reroutes_current_key_once() {
        let mut runtime = runtime_with(r#"{"idle.submit":["ctrl+x ctrl+s"]}"#);
        let now = Instant::now();
        assert_eq!(
            runtime.resolve(idle_owner(), ctrl('x'), now),
            ShortcutResolution::Pending,
        );
        assert_eq!(
            runtime.resolve(idle_owner(), ctrl('f'), now + Duration::from_millis(1)),
            ShortcutResolution::RetryCurrentKey,
        );
        assert!(!runtime.has_pending_chord());
    }

    #[test]
    fn accepted_intermediate_resets_deadline_and_expiry_reroutes_normally() {
        let mut runtime = runtime_with(r#"{"idle.submit":["ctrl+x ctrl+a ctrl+s"]}"#);
        let now = Instant::now();
        runtime.resolve(idle_owner(), ctrl('x'), now);
        let first_deadline = runtime.next_deadline().unwrap();
        runtime.resolve(idle_owner(), ctrl('a'), now + Duration::from_millis(900));
        let second_deadline = runtime.next_deadline().unwrap();
        assert!(second_deadline > first_deadline);

        assert_eq!(
            runtime.resolve(
                idle_owner(),
                ctrl('s'),
                second_deadline + Duration::from_millis(1),
            ),
            ShortcutResolution::NoMatch,
        );
        assert!(!runtime.has_pending_chord());
    }

    #[test]
    fn release_is_ignored_and_repeat_advances() {
        let mut runtime = runtime_with(r#"{"idle.submit":["ctrl+x ctrl+s"]}"#);
        let now = Instant::now();
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..ctrl('x')
        };
        assert_eq!(
            runtime.resolve(idle_owner(), release, now),
            ShortcutResolution::NoMatch,
        );
        let repeat = KeyEvent {
            kind: KeyEventKind::Repeat,
            ..ctrl('x')
        };
        assert_eq!(
            runtime.resolve(idle_owner(), repeat, now),
            ShortcutResolution::Pending,
        );
    }

    #[test]
    fn cancel_clears_pending_before_action_and_following_key_is_normal() {
        let mut runtime = runtime_with(r#"{"idle.submit":["ctrl+x ctrl+s"]}"#);
        let now = Instant::now();
        runtime.resolve(idle_owner(), ctrl('x'), now);
        assert_eq!(
            runtime.resolve(idle_owner(), ctrl('c'), now + Duration::from_millis(1)),
            ShortcutResolution::Action(ShortcutInvocation::key(
                ShortcutAction::Global(GlobalShortcut::Cancel),
                ctrl('c'),
            )),
        );
        assert!(!runtime.has_pending_chord());
        assert_eq!(
            runtime.resolve(idle_owner(), ctrl('s'), now + Duration::from_millis(2)),
            ShortcutResolution::NoMatch,
        );
    }

    #[test]
    fn owner_and_explicit_boundaries_clear_pending() {
        let now = Instant::now();
        for modal in [
            ModalOwner::TranscriptSearch,
            ModalOwner::Shortcuts,
            ModalOwner::SlashMenu,
            ModalOwner::MentionMenu,
            ModalOwner::WorkflowPanel,
            ModalOwner::Setup,
            ModalOwner::SessionPicker,
            ModalOwner::Approval,
        ] {
            let mut runtime = runtime_with(r#"{"idle.submit":["ctrl+x ctrl+s"]}"#);
            runtime.resolve(idle_owner(), ctrl('x'), now);
            let changed_owner = InputOwnerFingerprint {
                modal,
                vim_mode: Some(VimMode::Normal),
                panel: PanelMode::Workflows,
                ..idle_owner()
            };
            assert_eq!(
                runtime.resolve(changed_owner, ctrl('s'), now + Duration::from_millis(1)),
                ShortcutResolution::NoMatch,
                "{modal:?}",
            );
        }

        let mut runtime = runtime_with(r#"{"idle.submit":["ctrl+x ctrl+s"]}"#);
        runtime.resolve(idle_owner(), ctrl('x'), now + Duration::from_millis(2));
        runtime.clear_for_non_key();
        assert!(!runtime.has_pending_chord());
        runtime.resolve(idle_owner(), ctrl('x'), now + Duration::from_millis(3));
        runtime.clear_for_suspend();
        assert!(!runtime.has_pending_chord());
    }

    #[test]
    fn replacing_keymap_clears_pending_and_advances_generation() {
        let mut runtime = runtime_with(r#"{"idle.submit":["ctrl+x ctrl+s"]}"#);
        let now = Instant::now();
        runtime.resolve(idle_owner(), ctrl('x'), now);
        let generation = runtime.generation();
        runtime.install(crate::keybindings::config::Keymap::built_in());

        assert!(!runtime.has_pending_chord());
        assert_eq!(runtime.generation(), generation + 1);
    }
}
