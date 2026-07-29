#![cfg_attr(not(test), allow(dead_code))]

use orca_core::config::file::{
    UserConfigSaveError, UserPreferencePatch, UserPreferenceValidationError,
};
use orca_core::config::{ProviderKind, ThemeName};

const PROVIDER_OPTIONS: [ProviderKind; 1] = [ProviderKind::DeepSeek];
const THEME_OPTIONS: [ThemeName; 5] = [
    ThemeName::Auto,
    ThemeName::Dark,
    ThemeName::Light,
    ThemeName::Solarized,
    ThemeName::Catppuccin,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OnboardingStep {
    Welcome,
    Provider,
    ApiKey,
    Model,
    Theme,
    Review,
    Complete,
}

impl OnboardingStep {
    pub(crate) const ALL: [Self; 7] = [
        Self::Welcome,
        Self::Provider,
        Self::ApiKey,
        Self::Model,
        Self::Theme,
        Self::Review,
        Self::Complete,
    ];

    pub(crate) const fn ordinal(self) -> usize {
        match self {
            Self::Welcome => 1,
            Self::Provider => 2,
            Self::ApiKey => 3,
            Self::Model => 4,
            Self::Theme => 5,
            Self::Review => 6,
            Self::Complete => 7,
        }
    }

    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::Provider => "Provider",
            Self::ApiKey => "API Key",
            Self::Model => "Model",
            Self::Theme => "Theme",
            Self::Review => "Review",
            Self::Complete => "Complete",
        }
    }

    pub(crate) const fn instruction(self) -> &'static str {
        match self {
            Self::Welcome => "Set up local defaults for this device.",
            Self::Provider => "Choose the production provider.",
            Self::ApiKey => "Enter your DeepSeek API key.",
            Self::Model => "Choose the default model.",
            Self::Theme => "Choose a theme to preview.",
            Self::Review => "Confirm these local defaults.",
            Self::Complete => "Setup is complete.",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SaveOutcome {
    NotAttempted,
    Saved,
    Failed(UserConfigSaveError),
}

impl SaveOutcome {
    const fn safe_label(&self) -> &'static str {
        match self {
            Self::NotAttempted => "save not attempted",
            Self::Saved => "saved",
            Self::Failed(error) => error.safe_label(),
        }
    }

    const fn was_attempted(&self) -> bool {
        !matches!(self, Self::NotAttempted)
    }
}

impl From<Result<(), UserConfigSaveError>> for SaveOutcome {
    fn from(result: Result<(), UserConfigSaveError>) -> Self {
        match result {
            Ok(()) => Self::Saved,
            Err(error) => Self::Failed(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OnboardingError {
    MissingApiKey,
    InvalidStep,
    UnsupportedProvider,
    UnsupportedModel,
    SharedConfigUnavailable,
}

impl OnboardingError {
    pub(crate) const fn safe_label(self) -> &'static str {
        match self {
            Self::MissingApiKey => "API key is required",
            Self::InvalidStep => "review is unavailable",
            Self::UnsupportedProvider => "unsupported provider selection",
            Self::UnsupportedModel => "unsupported model selection",
            Self::SharedConfigUnavailable => "shared configuration unavailable",
        }
    }
}

pub(crate) const fn production_provider_options() -> &'static [ProviderKind] {
    &PROVIDER_OPTIONS
}

pub(crate) fn model_options() -> &'static [&'static str] {
    orca_core::model::allowed_models()
}

pub(crate) const fn theme_options() -> &'static [ThemeName] {
    &THEME_OPTIONS
}

struct OnboardingDraft {
    provider: ProviderKind,
    model: String,
    theme: ThemeName,
    api_key: Option<String>,
}

pub(crate) struct OnboardingState {
    step: OnboardingStep,
    draft: OnboardingDraft,
    selected: usize,
    auth_outcome: SaveOutcome,
    preferences_outcome: SaveOutcome,
    error: Option<OnboardingError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OnboardingOptionRow {
    pub(crate) label: &'static str,
    pub(crate) description: &'static str,
    pub(crate) selected: bool,
}

impl OnboardingState {
    pub(crate) fn new(provider: ProviderKind, model: &str, theme: ThemeName) -> Self {
        let provider = if production_provider_options().contains(&provider) {
            provider
        } else {
            ProviderKind::DeepSeek
        };
        let model = if model_options().contains(&model) {
            model.to_string()
        } else {
            orca_core::model::AUTO_MODEL.to_string()
        };
        Self {
            step: OnboardingStep::Welcome,
            draft: OnboardingDraft {
                provider,
                model,
                theme,
                api_key: None,
            },
            selected: 0,
            auth_outcome: SaveOutcome::NotAttempted,
            preferences_outcome: SaveOutcome::NotAttempted,
            error: None,
        }
    }

    pub(crate) const fn step(&self) -> OnboardingStep {
        self.step
    }

    pub(crate) const fn draft_provider(&self) -> ProviderKind {
        self.draft.provider
    }

    pub(crate) fn draft_model(&self) -> &str {
        &self.draft.model
    }

    pub(crate) const fn selected_theme(&self) -> ThemeName {
        self.draft.theme
    }

    pub(crate) fn set_api_key(&mut self, api_key: String) {
        let api_key = api_key.trim();
        if api_key.is_empty() {
            self.draft.api_key = None;
            self.error = Some(OnboardingError::MissingApiKey);
        } else {
            self.draft.api_key = Some(api_key.to_string());
            self.error = None;
        }
    }

    pub(crate) fn api_key(&self) -> Option<&str> {
        self.draft.api_key.as_deref()
    }

    pub(crate) fn take_api_key(&mut self) -> Option<String> {
        self.draft.api_key.take()
    }

    pub(crate) fn review_rows(&self) -> Vec<String> {
        vec![
            "Provider: DeepSeek".to_string(),
            format!("Model: {}", self.draft.model),
            format!("Theme: {}", self.draft.theme.as_str()),
            format!(
                "API key: {}",
                if self.draft.api_key.is_some() {
                    "configured"
                } else {
                    "missing"
                },
            ),
        ]
    }

    pub(crate) fn option_rows(&self) -> Vec<OnboardingOptionRow> {
        match self.step {
            OnboardingStep::Provider => PROVIDER_OPTIONS
                .iter()
                .enumerate()
                .map(|(index, _)| OnboardingOptionRow {
                    label: "DeepSeek",
                    description: "Production provider",
                    selected: index == self.selected,
                })
                .collect(),
            OnboardingStep::Model => model_options()
                .iter()
                .enumerate()
                .map(|(index, model)| OnboardingOptionRow {
                    label: model,
                    description: match *model {
                        orca_core::model::AUTO_MODEL => "Recommended",
                        orca_core::model::FLASH_MODEL => "Faster",
                        orca_core::model::PRO_MODEL => "Highest quality",
                        _ => "Unsupported",
                    },
                    selected: index == self.selected,
                })
                .collect(),
            OnboardingStep::Theme => THEME_OPTIONS
                .iter()
                .enumerate()
                .map(|(index, theme)| OnboardingOptionRow {
                    label: theme.as_str(),
                    description: match theme {
                        ThemeName::Auto => "Match terminal background",
                        ThemeName::Dark => "Dark",
                        ThemeName::Light => "Light",
                        ThemeName::Solarized => "Solarized dark",
                        ThemeName::Catppuccin => "Catppuccin Mocha",
                    },
                    selected: index == self.selected,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn completion_rows(&self) -> Vec<String> {
        fn row(name: &str, outcome: &SaveOutcome) -> String {
            match outcome {
                SaveOutcome::Saved => format!("{name}: {}", outcome.safe_label()),
                SaveOutcome::NotAttempted | SaveOutcome::Failed(_) => {
                    format!("{name}: current session only — {}", outcome.safe_label(),)
                }
            }
        }

        vec![
            row("API key", &self.auth_outcome),
            row("Preferences", &self.preferences_outcome),
        ]
    }

    pub(crate) const fn error_label(&self) -> Option<&'static str> {
        match self.error {
            Some(error) => Some(error.safe_label()),
            None => None,
        }
    }

    pub(crate) const fn auth_outcome(&self) -> &SaveOutcome {
        &self.auth_outcome
    }

    pub(crate) const fn preferences_outcome(&self) -> &SaveOutcome {
        &self.preferences_outcome
    }

    pub(crate) const fn review_error(&self) -> Option<OnboardingError> {
        self.error
    }

    pub(crate) fn set_error(&mut self, error: OnboardingError) {
        self.error = Some(error);
    }

    pub(crate) fn validate_review(&mut self) -> Result<UserPreferencePatch, OnboardingError> {
        if self.step != OnboardingStep::Review {
            return self.fail_review_validation(OnboardingError::InvalidStep);
        }
        let patch = match UserPreferencePatch::new(
            self.draft.provider,
            self.draft.model.clone(),
            self.draft.theme,
        ) {
            Ok(patch) => patch,
            Err(UserPreferenceValidationError::UnsupportedProvider) => {
                return self.fail_review_validation(OnboardingError::UnsupportedProvider);
            }
            Err(UserPreferenceValidationError::UnsupportedModel) => {
                return self.fail_review_validation(OnboardingError::UnsupportedModel);
            }
        };
        if !self
            .draft
            .api_key
            .as_deref()
            .is_some_and(|api_key| !api_key.trim().is_empty())
        {
            return self.fail_review_validation(OnboardingError::MissingApiKey);
        }
        self.error = None;
        Ok(patch)
    }

    fn fail_review_validation<T>(&mut self, error: OnboardingError) -> Result<T, OnboardingError> {
        self.error = Some(error);
        Err(error)
    }

    pub(crate) fn finish_review(&mut self, auth: SaveOutcome, preferences: SaveOutcome) -> bool {
        if self.step != OnboardingStep::Review
            || self.auth_outcome != SaveOutcome::NotAttempted
            || self.preferences_outcome != SaveOutcome::NotAttempted
            || !auth.was_attempted()
            || !preferences.was_attempted()
        {
            return false;
        }
        self.auth_outcome = auth;
        self.preferences_outcome = preferences;
        self.draft.api_key = None;
        self.error = None;
        self.step = OnboardingStep::Complete;
        true
    }

    pub(crate) fn advance(&mut self) -> bool {
        let next = match self.step {
            OnboardingStep::Welcome => OnboardingStep::Provider,
            OnboardingStep::Provider => OnboardingStep::ApiKey,
            OnboardingStep::ApiKey if self.draft.api_key.is_some() => OnboardingStep::Model,
            OnboardingStep::Model => OnboardingStep::Theme,
            OnboardingStep::Theme => OnboardingStep::Review,
            OnboardingStep::ApiKey => {
                self.error = Some(OnboardingError::MissingApiKey);
                return false;
            }
            OnboardingStep::Review | OnboardingStep::Complete => {
                return false;
            }
        };
        self.step = next;
        self.selected = self.index_for_current_value();
        self.error = None;
        true
    }

    pub(crate) fn move_previous(&mut self) -> Option<ThemeName> {
        self.move_by(-1)
    }

    pub(crate) fn move_next(&mut self) -> Option<ThemeName> {
        self.move_by(1)
    }

    fn move_by(&mut self, delta: isize) -> Option<ThemeName> {
        let len = self.option_len();
        if len == 0 {
            self.error = Some(OnboardingError::UnsupportedModel);
            return None;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        self.error = None;
        match self.step {
            OnboardingStep::Provider => {
                self.draft.provider = PROVIDER_OPTIONS[self.selected];
                None
            }
            OnboardingStep::Model => {
                self.draft.model = model_options()[self.selected].to_string();
                None
            }
            OnboardingStep::Theme => {
                self.draft.theme = THEME_OPTIONS[self.selected];
                Some(self.draft.theme)
            }
            _ => None,
        }
    }

    fn option_len(&self) -> usize {
        match self.step {
            OnboardingStep::Provider => PROVIDER_OPTIONS.len(),
            OnboardingStep::Model => model_options().len(),
            OnboardingStep::Theme => THEME_OPTIONS.len(),
            _ => 0,
        }
    }

    fn index_for_current_value(&self) -> usize {
        match self.step {
            OnboardingStep::Provider => PROVIDER_OPTIONS
                .iter()
                .position(|value| *value == self.draft.provider)
                .unwrap_or(0),
            OnboardingStep::Model => model_options()
                .iter()
                .position(|value| *value == self.draft.model)
                .unwrap_or(0),
            OnboardingStep::Theme => THEME_OPTIONS
                .iter()
                .position(|value| *value == self.draft.theme)
                .unwrap_or(0),
            _ => 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_step_for_test(&mut self, step: OnboardingStep) {
        self.step = step;
        self.selected = self.index_for_current_value();
    }

    #[cfg(test)]
    pub(crate) fn set_model_for_test(&mut self, model: String) {
        self.draft.model = model;
        self.selected = self.index_for_current_value();
    }

    #[cfg(test)]
    pub(crate) fn set_outcomes_for_test(&mut self, auth: SaveOutcome, preferences: SaveOutcome) {
        self.auth_outcome = auth;
        self.preferences_outcome = preferences;
    }
}

#[cfg(test)]
mod tests {
    use orca_core::config::file::UserConfigSaveError;
    use orca_core::config::{ProviderKind, ThemeName};

    use super::*;

    const USER_CONFIG_SAVE_ERRORS: [UserConfigSaveError; 11] = [
        UserConfigSaveError::ConfigDirectoryUnavailable,
        UserConfigSaveError::UnsafeExistingPath,
        UserConfigSaveError::ExistingFileTooLarge,
        UserConfigSaveError::InvalidExistingContent,
        UserConfigSaveError::ConcurrentModification,
        UserConfigSaveError::CreateDirectoryFailed,
        UserConfigSaveError::CreateTemporaryFileFailed,
        UserConfigSaveError::ReadFailed,
        UserConfigSaveError::WriteFailed,
        UserConfigSaveError::ReplaceFailed,
        UserConfigSaveError::RollbackFailed,
    ];

    fn assert_user_config_save_error_is_safe(error: UserConfigSaveError) {
        match error {
            UserConfigSaveError::ConfigDirectoryUnavailable
            | UserConfigSaveError::UnsafeExistingPath
            | UserConfigSaveError::ExistingFileTooLarge
            | UserConfigSaveError::InvalidExistingContent
            | UserConfigSaveError::ConcurrentModification
            | UserConfigSaveError::CreateDirectoryFailed
            | UserConfigSaveError::CreateTemporaryFileFailed
            | UserConfigSaveError::ReadFailed
            | UserConfigSaveError::WriteFailed
            | UserConfigSaveError::ReplaceFailed
            | UserConfigSaveError::RollbackFailed => {}
        }
        let label = error.safe_label();
        assert!(!label.contains('/'));
        assert!(!label.contains("sk-"));
        assert!(!label.chars().any(char::is_control));
    }

    fn state() -> OnboardingState {
        OnboardingState::new(ProviderKind::DeepSeek, "auto", ThemeName::Auto)
    }

    fn markdown_heading_level(line: &str) -> Option<usize> {
        let line = line.trim_end_matches(['\r', '\n']);
        let level = line.bytes().take_while(|byte| *byte == b'#').count();
        (level > 0 && level <= 6 && line.as_bytes().get(level) == Some(&b' ')).then_some(level)
    }

    fn markdown_fence(line: &str) -> Option<(u8, usize, &str)> {
        let line = line.trim_end_matches(['\r', '\n']);
        let indent = line.bytes().take_while(|byte| *byte == b' ').count();
        if indent > 3 {
            return None;
        }
        let content = &line[indent..];
        let marker = *content.as_bytes().first()?;
        if !matches!(marker, b'`' | b'~') {
            return None;
        }
        let length = content.bytes().take_while(|byte| *byte == marker).count();
        (length >= 3).then_some((marker, length, &content[length..]))
    }

    fn markdown_section<'a>(readme: &'a str, heading: &str) -> Result<&'a str, &'static str> {
        let target_level = markdown_heading_level(heading).ok_or("invalid heading")?;
        let mut fence = None;
        let mut offset = 0;
        let mut start = None;
        let mut end = None;
        let mut matches = 0;

        for line in readme.split_inclusive('\n') {
            if let Some((marker, length)) = fence {
                if let Some((candidate, candidate_length, suffix)) = markdown_fence(line)
                    && candidate == marker
                    && candidate_length >= length
                    && suffix.trim().is_empty()
                {
                    fence = None;
                }
                offset += line.len();
                continue;
            }

            if let Some((marker, length, _)) = markdown_fence(line) {
                fence = Some((marker, length));
                offset += line.len();
                continue;
            }

            let line_without_ending = line.trim_end_matches(['\r', '\n']);
            if line_without_ending == heading {
                matches += 1;
                start.get_or_insert(offset);
            } else if start.is_some()
                && end.is_none()
                && markdown_heading_level(line).is_some_and(|level| level <= target_level)
            {
                end = Some(offset);
            }
            offset += line.len();
        }

        match matches {
            0 => Err("missing heading"),
            1 => {
                let start = start.expect("matched heading start");
                Ok(&readme[start..end.unwrap_or(readme.len())])
            }
            _ => Err("duplicate heading"),
        }
    }

    #[test]
    fn onboarding_has_exact_seven_step_order() {
        assert_eq!(
            OnboardingStep::ALL,
            [
                OnboardingStep::Welcome,
                OnboardingStep::Provider,
                OnboardingStep::ApiKey,
                OnboardingStep::Model,
                OnboardingStep::Theme,
                OnboardingStep::Review,
                OnboardingStep::Complete,
            ],
        );
        assert_eq!(
            OnboardingStep::ALL.map(OnboardingStep::ordinal),
            [1, 2, 3, 4, 5, 6, 7],
        );
        assert_eq!(
            OnboardingStep::ALL.map(OnboardingStep::title),
            [
                "Welcome", "Provider", "API Key", "Model", "Theme", "Review", "Complete",
            ],
        );
        assert!(
            OnboardingStep::ALL
                .iter()
                .all(|step| !step.instruction().is_empty())
        );
    }

    #[test]
    fn onboarding_choices_are_closed_and_production_safe() {
        assert_eq!(production_provider_options(), [ProviderKind::DeepSeek]);
        assert!(!production_provider_options().contains(&ProviderKind::Mock));
        assert!(!production_provider_options().contains(&ProviderKind::DeepSeekFixture));
        assert_eq!(model_options(), orca_core::model::allowed_models());
        assert_eq!(
            theme_options(),
            [
                ThemeName::Auto,
                ThemeName::Dark,
                ThemeName::Light,
                ThemeName::Solarized,
                ThemeName::Catppuccin,
            ],
        );
    }

    #[test]
    fn markdown_section_requires_one_exact_heading_outside_fences() {
        let fenced_only = "# Guide\n\n```markdown\n### Setup\n```\n";
        assert_eq!(
            markdown_section(fenced_only, "### Setup"),
            Err("missing heading")
        );

        let duplicate = "# Guide\n\n### Setup\none\n\n### Setup\ntwo\n";
        assert_eq!(
            markdown_section(duplicate, "### Setup"),
            Err("duplicate heading")
        );

        let prefix = "# Guide\n\n### Setup details\nwrong\n";
        assert_eq!(
            markdown_section(prefix, "### Setup"),
            Err("missing heading")
        );
    }

    #[test]
    fn markdown_section_stops_at_same_or_higher_level_heading() {
        let readme =
            "# Guide\n\n### Setup\ninside\n\n#### Detail\nstill inside\n\n## Next\noutside\n";
        assert_eq!(
            markdown_section(readme, "### Setup"),
            Ok("### Setup\ninside\n\n#### Detail\nstill inside\n\n"),
        );
    }

    #[test]
    fn readmes_document_expanded_first_run_onboarding_contract() {
        fn assert_required(name: &str, section: &str, required: &[&str]) {
            for token in required {
                assert!(
                    section.contains(token),
                    "{name} onboarding docs must contain {token:?}",
                );
            }
        }

        fn assert_safe_examples(name: &str, section: &str) {
            for forbidden in ["Mock", "Fixture", "/Users/", "/home/", "C:\\", "sk-"] {
                assert!(
                    !section.contains(forbidden),
                    "{name} onboarding docs must not contain {forbidden:?}",
                );
            }
        }

        let english = markdown_section(
            include_str!("../../../README.md"),
            "### First-run onboarding",
        )
        .expect("unique English onboarding heading");
        assert_required(
            "README.md",
            english,
            &[
                "When the TUI starts without an effective API key, first-run onboarding follows exactly seven steps",
                "Welcome → Provider → API Key → Model → Theme → Review → Complete",
                "DeepSeek is the only production provider",
                "auto",
                "deepseek-v4-flash",
                "deepseek-v4-pro",
                "Auto",
                "Dark",
                "Light",
                "Solarized",
                "Catppuccin",
                "config.toml",
                "auth.json",
                "no network validation",
                "current session",
                "reports only sanitized error categories",
                "Esc",
                "zero writes",
                "draft-only",
            ],
        );
        assert_safe_examples("README.md", english);

        let chinese =
            markdown_section(include_str!("../../../README.zh-CN.md"), "### 首次启动设置")
                .expect("unique Chinese onboarding heading");
        assert_required(
            "README.zh-CN.md",
            chinese,
            &[
                "当 TUI 启动时未检测到有效 API 密钥，首次启动设置固定经过七步",
                "欢迎 → 服务商 → API 密钥 → 模型 → 主题 → 确认 → 完成",
                "DeepSeek 是唯一的生产服务商",
                "auto",
                "deepseek-v4-flash",
                "deepseek-v4-pro",
                "Auto",
                "Dark",
                "Light",
                "Solarized",
                "Catppuccin",
                "config.toml",
                "auth.json",
                "不进行网络验证",
                "当前会话",
                "不会产生任何写入",
                "仅显示不含敏感信息的错误类型",
                "Esc",
                "仅保存在草稿中",
            ],
        );
        assert_safe_examples("README.zh-CN.md", chinese);
    }

    #[test]
    fn initial_draft_uses_effective_values_and_normalizes_hidden_values() {
        let effective = OnboardingState::new(
            ProviderKind::DeepSeek,
            orca_core::model::PRO_MODEL,
            ThemeName::Solarized,
        );
        assert_eq!(effective.step(), OnboardingStep::Welcome);
        assert_eq!(effective.draft_provider(), ProviderKind::DeepSeek);
        assert_eq!(effective.draft_model(), orca_core::model::PRO_MODEL);
        assert_eq!(effective.selected_theme(), ThemeName::Solarized);

        for provider in [ProviderKind::Mock, ProviderKind::DeepSeekFixture] {
            let normalized = OnboardingState::new(provider, "unsupported", ThemeName::Light);
            assert_eq!(normalized.draft_provider(), ProviderKind::DeepSeek);
            assert_eq!(normalized.draft_model(), orca_core::model::AUTO_MODEL);
            assert_eq!(normalized.selected_theme(), ThemeName::Light);
        }
    }

    #[test]
    fn review_rows_never_include_api_key() {
        let mut state = OnboardingState::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark);
        state.set_api_key("sk-do-not-render".to_string());

        let review = state.review_rows().join("\n");
        assert_eq!(
            state.review_rows(),
            [
                "Provider: DeepSeek",
                "Model: auto",
                "Theme: dark",
                "API key: configured",
            ],
        );
        assert!(!review.contains("sk-do-not-render"));
        assert!(!format!("{:?}", state.review_rows()).contains("sk-do-not-render"));
    }

    #[test]
    fn option_rows_have_exact_safe_labels_and_descriptions() {
        let mut state = state();

        state.set_step_for_test(OnboardingStep::Provider);
        assert_eq!(
            state.option_rows(),
            [OnboardingOptionRow {
                label: "DeepSeek",
                description: "Production provider",
                selected: true,
            }],
        );

        state.set_step_for_test(OnboardingStep::Model);
        assert_eq!(
            state.option_rows(),
            [
                OnboardingOptionRow {
                    label: "auto",
                    description: "Recommended",
                    selected: true,
                },
                OnboardingOptionRow {
                    label: "deepseek-v4-flash",
                    description: "Faster",
                    selected: false,
                },
                OnboardingOptionRow {
                    label: "deepseek-v4-pro",
                    description: "Highest quality",
                    selected: false,
                },
            ],
        );

        state.set_step_for_test(OnboardingStep::Theme);
        assert_eq!(
            state.option_rows(),
            [
                OnboardingOptionRow {
                    label: "auto",
                    description: "Match terminal background",
                    selected: true,
                },
                OnboardingOptionRow {
                    label: "dark",
                    description: "Dark",
                    selected: false,
                },
                OnboardingOptionRow {
                    label: "light",
                    description: "Light",
                    selected: false,
                },
                OnboardingOptionRow {
                    label: "solarized",
                    description: "Solarized dark",
                    selected: false,
                },
                OnboardingOptionRow {
                    label: "catppuccin",
                    description: "Catppuccin Mocha",
                    selected: false,
                },
            ],
        );
    }

    #[test]
    fn option_selection_wraps_and_updates_draft() {
        let mut state = state();
        state.set_step_for_test(OnboardingStep::Theme);

        assert_eq!(state.selected_theme(), ThemeName::Auto);
        assert_eq!(state.move_previous(), Some(ThemeName::Catppuccin));
        assert_eq!(state.move_next(), Some(ThemeName::Auto));
        assert_eq!(state.move_next(), Some(ThemeName::Dark));

        state.set_step_for_test(OnboardingStep::Model);
        assert_eq!(state.move_previous(), None);
        assert_eq!(state.draft_model(), orca_core::model::PRO_MODEL);
        assert_eq!(state.move_next(), None);
        assert_eq!(state.draft_model(), orca_core::model::AUTO_MODEL);

        state.set_step_for_test(OnboardingStep::Provider);
        assert_eq!(state.move_next(), None);
        assert_eq!(state.draft_provider(), ProviderKind::DeepSeek);
    }

    #[test]
    fn test_helpers_realign_selection_and_set_persistence_outcomes() {
        let mut state = OnboardingState::new(
            ProviderKind::DeepSeek,
            orca_core::model::PRO_MODEL,
            ThemeName::Catppuccin,
        );

        state.set_step_for_test(OnboardingStep::Model);
        assert_eq!(state.option_rows()[2].selected, true);
        state.set_step_for_test(OnboardingStep::Theme);
        assert_eq!(state.option_rows()[4].selected, true);

        state.set_outcomes_for_test(
            SaveOutcome::Saved,
            SaveOutcome::Failed(UserConfigSaveError::WriteFailed),
        );
        assert_eq!(state.auth_outcome(), &SaveOutcome::Saved);
        assert_eq!(
            state.preferences_outcome(),
            &SaveOutcome::Failed(UserConfigSaveError::WriteFailed),
        );
    }

    #[test]
    fn advance_follows_the_seven_step_transition_matrix() {
        let mut state = state();
        assert_eq!(state.step(), OnboardingStep::Welcome);
        assert!(state.advance());
        assert_eq!(state.step(), OnboardingStep::Provider);
        assert!(state.advance());
        assert_eq!(state.step(), OnboardingStep::ApiKey);
        assert!(!state.advance());
        assert_eq!(state.step(), OnboardingStep::ApiKey);
        assert_eq!(state.review_error(), Some(OnboardingError::MissingApiKey),);

        state.set_api_key("sk-staged-only".to_string());
        assert!(state.advance());
        assert_eq!(state.step(), OnboardingStep::Model);
        assert!(state.advance());
        assert_eq!(state.step(), OnboardingStep::Theme);
        assert!(state.advance());
        assert_eq!(state.step(), OnboardingStep::Review);
        assert!(!state.advance());
        assert_eq!(state.step(), OnboardingStep::Review);

        assert!(state.finish_review(SaveOutcome::Saved, SaveOutcome::Saved));
        assert_eq!(state.step(), OnboardingStep::Complete);
        assert!(!state.advance());
        assert_eq!(state.step(), OnboardingStep::Complete);
    }

    #[test]
    fn whitespace_api_keys_fail_closed_and_valid_keys_are_trimmed() {
        let mut state = state();
        state.set_step_for_test(OnboardingStep::ApiKey);

        state.set_api_key(" \n\t  ".to_string());
        assert_eq!(state.api_key(), None);
        assert_eq!(state.review_error(), Some(OnboardingError::MissingApiKey),);
        assert!(!state.advance());
        assert_eq!(state.step(), OnboardingStep::ApiKey);
        assert_eq!(state.review_error(), Some(OnboardingError::MissingApiKey),);

        state.set_api_key("  sk-trimmed-key \n".to_string());
        assert_eq!(state.api_key(), Some("sk-trimmed-key"));
        assert_eq!(state.review_error(), None);
        assert!(state.advance());
        assert_eq!(state.step(), OnboardingStep::Model);
    }

    #[test]
    fn finish_review_rejects_calls_before_review_without_mutation() {
        let mut state = state();
        state.set_api_key("sk-stays-staged".to_string());

        assert!(!state.finish_review(SaveOutcome::Saved, SaveOutcome::Saved));

        assert_eq!(state.step(), OnboardingStep::Welcome);
        assert_eq!(state.auth_outcome(), &SaveOutcome::NotAttempted);
        assert_eq!(state.preferences_outcome(), &SaveOutcome::NotAttempted);
        assert_eq!(state.api_key(), Some("sk-stays-staged"));
    }

    #[test]
    fn finish_review_rejects_not_attempted_outcomes_without_mutation() {
        let mut state = state();
        state.set_api_key("sk-stays-staged".to_string());
        state.set_step_for_test(OnboardingStep::Review);

        assert!(!state.finish_review(SaveOutcome::NotAttempted, SaveOutcome::Saved));
        assert_eq!(state.step(), OnboardingStep::Review);
        assert_eq!(state.auth_outcome(), &SaveOutcome::NotAttempted);
        assert_eq!(state.preferences_outcome(), &SaveOutcome::NotAttempted);
        assert_eq!(state.api_key(), Some("sk-stays-staged"));

        assert!(!state.finish_review(SaveOutcome::Saved, SaveOutcome::NotAttempted));
        assert_eq!(state.step(), OnboardingStep::Review);
        assert_eq!(state.auth_outcome(), &SaveOutcome::NotAttempted);
        assert_eq!(state.preferences_outcome(), &SaveOutcome::NotAttempted);
        assert_eq!(state.api_key(), Some("sk-stays-staged"));
    }

    #[test]
    fn finish_review_rejects_repeated_completion_without_mutation() {
        let mut state = state();
        state.set_api_key("sk-original".to_string());
        state.set_step_for_test(OnboardingStep::Review);
        assert!(state.finish_review(SaveOutcome::Saved, SaveOutcome::Saved));
        state.set_api_key("sk-must-survive-rejected-repeat".to_string());

        assert!(!state.finish_review(
            SaveOutcome::Failed(UserConfigSaveError::WriteFailed),
            SaveOutcome::Failed(UserConfigSaveError::ReplaceFailed),
        ));

        assert_eq!(state.step(), OnboardingStep::Complete);
        assert_eq!(state.auth_outcome(), &SaveOutcome::Saved);
        assert_eq!(state.preferences_outcome(), &SaveOutcome::Saved);
        assert_eq!(state.api_key(), Some("sk-must-survive-rejected-repeat"));
    }

    #[test]
    fn finish_review_rejects_already_recorded_outcomes_even_if_step_is_review() {
        let mut state = state();
        state.set_api_key("sk-stays-staged".to_string());
        state.set_step_for_test(OnboardingStep::Review);
        state.set_outcomes_for_test(SaveOutcome::Saved, SaveOutcome::Saved);

        assert!(!state.finish_review(
            SaveOutcome::Failed(UserConfigSaveError::WriteFailed),
            SaveOutcome::Failed(UserConfigSaveError::ReplaceFailed),
        ));

        assert_eq!(state.step(), OnboardingStep::Review);
        assert_eq!(state.auth_outcome(), &SaveOutcome::Saved);
        assert_eq!(state.preferences_outcome(), &SaveOutcome::Saved);
        assert_eq!(state.api_key(), Some("sk-stays-staged"));
    }

    #[test]
    fn movement_on_non_option_steps_fails_closed() {
        let mut state = state();
        for step in [
            OnboardingStep::Welcome,
            OnboardingStep::ApiKey,
            OnboardingStep::Review,
            OnboardingStep::Complete,
        ] {
            state.set_step_for_test(step);
            assert_eq!(state.move_previous(), None);
            assert_eq!(state.move_next(), None);
            assert_eq!(state.step(), step);
            assert_eq!(
                state.error_label(),
                Some(OnboardingError::UnsupportedModel.safe_label()),
            );
        }
    }

    #[test]
    fn review_validation_accepts_only_review_with_valid_draft_and_key() {
        let mut wrong_step = state();
        wrong_step.set_api_key("sk-stays-staged".to_string());
        assert!(matches!(
            wrong_step.validate_review(),
            Err(OnboardingError::InvalidStep),
        ));
        assert_eq!(wrong_step.step(), OnboardingStep::Welcome);
        assert_eq!(wrong_step.auth_outcome(), &SaveOutcome::NotAttempted);
        assert_eq!(wrong_step.preferences_outcome(), &SaveOutcome::NotAttempted,);
        assert_eq!(wrong_step.api_key(), Some("sk-stays-staged"));
        assert_eq!(
            wrong_step.review_error(),
            Some(OnboardingError::InvalidStep)
        );

        let mut unsupported_provider = state();
        unsupported_provider.set_api_key("sk-stays-staged".to_string());
        unsupported_provider.set_step_for_test(OnboardingStep::Review);
        unsupported_provider.draft.provider = ProviderKind::Mock;
        assert!(matches!(
            unsupported_provider.validate_review(),
            Err(OnboardingError::UnsupportedProvider),
        ));
        assert_eq!(unsupported_provider.step(), OnboardingStep::Review);
        assert_eq!(
            unsupported_provider.auth_outcome(),
            &SaveOutcome::NotAttempted,
        );
        assert_eq!(
            unsupported_provider.preferences_outcome(),
            &SaveOutcome::NotAttempted,
        );
        assert_eq!(unsupported_provider.api_key(), Some("sk-stays-staged"));

        let mut unsupported_model = state();
        unsupported_model.set_api_key("sk-stays-staged".to_string());
        unsupported_model.set_step_for_test(OnboardingStep::Review);
        unsupported_model.draft.model = "invalid".to_string();
        assert!(matches!(
            unsupported_model.validate_review(),
            Err(OnboardingError::UnsupportedModel),
        ));
        assert_eq!(unsupported_model.step(), OnboardingStep::Review);
        assert_eq!(unsupported_model.api_key(), Some("sk-stays-staged"));

        let mut missing_key = state();
        missing_key.set_step_for_test(OnboardingStep::Review);
        missing_key.draft.api_key = Some(" \t ".to_string());
        assert!(matches!(
            missing_key.validate_review(),
            Err(OnboardingError::MissingApiKey),
        ));
        assert_eq!(missing_key.step(), OnboardingStep::Review);
        assert_eq!(missing_key.api_key(), Some(" \t "));
        assert_eq!(missing_key.auth_outcome(), &SaveOutcome::NotAttempted);
        assert_eq!(
            missing_key.preferences_outcome(),
            &SaveOutcome::NotAttempted,
        );

        let mut valid = state();
        valid.set_api_key("sk-valid".to_string());
        valid.set_step_for_test(OnboardingStep::Review);
        assert!(valid.validate_review().is_ok());
        assert_eq!(valid.review_error(), None);
        assert_eq!(valid.step(), OnboardingStep::Review);
        assert_eq!(valid.api_key(), Some("sk-valid"));
        assert_eq!(valid.auth_outcome(), &SaveOutcome::NotAttempted);
        assert_eq!(valid.preferences_outcome(), &SaveOutcome::NotAttempted);
    }

    #[test]
    fn persistence_starts_not_attempted_and_errors_are_safe() {
        let state = state();
        assert_eq!(state.auth_outcome(), &SaveOutcome::NotAttempted);
        assert_eq!(state.preferences_outcome(), &SaveOutcome::NotAttempted);

        for error in USER_CONFIG_SAVE_ERRORS {
            assert_user_config_save_error_is_safe(error);
        }

        assert_eq!(
            [
                OnboardingError::MissingApiKey.safe_label(),
                OnboardingError::InvalidStep.safe_label(),
                OnboardingError::UnsupportedProvider.safe_label(),
                OnboardingError::UnsupportedModel.safe_label(),
                OnboardingError::SharedConfigUnavailable.safe_label(),
            ],
            [
                "API key is required",
                "review is unavailable",
                "unsupported provider selection",
                "unsupported model selection",
                "shared configuration unavailable",
            ],
        );
        assert_eq!(SaveOutcome::NotAttempted.safe_label(), "save not attempted");
        assert_eq!(SaveOutcome::Saved.safe_label(), "saved");
        for error in USER_CONFIG_SAVE_ERRORS {
            assert_eq!(SaveOutcome::Failed(error).safe_label(), error.safe_label());
        }
    }

    #[test]
    fn completion_rows_use_only_safe_labels() {
        for error in USER_CONFIG_SAVE_ERRORS {
            let mut state = state();
            state.set_outcomes_for_test(SaveOutcome::Failed(error), SaveOutcome::Failed(error));
            let rows = state.completion_rows();
            assert!(rows.iter().all(|row| row.ends_with(error.safe_label())));
            assert!(rows.iter().all(|row| !row.contains('/')));
            assert!(rows.iter().all(|row| !row.contains("sk-")));
        }
    }

    #[test]
    fn finish_clears_secret_and_records_independent_outcomes() {
        let mut state = state();
        state.set_api_key("sk-clear-after-review".to_string());
        state.set_step_for_test(OnboardingStep::Review);
        state.set_error(OnboardingError::MissingApiKey);

        assert!(state.finish_review(
            SaveOutcome::Saved,
            SaveOutcome::Failed(UserConfigSaveError::ConcurrentModification),
        ));

        assert_eq!(state.step(), OnboardingStep::Complete);
        assert_eq!(state.api_key(), None);
        assert_eq!(state.take_api_key(), None);
        assert_eq!(state.auth_outcome(), &SaveOutcome::Saved);
        assert_eq!(
            state.preferences_outcome(),
            &SaveOutcome::Failed(UserConfigSaveError::ConcurrentModification),
        );
        assert_eq!(state.review_error(), None);
    }
}
