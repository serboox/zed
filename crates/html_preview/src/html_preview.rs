use gpui::{App, actions};
use workspace::Workspace;

#[cfg(feature = "servo")]
pub mod browser_tools_panel;
pub mod html_preview_settings;
pub mod html_preview_view;
#[cfg(feature = "servo")]
pub mod page_scroll;

pub use zed_actions::preview::html::{
    FindInPage, FindNextInPage, FindPreviousInPage, NewBrowserTab, OpenPreview,
    OpenPreviewToTheSide, StopFindingInPage,
};

// Declared here rather than beside the panel itself, which a build without the
// web engine leaves out: the page's own menu offers the tools in every build,
// and an item that names an action has to have one to name.
actions!(
    browser_tools,
    [
        /// Shows or hides the developer tools for the page being read.
        ToggleFocus,
        /// Shows the developer tools for the page being read, and puts the
        /// reader in them. Never hides them: a reader who asks to inspect a page
        /// with the tools already open means "show me", and toggling them shut
        /// there reads as the button doing nothing at all.
        Inspect
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        html_preview_view::HtmlPreviewView::register(workspace, window, cx);
    })
    .detach();
}
