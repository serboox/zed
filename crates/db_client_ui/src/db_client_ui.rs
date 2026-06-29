mod compare_data;
mod connection_view;
mod driver_icon;
mod modify_table;
mod panel;
mod result_view;
mod sql_completion_provider;
mod store;

pub use panel::{
    DatabasePanel, explain_current_sql_query, new_query_for_active_connection, open_new_sql_query,
    run_current_sql_query,
};

use gpui::App;

pub fn init(cx: &mut App) {
    panel::init(cx);
}
