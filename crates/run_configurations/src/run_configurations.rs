pub mod configurations_file;
pub mod configurations_store;
pub mod configurations_toolbar;
pub mod configurations_view;
pub mod debugging;
pub mod new_configuration_modal;
pub mod process_metrics;
pub mod ways_to_run_modal;

use gpui::App;

// The two actions live in `zed_actions` so the editor can dispatch one of them
// without depending on this crate.
pub use zed_actions::run_configurations::{
    CreateFromEntryPoint, EntryPointOffer, OpenRunConfigurations, RunFromEntryPoint,
};

pub fn init(cx: &mut App) {
    configurations_view::init(cx);
}
