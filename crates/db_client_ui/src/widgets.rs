use editor::Editor;
use gpui::{App, ClickEvent, Div, ElementId, Entity, SharedString, Window, div, prelude::*};
use ui::prelude::*;
use ui::{ElevationIndex, IconButton, IconName, Label, LabelSize, Tooltip, cyberpunk};

/// Standard floating surface for the database client's popups -- a history list,
/// a column picker, a chart, a date picker. These are not dialogs, so they keep
/// the editor's own theme; callers add sizing, padding, positioning and
/// `debug_selector`.
pub(crate) fn popup_surface(cx: &App) -> Div {
    div()
        .rounded_md()
        .bg(cx.theme().colors().elevated_surface_background)
        .border_1()
        .border_color(cx.theme().colors().border)
        .shadow(ElevationIndex::ElevatedSurface.shadow(cx))
        .occlude()
}

/// Surface for the database client's dialogs -- the ones that take over and wait
/// for an answer. Those carry the editor's dialog styling rather than the
/// editor's panel styling.
pub(crate) fn dialog_surface(cx: &App) -> Div {
    div()
        .cyberpunk_surface()
        .shadow(ElevationIndex::ElevatedSurface.shadow(cx))
        .occlude()
}

/// Wraps a single-line editor in the bordered field chrome shared by the
/// database client's dialogs, so every text input reads the same. The caller
/// still sets the width (`flex_1`, `w(...)`) and any `debug_selector`.
pub(crate) fn text_field(editor: &Entity<Editor>, cx: &App) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(cx.theme().colors().editor_background)
        .border_1()
        .border_color(cx.theme().colors().border)
        .child(editor.clone())
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
        .child(
            Label::new(title.into().to_uppercase())
                .size(LabelSize::Large)
                .weight(gpui::FontWeight::EXTRA_BOLD)
                .color(Color::Custom(cyberpunk::text_primary())),
        )
        .child(
            IconButton::new(close_id, IconName::Close)
                .tooltip(Tooltip::text("Close"))
                .on_click(on_close),
        )
}
