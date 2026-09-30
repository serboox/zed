use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use editor::{Editor, HighlightKey, MultiBufferOffset};
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use settings::{RegisterSetting, Settings, SubqueryChoice};
use sqlparser::dialect::Dialect;
use ui::{Label, LabelSize, ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace};

use crate::console_statements::{hash_starts_a_comment, nested_queries_at};
use crate::panel::skip_leading_whitespace_and_comments;

const PREVIEW_CHARACTERS: usize = 80;
const WHOLE_STATEMENT_LABEL: &str = "Whole statement";

/// How the query consoles are set by the reader.
#[derive(Clone, Debug, RegisterSetting)]
pub struct DatabaseConsoleSettings {
    pub subquery_choice: SubqueryChoice,
}

impl Settings for DatabaseConsoleSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            subquery_choice: content
                .database_console
                .as_ref()
                .and_then(|configured| configured.subquery_choice)
                .unwrap_or_default(),
        }
    }
}

/// One part of a statement that can be run or explained on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StatementChoice {
    pub(crate) label: String,
    pub(crate) range: Range<usize>,
    pub(crate) correlated: bool,
    pub(crate) start_row: u32,
    pub(crate) end_row: u32,
    pub(crate) preview: String,
}

impl StatementChoice {
    fn new(text: &str, label: String, range: Range<usize>, correlated: bool) -> Self {
        let piece = text.get(range.clone()).unwrap_or_default();
        let preview = piece
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default();
        let preview = match preview.char_indices().nth(PREVIEW_CHARACTERS) {
            Some((cut, _)) => format!("{}…", &preview[..cut]),
            None => preview.to_string(),
        };
        Self {
            label,
            start_row: rows_before(text, range.start),
            end_row: rows_before(text, range.end),
            range,
            correlated,
            preview,
        }
    }
}

fn rows_before(text: &str, offset: usize) -> u32 {
    text.as_bytes()
        .get(..offset.min(text.len()))
        .unwrap_or_default()
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count() as u32
}

/// The parts of the statement at `statement` that hold `cursor`, the smallest
/// first and the whole statement last. Empty when the cursor is in no part
/// smaller than the statement: there is nothing to choose from.
pub(crate) fn choices_at_cursor(
    text: &str,
    statement: Range<usize>,
    cursor: usize,
    dialect: Option<&dyn Dialect>,
) -> Vec<StatementChoice> {
    let Some(statement_text) = text.get(statement.clone()) else {
        return Vec::new();
    };
    if cursor < statement.start || cursor > statement.end {
        return Vec::new();
    }
    let nested = nested_queries_at(statement_text, cursor - statement.start, dialect);
    if nested.is_empty() {
        return Vec::new();
    }
    let mut choices: Vec<StatementChoice> = nested
        .into_iter()
        .map(|piece| {
            let range = statement.start + piece.range.start..statement.start + piece.range.end;
            StatementChoice::new(text, piece.label, range, piece.correlated)
        })
        .collect();
    let content_start = statement.start
        + skip_leading_whitespace_and_comments(statement_text, hash_starts_a_comment(dialect));
    choices.push(StatementChoice::new(
        text,
        WHOLE_STATEMENT_LABEL.to_string(),
        content_start..statement.end,
        false,
    ));
    choices
}

pub(crate) type OnChoose = Rc<dyn Fn(Range<usize>, &mut Window, &mut App)>;

pub(crate) struct StatementChooser {
    picker: Entity<Picker<StatementChooserDelegate>>,
    _subscription: Subscription,
}

impl StatementChooser {
    pub(crate) fn open(
        workspace: &mut Workspace,
        title: &'static str,
        editor: &Entity<Editor>,
        choices: Vec<StatementChoice>,
        on_choose: OnChoose,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let editor = editor.downgrade();
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(title, editor, choices, on_choose, window, cx)
        });
    }

    fn new(
        title: &'static str,
        editor: WeakEntity<Editor>,
        choices: Vec<StatementChoice>,
        on_choose: OnChoose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = StatementChooserDelegate {
            title: title.into(),
            editor,
            choices,
            selected_index: 0,
            on_choose,
        };
        let picker = cx.new(|cx| Picker::nonsearchable_uniform_list(delegate, window, cx));
        picker.update(cx, |picker, cx| picker.delegate.highlight_selected(cx));
        let subscription = cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| {
            cx.emit(DismissEvent);
        });
        Self {
            picker,
            _subscription: subscription,
        }
    }

    #[cfg(test)]
    pub(crate) fn selected_choice(&self, cx: &App) -> Option<StatementChoice> {
        let delegate = &self.picker.read(cx).delegate;
        delegate.choices.get(delegate.selected_index).cloned()
    }

    #[cfg(test)]
    pub(crate) fn choices(&self, cx: &App) -> Vec<StatementChoice> {
        self.picker.read(cx).delegate.choices.clone()
    }
}

impl ModalView for StatementChooser {}

impl EventEmitter<DismissEvent> for StatementChooser {}

impl Focusable for StatementChooser {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for StatementChooser {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("StatementChooser")
            .w(rems(36.))
            .child(self.picker.clone())
    }
}

pub(crate) struct StatementChooserDelegate {
    title: SharedString,
    editor: WeakEntity<Editor>,
    choices: Vec<StatementChoice>,
    selected_index: usize,
    on_choose: OnChoose,
}

impl StatementChooserDelegate {
    fn highlight_selected(&self, cx: &mut App) {
        let Some(choice) = self.choices.get(self.selected_index) else {
            return;
        };
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let range = snapshot.anchor_before(MultiBufferOffset(choice.range.start))
                ..snapshot.anchor_after(MultiBufferOffset(choice.range.end));
            editor.highlight_background(
                HighlightKey::PickerPreview,
                &[range],
                |_, theme| theme.colors().search_match_background,
                cx,
            );
        });
    }

    fn clear_highlight(&self, cx: &mut App) {
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |editor, cx| {
                editor.clear_background_highlights(HighlightKey::PickerPreview, cx);
            });
        }
    }
}

impl PickerDelegate for StatementChooserDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "statement chooser"
    }

    fn match_count(&self) -> usize {
        self.choices.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
        self.highlight_selected(cx);
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        Arc::from("")
    }

    fn update_matches(
        &mut self,
        _query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(choice) = self.choices.get(self.selected_index) else {
            return;
        };
        let range = choice.range.clone();
        let on_choose = self.on_choose.clone();
        self.clear_highlight(cx);
        cx.emit(DismissEvent);
        window.defer(cx, move |window, cx| on_choose(range, window, cx));
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.clear_highlight(cx);
        cx.emit(DismissEvent);
    }

    fn render_header(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        Some(
            div()
                .px_3()
                .py_2()
                .child(
                    Label::new(self.title.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        )
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let choice = self.choices.get(ix)?;
        let rows = if choice.start_row == choice.end_row {
            format!("line {}", choice.start_row + 1)
        } else {
            format!("lines {}–{}", choice.start_row + 1, choice.end_row + 1)
        };
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .w_full()
                        .child(
                            h_flex()
                                .justify_between()
                                .child(Label::new(choice.label.clone()))
                                .child(Label::new(rows).size(LabelSize::Small).color(Color::Muted)),
                        )
                        .child(
                            h_flex()
                                .justify_between()
                                .gap_2()
                                .child(
                                    Label::new(choice.preview.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                                .when(choice.correlated, |row| {
                                    row.child(
                                        Label::new("needs the outer query")
                                            .size(LabelSize::Small)
                                            .color(Color::Warning),
                                    )
                                }),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::MySqlDialect;

    fn choices(text: &str, cursor_at: &str) -> Vec<StatementChoice> {
        let cursor = text.find(cursor_at).expect("marker");
        choices_at_cursor(text, 0..text.len(), cursor, Some(&MySqlDialect {}))
    }

    #[test]
    fn a_cursor_outside_every_subquery_has_nothing_to_choose_from() {
        let text = "SELECT * FROM a WHERE id IN (SELECT id FROM b)";
        assert!(choices(text, "* FROM").is_empty());
    }

    #[test]
    fn the_smallest_part_comes_first_and_the_whole_statement_last() {
        let text = "SELECT * FROM a WHERE id IN (SELECT id FROM b)";
        let found = choices(text, "id FROM b");
        let labels: Vec<&str> = found.iter().map(|choice| choice.label.as_str()).collect();
        assert_eq!(labels, ["Subquery", "Whole statement"]);
        assert_eq!(&text[found[0].range.clone()], "SELECT id FROM b");
        assert_eq!(&text[found[1].range.clone()], text);
    }

    #[test]
    fn the_whole_statement_starts_after_the_comments_above_it() {
        let text = "-- about a\nSELECT * FROM a WHERE id IN (SELECT id FROM b)";
        let found = choices(text, "id FROM b");
        let whole = found.last().expect("the whole statement");
        assert!(text[whole.range.clone()].starts_with("SELECT * FROM a"));
        assert_eq!(whole.start_row, 1);
    }

    #[test]
    fn a_piece_knows_its_rows_and_shows_its_first_line() {
        let text = "SELECT * FROM a WHERE id IN (\n    SELECT id\n    FROM b\n)";
        let found = choices(text, "SELECT id");
        assert_eq!((found[0].start_row, found[0].end_row), (1, 2));
        assert_eq!(found[0].preview, "SELECT id");
        assert_eq!((found[1].start_row, found[1].end_row), (0, 3));
    }

    #[test]
    fn a_long_first_line_is_cut_for_the_preview() {
        let long = "x".repeat(200);
        let text = format!("SELECT * FROM a WHERE id IN (SELECT {long} FROM b)");
        let found = choices(&text, "SELECT xx");
        assert_eq!(found[0].preview.chars().count(), PREVIEW_CHARACTERS + 1);
        assert!(found[0].preview.ends_with('…'));
    }
}
