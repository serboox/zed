use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Context, Global, Subscription, TaskExt as _};
use theme::{ActiveTheme as _, Appearance};
use util::ResultExt as _;

const STORE_KEY: &str = "preview-appearance";

/// Which palette a document preview reads in, independent of the editor's own
/// theme: a page of prose or a rendered contract is often easier on light while
/// the code around it stays dark.
///
/// There are two, and only two. A third that followed the editor looked exactly
/// like one of these whenever the editor happened to agree with it, so pressing
/// the control did nothing at all every third press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewAppearance {
    Light,
    Dark,
}

impl PreviewAppearance {
    pub fn next(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Light,
        }
    }

    /// The palette itself.
    pub fn appearance(self) -> Appearance {
        match self {
            Self::Light => Appearance::Light,
            Self::Dark => Appearance::Dark,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    /// A letter for the control, since there is no icon for a palette.
    pub fn initial(self) -> &'static str {
        match self {
            Self::Light => "L",
            Self::Dark => "D",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Light => "Reading theme: light (click for dark)",
            Self::Dark => "Reading theme: dark (click for light)",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    fn from_editor(appearance: Appearance) -> Self {
        match appearance.is_light() {
            true => Self::Light,
            false => Self::Dark,
        }
    }
}

/// The choice, until one has been made. Nothing chosen means a preview opens in
/// whatever the editor is wearing, which is what a reader expects the first time
/// and never thinks about again.
#[derive(Default)]
struct GlobalPreviewAppearance {
    chosen: Option<PreviewAppearance>,
}

impl Global for GlobalPreviewAppearance {}

/// The palette every preview reads in: the one chosen last, whichever document
/// it was chosen on, so the choice does not have to be repeated per file. Until
/// something is chosen, the editor's own.
pub fn preview_appearance(cx: &App) -> PreviewAppearance {
    cx.try_global::<GlobalPreviewAppearance>()
        .and_then(|global| global.chosen)
        .unwrap_or_else(|| PreviewAppearance::from_editor(cx.theme().appearance()))
}

/// What the reader has actually chosen, if anything. A preview that has a
/// palette of its own -- a configured Markdown theme, say -- uses that until the
/// reader says otherwise, and this is how it knows they have not.
pub fn preview_appearance_choice(cx: &App) -> Option<PreviewAppearance> {
    cx.try_global::<GlobalPreviewAppearance>()
        .and_then(|global| global.chosen)
}

pub fn set_preview_appearance(appearance: PreviewAppearance, cx: &mut App) {
    cx.default_global::<GlobalPreviewAppearance>().chosen = Some(appearance);
    // The control that sets this is painted over a document rather than owned by
    // a view that observes it, so nothing would redraw it: the choice would
    // change and the button would go on showing the old one.
    cx.refresh_windows();
    let store = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        store
            .write_kvp(STORE_KEY.to_string(), appearance.as_str().to_string())
            .await
    })
    .detach_and_log_err(cx);
}

/// Notifies a preview whenever the choice changes, wherever it was changed from:
/// the control on the floating panel and the one inside a preview write to the
/// same place, and a view holding its own copy would otherwise keep showing the
/// palette it was opened with.
pub fn observe_preview_appearance<T: 'static>(cx: &mut Context<T>) -> Subscription {
    cx.observe_global::<GlobalPreviewAppearance>(|_, cx| cx.notify())
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
                if global.chosen.is_none() {
                    global.chosen = Some(appearance);
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
    fn there_are_two_palettes_and_pressing_goes_between_them() {
        assert_eq!(PreviewAppearance::Light.next(), PreviewAppearance::Dark);
        assert_eq!(PreviewAppearance::Dark.next(), PreviewAppearance::Light);
    }

    #[test]
    fn what_is_stored_comes_back() {
        for appearance in [PreviewAppearance::Light, PreviewAppearance::Dark] {
            assert_eq!(
                PreviewAppearance::from_str(appearance.as_str()),
                Some(appearance)
            );
        }
        // What an older editor wrote for the palette that followed the editor
        // means nothing now, and the reader is left following it.
        assert_eq!(PreviewAppearance::from_str("match"), None);
    }
}
