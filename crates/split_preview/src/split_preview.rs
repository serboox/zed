use gpui::{App, actions};
use workspace::Workspace;

pub mod item_overlay;
pub mod open_split_preview;
pub mod preview_project_item;
pub mod split_preview_view;

pub use open_split_preview::{
    PreviewKind, preview_kind_for, preview_kind_for_extension, preview_kind_for_path,
};
pub use preview_project_item::{PreviewableDocument, SplitPreviewSettings, opens_in_preview};
pub use split_preview_view::{PreviewLayout, SplitPreviewView};

actions!(
    preview,
    [
        /// Opens the active document in a tab that holds its editor next to a
        /// live preview.
        OpenSplitPreview,
        /// Shows only the source editor in a split preview tab.
        ShowEditorOnly,
        /// Shows the source editor and the rendered preview side by side.
        ShowEditorAndPreview,
        /// Shows only the rendered preview in a split preview tab.
        ShowPreviewOnly,
        /// Steps to the next layout: editor, editor and preview, preview.
        CycleLayout,
    ]
);

pub fn init(cx: &mut App) {
    // After the editor's own registration, so that a document with a rendered
    // view is opened by way of that view: the registry asks whoever registered
    // last first, and the editor answers for every path.
    workspace::register_project_item::<SplitPreviewView>(cx);
    item_overlay::init(cx);
    cx.observe_new(|workspace: &mut Workspace, window, _cx| {
        if window.is_none() {
            return;
        }
        open_split_preview::register(workspace);
    })
    .detach();
}
