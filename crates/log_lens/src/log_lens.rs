mod log_digest;
mod log_lens_view;
mod log_reader;

#[cfg(test)]
mod log_lens_tests;

pub use log_digest::{Advance, LogDigest, Row, TerminalTail};
pub use log_lens_view::LogLensView;
pub use log_reader::{Field, Level, LogEvent, LogLine, LogParser, read_line};

use gpui::{App, AppContext as _, Entity, WeakEntity, Window, actions};
use terminal::Terminal;
use ui::SharedString;
use workspace::Workspace;

actions!(
    log_lens,
    [
        /// Reads the run terminal's output in a laid-out view beside it.
        OpenLogLens,
    ]
);

/// Opens a lens on `terminal` in the workspace's active pane, or activates the
/// one already reading that terminal.
pub fn open(
    terminal: Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    title: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace_entity) = workspace.upgrade() else {
        return;
    };
    let existing = workspace_entity.update(cx, |workspace, cx| {
        let pane = workspace.active_pane().clone();
        let existing = pane
            .read(cx)
            .items_of_type::<LogLensView>()
            .find(|lens| lens.read(cx).reads(&terminal));
        existing.and_then(|lens| {
            let at = pane.read(cx).index_for_item(&lens)?;
            Some((pane, at))
        })
    });
    if let Some((pane, at)) = existing {
        pane.update(cx, |pane, cx| {
            pane.activate_item(at, true, true, window, cx);
        });
        return;
    }
    let lens = cx.new(|cx| LogLensView::new(terminal, workspace, title, window, cx));
    workspace_entity.update(cx, |workspace, cx| {
        workspace.active_pane().clone().update(cx, |pane, cx| {
            pane.add_item(Box::new(lens), true, true, None, window, cx);
        });
    });
}
