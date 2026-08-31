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

/// The close button's rank: the way out of the dialog, same as a Cancel
/// button elsewhere. Kept as its own function so the choice is covered by a
/// test independent of rendering a whole dialog.
pub(crate) fn dialog_close_button_style() -> ButtonStyle {
    cyberpunk::Rank::Neutral.style()
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
                .style(dialog_close_button_style())
                .tooltip(Tooltip::text("Close"))
                .on_click(on_close),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    // The dialog close button appears in every dialog built on `dialog_header`
    // (a dozen or more across this crate). If this ever regresses back to the
    // unframed default, every one of them silently loses its border at once --
    // this test exists to make that regression loud instead of silent.
    #[test]
    fn dialog_close_button_is_not_the_unframed_default() {
        let style = dialog_close_button_style();
        assert_ne!(style, ButtonStyle::Subtle);
        assert_ne!(style, ButtonStyle::Filled);
    }
}
