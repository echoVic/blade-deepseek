use crossbeam_channel as mpsc;
use std::io;
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_textarea::{Input, TextArea};

use orca_core::config::file::{
    UserConfigSaveError, UserPreferencePatch, save_api_key, save_user_preferences,
};
use orca_core::config::{RunConfig, ThemeName};
use orca_core::model::ModelSelection;

use crate::composer_textarea::{make_setup_textarea, make_textarea};
use crate::onboarding::{OnboardingError, OnboardingStep, SaveOutcome};
use crate::theme::Theme;
use crate::types::{AppState, AppStatus, ChatMessage, UserAction};
use crate::vim::VimState;

#[derive(Debug, PartialEq)]
pub(crate) enum SetupFlow {
    Continue,
    PreviewTheme(ThemeName),
    Exit(i32),
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_setup_key(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
    initial_prompt: Option<String>,
) -> io::Result<SetupFlow> {
    handle_setup_key_with_savers(
        ev,
        key,
        state,
        config,
        shared_config,
        action_tx,
        textarea,
        vim_state,
        theme,
        initial_prompt,
        save_api_key,
        save_user_preferences,
    )
}

#[allow(clippy::too_many_arguments)]
fn handle_setup_key_with_savers<AuthSave, PreferencesSave>(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
    initial_prompt: Option<String>,
    save_auth: AuthSave,
    save_preferences: PreferencesSave,
) -> io::Result<SetupFlow>
where
    AuthSave: FnOnce(&str) -> Result<(), UserConfigSaveError>,
    PreferencesSave: FnOnce(&UserPreferencePatch) -> Result<(), UserConfigSaveError>,
{
    match key.code {
        KeyCode::Esc => return Ok(SetupFlow::Exit(0)),
        KeyCode::Up | KeyCode::Char('k')
            if matches!(
                state.onboarding.step(),
                OnboardingStep::Provider | OnboardingStep::Model | OnboardingStep::Theme
            ) =>
        {
            return Ok(match state.onboarding.move_previous() {
                Some(theme) => SetupFlow::PreviewTheme(theme),
                None => SetupFlow::Continue,
            });
        }
        KeyCode::Down | KeyCode::Char('j')
            if matches!(
                state.onboarding.step(),
                OnboardingStep::Provider | OnboardingStep::Model | OnboardingStep::Theme
            ) =>
        {
            return Ok(match state.onboarding.move_next() {
                Some(theme) => SetupFlow::PreviewTheme(theme),
                None => SetupFlow::Continue,
            });
        }
        KeyCode::Enter => match state.onboarding.step() {
            OnboardingStep::Welcome
            | OnboardingStep::Provider
            | OnboardingStep::Model
            | OnboardingStep::Theme => {
                state.onboarding.advance();
            }
            OnboardingStep::ApiKey => {
                let api_key = textarea.lines().join("");
                state.onboarding.set_api_key(api_key);
                if state.onboarding.advance() {
                    *textarea = make_setup_textarea(theme);
                }
            }
            OnboardingStep::Review => {
                apply_review(state, config, shared_config, save_auth, save_preferences);
            }
            OnboardingStep::Complete => {
                state.set_status(AppStatus::Idle);
                *textarea = make_textarea(vim_state, theme);
                state.sync_vim_mode(vim_state);

                if let Some(prompt) = initial_prompt {
                    state.push_message(ChatMessage::User(prompt.clone()));
                    state.enter_running();
                    let _ = action_tx.send(UserAction::Submit(prompt));
                }
            }
        },
        _ if state.onboarding.step() == OnboardingStep::ApiKey => {
            textarea.input(Input::from(ev.clone()));
        }
        _ => {}
    }
    Ok(SetupFlow::Continue)
}

fn apply_review<AuthSave, PreferencesSave>(
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    save_auth: AuthSave,
    save_preferences: PreferencesSave,
) where
    AuthSave: FnOnce(&str) -> Result<(), UserConfigSaveError>,
    PreferencesSave: FnOnce(&UserPreferencePatch) -> Result<(), UserConfigSaveError>,
{
    let patch = match state.onboarding.validate_review() {
        Ok(patch) => patch,
        Err(_) => return,
    };
    let Some(api_key) = state.onboarding.api_key().map(str::to_string) else {
        state.onboarding.set_error(OnboardingError::MissingApiKey);
        return;
    };
    let provider = state.onboarding.draft_provider();
    let model = state.onboarding.draft_model().to_string();
    let theme = state.onboarding.selected_theme();
    let selection = match ModelSelection::parse(Some(model.clone())) {
        Ok(selection) => selection,
        Err(_) => {
            state
                .onboarding
                .set_error(OnboardingError::UnsupportedModel);
            return;
        }
    };
    let Ok(mut shared) = shared_config.lock() else {
        state
            .onboarding
            .set_error(OnboardingError::SharedConfigUnavailable);
        return;
    };

    config.provider = provider;
    config.model = selection.clone();
    config.theme = theme;
    config.api_key = Some(api_key.clone());
    shared.provider = provider;
    shared.model = selection;
    shared.theme = theme;
    shared.api_key = Some(api_key.clone());
    drop(shared);

    state.model_name = model;
    state.auth_configured = true;
    let auth_outcome = SaveOutcome::from(save_auth(&api_key));
    let preferences_outcome = SaveOutcome::from(save_preferences(&patch));
    if !state
        .onboarding
        .finish_review(auth_outcome, preferences_outcome)
    {
        state.onboarding.set_error(OnboardingError::InvalidStep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use orca_core::config::file::UserConfigSaveError;
    use orca_core::config::{ProviderKind, ThemeName};
    use orca_core::model::ModelSelection;

    use crate::composer_textarea::textarea_text;
    use crate::onboarding::{OnboardingError, OnboardingStep, SaveOutcome};

    #[derive(Default)]
    struct SaveCalls {
        auth: usize,
        preferences: usize,
    }

    fn setup_harness() -> (
        AppState,
        RunConfig,
        Arc<Mutex<RunConfig>>,
        mpsc::Sender<UserAction>,
        mpsc::Receiver<UserAction>,
        TextArea<'static>,
        VimState,
    ) {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut config = crate::test_support::test_run_config();
        config.provider = ProviderKind::DeepSeek;
        config.model = ModelSelection::from_unchecked(Some("auto".to_string()));
        config.theme = ThemeName::Auto;
        config.api_key = None;
        let shared = Arc::new(Mutex::new(config.clone()));
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "auto".to_string(),
            "/tmp".to_string(),
        );
        state.status = AppStatus::Setup;
        state.initialize_onboarding(&config);
        let theme = Theme::named(ThemeName::Dark);
        let textarea = make_setup_textarea(&theme);
        (
            state,
            config,
            shared,
            action_tx,
            action_rx,
            textarea,
            VimState::new(false),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn press_setup_key(
        code: KeyCode,
        state: &mut AppState,
        config: &mut RunConfig,
        shared: &Arc<Mutex<RunConfig>>,
        action_tx: &mpsc::Sender<UserAction>,
        textarea: &mut TextArea,
        vim: &VimState,
        theme: &Theme,
        initial_prompt: Option<String>,
        calls: &Arc<Mutex<SaveCalls>>,
        auth_result: Result<(), UserConfigSaveError>,
        preferences_result: Result<(), UserConfigSaveError>,
    ) -> SetupFlow {
        let key = KeyEvent::new(code, KeyModifiers::NONE);
        let auth_calls = Arc::clone(calls);
        let preference_calls = Arc::clone(calls);
        handle_setup_key_with_savers(
            &Event::Key(key),
            &key,
            state,
            config,
            shared,
            action_tx,
            textarea,
            vim,
            theme,
            initial_prompt,
            move |_| {
                auth_calls.lock().unwrap().auth += 1;
                auth_result
            },
            move |_| {
                preference_calls.lock().unwrap().preferences += 1;
                preferences_result
            },
        )
        .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn drive_to_review(
        state: &mut AppState,
        config: &mut RunConfig,
        shared: &Arc<Mutex<RunConfig>>,
        action_tx: &mpsc::Sender<UserAction>,
        textarea: &mut TextArea,
        vim: &VimState,
        theme: &Theme,
        calls: &Arc<Mutex<SaveCalls>>,
    ) {
        assert_eq!(state.onboarding.step(), OnboardingStep::Welcome);
        assert_eq!(
            press_setup_key(
                KeyCode::Enter,
                state,
                config,
                shared,
                action_tx,
                textarea,
                vim,
                theme,
                None,
                calls,
                Ok(()),
                Ok(()),
            ),
            SetupFlow::Continue,
        );
        assert_eq!(state.onboarding.step(), OnboardingStep::Provider);

        press_setup_key(
            KeyCode::Enter,
            state,
            config,
            shared,
            action_tx,
            textarea,
            vim,
            theme,
            None,
            calls,
            Ok(()),
            Ok(()),
        );
        assert_eq!(state.onboarding.step(), OnboardingStep::ApiKey);

        textarea.insert_str("  sk-test-secret  ");
        press_setup_key(
            KeyCode::Enter,
            state,
            config,
            shared,
            action_tx,
            textarea,
            vim,
            theme,
            None,
            calls,
            Ok(()),
            Ok(()),
        );
        assert_eq!(state.onboarding.step(), OnboardingStep::Model);
        assert_eq!(textarea_text(textarea), "");

        press_setup_key(
            KeyCode::Down,
            state,
            config,
            shared,
            action_tx,
            textarea,
            vim,
            theme,
            None,
            calls,
            Ok(()),
            Ok(()),
        );
        press_setup_key(
            KeyCode::Enter,
            state,
            config,
            shared,
            action_tx,
            textarea,
            vim,
            theme,
            None,
            calls,
            Ok(()),
            Ok(()),
        );
        assert_eq!(state.onboarding.step(), OnboardingStep::Theme);

        for _ in 0..3 {
            assert!(matches!(
                press_setup_key(
                    KeyCode::Char('j'),
                    state,
                    config,
                    shared,
                    action_tx,
                    textarea,
                    vim,
                    theme,
                    None,
                    calls,
                    Ok(()),
                    Ok(()),
                ),
                SetupFlow::PreviewTheme(_),
            ));
        }
        press_setup_key(
            KeyCode::Enter,
            state,
            config,
            shared,
            action_tx,
            textarea,
            vim,
            theme,
            None,
            calls,
            Ok(()),
            Ok(()),
        );
        assert_eq!(state.onboarding.step(), OnboardingStep::Review);
    }

    #[test]
    fn enter_advances_exact_wizard_sequence_without_early_persistence() {
        let (mut state, mut config, shared, action_tx, _rx, mut textarea, vim) = setup_harness();
        let theme = Theme::named(ThemeName::Dark);
        let calls = Arc::new(Mutex::new(SaveCalls::default()));

        drive_to_review(
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &vim,
            &theme,
            &calls,
        );

        assert!(config.api_key.is_none());
        assert!(shared.lock().unwrap().api_key.is_none());
        assert!(!state.auth_configured);
        assert_eq!(calls.lock().unwrap().auth, 0);
        assert_eq!(calls.lock().unwrap().preferences, 0);
        assert!(
            !state
                .onboarding
                .review_rows()
                .join("\n")
                .contains("sk-test-secret")
        );
    }

    #[test]
    fn empty_api_key_stays_on_api_key_without_persistence() {
        let (mut state, mut config, shared, action_tx, _rx, mut textarea, vim) = setup_harness();
        let theme = Theme::named(ThemeName::Dark);
        let calls = Arc::new(Mutex::new(SaveCalls::default()));
        state.onboarding.set_step_for_test(OnboardingStep::ApiKey);
        textarea.insert_str("   ");

        press_setup_key(
            KeyCode::Enter,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &vim,
            &theme,
            None,
            &calls,
            Ok(()),
            Ok(()),
        );

        assert_eq!(state.onboarding.step(), OnboardingStep::ApiKey);
        assert_eq!(
            state.onboarding.review_error(),
            Some(OnboardingError::MissingApiKey)
        );
        assert!(config.api_key.is_none());
        assert_eq!(calls.lock().unwrap().auth, 0);
        assert_eq!(calls.lock().unwrap().preferences, 0);
    }

    #[test]
    fn api_key_textarea_accepts_j_and_k_as_regular_input() {
        let (mut state, mut config, shared, action_tx, _rx, mut textarea, vim) = setup_harness();
        let theme = Theme::named(ThemeName::Dark);
        let calls = Arc::new(Mutex::new(SaveCalls::default()));
        state.onboarding.set_step_for_test(OnboardingStep::ApiKey);

        for key in [KeyCode::Char('j'), KeyCode::Char('k')] {
            press_setup_key(
                key,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &mut textarea,
                &vim,
                &theme,
                None,
                &calls,
                Ok(()),
                Ok(()),
            );
        }

        assert_eq!(textarea_text(&textarea), "jk");
    }

    #[test]
    fn selection_keys_wrap_only_options_and_only_theme_previews() {
        let keys = [
            KeyCode::Up,
            KeyCode::Char('k'),
            KeyCode::Down,
            KeyCode::Char('j'),
        ];
        for step in OnboardingStep::ALL {
            for key in keys {
                let (mut state, mut config, shared, action_tx, _rx, mut textarea, vim) =
                    setup_harness();
                let theme = Theme::named(ThemeName::Dark);
                let calls = Arc::new(Mutex::new(SaveCalls::default()));
                state.onboarding.set_step_for_test(step);
                let before = state.onboarding.option_rows();
                let flow = press_setup_key(
                    key,
                    &mut state,
                    &mut config,
                    &shared,
                    &action_tx,
                    &mut textarea,
                    &vim,
                    &theme,
                    None,
                    &calls,
                    Ok(()),
                    Ok(()),
                );
                if !matches!(
                    step,
                    OnboardingStep::Provider | OnboardingStep::Model | OnboardingStep::Theme
                ) {
                    assert_eq!(state.onboarding.option_rows(), before, "{step:?} {key:?}");
                }
                assert_eq!(
                    matches!(flow, SetupFlow::PreviewTheme(_)),
                    step == OnboardingStep::Theme,
                    "{step:?} {key:?}",
                );
            }
        }
    }

    #[test]
    fn review_applies_memory_before_independent_persistence_results() {
        let (mut state, mut config, shared, action_tx, action_rx, mut textarea, vim) =
            setup_harness();
        let theme = Theme::named(ThemeName::Dark);
        let calls = Arc::new(Mutex::new(SaveCalls::default()));
        drive_to_review(
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &vim,
            &theme,
            &calls,
        );

        assert_eq!(
            press_setup_key(
                KeyCode::Enter,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &mut textarea,
                &vim,
                &theme,
                None,
                &calls,
                Err(UserConfigSaveError::WriteFailed),
                Ok(()),
            ),
            SetupFlow::Continue,
        );
        assert_eq!(state.onboarding.step(), OnboardingStep::Complete);
        assert_eq!(config.provider, ProviderKind::DeepSeek);
        assert_eq!(config.model.display_name(), "deepseek-v4-flash");
        assert_eq!(config.theme, ThemeName::Solarized);
        assert_eq!(config.api_key.as_deref(), Some("sk-test-secret"));
        let shared = shared.lock().unwrap();
        assert_eq!(shared.provider, config.provider);
        assert_eq!(shared.model, config.model);
        assert_eq!(shared.theme, config.theme);
        assert_eq!(shared.api_key, config.api_key);
        drop(shared);
        assert!(state.auth_configured);
        assert_eq!(state.model_name, "deepseek-v4-flash");
        assert_eq!(
            state.onboarding.auth_outcome(),
            &SaveOutcome::Failed(UserConfigSaveError::WriteFailed),
        );
        assert_eq!(state.onboarding.preferences_outcome(), &SaveOutcome::Saved);
        assert_eq!(calls.lock().unwrap().auth, 1);
        assert_eq!(calls.lock().unwrap().preferences, 1);
        assert!(state.onboarding.api_key().is_none());
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn poisoned_shared_config_keeps_review_transaction_unapplied() {
        let (mut state, mut config, shared, action_tx, _rx, mut textarea, vim) = setup_harness();
        let theme = Theme::named(ThemeName::Dark);
        let calls = Arc::new(Mutex::new(SaveCalls::default()));
        drive_to_review(
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &vim,
            &theme,
            &calls,
        );
        let poison = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _guard = poison.lock().unwrap();
            panic!("poison shared config");
        })
        .join();

        press_setup_key(
            KeyCode::Enter,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &vim,
            &theme,
            None,
            &calls,
            Ok(()),
            Ok(()),
        );

        assert_eq!(state.onboarding.step(), OnboardingStep::Review);
        assert!(config.api_key.is_none());
        assert!(!state.auth_configured);
        assert_eq!(
            state.onboarding.review_error(),
            Some(OnboardingError::SharedConfigUnavailable),
        );
        assert_eq!(calls.lock().unwrap().auth, 0);
        assert_eq!(calls.lock().unwrap().preferences, 0);
    }

    #[test]
    fn invalid_review_draft_stays_staged_without_apply_or_save() {
        let (mut state, mut config, shared, action_tx, _rx, mut textarea, vim) = setup_harness();
        let theme = Theme::named(ThemeName::Dark);
        let calls = Arc::new(Mutex::new(SaveCalls::default()));
        state.onboarding.set_step_for_test(OnboardingStep::Review);
        state.onboarding.set_api_key("sk-staged".to_string());
        state
            .onboarding
            .set_model_for_test("unsupported-model".to_string());
        let config_before = config.clone();
        let shared_before = shared.lock().unwrap().clone();

        press_setup_key(
            KeyCode::Enter,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &vim,
            &theme,
            None,
            &calls,
            Ok(()),
            Ok(()),
        );

        assert_eq!(state.onboarding.step(), OnboardingStep::Review);
        assert_eq!(
            state.onboarding.review_error(),
            Some(OnboardingError::UnsupportedModel),
        );
        assert_eq!(config.provider, config_before.provider);
        assert_eq!(config.model, config_before.model);
        assert_eq!(config.theme, config_before.theme);
        assert_eq!(config.api_key, config_before.api_key);
        let shared_after = shared.lock().unwrap();
        assert_eq!(shared_after.provider, shared_before.provider);
        assert_eq!(shared_after.model, shared_before.model);
        assert_eq!(shared_after.theme, shared_before.theme);
        assert_eq!(shared_after.api_key, shared_before.api_key);
        assert_eq!(calls.lock().unwrap().auth, 0);
        assert_eq!(calls.lock().unwrap().preferences, 0);
    }

    #[test]
    fn esc_from_every_step_exits_without_persistence() {
        for step in OnboardingStep::ALL {
            let (mut state, mut config, shared, action_tx, _rx, mut textarea, vim) =
                setup_harness();
            let theme = Theme::named(ThemeName::Dark);
            let calls = Arc::new(Mutex::new(SaveCalls::default()));
            state.onboarding.set_step_for_test(step);
            state.onboarding.set_api_key("sk-test-secret".to_string());

            assert_eq!(
                press_setup_key(
                    KeyCode::Esc,
                    &mut state,
                    &mut config,
                    &shared,
                    &action_tx,
                    &mut textarea,
                    &vim,
                    &theme,
                    None,
                    &calls,
                    Ok(()),
                    Ok(()),
                ),
                SetupFlow::Exit(0),
                "{step:?}",
            );
            assert!(config.api_key.is_none());
            assert_eq!(calls.lock().unwrap().auth, 0);
            assert_eq!(calls.lock().unwrap().preferences, 0);
        }
    }

    #[test]
    fn complete_enter_dispatches_initial_prompt_once_after_setup() {
        let (mut state, mut config, shared, action_tx, action_rx, mut textarea, vim) =
            setup_harness();
        let theme = Theme::named(ThemeName::Dark);
        let calls = Arc::new(Mutex::new(SaveCalls::default()));
        drive_to_review(
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &vim,
            &theme,
            &calls,
        );
        press_setup_key(
            KeyCode::Enter,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &vim,
            &theme,
            Some("start".to_string()),
            &calls,
            Ok(()),
            Ok(()),
        );
        assert!(action_rx.try_recv().is_err());

        press_setup_key(
            KeyCode::Enter,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &vim,
            &theme,
            Some("start".to_string()),
            &calls,
            Ok(()),
            Ok(()),
        );

        assert_eq!(state.status, AppStatus::Running);
        assert_eq!(state.messages.len(), 1);
        assert!(matches!(&state.messages[0], ChatMessage::User(message) if message == "start"));
        assert!(
            matches!(action_rx.try_recv(), Ok(UserAction::Submit(prompt)) if prompt == "start")
        );
        assert!(action_rx.try_recv().is_err());
    }
}
