use std::rc::Rc;

use gpui::{Anchor, AnyElement, App, Window};

use crate::{
    ButtonSize, ContextMenu, IconButton, IconButtonShape, IconName, IconPosition, IconSize,
    PopoverMenu, Tooltip, prelude::*,
};

/// When a preview redraws from the file it shows.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshMode {
    /// Redraw on every change: typing, an edit made elsewhere, or a reload from disk
    #[default]
    Auto,
    /// Redraw only when the refresh button is pressed
    Manual,
    /// Keep what is shown; the refresh button is disabled
    Off,
}

impl RefreshMode {
    pub const ALL: [RefreshMode; 3] = [RefreshMode::Auto, RefreshMode::Manual, RefreshMode::Off];

    pub fn label(self) -> &'static str {
        match self {
            RefreshMode::Auto => "Refresh Automatically",
            RefreshMode::Manual => "Refresh Manually",
            RefreshMode::Off => "Don't Refresh",
        }
    }
}

/// The refresh-now button and the menu choosing a [`RefreshMode`], meant to sit
/// in a preview's framed group of actions.
pub fn refresh_mode_controls(
    mode: RefreshMode,
    on_refresh: impl Fn(&mut Window, &mut App) + 'static,
    on_mode_change: impl Fn(RefreshMode, &mut Window, &mut App) + 'static,
) -> [AnyElement; 2] {
    let tooltip = match mode {
        RefreshMode::Auto => "Refresh now (the preview also follows every change)",
        RefreshMode::Manual => "Refresh now (changes are not picked up on their own)",
        RefreshMode::Off => "Refreshing is off; choose a mode in the menu next to this",
    };
    let refresh = IconButton::new("refresh-now", IconName::RotateCw)
        .shape(IconButtonShape::Square)
        .icon_size(IconSize::Small)
        .size(ButtonSize::Compact)
        .when(mode == RefreshMode::Auto, |button| {
            button.icon_color(Color::Accent)
        })
        .disabled(mode == RefreshMode::Off)
        .tooltip(Tooltip::text(tooltip))
        .on_click(move |_, window, cx| on_refresh(window, cx));

    let on_mode_change = Rc::new(on_mode_change);
    let mode_menu = PopoverMenu::new("refresh-mode-menu")
        .trigger_with_tooltip(
            IconButton::new("refresh-mode", IconName::ChevronDown)
                .icon_size(IconSize::XSmall)
                .size(ButtonSize::Compact),
            Tooltip::text("Refresh Mode"),
        )
        .anchor(Anchor::TopRight)
        .menu(move |window, cx| {
            let on_mode_change = on_mode_change.clone();
            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                RefreshMode::ALL.into_iter().fold(menu, |menu, entry_mode| {
                    let on_mode_change = on_mode_change.clone();
                    menu.toggleable_entry(
                        entry_mode.label(),
                        entry_mode == mode,
                        IconPosition::Start,
                        None,
                        move |window, cx| on_mode_change(entry_mode, window, cx),
                    )
                })
            }))
        });

    [refresh.into_any_element(), mode_menu.into_any_element()]
}
