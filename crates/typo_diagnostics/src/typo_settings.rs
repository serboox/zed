use settings::{RegisterSetting, Settings};

/// Whether misspellings are reported at all, as the reader has chosen it in
/// their own settings.
#[derive(Clone, Debug, RegisterSetting)]
pub struct TypoSettings {
    /// Whether a misspelling in a name or a comment is reported. Off means
    /// nothing is read and nothing is shown.
    pub enabled: bool,
}

impl Settings for TypoSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            enabled: content
                .typo_diagnostics
                .as_ref()
                .and_then(|configured| configured.enabled)
                .unwrap_or(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_unset_misspellings_are_reported() {
        let content = settings::SettingsContent::default();
        assert!(TypoSettings::from_settings(&content).enabled);
    }

    #[test]
    fn the_reader_may_turn_it_off() {
        let mut content = settings::SettingsContent::default();
        content.typo_diagnostics = Some(settings::TypoDiagnosticsSettingsContent {
            enabled: Some(false),
        });
        assert!(!TypoSettings::from_settings(&content).enabled);
    }
}
