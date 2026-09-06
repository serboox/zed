use settings::{RegisterSetting, Settings};

/// Whether prose is read at all, and where, as the reader has chosen it in
/// their own settings.
#[derive(Clone, Debug, PartialEq, Eq, RegisterSetting)]
pub struct ProseDiagnosticsSettings {
    /// Whether prose is read for grammar and spelling at all. Off means neither
    /// Markdown nor comments are looked at.
    pub enabled: bool,
    /// Whether the comments of source files are read as well as Markdown.
    /// Advice about a comment is a matter of taste in a way that advice about a
    /// document is not, so it can be turned off on its own.
    pub check_comments: bool,
}

impl Settings for ProseDiagnosticsSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let configured = content.prose_diagnostics.as_ref();
        Self {
            enabled: configured
                .and_then(|configured| configured.enabled)
                .unwrap_or(true),
            check_comments: configured
                .and_then(|configured| configured.check_comments)
                .unwrap_or(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn left_unset_both_halves_are_on() {
        let content = settings::SettingsContent::default();
        assert_eq!(
            ProseDiagnosticsSettings::from_settings(&content),
            ProseDiagnosticsSettings {
                enabled: true,
                check_comments: true,
            }
        );
    }

    #[test]
    fn the_reader_may_turn_the_whole_thing_off() {
        let mut content = settings::SettingsContent::default();
        content.prose_diagnostics = Some(settings::ProseDiagnosticsSettingsContent {
            enabled: Some(false),
            ..Default::default()
        });
        let settings = ProseDiagnosticsSettings::from_settings(&content);
        assert!(!settings.enabled);
    }

    /// The comments are the half that is a matter of taste, so turning them off
    /// must leave Markdown alone.
    #[test]
    fn the_reader_may_keep_markdown_and_drop_the_comments() {
        let mut content = settings::SettingsContent::default();
        content.prose_diagnostics = Some(settings::ProseDiagnosticsSettingsContent {
            check_comments: Some(false),
            ..Default::default()
        });
        let settings = ProseDiagnosticsSettings::from_settings(&content);
        assert!(settings.enabled);
        assert!(!settings.check_comments);
    }
}
