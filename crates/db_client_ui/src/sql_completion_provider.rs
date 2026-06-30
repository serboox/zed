use crate::store::DatabaseStore;
use db_client::schema::TableKind;
use db_client::{ConnectionId, DatabaseDriver};
use editor::{CompletionContext, CompletionProvider, Editor};
use gpui::{App, Context, Entity, Task, WeakEntity, Window};
use language::{Anchor, Buffer, CodeLabel, ToOffset};
use project::lsp_store::CompletionDocumentation;
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use util::ResultExt as _;

const CORE_KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "FULL", "CROSS", "ON",
    "USING", "AND", "OR", "NOT", "IN", "IS", "NULL", "AS", "DISTINCT", "ORDER", "GROUP", "BY",
    "HAVING", "LIMIT", "OFFSET", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE",
    "TABLE", "VIEW", "INDEX", "DROP", "ALTER", "ADD", "COLUMN", "PRIMARY", "KEY", "UNIQUE",
    "REFERENCES", "FOREIGN", "CONSTRAINT", "DEFAULT", "SHOW", "DESCRIBE", "EXPLAIN", "USE", "LIKE",
    "BETWEEN", "EXISTS", "UNION", "ALL", "CASE", "WHEN", "THEN", "ELSE", "END", "CAST", "ASC",
    "DESC", "WITH", "TRUNCATE",
];

const CORE_FUNCTIONS: &[&str] = &[
    "COUNT", "SUM", "AVG", "MIN", "MAX", "COALESCE", "NULLIF", "ABS", "ROUND", "FLOOR", "CEIL",
    "LENGTH", "LOWER", "UPPER", "TRIM", "REPLACE", "SUBSTRING", "CONCAT", "NOW",
];

const MYSQL_KEYWORDS: &[&str] = &[
    "AUTO_INCREMENT",
    "ENGINE",
    "UNSIGNED",
    "ZEROFILL",
    "DUPLICATE",
    "IGNORE",
    "REGEXP",
    "STRAIGHT_JOIN",
];
const MYSQL_FUNCTIONS: &[&str] = &[
    "IFNULL",
    "IF",
    "GROUP_CONCAT",
    "DATE_FORMAT",
    "UNIX_TIMESTAMP",
    "JSON_EXTRACT",
    "CURDATE",
];

const POSTGRES_KEYWORDS: &[&str] = &[
    "RETURNING",
    "ILIKE",
    "SIMILAR",
    "LATERAL",
    "ARRAY",
    "SERIAL",
    "BIGSERIAL",
    "JSONB",
    "USING",
    "CONFLICT",
];
const POSTGRES_FUNCTIONS: &[&str] = &[
    "COALESCE",
    "GREATEST",
    "LEAST",
    "JSONB_BUILD_OBJECT",
    "JSON_AGG",
    "ARRAY_AGG",
    "TO_CHAR",
    "GENERATE_SERIES",
];

const SQLITE_KEYWORDS: &[&str] = &[
    "AUTOINCREMENT",
    "WITHOUT",
    "ROWID",
    "PRAGMA",
    "VACUUM",
    "GLOB",
    "ATTACH",
    "DETACH",
];
const SQLITE_FUNCTIONS: &[&str] = &["IFNULL", "TYPEOF", "DATETIME", "STRFTIME", "JSON_EXTRACT"];

const CLICKHOUSE_KEYWORDS: &[&str] = &[
    "ENGINE",
    "PARTITION",
    "PREWHERE",
    "FINAL",
    "SAMPLE",
    "MATERIALIZED",
    "TTL",
    "ARRAY",
];
const CLICKHOUSE_FUNCTIONS: &[&str] = &[
    "toDateTime",
    "toDate",
    "uniq",
    "arrayJoin",
    "groupArray",
    "quantile",
    "countIf",
];

fn keywords_for(driver: DatabaseDriver) -> Vec<&'static str> {
    let extra = match driver {
        DatabaseDriver::MySQL => MYSQL_KEYWORDS,
        DatabaseDriver::PostgreSQL => POSTGRES_KEYWORDS,
        DatabaseDriver::SQLite => SQLITE_KEYWORDS,
        DatabaseDriver::ClickHouse => CLICKHOUSE_KEYWORDS,
        DatabaseDriver::Redis => &[],
    };
    CORE_KEYWORDS.iter().chain(extra.iter()).copied().collect()
}

fn functions_for(driver: DatabaseDriver) -> Vec<&'static str> {
    let extra = match driver {
        DatabaseDriver::MySQL => MYSQL_FUNCTIONS,
        DatabaseDriver::PostgreSQL => POSTGRES_FUNCTIONS,
        DatabaseDriver::SQLite => SQLITE_FUNCTIONS,
        DatabaseDriver::ClickHouse => CLICKHOUSE_FUNCTIONS,
        DatabaseDriver::Redis => &[],
    };
    CORE_FUNCTIONS.iter().chain(extra.iter()).copied().collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextKind {
    Tables,
    Columns,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TableRef {
    name: String,
    alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedContext {
    qualifier: Option<String>,
    kind: ContextKind,
    tables_in_scope: Vec<TableRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Schema,
    Table,
    View,
    Column,
    Trigger,
    Keyword,
    Function,
}

impl ItemKind {
    fn rank(self) -> u8 {
        match self {
            ItemKind::Column => 0,
            ItemKind::Table => 1,
            ItemKind::View => 2,
            ItemKind::Schema => 3,
            ItemKind::Trigger => 4,
            ItemKind::Function => 5,
            ItemKind::Keyword => 6,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ItemKind::Schema => "schema",
            ItemKind::Table => "table",
            ItemKind::View => "view",
            ItemKind::Column => "column",
            ItemKind::Trigger => "trigger",
            ItemKind::Keyword => "keyword",
            ItemKind::Function => "function",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CandidateItem {
    text: String,
    kind: ItemKind,
    detail: Option<String>,
}

/// Owned snapshot of the schema cache for the active connection, so candidate
/// building is a pure function (no GPUI context) and unit-testable.
#[derive(Debug, Default, Clone)]
struct SchemaSnapshot {
    databases: Vec<String>,
    tables: Vec<(String, bool)>,
    columns_by_table: HashMap<String, Vec<(String, String)>>,
    schema_objects: HashMap<String, Vec<(String, bool)>>,
    triggers: Vec<String>,
}

fn is_identifier_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Text of the statement under the cursor: everything after the last `;`.
fn current_statement(text_before: &str) -> &str {
    match text_before.rfind(';') {
        Some(index) => &text_before[index + 1..],
        None => text_before,
    }
}

/// Identifier that sits immediately before a trailing `.` (after stripping the
/// word currently being typed). `from instruments.spl` -> `instruments`.
fn extract_qualifier(text_before: &str) -> Option<String> {
    let before_word = text_before.trim_end_matches(is_identifier_char);
    let before_dot = before_word.strip_suffix('.')?;
    let qualifier: String = before_dot
        .chars()
        .rev()
        .take_while(|c| is_identifier_char(*c))
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if qualifier.is_empty() {
        None
    } else {
        Some(qualifier)
    }
}

fn upper_words(statement: &str) -> Vec<String> {
    statement
        .split(|c: char| !is_identifier_char(c))
        .filter(|w| !w.is_empty())
        .map(|w| w.to_uppercase())
        .collect()
}

/// Whether the cursor expects table-like names or column-like names, based on
/// the most recent significant clause keyword in the current statement.
fn clause_kind(statement_before: &str) -> ContextKind {
    let words = upper_words(statement_before);
    for word in words.iter().rev() {
        match word.as_str() {
            "FROM" | "JOIN" | "INTO" | "UPDATE" | "TABLE" | "DESCRIBE" | "DESC" => {
                return ContextKind::Tables;
            }
            "SELECT" | "WHERE" | "ON" | "SET" | "HAVING" | "USING" | "AND" | "OR" | "ORDER"
            | "GROUP" => {
                return ContextKind::Columns;
            }
            _ => {}
        }
    }
    ContextKind::Tables
}

/// Tables (with optional aliases) referenced in the FROM/JOIN clauses of the
/// current statement, used to resolve `alias.` and to scope column suggestions.
fn parse_table_refs(statement: &str) -> Vec<TableRef> {
    let tokens: Vec<&str> = statement
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .collect();
    let stop = [
        "WHERE", "GROUP", "ORDER", "HAVING", "LIMIT", "OFFSET", "SET", "ON", "USING", "VALUES",
        "SELECT", "UNION",
    ];
    let mut refs = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let keyword = tokens[index].to_uppercase();
        let collecting = keyword == "FROM" || keyword == "JOIN" || keyword == "INTO";
        if !collecting {
            index += 1;
            continue;
        }
        index += 1;
        while index < tokens.len() {
            let raw = tokens[index];
            let upper = raw.to_uppercase();
            if stop.contains(&upper.as_str()) || upper == "JOIN" || upper == "FROM" {
                break;
            }
            let name = strip_qualifier(raw);
            if name.is_empty() {
                index += 1;
                continue;
            }
            let mut table_ref = TableRef {
                name: name.to_string(),
                alias: None,
            };
            // Optional alias: `table alias` or `table AS alias`.
            let mut look = index + 1;
            if look < tokens.len() && tokens[look].eq_ignore_ascii_case("AS") {
                look += 1;
            }
            if look < tokens.len() {
                let candidate = tokens[look];
                let candidate_upper = candidate.to_uppercase();
                let is_keyword = stop.contains(&candidate_upper.as_str())
                    || candidate_upper == "JOIN"
                    || candidate_upper == "INNER"
                    || candidate_upper == "LEFT"
                    || candidate_upper == "RIGHT"
                    || candidate_upper == "FULL"
                    || candidate_upper == "CROSS"
                    || candidate_upper == "OUTER";
                if !is_keyword && candidate.chars().all(is_identifier_char) {
                    table_ref.alias = Some(candidate.to_string());
                    index = look;
                }
            }
            refs.push(table_ref);
            index += 1;
        }
    }
    refs
}

/// Drops a `schema.` prefix from a qualified name, keeping the final segment.
fn strip_qualifier(raw: &str) -> &str {
    raw.rsplit('.')
        .next()
        .unwrap_or(raw)
        .trim_matches(|c: char| c == '`' || c == '"' || c == '(' || c == ')')
}

fn parse_context(text_before: &str) -> ParsedContext {
    let statement = current_statement(text_before);
    ParsedContext {
        qualifier: extract_qualifier(text_before),
        kind: clause_kind(statement),
        tables_in_scope: parse_table_refs(statement),
    }
}

/// Resolves a qualifier to a table name: an alias maps to its table, otherwise
/// a direct table-name match maps to itself.
fn resolve_table(qualifier: &str, tables: &[TableRef]) -> Option<String> {
    if let Some(found) = tables
        .iter()
        .find(|t| t.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(qualifier)))
    {
        return Some(found.name.clone());
    }
    tables
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(qualifier))
        .map(|t| t.name.clone())
}

/// Pure candidate builder: turns a schema snapshot + parsed context + dialect
/// into the ordered list of suggestions, before any prefix filtering (the
/// editor filters interactively).
fn build_candidates(
    context: &ParsedContext,
    schema: &SchemaSnapshot,
    driver: DatabaseDriver,
) -> Vec<CandidateItem> {
    let mut items: Vec<CandidateItem> = Vec::new();

    if let Some(qualifier) = &context.qualifier {
        // `schema.` -> the objects of that schema.
        if let Some(objects) = schema.schema_objects.get(qualifier) {
            for (name, is_view) in objects {
                items.push(CandidateItem {
                    text: name.clone(),
                    kind: if *is_view { ItemKind::View } else { ItemKind::Table },
                    detail: None,
                });
            }
        }
        // `alias.`/`table.` -> that table's columns.
        let table = resolve_table(qualifier, &context.tables_in_scope)
            .unwrap_or_else(|| qualifier.clone());
        if let Some(columns) = schema.columns_by_table.get(&table.to_lowercase()) {
            for (name, data_type) in columns {
                items.push(CandidateItem {
                    text: name.clone(),
                    kind: ItemKind::Column,
                    detail: Some(data_type.clone()),
                });
            }
        }
        return dedup_sorted(items);
    }

    match context.kind {
        ContextKind::Columns => {
            for table_ref in &context.tables_in_scope {
                if let Some(columns) = schema.columns_by_table.get(&table_ref.name.to_lowercase()) {
                    for (name, data_type) in columns {
                        items.push(CandidateItem {
                            text: name.clone(),
                            kind: ItemKind::Column,
                            detail: Some(data_type.clone()),
                        });
                    }
                }
            }
            for function in functions_for(driver) {
                items.push(CandidateItem {
                    text: function.to_string(),
                    kind: ItemKind::Function,
                    detail: None,
                });
            }
        }
        ContextKind::Tables => {
            for (name, is_view) in &schema.tables {
                items.push(CandidateItem {
                    text: name.clone(),
                    kind: if *is_view { ItemKind::View } else { ItemKind::Table },
                    detail: None,
                });
            }
            for database in &schema.databases {
                items.push(CandidateItem {
                    text: database.clone(),
                    kind: ItemKind::Schema,
                    detail: None,
                });
            }
            for trigger in &schema.triggers {
                items.push(CandidateItem {
                    text: trigger.clone(),
                    kind: ItemKind::Trigger,
                    detail: None,
                });
            }
        }
    }

    for keyword in keywords_for(driver) {
        items.push(CandidateItem {
            text: keyword.to_string(),
            kind: ItemKind::Keyword,
            detail: None,
        });
    }

    dedup_sorted(items)
}

fn dedup_sorted(mut items: Vec<CandidateItem>) -> Vec<CandidateItem> {
    items.sort_by(|a, b| {
        a.kind
            .rank()
            .cmp(&b.kind.rank())
            .then_with(|| a.text.to_lowercase().cmp(&b.text.to_lowercase()))
    });
    items.dedup_by(|a, b| a.text == b.text && a.kind == b.kind);
    items
}

pub struct SqlCompletionProvider {
    store: WeakEntity<DatabaseStore>,
    connection_id: Option<ConnectionId>,
}

impl SqlCompletionProvider {
    pub fn new(store: WeakEntity<DatabaseStore>, connection_id: Option<ConnectionId>) -> Self {
        Self {
            store,
            connection_id,
        }
    }

    fn is_sql_buffer(buffer: &Entity<Buffer>, cx: &App) -> bool {
        buffer
            .read(cx)
            .language()
            .map(|lang| lang.name().as_ref().to_lowercase() == "sql")
            .unwrap_or(false)
    }

    fn text_before(buffer: &Entity<Buffer>, position: Anchor, cx: &App) -> String {
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        snapshot.text_for_range(0..offset).collect()
    }

    fn compute_replace_range(buffer: &Entity<Buffer>, position: Anchor, cx: &App) -> Range<Anchor> {
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let text_before: String = snapshot.text_for_range(0..offset).collect();
        let start = snapshot.anchor_before(offset - trailing_identifier_byte_len(&text_before));
        let end = snapshot.anchor_after(offset);
        start..end
    }
}

fn make_completion(item: &CandidateItem, replace_range: Range<Anchor>) -> Completion {
    let detail = match (&item.detail, item.kind) {
        (Some(data_type), ItemKind::Column) => format!("column · {data_type}"),
        _ => item.kind.label().to_string(),
    };
    Completion {
        replace_range,
        new_text: item.text.clone(),
        label: CodeLabel::plain(item.text.clone(), None),
        documentation: Some(CompletionDocumentation::SingleLine(detail.into())),
        source: CompletionSource::Custom,
        icon_path: None,
        icon_color: None,
        match_start: None,
        snippet_deduplication_key: None,
        insert_text_mode: None,
        confirm: None,
        group: None,
    }
}

/// Connection used for completion: the editor's bound connection when it is
/// connected, otherwise the store's active connection. Returns its id, dialect,
/// and the database to complete against.
fn resolve_connection(
    store: &DatabaseStore,
    preferred: Option<ConnectionId>,
) -> Option<(ConnectionId, DatabaseDriver, String)> {
    let conn = preferred
        .and_then(|id| store.connections().iter().find(|c| c.config.id == id))
        .filter(|c| matches!(c.status, crate::store::ConnectionStatus::Connected))
        .or_else(|| store.active_connection())?;
    let database = conn
        .config
        .database
        .clone()
        .filter(|d| !d.is_empty())
        .or_else(|| {
            conn.databases
                .as_ref()
                .and_then(|list| list.first())
                .map(|info| info.name.clone())
        })
        .unwrap_or_default();
    Some((conn.config.id, conn.config.driver, database))
}

fn read_schema_snapshot(
    store: &DatabaseStore,
    id: ConnectionId,
    database: &str,
    qualifier_schema: Option<&str>,
) -> SchemaSnapshot {
    let Some(conn) = store.connections().iter().find(|c| c.config.id == id) else {
        return SchemaSnapshot::default();
    };
    let mut snapshot = SchemaSnapshot::default();
    if let Some(databases) = &conn.databases {
        snapshot.databases = databases.iter().map(|d| d.name.clone()).collect();
    }
    let mut object_set: HashMap<String, bool> = HashMap::new();
    if let Some(tables) = conn.expanded_databases.get(database) {
        for table in tables {
            object_set.insert(table.name.clone(), matches!(table.kind, TableKind::View));
        }
    }
    if let Some(views) = conn.db_views.get(database) {
        for view in views {
            object_set.insert(view.clone(), true);
        }
    }
    snapshot.tables = object_set.into_iter().collect();

    if let Some(schema_name) = qualifier_schema {
        let mut objects: HashMap<String, bool> = HashMap::new();
        if let Some(tables) = conn.expanded_databases.get(schema_name) {
            for table in tables {
                objects.insert(table.name.clone(), matches!(table.kind, TableKind::View));
            }
        }
        if let Some(views) = conn.db_views.get(schema_name) {
            for view in views {
                objects.insert(view.clone(), true);
            }
        }
        snapshot
            .schema_objects
            .insert(schema_name.to_string(), objects.into_iter().collect());
    }

    for ((_db, table), columns) in &conn.expanded_tables {
        snapshot.columns_by_table.insert(
            table.to_lowercase(),
            columns
                .iter()
                .map(|c| (c.name.clone(), c.data_type.clone()))
                .collect(),
        );
    }
    for triggers in conn.table_triggers.values() {
        for trigger in triggers {
            snapshot.triggers.push(trigger.name.clone());
        }
    }
    snapshot
}

impl CompletionProvider for SqlCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: Anchor,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        if !Self::is_sql_buffer(buffer, cx) {
            return Task::ready(Ok(vec![]));
        }
        let Some(store) = self.store.upgrade() else {
            return Task::ready(Ok(vec![]));
        };

        let text_before = Self::text_before(buffer, buffer_position, cx);
        let replace_range = Self::compute_replace_range(buffer, buffer_position, cx);
        let context = parse_context(&text_before);

        let Some((connection_id, driver, database)) =
            resolve_connection(store.read(cx), self.connection_id)
        else {
            return Task::ready(Ok(vec![]));
        };

        let store = store.downgrade();
        cx.spawn(async move |_editor, cx| {
            ensure(&store, cx, connection_id, |store, cx| {
                store.ensure_schema_for_completion(connection_id, database.clone(), cx)
            })
            .await;

            // With the database list loaded, decide whether the qualifier names
            // a schema (suggest its objects) or a table/alias (suggest columns).
            let qualifier_is_schema = if let Some(qualifier) = &context.qualifier {
                store
                    .read_with(cx, |store, _| {
                        store
                            .connections()
                            .iter()
                            .find(|c| c.config.id == connection_id)
                            .and_then(|c| c.databases.as_ref())
                            .map(|dbs| dbs.iter().any(|d| d.name.eq_ignore_ascii_case(qualifier)))
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            } else {
                false
            };

            // Tables whose columns we may need: the qualifier target plus every
            // table in scope when completing columns.
            let mut tables_to_load: Vec<String> = Vec::new();
            if let Some(qualifier) = &context.qualifier {
                if qualifier_is_schema {
                    ensure(&store, cx, connection_id, |store, cx| {
                        store.ensure_schema_for_completion(connection_id, qualifier.clone(), cx)
                    })
                    .await;
                } else {
                    tables_to_load.push(
                        resolve_table(qualifier, &context.tables_in_scope)
                            .unwrap_or_else(|| qualifier.clone()),
                    );
                }
            } else if context.kind == ContextKind::Columns {
                tables_to_load.extend(context.tables_in_scope.iter().map(|t| t.name.clone()));
            }
            for table in tables_to_load {
                let database = database.clone();
                ensure(&store, cx, connection_id, move |store, cx| {
                    store.ensure_columns_for_completion(connection_id, database, table, cx)
                })
                .await;
            }

            let qualifier_schema = if qualifier_is_schema {
                context.qualifier.as_deref()
            } else {
                None
            };
            let snapshot = store
                .read_with(cx, |store, _| {
                    read_schema_snapshot(store, connection_id, &database, qualifier_schema)
                })
                .unwrap_or_default();

            let completions = build_candidates(&context, &snapshot, driver)
                .iter()
                .map(|item| make_completion(item, replace_range.clone()))
                .collect();

            Ok(vec![CompletionResponse {
                completions,
                display_options: CompletionDisplayOptions::default(),
                is_incomplete: false,
            }])
        })
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        _position: language::Anchor,
        text: &str,
        trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        if !Self::is_sql_buffer(buffer, cx) {
            return false;
        }
        text == "." || trigger_in_words
    }

    fn resolve_completions(
        &self,
        _buffer: Entity<Buffer>,
        _completion_indices: Vec<usize>,
        _completions: Rc<RefCell<Box<[Completion]>>>,
        _cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<bool>> {
        Task::ready(Ok(false))
    }

    fn sort_completions(&self) -> bool {
        true
    }

    fn filter_completions(&self) -> bool {
        true
    }
}

/// Runs a store schema-fetch task to completion, ignoring failures (completion
/// must never surface an error; a missing connection just yields no fetch).
async fn ensure<F>(
    store: &WeakEntity<DatabaseStore>,
    cx: &mut gpui::AsyncApp,
    _connection_id: ConnectionId,
    build: F,
) where
    F: FnOnce(&mut DatabaseStore, &mut Context<DatabaseStore>) -> Task<anyhow::Result<()>>,
{
    if let Ok(task) = store.update(cx, |store, cx| build(store, cx)) {
        task.await.log_err();
    }
}

// Byte length of the trailing identifier in `text`. Summing per-char byte
// lengths keeps a derived start offset on a UTF-8 boundary; mixing a byte
// offset with a char count lands mid-character and panics the rope.
fn trailing_identifier_byte_len(text: &str) -> usize {
    text.chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .map(|c| c.len_utf8())
        .sum()
}

pub fn install_on_editor(
    editor: Entity<Editor>,
    store: WeakEntity<DatabaseStore>,
    connection_id: Option<ConnectionId>,
    cx: &mut App,
) {
    editor.update(cx, |editor, cx| {
        let is_sql = editor
            .language_at(language::Point::new(0, 0), cx)
            .map(|lang| lang.name().as_ref().to_lowercase() == "sql")
            .unwrap_or(false);
        if is_sql {
            let provider = Rc::new(SqlCompletionProvider::new(store, connection_id));
            editor.set_completion_provider(Some(provider));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(text_before: &str) -> ParsedContext {
        parse_context(text_before)
    }

    #[test]
    fn ascii_identifier_length_is_byte_count() {
        assert_eq!(trailing_identifier_byte_len("SELECT * FROM use"), 3);
        assert_eq!(trailing_identifier_byte_len("col_name"), 8);
        assert_eq!(trailing_identifier_byte_len(""), 0);
    }

    #[test]
    fn multibyte_identifier_counts_bytes_not_chars() {
        assert_eq!(trailing_identifier_byte_len("таблица"), 14);
        assert_eq!(trailing_identifier_byte_len("SELECT поле"), 8);
    }

    #[test]
    fn current_statement_is_after_last_semicolon() {
        assert_eq!(current_statement("SELECT 1; SELECT 2"), " SELECT 2");
        assert_eq!(current_statement("SELECT 1"), "SELECT 1");
    }

    #[test]
    fn qualifier_is_identifier_before_dot() {
        assert_eq!(extract_qualifier("SELECT * FROM s.tab"), Some("s".to_string()));
        assert_eq!(extract_qualifier("SELECT u.").as_deref(), Some("u"));
        assert_eq!(extract_qualifier("SELECT col"), None);
        assert_eq!(extract_qualifier("SELECT "), None);
    }

    #[test]
    fn clause_kind_picks_latest_keyword() {
        assert_eq!(clause_kind("SELECT * FROM "), ContextKind::Tables);
        assert_eq!(clause_kind("SELECT "), ContextKind::Columns);
        assert_eq!(clause_kind("SELECT * FROM t WHERE "), ContextKind::Columns);
        assert_eq!(clause_kind("UPDATE "), ContextKind::Tables);
    }

    #[test]
    fn parse_table_refs_reads_aliases() {
        let refs = parse_table_refs("SELECT * FROM users u JOIN orders AS o ON u.id = o.user_id");
        assert_eq!(
            refs,
            vec![
                TableRef {
                    name: "users".to_string(),
                    alias: Some("u".to_string())
                },
                TableRef {
                    name: "orders".to_string(),
                    alias: Some("o".to_string())
                },
            ]
        );
    }

    #[test]
    fn parse_table_refs_strips_schema_and_handles_no_alias() {
        let refs = parse_table_refs("SELECT * FROM instruments.splits");
        assert_eq!(
            refs,
            vec![TableRef {
                name: "splits".to_string(),
                alias: None
            }]
        );
    }

    #[test]
    fn resolve_table_maps_alias_then_name() {
        let refs = vec![TableRef {
            name: "users".to_string(),
            alias: Some("u".to_string()),
        }];
        assert_eq!(resolve_table("u", &refs).as_deref(), Some("users"));
        assert_eq!(resolve_table("users", &refs).as_deref(), Some("users"));
        assert_eq!(resolve_table("x", &refs), None);
    }

    #[test]
    fn dialect_keywords_differ_by_driver() {
        assert!(keywords_for(DatabaseDriver::PostgreSQL).contains(&"RETURNING"));
        assert!(!keywords_for(DatabaseDriver::MySQL).contains(&"RETURNING"));
        assert!(keywords_for(DatabaseDriver::MySQL).contains(&"AUTO_INCREMENT"));
        assert!(keywords_for(DatabaseDriver::SQLite).contains(&"AUTOINCREMENT"));
        assert!(functions_for(DatabaseDriver::ClickHouse).contains(&"arrayJoin"));
    }

    fn sample_schema() -> SchemaSnapshot {
        let mut columns_by_table = HashMap::new();
        columns_by_table.insert(
            "users".to_string(),
            vec![
                ("id".to_string(), "int".to_string()),
                ("email".to_string(), "varchar".to_string()),
            ],
        );
        let mut schema_objects = HashMap::new();
        schema_objects.insert(
            "instruments".to_string(),
            vec![("splits".to_string(), false), ("prices".to_string(), false)],
        );
        SchemaSnapshot {
            databases: vec!["instruments".to_string(), "mysql".to_string()],
            tables: vec![("users".to_string(), false), ("orders".to_string(), false)],
            columns_by_table,
            schema_objects,
            triggers: Vec::new(),
        }
    }

    #[test]
    fn after_from_suggests_tables_and_keywords() {
        let context = ctx("SELECT * FROM ");
        let items = build_candidates(&context, &sample_schema(), DatabaseDriver::MySQL);
        assert!(items.iter().any(|i| i.text == "users" && i.kind == ItemKind::Table));
        assert!(items.iter().any(|i| i.kind == ItemKind::Schema && i.text == "instruments"));
        assert!(items.iter().any(|i| i.kind == ItemKind::Keyword));
        // Tables rank before keywords.
        let first_keyword = items.iter().position(|i| i.kind == ItemKind::Keyword).unwrap();
        let first_table = items.iter().position(|i| i.kind == ItemKind::Table).unwrap();
        assert!(first_table < first_keyword);
    }

    #[test]
    fn alias_dot_suggests_that_tables_columns() {
        let context = ctx("SELECT * FROM users u WHERE u.");
        let items = build_candidates(&context, &sample_schema(), DatabaseDriver::MySQL);
        assert!(items.iter().any(|i| i.text == "id" && i.kind == ItemKind::Column));
        assert!(items.iter().any(|i| i.text == "email" && i.kind == ItemKind::Column));
        // Only columns for a qualified completion.
        assert!(items.iter().all(|i| i.kind == ItemKind::Column));
    }

    #[test]
    fn schema_dot_suggests_schema_objects() {
        let context = ctx("SELECT * FROM instruments.");
        let items = build_candidates(&context, &sample_schema(), DatabaseDriver::MySQL);
        assert!(items.iter().any(|i| i.text == "splits" && i.kind == ItemKind::Table));
        assert!(items.iter().any(|i| i.text == "prices"));
    }

    #[test]
    fn columns_context_includes_scoped_columns_and_functions() {
        let context = ctx("SELECT  FROM users u");
        // Cursor is right after SELECT (columns expected).
        let context = ParsedContext {
            qualifier: None,
            kind: ContextKind::Columns,
            tables_in_scope: context.tables_in_scope,
        };
        let items = build_candidates(&context, &sample_schema(), DatabaseDriver::MySQL);
        assert!(items.iter().any(|i| i.text == "id" && i.kind == ItemKind::Column));
        assert!(items.iter().any(|i| i.kind == ItemKind::Function));
    }
}
