use std::sync::Arc;

use gpui::{App, Hsla, SharedString};
use settings::Settings as _;
use theme::{ActiveTheme, Appearance, Theme, ThemeRegistry};
use theme_settings::{ThemeSelection, ThemeSettings, default_theme};
use ui::Color;
use workspace::preview_appearance::PreviewAppearance;

/// Every colour the preview paints with, resolved once per render from
/// whichever theme the reading choice picks. `cx.theme()` is a global and
/// GPUI has no way to swap it for one subtree, so the preview reads colours
/// from this instead of calling `cx.theme()` directly.
#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Hsla,
    pub surface_background: Hsla,
    pub elevated_surface_background: Hsla,
    pub element_background: Hsla,
    pub element_hover: Hsla,
    pub border: Hsla,
    pub border_variant: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    pub info: Hsla,
    pub created: Hsla,
    pub warning: Hsla,
    pub warning_background: Hsla,
    pub error: Hsla,
    pub success: Hsla,
}

impl Palette {
    pub fn from_theme(theme: &Theme) -> Self {
        let colors = theme.colors();
        let status = theme.status();
        Self {
            background: colors.editor_background,
            surface_background: colors.surface_background,
            elevated_surface_background: colors.elevated_surface_background,
            element_background: colors.element_background,
            element_hover: colors.element_hover,
            border: colors.border,
            border_variant: colors.border_variant,
            text: colors.text,
            text_muted: colors.text_muted,
            accent: colors.text_accent,
            info: status.info,
            created: status.created,
            warning: status.warning,
            warning_background: status.warning_background,
            error: status.error,
            success: status.success,
        }
    }

    /// Resolves a semantic `Color` against this palette instead of the global
    /// theme. Every other `Color` variant already carries a fixed value (or is
    /// computed some other theme-independent way) and needs no resolution.
    pub fn resolve(&self, color: Color) -> Color {
        match color {
            Color::Default => Color::Custom(self.text),
            Color::Muted => Color::Custom(self.text_muted),
            Color::Accent => Color::Custom(self.accent),
            Color::Error => Color::Custom(self.error),
            Color::Warning => Color::Custom(self.warning),
            Color::Success => Color::Custom(self.success),
            other => other,
        }
    }
}

/// The theme a render should paint with: the editor's own theme when the
/// reader wants to match it, otherwise the theme configured for the forced
/// appearance -- never a theme of the wrong appearance, and never a panic
/// when nothing resolves.
pub fn resolve_theme(reading_appearance: PreviewAppearance, cx: &App) -> Arc<Theme> {
    let Some(appearance) = reading_appearance.resolve() else {
        return cx.theme().clone();
    };
    let registry = ThemeRegistry::global(cx);
    let selection = &ThemeSettings::get_global(cx).theme;
    let name = resolve_theme_name(selection, appearance, |name| {
        registry.get(name).ok().map(|theme| theme.appearance())
    });
    registry
        .get(name.as_ref())
        .ok()
        .unwrap_or_else(|| cx.theme().clone())
}

/// The theme name to try first for a forced appearance (the user's own
/// light/dark pick, or their single static theme), and Zed's own default for
/// that appearance to fall back to.
fn theme_name_candidates(
    selection: &ThemeSelection,
    appearance: Appearance,
) -> (SharedString, SharedString) {
    let configured = match selection {
        ThemeSelection::Static(name) => SharedString::from(name.0.clone()),
        ThemeSelection::Dynamic { light, dark, .. } => match appearance {
            Appearance::Light => SharedString::from(light.0.clone()),
            Appearance::Dark => SharedString::from(dark.0.clone()),
        },
    };
    (configured, SharedString::from(default_theme(appearance)))
}

/// Resolves which theme name a forced appearance should paint with: the
/// configured candidate when a lookup confirms it actually has that
/// appearance, otherwise Zed's own default. A theme locked to a single static
/// pick, or a stale/renamed theme name, has nothing else to offer.
fn resolve_theme_name(
    selection: &ThemeSelection,
    appearance: Appearance,
    lookup_appearance: impl Fn(&str) -> Option<Appearance>,
) -> SharedString {
    let (configured, default) = theme_name_candidates(selection, appearance);
    if lookup_appearance(configured.as_ref()) == Some(appearance) {
        configured
    } else {
        default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::hsla;
    use theme_settings::{ThemeAppearanceMode, ThemeName};

    fn sample_palette() -> Palette {
        Palette {
            background: hsla(0., 0., 0.1, 1.),
            surface_background: hsla(0., 0., 0.15, 1.),
            elevated_surface_background: hsla(0., 0., 0.2, 1.),
            element_background: hsla(0., 0., 0.25, 1.),
            element_hover: hsla(0., 0., 0.3, 1.),
            border: hsla(0., 0., 0.35, 1.),
            border_variant: hsla(0., 0., 0.4, 1.),
            text: hsla(0., 0., 0.9, 1.),
            text_muted: hsla(0., 0., 0.6, 1.),
            accent: hsla(0.55, 0.8, 0.6, 1.),
            info: hsla(0.6, 0.8, 0.5, 1.),
            created: hsla(0.3, 0.8, 0.5, 1.),
            warning: hsla(0.1, 0.8, 0.5, 1.),
            warning_background: hsla(0.1, 0.8, 0.15, 1.),
            error: hsla(0., 0.8, 0.5, 1.),
            success: hsla(0.3, 0.8, 0.4, 1.),
        }
    }

    #[test]
    fn resolve_maps_every_semantic_colour_to_its_own_palette_field() {
        let palette = sample_palette();

        assert_eq!(palette.resolve(Color::Default), Color::Custom(palette.text));
        assert_eq!(
            palette.resolve(Color::Muted),
            Color::Custom(palette.text_muted)
        );
        assert_eq!(
            palette.resolve(Color::Accent),
            Color::Custom(palette.accent)
        );
        assert_eq!(palette.resolve(Color::Error), Color::Custom(palette.error));
        assert_eq!(
            palette.resolve(Color::Warning),
            Color::Custom(palette.warning)
        );
        assert_eq!(
            palette.resolve(Color::Success),
            Color::Custom(palette.success)
        );
    }

    #[test]
    fn resolve_leaves_every_other_colour_untouched() {
        let palette = sample_palette();
        let fixed = hsla(0.8, 0.5, 0.5, 1.);

        assert_eq!(palette.resolve(Color::Custom(fixed)), Color::Custom(fixed));
        assert_eq!(palette.resolve(Color::Disabled), Color::Disabled);
    }

    fn dynamic(mode: ThemeAppearanceMode, light: &str, dark: &str) -> ThemeSelection {
        ThemeSelection::Dynamic {
            mode,
            light: ThemeName(Arc::from(light)),
            dark: ThemeName(Arc::from(dark)),
        }
    }

    #[test]
    fn a_dynamic_selection_picks_light_or_dark_by_the_requested_appearance_not_its_mode() {
        // The reader forced Dark, even though the editor's own mode is Light --
        // the forced choice must still win over the ambient mode.
        let selection = dynamic(
            ThemeAppearanceMode::Light,
            "Solarized Light",
            "Solarized Dark",
        );
        let lookup = |name: &str| match name {
            "Solarized Light" => Some(Appearance::Light),
            "Solarized Dark" => Some(Appearance::Dark),
            _ => None,
        };
        assert_eq!(
            resolve_theme_name(&selection, Appearance::Dark, lookup),
            SharedString::from("Solarized Dark")
        );
        assert_eq!(
            resolve_theme_name(&selection, Appearance::Light, lookup),
            SharedString::from("Solarized Light")
        );
    }

    #[test]
    fn a_static_selection_is_honoured_only_when_it_matches_the_requested_appearance() {
        let selection = ThemeSelection::Static(ThemeName(Arc::from("One Dark")));
        let lookup = |name: &str| match name {
            "One Dark" => Some(Appearance::Dark),
            _ => None,
        };
        // Asking for Dark gets the static pick back, unmodified.
        assert_eq!(
            resolve_theme_name(&selection, Appearance::Dark, lookup),
            SharedString::from("One Dark")
        );
        // Asking for Light has nothing to honour -- a single static theme has
        // no separate light choice -- so it falls back to Zed's own default.
        assert_eq!(
            resolve_theme_name(&selection, Appearance::Light, lookup),
            SharedString::from(default_theme(Appearance::Light))
        );
    }

    #[test]
    fn a_configured_theme_that_no_longer_exists_falls_back_to_the_default() {
        let selection = dynamic(ThemeAppearanceMode::System, "Deleted Theme", "Also Deleted");
        let lookup = |_: &str| None;
        assert_eq!(
            resolve_theme_name(&selection, Appearance::Light, lookup),
            SharedString::from(default_theme(Appearance::Light))
        );
        assert_eq!(
            resolve_theme_name(&selection, Appearance::Dark, lookup),
            SharedString::from(default_theme(Appearance::Dark))
        );
    }
}
