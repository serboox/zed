use settings::{RegisterSetting, Settings};

/// What a page drawn in the preview costs is mostly what its pixels cost, and a
/// display with two of them to the editor's one asks for four times as many.
#[derive(Clone, Copy, Debug, RegisterSetting)]
pub struct HtmlPreviewSettings {
    /// How many device pixels the page is drawn with for each editor pixel.
    /// `None` means the display's own, which is the sharpest and the dearest.
    pub render_scale: Option<f32>,
}

/// A page drawn smaller than a tenth of the display, or larger than four times
/// it, is a mistake rather than a preference.
const SENSIBLE: std::ops::RangeInclusive<f32> = 0.1..=4.0;

impl HtmlPreviewSettings {
    /// The scale to draw a page at in a window whose own is `display`.
    pub fn scale_in(&self, display: f32) -> f32 {
        match self.render_scale {
            Some(asked) if SENSIBLE.contains(&asked) => asked,
            Some(asked) => {
                log::warn!("a page cannot be drawn at {asked} pixels to one; using {display}");
                display
            }
            None => display,
        }
    }
}

impl Settings for HtmlPreviewSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            render_scale: content
                .html_preview
                .as_ref()
                .and_then(|preview| preview.render_scale),
        }
    }
}
