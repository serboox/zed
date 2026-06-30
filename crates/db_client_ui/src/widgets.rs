use gpui::{App, Div, div, prelude::*};
use ui::prelude::*;

/// Standard floating surface for the database client's popups and dialogs, so
/// every overlay shares one source for background, border, radius and shadow.
/// Callers add their own sizing, padding, positioning and `debug_selector`.
pub(crate) fn popup_surface(cx: &App) -> Div {
    div().elevation_2(cx).occlude()
}
