use gpui::{Anchor, Entity};
use ui::{
    ButtonSize, ContextMenu, ContextMenuEntry, DocumentationSide, IconButton, IconButtonShape,
    IconName, IconPosition, IconSize, Label, PopoverMenu, Tooltip, prelude::*,
};

use crate::{
    TabularDataPreviewPane,
    settings::{FilterSortOrder, RefreshMode, TabularDataPreviewSettings, VerticalAlignment},
};

///// Settings related /////

/// Adds a toggleable entry that applies `set` to the pane's settings when clicked. `description`,
/// when given, shows as a documentation aside explaining what the entry does.
fn toggle_entry(
    menu: ContextMenu,
    label: &'static str,
    description: Option<&'static str>,
    selected: bool,
    view_entity: &Entity<TabularDataPreviewPane>,
    set: impl Fn(&mut TabularDataPreviewSettings) + 'static,
) -> ContextMenu {
    let view_entity = view_entity.clone();
    let entry = ContextMenuEntry::new(label)
        .toggleable(IconPosition::Start, selected)
        .handler(move |_, cx| {
            view_entity.update(cx, |this, cx| {
                set(&mut this.settings);
                cx.notify();
            });
        });
    let entry = if let Some(description) = description {
        entry.documentation_aside(DocumentationSide::Right, move |_| {
            Label::new(description).into_any_element()
        })
    } else {
        entry
    };
    menu.item(entry)
}

/// The refresh-now button and the menu choosing when the table follows the file.
pub(crate) fn refresh_controls(
    view_entity: Entity<TabularDataPreviewPane>,
    mode: RefreshMode,
) -> [AnyElement; 2] {
    let tooltip = match mode {
        RefreshMode::Auto => "Refresh now (the table also follows every change)",
        RefreshMode::Manual => "Refresh now (changes are not picked up on their own)",
        RefreshMode::Off => "Refreshing is off; choose a mode in the menu next to this",
    };
    let refresh = IconButton::new("table-refresh", IconName::RotateCw)
        .shape(IconButtonShape::Square)
        .icon_size(IconSize::Small)
        .size(ButtonSize::Compact)
        .when(mode == RefreshMode::Auto, |button| {
            button.icon_color(Color::Accent)
        })
        .disabled(mode == RefreshMode::Off)
        .tooltip(Tooltip::text(tooltip))
        .on_click({
            let view_entity = view_entity.clone();
            move |_, _, cx| view_entity.update(cx, |this, cx| this.refresh(cx))
        });

    let mode_menu = PopoverMenu::new("table-refresh-mode-menu")
        .trigger_with_tooltip(
            IconButton::new("table-refresh-mode", IconName::ChevronDown)
                .icon_size(IconSize::XSmall)
                .size(ButtonSize::Compact),
            Tooltip::text("Refresh Mode"),
        )
        .anchor(Anchor::TopRight)
        .menu(move |window, cx| {
            let view_entity = view_entity.clone();
            let current = view_entity.read(cx).settings.refresh_mode;
            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                RefreshMode::ALL.into_iter().fold(menu, |menu, mode| {
                    let view_entity = view_entity.clone();
                    menu.toggleable_entry(
                        mode.label(),
                        mode == current,
                        IconPosition::Start,
                        None,
                        move |_, cx| {
                            view_entity.update(cx, |this, cx| this.set_refresh_mode(mode, cx))
                        },
                    )
                })
            }))
        });

    [refresh.into_any_element(), mode_menu.into_any_element()]
}

pub(crate) fn settings_popover_menu(
    view_entity: Entity<TabularDataPreviewPane>,
) -> PopoverMenu<ContextMenu> {
    PopoverMenu::new("table-settings-menu")
        .trigger_with_tooltip(
            IconButton::new("table-settings-trigger", IconName::Filter)
                .icon_size(IconSize::Small)
                .size(ButtonSize::Compact),
            Tooltip::text("Table Settings"),
        )
        .anchor(Anchor::TopRight)
        .menu(move |window, cx| {
            let view_entity = view_entity.clone();
            Some(ContextMenu::build_persistent(
                window,
                cx,
                move |menu, _window, cx| {
                    let settings = view_entity.read(cx).settings.clone();

                    let menu = toggle_entry(
                        menu.header("Text Alignment"),
                        "Top",
                        Some("Choose vertical text alignment within cells"),
                        matches!(settings.vertical_alignment, VerticalAlignment::Top),
                        &view_entity,
                        |settings| settings.vertical_alignment = VerticalAlignment::Top,
                    );
                    let menu = toggle_entry(
                        menu,
                        "Center",
                        None,
                        matches!(settings.vertical_alignment, VerticalAlignment::Center),
                        &view_entity,
                        |settings| settings.vertical_alignment = VerticalAlignment::Center,
                    );

                    let menu = menu.separator().header("Filter Sort");
                    let menu = toggle_entry(
                        menu,
                        "A-Z, then Count",
                        Some("Choose how filter values are sorted in the filter menu"),
                        settings.filter_sort_order == FilterSortOrder::AlphaThenCount,
                        &view_entity,
                        |settings| settings.filter_sort_order = FilterSortOrder::AlphaThenCount,
                    );
                    let menu = toggle_entry(
                        menu,
                        "Count, then A-Z",
                        None,
                        settings.filter_sort_order == FilterSortOrder::CountThenAlpha,
                        &view_entity,
                        |settings| settings.filter_sort_order = FilterSortOrder::CountThenAlpha,
                    );

                    let menu = toggle_entry(
                        menu.separator(),
                        "Display multiline rows",
                        Some(
                            "When enabled, row height grows to show all content. \
                             When disabled, only the first line is visible — hover a cell to see the rest.",
                        ),
                        settings.multiline_cells_enabled,
                        &view_entity,
                        |settings| settings.multiline_cells_enabled = !settings.multiline_cells_enabled,
                    );

                    #[cfg(feature = "dev-tools")]
                    let menu = append_dev_only_entries(menu, &view_entity, &settings);

                    menu
                },
            ))
        })
}

#[cfg(feature = "dev-tools")]
fn append_dev_only_entries(
    menu: ContextMenu,
    view_entity: &Entity<TabularDataPreviewPane>,
    settings: &TabularDataPreviewSettings,
) -> ContextMenu {
    use crate::settings::RowRenderMechanism;

    let menu = menu.separator().header("Dev-only: Rendering Mode");
    let menu = toggle_entry(
        menu,
        "Variable Height",
        Some(
            "Dev-only section used for debugging purposes.\n\
             Will be removed on public release of the tabular data preview feature",
        ),
        settings.rendering_with == RowRenderMechanism::VariableList,
        view_entity,
        |settings| settings.rendering_with = RowRenderMechanism::VariableList,
    );
    let menu = toggle_entry(
        menu,
        "Uniform Height",
        None,
        settings.rendering_with == RowRenderMechanism::UniformList,
        view_entity,
        |settings| settings.rendering_with = RowRenderMechanism::UniformList,
    );

    let menu = toggle_entry(
        menu.separator(),
        "Show perf metrics",
        None,
        settings.show_perf_metrics_overlay,
        view_entity,
        |settings| settings.show_perf_metrics_overlay = !settings.show_perf_metrics_overlay,
    );
    toggle_entry(
        menu,
        "Show cell positions",
        None,
        settings.show_debug_info,
        view_entity,
        |settings| settings.show_debug_info = !settings.show_debug_info,
    )
}
