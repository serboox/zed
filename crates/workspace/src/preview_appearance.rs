use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Global, TaskExt as _};
use theme::Appearance;
use util::ResultExt as _;

const STORE_KEY: &str = "preview-appearance";

/// Which palette a document preview reads in, independent of the editor's own
/// theme: a page of prose or a rendered contract is often easier on light while
/// the code around it stays dark.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PreviewAppearance {
    /// Follow whatever the editor's theme is.
    #[default]
    Match,
    Light,
    Dark,
}

impl PreviewAppearance {
    pub fn next(self) -> Self {
        match self {
            Self::Match => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::Match,
        }
    }

    /// The palette to force, or `None` to leave the editor's own in place.
    pub fn resolve(self) -> Option<Appearance> {
        match self {
            Self::Match => None,
            Self::Light => Some(Appearance::Light),
            Self::Dark => Some(Appearance::Dark),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Match => "Match Editor Theme",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    /// Whether the choice differs from the editor's own theme, which is what a
    /// toggle control shows as "on".
    pub fn overrides_editor(self) -> bool {
        !matches!(self, Self::Match)
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Match => "Reading theme: follow the editor (click for light)",
            Self::Light => "Reading theme: light (click for dark)",
            Self::Dark => "Reading theme: dark (click to follow the editor)",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "match" => Some(Self::Match),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
}

#[derive(Default)]
struct GlobalPreviewAppearance {
    appearance: PreviewAppearance,
    /// Set once the reader has chosen. The stored value arrives from disk a
    /// moment after startup, and it must not undo a choice made in between.
    chosen: bool,
}

impl Global for GlobalPreviewAppearance {}

/// The palette every preview opens in: the one chosen last, whichever document
/// it was chosen on, so the choice does not have to be repeated per file.
pub fn preview_appearance(cx: &App) -> PreviewAppearance {
    cx.try_global::<GlobalPreviewAppearance>()
        .map(|global| global.appearance)
        .unwrap_or_default()
}

pub fn set_preview_appearance(appearance: PreviewAppearance, cx: &mut App) {
    let global = cx.default_global::<GlobalPreviewAppearance>();
    global.appearance = appearance;
    global.chosen = true;
    let store = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        store
            .write_kvp(STORE_KEY.to_string(), appearance.as_str().to_string())
            .await
    })
    .detach_and_log_err(cx);
}

/// Restores the last choice. The read goes through the background so startup is
/// not held up by the database; a preview opened in that first moment follows
/// the editor and picks the choice up on the next one.
pub fn init(cx: &mut App) {
    let store = KeyValueStore::global(cx);
    cx.spawn(async move |cx| {
        let stored = cx
            .background_spawn(async move { store.read_kvp(STORE_KEY) })
            .await
            .log_err()
            .flatten();
        if let Some(appearance) = stored.as_deref().and_then(PreviewAppearance::from_str) {
            cx.update(|cx| {
                let global = cx.default_global::<GlobalPreviewAppearance>();
                if !global.chosen {
                    global.appearance = appearance;
                }
            });
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycling_visits_every_palette_and_wraps() {
        let mut seen = vec![PreviewAppearance::default()];
        for _ in 0..3 {
            seen.push(seen.last().copied().unwrap_or_default().next());
        }
        assert_eq!(
            seen,
            vec![
                PreviewAppearance::Match,
                PreviewAppearance::Light,
                PreviewAppearance::Dark,
                PreviewAppearance::Match,
            ]
        );
    }

    #[test]
    fn only_a_forced_palette_overrides_the_editor() {
        assert_eq!(PreviewAppearance::Match.resolve(), None);
        assert_eq!(
            PreviewAppearance::Light.resolve(),
            Some(Appearance::Light),
            "light has to override, or the choice does nothing"
        );
        assert_eq!(PreviewAppearance::Dark.resolve(), Some(Appearance::Dark));
    }

    #[gpui::test]
    fn a_choice_made_before_the_stored_one_arrives_wins(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            set_preview_appearance(PreviewAppearance::Light, cx);
            // What `init`'s background read does when it finally returns.
            let global = cx.default_global::<GlobalPreviewAppearance>();
            if !global.chosen {
                global.appearance = PreviewAppearance::Dark;
            }
            assert_eq!(
                preview_appearance(cx),
                PreviewAppearance::Light,
                "a reader's choice must not be undone by the value on disk"
            );
        });
    }

    #[test]
    fn what_is_written_can_be_read_back() {
        for appearance in [
            PreviewAppearance::Match,
            PreviewAppearance::Light,
            PreviewAppearance::Dark,
        ] {
            assert_eq!(
                PreviewAppearance::from_str(appearance.as_str()),
                Some(appearance),
                "a stored choice has to survive a restart"
            );
        }
        assert_eq!(PreviewAppearance::from_str("sepia"), None);
    }
}
