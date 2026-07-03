use collections::HashSet;
use db_client::schema::QueryResult;
use editor::Editor;
use editor::display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId};
use editor::{Anchor, ToOffset};
use gpui::{Context, Entity, Render, WeakEntity, Window, div, prelude::*};
use language::Point;
use theme::ActiveTheme;
use ui::prelude::*;

/// The maximum number of rows shown inline before falling back to "N more
/// rows -- open the full result in the bottom dock to see the rest." Keeping
/// this small is deliberate: the inline block sits inside a text editor, not
/// a scrollable panel, so a huge table here would make the console itself
/// unreadable.
const MAX_INLINE_ROWS: usize = 20;

/// Manages the inline result blocks for one SQL console editor. Mirrors
/// `repl::Session`'s block-lifecycle pattern (see `crates/repl/src/session.rs`):
/// a fresh run removes any existing block(s) whose statement range overlaps
/// the one being (re-)run, then inserts a new block below the statement.
pub struct InlineResultsController {
    editor: WeakEntity<Editor>,
    pub enabled: bool,
    blocks: Vec<InlineBlockEntry>,
}

struct InlineBlockEntry {
    range: Range<Anchor>,
    block_id: CustomBlockId,
}

use std::ops::Range;

impl InlineResultsController {
    pub fn new(editor: WeakEntity<Editor>) -> Self {
        Self {
            editor,
            enabled: false,
            blocks: Vec::new(),
        }
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        self.enabled = !self.enabled;
        if !self.enabled {
            self.clear_all(cx);
        }
        cx.notify();
    }

    fn clear_all(&mut self, cx: &mut Context<Self>) {
        let ids: HashSet<CustomBlockId> =
            self.blocks.drain(..).map(|entry| entry.block_id).collect();
        if ids.is_empty() {
            return;
        }
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |editor, cx| {
                editor.remove_blocks(ids, None, cx);
            });
        }
    }

    /// Removes any existing inline block for the same statement (identified
    /// by its `start_row..=end_row` overlapping a previous run's range -- a
    /// re-run replaces, it never duplicates) and inserts a fresh block in a
    /// loading state below the statement's last line. Returns `None` when
    /// inline mode is off, or the editor has been dropped.
    pub fn begin_statement(
        &mut self,
        start_row: u32,
        end_row: u32,
        cx: &mut Context<Self>,
    ) -> Option<Entity<InlineResultView>> {
        if !self.enabled {
            return None;
        }
        let editor = self.editor.upgrade()?;

        let (new_range, buffer_snapshot) = editor.update(cx, |editor, cx| {
            let buffer_snapshot = editor.buffer().read(cx).snapshot(cx);
            let start_anchor = buffer_snapshot.anchor_before(Point::new(start_row, 0));
            let end_anchor = buffer_snapshot.anchor_after(Point::new(end_row, 0));
            (start_anchor..end_anchor, buffer_snapshot)
        });

        let mut removed_ids = HashSet::default();
        self.blocks.retain(|entry| {
            let overlaps = entry.range.start.to_offset(&buffer_snapshot)
                <= new_range.end.to_offset(&buffer_snapshot)
                && new_range.start.to_offset(&buffer_snapshot)
                    <= entry.range.end.to_offset(&buffer_snapshot);
            if overlaps {
                removed_ids.insert(entry.block_id);
                false
            } else {
                true
            }
        });

        let view = cx.new(|_| InlineResultView::loading());
        let render_view = view.clone();

        let block_id = editor.update(cx, |editor, cx| {
            if !removed_ids.is_empty() {
                editor.remove_blocks(removed_ids, None, cx);
            }
            let block = BlockProperties {
                placement: BlockPlacement::Below(new_range.end),
                height: Some(1),
                style: BlockStyle::Sticky,
                render: std::sync::Arc::new(move |_cx| render_view.clone().into_any_element()),
                priority: 0,
            };
            editor.insert_blocks([block], None, cx)[0]
        });

        self.blocks.push(InlineBlockEntry {
            range: new_range,
            block_id,
        });
        Some(view)
    }
}

/// The content of one inline result block: a compact, read-only preview of a
/// single statement's result (or its error), capped at `MAX_INLINE_ROWS`.
/// This is deliberately NOT the full interactive `ResultView` -- that entity
/// owns its own focus handling, keyboard shortcuts, and cell editing, all of
/// which would compete with the host console editor's own input handling if
/// embedded as-is inside one of its block decorations. Full editing/sorting/
/// filtering stays in the existing bottom-dock tab, unaffected by this mode.
pub struct InlineResultView {
    state: InlineResultState,
}

enum InlineResultState {
    Loading,
    Error(String),
    Loaded {
        columns: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
        total_rows: usize,
    },
}

impl InlineResultView {
    pub fn loading() -> Self {
        Self {
            state: InlineResultState::Loading,
        }
    }

    pub fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.state = InlineResultState::Error(message);
        cx.notify();
    }

    pub fn set_result(&mut self, result: &QueryResult, cx: &mut Context<Self>) {
        let total_rows = result.rows.len();
        let (shown, _hidden) = capped_rows(&result.rows, MAX_INLINE_ROWS);
        self.state = InlineResultState::Loaded {
            columns: result.columns.clone(),
            rows: shown.to_vec(),
            total_rows,
        };
        cx.notify();
    }
}

/// Splits `rows` into the slice that fits within `cap` and the count of rows
/// left out, so the view can render "N more rows..." honestly instead of
/// silently truncating. Pure so the capping logic is unit-testable without a
/// GPUI render pass.
fn capped_rows<T>(rows: &[T], cap: usize) -> (&[T], usize) {
    if rows.len() <= cap {
        (rows, 0)
    } else {
        (&rows[..cap], rows.len() - cap)
    }
}

impl Render for InlineResultView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let container = div()
            .id("inline-result")
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .p_1()
            .text_size(px(12.));

        match &self.state {
            InlineResultState::Loading => container.child(Label::new("Running…").color(Color::Muted)),
            InlineResultState::Error(message) => {
                container.child(Label::new(message.clone()).color(Color::Error))
            }
            InlineResultState::Loaded {
                columns,
                rows,
                total_rows,
            } => {
                let header = h_flex().gap_2().children(columns.iter().map(|column| {
                    div()
                        .flex_1()
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(column.clone())
                }));
                let body = rows.iter().map(|row| {
                    h_flex().gap_2().children(row.iter().map(|cell| {
                        div()
                            .flex_1()
                            .child(cell.clone().unwrap_or_else(|| "NULL".to_string()))
                    }))
                });
                let hidden = total_rows.saturating_sub(rows.len());
                container
                    .child(header)
                    .children(body)
                    .when(hidden > 0, |el| {
                        el.child(
                            Label::new(format!("{hidden} more row{}…", if hidden == 1 { "" } else { "s" }))
                                .color(Color::Muted),
                        )
                    })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use multi_buffer::MultiBuffer;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    /// Counts the blocks currently attached to `editor` across its full
    /// buffer -- the real, observable proxy for "does an inline result block
    /// exist" (as opposed to inspecting `InlineResultsController`'s private
    /// bookkeeping, which could pass even if the block never actually made
    /// it into the editor's display map).
    fn block_count(editor: &Entity<Editor>, cx: &mut TestAppContext) -> usize {
        editor.update(cx, |editor, cx| {
            let map = editor.display_map.update(cx, |map, cx| map.snapshot(cx));
            map.blocks_in_range(
                editor::display_map::DisplayRow(0)..editor::display_map::DisplayRow(u32::MAX),
            )
            .count()
        })
    }

    #[gpui::test]
    fn toggling_on_and_running_a_statement_inserts_an_inline_block(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| {
            let buffer = MultiBuffer::build_simple("SELECT 1;\nSELECT 2;", cx);
            Editor::for_multibuffer(buffer, None, window, cx)
        });
        let editor = window.root(cx).expect("window root must be the constructed editor");
        let weak_editor = editor.downgrade();

        assert_eq!(
            block_count(&editor, cx),
            0,
            "a freshly opened console must start with no inline blocks"
        );

        let controller = cx.new(|_| InlineResultsController::new(weak_editor));
        controller.update(cx, |controller, cx| {
            controller.toggle(cx);
            assert!(controller.enabled, "toggle must turn inline mode on");
        });

        let view = controller
            .update(cx, |controller, cx| controller.begin_statement(0, 0, cx))
            .expect("enabled controller must produce a block view");
        view.update(cx, |view, cx| {
            view.set_result(
                &QueryResult {
                    columns: vec!["id".to_string()],
                    rows: vec![vec![Some("1".to_string())]],
                    rows_affected: 0,
                    execution_time_ms: 0,
                },
                cx,
            );
        });

        assert_eq!(
            block_count(&editor, cx),
            1,
            "running a statement with inline mode on must insert exactly one block"
        );
    }

    #[gpui::test]
    fn rerunning_the_same_statement_replaces_its_block_instead_of_duplicating(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let window = cx.add_window(|window, cx| {
            let buffer = MultiBuffer::build_simple("SELECT 1;", cx);
            Editor::for_multibuffer(buffer, None, window, cx)
        });
        let editor = window.root(cx).expect("window root must be the constructed editor");
        let controller = cx.new(|_| InlineResultsController::new(editor.downgrade()));
        controller.update(cx, |controller, cx| controller.toggle(cx));

        controller.update(cx, |controller, cx| controller.begin_statement(0, 0, cx));
        assert_eq!(block_count(&editor, cx), 1);

        // Re-running the exact same statement range must replace, not add.
        controller.update(cx, |controller, cx| controller.begin_statement(0, 0, cx));
        assert_eq!(
            block_count(&editor, cx),
            1,
            "re-running the same statement must replace its existing block, not duplicate it"
        );
    }

    #[gpui::test]
    fn running_a_second_statement_adds_an_independent_block(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| {
            let buffer = MultiBuffer::build_simple("SELECT 1;\nSELECT 2;", cx);
            Editor::for_multibuffer(buffer, None, window, cx)
        });
        let editor = window.root(cx).expect("window root must be the constructed editor");
        let controller = cx.new(|_| InlineResultsController::new(editor.downgrade()));
        controller.update(cx, |controller, cx| controller.toggle(cx));

        controller.update(cx, |controller, cx| controller.begin_statement(0, 0, cx));
        assert_eq!(block_count(&editor, cx), 1);

        controller.update(cx, |controller, cx| controller.begin_statement(1, 1, cx));
        assert_eq!(
            block_count(&editor, cx),
            2,
            "a second, non-overlapping statement must get its own independent block, \
             leaving the first untouched"
        );
    }

    #[gpui::test]
    fn disabled_controller_never_inserts_a_block(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| {
            let buffer = MultiBuffer::build_simple("SELECT 1;", cx);
            Editor::for_multibuffer(buffer, None, window, cx)
        });
        let editor = window.root(cx).expect("window root must be the constructed editor");
        let controller = cx.new(|_| InlineResultsController::new(editor.downgrade()));

        let view = controller.update(cx, |controller, cx| controller.begin_statement(0, 0, cx));
        assert!(
            view.is_none(),
            "a controller that was never toggled on must not produce a block view"
        );
        assert_eq!(
            block_count(&editor, cx),
            0,
            "the existing bottom-dock-only behavior must be completely unaffected when inline \
             mode has never been enabled for this console"
        );
    }

    #[test]
    fn capped_rows_returns_everything_when_under_the_cap() {
        let rows = vec![1, 2, 3];
        let (shown, hidden) = capped_rows(&rows, 20);
        assert_eq!(shown, &[1, 2, 3]);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn capped_rows_truncates_and_reports_the_true_remainder() {
        let rows: Vec<u32> = (0..25).collect();
        let (shown, hidden) = capped_rows(&rows, 20);
        assert_eq!(shown.len(), 20);
        assert_eq!(hidden, 5);
    }

    #[test]
    fn capped_rows_handles_an_empty_input_without_panicking() {
        let rows: Vec<u32> = Vec::new();
        let (shown, hidden) = capped_rows(&rows, 20);
        assert!(shown.is_empty());
        assert_eq!(hidden, 0);
    }
}
