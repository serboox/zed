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
        PdfZoomReset,
        /// Copies the text under the selection.
        PdfCopy,
        /// Shows the next page.
        PdfNextPage,
        /// Shows the previous page.
        PdfPreviousPage,
        /// Turns every page a quarter turn clockwise.
        PdfRotate,
        /// Turns every page a quarter turn anticlockwise.
        PdfRotateBack,
        /// Looks for text in the document.
        PdfFind,
        /// Shows the next place the text was found.
        PdfFindNext,
        /// Shows the previous place the text was found.
        PdfFindPrevious,
        /// Sends the document to a printer.
        PdfPrint,
        /// Scales pages so one fills the width of the window.
        PdfFitWidth,
        /// Scales pages so a whole page fits in the window.
        PdfFitPage,
        /// Shows the first page.
        PdfFirstPage,
        /// Shows the last page.
        PdfLastPage,
        /// Asks which page to show.
        PdfGoToPage,
        /// Selects everything on the page in view.
        PdfSelectPage,
        /// Saves the document somewhere else.
        PdfSaveACopy,
        /// Shows what the document says about itself.
        PdfProperties,
        /// Gives the whole window over to the document.
        PdfFullScreen,
        /// Shows or hides the small pictures of the pages.
        PdfThumbnails,
        /// Shows or hides the document's own contents.
        PdfContents,
        /// Reads one page at a time rather than a column of them.
        PdfOnePage
    ]
);

pub fn init(cx: &mut App) {
    workspace::register_project_item::<pdf_preview_view::PdfView>(cx);
}
