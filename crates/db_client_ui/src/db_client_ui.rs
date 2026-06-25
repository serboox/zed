mod connection_modal;
mod panel;
mod result_view;
mod sql_completion_provider;
mod store;

pub use panel::DatabasePanel;

use gpui::App;

pub fn init(cx: &mut App) {
    panel::init(cx);
}
