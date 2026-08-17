pub mod configurations_file;
pub mod configurations_store;
pub mod configurations_view;

use gpui::{App, actions};

actions!(
    run_configurations,
    [
        /// Shows the project's run configurations.
        OpenRunConfigurations
    ]
);

pub fn init(cx: &mut App) {
    configurations_view::init(cx);
}
