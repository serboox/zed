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
    /// Where the engine answers a browser's own developer tools.
    pub devtools_port: Option<u16>,
    /// Somewhere to send the page's requests through.
    pub proxy: Option<std::sync::Arc<str>>,
}

/// What `typed` names on this machine, if it names anything: an absolute path, a
/// path from the reader's own folder, or one from where the editor was started.
///
/// Only a file that is really there comes back. A name that is not on the machine
/// may well be a host, and guessing otherwise would send every mistyped address
/// to a file that does not exist.
fn as_a_file_on_this_machine(typed: &str) -> Option<url::Url> {
    let path = match typed.strip_prefix("~/") {
        Some(rest) => paths::home_dir().join(rest),
        None => std::path::PathBuf::from(typed),
    };
    let path = match path.is_absolute() {
        true => path,
        false => std::env::current_dir().ok()?.join(path),
    };
    path.exists()
        .then(|| url::Url::from_file_path(&path).ok())
        .flatten()
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
        // Something on this machine, before anything is asked of the network. A
        // path holds dots like a host does, so read as a host it becomes a site
        // called `home` that nobody can reach and every attempt waits out the
        // connection timeout before saying so.
        if let Some(file) = as_a_file_on_this_machine(typed) {
            return Some(file);
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
    ///
    /// The display's own, unless the reader asks for something else. A page drawn
    /// at less than the display can show is a page that has to be read through
    /// the blur, and no frame rate is worth that: what a frame costs turned out
    /// to be the editor asking the page where it stood on every turn of the
    /// engine, not the pixels -- see `WHILE_THE_PAGE_MOVES`. Drawing coarser
    /// bought a third of a frame and cost the reading.
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
            devtools_port: content
                .html_preview
                .as_ref()
                .and_then(|preview| preview.devtools_port),
            proxy: content
                .html_preview
                .as_ref()
                .and_then(|preview| preview.proxy.as_deref())
                .filter(|proxy| !proxy.trim().is_empty())
                .map(Into::into),
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
            devtools_port: None,
            proxy: None,
        }
    }

    /// A page is drawn at everything the display can show. Drawn coarser it has
    /// to be read through the blur, and that was tried: it bought a third of a
    /// frame and cost the reading, while what a frame actually cost was the
    /// editor asking the page where it stood on every turn of the engine.
    #[test]
    fn a_page_is_drawn_at_everything_the_display_can_show() {
        let left_unsaid = settings(SEARCH);
        assert_eq!(left_unsaid.scale_in(1.), 1.);
        assert_eq!(
            left_unsaid.scale_in(2.),
            2.,
            "a fine display is drawn at its own, however dear that is"
        );
        assert_eq!(left_unsaid.scale_in(2.5), 2.5);

        // Whatever the reader says goes, in either direction.
        let mut asked = settings(SEARCH);
        asked.render_scale = Some(1.5);
        assert_eq!(asked.scale_in(2.), 1.5);
        asked.render_scale = Some(0.5);
        assert_eq!(asked.scale_in(2.), 0.5);

        // A number that is no scale at all is not one to draw by.
        asked.render_scale = Some(40.);
        assert_eq!(asked.scale_in(2.), 2.);
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

    /// A path on this machine is a file, not a website. Read as a host, one that
    /// starts at `/home` becomes a site called `home`, and every attempt to reach
    /// it waits out the connection timeout before saying so.
    #[test]
    fn a_file_on_this_machine_is_opened_rather_than_asked_of_the_network() {
        let settings = settings(SEARCH);
        let folder = tempfile::tempdir().expect("somewhere to put a file");
        let page = folder.path().join("bonds-2026-08-20.html");
        std::fs::write(&page, "<p>hello</p>").expect("a page to open");

        let went = settings
            .where_to_go(&page.to_string_lossy())
            .expect("it goes somewhere");
        assert_eq!(went.scheme(), "file", "it is a file: {went}");
        assert_eq!(
            went.to_file_path().ok().as_deref(),
            Some(page.as_path()),
            "and it is that file"
        );

        // Spelled out, it is the same file.
        let spelled = format!("file://{}", page.display());
        assert_eq!(
            settings
                .where_to_go(&spelled)
                .and_then(|went| went.to_file_path().ok()),
            Some(page),
        );
    }

    /// A path that names nothing may well be a host, and a host is what it is
    /// taken for.
    #[test]
    fn a_path_to_nothing_is_still_read_as_a_host() {
        let settings = settings(SEARCH);
        assert_eq!(
            settings
                .where_to_go("example.com/nothing/here.html")
                .map(|url| url.to_string()),
            Some("https://example.com/nothing/here.html".to_string()),
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
