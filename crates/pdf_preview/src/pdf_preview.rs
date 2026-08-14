pub mod pdf_engine;
pub mod pdf_item;
pub mod pdf_preview_view;

use gpui::{App, actions};

actions!(
    pdf,
    [
        /// Renders the document larger.
        PdfZoomIn,
        /// Renders the document smaller.
        PdfZoomOut,
        /// Returns the document to the size it opened at.
        PdfZoomReset
    ]
);

pub fn init(cx: &mut App) {
    workspace::register_project_item::<pdf_preview_view::PdfView>(cx);
}
