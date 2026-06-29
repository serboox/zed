use db_client::connection::{ConnectionId, DatabaseDriver};
use editor::Editor;
use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, Window, prelude::*, px};
use ui::prelude::*;
use ui::{
    Button, ButtonStyle, Checkbox, ContextMenu, Divider, IconButton, IconName, Label, PopoverMenu,
};
use util::ResultExt;

use crate::store::DatabaseStore;

fn quote_ident(name: &str, driver: DatabaseDriver) -> String {
    match driver {
        DatabaseDriver::MySQL => format!("`{}`", name.replace('`', "``")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

fn quote_value(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFile {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Picks the most likely delimiter by counting candidates in the first line.
pub fn detect_delimiter(sample: &str) -> char {
    let first_line = sample.lines().next().unwrap_or_default();
    let candidates = [',', '\t', ';', '|'];
    candidates
        .into_iter()
        .max_by_key(|&candidate| first_line.matches(candidate).count())
        .filter(|&candidate| first_line.contains(candidate))
        .unwrap_or(',')
}

/// Parses delimited text into records, honoring RFC-4180-style quoting: a field
/// wrapped in double quotes may contain the delimiter, newlines, and escaped
/// quotes (`""`). When `has_header` is set the first record becomes the headers;
/// otherwise headers are synthesized as `column_1`, `column_2`, …
pub fn parse_delimited(text: &str, delimiter: char, has_header: bool) -> ParsedFile {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    let mut field_started = false;

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(ch);
            }
            continue;
        }
        match ch {
            '"' if !field_started || field.is_empty() => {
                in_quotes = true;
                field_started = true;
            }
            '\r' => {}
            '\n' => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
                field_started = false;
            }
            _ if ch == delimiter => {
                record.push(std::mem::take(&mut field));
                field_started = false;
            }
            _ => {
                field.push(ch);
                field_started = true;
            }
        }
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }

    let mut records = records.into_iter();
    let headers = if has_header {
        records.next().unwrap_or_default()
    } else {
        Vec::new()
    };
    let rows: Vec<Vec<String>> = records.collect();
    let headers = if has_header {
        headers
    } else {
        let width = rows.first().map_or(0, |row| row.len());
        (1..=width).map(|index| format!("column_{index}")).collect()
    };
    ParsedFile { headers, rows }
}

/// Builds multi-row INSERT statements that load `rows` into `table`. `mapping[i]`
/// is the file-column index feeding target column `i`, or `None` to leave that
/// column unset (engine default). Empty source cells become NULL when
/// `null_on_empty`, otherwise an empty string.
pub fn build_insert_statements(
    table: &str,
    driver: DatabaseDriver,
    target_columns: &[String],
    rows: &[Vec<String>],
    mapping: &[Option<usize>],
    null_on_empty: bool,
    batch_size: usize,
) -> Vec<String> {
    let included: Vec<usize> = (0..target_columns.len())
        .filter(|&index| mapping.get(index).copied().flatten().is_some())
        .collect();
    if included.is_empty() || rows.is_empty() {
        return Vec::new();
    }
    let batch_size = batch_size.max(1);
    let columns_sql = included
        .iter()
        .map(|&index| quote_ident(&target_columns[index], driver))
        .collect::<Vec<_>>()
        .join(", ");
    let table_sql = quote_ident(table, driver);

    let mut statements = Vec::new();
    for chunk in rows.chunks(batch_size) {
        let mut value_groups = Vec::with_capacity(chunk.len());
        for row in chunk {
            let cells = included
                .iter()
                .map(|&target_index| {
                    let source_index = mapping[target_index].expect("included implies mapped");
                    match row.get(source_index) {
                        Some(value) if value.is_empty() && null_on_empty => "NULL".to_string(),
                        Some(value) => quote_value(value),
                        None if null_on_empty => "NULL".to_string(),
                        None => quote_value(""),
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            value_groups.push(format!("({cells})"));
        }
        statements.push(format!(
            "INSERT INTO {table_sql} ({columns_sql}) VALUES {}",
            value_groups.join(", ")
        ));
    }
    statements
}

// Default file column for a target column: an exact (case-insensitive) header
// match, otherwise the same position, otherwise unset.
fn default_mapping(target_columns: &[String], headers: &[String]) -> Vec<Option<usize>> {
    target_columns
        .iter()
        .enumerate()
        .map(|(position, target)| {
            headers
                .iter()
                .position(|header| header.eq_ignore_ascii_case(target))
                .or(Some(position).filter(|&index| index < headers.len()))
        })
        .collect()
}

pub enum DataImportEvent {
    Dismissed,
}

const IMPORT_BATCH_SIZE: usize = 200;
const PREVIEW_ROW_LIMIT: usize = 20;

pub struct ImportDataView {
    focus_handle: FocusHandle,
    store: Entity<DatabaseStore>,
    connection_id: ConnectionId,
    database: String,
    table: String,
    driver: DatabaseDriver,
    target_columns: Vec<String>,
    path_editor: Entity<Editor>,
    delimiter: char,
    has_header: bool,
    null_on_empty: bool,
    parsed: Option<ParsedFile>,
    mapping: Vec<Option<usize>>,
    status: Option<String>,
    importing: bool,
}

impl ImportDataView {
    pub fn new(
        store: Entity<DatabaseStore>,
        connection_id: ConnectionId,
        database: String,
        table: String,
        driver: DatabaseDriver,
        target_columns: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("/path/to/file.csv", window, cx);
            editor
        });
        let mapping = vec![None; target_columns.len()];
        Self {
            focus_handle: cx.focus_handle(),
            store,
            connection_id,
            database,
            table,
            driver,
            target_columns,
            path_editor,
            delimiter: ',',
            has_header: true,
            null_on_empty: true,
            parsed: None,
            mapping,
            status: None,
            importing: false,
        }
    }

    fn load_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.path_editor.read(cx).text(cx).trim().to_string();
        if path.is_empty() {
            self.status = Some("Enter a file path to load.".into());
            cx.notify();
            return;
        }
        let has_header = self.has_header;
        let chosen_delimiter = self.delimiter;
        self.status = Some("Loading…".into());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let read = cx
                .background_spawn(async move { std::fs::read_to_string(&path) })
                .await;
            this.update(cx, |view, cx| {
                match read {
                    Ok(text) => {
                        let delimiter = if chosen_delimiter == ',' {
                            detect_delimiter(&text)
                        } else {
                            chosen_delimiter
                        };
                        view.delimiter = delimiter;
                        let parsed = parse_delimited(&text, delimiter, has_header);
                        view.mapping = default_mapping(&view.target_columns, &parsed.headers);
                        view.status = Some(format!("Loaded {} rows.", parsed.rows.len()));
                        view.parsed = Some(parsed);
                    }
                    Err(error) => {
                        view.status = Some(format!("Could not read file: {error}"));
                    }
                }
                cx.notify();
            })
            .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn run_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(parsed) = self.parsed.clone() else {
            self.status = Some("Load a file first.".into());
            cx.notify();
            return;
        };
        let statements = build_insert_statements(
            &self.table,
            self.driver,
            &self.target_columns,
            &parsed.rows,
            &self.mapping,
            self.null_on_empty,
            IMPORT_BATCH_SIZE,
        );
        if statements.is_empty() {
            self.status = Some("Map at least one column before importing.".into());
            cx.notify();
            return;
        }
        let total_rows = parsed.rows.len();
        let store = self.store.clone();
        let connection_id = self.connection_id;
        let database = self.database.clone();
        self.importing = true;
        self.status = Some("Importing…".into());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let mut imported = 0usize;
            let mut failure: Option<String> = None;
            for statement in statements {
                let task = store.update(cx, |store, cx| {
                    store.execute_query(connection_id, database.clone(), statement, cx)
                });
                match task.await {
                    Ok(_) => imported += 1,
                    Err(error) => {
                        failure = Some(error.to_string());
                        break;
                    }
                }
            }
            this.update(cx, |view, cx| {
                view.importing = false;
                view.status = Some(match failure {
                    Some(error) => format!("Import failed after {imported} batch(es): {error}"),
                    None => format!("Imported {total_rows} rows."),
                });
                cx.notify();
            })
            .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn set_mapping(&mut self, target_index: usize, source: Option<usize>, cx: &mut Context<Self>) {
        if let Some(slot) = self.mapping.get_mut(target_index) {
            *slot = source;
            cx.notify();
        }
    }

    fn render_mapping_row(
        &self,
        target_index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let target = self.target_columns[target_index].clone();
        let headers = self
            .parsed
            .as_ref()
            .map(|parsed| parsed.headers.clone())
            .unwrap_or_default();
        let current = self.mapping.get(target_index).copied().flatten();
        let current_label: SharedString = current
            .and_then(|index| headers.get(index).cloned())
            .map(SharedString::from)
            .unwrap_or_else(|| "(skip)".into());
        let view = cx.entity().downgrade();

        h_flex()
            .gap_2()
            .justify_between()
            .child(Label::new(target).size(LabelSize::Small))
            .child(
                PopoverMenu::new(("map", target_index))
                    .trigger(
                        Button::new(("map-trigger", target_index), current_label)
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small),
                    )
                    .menu(move |window, cx| {
                        let headers = headers.clone();
                        let view = view.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            menu = menu.entry("(skip)", None, {
                                let view = view.clone();
                                move |_, cx| {
                                    view.update(cx, |this, cx| this.set_mapping(target_index, None, cx))
                                        .ok();
                                }
                            });
                            for (source_index, header) in headers.iter().enumerate() {
                                let view = view.clone();
                                menu = menu.entry(header.clone(), None, move |_, cx| {
                                    view.update(cx, |this, cx| {
                                        this.set_mapping(target_index, Some(source_index), cx)
                                    })
                                    .ok();
                                });
                            }
                            menu
                        }))
                    }),
            )
    }
}

impl EventEmitter<DataImportEvent> for ImportDataView {}

impl Focusable for ImportDataView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ImportDataView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preview = self.parsed.clone();
        let mut mapping_rows = Vec::with_capacity(self.target_columns.len());
        for index in 0..self.target_columns.len() {
            mapping_rows.push(self.render_mapping_row(index, cx).into_any_element());
        }
        let has_header = self.has_header;
        let null_on_empty = self.null_on_empty;
        let importing = self.importing;

        v_flex()
            .key_context("DataImport")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(px(640.0))
            .max_h(px(560.0))
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .child(Label::new(format!("Import data into {}", self.table)))
                    .child(
                        IconButton::new("close-import", IconName::Close).on_click(
                            cx.listener(|_, _, _, cx| cx.emit(DataImportEvent::Dismissed)),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(self.path_editor.clone()))
                    .child(
                        ui::Button::new("load-file", "Load")
                            .on_click(cx.listener(|view, _, window, cx| view.load_file(window, cx))),
                    ),
            )
            .child(
                h_flex()
                    .gap_4()
                    .child(
                        Checkbox::new("has-header", has_header.into()).on_click(cx.listener(
                            |view, _, _, cx| {
                                view.has_header = !view.has_header;
                                cx.notify();
                            },
                        )),
                    )
                    .child(Label::new("First row is header").size(LabelSize::Small))
                    .child(
                        Checkbox::new("null-empty", null_on_empty.into()).on_click(cx.listener(
                            |view, _, _, cx| {
                                view.null_on_empty = !view.null_on_empty;
                                cx.notify();
                            },
                        )),
                    )
                    .child(Label::new("Insert empty as NULL").size(LabelSize::Small)),
            )
            .child(Divider::horizontal())
            .child(Label::new("Column mapping").size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .id("mapping-scroll")
                    .max_h(px(200.0))
                    .overflow_y_scroll()
                    .child(v_flex().gap_1().children(mapping_rows)),
            )
            .when_some(preview, |el, parsed| {
                el.child(
                    Label::new(format!(
                        "{} columns, {} rows ({} previewed)",
                        parsed.headers.len(),
                        parsed.rows.len(),
                        parsed.rows.len().min(PREVIEW_ROW_LIMIT)
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .when_some(self.status.clone(), |el, status| {
                el.child(Label::new(status).size(LabelSize::Small))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        ui::Button::new("cancel-import", "Cancel").on_click(
                            cx.listener(|_, _, _, cx| cx.emit(DataImportEvent::Dismissed)),
                        ),
                    )
                    .child(
                        ui::Button::new("run-import", "Import")
                            .disabled(importing)
                            .on_click(cx.listener(|view, _, window, cx| view.run_import(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_delimiters() {
        assert_eq!(detect_delimiter("a,b,c\n1,2,3"), ',');
        assert_eq!(detect_delimiter("a\tb\tc"), '\t');
        assert_eq!(detect_delimiter("a;b;c"), ';');
        assert_eq!(detect_delimiter("only_one_column"), ',');
    }

    #[test]
    fn parses_header_and_rows() {
        let parsed = parse_delimited("id,name\n1,Alice\n2,Bob", ',', true);
        assert_eq!(parsed.headers, vec!["id", "name"]);
        assert_eq!(parsed.rows, vec![vec!["1", "Alice"], vec!["2", "Bob"]]);
    }

    #[test]
    fn parses_without_header_synthesizes_names() {
        let parsed = parse_delimited("1,Alice\n2,Bob", ',', false);
        assert_eq!(parsed.headers, vec!["column_1", "column_2"]);
        assert_eq!(parsed.rows.len(), 2);
    }

    #[test]
    fn parses_quoted_fields_with_embedded_delimiter_and_quotes() {
        let parsed = parse_delimited("name,note\n\"Smith, Jr.\",\"He said \"\"hi\"\"\"", ',', true);
        assert_eq!(parsed.rows[0][0], "Smith, Jr.");
        assert_eq!(parsed.rows[0][1], "He said \"hi\"");
    }

    #[test]
    fn build_insert_maps_and_quotes() {
        let target = vec!["id".to_string(), "name".to_string()];
        let rows = vec![
            vec!["1".to_string(), "O'Brien".to_string()],
            vec!["2".to_string(), "Bob".to_string()],
        ];
        let mapping = vec![Some(0), Some(1)];
        let statements = build_insert_statements(
            "people",
            DatabaseDriver::PostgreSQL,
            &target,
            &rows,
            &mapping,
            true,
            10,
        );
        assert_eq!(statements.len(), 1);
        assert!(statements[0].contains("INSERT INTO \"people\" (\"id\", \"name\")"));
        assert!(statements[0].contains("'O''Brien'"));
    }

    #[test]
    fn build_insert_skips_unmapped_and_nulls_empty() {
        let target = vec!["id".to_string(), "name".to_string(), "note".to_string()];
        let rows = vec![vec!["1".to_string(), "".to_string()]];
        // name <- column 1 (empty -> NULL), note unmapped (skipped), id <- column 0.
        let mapping = vec![Some(0), Some(1), None];
        let statements = build_insert_statements(
            "t",
            DatabaseDriver::MySQL,
            &target,
            &rows,
            &mapping,
            true,
            10,
        );
        assert!(statements[0].contains("`id`, `name`"));
        assert!(!statements[0].contains("note"));
        assert!(statements[0].contains("NULL"));
    }

    #[test]
    fn build_insert_batches_rows() {
        let target = vec!["id".to_string()];
        let rows: Vec<Vec<String>> = (0..5).map(|n| vec![n.to_string()]).collect();
        let mapping = vec![Some(0)];
        let statements =
            build_insert_statements("t", DatabaseDriver::MySQL, &target, &rows, &mapping, true, 2);
        assert_eq!(statements.len(), 3);
    }
}
