use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::TryStreamExt as _;
use smol::lock::Mutex as AsyncMutex;
use sqlx::AssertSqlSafe;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgPool, PgPoolOptions, PgSslMode};
use sqlx::{Column as _, Connection as _, Row as _, ValueRef as _};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::MAX_RESULT_ROWS;
use crate::connection::{ConnectionConfig, SslMode};
use crate::provider::DbProvider;
use crate::schema::{
    CheckConstraintInfo, ColumnInfo, DatabaseInfo, FkInfo, IndexInfo, ProcedureInfo, ProcedureKind,
    QueryResult, SequenceInfo, TableInfo, TableKind, TriggerInfo, UserInfo,
};

pub struct PostgresProvider {
    pool: PgPool,
    /// Kept so a held transaction can open a physical connection of its own
    /// rather than borrow the pool's -- see [`HeldTransaction`].
    connect_options: PgConnectOptions,
    /// How long a transaction here may sit idle before it is given up on, or
    /// nothing where the reader asked for no limit.
    ///
    /// Read from the connection's own settings at connect time and kept, so
    /// that a transaction the reader opens by typing the statement themselves
    /// is guarded by the same number as one opened through the editor. Two
    /// numbers for the same thing is how a guard ends up meaning nothing.
    transaction_idle_limit: Option<Duration>,
    held_transaction: AsyncMutex<Option<HeldTransaction>>,
    /// When the held transaction opened, deliberately outside the async mutex
    /// above. A statement running inside the transaction holds that mutex for
    /// as long as the statement takes, so a `transaction_open_since` that
    /// waited for it would freeze whatever displays the elapsed time, and one
    /// that gave up on it would report "no transaction" for the whole of every
    /// long statement -- the two moments a reader most wants to see the clock.
    /// Written only under the async mutex, in the same critical section that
    /// sets or clears the transaction, so the two cannot disagree.
    transaction_opened_at: Mutex<Option<Instant>>,
}

/// A transaction the console holds open across statements, and the one physical
/// connection it lives on.
struct HeldTransaction {
    /// A standalone connection, established from `connect_options` rather than
    /// taken from the pool.
    ///
    /// Not from the pool for two reasons, either of them sufficient. First,
    /// `connect` sets only `max_connections(1)` and leaves sqlx's pool
    /// lifecycle defaults in place -- `idle_timeout` of 10 minutes and
    /// `max_lifetime` of 30 minutes -- so a pooled connection sitting inside a
    /// transaction while the reader thinks about their next statement would be
    /// closed out from under them with nothing said: the statement after it
    /// would run outside the transaction they believed they were in, and a
    /// commit would report success over work the server had already discarded.
    /// Second, that pool has exactly one connection, so a transaction holding
    /// it would leave nothing for schema browsing to run on at all.
    connection: PgConnection,
    /// The schema `search_path` was last set to on this connection. The switch
    /// is a round trip of its own, so it is not repeated for a schema already
    /// current, and dropping the connection is what invalidates it.
    search_path: String,
}

/// What a statement the reader typed does to the transaction the console is
/// holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionEffect {
    /// Opens one: `BEGIN`, `BEGIN TRANSACTION ISOLATION LEVEL ...`,
    /// `START TRANSACTION`.
    Opens,
    /// Ends the one that is open, whether or not the work survives: `COMMIT`,
    /// `END`, `ROLLBACK`, `ABORT`, `PREPARE TRANSACTION`.
    Ends,
    /// Leaves it exactly as it was.
    Neither,
}

/// Classifies a statement by what it does to an open transaction.
///
/// This exists because a transaction the editor did not notice being opened is
/// the worst state to be in: it holds its locks, nothing on screen says it is
/// there, and nothing will ever close it. So a `BEGIN` the reader typed has to
/// be recognised as opening one, and a `COMMIT` as closing it, exactly as if
/// the editor's own buttons had been pressed.
///
/// Two shapes are easy to get wrong. `ROLLBACK TO SAVEPOINT x` (and its
/// `ROLLBACK TO x` short form) reads like a rollback and ends nothing -- it
/// undoes back to a point and leaves the transaction open and still holding
/// everything. And `COMMIT PREPARED 'x'` / `ROLLBACK PREPARED 'x'` act on some
/// other, already-prepared transaction by name and cannot even run inside a
/// transaction block, so they end nothing here either.
///
/// PostgreSQL has no server-side autocommit setting to watch for. `psql`'s
/// `\set AUTOCOMMIT off` is the client wrapping statements in `BEGIN` itself,
/// and `SET autocommit` was removed from the server long ago, so unlike MySQL
/// there is no statement here that silently changes whether the next statement
/// starts a transaction.
fn transaction_effect(sql: &str) -> TransactionEffect {
    let words = statement_keywords(sql);
    let mut words = words.iter().map(String::as_str);
    match (words.next(), words.next()) {
        (Some("BEGIN"), _) | (Some("START"), Some("TRANSACTION")) => TransactionEffect::Opens,
        (Some("ABORT"), _) => TransactionEffect::Ends,
        (Some("COMMIT") | Some("ROLLBACK"), Some("PREPARED")) => TransactionEffect::Neither,
        (Some("ROLLBACK"), Some("TO")) => TransactionEffect::Neither,
        (Some("COMMIT") | Some("ROLLBACK") | Some("END"), _) => TransactionEffect::Ends,
        (Some("PREPARE"), Some("TRANSACTION")) => TransactionEffect::Ends,
        _ => TransactionEffect::Neither,
    }
}

/// The leading words of a statement, upper-cased, with the leading comments,
/// surrounding whitespace and trailing semicolons that a reader's editor buffer
/// is full of taken off.
///
/// Only the first few words are ever looked at, so the rest of the statement is
/// not scanned; every caller here decides on at most two.
fn statement_keywords(sql: &str) -> Vec<String> {
    let mut rest = sql.trim_start();
    // Every statement this crate sends gets an `ApplicationName` comment
    // prepended, so the first word of a statement as the server sees it is
    // routinely not the first word of the text.
    while let Some(after_open) = rest.strip_prefix("/*") {
        match after_open.find("*/") {
            Some(end) => rest = after_open[end + 2..].trim_start(),
            None => return Vec::new(),
        }
    }
    rest.split(|character: char| character.is_whitespace() || character == ';')
        .filter(|word| !word.is_empty())
        .take(2)
        .map(str::to_uppercase)
        .collect()
}

/// The session-scope guard that ends an abandoned transaction even though the
/// editor is not there to end it.
///
/// `idle_in_transaction_session_timeout` takes milliseconds when no unit is
/// given, zero disables it, and when it fires the server *terminates the
/// session*, which rolls the transaction back and releases its locks. That last
/// part is why it is this parameter and not `statement_timeout`: a statement
/// timeout aborts the statement and leaves the transaction open and holding
/// everything, which is not a guard against abandonment at all.
///
/// Clamped to at least one millisecond because a patience under half a
/// millisecond would round to zero, and zero means "never give up" -- the exact
/// opposite of what asking for a short one says. Clamped at the top to what the
/// parameter can hold.
fn idle_in_transaction_timeout_statement(abandoned_after: Duration) -> String {
    let milliseconds = abandoned_after.as_millis().clamp(1, i32::MAX as u128);
    format!("SET SESSION idle_in_transaction_session_timeout = {milliseconds}")
}

/// Whether a statement returns rows, and so has to be read as a stream rather
/// than executed for a row count.
fn is_read_query(sql: &str) -> bool {
    let trimmed_upper = sql.trim().to_uppercase();
    trimmed_upper.starts_with("SELECT")
        || trimmed_upper.starts_with("SHOW")
        || trimmed_upper.starts_with("EXPLAIN")
        || trimmed_upper.starts_with("DESCRIBE")
        || trimmed_upper.starts_with("DESC")
        || trimmed_upper.starts_with("TABLE")
        || trimmed_upper.starts_with("WITH")
}

fn search_path_statement(schema: &str) -> String {
    format!("SET search_path = \"{}\"", schema.replace('"', "\"\""))
}

fn prefixed_statement(sql: &str) -> String {
    format!(
        "{}{}",
        crate::application_name_comment(crate::DEFAULT_APPLICATION_NAME),
        sql
    )
}

fn rows_result(
    columns: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
    start: Instant,
) -> QueryResult {
    let rows_affected = rows.len() as u64;
    QueryResult {
        raw_documents: None,
        columns,
        rows,
        rows_affected,
        execution_time_ms: start.elapsed().as_millis() as u64,
        timing: None,
    }
}

fn affected_result(rows_affected: u64, start: Instant) -> QueryResult {
    QueryResult {
        raw_documents: None,
        columns: vec![],
        rows: vec![],
        rows_affected,
        execution_time_ms: start.elapsed().as_millis() as u64,
        timing: None,
    }
}

async fn execute_statement<'e, E>(executor: E, sql: &str) -> Result<u64>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query(AssertSqlSafe(sql))
        .execute(executor)
        .await
        .context("Query execution failed")?;
    Ok(result.rows_affected())
}

/// Takes the connection itself rather than something to run on, because this
/// needs two turns on it: the rows, and then -- only when there were none --
/// what columns the statement would have returned. It has to be the same
/// connection: a table created inside the reader's transaction does not exist
/// for any other.
async fn collect_rows(
    connection: &mut sqlx::PgConnection,
    sql: &str,
) -> Result<(Vec<String>, Vec<Vec<Option<String>>>)> {
    let mut stream = sqlx::raw_sql(AssertSqlSafe(sql)).fetch(&mut *connection);
    let mut columns: Vec<String> = Vec::new();
    let mut result_rows: Vec<Vec<Option<String>>> = Vec::new();

    while let Some(row) = stream.try_next().await.context("Query execution failed")? {
        if columns.is_empty() {
            columns = row
                .columns()
                .iter()
                .map(|column| column.name().to_string())
                .collect();
        }
        let decoded: Vec<Option<String>> = (0..columns.len())
            .map(|index| PostgresProvider::extract_cell(&row, index))
            .collect();
        result_rows.push(decoded);

        if result_rows.len() >= MAX_RESULT_ROWS {
            break;
        }
    }
    drop(stream);
    if columns.is_empty() {
        columns = the_columns_the_statement_returns(connection, sql).await;
    }
    Ok((columns, result_rows))
}

/// The column names a statement would return, asked of the server rather than
/// read off a row.
///
/// A result with no rows has no row to take the names from, and a grid with no
/// columns is not an empty result -- it is a table the reader cannot see the
/// shape of, and cannot add a row to. Asked for only in that case: it is one
/// more round trip, and a round trip to a distant server costs as much as the
/// query did.
///
/// Best effort on purpose. A statement the server will not describe leaves the
/// grid as it was rather than turning an empty result into an error.
async fn the_columns_the_statement_returns(
    connection: &mut sqlx::PgConnection,
    sql: &str,
) -> Vec<String> {
    use sqlx::{Executor as _, SqlSafeStr as _, Statement as _};

    // Prepared rather than described: `describe` is hidden behind a feature
    // meant for the query macros, while preparing a statement is a public way
    // to ask the same thing and gives the columns it would return.
    let statement = AssertSqlSafe(sql.to_string()).into_sql_str();
    match (&mut *connection).prepare(statement).await {
        Ok(prepared) => prepared
            .columns()
            .iter()
            .map(|column| column.name().to_string())
            .collect(),
        // A statement the server will not describe leaves the grid as it was
        // rather than turning an empty result into an error.
        Err(_) => Vec::new(),
    }
}

/// Unlike [`collect_rows`], this never breaks at `MAX_RESULT_ROWS` — the whole
/// point of "execute to file" is exporting result sets too large for the grid.
async fn stream_rows(
    connection: &mut sqlx::PgConnection,
    sql: &str,
    sink: &mut dyn crate::provider::RowSink,
) -> Result<u64> {
    let mut stream = sqlx::raw_sql(AssertSqlSafe(sql)).fetch(&mut *connection);
    let mut columns: Vec<String> = Vec::new();
    let mut row_count: u64 = 0;

    while let Some(row) = stream.try_next().await.context("Query execution failed")? {
        if columns.is_empty() {
            columns = row
                .columns()
                .iter()
                .map(|column| column.name().to_string())
                .collect();
            sink.write_columns(&columns)?;
        }
        let decoded: Vec<Option<String>> = (0..columns.len())
            .map(|index| PostgresProvider::extract_cell(&row, index))
            .collect();
        sink.write_row(&decoded)?;
        row_count += 1;
    }

    if columns.is_empty() {
        // The same reason the grid gets its columns asked for: a file whose
        // header row is missing does not say the result was empty, it says
        // nothing at all about what was exported.
        drop(stream);
        let asked = the_columns_the_statement_returns(connection, sql).await;
        sink.write_columns(&asked)?;
    }
    Ok(row_count)
}

async fn run_on_pool(
    pool: &PgPool,
    schema: &str,
    sql: &str,
    prefixed: &str,
) -> Result<QueryResult> {
    if !schema.is_empty() {
        let set_path = search_path_statement(schema);
        sqlx::query(AssertSqlSafe(set_path.as_str()))
            .execute(pool)
            .await
            .context("Failed to set search_path")?;
    }

    let start = Instant::now();
    if is_read_query(sql) {
        let mut connection = pool
            .acquire()
            .await
            .context("Failed to take a connection for the query")?;
        let (columns, rows) = collect_rows(&mut connection, prefixed).await?;
        Ok(rows_result(columns, rows, start))
    } else {
        let rows_affected = execute_statement(pool, prefixed).await?;
        Ok(affected_result(rows_affected, start))
    }
}

fn postgres_ssl_mode(mode: SslMode) -> PgSslMode {
    match mode {
        SslMode::Disabled => PgSslMode::Disable,
        SslMode::Require => PgSslMode::Require,
        SslMode::VerifyCa => PgSslMode::VerifyCa,
        SslMode::VerifyFull => PgSslMode::VerifyFull,
    }
}

pub(crate) fn postgres_connect_options(config: &ConnectionConfig) -> PgConnectOptions {
    let mut opts = PgConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .username(&config.username)
        .password(&config.password)
        .database(config.database.as_deref().unwrap_or("postgres"))
        .ssl_mode(postgres_ssl_mode(config.ssl_mode));
    if let Some(ca_path) = &config.ssl_ca_path {
        opts = opts.ssl_root_cert(ca_path);
    }
    if let Some(cert_path) = &config.ssl_client_cert_path {
        opts = opts.ssl_client_cert(cert_path);
    }
    if let Some(key_path) = &config.ssl_client_key_path {
        opts = opts.ssl_client_key(key_path);
    }
    opts
}

/// Quotes an identifier the way the server would, doubling any quote inside it
/// so that a name holding one cannot end the quoting early.
fn quoted(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Quotes a string literal for `COMMENT ON`, doubling any apostrophe inside it.
fn quoted_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Puts the pieces the catalogue gave back together into one statement, then the
/// indexes and comments after it. Kept apart from the reading so that what it
/// writes can be checked without a server.
fn render_table_ddl(
    schema: &str,
    table: &str,
    columns: &[(String, String, bool, Option<String>, Option<String>)],
    constraints: &[(String, String)],
    indexes: &[(String,)],
    table_comment: Option<&str>,
) -> String {
    let qualified = format!("{}.{}", quoted(schema), quoted(table));
    let mut lines: Vec<String> = Vec::with_capacity(columns.len() + constraints.len());

    for (name, kind, not_null, default, _comment) in columns {
        let mut line = format!("  {} {}", quoted(name), kind);
        if let Some(default) = default {
            line.push_str(&format!(" DEFAULT {default}"));
        }
        if *not_null {
            line.push_str(" NOT NULL");
        }
        lines.push(line);
    }
    for (name, definition) in constraints {
        lines.push(format!("  CONSTRAINT {} {}", quoted(name), definition));
    }

    let mut ddl = format!("CREATE TABLE {qualified} (\n{}\n);\n", lines.join(",\n"));

    if !indexes.is_empty() {
        ddl.push('\n');
        for (definition,) in indexes {
            ddl.push_str(definition);
            ddl.push_str(";\n");
        }
    }

    let commented: Vec<String> = std::iter::once(table_comment.map(|comment| {
        format!(
            "COMMENT ON TABLE {qualified} IS {};",
            quoted_literal(comment)
        )
    }))
    .chain(columns.iter().map(|(name, _, _, _, comment)| {
        comment.as_deref().map(|comment| {
            format!(
                "COMMENT ON COLUMN {qualified}.{} IS {};",
                quoted(name),
                quoted_literal(comment)
            )
        })
    }))
    .flatten()
    .collect();
    if !commented.is_empty() {
        ddl.push('\n');
        ddl.push_str(&commented.join("\n"));
        ddl.push('\n');
    }

    ddl
}

impl PostgresProvider {
    /// The schema and table as the catalogue actually spells them, or nothing if
    /// it holds no such table.
    ///
    /// A name reaches here however the reader wrote it. Postgres folds an
    /// unquoted identifier to lower case as it parses it, so a name copied out of
    /// a query that runs perfectly well -- `ciqestimatenumericData` -- is not the
    /// name stored against the table; and a table created with quotes keeps a
    /// spelling nobody will type back exactly. Matching without regard to case
    /// answers both, and an exact match still wins, so two tables differing only
    /// in case each find themselves.
    async fn stored_table_name(
        &self,
        schema: &str,
        table: &str,
    ) -> Result<Option<(String, String)>> {
        let found = sqlx::query_as::<_, (String, String)>(
            "-- name: ResolveTableName :one
             SELECT table_schema, table_name
             FROM information_schema.tables
             WHERE lower(table_schema) = lower($1)
               AND lower(table_name) = lower($2)
             ORDER BY (table_schema = $1 AND table_name = $2) DESC,
                      table_schema,
                      table_name
             LIMIT 1",
        )
        .bind(schema)
        .bind(table)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to look up the table")?;
        Ok(found)
    }

    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let opts = postgres_connect_options(config);
        // Single connection: `execute_query` relies on `SET search_path`
        // staying applied for the query that follows it, which only holds
        // when both run on the same physical connection. The metadata
        // queries are fully qualified, so serializing them through one
        // connection is acceptable for a single-user GUI client.
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(opts.clone())
            .await
            .context("Failed to connect to PostgreSQL")?;
        Ok(Self {
            pool,
            connect_options: opts,
            transaction_idle_limit: config.transaction_idle_limit(),
            held_transaction: AsyncMutex::new(None),
            transaction_opened_at: Mutex::new(None),
        })
    }

    /// Opens a connection of its own, tells the server how long to wait on it
    /// in silence, and runs `opening_statement` on it.
    ///
    /// Takes the statement rather than always sending `BEGIN` so that a reader
    /// who typed `BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE` gets the
    /// isolation level they asked for instead of a plain `BEGIN` alongside it.
    ///
    /// The guard is set before the transaction opens, not inside it: `SET`
    /// issued inside a transaction block is undone when that block ends, so a
    /// guard set there would be gone by the time the next transaction on this
    /// connection needed it.
    async fn open_held_transaction(
        &self,
        schema: &str,
        abandoned_after: Option<Duration>,
        opening_statement: &str,
    ) -> Result<HeldTransaction> {
        let mut connection = PgConnection::connect_with(&self.connect_options)
            .await
            .context("Failed to open a connection for the transaction")?;

        // Nothing is set where no limit was asked for: the reader stepping
        // through a migration by hand has said they want none, and a value
        // invented here would terminate their session under them.
        if let Some(abandoned_after) = abandoned_after {
            let guard = idle_in_transaction_timeout_statement(abandoned_after);
            sqlx::query(AssertSqlSafe(guard.as_str()))
                .execute(&mut connection)
                .await
                .context("Failed to set the transaction's idle timeout")?;
        }

        let mut search_path = String::new();
        if !schema.is_empty() {
            let set_path = search_path_statement(schema);
            sqlx::query(AssertSqlSafe(set_path.as_str()))
                .execute(&mut connection)
                .await
                .context("Failed to set search_path")?;
            search_path = schema.to_string();
        }

        sqlx::query(AssertSqlSafe(opening_statement))
            .execute(&mut connection)
            .await
            .context("Failed to begin the transaction")?;

        Ok(HeldTransaction {
            connection,
            search_path,
        })
    }

    /// Decides whether a transaction whose statement just failed is still there
    /// to be finished by hand.
    ///
    /// A failed statement normally leaves the transaction open and aborted, and
    /// only `ROLLBACK` gets out of that -- so it must stay held. But the same
    /// failure is what the reader sees when the server has ended the session
    /// itself, whether on the idle guard or otherwise, and holding a dead
    /// connection would wedge the console: every later statement, `ROLLBACK`
    /// included, would fail on it forever with no way left to clear it.
    async fn keep_if_alive(mut transaction: HeldTransaction) -> Option<HeldTransaction> {
        match transaction.connection.ping().await {
            Ok(()) => Some(transaction),
            Err(_) => None,
        }
    }

    /// Hangs up on a transaction's connection, having already committed or
    /// rolled it back.
    async fn release(transaction: HeldTransaction) -> Result<()> {
        transaction
            .connection
            .close()
            .await
            .context("Failed to close the transaction's connection")
    }

    /// Points `search_path` at `schema` on the held connection, unless it is
    /// already there.
    async fn point_at_schema(transaction: &mut HeldTransaction, schema: &str) -> Result<()> {
        if schema.is_empty() || transaction.search_path == schema {
            return Ok(());
        }
        let set_path = search_path_statement(schema);
        sqlx::query(AssertSqlSafe(set_path.as_str()))
            .execute(&mut transaction.connection)
            .await
            .context("Failed to set search_path")?;
        transaction.search_path = schema.to_string();
        Ok(())
    }

    async fn run_held(
        transaction: &mut HeldTransaction,
        schema: &str,
        sql: &str,
        prefixed: &str,
    ) -> Result<QueryResult> {
        Self::point_at_schema(transaction, schema).await?;
        let start = Instant::now();
        if is_read_query(sql) {
            let (columns, rows) = collect_rows(&mut transaction.connection, prefixed).await?;
            Ok(rows_result(columns, rows, start))
        } else {
            let rows_affected = execute_statement(&mut transaction.connection, prefixed).await?;
            Ok(affected_result(rows_affected, start))
        }
    }

    // A poisoned lock here means a panic happened while a timestamp was being
    // written; the timestamp itself cannot be left half-written, so the value
    // is taken back rather than propagated as an error nobody could act on.
    fn record_transaction_opened_at(&self, opened_at: Option<Instant>) {
        match self.transaction_opened_at.lock() {
            Ok(mut slot) => *slot = opened_at,
            Err(poisoned) => *poisoned.into_inner() = opened_at,
        }
    }

    fn transaction_opened_at(&self) -> Option<Instant> {
        match self.transaction_opened_at.lock() {
            Ok(slot) => *slot,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    /// Renders one cell as the text a reader sees, which is the text `psql` shows
    /// for the same value.
    ///
    /// Rows are read over the simple query protocol, the one `psql` uses, so the
    /// server has already rendered every value with its own output function: a
    /// `numeric` keeps its declared digits, a `bytea` comes as `\x…`, a boolean
    /// as `t` or `f`, a list as `{1,2,3}`, an interval as `1 day 02:00:00`, money
    /// with the server's own currency format. Taking that text as it stands is
    /// both exact and free of a per-type rule here to get wrong.
    fn extract_cell(row: &sqlx::postgres::PgRow, index: usize) -> Option<String> {
        let value = row.try_get_raw(index).ok()?;
        if value.is_null() {
            return None;
        }
        value.as_str().ok().map(str::to_string)
    }
}

#[async_trait]
impl DbProvider for PostgresProvider {
    async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .context("Ping failed")?;
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
        let rows = sqlx::query_as::<_, (String,)>(
            "-- name: ListSchemas :many
             SELECT schema_name
             FROM information_schema.schemata
             WHERE schema_name NOT IN ('pg_catalog', 'information_schema')
               AND schema_name NOT LIKE 'pg_toast%'
               AND schema_name NOT LIKE 'pg_temp_%'
             ORDER BY schema_name",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list schemas")?;
        Ok(rows
            .into_iter()
            .map(|(name,)| DatabaseInfo { name })
            .collect())
    }

    async fn list_tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "-- name: ListTables :many
             SELECT table_name, table_type
             FROM information_schema.tables
             WHERE table_schema = $1
             ORDER BY table_name",
        )
        .bind(schema)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list tables")?;
        Ok(rows
            .into_iter()
            .map(|(name, table_type)| TableInfo {
                name,
                kind: if table_type == "VIEW" {
                    TableKind::View
                } else {
                    TableKind::Table
                },
            })
            .collect())
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        // Same reason as the DDL: the name arrives spelled however it was
        // written, and the catalogue holds one spelling.
        let (schema, table) = self
            .stored_table_name(schema, table)
            .await?
            .unwrap_or_else(|| (schema.to_string(), table.to_string()));
        let rows = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>)>(
            "-- name: DescribeTable :many
             SELECT column_name, data_type, is_nullable, column_default, ''
             FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = $2
             ORDER BY ordinal_position",
        )
        .bind(&schema)
        .bind(&table)
        .fetch_all(&self.pool)
        .await
        .context("Failed to describe table")?;
        Ok(rows
            .into_iter()
            .map(
                |(name, data_type, nullable, default_value, _extra)| ColumnInfo {
                    name,
                    data_type,
                    is_nullable: nullable == "YES",
                    column_key: None,
                    default_value,
                    extra: String::new(),
                },
            )
            .collect())
    }

    /// The table as the server itself describes it: exact types, every
    /// constraint, every index and every comment.
    ///
    /// Read from the system catalogue rather than `information_schema`, which
    /// cannot say what a column's type really is -- it reports `numeric` for a
    /// `numeric(32,9)`, splitting the precision into columns of its own -- and
    /// knows nothing of indexes. The catalogue's own functions print each
    /// definition the way the server would, so nothing here is reassembled by
    /// hand and nothing is guessed.
    async fn get_table_ddl(&self, schema: &str, table: &str) -> Result<String> {
        let Some((schema, table)) = self.stored_table_name(schema, table).await? else {
            anyhow::bail!("there is no table \"{schema}\".\"{table}\" in this database");
        };

        let columns = sqlx::query_as::<_, (String, String, bool, Option<String>, Option<String>)>(
            "-- name: GetTableColumnsForDDL :many
             SELECT attribute.attname,
                    format_type(attribute.atttypid, attribute.atttypmod),
                    attribute.attnotnull,
                    pg_get_expr(fallback.adbin, fallback.adrelid),
                    col_description(attribute.attrelid, attribute.attnum)
             FROM pg_attribute attribute
             JOIN pg_class relation ON relation.oid = attribute.attrelid
             JOIN pg_namespace space ON space.oid = relation.relnamespace
             LEFT JOIN pg_attrdef fallback
                    ON fallback.adrelid = attribute.attrelid
                   AND fallback.adnum = attribute.attnum
             WHERE space.nspname = $1
               AND relation.relname = $2
               AND attribute.attnum > 0
               AND NOT attribute.attisdropped
             ORDER BY attribute.attnum",
        )
        .bind(&schema)
        .bind(&table)
        .fetch_all(&self.pool)
        .await
        .context("Failed to read the table's columns")?;

        // Printed by the server, so a check expression or a foreign key reads
        // exactly as it would in a dump. Primary key first, then unique, then
        // foreign keys, then checks -- the order a reader expects.
        let constraints = sqlx::query_as::<_, (String, String)>(
            "-- name: GetTableConstraintsForDDL :many
             SELECT constraint_.conname, pg_get_constraintdef(constraint_.oid)
             FROM pg_constraint constraint_
             JOIN pg_class relation ON relation.oid = constraint_.conrelid
             JOIN pg_namespace space ON space.oid = relation.relnamespace
             WHERE space.nspname = $1 AND relation.relname = $2
             ORDER BY CASE constraint_.contype
                        WHEN 'p' THEN 0 WHEN 'u' THEN 1
                        WHEN 'f' THEN 2 ELSE 3
                      END,
                      constraint_.conname",
        )
        .bind(&schema)
        .bind(&table)
        .fetch_all(&self.pool)
        .await
        .context("Failed to read the table's constraints")?;

        // An index that exists only to enforce a constraint is left out: it is
        // already stated by the constraint above, and repeating it would read as
        // a second index that is not there.
        let indexes = sqlx::query_as::<_, (String,)>(
            "-- name: GetTableIndexesForDDL :many
             SELECT pg_get_indexdef(index_.indexrelid)
             FROM pg_index index_
             JOIN pg_class relation ON relation.oid = index_.indrelid
             JOIN pg_class index_relation ON index_relation.oid = index_.indexrelid
             JOIN pg_namespace space ON space.oid = relation.relnamespace
             WHERE space.nspname = $1
               AND relation.relname = $2
               AND NOT EXISTS (
                     SELECT 1 FROM pg_constraint constraint_
                     WHERE constraint_.conindid = index_.indexrelid
                   )
             ORDER BY index_relation.relname",
        )
        .bind(&schema)
        .bind(&table)
        .fetch_all(&self.pool)
        .await
        .context("Failed to read the table's indexes")?;

        let table_comment = sqlx::query_as::<_, (Option<String>,)>(
            "-- name: GetTableCommentForDDL :one
             SELECT obj_description(relation.oid)
             FROM pg_class relation
             JOIN pg_namespace space ON space.oid = relation.relnamespace
             WHERE space.nspname = $1 AND relation.relname = $2",
        )
        .bind(&schema)
        .bind(&table)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to read the table's comment")?
        .and_then(|(comment,)| comment);

        Ok(render_table_ddl(
            &schema,
            &table,
            &columns,
            &constraints,
            &indexes,
            table_comment.as_deref(),
        ))
    }

    async fn get_database_ddl(&self, database: &str) -> Result<String> {
        let owner = sqlx::query_as::<_, (Option<String>,)>(
            "-- name: GetSchemaOwner :one
             SELECT schema_owner FROM information_schema.schemata
             WHERE schema_name = $1",
        )
        .bind(database)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to query schema metadata")?;

        let escaped = database.replace('"', "\"\"");
        match owner {
            Some((Some(owner),)) => Ok(format!(
                "CREATE SCHEMA \"{}\" AUTHORIZATION \"{}\";\n",
                escaped,
                owner.replace('"', "\"\"")
            )),
            _ => Ok(format!("CREATE SCHEMA \"{}\";\n", escaped)),
        }
    }

    fn holds_transactions(&self) -> bool {
        true
    }

    async fn begin_transaction(
        &self,
        schema: &str,
        abandoned_after: Option<Duration>,
    ) -> Result<()> {
        let mut held = self.held_transaction.lock().await;
        anyhow::ensure!(
            held.is_none(),
            "a transaction is already open on this connection"
        );
        let transaction = self
            .open_held_transaction(schema, abandoned_after, "BEGIN")
            .await?;
        self.record_transaction_opened_at(Some(Instant::now()));
        *held = Some(transaction);
        Ok(())
    }

    async fn commit_transaction(&self) -> Result<()> {
        let mut held = self.held_transaction.lock().await;
        let mut transaction = held
            .take()
            .context("no transaction is open on this connection")?;
        match sqlx::query("COMMIT")
            .execute(&mut transaction.connection)
            .await
        {
            Ok(_) => {
                self.record_transaction_opened_at(None);
                Self::release(transaction).await
            }
            Err(error) => {
                // A commit can fail on its own merits -- a deferred
                // constraint, a serialization failure -- and PostgreSQL rolls
                // the transaction back when it does. Keeping it held anyway,
                // when the connection is still there, is the honest state to
                // be in: nothing here can tell the reader's transaction is
                // gone, and offering them a rollback that reports "nothing
                // open" would be worse than one that says so from the server.
                match Self::keep_if_alive(transaction).await {
                    Some(transaction) => *held = Some(transaction),
                    None => self.record_transaction_opened_at(None),
                }
                Err(error).context("Failed to commit the transaction")
            }
        }
    }

    async fn rollback_transaction(&self) -> Result<()> {
        let mut held = self.held_transaction.lock().await;
        let Some(mut transaction) = held.take() else {
            return Ok(());
        };
        self.record_transaction_opened_at(None);
        let hung_up = sqlx::query("ROLLBACK")
            .execute(&mut transaction.connection)
            .await
            .is_err();
        // A failed `ROLLBACK` is not a failure to roll back. Hanging up makes
        // the server roll the transaction back and release its locks either
        // way, and the ordinary reason the statement fails is that the server
        // has already ended the session for us. What the failure does say is
        // that waiting for a polite shutdown handshake would wait on a socket
        // nobody is reading, so drop the connection instead of closing it.
        if hung_up {
            return Ok(());
        }
        Self::release(transaction).await
    }

    fn transaction_open_since(&self) -> Option<Instant> {
        self.transaction_opened_at()
    }

    async fn execute_query(&self, schema: &str, sql: &str) -> Result<QueryResult> {
        // The reader's own statements are the only ones routed to the held
        // connection. Every metadata and schema call in this file goes to the
        // pool instead, on purpose: browsing a schema must not become part of
        // the reader's transaction, where it would read under their snapshot
        // and hold its own locks until they were finished.
        let mut held = self.held_transaction.lock().await;
        let effect = transaction_effect(sql);
        let prefixed = prefixed_statement(sql);

        let Some(mut transaction) = held.take() else {
            if effect == TransactionEffect::Opens {
                let transaction = self
                    .open_held_transaction(schema, self.transaction_idle_limit, prefixed.as_str())
                    .await?;
                self.record_transaction_opened_at(Some(Instant::now()));
                *held = Some(transaction);
                return Ok(affected_result(0, Instant::now()));
            }
            return run_on_pool(&self.pool, schema, sql, prefixed.as_str()).await;
        };

        let outcome = Self::run_held(&mut transaction, schema, sql, prefixed.as_str()).await;
        match outcome {
            Ok(result) => {
                if effect == TransactionEffect::Ends {
                    self.record_transaction_opened_at(None);
                    Self::release(transaction).await?;
                } else {
                    *held = Some(transaction);
                }
                Ok(result)
            }
            Err(error) => {
                match Self::keep_if_alive(transaction).await {
                    Some(transaction) => *held = Some(transaction),
                    None => self.record_transaction_opened_at(None),
                }
                Err(error)
            }
        }
    }

    async fn execute_query_streaming(
        &self,
        schema: &str,
        sql: &str,
        sink: &mut dyn crate::provider::RowSink,
    ) -> Result<u64> {
        // Routed exactly as `execute_query` is, and for the same reason: an
        // export the reader asked for inside their transaction has to see the
        // rows their transaction sees, not the rows everyone else does.
        let mut held = self.held_transaction.lock().await;
        let effect = transaction_effect(sql);
        let prefixed = prefixed_statement(sql);

        let Some(mut transaction) = held.take() else {
            if effect == TransactionEffect::Opens {
                let transaction = self
                    .open_held_transaction(schema, self.transaction_idle_limit, prefixed.as_str())
                    .await?;
                self.record_transaction_opened_at(Some(Instant::now()));
                *held = Some(transaction);
                sink.write_columns(&[])?;
                return Ok(0);
            }
            if !schema.is_empty() {
                let set_path = search_path_statement(schema);
                sqlx::query(AssertSqlSafe(set_path.as_str()))
                    .execute(&self.pool)
                    .await
                    .context("Failed to set search_path")?;
            }
            if !is_read_query(sql) {
                execute_statement(&self.pool, prefixed.as_str()).await?;
                return Ok(0);
            }
            let mut connection = self
                .pool
                .acquire()
                .await
                .context("Failed to take a connection for the export")?;
            return stream_rows(&mut connection, prefixed.as_str(), sink).await;
        };

        let outcome = async {
            Self::point_at_schema(&mut transaction, schema).await?;
            if !is_read_query(sql) {
                execute_statement(&mut transaction.connection, prefixed.as_str()).await?;
                return Ok(0);
            }
            stream_rows(&mut transaction.connection, prefixed.as_str(), sink).await
        }
        .await;

        match outcome {
            Ok(row_count) => {
                if effect == TransactionEffect::Ends {
                    self.record_transaction_opened_at(None);
                    Self::release(transaction).await?;
                } else {
                    *held = Some(transaction);
                }
                Ok(row_count)
            }
            Err(error) => {
                match Self::keep_if_alive(transaction).await {
                    Some(transaction) => *held = Some(transaction),
                    None => self.record_transaction_opened_at(None),
                }
                Err(error)
            }
        }
    }

    async fn list_indexes(&self, database: &str, table: &str) -> Result<Vec<IndexInfo>> {
        let rows = sqlx::query_as::<_, (String, String, bool, String)>(
            "-- name: ListIndexes :many
             SELECT i.relname, ARRAY_TO_STRING(ARRAY_AGG(a.attname ORDER BY x.ord), ','), ix.indisunique, am.amname
             FROM pg_index ix
             JOIN pg_class i ON i.oid = ix.indexrelid
             JOIN pg_class t ON t.oid = ix.indrelid
             JOIN pg_namespace n ON n.oid = t.relnamespace
             JOIN LATERAL UNNEST(ix.indkey) WITH ORDINALITY AS x(attnum, ord) ON TRUE
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = x.attnum
             JOIN pg_am am ON am.oid = i.relam
             WHERE n.nspname = $1 AND t.relname = $2
             GROUP BY i.relname, ix.indisunique, am.amname
             ORDER BY i.relname",
        )
        .bind(database)
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list indexes")?;

        Ok(rows
            .into_iter()
            .map(|(name, cols_csv, unique, index_type)| IndexInfo {
                name,
                columns: cols_csv.split(',').map(|s| s.to_string()).collect(),
                unique,
                index_type,
            })
            .collect())
    }

    async fn list_foreign_keys(&self, database: &str, table: &str) -> Result<Vec<FkInfo>> {
        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            "-- name: ListForeignKeys :many
             SELECT tc.constraint_name, kcu.column_name, ccu.table_name, ccu.column_name
             FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage kcu
               ON kcu.constraint_schema = tc.constraint_schema
              AND kcu.constraint_name = tc.constraint_name
             JOIN information_schema.constraint_column_usage ccu
               ON ccu.constraint_schema = tc.constraint_schema
              AND ccu.constraint_name = tc.constraint_name
             WHERE tc.constraint_type = 'FOREIGN KEY'
               AND tc.table_schema = $1 AND tc.table_name = $2
             ORDER BY tc.constraint_name, kcu.ordinal_position",
        )
        .bind(database)
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list foreign keys")?;

        Ok(rows
            .into_iter()
            .map(|(name, from_column, to_table, to_column)| FkInfo {
                name,
                from_column,
                to_table,
                to_column,
            })
            .collect())
    }

    async fn list_check_constraints(
        &self,
        database: &str,
        table: &str,
    ) -> Result<Vec<CheckConstraintInfo>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "-- name: ListCheckConstraints :many
             SELECT tc.constraint_name, cc.check_clause
             FROM information_schema.table_constraints tc
             JOIN information_schema.check_constraints cc
               ON cc.constraint_schema = tc.constraint_schema
              AND cc.constraint_name = tc.constraint_name
             WHERE tc.constraint_type = 'CHECK'
               AND tc.table_schema = $1 AND tc.table_name = $2
             ORDER BY tc.constraint_name",
        )
        .bind(database)
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list check constraints")?;

        Ok(rows
            .into_iter()
            .map(|(name, expression)| CheckConstraintInfo { name, expression })
            .collect())
    }

    async fn list_procedures(&self, database: &str) -> Result<Vec<ProcedureInfo>> {
        let rows = sqlx::query_as::<_, (String, String, Option<String>)>(
            "-- name: ListProcedures :many
             SELECT p.proname, p.prokind::text, pg_get_functiondef(p.oid)
             FROM pg_proc p
             JOIN pg_namespace n ON n.oid = p.pronamespace
             WHERE n.nspname = $1 AND p.prokind IN ('f', 'p')
             ORDER BY p.prokind, p.proname",
        )
        .bind(database)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list procedures")?;

        Ok(rows
            .into_iter()
            .map(|(name, prokind, definition)| ProcedureInfo {
                name,
                kind: if prokind == "f" {
                    ProcedureKind::Function
                } else {
                    ProcedureKind::Procedure
                },
                definition,
            })
            .collect())
    }

    async fn list_triggers(&self, database: &str, table: &str) -> Result<Vec<TriggerInfo>> {
        let rows = sqlx::query_as::<_, (String, String, String, String, String)>(
            "-- name: ListTriggers :many
             SELECT t.tgname,
               CASE WHEN t.tgtype & 4 <> 0 THEN 'INSERT' WHEN t.tgtype & 8 <> 0 THEN 'DELETE' ELSE 'UPDATE' END,
               CASE WHEN t.tgtype & 2 <> 0 THEN 'BEFORE' WHEN t.tgtype & 64 <> 0 THEN 'INSTEAD OF' ELSE 'AFTER' END,
               c.relname, pg_get_triggerdef(t.oid)
             FROM pg_trigger t
             JOIN pg_class c ON c.oid = t.tgrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND c.relname = $2 AND NOT t.tgisinternal
             ORDER BY t.tgname",
        )
        .bind(database)
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list triggers")?;

        Ok(rows
            .into_iter()
            .map(
                |(name, event, timing, table_name, definition)| TriggerInfo {
                    name,
                    event,
                    timing,
                    table_name,
                    definition: Some(definition),
                },
            )
            .collect())
    }

    async fn list_sequences(&self, database: &str) -> Result<Vec<SequenceInfo>> {
        let rows = sqlx::query_as::<_, (String, Option<i64>, Option<i64>)>(
            "-- name: ListSequences :many
             SELECT sequencename, last_value, increment_by
             FROM pg_sequences
             WHERE schemaname = $1
             ORDER BY sequencename",
        )
        .bind(database)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list sequences")?;

        Ok(rows
            .into_iter()
            .map(|(name, current_value, increment)| SequenceInfo {
                name,
                current_value,
                increment,
            })
            .collect())
    }

    async fn list_users(&self) -> Result<Vec<UserInfo>> {
        let rows = sqlx::query_as::<_, (String,)>(
            "-- name: ListUsers :many
             SELECT usename FROM pg_user ORDER BY usename",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list users")?;

        Ok(rows
            .into_iter()
            .map(|(name,)| UserInfo {
                name,
                host: String::new(),
                grants: Vec::new(),
            })
            .collect())
    }

    async fn truncate_table(&self, database: &str, table: &str) -> Result<()> {
        let sql = format!(
            "-- name: TruncateTable :exec\nTRUNCATE TABLE \"{}\".\"{}\"",
            database.replace('"', "\"\""),
            table.replace('"', "\"\""),
        );
        sqlx::query(AssertSqlSafe(sql.as_str()))
            .execute(&self.pool)
            .await
            .context("Failed to truncate table")?;
        Ok(())
    }

    async fn drop_table(&self, database: &str, table: &str) -> Result<()> {
        let sql = format!(
            "-- name: DropTable :exec\nDROP TABLE \"{}\".\"{}\"",
            database.replace('"', "\"\""),
            table.replace('"', "\"\""),
        );
        sqlx::query(AssertSqlSafe(sql.as_str()))
            .execute(&self.pool)
            .await
            .context("Failed to drop table")?;
        Ok(())
    }

    async fn rename_table(&self, database: &str, old_name: &str, new_name: &str) -> Result<()> {
        sqlx::query(AssertSqlSafe(rename_table_sql(
            database, old_name, new_name,
        )))
        .execute(&self.pool)
        .await
        .context("Failed to rename table")?;
        Ok(())
    }
}

fn rename_table_sql(database: &str, old_name: &str, new_name: &str) -> String {
    format!(
        "-- name: RenameTable :exec\nALTER TABLE \"{}\".\"{}\" RENAME TO \"{}\"",
        database.replace('"', "\"\""),
        old_name.replace('"', "\"\""),
        new_name.replace('"', "\"\""),
    )
}

#[cfg(test)]
mod rename_table_tests {
    use super::*;

    #[test]
    fn rename_table_sql_qualifies_the_schema_and_keeps_the_new_name_unqualified() {
        assert_eq!(
            rename_table_sql("public", "users", "customers"),
            "-- name: RenameTable :exec\nALTER TABLE \"public\".\"users\" RENAME TO \"customers\""
        );
    }

    #[test]
    fn rename_table_sql_escapes_embedded_double_quotes() {
        assert_eq!(
            rename_table_sql("pu\"blic", "us\"ers", "cust\"omers"),
            "-- name: RenameTable :exec\nALTER TABLE \"pu\"\"blic\".\"us\"\"ers\" RENAME TO \"cust\"\"omers\""
        );
    }
}

#[cfg(test)]
mod transaction_statement_tests {
    use super::*;

    #[test]
    fn classifies_every_statement_a_reader_can_type() {
        let cases: &[(&str, TransactionEffect)] = &[
            ("BEGIN", TransactionEffect::Opens),
            ("begin", TransactionEffect::Opens),
            ("  BeGiN ;  ", TransactionEffect::Opens),
            ("BEGIN;", TransactionEffect::Opens),
            ("BEGIN WORK", TransactionEffect::Opens),
            ("BEGIN TRANSACTION", TransactionEffect::Opens),
            (
                "begin transaction isolation level serializable",
                TransactionEffect::Opens,
            ),
            ("START TRANSACTION", TransactionEffect::Opens),
            ("start\ttransaction ;", TransactionEffect::Opens),
            ("COMMIT", TransactionEffect::Ends),
            ("commit;", TransactionEffect::Ends),
            ("Commit Work", TransactionEffect::Ends),
            ("END", TransactionEffect::Ends),
            ("end transaction", TransactionEffect::Ends),
            ("ROLLBACK", TransactionEffect::Ends),
            ("  rollback ; ", TransactionEffect::Ends),
            ("ROLLBACK TRANSACTION", TransactionEffect::Ends),
            ("ABORT", TransactionEffect::Ends),
            ("PREPARE TRANSACTION 'gid'", TransactionEffect::Ends),
            // Reads like a rollback and ends nothing: it undoes back to a
            // point and leaves the transaction open, still holding its locks.
            ("ROLLBACK TO SAVEPOINT a", TransactionEffect::Neither),
            ("rollback to savepoint a;", TransactionEffect::Neither),
            ("ROLLBACK TO a", TransactionEffect::Neither),
            ("SAVEPOINT a", TransactionEffect::Neither),
            ("savepoint a;", TransactionEffect::Neither),
            ("RELEASE SAVEPOINT a", TransactionEffect::Neither),
            ("RELEASE a", TransactionEffect::Neither),
            // These name someone else's prepared transaction and cannot even
            // run inside a transaction block.
            ("COMMIT PREPARED 'gid'", TransactionEffect::Neither),
            ("rollback prepared 'gid'", TransactionEffect::Neither),
            ("SELECT 1", TransactionEffect::Neither),
            ("UPDATE t SET a = 1", TransactionEffect::Neither),
            ("", TransactionEffect::Neither),
            ("   ", TransactionEffect::Neither),
            (";", TransactionEffect::Neither),
        ];
        for (sql, expected) in cases {
            assert_eq!(transaction_effect(sql), *expected, "misclassified {sql:?}");
        }
    }

    #[test]
    fn classifies_a_statement_behind_the_application_name_comment() {
        let commit = prefixed_statement("commit;");
        assert_eq!(transaction_effect(&commit), TransactionEffect::Ends);
        let begin = prefixed_statement("  BEGIN  ");
        assert_eq!(transaction_effect(&begin), TransactionEffect::Opens);
        let savepoint = prefixed_statement("ROLLBACK TO SAVEPOINT a");
        assert_eq!(transaction_effect(&savepoint), TransactionEffect::Neither);
    }

    #[test]
    fn an_unterminated_comment_hides_the_whole_statement() {
        assert_eq!(
            transaction_effect("/* never closed COMMIT"),
            TransactionEffect::Neither
        );
    }

    #[test]
    fn the_idle_guard_is_written_in_milliseconds_at_session_scope() {
        assert_eq!(
            idle_in_transaction_timeout_statement(Duration::from_secs(90)),
            "SET SESSION idle_in_transaction_session_timeout = 90000"
        );
    }

    #[test]
    fn a_patience_that_would_round_to_zero_still_guards() {
        // Zero means "never give up" to the server, which is the opposite of
        // what asking for a very short patience says.
        assert_eq!(
            idle_in_transaction_timeout_statement(Duration::ZERO),
            "SET SESSION idle_in_transaction_session_timeout = 1"
        );
        assert_eq!(
            idle_in_transaction_timeout_statement(Duration::from_nanos(1)),
            "SET SESSION idle_in_transaction_session_timeout = 1"
        );
    }

    #[test]
    fn a_patience_beyond_the_parameter_is_capped_to_what_it_holds() {
        assert_eq!(
            idle_in_transaction_timeout_statement(Duration::from_secs(60 * 60 * 24 * 365)),
            format!(
                "SET SESSION idle_in_transaction_session_timeout = {}",
                i32::MAX
            )
        );
    }

    #[test]
    fn read_queries_are_told_apart_from_statements() {
        for sql in [
            "SELECT 1",
            "  select 1",
            "with x as (select 1) select * from x",
            "EXPLAIN SELECT 1",
            "TABLE users",
            "SHOW search_path",
        ] {
            assert!(is_read_query(sql), "{sql:?} should be read as a query");
        }
        for sql in [
            "INSERT INTO t VALUES (1)",
            "COMMIT",
            "BEGIN",
            "CREATE TABLE t (a int)",
            "",
        ] {
            assert!(!is_read_query(sql), "{sql:?} should not be read as a query");
        }
    }

    #[test]
    fn a_schema_holding_a_quote_cannot_end_the_quoting_early() {
        assert_eq!(
            search_path_statement("pu\"blic"),
            "SET search_path = \"pu\"\"blic\""
        );
    }
}

/// Integration tests against a real Postgres server.
///
#[cfg(test)]
mod connect_options_tests {
    use super::postgres_connect_options;
    use crate::connection::{ConnectionConfig, DatabaseDriver, SslMode};
    use sqlx::postgres::PgSslMode;

    fn base_config() -> ConnectionConfig {
        ConnectionConfig {
            driver: DatabaseDriver::PostgreSQL,
            host: "db.example.com".to_string(),
            port: 5432,
            username: "postgres".to_string(),
            password: "secret".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn ssl_mode_disabled_by_default() {
        let opts = postgres_connect_options(&base_config());
        assert!(matches!(opts.get_ssl_mode(), PgSslMode::Disable));
    }

    #[test]
    fn ssl_mode_require_maps_to_require() {
        let mut config = base_config();
        config.ssl_mode = SslMode::Require;
        let opts = postgres_connect_options(&config);
        assert!(matches!(opts.get_ssl_mode(), PgSslMode::Require));
    }

    #[test]
    fn ssl_mode_verify_ca_maps_to_verify_ca() {
        let mut config = base_config();
        config.ssl_mode = SslMode::VerifyCa;
        config.ssl_ca_path = Some("/tmp/ca.pem".to_string());
        let opts = postgres_connect_options(&config);
        assert!(matches!(opts.get_ssl_mode(), PgSslMode::VerifyCa));
    }

    #[test]
    fn ssl_mode_verify_full_maps_to_verify_full() {
        let mut config = base_config();
        config.ssl_mode = SslMode::VerifyFull;
        let opts = postgres_connect_options(&config);
        assert!(matches!(opts.get_ssl_mode(), PgSslMode::VerifyFull));
    }
}

#[cfg(test)]
mod ddl_rendering_tests {
    use super::render_table_ddl;

    fn column(
        name: &str,
        kind: &str,
        not_null: bool,
        default: Option<&str>,
        comment: Option<&str>,
    ) -> (String, String, bool, Option<String>, Option<String>) {
        (
            name.to_string(),
            kind.to_string(),
            not_null,
            default.map(str::to_string),
            comment.map(str::to_string),
        )
    }

    #[test]
    fn a_column_keeps_the_precision_its_type_was_declared_with() {
        let ddl = render_table_ddl(
            "public",
            "prices",
            &[column("amount", "numeric(32,9)", false, None, None)],
            &[],
            &[],
            None,
        );
        assert!(
            ddl.contains("\"amount\" numeric(32,9)"),
            "a type without its precision is a different type: {ddl}"
        );
    }

    #[test]
    fn constraints_and_indexes_both_appear_and_each_in_its_place() {
        let ddl = render_table_ddl(
            "public",
            "prices",
            &[column("id", "integer", true, None, None)],
            &[("pk_prices".to_string(), "PRIMARY KEY (id)".to_string())],
            &[("CREATE INDEX idx_prices_id ON public.prices USING btree (id)".to_string(),)],
            None,
        );
        let statement_ends = ddl.find(");").expect("the table statement is closed");
        let constraint = ddl.find("CONSTRAINT").expect("the constraint is stated");
        let index = ddl.find("CREATE INDEX").expect("the index is stated");
        assert!(
            constraint < statement_ends,
            "a constraint belongs inside the table statement: {ddl}"
        );
        assert!(
            index > statement_ends,
            "an index is its own statement, after the table: {ddl}"
        );
    }

    #[test]
    fn comments_are_written_as_their_own_statements_and_quoted() {
        let ddl = render_table_ddl(
            "public",
            "prices",
            &[column("id", "integer", true, None, Some("what it's for"))],
            &[],
            &[],
            Some("a table"),
        );
        assert!(ddl.contains("COMMENT ON TABLE \"public\".\"prices\" IS 'a table';"));
        assert!(
            ddl.contains("IS 'what it''s for';"),
            "an apostrophe has to be doubled or the statement ends early: {ddl}"
        );
    }

    #[test]
    fn a_table_with_nothing_extra_still_reads_as_one_statement() {
        let ddl = render_table_ddl(
            "public",
            "plain",
            &[column("id", "integer", false, None, None)],
            &[],
            &[],
            None,
        );
        assert_eq!(
            ddl,
            "CREATE TABLE \"public\".\"plain\" (\n  \"id\" integer\n);\n"
        );
    }

    #[test]
    fn a_name_holding_a_quote_cannot_end_its_own_quoting() {
        let ddl = render_table_ddl(
            "public",
            "od\"d",
            &[column("id", "integer", false, None, None)],
            &[],
            &[],
            None,
        );
        assert!(
            ddl.contains("\"od\"\"d\""),
            "the quote in the name has to be doubled: {ddl}"
        );
    }
}

/// Set POSTGRES_TEST_URL=postgres://user:password@host:port/dbname before
/// running, then use `cargo test -p db_client -- --include-ignored` to
/// execute. Mirrors the MySQL provider's integration_tests convention.
#[cfg(test)]
mod integration_tests {
    use super::PostgresProvider;
    use crate::provider::DbProvider;
    use crate::schema::ProcedureKind;
    use crate::{ConnectionConfig, DatabaseDriver};
    use uuid::Uuid;

    fn test_config_from_env() -> Option<ConnectionConfig> {
        let url = std::env::var("POSTGRES_TEST_URL").ok()?;
        let url = url.strip_prefix("postgres://")?;
        let (userinfo, hostpart) = url.split_once('@')?;
        let (username, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let (hostport, database) = hostpart.split_once('/').unwrap_or((hostpart, ""));
        let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, "5432"));
        let port: u16 = port_str.parse().unwrap_or(5432);

        Some(ConnectionConfig {
            id: Uuid::new_v4(),
            label: "test".to_string(),
            driver: DatabaseDriver::PostgreSQL,
            host: host.to_string(),
            port,
            username: username.to_string(),
            password: password.to_string(),
            database: if database.is_empty() {
                None
            } else {
                Some(database.to_string())
            },
            auto_connect: false,
            ..ConnectionConfig::default()
        })
    }

    // Gates the "NULL decodes as 0" hypothesis from the grid UX audit for
    // Postgres specifically -- SQLite's manifest typing made the hypothesis
    // true there, but Postgres's strongly-typed driver may already behave
    // correctly; this test must not be skipped in favor of assuming so.
    #[tokio::test]
    #[ignore]
    async fn test_null_cells_decode_as_none() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let result = provider
            .execute_query(
                "public",
                "SELECT NULL::text AS text_col, NULL::bigint AS int_col",
            )
            .await
            .expect("Failed to execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0][0], None,
            "a NULL text column must decode to None, not Some(\"0\")/Some(\"\")"
        );
        assert_eq!(
            result.rows[0][1], None,
            "a NULL integer column must decode to None, not Some(\"0\")"
        );
    }

    /// What the grid shows has to be what `psql` shows. Every expected string
    /// here was taken from that client, run against the same values.
    #[tokio::test]
    #[ignore]
    async fn test_cells_read_the_way_the_psql_client_prints_them() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        let result = provider
            .execute_query(
                "public",
                "SELECT '\\xd41d8cd98f00b204e9800998ecf8427e'::bytea AS a_bytea, \
                        '0b4bd6cb-2b8f-4b0a-9d4c-6d5f1a2b3c4d'::uuid AS an_id, \
                        true AS yes, false AS no, \
                        12345678901234.567890::numeric(20,6) AS exact, \
                        1e30::double precision AS large, \
                        '2026-08-02'::date AS a_day, \
                        '16:28:50'::time AS an_hour, \
                        '2026-08-02 16:28:50'::timestamp AS a_moment, \
                        '2026-08-02 16:28:50+00'::timestamptz AS a_moment_somewhere, \
                        '1 day 2 hours'::interval AS a_span, \
                        '{\"a\": 1}'::jsonb AS a_document, \
                        ARRAY[1,2,3] AS numbers, \
                        ARRAY['x','has space'] AS words, \
                        '192.168.0.1'::inet AS an_address, \
                        1234.56::money AS an_amount, \
                        NULL::text AS nothing",
            )
            .await
            .expect("Failed to execute query");

        let rendered: Vec<String> = result.rows[0]
            .iter()
            .map(|cell| cell.clone().unwrap_or_else(|| "<absent>".to_string()))
            .collect();

        assert_eq!(
            rendered,
            vec![
                "\\xd41d8cd98f00b204e9800998ecf8427e".to_string(),
                "0b4bd6cb-2b8f-4b0a-9d4c-6d5f1a2b3c4d".to_string(),
                "t".to_string(),
                "f".to_string(),
                "12345678901234.567890".to_string(),
                "1e+30".to_string(),
                "2026-08-02".to_string(),
                "16:28:50".to_string(),
                "2026-08-02 16:28:50".to_string(),
                "2026-08-02 16:28:50+00".to_string(),
                "1 day 02:00:00".to_string(),
                "{\"a\": 1}".to_string(),
                "{1,2,3}".to_string(),
                "{x,\"has space\"}".to_string(),
                "192.168.0.1".to_string(),
                "$1,234.56".to_string(),
                // Only a real absence reads as absent: everything above has to
                // arrive as text, not fall through to nothing.
                "<absent>".to_string(),
            ],
            "every value reads the way the psql client prints it"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_ping() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");
        provider.ping().await.expect("Ping failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_databases_finds_public_schema() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let schemas = provider
            .list_databases()
            .await
            .expect("Failed to list schemas");
        assert!(schemas.iter().any(|s| s.name == "public"));
    }

    /// Runs `body` against a fresh scratch schema (Postgres's rough
    /// equivalent of a MySQL scratch database within one physical database),
    /// dropping it afterward with CASCADE regardless of the outcome.
    async fn with_scratch_schema<'p, F, Fut, T>(provider: &'p PostgresProvider, body: F) -> T
    where
        F: FnOnce(&'p PostgresProvider, String) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let schema = format!("zdbt_{}", Uuid::new_v4().simple());
        provider
            .execute_query("public", &format!("CREATE SCHEMA \"{schema}\""))
            .await
            .expect("Failed to create scratch schema");

        let result = body(provider, schema.clone()).await;

        provider
            .execute_query("public", &format!("DROP SCHEMA \"{schema}\" CASCADE"))
            .await
            .expect("Failed to clean up scratch schema");

        result
    }

    /// What Ctrl-clicking a table has to answer with: the table found however it
    /// was spelled, and described in full.
    ///
    /// Postgres folds an unquoted identifier to lower case, so a name copied out
    /// of a query that runs perfectly well can be spelled in a way the catalogue
    /// has never heard of. Before, that produced an empty `CREATE TABLE ( )` --
    /// which reads as a table with no columns rather than as a table not found.
    #[tokio::test]
    #[ignore]
    async fn test_table_ddl_is_found_whatever_the_spelling_and_says_everything() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, async |provider, schema| {
            provider
                .execute_query(
                    "public",
                    &format!(
                        "CREATE TABLE \"{schema}\".estimates (
                             consensus_id integer NOT NULL,
                             amount numeric(32,9),
                             note text,
                             CONSTRAINT pk_estimates PRIMARY KEY (consensus_id)
                         )"
                    ),
                )
                .await
                .expect("the table was created");
            provider
                .execute_query(
                    "public",
                    &format!(
                        "CREATE INDEX idx_estimates_amount ON \"{schema}\".estimates (amount)"
                    ),
                )
                .await
                .expect("the index was created");
            provider
                .execute_query(
                    "public",
                    &format!("COMMENT ON TABLE \"{schema}\".estimates IS 'what is expected'"),
                )
                .await
                .expect("the comment was set");

            // However it is spelled, it is the same table.
            for spelling in ["estimates", "Estimates", "ESTIMATES", "eStImAtEs"] {
                let found = provider.get_table_ddl(&schema, spelling).await;
                assert!(
                    found.is_ok(),
                    "{spelling} has to find the table: {:?}",
                    found.err()
                );
            }
            // And a table the catalogue holds in mixed case is found by a name
            // written in none of it, which is how anyone would type it back.
            provider
                .execute_query(
                    "public",
                    &format!("CREATE TABLE \"{schema}\".\"MixedCase\" (id integer)"),
                )
                .await
                .expect("the mixed case table was created");
            let mixed = provider
                .get_table_ddl(&schema, "mixedcase")
                .await
                .expect("a mixed case table is found by a lower case name");
            assert!(
                mixed.contains("\"MixedCase\""),
                "the name it is stored under is the name to print: {mixed}"
            );

            let ddl = provider
                .get_table_ddl(&schema, "Estimates")
                .await
                .expect("a table spelled in another case is still that table");

            assert!(
                ddl.contains("\"amount\" numeric(32,9)"),
                "the type has to keep the precision it was declared with: {ddl}"
            );
            assert!(
                ddl.contains("PRIMARY KEY"),
                "the primary key is part of what the table is: {ddl}"
            );
            assert!(
                ddl.contains("CREATE INDEX idx_estimates_amount"),
                "an index of its own has to be shown: {ddl}"
            );
            assert!(
                ddl.contains("IS 'what is expected'"),
                "a comment is worth more than most of the rest: {ddl}"
            );
            assert!(
                !ddl.contains("pk_estimates ON"),
                "the index behind the primary key is already stated by the key: {ddl}"
            );

            let missing = provider.get_table_ddl(&schema, "no_such_table").await;
            assert!(
                missing.is_err(),
                "a table that is not there has to say so, not come back as an empty shell"
            );
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_create_alter_and_drop_table() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(
                    &schema,
                    &format!(
                        "CREATE TABLE \"{schema}\".widgets (id INT PRIMARY KEY, name TEXT NOT NULL)"
                    ),
                )
                .await
                .expect("Failed to create table");

            let columns_before = provider
                .describe_table(&schema, "widgets")
                .await
                .expect("Failed to describe table");
            assert_eq!(columns_before.len(), 2);

            provider
                .execute_query(
                    &schema,
                    &format!("ALTER TABLE \"{schema}\".widgets ADD COLUMN weight INT"),
                )
                .await
                .expect("Failed to alter table");
            let columns_after = provider
                .describe_table(&schema, "widgets")
                .await
                .expect("Failed to describe table after ALTER");
            assert_eq!(columns_after.len(), 3);
            assert!(columns_after.iter().any(|c| c.name == "weight"));

            provider
                .drop_table(&schema, "widgets")
                .await
                .expect("Failed to drop table");
            let tables = provider
                .list_tables(&schema)
                .await
                .expect("Failed to list tables");
            assert!(!tables.iter().any(|t| t.name == "widgets"));
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_create_and_drop_index() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(
                    &schema,
                    &format!(
                        "CREATE TABLE \"{schema}\".indexed_widgets (id INT PRIMARY KEY, sku TEXT NOT NULL)"
                    ),
                )
                .await
                .expect("Failed to create table");
            provider
                .execute_query(
                    &schema,
                    &format!(
                        "CREATE UNIQUE INDEX sku_idx ON \"{schema}\".indexed_widgets (sku)"
                    ),
                )
                .await
                .expect("Failed to create index");

            let indexes = provider
                .list_indexes(&schema, "indexed_widgets")
                .await
                .expect("Failed to list indexes");
            let sku_index = indexes
                .iter()
                .find(|i| i.name == "sku_idx")
                .expect("sku_idx should be listed");
            assert!(sku_index.unique, "sku_idx was created as UNIQUE");

            provider
                .execute_query(&schema, &format!("DROP INDEX \"{schema}\".sku_idx"))
                .await
                .expect("Failed to drop index");
            let indexes_after = provider
                .list_indexes(&schema, "indexed_widgets")
                .await
                .expect("Failed to list indexes after drop");
            assert!(!indexes_after.iter().any(|i| i.name == "sku_idx"));
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_create_query_and_drop_a_view() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(
                    &schema,
                    &format!("CREATE TABLE \"{schema}\".items (id INT PRIMARY KEY, price INT NOT NULL)"),
                )
                .await
                .expect("Failed to create table");
            provider
                .execute_query(&schema, &format!("INSERT INTO \"{schema}\".items VALUES (1, 150)"))
                .await
                .expect("Failed to insert row");
            provider
                .execute_query(
                    &schema,
                    &format!(
                        "CREATE VIEW \"{schema}\".pricey_items AS SELECT * FROM \"{schema}\".items WHERE price > 100"
                    ),
                )
                .await
                .expect("Failed to create view");

            let result = provider
                .execute_query(&schema, &format!("SELECT id, price FROM \"{schema}\".pricey_items"))
                .await
                .expect("Failed to query the view");
            assert_eq!(result.rows.len(), 1);
            assert_eq!(result.rows[0][0].as_deref(), Some("1"));

            provider
                .execute_query(&schema, &format!("DROP VIEW \"{schema}\".pricey_items"))
                .await
                .expect("Failed to drop view");
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_insert_update_and_delete_row_lifecycle() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(
                    &schema,
                    &format!(
                        "CREATE TABLE \"{schema}\".accounts (id INT PRIMARY KEY, balance INT NOT NULL)"
                    ),
                )
                .await
                .expect("Failed to create table");

            provider
                .execute_query(&schema, &format!("INSERT INTO \"{schema}\".accounts VALUES (1, 100)"))
                .await
                .expect("Failed to insert row");
            let after_insert = provider
                .execute_query(&schema, &format!("SELECT balance FROM \"{schema}\".accounts WHERE id = 1"))
                .await
                .expect("Failed to select after insert");
            assert_eq!(after_insert.rows[0][0].as_deref(), Some("100"));

            provider
                .execute_query(
                    &schema,
                    &format!("UPDATE \"{schema}\".accounts SET balance = 250 WHERE id = 1"),
                )
                .await
                .expect("Failed to update row");
            let after_update = provider
                .execute_query(&schema, &format!("SELECT balance FROM \"{schema}\".accounts WHERE id = 1"))
                .await
                .expect("Failed to select after update");
            assert_eq!(after_update.rows[0][0].as_deref(), Some("250"));

            provider
                .execute_query(&schema, &format!("DELETE FROM \"{schema}\".accounts WHERE id = 1"))
                .await
                .expect("Failed to delete row");
            let after_delete = provider
                .execute_query(&schema, &format!("SELECT balance FROM \"{schema}\".accounts WHERE id = 1"))
                .await
                .expect("Failed to select after delete");
            assert!(after_delete.rows.is_empty(), "row should be gone after DELETE");
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_upsert_via_on_conflict_do_update() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(
                    &schema,
                    &format!(
                        "CREATE TABLE \"{schema}\".counters (name TEXT PRIMARY KEY, hits INT NOT NULL)"
                    ),
                )
                .await
                .expect("Failed to create table");

            let upsert_sql = format!(
                "INSERT INTO \"{schema}\".counters (name, hits) VALUES ('clicks', 1) \
                 ON CONFLICT (name) DO UPDATE SET hits = \"{schema}\".counters.hits + 1"
            );
            provider
                .execute_query(&schema, &upsert_sql)
                .await
                .expect("Failed first upsert (insert path)");
            provider
                .execute_query(&schema, &upsert_sql)
                .await
                .expect("Failed second upsert (update path)");

            let result = provider
                .execute_query(&schema, &format!("SELECT hits FROM \"{schema}\".counters WHERE name = 'clicks'"))
                .await
                .expect("Failed to select counter");
            assert_eq!(result.rows.len(), 1, "upsert must not create a duplicate row");
            assert_eq!(
                result.rows[0][0].as_deref(),
                Some("2"),
                "the second upsert must have taken the UPDATE branch, not re-inserted at 1"
            );
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_foreign_keys_finds_a_declared_fk() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(&schema, &format!("CREATE TABLE \"{schema}\".authors (id INT PRIMARY KEY)"))
                .await
                .expect("Failed to create authors table");
            provider
                .execute_query(
                    &schema,
                    &format!(
                        "CREATE TABLE \"{schema}\".posts (\
                             id INT PRIMARY KEY, \
                             author_id INT NOT NULL, \
                             CONSTRAINT fk_posts_author FOREIGN KEY (author_id) REFERENCES \"{schema}\".authors (id)\
                         )"
                    ),
                )
                .await
                .expect("Failed to create posts table");

            let fks = provider
                .list_foreign_keys(&schema, "posts")
                .await
                .expect("Failed to list foreign keys");
            assert_eq!(fks.len(), 1);
            assert_eq!(fks[0].name, "fk_posts_author");
            assert_eq!(fks[0].from_column, "author_id");
            assert_eq!(fks[0].to_table, "authors");
            assert_eq!(fks[0].to_column, "id");
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_check_constraints_finds_a_declared_check() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(
                    &schema,
                    &format!(
                        "CREATE TABLE \"{schema}\".products (\
                             id INT PRIMARY KEY, \
                             price INT NOT NULL, \
                             CONSTRAINT chk_price_positive CHECK (price > 0)\
                         )"
                    ),
                )
                .await
                .expect("Failed to create products table");

            // Postgres's `information_schema.check_constraints` also reports
            // an implicit not-null-derived entry per NOT NULL column (here:
            // `id` via the primary key, and `price`), alongside the real
            // named constraint -- so this asserts the named one is present
            // rather than asserting the total count is exactly one.
            let checks = provider
                .list_check_constraints(&schema, "products")
                .await
                .expect("Failed to list check constraints");
            let chk_price_positive = checks
                .iter()
                .find(|check| check.name == "chk_price_positive")
                .unwrap_or_else(|| panic!("expected chk_price_positive among {checks:?}"));
            assert!(
                chk_price_positive.expression.contains("price"),
                "expected the check expression to reference `price`, got {}",
                chk_price_positive.expression
            );
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_procedures_finds_a_created_procedure_and_function() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(
                    &schema,
                    &format!(
                        "CREATE PROCEDURE \"{schema}\".greet(who TEXT) \
                         LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'Hello, %', who; END; $$"
                    ),
                )
                .await
                .expect("Failed to create procedure");
            provider
                .execute_query(
                    &schema,
                    &format!(
                        "CREATE FUNCTION \"{schema}\".double_it(n INT) RETURNS INT \
                         LANGUAGE plpgsql AS $$ BEGIN RETURN n * 2; END; $$"
                    ),
                )
                .await
                .expect("Failed to create function");

            let procedures = provider
                .list_procedures(&schema)
                .await
                .expect("Failed to list procedures");
            let names: Vec<&str> = procedures.iter().map(|p| p.name.as_str()).collect();
            assert!(names.contains(&"greet"), "expected `greet` among {names:?}");
            assert!(
                names.contains(&"double_it"),
                "expected `double_it` among {names:?}"
            );
            let greet = procedures.iter().find(|p| p.name == "greet").unwrap();
            assert_eq!(greet.kind, ProcedureKind::Procedure);
            let double_it = procedures.iter().find(|p| p.name == "double_it").unwrap();
            assert_eq!(double_it.kind, ProcedureKind::Function);
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_triggers_finds_a_created_trigger() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(
                    &schema,
                    &format!("CREATE TABLE \"{schema}\".widgets (id INT PRIMARY KEY, name TEXT)"),
                )
                .await
                .expect("Failed to create widgets table");
            provider
                .execute_query(
                    &schema,
                    &format!(
                        "CREATE FUNCTION \"{schema}\".widgets_trigger_fn() RETURNS TRIGGER \
                         LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$"
                    ),
                )
                .await
                .expect("Failed to create trigger function");
            provider
                .execute_query(
                    &schema,
                    &format!(
                        "CREATE TRIGGER widgets_after_insert AFTER INSERT ON \"{schema}\".widgets \
                         FOR EACH ROW EXECUTE FUNCTION \"{schema}\".widgets_trigger_fn()"
                    ),
                )
                .await
                .expect("Failed to create trigger");

            let triggers = provider
                .list_triggers(&schema, "widgets")
                .await
                .expect("Failed to list triggers");
            assert_eq!(triggers.len(), 1);
            assert_eq!(triggers[0].name, "widgets_after_insert");
            assert_eq!(triggers[0].event, "INSERT");
            assert_eq!(triggers[0].timing, "AFTER");
            assert_eq!(triggers[0].table_name, "widgets");
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_sequences_finds_a_created_sequence() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(&schema, &format!("CREATE SEQUENCE \"{schema}\".order_ids"))
                .await
                .expect("Failed to create sequence");
            provider
                .execute_query(
                    &schema,
                    &format!("SELECT nextval('\"{schema}\".order_ids')"),
                )
                .await
                .expect("Failed to advance sequence");

            let sequences = provider
                .list_sequences(&schema)
                .await
                .expect("Failed to list sequences");
            assert_eq!(sequences.len(), 1);
            assert_eq!(sequences[0].name, "order_ids");
            assert_eq!(sequences[0].current_value, Some(1));
            assert_eq!(sequences[0].increment, Some(1));
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_users_finds_the_connected_root_user() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        let users = provider.list_users().await.expect("Failed to list users");
        assert!(
            users.iter().any(|user| user.name == "root"),
            "expected the connected root user among {:?}",
            users.iter().map(|u| &u.name).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_truncate_table_removes_rows_but_keeps_the_table() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(
                    &schema,
                    &format!("CREATE TABLE \"{schema}\".crumbs (id INT PRIMARY KEY)"),
                )
                .await
                .expect("Failed to create table");
            provider
                .execute_query(
                    &schema,
                    &format!("INSERT INTO \"{schema}\".crumbs (id) VALUES (1), (2), (3)"),
                )
                .await
                .expect("Failed to insert rows");

            provider
                .truncate_table(&schema, "crumbs")
                .await
                .expect("Failed to truncate table");

            let tables = provider
                .list_tables(&schema)
                .await
                .expect("Failed to list tables");
            assert!(
                tables.iter().any(|t| t.name == "crumbs"),
                "truncate must not drop the table itself"
            );
            let remaining = provider
                .execute_query(&schema, &format!("SELECT id FROM \"{schema}\".crumbs"))
                .await
                .expect("Failed to select from truncated table");
            assert!(
                remaining.rows.is_empty(),
                "truncate must remove every row, got {:?}",
                remaining.rows
            );
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_table_changes_the_visible_name() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(
                    &schema,
                    &format!("CREATE TABLE \"{schema}\".old_name (id INT PRIMARY KEY)"),
                )
                .await
                .expect("Failed to create table");

            provider
                .rename_table(&schema, "old_name", "new_name")
                .await
                .expect("Failed to rename table");

            let tables = provider
                .list_tables(&schema)
                .await
                .expect("Failed to list tables");
            let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
            assert!(
                !names.contains(&"old_name"),
                "the old name must no longer be listed, got {names:?}"
            );
            assert!(
                names.contains(&"new_name"),
                "the new name must be listed, got {names:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_drop_table_via_provider_method_removes_it() {
        let config = test_config_from_env()
            .expect("POSTGRES_TEST_URL env var required for integration tests");
        let provider = PostgresProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_schema(&provider, |provider, schema| async move {
            provider
                .execute_query(
                    &schema,
                    &format!("CREATE TABLE \"{schema}\".throwaway (id INT PRIMARY KEY)"),
                )
                .await
                .expect("Failed to create table");

            provider
                .drop_table(&schema, "throwaway")
                .await
                .expect("Failed to drop table via provider method");

            let tables = provider
                .list_tables(&schema)
                .await
                .expect("Failed to list tables");
            assert!(
                !tables.iter().any(|t| t.name == "throwaway"),
                "drop_table must remove the table"
            );
        })
        .await;
    }
}
