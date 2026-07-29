use crate::store::{CliQueryOutput, DatabaseStore};
use agent::{AgentTool, ToolCallEventStream, ToolInput, ToolPermissionContext};
use agent_client_protocol::schema::v1 as acp;
use db_client::DatabaseDriver;
use gpui::{App, SharedString, Task};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::ops::ControlFlow;
use std::sync::Arc;

type Result<T, E = String> = core::result::Result<T, E>;

const MAX_OUTPUT_ROWS: usize = 200;
const MAX_CELL_CHARS: usize = 500;

pub fn init() {
    agent::register_extra_tool(|| DbListConnectionsTool.erase());
    agent::register_extra_tool(|| DbRunQueryTool.erase());
    agent::register_extra_tool(|| DbDescribeSchemaTool.erase());
    agent::register_extra_tool(|| DbExplainErrorTool.erase());
}

fn no_store_message() -> String {
    "No database connections are available: open the Database Explorer panel in this Zed window first.".to_string()
}

fn store(cx: &App) -> Result<gpui::Entity<DatabaseStore>> {
    DatabaseStore::global(cx).ok_or_else(no_store_message)
}

const READ_ONLY_STARTERS: [&str; 7] = [
    "SELECT", "WITH", "SHOW", "DESCRIBE", "DESC", "EXPLAIN", "USE",
];

const WRITE_KEYWORDS: [&str; 19] = [
    "INSERT", "UPDATE", "DELETE", "REPLACE", "MERGE", "TRUNCATE", "DROP", "ALTER", "CREATE",
    "RENAME", "GRANT", "REVOKE", "SET", "CALL", "LOAD", "IMPORT", "COPY", "OUTFILE", "DUMPFILE",
];

/// Splits SQL into uppercased word tokens, skipping line/block comments and the
/// contents of quoted strings and identifiers, so a keyword is only reported
/// when it is a real SQL keyword and not text inside a literal or an identifier.
fn sql_word_tokens(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'-' && bytes.get(index + 1) == Some(&b'-') {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index += 2;
            continue;
        }
        if byte == b'\'' || byte == b'"' || byte == b'`' {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current).to_ascii_uppercase());
            }
            let quote = byte;
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'\\' && (quote == b'\'' || quote == b'"') {
                    index += 2;
                    continue;
                }
                if bytes[index] == quote {
                    // A doubled quote is an escaped quote that stays inside the string.
                    if bytes.get(index + 1) == Some(&quote) {
                        index += 2;
                        continue;
                    }
                    index += 1;
                    break;
                }
                index += 1;
            }
            continue;
        }
        let character = byte as char;
        if character.is_ascii_alphanumeric() || character == '_' {
            current.push(character);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current).to_ascii_uppercase());
        }
        index += 1;
    }
    if !current.is_empty() {
        tokens.push(current.to_ascii_uppercase());
    }
    tokens
}

/// Read-only guard for the agent: the statement must start with a read-only
/// keyword and contain no data- or schema-modifying keyword anywhere, so a
/// `WITH ... DELETE` data-modifying CTE or a chained write is rejected. Keywords
/// inside quotes or comments are ignored. Anything unrecognized is rejected too,
/// which is the safe default for a read-only agent.
pub(crate) fn is_read_only_sql(sql: &str) -> bool {
    let tokens = sql_word_tokens(sql);
    let Some(first) = tokens.first() else {
        return false;
    };
    if !READ_ONLY_STARTERS.contains(&first.as_str()) {
        return false;
    }
    !tokens
        .iter()
        .any(|token| WRITE_KEYWORDS.contains(&token.as_str()))
}

/// A statement that is not read-only changes data or schema, so the agent must
/// ask the user to confirm before running it.
pub(crate) fn requires_confirmation(sql: &str) -> bool {
    !is_read_only_sql(sql)
}

fn format_connections(summaries: &[(String, String, String)]) -> String {
    if summaries.is_empty() {
        return "No database connections are saved.".to_string();
    }
    let mut output = String::new();
    for (id, label, driver) in summaries {
        let _ = writeln!(output, "{label} ({driver}) [id: {id}]");
    }
    output
}

fn truncate_cell(value: &Option<String>) -> String {
    match value {
        None => "NULL".to_string(),
        Some(text) if text.chars().count() > MAX_CELL_CHARS => {
            let truncated: String = text.chars().take(MAX_CELL_CHARS).collect();
            format!("{truncated}…")
        }
        Some(text) => text.clone(),
    }
}

fn format_query_output(result: &CliQueryOutput) -> String {
    let mut output = String::new();
    if !result.columns.is_empty() {
        let _ = writeln!(output, "{}", result.columns.join(" | "));
    }
    for row in result.rows.iter().take(MAX_OUTPUT_ROWS) {
        let cells: Vec<String> = row.iter().map(truncate_cell).collect();
        let _ = writeln!(output, "{}", cells.join(" | "));
    }
    if result.rows.len() > MAX_OUTPUT_ROWS {
        let _ = writeln!(
            output,
            "… {} more rows not shown",
            result.rows.len() - MAX_OUTPUT_ROWS
        );
    }
    let _ = write!(
        output,
        "({} rows, {} affected, {} ms)",
        result.rows.len(),
        result.rows_affected,
        result.execution_time_ms
    );
    output
}

/// Lists the database connections saved in Zed's Database Explorer, with their
/// id, label, and driver. Use the id or label as the `connection` argument to
/// `db_run_query`.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct DbListConnectionsInput {}

pub struct DbListConnectionsTool;

impl AgentTool for DbListConnectionsTool {
    type Input = DbListConnectionsInput;
    type Output = String;

    const NAME: &'static str = "db_list_connections";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "List database connections".into()
    }

    fn run(
        self: Arc<Self>,
        _input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let summaries = match store(cx) {
            Ok(store) => store.read(cx).connection_summaries(),
            Err(message) => return Task::ready(Err(message)),
        };
        Task::ready(Ok(format_connections(&summaries)))
    }
}

/// Runs a SQL query against a saved database connection and returns the rows.
/// Read statements (SELECT, WITH, SHOW, DESCRIBE, EXPLAIN) run directly;
/// statements that change data or schema (INSERT, UPDATE, DELETE, DDL, ...) run
/// only after the user confirms. The connection is opened automatically if it
/// is not connected yet.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct DbRunQueryInput {
    /// Connection id or label (see `db_list_connections`).
    pub connection: String,
    /// Database/schema to run against. Defaults to the connection's database.
    #[serde(default)]
    pub database: Option<String>,
    /// The SQL statement to run. Writes and DDL require user confirmation.
    pub sql: String,
}

pub struct DbRunQueryTool;

impl AgentTool for DbRunQueryTool {
    type Input = DbRunQueryInput;
    type Output = String;

    const NAME: &'static str = "db_run_query";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Execute
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Query {}", input.connection).into(),
            Err(_) => "Run database query".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let store = match store(cx) {
            Ok(store) => store,
            Err(message) => return Task::ready(Err(message)),
        };
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            if requires_confirmation(&input.sql) {
                let title = format!("Run write query on {}", input.connection);
                let context = ToolPermissionContext::new(Self::NAME, vec![input.sql.clone()]);
                let authorize = cx.update(|cx| event_stream.authorize(title, context, cx));
                if let Err(error) = authorize.await {
                    return Err(format!("The write query was not run: {error}"));
                }
            }
            let query = store.update(cx, |store, cx| {
                store.run_query_for_cli(input.connection, input.database, input.sql, cx)
            });
            let result = query.await.map_err(|error| error.to_string())?;
            Ok(format_query_output(&result))
        })
    }
}

fn resolve_connection(
    store: &DatabaseStore,
    connection: &str,
) -> Result<(db_client::ConnectionId, DatabaseDriver, Option<String>)> {
    let id = store
        .resolve_connection_id(connection)
        .ok_or_else(|| format!("No database connection matching '{connection}'"))?;
    let connection = store
        .connections()
        .iter()
        .find(|c| c.config.id == id)
        .ok_or_else(|| format!("No database connection matching '{connection}'"))?;
    Ok((
        id,
        connection.config.driver,
        connection.config.database.clone(),
    ))
}

fn resolve_database(
    input_database: &Option<String>,
    default_database: Option<String>,
) -> Result<String> {
    input_database
        .clone()
        .filter(|database| !database.is_empty())
        .or(default_database)
        .ok_or_else(|| {
            "No database specified and the connection has no default database".to_string()
        })
}

/// Formats cached schema info (tables and their columns) for a connection,
/// optionally narrowed to one database and/or one table. Reads only the
/// already-prefetched schema cache -- never queries the database itself, so
/// it works for a read-only connection and never blocks on the network.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct DbDescribeSchemaInput {
    /// Connection id or label (see `db_list_connections`).
    pub connection: String,
    /// Database/schema to describe. Defaults to the connection's database.
    #[serde(default)]
    pub database: Option<String>,
    /// Restrict to this single table. When omitted, every cached table's
    /// columns are listed.
    #[serde(default)]
    pub table: Option<String>,
}

pub struct DbDescribeSchemaTool;

impl AgentTool for DbDescribeSchemaTool {
    type Input = DbDescribeSchemaInput;
    type Output = String;

    const NAME: &'static str = "db_describe_schema";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Describe schema for {}", input.connection).into(),
            Err(_) => "Describe database schema".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let store = match store(cx) {
            Ok(store) => store,
            Err(message) => return Task::ready(Err(message)),
        };
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            store.update(cx, |store, _cx| describe_schema(store, &input))
        })
    }
}

fn describe_schema(store: &DatabaseStore, input: &DbDescribeSchemaInput) -> Result<String> {
    let (id, _driver, default_database) = resolve_connection(store, &input.connection)?;
    let database = resolve_database(&input.database, default_database)?;
    let tables = match &input.table {
        Some(table) => vec![table.clone()],
        None => store.cached_table_names(id, &database),
    };
    if tables.is_empty() {
        return Ok(format!(
            "No cached schema for database '{database}'. Open the Database Explorer and \
             expand this connection first, or run db_run_query against it once to trigger \
             a prefetch."
        ));
    }
    let mut output = String::new();
    for table in &tables {
        let columns = store.cached_table_columns(id, &database, table);
        if columns.is_empty() {
            let _ = writeln!(output, "{table}: (no cached columns)");
            continue;
        }
        let _ = writeln!(output, "{table}:");
        for column in &columns {
            let nullable = if column.is_nullable {
                "NULL"
            } else {
                "NOT NULL"
            };
            let key = column
                .column_key
                .as_deref()
                .map(|key| format!(" [{key}]"))
                .unwrap_or_default();
            let _ = writeln!(
                output,
                "  {} {} {nullable}{key}",
                column.name, column.data_type
            );
        }
    }
    Ok(output)
}

/// Walks a parsed SQL statement and collects every distinct table name it
/// references (FROM/JOIN targets, INSERT/UPDATE/DELETE targets, subqueries),
/// in first-seen order. Returns an empty list on a parse failure -- the
/// caller falls back to reporting the raw error without schema context
/// rather than failing the whole tool call.
fn referenced_tables(sql: &str, driver: DatabaseDriver) -> Vec<String> {
    struct TableCollector {
        tables: Vec<String>,
    }
    impl sqlparser::ast::Visitor for TableCollector {
        type Break = ();
        fn pre_visit_relation(
            &mut self,
            relation: &sqlparser::ast::ObjectName,
        ) -> ControlFlow<Self::Break> {
            let (_, table) = crate::sql_binder::database_and_table(relation);
            if !self.tables.contains(&table) {
                self.tables.push(table);
            }
            ControlFlow::Continue(())
        }
    }

    let Some(dialect) = crate::sql_ast::dialect_for_driver(driver) else {
        return Vec::new();
    };
    let Ok(statements) = sqlparser::parser::Parser::parse_sql(dialect.as_ref(), sql) else {
        return Vec::new();
    };
    let mut collector = TableCollector { tables: Vec::new() };
    for statement in &statements {
        let _ = sqlparser::ast::Visit::visit(statement, &mut collector);
    }
    collector.tables
}

/// Bundles a failed statement and the driver's error message with the real
/// column list of every table the statement references, resolved against the
/// cached schema, so the calling model can spot e.g. a typo'd column name
/// against the table's actual columns instead of guessing blind.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct DbExplainErrorInput {
    /// Connection id or label (see `db_list_connections`).
    pub connection: String,
    /// Database/schema the statement targeted. Defaults to the connection's database.
    #[serde(default)]
    pub database: Option<String>,
    /// The SQL statement that failed.
    pub sql: String,
    /// The error message the database returned.
    pub error: String,
}

pub struct DbExplainErrorTool;

impl AgentTool for DbExplainErrorTool {
    type Input = DbExplainErrorInput;
    type Output = String;

    const NAME: &'static str = "db_explain_error";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Explain database error".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let store = match store(cx) {
            Ok(store) => store,
            Err(message) => return Task::ready(Err(message)),
        };
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            store.update(cx, |store, _cx| explain_error(store, &input))
        })
    }
}

fn explain_error(store: &DatabaseStore, input: &DbExplainErrorInput) -> Result<String> {
    let (id, driver, default_database) = resolve_connection(store, &input.connection)?;
    let database = resolve_database(&input.database, default_database)?;

    let mut output = String::new();
    let _ = writeln!(output, "Error: {}", input.error);
    let _ = writeln!(output, "Statement: {}", input.sql);

    let tables = referenced_tables(&input.sql, driver);
    if tables.is_empty() {
        let _ = write!(
            output,
            "\nNo table references could be parsed from the statement."
        );
        return Ok(output);
    }
    let _ = write!(output, "\nReferenced tables (from cached schema):");
    for table in &tables {
        let columns = store.cached_table_columns(id, &database, table);
        if columns.is_empty() {
            let _ = write!(
                output,
                "\n{table}: not found in cached schema for database '{database}' (check for \
                 a typo, or the schema may not be prefetched yet)"
            );
        } else {
            let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
            let _ = write!(output, "\n{table}: {}", names.join(", "));
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::GlobalDatabaseStore;
    use db_client::ConnectionConfig;
    use gpui::AppContext as _;

    struct SchemaContextMockProvider;

    #[async_trait::async_trait]
    impl db_client::provider::DbProvider for SchemaContextMockProvider {
        async fn ping(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> anyhow::Result<Vec<db_client::DatabaseInfo>> {
            Ok(vec![db_client::DatabaseInfo {
                name: "shop".into(),
            }])
        }
        async fn list_tables(&self, _database: &str) -> anyhow::Result<Vec<db_client::TableInfo>> {
            Ok(vec![db_client::TableInfo {
                name: "users".into(),
                kind: db_client::schema::TableKind::Table,
            }])
        }
        async fn describe_table(
            &self,
            _database: &str,
            table: &str,
        ) -> anyhow::Result<Vec<db_client::ColumnInfo>> {
            if table != "users" {
                return Ok(Vec::new());
            }
            Ok(vec![
                db_client::ColumnInfo {
                    name: "id".into(),
                    data_type: "int".into(),
                    is_nullable: false,
                    column_key: Some("PRI".into()),
                    default_value: None,
                    extra: String::new(),
                },
                db_client::ColumnInfo {
                    name: "email".into(),
                    data_type: "varchar(255)".into(),
                    is_nullable: false,
                    column_key: None,
                    default_value: None,
                    extra: String::new(),
                },
            ])
        }
        async fn execute_query(
            &self,
            _database: &str,
            _sql: &str,
        ) -> anyhow::Result<db_client::schema::QueryResult> {
            Ok(db_client::schema::QueryResult {
                columns: vec!["id".to_string()],
                rows: vec![vec![Some("1".to_string())]],
                rows_affected: 1,
                execution_time_ms: 0,
                timing: None,
                raw_documents: None,
            })
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
            Ok("TABLE_DDL".to_string())
        }
    }

    /// Seeds a global store with one connected connection whose schema cache
    /// has been fully prefetched from `SchemaContextMockProvider`.
    async fn seeded_store_with_schema(cx: &mut gpui::TestAppContext) -> String {
        let config = ConnectionConfig::default();
        let id = config.id;
        let connection = id.to_string();
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(SchemaContextMockProvider), cx);
            store.prefetch_full_schema(id, cx).detach();
        });
        cx.run_until_parked();
        cx.update(|cx| cx.set_global(GlobalDatabaseStore(store.clone())));
        connection
    }

    #[gpui::test]
    async fn db_describe_schema_lists_every_cached_table_by_default(cx: &mut gpui::TestAppContext) {
        let connection = seeded_store_with_schema(cx).await;
        let task = cx.update(|cx| {
            let (event_stream, _receiver) = ToolCallEventStream::test();
            Arc::new(DbDescribeSchemaTool).run(
                ToolInput::resolved(DbDescribeSchemaInput {
                    connection,
                    database: Some("shop".to_string()),
                    table: None,
                }),
                event_stream,
                cx,
            )
        });
        let output = task.await.expect("describing schema succeeds");
        assert!(output.contains("users:"));
        assert!(output.contains("id int NOT NULL [PRI]"));
        assert!(output.contains("email varchar(255) NOT NULL"));
    }

    #[gpui::test]
    async fn db_describe_schema_narrows_to_one_table_when_given(cx: &mut gpui::TestAppContext) {
        let connection = seeded_store_with_schema(cx).await;
        let task = cx.update(|cx| {
            let (event_stream, _receiver) = ToolCallEventStream::test();
            Arc::new(DbDescribeSchemaTool).run(
                ToolInput::resolved(DbDescribeSchemaInput {
                    connection,
                    database: Some("shop".to_string()),
                    table: Some("users".to_string()),
                }),
                event_stream,
                cx,
            )
        });
        let output = task.await.expect("describing one table succeeds");
        assert_eq!(output.matches("users:").count(), 1);
        assert!(output.contains("email"));
    }

    #[gpui::test]
    async fn db_describe_schema_reports_unknown_connection_without_panicking(
        cx: &mut gpui::TestAppContext,
    ) {
        let store = cx.new(DatabaseStore::new);
        cx.update(|cx| cx.set_global(GlobalDatabaseStore(store.clone())));
        let task = cx.update(|cx| {
            let (event_stream, _receiver) = ToolCallEventStream::test();
            Arc::new(DbDescribeSchemaTool).run(
                ToolInput::resolved(DbDescribeSchemaInput {
                    connection: "nonexistent".to_string(),
                    database: None,
                    table: None,
                }),
                event_stream,
                cx,
            )
        });
        let error = task
            .await
            .expect_err("an unknown connection must error, not panic");
        assert!(error.contains("nonexistent"));
    }

    #[gpui::test]
    async fn db_explain_error_resolves_the_typo_free_column_list_for_referenced_tables(
        cx: &mut gpui::TestAppContext,
    ) {
        let connection = seeded_store_with_schema(cx).await;
        let task = cx.update(|cx| {
            let (event_stream, _receiver) = ToolCallEventStream::test();
            Arc::new(DbExplainErrorTool).run(
                ToolInput::resolved(DbExplainErrorInput {
                    connection,
                    database: Some("shop".to_string()),
                    // "emial" is a typo for the real "email" column.
                    sql: "SELECT emial FROM users".to_string(),
                    error: "Unknown column 'emial' in 'field list'".to_string(),
                }),
                event_stream,
                cx,
            )
        });
        let output = task.await.expect("explaining the error succeeds");
        assert!(output.contains("Unknown column 'emial'"));
        assert!(
            output.contains("users: id, email"),
            "must resolve the referenced table's real columns so the typo is spottable: {output}"
        );
    }

    #[gpui::test]
    async fn db_explain_error_reports_unknown_connection_without_panicking(
        cx: &mut gpui::TestAppContext,
    ) {
        let store = cx.new(DatabaseStore::new);
        cx.update(|cx| cx.set_global(GlobalDatabaseStore(store.clone())));
        let task = cx.update(|cx| {
            let (event_stream, _receiver) = ToolCallEventStream::test();
            Arc::new(DbExplainErrorTool).run(
                ToolInput::resolved(DbExplainErrorInput {
                    connection: "nonexistent".to_string(),
                    database: None,
                    sql: "SELECT 1".to_string(),
                    error: "boom".to_string(),
                }),
                event_stream,
                cx,
            )
        });
        let error = task
            .await
            .expect_err("an unknown connection must error, not panic");
        assert!(error.contains("nonexistent"));
    }

    #[test]
    fn referenced_tables_extracts_from_from_join_insert_update_and_delete() {
        assert_eq!(
            referenced_tables("SELECT * FROM users", DatabaseDriver::MySQL),
            vec!["users".to_string()]
        );
        assert_eq!(
            referenced_tables(
                "SELECT * FROM orders o JOIN users u ON u.id = o.user_id",
                DatabaseDriver::PostgreSQL
            ),
            vec!["orders".to_string(), "users".to_string()]
        );
        assert_eq!(
            referenced_tables("INSERT INTO users (id) VALUES (1)", DatabaseDriver::MySQL),
            vec!["users".to_string()]
        );
        assert_eq!(
            referenced_tables("UPDATE users SET email = 'x'", DatabaseDriver::MySQL),
            vec!["users".to_string()]
        );
        assert_eq!(
            referenced_tables("DELETE FROM users WHERE id = 1", DatabaseDriver::MySQL),
            vec!["users".to_string()]
        );
        assert!(referenced_tables("not valid sql (((", DatabaseDriver::MySQL).is_empty());
    }

    #[gpui::test]
    async fn db_list_connections_tool_reads_global_store(cx: &mut gpui::TestAppContext) {
        let mut config = ConnectionConfig::default();
        config.label = "primary".into();
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| store.add_connection(config, cx));
        cx.update(|cx| cx.set_global(GlobalDatabaseStore(store.clone())));

        let task = cx.update(|cx| {
            let (event_stream, _receiver) = ToolCallEventStream::test();
            Arc::new(DbListConnectionsTool).run(
                ToolInput::resolved(DbListConnectionsInput {}),
                event_stream,
                cx,
            )
        });
        let output = task.await.expect("listing connections succeeds");
        assert!(output.contains("primary"));
    }

    #[test]
    fn read_only_sql_allows_reads_and_blocks_writes() {
        assert!(is_read_only_sql("SELECT * FROM t"));
        assert!(is_read_only_sql("  select 1"));
        assert!(is_read_only_sql("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(is_read_only_sql("SHOW TABLES"));
        assert!(is_read_only_sql("DESCRIBE t"));
        assert!(is_read_only_sql("-- a comment\nSELECT 1"));
        assert!(is_read_only_sql("EXPLAIN SELECT 1"));

        assert!(!is_read_only_sql("INSERT INTO t VALUES (1)"));
        assert!(!is_read_only_sql("UPDATE t SET a = 1"));
        assert!(!is_read_only_sql("DELETE FROM t"));
        assert!(!is_read_only_sql("DROP TABLE t"));
        assert!(!is_read_only_sql("ALTER TABLE t ADD c INT"));
        assert!(!is_read_only_sql("TRUNCATE t"));
        assert!(!is_read_only_sql("CREATE TABLE t (a INT)"));
    }

    #[test]
    fn read_only_sql_rejects_data_modifying_cte_and_chained_writes() {
        // A data-modifying CTE starts with WITH but must still be rejected.
        assert!(!is_read_only_sql(
            "WITH t AS (DELETE FROM a RETURNING id) SELECT * FROM t"
        ));
        assert!(!is_read_only_sql(
            "WITH t AS (INSERT INTO a VALUES (1) RETURNING id) SELECT * FROM t"
        ));
        // A read-only CTE is still allowed.
        assert!(is_read_only_sql("WITH t AS (SELECT 1) SELECT * FROM t"));
        // A write chained after a read is rejected.
        assert!(!is_read_only_sql("SELECT * FROM t; DROP TABLE x"));
        assert!(!is_read_only_sql("USE db; UPDATE t SET a = 1"));
        // Unrecognized leading keyword is rejected (safe default).
        assert!(!is_read_only_sql("VACUUM"));
        assert!(!is_read_only_sql("PRAGMA writable_schema = 1"));
    }

    #[test]
    fn requires_confirmation_only_for_writes() {
        // Read-only statements run without confirmation.
        assert!(!requires_confirmation("SELECT * FROM t"));
        assert!(!requires_confirmation(
            "WITH x AS (SELECT 1) SELECT * FROM x"
        ));
        assert!(!requires_confirmation("SHOW TABLES"));
        // Writes, DDL, data-modifying CTEs, and chained writes need confirmation.
        assert!(requires_confirmation("INSERT INTO t VALUES (1)"));
        assert!(requires_confirmation("UPDATE t SET a = 1"));
        assert!(requires_confirmation("DROP TABLE t"));
        assert!(requires_confirmation(
            "WITH t AS (DELETE FROM a RETURNING id) SELECT * FROM t"
        ));
        assert!(requires_confirmation("SELECT * FROM t; DROP TABLE x"));
        // `SELECT ... INTO OUTFILE/DUMPFILE` writes a file on the database
        // server and must not slip through as read-only just because it
        // starts with SELECT.
        assert!(requires_confirmation(
            "SELECT * FROM t INTO OUTFILE '/tmp/x.csv'"
        ));
        assert!(requires_confirmation(
            "SELECT * FROM t INTO DUMPFILE '/tmp/x.bin'"
        ));
        // The common case without INTO OUTFILE/DUMPFILE stays read-only.
        assert!(!requires_confirmation("SELECT * FROM t"));
    }

    #[test]
    fn read_only_sql_ignores_keywords_inside_quotes_and_comments() {
        // DML keywords inside string literals or quoted identifiers are not keywords.
        assert!(is_read_only_sql("SELECT 'DROP TABLE x' AS note"));
        assert!(is_read_only_sql("SELECT \"DELETE\" FROM t"));
        assert!(is_read_only_sql("SELECT `update` FROM t"));
        assert!(is_read_only_sql("SELECT /* DROP TABLE x */ 1"));
        assert!(is_read_only_sql("USE analytics"));
    }

    #[test]
    fn format_query_output_renders_header_rows_and_footer() {
        let result = CliQueryOutput {
            columns: vec!["id".into(), "name".into()],
            rows: vec![
                vec![Some("1".into()), Some("a".into())],
                vec![Some("2".into()), None],
            ],
            rows_affected: 0,
            execution_time_ms: 7,
        };
        let text = format_query_output(&result);
        assert!(text.contains("id | name"));
        assert!(text.contains("1 | a"));
        assert!(text.contains("2 | NULL"));
        assert!(text.contains("(2 rows, 0 affected, 7 ms)"));
    }

    #[test]
    fn format_connections_lists_each_entry() {
        let summaries = vec![(
            "abc".to_string(),
            "Local MySQL".to_string(),
            "MySQL".to_string(),
        )];
        let text = format_connections(&summaries);
        assert!(text.contains("Local MySQL (MySQL) [id: abc]"));
        assert_eq!(
            format_connections(&[]),
            "No database connections are saved."
        );
    }
}
