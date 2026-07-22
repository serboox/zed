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

/// Whether ClickHouse's `LIKE` accepts a column of `data_type` at all.
///
/// The generic blocklist above assumes MySQL/Postgres-style type names
/// (`int`, `bigint`, `float`, ...), which don't match ClickHouse's own
/// naming (`UInt32`/`Int64`/`Float64` don't share the `int`/`float`
/// prefixes) and can't enumerate ClickHouse's composite types (`Array`,
/// `Map`, `Tuple`, `Enum8`/`Enum16`, `IPv4`, `IPv6`, ...) -- every one of
/// which raises `ILLEGAL_TYPE_OF_ARGUMENT` if pushed into `LIKE` (verified
/// against a live ClickHouse 24.10 instance). Rather than chase an
/// ever-growing non-text blocklist, this checks the inverse: whether the
/// type resolves down to a text type at all, unwrapping the `Nullable(...)`
/// and `LowCardinality(...)` wrappers ClickHouse allows around any base
/// type (including around each other, e.g. `LowCardinality(Nullable(String))`).
fn is_searchable_clickhouse_column_type(data_type: &str) -> bool {
    let mut remaining = data_type.trim().to_ascii_lowercase();
    loop {
        // Only unwrap when the wrapper's own closing paren is actually
        // present -- a malformed type string like `Nullable(String` (no
        // closing paren) must not be treated as if it unwrapped to `string`,
        // it must fall through to the non-text default below instead.
        let unwrapped = remaining
            .strip_prefix("nullable(")
            .or_else(|| remaining.strip_prefix("lowcardinality("))
            .and_then(|inner| inner.strip_suffix(')'));
        match unwrapped {
            Some(inner) => remaining = inner.to_string(),
            None => break,
        }
    }
    remaining == "string" || remaining.starts_with("fixedstring(")
}

/// Whether `data_type` is worth `LIKE`-searching for `driver`, dispatching to
/// the ClickHouse-specific allowlist above where the generic blocklist
/// doesn't fit (see `is_searchable_clickhouse_column_type`).
fn is_searchable_for_driver(driver: DatabaseDriver, data_type: &str) -> bool {
    if driver == DatabaseDriver::ClickHouse {
        is_searchable_clickhouse_column_type(data_type)
    } else {
        is_searchable_column_type(data_type)
    }
}

/// How large a window `build_search_query`'s Cassandra branch scans (as a
/// multiple of the per-table row cap) before filtering client-side, since
/// there's no server-side substring filter to rely on there.
const CASSANDRA_SCAN_MULTIPLIER: usize = 25;

/// Absolute ceiling on the Cassandra scan window, independent of the row
/// cap, so a large cap can't turn one search into an unbounded table scan.
const CASSANDRA_MAX_SCAN: usize = 2000;

/// The query to run for one table, plus an optional term to additionally
/// filter the fetched rows by in the client. `client_filter_term` is set
/// only for drivers whose query language can't safely push a free-text
/// filter into the query itself (see the Cassandra branch below).
pub struct SearchQuery {
    pub sql: String,
    pub client_filter_term: Option<String>,
}

/// Builds the query to run against `table`'s searchable columns for `term`.
/// Returns `None` when `table` has no searchable columns at all.
///
/// For most drivers this is `SELECT * FROM table WHERE col1 LIKE '%term%' OR
/// col2 LIKE '%term%' ... LIMIT cap`, quoting identifiers for `driver` and
/// escaping `%`/`_`/`'` in `term` so it's always matched literally.
///
/// Cassandra/Scylla get a different query entirely: CQL's `LIKE` only works
/// against a column with a SASI/SAI text index, which can't be assumed to
/// exist, and `ALLOW FILTERING` alone does not enable substring matching on
/// an unindexed column — so pushing a `LIKE ... OR ...` clause into CQL
/// would fail at execution for most real tables. Instead this scans a
/// bounded window of the table with no `WHERE` clause at all (always valid
/// CQL) and returns `client_filter_term` so the caller matches the term
/// against the fetched rows itself.
pub fn build_search_query(
    driver: DatabaseDriver,
    table: &str,
    columns: &[ColumnInfo],
    term: &str,
    cap: usize,
) -> Option<SearchQuery> {
    let searchable: Vec<&ColumnInfo> = columns
        .iter()
        .filter(|column| is_searchable_for_driver(driver, &column.data_type))
        .collect();
    if searchable.is_empty() {
        return None;
    }
    let quoted_table = driver.quote_identifier(table);
    if driver == DatabaseDriver::Cassandra {
        let scan_cap = cap
            .saturating_mul(CASSANDRA_SCAN_MULTIPLIER)
            .min(CASSANDRA_MAX_SCAN);
        return Some(SearchQuery {
            sql: format!("SELECT * FROM {quoted_table} LIMIT {scan_cap} ALLOW FILTERING;"),
            client_filter_term: Some(term.to_string()),
        });
    }
    let escaped_term = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
        .replace('\'', "''");
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
    Some(SearchQuery {
        sql: format!(
            "SELECT * FROM {quoted_table} WHERE {} LIMIT {cap}",
            conditions.join(" OR ")
        ),
        client_filter_term: None,
    })
}

/// Keeps only the rows where at least one of `searchable_columns` contains
/// `term` (case-insensitive), then caps the result to `cap` rows. Used for
/// drivers (Cassandra) whose query can't filter by the term itself.
fn filter_rows_by_term(
    result: &mut QueryResult,
    term: &str,
    searchable_columns: &[String],
    cap: usize,
) {
    let term_lower = term.to_lowercase();
    let matching_indices: Vec<usize> = result
        .columns
        .iter()
        .enumerate()
        .filter(|(_, name)| searchable_columns.contains(name))
        .map(|(index, _)| index)
        .collect();
    result.rows.retain(|row| {
        matching_indices.iter().any(|&index| {
            row.get(index)
                .and_then(|value| value.as_deref())
                .is_some_and(|value| value.to_lowercase().contains(&term_lower))
        })
    });
    result.rows.truncate(cap);
}

/// One table's hits: the exact query that found them, so opening the hit in a
/// full grid re-runs the identical filter rather than re-deriving it.
/// `client_filter_term`/`searchable_columns` are set alongside `sql` when the
/// hits were narrowed down client-side (see `build_search_query`), so
/// re-opening the hit can reapply the same filter to the freshly fetched rows.
pub struct TableSearchResult {
    pub table: String,
    pub sql: String,
    pub result: QueryResult,
    pub client_filter_term: Option<String>,
    pub searchable_columns: Vec<String>,
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
                let searchable_columns: Vec<String> = columns
                    .iter()
                    .filter(|column| is_searchable_for_driver(driver, &column.data_type))
                    .map(|column| column.name.clone())
                    .collect();

                let query =
                    build_search_query(driver, &table, &columns, &term, SEARCH_ROW_CAP_PER_TABLE);
                let outcome = match &query {
                    Some(query) => {
                        let query_task = store.update(cx, |store, cx| {
                            store.execute_query(
                                connection_id,
                                database.clone(),
                                query.sql.clone(),
                                cx,
                            )
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
                    if let (Some(query), Some(Ok(mut result))) = (query, outcome) {
                        if let Some(term) = &query.client_filter_term {
                            filter_rows_by_term(
                                &mut result,
                                term,
                                &searchable_columns,
                                SEARCH_ROW_CAP_PER_TABLE,
                            );
                        }
                        if !result.rows.is_empty() {
                            view.results.push(TableSearchResult {
                                table,
                                sql: query.sql,
                                result,
                                client_filter_term: query.client_filter_term,
                                searchable_columns,
                            });
                        }
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
        let client_filter_term = hit.client_filter_term.clone();
        let searchable_columns = hit.searchable_columns.clone();
        let title = SharedString::from(hit.table.clone());
        let task = self.store.update(cx, |store, cx| {
            store.execute_query(connection_id, database, sql, cx)
        });
        let store_weak = self.store.downgrade();
        let env_color = crate::panel::connection_env_color(&store_weak, connection_id, cx);
        let result_view = cx.new(|cx| ResultView::new(title, cx).with_env_color(env_color));
        let rv = result_view.clone();
        let workspace = self.workspace.clone();
        window
            .spawn(cx, async move |cx| {
                let outcome = task.await;
                rv.update(cx, |view, cx| match outcome {
                    Ok(mut result) => {
                        if let Some(term) = &client_filter_term {
                            filter_rows_by_term(
                                &mut result,
                                term,
                                &searchable_columns,
                                SEARCH_ROW_CAP_PER_TABLE,
                            );
                        }
                        view.set_result(result, cx)
                    }
                    Err(err) => view.set_error(format_query_error(&err), cx),
                });
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.add_item_to_active_pane(
                            Box::new(result_view),
                            None,
                            true,
                            window,
                            cx,
                        );
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
                        Label::new(format!(
                            "{} row{}",
                            hit.result.rows.len(),
                            if hit.result.rows.len() == 1 { "" } else { "s" }
                        ))
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
                    .child(
                        Label::new(format!("Search in {}", self.connection_label))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        crate::widgets::text_field(&self.term_editor, cx)
                            .flex_1()
                            .debug_selector(|| "SEARCH_TERM_EDITOR".to_string()),
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
                el.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            Icon::new(IconName::Warning)
                                .color(Color::Warning)
                                .size(IconSize::Small),
                        )
                        .child(
                            Label::new(status)
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        ),
                )
            })
            .when_some(progress, |el, progress| {
                el.child(
                    div()
                        .debug_selector(|| "SEARCH_PROGRESS".to_string())
                        .child(
                            Label::new(progress)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
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

    // Before this fix, ClickHouse fell through to the generic blocklist,
    // which excludes types by an MySQL/Postgres-style `int`/`float`/...
    // prefix that ClickHouse's own type names (`UInt32`, `Enum8`, ...)
    // don't share, and has no entry at all for ClickHouse's composite types.
    // Verified against a live ClickHouse 24.10 instance: `LIKE` against any
    // of these raises `ILLEGAL_TYPE_OF_ARGUMENT`, including the plain
    // `UInt32` id column nearly every ClickHouse table has.
    #[test]
    fn clickhouse_column_type_allowlist_accepts_only_text_shaped_types() {
        assert!(is_searchable_clickhouse_column_type("String"));
        assert!(is_searchable_clickhouse_column_type("FixedString(10)"));
        assert!(is_searchable_clickhouse_column_type("Nullable(String)"));
        assert!(is_searchable_clickhouse_column_type(
            "LowCardinality(String)"
        ));
        assert!(is_searchable_clickhouse_column_type(
            "LowCardinality(Nullable(String))"
        ));
        assert!(is_searchable_clickhouse_column_type(
            "Nullable(FixedString(5))"
        ));

        assert!(!is_searchable_clickhouse_column_type("UInt32"));
        assert!(!is_searchable_clickhouse_column_type("Int64"));
        assert!(!is_searchable_clickhouse_column_type("Nullable(UInt32)"));
        assert!(!is_searchable_clickhouse_column_type("Array(String)"));
        assert!(!is_searchable_clickhouse_column_type("Map(String, String)"));
        assert!(!is_searchable_clickhouse_column_type(
            "Tuple(String, UInt32)"
        ));
        assert!(!is_searchable_clickhouse_column_type(
            "Enum8('active' = 1, 'inactive' = 2)"
        ));
        assert!(!is_searchable_clickhouse_column_type("IPv4"));
        assert!(!is_searchable_clickhouse_column_type("IPv6"));
        assert!(!is_searchable_clickhouse_column_type("DateTime"));
        assert!(!is_searchable_clickhouse_column_type("UUID"));
    }

    // A malformed type string with an unclosed wrapper must never be
    // classified as searchable just because stripping the prefix alone
    // happens to leave a string equal to `string` -- the wrapper's own
    // closing paren must also be present before the layer is peeled off.
    #[test]
    fn clickhouse_column_type_allowlist_rejects_a_malformed_unclosed_wrapper() {
        assert!(!is_searchable_clickhouse_column_type("Nullable(String"));
        assert!(!is_searchable_clickhouse_column_type(
            "LowCardinality(String"
        ));
        assert!(!is_searchable_clickhouse_column_type("Nullable(String))"));
    }

    #[test]
    fn build_search_query_for_clickhouse_only_searches_text_shaped_columns() {
        let columns = vec![
            column("id", "UInt32"),
            column("amount", "Nullable(UInt32)"),
            column("tags", "Array(String)"),
            column("name", "String"),
            column("note", "Nullable(String)"),
        ];
        let query = build_search_query(DatabaseDriver::ClickHouse, "events", &columns, "x", 20)
            .expect("String/Nullable(String) columns exist so a query must be built");
        assert!(query.sql.contains("\"name\" LIKE"));
        assert!(query.sql.contains("\"note\" LIKE"));
        assert!(!query.sql.contains("\"id\""));
        assert!(!query.sql.contains("\"amount\""));
        assert!(!query.sql.contains("\"tags\""));
    }

    #[test]
    fn build_search_query_for_clickhouse_returns_none_when_only_non_text_columns_exist() {
        let columns = vec![column("id", "UInt32"), column("created", "DateTime")];
        assert!(
            build_search_query(DatabaseDriver::ClickHouse, "events", &columns, "x", 20).is_none()
        );
    }

    #[test]
    fn build_search_query_quotes_identifiers_and_escapes_the_term_per_driver() {
        let columns = vec![column("name", "varchar(255)"), column("age", "int")];
        let query =
            build_search_query(DatabaseDriver::PostgreSQL, "users", &columns, "O'Brien", 20)
                .expect("a text column exists so a query must be built");
        assert_eq!(
            query.sql,
            "SELECT * FROM \"users\" WHERE \"name\" LIKE '%O''Brien%' LIMIT 20"
        );
        assert!(query.client_filter_term.is_none());

        let query = build_search_query(DatabaseDriver::MySQL, "users", &columns, "50%", 20)
            .expect("a text column exists so a query must be built");
        assert_eq!(
            query.sql,
            "SELECT * FROM `users` WHERE `name` LIKE '%50\\%%' LIMIT 20"
        );
    }

    #[test]
    fn build_search_query_returns_none_when_no_column_is_searchable() {
        let columns = vec![column("id", "int"), column("created_at", "timestamp")];
        assert!(
            build_search_query(DatabaseDriver::MySQL, "events", &columns, "term", 20).is_none()
        );
    }

    #[test]
    fn build_search_query_respects_the_row_cap() {
        let columns = vec![column("name", "text")];
        let query = build_search_query(DatabaseDriver::SQLite, "items", &columns, "x", 7).unwrap();
        assert!(query.sql.ends_with("LIMIT 7"));
    }

    // Before this fix, Cassandra reused the generic `LIKE ... OR ...` branch,
    // which CQL rejects outright without a SASI/SAI text index on every
    // searched column -- an index this code can't assume exists. The fix
    // must never emit `LIKE` or `OR` for Cassandra, and must ask the caller
    // to filter client-side instead.
    #[test]
    fn build_search_query_for_cassandra_never_emits_like_or_or_and_asks_for_client_filtering() {
        let columns = vec![column("name", "text"), column("bio", "text")];
        let query = build_search_query(DatabaseDriver::Cassandra, "users", &columns, "term", 20)
            .expect("a text column exists so a query must be built");
        assert!(!query.sql.contains("LIKE"));
        assert!(!query.sql.contains(" OR "));
        assert!(query.sql.contains("ALLOW FILTERING"));
        assert_eq!(query.client_filter_term.as_deref(), Some("term"));
    }

    #[test]
    fn build_search_query_for_cassandra_scans_a_bounded_window_larger_than_the_cap() {
        let columns = vec![column("name", "text")];
        let query =
            build_search_query(DatabaseDriver::Cassandra, "users", &columns, "term", 20).unwrap();
        assert_eq!(
            query.sql,
            "SELECT * FROM \"users\" LIMIT 500 ALLOW FILTERING;"
        );
    }

    fn result_with_rows(columns: &[&str], rows: Vec<Vec<Option<&str>>>) -> QueryResult {
        QueryResult {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(|v| v.map(str::to_string)).collect())
                .collect(),
            rows_affected: 0,
            execution_time_ms: 0,
        }
    }

    #[test]
    fn filter_rows_by_term_keeps_only_rows_matching_a_searchable_column_case_insensitively() {
        let mut result = result_with_rows(
            &["id", "name"],
            vec![
                vec![Some("1"), Some("Alice")],
                vec![Some("2"), Some("Bob")],
                vec![Some("3"), Some("ALICIA")],
            ],
        );
        filter_rows_by_term(&mut result, "alic", &["name".to_string()], 20);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][1], Some("Alice".to_string()));
        assert_eq!(result.rows[1][1], Some("ALICIA".to_string()));
    }

    #[test]
    fn filter_rows_by_term_ignores_non_searchable_columns() {
        let mut result =
            result_with_rows(&["id", "name"], vec![vec![Some("alic-999"), Some("Bob")]]);
        filter_rows_by_term(&mut result, "alic", &["name".to_string()], 20);
        assert!(result.rows.is_empty());
    }

    #[test]
    fn filter_rows_by_term_respects_the_cap() {
        let mut result = result_with_rows(
            &["name"],
            vec![
                vec![Some("match one")],
                vec![Some("match two")],
                vec![Some("match three")],
            ],
        );
        filter_rows_by_term(&mut result, "match", &["name".to_string()], 2);
        assert_eq!(result.rows.len(), 2);
    }
}
