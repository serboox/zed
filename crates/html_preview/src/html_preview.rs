use gpui::App;
use workspace::Workspace;

pub mod html_preview_settings;
#[cfg(feature = "servo")]
pub mod page_scroll;
pub mod html_preview_view;

pub use zed_actions::preview::html::{
    FindInPage, FindNextInPage, FindPreviousInPage, NewBrowserTab, OpenPreview,
    OpenPreviewToTheSide, StopFindingInPage,
};

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        html_preview_view::HtmlPreviewView::register(workspace, window, cx);
    })
    .detach();
}
