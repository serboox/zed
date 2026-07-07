use db_client::schema::{ColumnInfo, QueryResult};
use db_client::{ConnectionId, DatabaseDriver};
use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    WeakEntity, Window, prelude::*, px,
};
use ui::prelude::*;
use ui::{Button, ButtonStyle, Icon, IconName, Label, LabelSize};
use util::ResultExt;
use workspace::{Item, Workspace, item::ItemEvent};

use crate::result_view::{ResultView, format_query_error};
use crate::store::DatabaseStore;

/// Upper bound on rows fetched per table, so a term that matches almost every
/// row of a huge table can't turn one search into an unbounded scan.
pub const SEARCH_ROW_CAP_PER_TABLE: usize = 20;

/// Upper bound on how many of a database's tables a single search scans, so a
/// schema with thousands of tables can't turn one search into a runaway task.
pub const SEARCH_TABLE_LIMIT: usize = 200;

/// Whether a column's SQL type is worth `LIKE`-searching. Numeric, boolean,
/// date/time, and binary types are excluded: `LIKE '%term%'` against them
/// either errors or never matches, depending on the driver.
pub fn is_searchable_column_type(data_type: &str) -> bool {
    let lower = data_type.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    const EXCLUDED_PREFIXES: &[&str] = &[
        "int",
        "bigint",
        "smallint",
        "mediumint",
        "tinyint",
        "decimal",
        "float",
        "double",
        "numeric",
        "real",
        "number",
        "integer",
        "bool",
        "boolean",
        "bit",
        "date",
        "time",
        "timestamp",
        "year",
        "blob",
        "binary",
        "varbinary",
        "json",
        "uuid",
    ];
    !EXCLUDED_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// Builds a `SELECT * FROM table WHERE col1 LIKE '%term%' OR col2 LIKE
/// '%term%' ... LIMIT cap` query over `table`'s searchable columns, quoting
/// identifiers for `driver` and escaping `%`/`_`/`'` in `term` so the search
/// term is always matched literally. Returns `None` when `table` has no
/// searchable columns at all.
pub fn build_search_sql(
    driver: DatabaseDriver,
    table: &str,
    columns: &[ColumnInfo],
    term: &str,
    cap: usize,
) -> Option<String> {
    let searchable: Vec<&ColumnInfo> = columns
        .iter()
        .filter(|column| is_searchable_column_type(&column.data_type))
        .collect();
    if searchable.is_empty() {
        return None;
    }
    let escaped_term = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
        .replace('\'', "''");
    let quoted_table = driver.quote_identifier(table);
    let conditions: Vec<String> = searchable
        .iter()
        .map(|column| {
            format!(
                "{} LIKE '%{}%'",
                driver.quote_identifier(&column.name),
                escaped_term
            )
        })
        .collect();
    Some(format!(
        "SELECT * FROM {quoted_table} WHERE {} LIMIT {cap}",
        conditions.join(" OR ")
    ))
}

/// One table's hits: the exact query that found them, so opening the hit in a
/// full grid re-runs the identical filter rather than re-deriving it.
pub struct TableSearchResult {
    pub table: String,
    pub sql: String,
    pub result: QueryResult,
}

/// A workspace tab that searches every text-like column of every table in a
/// database for a term, showing per-table hits. Runs one query per table,
/// sequentially, so results stream in and a mid-search close/generation bump
/// stops the remaining tables from ever being queried.
pub struct FullTextSearchView {
    focus_handle: FocusHandle,
    store: Entity<DatabaseStore>,
    workspace: WeakEntity<Workspace>,
    connection_id: ConnectionId,
    connection_label: SharedString,
    database: String,
    driver: DatabaseDriver,
    tables: Vec<String>,
    term_editor: Entity<Editor>,
    results: Vec<TableSearchResult>,
    tables_scanned: usize,
    is_running: bool,
    generation: usize,
    status: Option<SharedString>,
}

impl FullTextSearchView {
    pub fn new(
        store: Entity<DatabaseStore>,
        workspace: WeakEntity<Workspace>,
        connection_id: ConnectionId,
        connection_label: SharedString,
        database: String,
        driver: DatabaseDriver,
        tables: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let term_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search text across all tables…", window, cx);
            editor
        });
        Self {
            focus_handle: cx.focus_handle(),
            store,
            workspace,
            connection_id,
            connection_label,
            database,
            driver,
            tables: tables.into_iter().take(SEARCH_TABLE_LIMIT).collect(),
            term_editor,
            results: Vec::new(),
            tables_scanned: 0,
            is_running: false,
            generation: 0,
            status: None,
        }
    }

    fn search_term(&self, cx: &App) -> String {
        self.term_editor.read(cx).text(cx).trim().to_string()
    }

    pub fn start_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let term = self.search_term(cx);
        if term.is_empty() {
            self.status = Some("Enter a search term.".into());
            cx.notify();
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.results.clear();
        self.tables_scanned = 0;
        self.is_running = true;
        self.status = None;
        cx.notify();

        let tables = self.tables.clone();
        let store = self.store.clone();
        let connection_id = self.connection_id;
        let database = self.database.clone();
        let driver = self.driver;

        cx.spawn_in(window, async move |this, cx| {
            for table in tables {
                let is_current = this
                    .update(cx, |view, _| view.generation == generation)
                    .unwrap_or(false);
                if !is_current {
                    return;
                }

                let describe_task = store.update(cx, |store, cx| {
                    store.describe_table(connection_id, database.clone(), table.clone(), cx)
                });
                let columns = describe_task.await.log_err().unwrap_or_default();

                let sql = build_search_sql(driver, &table, &columns, &term, SEARCH_ROW_CAP_PER_TABLE);
                let outcome = match &sql {
                    Some(sql) => {
                        let query_task = store.update(cx, |store, cx| {
                            store.execute_query(connection_id, database.clone(), sql.clone(), cx)
                        });
                        Some(query_task.await)
                    }
                    None => None,
                };

                this.update(cx, |view, cx| {
                    if view.generation != generation {
                        return;
                    }
                    view.tables_scanned += 1;
                    if let (Some(sql), Some(Ok(result))) = (sql, outcome)
                        && !result.rows.is_empty()
                    {
                        view.results.push(TableSearchResult {
                            table,
                            sql,
                            result,
                        });
                    }
                    cx.notify();
                })
                .ok();
            }
            this.update(cx, |view, cx| {
                if view.generation == generation {
                    view.is_running = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn open_hit(&self, hit_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.results.get(hit_index) else {
            return;
        };
        let connection_id = self.connection_id;
        let database = self.database.clone();
        let sql = hit.sql.clone();
        let title = SharedString::from(hit.table.clone());
        let task = self
            .store
            .update(cx, |store, cx| store.execute_query(connection_id, database, sql, cx));
        let result_view = cx.new(|cx| ResultView::new(title, cx));
        let rv = result_view.clone();
        let workspace = self.workspace.clone();
        window
            .spawn(cx, async move |cx| {
                let outcome = task.await;
                rv.update(cx, |view, cx| match outcome {
                    Ok(result) => view.set_result(result, cx),
                    Err(err) => view.set_error(format_query_error(&err), cx),
                });
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.add_item_to_active_pane(Box::new(result_view), None, true, window, cx);
                    })
                    .log_err();
            })
            .detach();
    }
}

impl EventEmitter<DismissEvent> for FullTextSearchView {}

impl Item for FullTextSearchView {
    type Event = DismissEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Search: {}", self.database).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::MagnifyingGlass))
    }

    fn to_item_events(_event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(ItemEvent::CloseItem);
    }
}

impl Focusable for FullTextSearchView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FullTextSearchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let total_tables = self.tables.len();
        let progress = if self.is_running || self.tables_scanned > 0 {
            Some(format!(
                "Scanned {} of {} table{}",
                self.tables_scanned,
                total_tables,
                if total_tables == 1 { "" } else { "s" },
            ))
        } else {
            None
        };

        let results = self
            .results
            .iter()
            .enumerate()
            .map(|(index, hit)| {
                h_flex()
                    .id(("full-text-search-hit", index))
                    .debug_selector(move || format!("SEARCH_HIT-{}", hit.table))
                    .w_full()
                    .justify_between()
                    .items_center()
                    .px_2()
                    .py_1()
                    .child(Label::new(hit.table.clone()).size(LabelSize::Default))
                    .child(
                        Label::new(format!("{} row{}", hit.result.rows.len(), if hit.result.rows.len() == 1 { "" } else { "s" }))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new(("full-text-search-open", index), "Open")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.open_hit(index, window, cx);
                            })),
                    )
            })
            .collect::<Vec<_>>();

        v_flex()
            .id("full-text-search-view")
            .size_full()
            .p_2()
            .gap_2()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Label::new(format!("Search in {}", self.connection_label)).size(LabelSize::Small).color(Color::Muted))
                    .child(
                        div()
                            .flex_1()
                            .debug_selector(|| "SEARCH_TERM_EDITOR".to_string())
                            .child(self.term_editor.clone()),
                    )
                    .child(
                        Button::new("full-text-search-run", "Search")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.start_search(window, cx);
                            })),
                    ),
            )
            .when_some(self.status.clone(), |el, status| {
                el.child(Label::new(status).size(LabelSize::Small).color(Color::Error))
            })
            .when_some(progress, |el, progress| {
                el.child(
                    div()
                        .debug_selector(|| "SEARCH_PROGRESS".to_string())
                        .child(Label::new(progress).size(LabelSize::Small).color(Color::Muted)),
                )
            })
            .child(
                v_flex()
                    .id("full-text-search-results")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(results),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use db_client::schema::ColumnInfo;

    fn column(name: &str, data_type: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            data_type: data_type.to_string(),
            is_nullable: true,
            column_key: None,
            default_value: None,
            extra: String::new(),
        }
    }

    #[test]
    fn searchable_column_type_excludes_numeric_date_boolean_and_binary() {
        assert!(is_searchable_column_type("varchar(255)"));
        assert!(is_searchable_column_type("text"));
        assert!(is_searchable_column_type("char(10)"));
        assert!(is_searchable_column_type("TEXT"));
        assert!(!is_searchable_column_type("int"));
        assert!(!is_searchable_column_type("bigint unsigned"));
        assert!(!is_searchable_column_type("decimal(10,2)"));
        assert!(!is_searchable_column_type("boolean"));
        assert!(!is_searchable_column_type("date"));
        assert!(!is_searchable_column_type("datetime"));
        assert!(!is_searchable_column_type("timestamp"));
        assert!(!is_searchable_column_type("blob"));
        assert!(!is_searchable_column_type("varbinary(16)"));
        assert!(!is_searchable_column_type(""));
    }

    #[test]
    fn build_search_sql_quotes_identifiers_and_escapes_the_term_per_driver() {
        let columns = vec![column("name", "varchar(255)"), column("age", "int")];
        let sql = build_search_sql(DatabaseDriver::PostgreSQL, "users", &columns, "O'Brien", 20)
            .expect("a text column exists so a query must be built");
        assert_eq!(
            sql,
            "SELECT * FROM \"users\" WHERE \"name\" LIKE '%O''Brien%' LIMIT 20"
        );

        let sql = build_search_sql(DatabaseDriver::MySQL, "users", &columns, "50%", 20)
            .expect("a text column exists so a query must be built");
        assert_eq!(
            sql,
            "SELECT * FROM `users` WHERE `name` LIKE '%50\\%%' LIMIT 20"
        );
    }

    #[test]
    fn build_search_sql_returns_none_when_no_column_is_searchable() {
        let columns = vec![column("id", "int"), column("created_at", "timestamp")];
        assert!(build_search_sql(DatabaseDriver::MySQL, "events", &columns, "term", 20).is_none());
    }

    #[test]
    fn build_search_sql_respects_the_row_cap() {
        let columns = vec![column("name", "text")];
        let sql = build_search_sql(DatabaseDriver::SQLite, "items", &columns, "x", 7).unwrap();
        assert!(sql.ends_with("LIMIT 7"));
    }
}
