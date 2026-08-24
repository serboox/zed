use settings::{RegisterSetting, Settings};

use crate::configurations_store::MOST_TEMPORARIES_KEPT;

/// How many ways run on the spot are worth keeping, as the reader has chosen
/// it in their own settings.
#[derive(Clone, Debug, RegisterSetting)]
pub struct RunConfigurationsSettings {
    /// How many of the ways run on the spot are kept before the oldest is
    /// dropped. Zero keeps none of them at all.
    pub most_temporaries_kept: usize,
    /// Whether the strip of readings for a running configuration is shown at
    /// all. Off means it is neither drawn nor measured.
    pub show_process_metrics: bool,
}

impl Settings for RunConfigurationsSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            most_temporaries_kept: content
                .run_configurations
                .as_ref()
                .and_then(|configured| configured.most_temporaries_kept)
                .unwrap_or(MOST_TEMPORARIES_KEPT),
            show_process_metrics: content
                .run_configurations
                .as_ref()
                .and_then(|configured| configured.show_process_metrics)
                .unwrap_or(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_unset_it_is_the_same_five_as_before() {
        let content = settings::SettingsContent::default();
        assert_eq!(
            RunConfigurationsSettings::from_settings(&content).most_temporaries_kept,
            MOST_TEMPORARIES_KEPT
        );
    }

    #[test]
    fn the_reader_may_choose_a_different_number() {
        let mut content = settings::SettingsContent::default();
        content.run_configurations = Some(settings::RunConfigurationsSettingsContent {
            most_temporaries_kept: Some(9),
            ..Default::default()
        });
        assert_eq!(
            RunConfigurationsSettings::from_settings(&content).most_temporaries_kept,
            9
        );
    }
}
