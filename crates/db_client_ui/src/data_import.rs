use db_client::connection::{ConnectionId, DatabaseDriver};
use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Window, prelude::*,
    px,
};
use ui::prelude::*;
use workspace::ModalView;
use ui::{
    Button, ButtonStyle, Checkbox, ContextMenu, Divider, Label, PopoverMenu,
};
use util::ResultExt;

use crate::store::DatabaseStore;

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
        .map(|&index| driver.quote_identifier(&target_columns[index]))
        .collect::<Vec<_>>()
        .join(", ");
    let table_sql = driver.quote_identifier(table);

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

/// Statements to run before and after a bulk load to skip index/trigger
/// maintenance per row, or `None` when the driver has no clean equivalent
/// (SQLite, ClickHouse, Redis).
pub fn disable_indexes_statements(driver: DatabaseDriver, table: &str) -> Option<(String, String)> {
    match driver {
        DatabaseDriver::MySQL => Some((
            format!("ALTER TABLE {} DISABLE KEYS", driver.quote_identifier(table)),
            format!("ALTER TABLE {} ENABLE KEYS", driver.quote_identifier(table)),
        )),
        // Postgres has no bulk "disable indexes" statement; disabling triggers
        // (which also suspends FK-enforcement triggers) is the closest
        // equivalent a bulk load actually benefits from.
        DatabaseDriver::PostgreSQL => Some((
            "SET session_replication_role = replica".to_string(),
            "SET session_replication_role = DEFAULT".to_string(),
        )),
        DatabaseDriver::SQLite | DatabaseDriver::ClickHouse | DatabaseDriver::Redis => None,
    }
}

/// Renders a source row back out as a single line for the import error file,
/// using the same delimiter the file was parsed with.
fn format_error_line(row: &[String], delimiter: char) -> String {
    row.join(&delimiter.to_string())
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


const IMPORT_BATCH_SIZE: usize = 200;
const PREVIEW_ROW_LIMIT: usize = 20;

/// Charsets offered for decoding the source file before parsing. Label strings
/// match what `encoding_rs::Encoding::for_label` accepts.
const IMPORT_CHARSETS: &[&str] = &["utf-8", "windows-1251", "iso-8859-1", "utf-16"];

pub struct ImportDataView {
    focus_handle: FocusHandle,
    store: Entity<DatabaseStore>,
    connection_id: ConnectionId,
    database: String,
    table: String,
    driver: DatabaseDriver,
    target_columns: Vec<String>,
    path_editor: Entity<Editor>,
    loaded_path: Option<String>,
    delimiter: char,
    has_header: bool,
    null_on_empty: bool,
    charset: &'static str,
    continue_on_error: bool,
    disable_indexes: bool,
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
            loaded_path: None,
            delimiter: ',',
            has_header: true,
            null_on_empty: true,
            charset: "utf-8",
            continue_on_error: false,
            disable_indexes: false,
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
        let charset = self.charset;
        self.status = Some("Loading…".into());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let read_path = path.clone();
            let read = cx
                .background_spawn(async move {
                    let bytes = std::fs::read(&read_path)?;
                    let encoding =
                        encoding_rs::Encoding::for_label(charset.as_bytes()).unwrap_or(encoding_rs::UTF_8);
                    let (decoded, _, _) = encoding.decode(&bytes);
                    std::io::Result::Ok(decoded.into_owned())
                })
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
                        view.loaded_path = Some(path);
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
        // Continuing past a failed row needs to know exactly which row failed,
        // so each row becomes its own statement instead of one shared batch.
        let batch_size = if self.continue_on_error {
            1
        } else {
            IMPORT_BATCH_SIZE
        };
        let statements = build_insert_statements(
            &self.table,
            self.driver,
            &self.target_columns,
            &parsed.rows,
            &self.mapping,
            self.null_on_empty,
            batch_size,
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
        let continue_on_error = self.continue_on_error;
        let delimiter = self.delimiter;
        let error_file_path = self
            .loaded_path
            .as_ref()
            .map(|path| format!("{path}.errors.txt"));
        let disable_indexes = self
            .disable_indexes
            .then(|| disable_indexes_statements(self.driver, &self.table))
            .flatten();
        self.importing = true;
        self.status = Some("Importing…".into());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            if let Some((pre, _)) = &disable_indexes {
                let task = store.update(cx, |store, cx| {
                    store.execute_query(connection_id, database.clone(), pre.clone(), cx)
                });
                task.await.log_err();
            }

            let mut imported = 0usize;
            let mut failed_rows: Vec<&Vec<String>> = Vec::new();
            let mut failure: Option<String> = None;
            for (index, statement) in statements.into_iter().enumerate() {
                let task = store.update(cx, |store, cx| {
                    store.execute_query(connection_id, database.clone(), statement, cx)
                });
                match task.await {
                    Ok(_) => imported += 1,
                    Err(error) => {
                        if continue_on_error {
                            if let Some(row) = parsed.rows.get(index) {
                                failed_rows.push(row);
                            }
                        } else {
                            failure = Some(error.to_string());
                            break;
                        }
                    }
                }
            }

            if let Some((_, post)) = &disable_indexes {
                let task = store.update(cx, |store, cx| {
                    store.execute_query(connection_id, database.clone(), post.clone(), cx)
                });
                task.await.log_err();
            }

            let mut error_file_write_failed = None;
            if !failed_rows.is_empty()
                && let Some(error_file_path) = &error_file_path
            {
                let lines: Vec<String> = failed_rows
                    .iter()
                    .map(|row| format_error_line(row, delimiter))
                    .collect();
                let contents = lines.join("\n");
                let error_file_path = error_file_path.clone();
                let write = cx
                    .background_spawn(async move { std::fs::write(&error_file_path, contents) })
                    .await;
                if let Err(error) = write {
                    error_file_write_failed = Some(error.to_string());
                }
            }

            this.update(cx, |view, cx| {
                view.importing = false;
                view.status = Some(match failure {
                    Some(error) => format!("Import failed after {imported} row(s): {error}"),
                    None if !failed_rows.is_empty() => {
                        let failed = failed_rows.len();
                        match (&error_file_path, error_file_write_failed) {
                            (Some(path), None) => format!(
                                "Imported {imported} rows, {failed} failed (see {path})."
                            ),
                            (_, Some(write_error)) => format!(
                                "Imported {imported} rows, {failed} failed (could not write error file: {write_error})."
                            ),
                            (None, None) => format!("Imported {imported} rows, {failed} failed."),
                        }
                    }
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

    fn render_charset_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.charset;
        let view = cx.entity().downgrade();

        PopoverMenu::new("import-charset")
            .trigger(
                Button::new("import-charset-trigger", current)
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small),
            )
            .menu(move |window, cx| {
                let view = view.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for charset in IMPORT_CHARSETS {
                        let view = view.clone();
                        menu = menu.entry(*charset, None, move |_, cx| {
                            view.update(cx, |this, cx| {
                                this.charset = charset;
                                cx.notify();
                            })
                            .ok();
                        });
                    }
                    menu
                }))
            })
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

impl EventEmitter<DismissEvent> for ImportDataView {}

impl ModalView for ImportDataView {}

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
        let continue_on_error = self.continue_on_error;
        let disable_indexes = self.disable_indexes;
        let importing = self.importing;
        let charset_picker = self.render_charset_picker(cx).into_any_element();

        v_flex()
            .key_context("DataImport")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(px(640.0))
            .max_h(px(560.0))
            .p_4()
            .gap_3()
            .child(crate::widgets::dialog_header(
                format!("Import data into {}", self.table),
                "close-import",
                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
            ))
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
                    .child(Label::new("Insert empty as NULL").size(LabelSize::Small))
                    .child(Label::new("Charset").size(LabelSize::Small).color(Color::Muted))
                    .child(charset_picker),
            )
            .child(
                h_flex()
                    .gap_4()
                    .child(
                        Checkbox::new("continue-on-error", continue_on_error.into()).on_click(
                            cx.listener(|view, _, _, cx| {
                                view.continue_on_error = !view.continue_on_error;
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        Label::new("Continue on error, write failures to file")
                            .size(LabelSize::Small),
                    )
                    .child(
                        Checkbox::new("disable-indexes", disable_indexes.into()).on_click(
                            cx.listener(|view, _, _, cx| {
                                view.disable_indexes = !view.disable_indexes;
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        Label::new("Disable indexes/triggers during load").size(LabelSize::Small),
                    ),
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
                            cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                        ),
                    )
                    .child(
                        ui::Button::new("run-import", "Import")
                            .style(ButtonStyle::Filled)
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

    #[test]
    fn disable_indexes_statements_covers_mysql_and_postgres_only() {
        let (pre, post) = disable_indexes_statements(DatabaseDriver::MySQL, "people")
            .expect("MySQL should offer a disable-keys pair");
        assert_eq!(pre, "ALTER TABLE `people` DISABLE KEYS");
        assert_eq!(post, "ALTER TABLE `people` ENABLE KEYS");

        let (pre, post) = disable_indexes_statements(DatabaseDriver::PostgreSQL, "people")
            .expect("Postgres should offer a session_replication_role pair");
        assert_eq!(pre, "SET session_replication_role = replica");
        assert_eq!(post, "SET session_replication_role = DEFAULT");

        assert!(disable_indexes_statements(DatabaseDriver::SQLite, "people").is_none());
        assert!(disable_indexes_statements(DatabaseDriver::ClickHouse, "people").is_none());
        assert!(disable_indexes_statements(DatabaseDriver::Redis, "people").is_none());
    }

    #[test]
    fn format_error_line_joins_cells_with_the_delimiter() {
        let row = vec!["2".to_string(), "BAD".to_string()];
        assert_eq!(format_error_line(&row, ','), "2,BAD");
        assert_eq!(format_error_line(&row, '\t'), "2\tBAD");
    }

    struct RecordingProvider {
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        fail_containing: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl db_client::provider::DbProvider for RecordingProvider {
        async fn ping(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> anyhow::Result<Vec<db_client::schema::DatabaseInfo>> {
            Ok(Vec::new())
        }
        async fn list_tables(
            &self,
            _database: &str,
        ) -> anyhow::Result<Vec<db_client::schema::TableInfo>> {
            Ok(Vec::new())
        }
        async fn describe_table(
            &self,
            _database: &str,
            _table: &str,
        ) -> anyhow::Result<Vec<db_client::schema::ColumnInfo>> {
            Ok(Vec::new())
        }
        async fn execute_query(
            &self,
            _database: &str,
            sql: &str,
        ) -> anyhow::Result<db_client::schema::QueryResult> {
            self.log.lock().unwrap().push(sql.to_string());
            if let Some(needle) = self.fail_containing
                && sql.contains(needle)
            {
                return Err(anyhow::anyhow!("simulated row failure"));
            }
            Ok(db_client::schema::QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected: 0,
                execution_time_ms: 0,
            })
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    fn test_import_view(
        cx: &mut gpui::TestAppContext,
        provider: RecordingProvider,
    ) -> (
        gpui::WindowHandle<ImportDataView>,
        Entity<DatabaseStore>,
        ConnectionId,
    ) {
        let connection_id = uuid::Uuid::new_v4();
        let store = cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);

            let store = cx.new(DatabaseStore::new);
            store.update(cx, |store, cx| {
                let mut config = db_client::ConnectionConfig::default();
                config.id = connection_id;
                store.add_connected_for_test(config, std::sync::Arc::new(provider), cx);
            });
            store
        });
        let window = cx.add_window({
            let store = store.clone();
            move |window, cx| {
                ImportDataView::new(
                    store,
                    connection_id,
                    "public".to_string(),
                    "people".to_string(),
                    DatabaseDriver::MySQL,
                    vec!["id".to_string(), "name".to_string()],
                    window,
                    cx,
                )
            }
        });
        (window, store, connection_id)
    }

    #[gpui::test]
    async fn import_continues_past_a_failed_row_and_writes_it_to_an_error_file(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, _store, _connection_id) = test_import_view(
            cx,
            RecordingProvider {
                log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                fail_containing: Some("BAD"),
            },
        );

        let dir = std::env::temp_dir().join(format!("db_client_import_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let csv_path = dir.join("rows.csv");
        std::fs::write(&csv_path, "id,name\n1,Alice\n2,BAD\n3,Claire\n").expect("write fixture csv");
        let csv_path_string = csv_path.to_string_lossy().to_string();

        window
            .update(cx, |view, window, cx| {
                view.path_editor
                    .update(cx, |ed, cx| ed.set_text(csv_path_string.clone(), window, cx));
                view.load_file(window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                view.mapping = vec![Some(0), Some(1)];
                view.continue_on_error = true;
                view.run_import(window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        let status = window
            .read_with(cx, |view, _cx| view.status.clone())
            .unwrap();
        assert_eq!(
            status.as_deref(),
            Some(format!("Imported 2 rows, 1 failed (see {csv_path_string}.errors.txt).").as_str()),
            "real per-row failures should be counted and reported without aborting: {status:?}"
        );

        let error_path = format!("{csv_path_string}.errors.txt");
        let error_contents =
            std::fs::read_to_string(&error_path).expect("the error file should have been written");
        assert_eq!(
            error_contents.trim(),
            "2,BAD",
            "the failed row's raw text must appear in the error file, valid rows must not"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    async fn import_disables_and_reenables_indexes_around_a_mysql_load(
        cx: &mut gpui::TestAppContext,
    ) {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (window, _store, _connection_id) = test_import_view(
            cx,
            RecordingProvider {
                log: log.clone(),
                fail_containing: None,
            },
        );

        let dir = std::env::temp_dir().join(format!("db_client_import_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let csv_path = dir.join("rows.csv");
        std::fs::write(&csv_path, "id,name\n1,Alice\n2,Bob\n").expect("write fixture csv");
        let csv_path_string = csv_path.to_string_lossy().to_string();

        window
            .update(cx, |view, window, cx| {
                view.path_editor
                    .update(cx, |ed, cx| ed.set_text(csv_path_string.clone(), window, cx));
                view.load_file(window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                view.mapping = vec![Some(0), Some(1)];
                view.disable_indexes = true;
                view.run_import(window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        let statements = log.lock().unwrap().clone();
        assert_eq!(
            statements.first().map(String::as_str),
            Some("ALTER TABLE `people` DISABLE KEYS"),
            "the disable statement must run before any row insert: {statements:?}"
        );
        assert_eq!(
            statements.last().map(String::as_str),
            Some("ALTER TABLE `people` ENABLE KEYS"),
            "the re-enable statement must run after every row insert: {statements:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    async fn import_decodes_a_non_utf8_file_using_the_selected_charset(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, _store, _connection_id) = test_import_view(
            cx,
            RecordingProvider {
                log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                fail_containing: None,
            },
        );

        // "Имя" (name, in Cyrillic) encoded as windows-1251, not valid UTF-8.
        let (encoded, _, _) = encoding_rs::WINDOWS_1251.encode("id,Имя\n1,Тест\n");
        let dir = std::env::temp_dir().join(format!("db_client_import_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let csv_path = dir.join("cyrillic.csv");
        std::fs::write(&csv_path, &encoded).expect("write fixture csv");
        let csv_path_string = csv_path.to_string_lossy().to_string();

        window
            .update(cx, |view, window, cx| {
                view.path_editor
                    .update(cx, |ed, cx| ed.set_text(csv_path_string.clone(), window, cx));
                view.charset = "windows-1251";
                view.load_file(window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        let parsed = window.read_with(cx, |view, _cx| view.parsed.clone()).unwrap();
        let parsed = parsed.expect("file should have loaded and parsed");
        assert_eq!(parsed.headers, vec!["id".to_string(), "Имя".to_string()]);
        assert_eq!(parsed.rows, vec![vec!["1".to_string(), "Тест".to_string()]]);

        std::fs::remove_dir_all(&dir).ok();
    }
}
