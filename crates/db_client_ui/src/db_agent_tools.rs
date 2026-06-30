use crate::store::{CliQueryOutput, DatabaseStore};
use agent::{AgentTool, ToolCallEventStream, ToolInput, ToolPermissionContext};
use agent_client_protocol::schema::v1 as acp;
use gpui::{App, SharedString, Task};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::sync::Arc;

type Result<T, E = String> = core::result::Result<T, E>;

const MAX_OUTPUT_ROWS: usize = 200;
const MAX_CELL_CHARS: usize = 500;

pub fn init() {
    agent::register_extra_tool(|| DbListConnectionsTool.erase());
    agent::register_extra_tool(|| DbRunQueryTool.erase());
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

const WRITE_KEYWORDS: [&str; 17] = [
    "INSERT", "UPDATE", "DELETE", "REPLACE", "MERGE", "TRUNCATE", "DROP", "ALTER", "CREATE",
    "RENAME", "GRANT", "REVOKE", "SET", "CALL", "LOAD", "IMPORT", "COPY",
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
fn is_read_only_sql(sql: &str) -> bool {
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
fn requires_confirmation(sql: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::GlobalDatabaseStore;
    use db_client::ConnectionConfig;
    use gpui::AppContext as _;

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
        assert!(!requires_confirmation("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(!requires_confirmation("SHOW TABLES"));
        // Writes, DDL, data-modifying CTEs, and chained writes need confirmation.
        assert!(requires_confirmation("INSERT INTO t VALUES (1)"));
        assert!(requires_confirmation("UPDATE t SET a = 1"));
        assert!(requires_confirmation("DROP TABLE t"));
        assert!(requires_confirmation(
            "WITH t AS (DELETE FROM a RETURNING id) SELECT * FROM t"
        ));
        assert!(requires_confirmation("SELECT * FROM t; DROP TABLE x"));
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
        assert_eq!(format_connections(&[]), "No database connections are saved.");
    }
}
