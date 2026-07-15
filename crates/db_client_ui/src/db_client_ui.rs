mod aerospike_view;
mod compare_data;
mod connection_view;
mod data_import;
mod db_agent_tools;
mod db_migration;
mod ddl_source;
mod driver_icon;
mod erd_diagram;
mod execute_to_file;
mod explain_plan;
mod full_text_search;
mod go_to_object;
mod inline_results;
mod modify_table;
pub mod native_dump;
mod panel;
mod rename_refactor;
mod result_view;
mod schema_diff;
mod sql_ast;
mod sql_binder;
mod sql_completion_provider;
pub mod sql_exec;
mod sql_highlight;
mod sql_validator;
mod store;
mod table_copy;
mod widgets;

pub use panel::{
    DatabasePanel, execute_current_sql_query_to_file, explain_analyze_current_sql_query,
    explain_current_sql_query, format_current_sql_query, new_query_for_active_connection,
    open_new_sql_query, run_current_sql_query, run_sql_file, save_run_configuration,
    toggle_inline_results,
};
pub use sql_exec::ExecStatusIndicator;
pub use store::{CliQueryOutput, DatabaseStore, GlobalDatabaseStore};

use gpui::App;

pub fn init(cx: &mut App) {
    panel::init(cx);
    db_agent_tools::init();
}
