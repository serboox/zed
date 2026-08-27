use std::fmt::{self, Display, Formatter};

use gpui::{App, BoxShadow, Hsla, Pixels, hsla, px};
use theme::{ActiveTheme, Appearance};

/// Today, elevation is primarily used to add shadows to elements, and set the correct background for elements like buttons.
///
/// Elevation can be thought of as the physical closeness of an element to the
/// user. Elements with lower elevations are physically further away on the
/// z-axis and appear to be underneath elements with higher elevations.
///
/// In the future, a more complete approach to elevation may be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElevationIndex {
    /// On the layer of the app background. This is under panels, panes, and
    /// other surfaces.
    Background,
    /// The primary surface – Contains panels, panes, containers, etc.
    Surface,
    /// The same elevation as the primary surface, but used for the editable areas, like buffers
    EditorSurface,
    /// A surface that is elevated above the primary surface. but below washes, models, and dragged elements.
    ElevatedSurface,
    /// A surface above the [ElevationIndex::ElevatedSurface] that is used for dialogs, alerts, modals, etc.
    ModalSurface,
}

impl Display for ElevationIndex {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            ElevationIndex::Background => write!(f, "Background"),
            ElevationIndex::Surface => write!(f, "Surface"),
            ElevationIndex::EditorSurface => write!(f, "Editor Surface"),
            ElevationIndex::ElevatedSurface => write!(f, "Elevated Surface"),
            ElevationIndex::ModalSurface => write!(f, "Modal Surface"),
        }
    }
}

impl ElevationIndex {
    /// The corner radius a surface at this elevation announces about itself.
    ///
    /// Docked chrome -- the title bar, panels, the tab bar, the editor -- keeps
    /// square corners, which is deliberate in this fork. Only what floats over
    /// the content is rounded, and the higher it floats the larger the radius,
    /// so the radius alone says how far above the content a surface sits.
    pub fn radius(self) -> Pixels {
        match self {
            ElevationIndex::Background
            | ElevationIndex::Surface
            | ElevationIndex::EditorSurface => px(0.),
            ElevationIndex::ElevatedSurface => px(8.),
            ElevationIndex::ModalSurface => px(12.),
        }
    }

    /// Returns an appropriate shadow for the given elevation index.
    ///
    /// A ladder of two, three and four layers with growing blur. The tight
    /// layer draws the contact edge, the wide one carries the sense of height;
    /// without the wide layer a menu does not separate from the buffer behind it
    /// and the whole window reads as one flat sheet.
    pub fn shadow(self, cx: &App) -> Vec<BoxShadow> {
        let is_light = cx.theme().appearance() == Appearance::Light;

        match self {
            ElevationIndex::Surface => vec![],
            ElevationIndex::EditorSurface => vec![],

            ElevationIndex::ElevatedSurface => vec![
                BoxShadow::new(
                    px(0.),
                    px(1.),
                    hsla(0., 0., 0., if is_light { 0.06 } else { 0.16 }),
                )
                .blur_radius(px(2.)),
                BoxShadow::new(
                    px(0.),
                    px(4.),
                    hsla(0., 0., 0., if is_light { 0.08 } else { 0.24 }),
                )
                .blur_radius(px(10.)),
                BoxShadow::new(
                    px(0.),
                    px(10.),
                    hsla(0., 0., 0., if is_light { 0.06 } else { 0.28 }),
                )
                .blur_radius(px(24.)),
            ],

            ElevationIndex::ModalSurface => vec![
                BoxShadow::new(
                    px(0.),
                    px(1.),
                    hsla(0., 0., 0., if is_light { 0.06 } else { 0.18 }),
                )
                .blur_radius(px(2.)),
                BoxShadow::new(
                    px(0.),
                    px(4.),
                    hsla(0., 0., 0., if is_light { 0.08 } else { 0.22 }),
                )
                .blur_radius(px(8.)),
                BoxShadow::new(
                    px(0.),
                    px(12.),
                    hsla(0., 0., 0., if is_light { 0.10 } else { 0.32 }),
                )
                .blur_radius(px(28.)),
                BoxShadow::new(
                    px(0.),
                    px(28.),
                    hsla(0., 0., 0., if is_light { 0.12 } else { 0.44 }),
                )
                .blur_radius(px(64.)),
            ],

            _ => vec![],
        }
    }

    /// Returns the background color for the given elevation index.
    pub fn bg(&self, cx: &mut App) -> Hsla {
        match self {
            ElevationIndex::Background => cx.theme().colors().background,
            ElevationIndex::Surface => cx.theme().colors().surface_background,
            ElevationIndex::EditorSurface => cx.theme().colors().editor_background,
            ElevationIndex::ElevatedSurface => cx.theme().colors().elevated_surface_background,
            ElevationIndex::ModalSurface => cx.theme().colors().elevated_surface_background,
        }
    }

    /// Returns a color that is appropriate a filled element on this elevation
    pub fn on_elevation_bg(&self, cx: &App) -> Hsla {
        match self {
            ElevationIndex::Background => cx.theme().colors().surface_background,
            ElevationIndex::Surface => cx.theme().colors().background,
            ElevationIndex::EditorSurface => cx.theme().colors().surface_background,
            ElevationIndex::ElevatedSurface => cx.theme().colors().background,
            ElevationIndex::ModalSurface => cx.theme().colors().background,
        }
    }

    /// Attempts to return a darker background color than the current elevation index's background.
    ///
    /// If the current background color is already dark, it will return a lighter color instead.
    pub fn darker_bg(&self, cx: &App) -> Hsla {
        match self {
            ElevationIndex::Background => cx.theme().colors().surface_background,
            ElevationIndex::Surface => cx.theme().colors().editor_background,
            ElevationIndex::EditorSurface => cx.theme().colors().surface_background,
            ElevationIndex::ElevatedSurface => cx.theme().colors().editor_background,
            ElevationIndex::ModalSurface => cx.theme().colors().editor_background,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ElevationIndex;
    use gpui::{TestAppContext, px};

    // The fork keeps docked chrome square on purpose; only what floats over the
    // content is rounded, and the radius grows with height so the corner alone
    // says how far above the buffer a surface sits.
    #[test]
    fn docked_chrome_stays_square_and_floating_surfaces_do_not() {
        assert_eq!(ElevationIndex::Background.radius(), px(0.));
        assert_eq!(ElevationIndex::Surface.radius(), px(0.));
        assert_eq!(ElevationIndex::EditorSurface.radius(), px(0.));

        let menu = ElevationIndex::ElevatedSurface.radius();
        let modal = ElevationIndex::ModalSurface.radius();
        assert!(
            menu > px(0.),
            "a menu or popover floats over the buffer and must not share the square corner of a panel"
        );
        assert!(
            modal > menu,
            "a modal is further from the content than a menu, so its radius is larger: {menu:?} vs {modal:?}"
        );
    }

    // Depth has to be carried by the shadow. When the layers are few and their
    // blur is tight, the surfaces differ almost only by background colour and
    // the whole window reads as one flat sheet.
    #[gpui::test]
    async fn the_shadow_ladder_grows_with_elevation(cx: &mut TestAppContext) {
        let (docked, menu, modal) = cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            (
                ElevationIndex::Surface.shadow(cx),
                ElevationIndex::ElevatedSurface.shadow(cx),
                ElevationIndex::ModalSurface.shadow(cx),
            )
        });

        assert!(
            docked.is_empty(),
            "docked chrome is not floating and casts no shadow"
        );
        assert!(
            menu.len() >= 3 && modal.len() >= menu.len(),
            "the ladder is 3 layers for a menu and at least as many for a modal: {} and {}",
            menu.len(),
            modal.len()
        );

        let widest = |layers: &[gpui::BoxShadow]| {
            layers
                .iter()
                .map(|layer| layer.blur_radius)
                .fold(px(0.), |a: gpui::Pixels, b| if b > a { b } else { a })
        };
        assert!(
            widest(&menu) >= px(20.),
            "a menu needs one wide layer to lift off the buffer, widest is {:?}",
            widest(&menu)
        );
        assert!(
            widest(&modal) > widest(&menu),
            "a modal sits higher than a menu, so its widest layer is wider: {:?} vs {:?}",
            widest(&modal),
            widest(&menu)
        );
    }
}
