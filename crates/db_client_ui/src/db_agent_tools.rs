use crate::store::{CliQueryOutput, DatabaseStore};
use agent::{AgentTool, ToolCallEventStream, ToolInput};
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

/// Read-only guard: only statements that just read data are allowed for the
/// agent. A `WITH` CTE that ends in a DML statement is not detected here, so
/// it is allowed by the leading keyword — writes are otherwise blocked.
fn is_read_only_sql(sql: &str) -> bool {
    let mut rest = sql.trim_start();
    while rest.starts_with("--") {
        let line_end = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
        rest = rest[line_end..].trim_start();
    }
    let keyword: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(
        keyword.as_str(),
        "SELECT" | "WITH" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN"
    )
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

/// Runs a read-only SQL query against a saved database connection and returns
/// the rows. Only read statements are allowed (SELECT, WITH, SHOW, DESCRIBE,
/// EXPLAIN); statements that change data or schema are rejected. The connection
/// is opened automatically if it is not connected yet.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct DbRunQueryInput {
    /// Connection id or label (see `db_list_connections`).
    pub connection: String,
    /// Database/schema to run against. Defaults to the connection's database.
    #[serde(default)]
    pub database: Option<String>,
    /// The read-only SQL statement to run.
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
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let store = match store(cx) {
            Ok(store) => store,
            Err(message) => return Task::ready(Err(message)),
        };
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            if !is_read_only_sql(&input.sql) {
                return Err("This statement changes data or schema. The database agent is read-only; run writes and DDL from the Database Explorer panel instead.".to_string());
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
