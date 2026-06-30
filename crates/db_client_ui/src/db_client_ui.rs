mod compare_data;
mod connection_view;
mod data_import;
mod db_agent_tools;
mod ddl_source;
mod driver_icon;
mod erd_diagram;
mod explain_plan;
mod modify_table;
pub mod native_dump;
mod panel;
mod result_view;
mod sql_completion_provider;
mod store;
mod widgets;

pub use panel::{
    DatabasePanel, explain_current_sql_query, new_query_for_active_connection, open_new_sql_query,
    run_current_sql_query,
};
pub use store::{CliQueryOutput, DatabaseStore, GlobalDatabaseStore};

use gpui::App;

pub fn init(cx: &mut App) {
    panel::init(cx);
    db_agent_tools::init();
}
