use gpui::{App, ClickEvent, Div, ElementId, SharedString, Window, div, prelude::*};
use ui::prelude::*;
use ui::{IconButton, IconName, Label, LabelSize, Tooltip};

/// Standard floating surface for the database client's popups and dialogs, so
/// every overlay shares one source for background, border, radius and shadow.
/// Callers add their own sizing, padding, positioning and `debug_selector`.
pub(crate) fn popup_surface(cx: &App) -> Div {
    div().elevation_2(cx).occlude()
}

/// Title row shared by the database client's centered dialogs: a large title on
/// the left and a close button with a tooltip on the right.
pub(crate) fn dialog_header(
    title: impl Into<SharedString>,
    close_id: impl Into<ElementId>,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .justify_between()
        .items_center()
        .child(Label::new(title).size(LabelSize::Large))
        .child(
            IconButton::new(close_id, IconName::Close)
                .tooltip(Tooltip::text("Close"))
                .on_click(on_close),
        )
}
