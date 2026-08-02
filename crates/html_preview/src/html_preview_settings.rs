use settings::{RegisterSetting, Settings};

/// What a page drawn in the preview costs is mostly what its pixels cost, and a
/// display with two of them to the editor's one asks for four times as many.
#[derive(Clone, Debug, RegisterSetting)]
pub struct HtmlPreviewSettings {
    /// How many device pixels the page is drawn with for each editor pixel.
    /// `None` means the display's own, which is the sharpest and the dearest.
    pub render_scale: Option<f32>,
    /// Where words typed into the address bar are sent when they are not an
    /// address. `{query}` is where they go.
    pub search_engine: std::sync::Arc<str>,
}

/// A page drawn smaller than a tenth of the display, or larger than four times
/// it, is a mistake rather than a preference.
const SENSIBLE: std::ops::RangeInclusive<f32> = 0.1..=4.0;

/// Where a search goes when the reader has not said otherwise.
/// Google reads `hl` as the language to answer in and `gl` as the country to
/// answer for; without them it guesses both from where the request came from.
const SEARCH: &str = "https://www.google.com/search?q={query}&hl=en&gl=us";

impl HtmlPreviewSettings {
    /// Where to send what the reader typed: the address itself if it is one,
    /// and a search for it if it is not.
    pub fn where_to_go(&self, typed: &str) -> Option<url::Url> {
        let typed = typed.trim();
        if typed.is_empty() {
            return None;
        }
        if let Ok(address) = url::Url::parse(typed)
            && !address.cannot_be_a_base()
        {
            return Some(address);
        }
        // A single word with a dot in it and nothing that looks like a sentence
        // is an address someone did not bother to spell out.
        let looks_like_a_host = !typed.contains(char::is_whitespace)
            && typed.contains('.')
            && !typed.starts_with('.')
            && !typed.ends_with('.');
        if looks_like_a_host && let Ok(address) = url::Url::parse(&format!("https://{typed}")) {
            return Some(address);
        }
        let asked: String = url::form_urlencoded::byte_serialize(typed.as_bytes()).collect();
        url::Url::parse(&self.search_engine.replace("{query}", &asked)).ok()
    }

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
            search_engine: content
                .html_preview
                .as_ref()
                .and_then(|preview| preview.search_engine.as_deref())
                .filter(|engine| engine.contains("{query}"))
                .unwrap_or(SEARCH)
                .into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(engine: &str) -> HtmlPreviewSettings {
        HtmlPreviewSettings {
            render_scale: None,
            search_engine: engine.into(),
        }
    }

    #[test]
    fn an_address_is_taken_as_one() {
        let settings = settings(SEARCH);
        assert_eq!(
            settings
                .where_to_go("https://example.com/page")
                .map(|url| url.to_string()),
            Some("https://example.com/page".to_string())
        );
        // A host nobody bothered to spell out is still a host.
        assert_eq!(
            settings
                .where_to_go("example.com")
                .map(|url| url.to_string()),
            Some("https://example.com/".to_string())
        );
    }

    #[test]
    fn anything_else_is_searched_for() {
        let settings = settings(SEARCH);
        assert_eq!(
            settings
                .where_to_go("how tall is a giraffe")
                .map(|url| url.to_string()),
            Some("https://www.google.com/search?q=how+tall+is+a+giraffe&hl=en&gl=us".to_string())
        );
        // A word with no dot is a search, not a host.
        assert!(
            settings
                .where_to_go("giraffe")
                .is_some_and(|url| url.query().is_some())
        );
    }

    #[test]
    fn the_reader_may_choose_where_searches_go() {
        let settings = settings("https://duckduckgo.com/?q={query}");
        assert_eq!(
            settings.where_to_go("giraffe").map(|url| url.to_string()),
            Some("https://duckduckgo.com/?q=giraffe".to_string())
        );
    }

    #[test]
    fn nothing_typed_goes_nowhere() {
        assert!(settings(SEARCH).where_to_go("   ").is_none());
    }
}
