use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::TryStreamExt as _;
use smol::lock::Mutex as AsyncMutex;
use sqlx::AssertSqlSafe;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqlitePool, SqlitePoolOptions};
use sqlx::{Column as _, ConnectOptions as _, Row as _, ValueRef as _};
use std::path::Path;
use std::str::FromStr as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::MAX_RESULT_ROWS;
use crate::connection::ConnectionConfig;
use crate::provider::DbProvider;
use crate::schema::{
    CheckConstraintInfo, ColumnInfo, DatabaseInfo, IndexInfo, QueryResult, TableInfo, TableKind,
    TriggerInfo,
};

pub struct SqliteProvider {
    pool: SqlitePool,
    db_name: String,
    /// The options the pool was built from, kept so that the transaction's own
    /// connection below can be opened against the same file with the same
    /// pragmas rather than a guess at them.
    connect_options: SqliteConnectOptions,
    /// Whether a transaction can be held at all, which is false only for an
    /// in-memory database. An in-memory database belongs to the single
    /// connection that opened it, so a second connection opens a different,
    /// empty database: a transaction held there would not be the data the
    /// console is reading. Only tests reach that case, but answering honestly
    /// costs a bool.
    can_hold_transactions: bool,
    /// The connection an open transaction lives on, or `None` when none is
    /// open.
    ///
    /// It is deliberately not a pooled connection. This provider sets only
    /// `max_connections`, leaving sqlx's `idle_timeout` (10 minutes) and
    /// `max_lifetime` (30 minutes) at their defaults, so a pooled connection
    /// sitting idle while the reader thinks about the next statement would be
    /// closed underneath the transaction, and one that merely lived long
    /// enough would be retired at the next release. SQLite rolls back an
    /// unfinished transaction when its connection closes, so either would
    /// discard the reader's staged work with nothing said.
    held_connection: AsyncMutex<Option<SqliteConnection>>,
    /// When the held transaction opened. Kept beside the connection instead of
    /// inside it because `transaction_open_since` is synchronous: it must
    /// neither block on the async lock that guards the connection nor report
    /// "nothing open" because that lock happened to be busy.
    held_since: Mutex<Option<Instant>>,
}

impl SqliteProvider {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let path = &config.host;
        let url = format!("sqlite://{path}");
        let connect_options =
            SqliteConnectOptions::from_str(&url).context("Failed to read SQLite database path")?;
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(connect_options.clone())
            .await
            .context("Failed to open SQLite database")?;
        let db_name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path)
            .to_string();
        Ok(Self::new(pool, connect_options, db_name))
    }

    fn new(pool: SqlitePool, connect_options: SqliteConnectOptions, db_name: String) -> Self {
        let can_hold_transactions = connect_options.get_filename() != Path::new(":memory:");
        Self {
            pool,
            db_name,
            connect_options,
            can_hold_transactions,
            held_connection: AsyncMutex::new(None),
            held_since: Mutex::new(None),
        }
    }

    fn remember_open_since(&self, at: Option<Instant>) {
        *self
            .held_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = at;
    }

    async fn open_the_transaction_connection(&self) -> Result<SqliteConnection> {
        self.connect_options
            .connect()
            .await
            .context("Failed to open a connection for the transaction")
    }
}

#[async_trait]
impl DbProvider for SqliteProvider {
    async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .context("Ping failed")?;
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
        Ok(vec![DatabaseInfo {
            name: self.db_name.clone(),
        }])
    }

    async fn list_tables(&self, _database: &str) -> Result<Vec<TableInfo>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "-- name: ListTables :many
             SELECT name, type FROM sqlite_master WHERE type IN ('table', 'view') ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list tables")?;

        Ok(rows
            .into_iter()
            .map(|(name, kind)| TableInfo {
                name,
                kind: if kind == "view" {
                    TableKind::View
                } else {
                    TableKind::Table
                },
            })
            .collect())
    }

    async fn describe_table(&self, _database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let safe_table = table.replace('\'', "''");
        let sql = format!("PRAGMA table_info('{safe_table}')");
        let rows = sqlx::query(AssertSqlSafe(sql.as_str()))
            .fetch_all(&self.pool)
            .await
            .context("Failed to describe table")?;

        let mut columns = Vec::new();
        for row in rows {
            let name: String = row.try_get("name").context("Missing column 'name'")?;
            let data_type: String = row.try_get("type").unwrap_or_default();
            let notnull: i32 = row.try_get("notnull").unwrap_or(0);
            let default_value: Option<String> = row.try_get("dflt_value").unwrap_or(None);
            let pk: i32 = row.try_get("pk").unwrap_or(0);

            columns.push(ColumnInfo {
                name,
                data_type,
                is_nullable: notnull == 0,
                column_key: if pk > 0 {
                    Some("PRI".to_string())
                } else {
                    None
                },
                default_value,
                extra: String::new(),
            });
        }
        Ok(columns)
    }

    async fn get_database_ddl(&self, _database: &str) -> Result<String> {
        Ok("-- SQLite databases are files; there is no CREATE DATABASE statement.\n".to_string())
    }

    async fn get_table_ddl(&self, _database: &str, table: &str) -> Result<String> {
        let row = sqlx::query_as::<_, (Option<String>,)>(
            "-- name: GetTableDDL :one
             SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        )
        .bind(table)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to query sqlite_master for DDL")?;

        match row {
            Some((Some(ddl),)) => Ok(ddl),
            _ => anyhow::bail!("Table '{}' not found in sqlite_master", table),
        }
    }

    async fn list_indexes(&self, _database: &str, table: &str) -> Result<Vec<IndexInfo>> {
        let safe_table = table.replace('\'', "''");
        let list_sql = format!("PRAGMA index_list('{safe_table}')");
        let index_rows = sqlx::query(AssertSqlSafe(list_sql.as_str()))
            .fetch_all(&self.pool)
            .await
            .context("Failed to list indexes")?;

        let mut indexes = Vec::new();
        for row in index_rows {
            let name: String = row.try_get("name").context("Missing index name")?;
            let unique_val: i64 = row.try_get("unique").unwrap_or(0);
            let safe_name = name.replace('\'', "''");
            let info_sql = format!("PRAGMA index_info('{safe_name}')");
            let col_rows = sqlx::query(AssertSqlSafe(info_sql.as_str()))
                .fetch_all(&self.pool)
                .await
                .context("Failed to get index info")?;
            let columns: Vec<String> = col_rows
                .iter()
                .filter_map(|r| r.try_get::<String, _>("name").ok())
                .collect();
            indexes.push(IndexInfo {
                name,
                columns,
                unique: unique_val != 0,
                index_type: "BTREE".to_string(),
            });
        }
        Ok(indexes)
    }

    // SQLite exposes no system view for CHECK constraints, so this scans the
    // table's own stored CREATE TABLE text for `CHECK (...)` clauses. It is a
    // best-effort text scan, not a SQL parser: it can be fooled by a `CHECK`
    // substring appearing inside a string literal or comment. Good enough for
    // a first version; a real parser would be needed for full correctness.
    async fn list_check_constraints(
        &self,
        _database: &str,
        table: &str,
    ) -> Result<Vec<CheckConstraintInfo>> {
        let row = sqlx::query_as::<_, (Option<String>,)>(
            "-- name: GetTableDDLForCheckConstraints :one
             SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        )
        .bind(table)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to query sqlite_master for check constraints")?;

        match row {
            Some((Some(ddl),)) => Ok(extract_check_constraints(&ddl)),
            _ => Ok(Vec::new()),
        }
    }

    async fn list_triggers(&self, _database: &str, table: &str) -> Result<Vec<TriggerInfo>> {
        let rows = sqlx::query_as::<_, (String, Option<String>)>(
            "-- name: ListTriggers :many
             SELECT name, sql FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1",
        )
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list triggers")?;

        Ok(rows
            .into_iter()
            .map(|(name, sql_opt)| {
                let sql_upper = sql_opt.as_deref().unwrap_or("").to_uppercase();
                let timing = if sql_upper.contains("BEFORE") {
                    "BEFORE"
                } else if sql_upper.contains("INSTEAD OF") {
                    "INSTEAD OF"
                } else {
                    "AFTER"
                }
                .to_string();
                let event = if sql_upper.contains("INSERT") {
                    "INSERT"
                } else if sql_upper.contains("UPDATE") {
                    "UPDATE"
                } else {
                    "DELETE"
                }
                .to_string();
                TriggerInfo {
                    name,
                    event,
                    timing,
                    table_name: table.to_string(),
                    definition: sql_opt,
                }
            })
            .collect())
    }

    async fn truncate_table(&self, _database: &str, table: &str) -> Result<()> {
        let safe = table.replace('"', "\"\"");
        sqlx::query(AssertSqlSafe(format!("DELETE FROM \"{safe}\"")))
            .execute(&self.pool)
            .await
            .context("Failed to truncate table")?;
        Ok(())
    }

    async fn drop_table(&self, _database: &str, table: &str) -> Result<()> {
        let safe = table.replace('"', "\"\"");
        sqlx::query(AssertSqlSafe(format!("DROP TABLE \"{safe}\"")))
            .execute(&self.pool)
            .await
            .context("Failed to drop table")?;
        Ok(())
    }

    async fn rename_table(&self, _database: &str, old_name: &str, new_name: &str) -> Result<()> {
        sqlx::query(AssertSqlSafe(rename_table_sql(old_name, new_name)))
            .execute(&self.pool)
            .await
            .context("Failed to rename table")?;
        Ok(())
    }

    async fn execute_query(&self, _database: &str, sql: &str) -> Result<QueryResult> {
        // Only statements from the console are routed onto the transaction's
        // connection. Every schema and metadata call in this file keeps using
        // the pool on purpose: browsing the tree while a transaction is open
        // must not enrol those reads in the reader's transaction, where they
        // would see its uncommitted DDL and keep holding its locks for as long
        // as it stayed open. `execute_query_streaming` is not overridden here,
        // so its default routes through this method and inherits the same
        // split.
        let mut held = self.held_connection.lock().await;
        let effect = what_it_does_to_the_transaction(sql);

        if let Some(mut connection) = held.take() {
            let result = run_query(&mut connection, sql).await;
            // Only a statement that succeeded ended anything. A `COMMIT` that
            // failed -- SQLite answers SQLITE_BUSY when another connection
            // still holds a read lock -- leaves the transaction open, so the
            // connection has to go back, or the reader's work would be thrown
            // away right after they were told the commit did not happen. The
            // same goes for any other statement that failed inside the
            // transaction: SQLite leaves it open.
            if result.is_ok() && effect == TransactionEffect::Ends {
                self.remember_open_since(None);
            } else {
                *held = Some(connection);
            }
            return result;
        }

        if self.can_hold_transactions && effect == TransactionEffect::Begins {
            let mut connection = self.open_the_transaction_connection().await?;
            let result = run_query(&mut connection, sql).await?;
            *held = Some(connection);
            self.remember_open_since(Some(Instant::now()));
            return Ok(result);
        }

        let mut connection = self
            .pool
            .acquire()
            .await
            .context("Failed to take a connection for the query")?;
        run_query(&mut connection, sql).await
    }

    fn holds_transactions(&self) -> bool {
        self.can_hold_transactions
    }

    /// `abandoned_after` is accepted and has nothing to do on this driver, and
    /// that is a property of SQLite rather than an omission here.
    ///
    /// It exists so that a transaction cannot be left holding locks after the
    /// editor is killed and its own idle timer dies with it, which for a server
    /// database is a setting only the server can enforce. SQLite has no server
    /// and needs none for this: its locks are operating-system file locks held
    /// by the process, so killing the process drops them, and the next
    /// connection to open the file finds a hot journal (or an unclean WAL) and
    /// rolls the abandoned transaction back before it reads anything
    /// (https://www.sqlite.org/lockingv3.html#hot_journals). The failure this
    /// parameter guards against therefore cannot happen, and SQLite offers no
    /// equivalent knob to pass it to. While the editor *is* alive its own idle
    /// timer is the guard, and that timer works precisely because the process
    /// is still there.
    async fn begin_transaction(
        &self,
        _database: &str,
        _abandoned_after: Option<Duration>,
    ) -> Result<()> {
        anyhow::ensure!(
            self.can_hold_transactions,
            "an in-memory SQLite database cannot hold a transaction: a second connection to it \
             would be a different, empty database"
        );
        let mut held = self.held_connection.lock().await;
        anyhow::ensure!(
            held.is_none(),
            "a transaction is already open on this connection"
        );
        let mut connection = self.open_the_transaction_connection().await?;
        // Plain `BEGIN`, which SQLite defines as DEFERRED: it takes no lock
        // until the first statement inside it runs, so opening one and then
        // waiting on the reader blocks nobody. `BEGIN IMMEDIATE` would take
        // the write lock here and hold it for the whole time the reader spends
        // typing. https://www.sqlite.org/lang_transaction.html
        sqlx::query("BEGIN")
            .execute(&mut connection)
            .await
            .context("Failed to begin transaction")?;
        *held = Some(connection);
        self.remember_open_since(Some(Instant::now()));
        Ok(())
    }

    async fn commit_transaction(&self) -> Result<()> {
        let mut held = self.held_connection.lock().await;
        let Some(mut connection) = held.take() else {
            anyhow::bail!("no transaction is open on this connection");
        };
        self.remember_open_since(None);
        // The connection is dropped as this returns whether the COMMIT worked
        // or not. Keeping it pinned after reporting a failed commit would
        // leave the reader with a transaction the UI no longer shows a way to
        // end; dropping it closes the connection, and SQLite rolls an
        // unfinished transaction back when its connection is destroyed
        // (https://www.sqlite.org/c3ref/close.html).
        sqlx::query("COMMIT")
            .execute(&mut connection)
            .await
            .context("Failed to commit transaction")?;
        Ok(())
    }

    async fn rollback_transaction(&self) -> Result<()> {
        let mut held = self.held_connection.lock().await;
        let Some(mut connection) = held.take() else {
            return Ok(());
        };
        self.remember_open_since(None);
        let rolled_back = sqlx::query("ROLLBACK").execute(&mut connection).await;
        if let Err(error) = rolled_back {
            // A pinned connection can legitimately have no transaction left on
            // it: a hand-typed `RELEASE` of an outermost savepoint is defined
            // to be the same as `COMMIT`
            // (https://www.sqlite.org/lang_savepoint.html) and the statement
            // text alone does not say whether that savepoint was outermost, so
            // `what_it_does_to_the_transaction` cannot have released the
            // connection. SQLite's "no transaction is active" is then the state
            // this call wanted, not a failure to produce it.
            let message = error.to_string();
            if !message.contains("no transaction is active") {
                return Err(error).context("Failed to roll back transaction");
            }
        }
        Ok(())
    }

    fn transaction_open_since(&self) -> Option<Instant> {
        *self
            .held_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Runs one statement on whichever connection the caller chose -- a pooled one
/// for schema work, the transaction's own for the console -- so that the
/// routing decision lives in exactly one place and the decoding below cannot
/// drift between the two.
/// Takes the connection itself rather than something to run on, because the
/// read below needs two turns on it: the rows, and then -- only when there
/// were none -- what columns the statement would have returned. It has to be
/// the same connection: a table created inside the reader's transaction does
/// not exist for any other.
async fn run_query(connection: &mut SqliteConnection, sql: &str) -> Result<QueryResult> {
    let start = Instant::now();
    let trimmed_upper = sql.trim().to_uppercase();
    let is_read_query = trimmed_upper.starts_with("SELECT")
        || trimmed_upper.starts_with("EXPLAIN")
        || trimmed_upper.starts_with("PRAGMA")
        || trimmed_upper.starts_with("WITH");

    if is_read_query {
        let mut stream = sqlx::query(AssertSqlSafe(sql)).fetch(&mut *connection);
        let mut columns: Vec<String> = Vec::new();
        let mut result_rows: Vec<Vec<Option<String>>> = Vec::new();

        while let Some(row) = stream.try_next().await.context("Query execution failed")? {
            if columns.is_empty() {
                columns = row
                    .columns()
                    .iter()
                    .map(|col| col.name().to_string())
                    .collect();
            }
            let decoded: Vec<Option<String>> = (0..columns.len())
                .map(|index| {
                    // SQLite's manifest typing means a NULL column still
                    // satisfies a typed try_get (e.g. i64) as 0 instead of
                    // erroring, so nullness must be checked before any
                    // typed decode is attempted, not inferred from decode
                    // failure.
                    if row.try_get_raw(index).map(|v| v.is_null()).unwrap_or(false) {
                        return None;
                    }
                    row.try_get::<Option<String>, _>(index)
                        .ok()
                        .flatten()
                        .or_else(|| row.try_get::<i64, _>(index).ok().map(|v| v.to_string()))
                        .or_else(|| row.try_get::<i32, _>(index).ok().map(|v| v.to_string()))
                        .or_else(|| row.try_get::<f64, _>(index).ok().map(|v| v.to_string()))
                        .or_else(|| row.try_get::<bool, _>(index).ok().map(|v| v.to_string()))
                })
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

        let execution_time_ms = start.elapsed().as_millis() as u64;
        let rows_affected = result_rows.len() as u64;
        Ok(QueryResult {
            raw_documents: None,
            columns,
            rows: result_rows,
            rows_affected,
            execution_time_ms,
            timing: None,
        })
    } else {
        let result = sqlx::query(AssertSqlSafe(sql))
            .execute(&mut *connection)
            .await
            .context("Query execution failed")?;

        Ok(QueryResult {
            raw_documents: None,
            columns: vec![],
            rows: vec![],
            rows_affected: result.rows_affected(),
            execution_time_ms: start.elapsed().as_millis() as u64,
            timing: None,
        })
    }
}

/// The column names a statement would return, asked of the database rather
/// than read off a row.
///
/// A result with no rows has no row to take the names from, and a grid with no
/// columns is not an empty result -- it is a table the reader cannot see the
/// shape of, and cannot add a row to. Asked for only in that case.
///
/// Best effort on purpose. A statement that will not describe leaves the grid
/// as it was rather than turning an empty result into an error.
async fn the_columns_the_statement_returns(
    connection: &mut sqlx::SqliteConnection,
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
        // A statement that will not describe leaves the grid as it was rather
        // than turning an empty result into an error.
        Err(_) => Vec::new(),
    }
}

/// What a statement does to an open transaction.
///
/// This decides both whether a statement is routed to the held connection and
/// whether that connection is released afterwards, and SQLite's spellings make
/// the decision less obvious than it looks:
///
/// * `END` and `END TRANSACTION` are aliases for `COMMIT`, so a reader who
///   types `END` has ended the transaction even though the word never appears
///   in this driver's own SQL.
/// * The locking behaviour is part of the `BEGIN` syntax rather than a
///   statement of its own, so `BEGIN DEFERRED`, `BEGIN IMMEDIATE` and
///   `BEGIN EXCLUSIVE`, with or without the `TRANSACTION` keyword, all begin
///   one and must not be mistaken for something else because of the extra word.
/// * `ROLLBACK TO <name>` reads like a rollback and is not one: it unwinds to
///   the savepoint, leaves that savepoint in place and leaves the transaction
///   open. Treating it as an end would unpin the connection the reader is
///   still working inside.
/// * `SAVEPOINT <name>` outside a transaction is defined to behave as
///   `BEGIN DEFERRED`, so it begins one. It has to be pinned rather than sent
///   to the pool, because sqlx does not roll back a transaction opened by raw
///   SQL when a connection returns to the pool -- it only pings it -- so a
///   savepoint left on a pooled connection would stay open on it and be handed
///   to whatever query ran next.
///
/// https://www.sqlite.org/lang_transaction.html
/// https://www.sqlite.org/lang_savepoint.html
///
/// `RELEASE <name>` is deliberately `Leaves`. Releasing an inner savepoint
/// writes nothing and ends nothing, while releasing the outermost savepoint is
/// the same as `COMMIT`, and the text of the statement does not say which it
/// is. Since `begin_transaction` issues a real `BEGIN`, no `RELEASE` can be
/// outermost there and this is exact; the inexact case is a reader who opened
/// the transaction by typing `SAVEPOINT` themselves, and it costs a connection
/// left pinned with nothing open on it, which the next rollback clears.
#[derive(Debug, PartialEq, Eq)]
enum TransactionEffect {
    Begins,
    Ends,
    Leaves,
}

fn what_it_does_to_the_transaction(sql: &str) -> TransactionEffect {
    let statement = sql.trim().trim_end_matches(';');
    let mut words = statement.split_whitespace().map(str::to_ascii_uppercase);
    let Some(first) = words.next() else {
        return TransactionEffect::Leaves;
    };
    match first.as_str() {
        "BEGIN" | "SAVEPOINT" => TransactionEffect::Begins,
        "COMMIT" | "END" => TransactionEffect::Ends,
        "ROLLBACK" => {
            if words.any(|word| word == "TO") {
                TransactionEffect::Leaves
            } else {
                TransactionEffect::Ends
            }
        }
        _ => TransactionEffect::Leaves,
    }
}

fn rename_table_sql(old_name: &str, new_name: &str) -> String {
    let safe_old = old_name.replace('"', "\"\"");
    let safe_new = new_name.replace('"', "\"\"");
    format!("ALTER TABLE \"{safe_old}\" RENAME TO \"{safe_new}\"")
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (offset, &b) in bytes[open..].iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

// Looks for `CONSTRAINT <name>` immediately before a `CHECK` keyword, so an
// explicitly named constraint keeps its name instead of getting a synthetic
// one.
fn explicit_constraint_name(sql: &str, check_keyword_start: usize) -> Option<String> {
    let prefix = sql[..check_keyword_start].trim_end();
    let name_start = prefix
        .rfind(|c: char| c.is_whitespace())
        .map_or(0, |i| i + 1);
    let name = &prefix[name_start..];
    if name.is_empty() {
        return None;
    }
    let before_name = prefix[..name_start].trim_end();
    if before_name.to_uppercase().ends_with("CONSTRAINT") {
        Some(
            name.trim_matches(|c| c == '"' || c == '`' || c == '\'')
                .to_string(),
        )
    } else {
        None
    }
}

fn extract_check_constraints(create_table_sql: &str) -> Vec<CheckConstraintInfo> {
    let bytes = create_table_sql.as_bytes();
    let upper = create_table_sql.to_uppercase();
    let mut constraints = Vec::new();
    let mut search_start = 0;
    let mut anon_index = 1;
    while let Some(rel_pos) = upper[search_start..].find("CHECK") {
        let keyword_start = search_start + rel_pos;
        let keyword_end = keyword_start + "CHECK".len();
        let word_boundary_before = keyword_start == 0 || !is_ident_byte(bytes[keyword_start - 1]);
        let word_boundary_after = keyword_end >= bytes.len() || !is_ident_byte(bytes[keyword_end]);
        let mut cursor = keyword_end;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if word_boundary_before && word_boundary_after && bytes.get(cursor) == Some(&b'(') {
            if let Some(close) = matching_paren(bytes, cursor) {
                let expression = create_table_sql[cursor + 1..close].trim().to_string();
                let name = explicit_constraint_name(create_table_sql, keyword_start)
                    .unwrap_or_else(|| {
                        let generated = format!("check_{anon_index}");
                        anon_index += 1;
                        generated
                    });
                constraints.push(CheckConstraintInfo { name, expression });
                search_start = close + 1;
                continue;
            }
        }
        search_start = keyword_end;
    }
    constraints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_table_sql_quotes_both_names() {
        assert_eq!(
            rename_table_sql("users", "customers"),
            "ALTER TABLE \"users\" RENAME TO \"customers\""
        );
    }

    #[test]
    fn rename_table_sql_escapes_embedded_double_quotes() {
        assert_eq!(
            rename_table_sql("us\"ers", "cust\"omers"),
            "ALTER TABLE \"us\"\"ers\" RENAME TO \"cust\"\"omers\""
        );
    }

    // Gates the "NULL decodes as 0" hypothesis from the grid UX audit: writes
    // a genuine SQL NULL into a TEXT and an INTEGER column and decodes it
    // through the exact same execute_query path production queries use.
    #[tokio::test]
    async fn null_cells_decode_as_none_not_as_zero_or_empty_string() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("failed to open in-memory sqlite pool");
        let provider = SqliteProvider::new(pool, SqliteConnectOptions::new(), "test".to_string());

        sqlx::query("CREATE TABLE t (name TEXT, amount INTEGER)")
            .execute(&provider.pool)
            .await
            .expect("failed to create table");
        sqlx::query("INSERT INTO t (name, amount) VALUES (NULL, NULL)")
            .execute(&provider.pool)
            .await
            .expect("failed to insert null row");

        let result = provider
            .execute_query("test", "SELECT name, amount FROM t")
            .await
            .expect("failed to execute select");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0][0], None,
            "a NULL TEXT column must decode to None, not Some(\"0\")/Some(\"\")"
        );
        assert_eq!(
            result.rows[0][1], None,
            "a NULL INTEGER column must decode to None, not Some(\"0\")"
        );
    }

    #[test]
    fn extract_check_constraints_finds_an_unnamed_check() {
        let ddl = "CREATE TABLE t (age INTEGER, CHECK (age >= 0))";
        let checks = extract_check_constraints(ddl);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "check_1");
        assert_eq!(checks[0].expression, "age >= 0");
    }

    #[test]
    fn extract_check_constraints_uses_the_explicit_constraint_name() {
        let ddl = "CREATE TABLE t (age INTEGER, CONSTRAINT age_non_negative CHECK (age >= 0))";
        let checks = extract_check_constraints(ddl);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "age_non_negative");
        assert_eq!(checks[0].expression, "age >= 0");
    }

    #[test]
    fn extract_check_constraints_handles_nested_parens_and_multiple_checks() {
        let ddl = "CREATE TABLE t (\
            age INTEGER CHECK (age BETWEEN 0 AND (100 + 1)), \
            status TEXT CONSTRAINT status_valid CHECK (status IN ('a', 'b'))\
        )";
        let checks = extract_check_constraints(ddl);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].name, "check_1");
        assert_eq!(checks[0].expression, "age BETWEEN 0 AND (100 + 1)");
        assert_eq!(checks[1].name, "status_valid");
        assert_eq!(checks[1].expression, "status IN ('a', 'b')");
    }

    #[test]
    fn extract_check_constraints_ignores_a_column_named_check_like() {
        let ddl = "CREATE TABLE t (checksum TEXT, id INTEGER)";
        assert!(extract_check_constraints(ddl).is_empty());
    }

    #[test]
    fn extract_check_constraints_returns_empty_for_a_table_without_checks() {
        let ddl = "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)";
        assert!(extract_check_constraints(ddl).is_empty());
    }

    async fn a_provider_on_a_real_file() -> (tempfile::TempDir, SqliteProvider) {
        let directory = tempfile::tempdir().expect("failed to make a temp directory");
        let options = SqliteConnectOptions::new()
            .filename(directory.path().join("held.db"))
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options.clone())
            .await
            .expect("failed to open the temp database");
        let provider = SqliteProvider::new(pool, options, "held.db".to_string());
        sqlx::query("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)")
            .execute(&provider.pool)
            .await
            .expect("failed to create the table");
        (directory, provider)
    }

    async fn names_via_the_console(provider: &SqliteProvider) -> Vec<String> {
        provider
            .execute_query("held.db", "SELECT name FROM people ORDER BY name")
            .await
            .expect("failed to read people through the console path")
            .rows
            .into_iter()
            .filter_map(|row| row.into_iter().next().flatten())
            .collect()
    }

    async fn names_via_the_pool(provider: &SqliteProvider) -> Vec<String> {
        sqlx::query_as::<_, (String,)>("SELECT name FROM people ORDER BY name")
            .fetch_all(&provider.pool)
            .await
            .expect("failed to read people through the pool")
            .into_iter()
            .map(|(name,)| name)
            .collect()
    }

    /// A query that matches nothing still has to say what the columns are.
    ///
    /// The reported symptom was a grid reading "0 rows · 0 cols" with not even
    /// a header, which is not an empty table -- it is a table whose shape is
    /// invisible and which no row can be added to by hand. The names used to
    /// be taken off the first row, so an empty result had nowhere to take them
    /// from.
    #[tokio::test]
    async fn an_empty_result_still_says_what_its_columns_are() {
        let (_directory, provider) = a_provider_on_a_real_file().await;

        let answered = provider
            .execute_query(
                "held.db",
                "SELECT id, name FROM people WHERE name = 'nobody'",
            )
            .await
            .expect("failed to run a query that matches nothing");

        assert!(answered.rows.is_empty(), "{:?}", answered.rows);
        assert_eq!(
            answered.columns,
            vec!["id".to_string(), "name".to_string()],
            "an empty result must still carry the columns it would have had"
        );
    }

    /// The same, inside a transaction and for a table that exists only inside
    /// it. This is what forces the columns to be asked of the transaction's own
    /// connection: on any other one the table does not exist, the question
    /// fails, and the grid is empty again.
    #[tokio::test]
    async fn an_empty_result_from_a_table_made_inside_the_transaction_says_its_columns() {
        let (_directory, provider) = a_provider_on_a_real_file().await;

        provider
            .begin_transaction("held.db", Some(Duration::from_secs(60)))
            .await
            .expect("failed to begin");
        provider
            .execute_query("held.db", "CREATE TABLE staged (ticker TEXT, price REAL)")
            .await
            .expect("failed to create a table inside the transaction");

        let answered = provider
            .execute_query("held.db", "SELECT ticker, price FROM staged")
            .await
            .expect("failed to read the staged table");

        assert!(answered.rows.is_empty(), "{:?}", answered.rows);
        assert_eq!(
            answered.columns,
            vec!["ticker".to_string(), "price".to_string()],
            "the columns must come from the transaction's own connection"
        );

        provider
            .rollback_transaction()
            .await
            .expect("failed to roll back");
    }

    #[tokio::test]
    async fn a_rolled_back_transaction_leaves_no_row_behind() {
        let (_directory, provider) = a_provider_on_a_real_file().await;

        provider
            .begin_transaction("held.db", Some(Duration::from_secs(60)))
            .await
            .expect("failed to begin");
        provider
            .execute_query("held.db", "INSERT INTO people (name) VALUES ('ada')")
            .await
            .expect("failed to insert");
        assert_eq!(names_via_the_console(&provider).await, vec!["ada"]);

        provider
            .rollback_transaction()
            .await
            .expect("failed to roll back");

        assert!(names_via_the_console(&provider).await.is_empty());
        assert!(names_via_the_pool(&provider).await.is_empty());
        assert_eq!(provider.transaction_open_since(), None);
    }

    #[tokio::test]
    async fn a_committed_transaction_keeps_the_row() {
        let (_directory, provider) = a_provider_on_a_real_file().await;

        provider
            .begin_transaction("held.db", Some(Duration::from_secs(60)))
            .await
            .expect("failed to begin");
        provider
            .execute_query("held.db", "INSERT INTO people (name) VALUES ('grace')")
            .await
            .expect("failed to insert");
        provider
            .commit_transaction()
            .await
            .expect("failed to commit");

        assert_eq!(names_via_the_pool(&provider).await, vec!["grace"]);
        assert_eq!(provider.transaction_open_since(), None);
    }

    // The point of the held connection: a row that only exists inside the
    // transaction has to be visible to the console and invisible to anything
    // going through the pool, because those are different connections and only
    // one of them is inside the transaction.
    #[tokio::test]
    async fn an_uncommitted_row_is_visible_to_the_console_and_not_to_the_pool() {
        let (_directory, provider) = a_provider_on_a_real_file().await;

        provider
            .begin_transaction("held.db", Some(Duration::from_secs(60)))
            .await
            .expect("failed to begin");
        provider
            .execute_query("held.db", "INSERT INTO people (name) VALUES ('staged')")
            .await
            .expect("failed to insert");

        assert_eq!(names_via_the_console(&provider).await, vec!["staged"]);
        assert!(
            names_via_the_pool(&provider).await.is_empty(),
            "the pool must not be inside the transaction"
        );

        provider
            .rollback_transaction()
            .await
            .expect("failed to roll back");
    }

    // The same split through a real metadata call rather than a raw pool read:
    // a table created inside the transaction must not appear in the schema
    // tree, because schema browsing must not be enrolled in the reader's
    // transaction.
    #[tokio::test]
    async fn a_table_created_inside_the_transaction_is_not_listed_by_the_metadata_path() {
        let (_directory, provider) = a_provider_on_a_real_file().await;

        provider
            .begin_transaction("held.db", Some(Duration::from_secs(60)))
            .await
            .expect("failed to begin");
        provider
            .execute_query("held.db", "CREATE TABLE staged_table (id INTEGER)")
            .await
            .expect("failed to create the staged table");

        let listed: Vec<String> = provider
            .list_tables("held.db")
            .await
            .expect("failed to list tables")
            .into_iter()
            .map(|table| table.name)
            .collect();
        assert!(
            !listed.iter().any(|name| name == "staged_table"),
            "list_tables goes through the pool, so it must not see the staged table: {listed:?}"
        );

        let seen_by_the_console = provider
            .execute_query(
                "held.db",
                "SELECT name FROM sqlite_master WHERE name = 'staged_table'",
            )
            .await
            .expect("failed to read sqlite_master through the console path");
        assert_eq!(
            seen_by_the_console.rows.len(),
            1,
            "the console is inside the transaction, so it must see the staged table"
        );

        provider
            .rollback_transaction()
            .await
            .expect("failed to roll back");
    }

    #[tokio::test]
    async fn rolling_back_to_a_savepoint_undoes_only_that_much_and_keeps_the_transaction_open() {
        let (_directory, provider) = a_provider_on_a_real_file().await;

        provider
            .begin_transaction("held.db", Some(Duration::from_secs(60)))
            .await
            .expect("failed to begin");
        provider
            .execute_query("held.db", "INSERT INTO people (name) VALUES ('kept')")
            .await
            .expect("failed to insert the kept row");
        provider
            .execute_query("held.db", "SAVEPOINT after_kept")
            .await
            .expect("failed to open the savepoint");
        provider
            .execute_query("held.db", "INSERT INTO people (name) VALUES ('undone')")
            .await
            .expect("failed to insert the undone row");
        provider
            .execute_query("held.db", "ROLLBACK TO after_kept")
            .await
            .expect("failed to roll back to the savepoint");

        assert!(
            provider.transaction_open_since().is_some(),
            "ROLLBACK TO must not end the transaction"
        );
        assert_eq!(names_via_the_console(&provider).await, vec!["kept"]);

        provider
            .commit_transaction()
            .await
            .expect("failed to commit");
        assert_eq!(names_via_the_pool(&provider).await, vec!["kept"]);
    }

    #[tokio::test]
    async fn a_hand_typed_begin_is_held_and_a_hand_typed_end_releases_it() {
        let (_directory, provider) = a_provider_on_a_real_file().await;

        provider
            .execute_query("held.db", "BEGIN IMMEDIATE TRANSACTION")
            .await
            .expect("failed to begin by hand");
        assert!(
            provider.transaction_open_since().is_some(),
            "a hand-typed BEGIN must pin a connection just as begin_transaction does"
        );
        provider
            .execute_query("held.db", "INSERT INTO people (name) VALUES ('typed')")
            .await
            .expect("failed to insert");
        assert!(names_via_the_pool(&provider).await.is_empty());

        // END is a synonym for COMMIT, so this must release the connection and
        // make the row durable.
        provider
            .execute_query("held.db", "END TRANSACTION;")
            .await
            .expect("failed to end by hand");

        assert_eq!(provider.transaction_open_since(), None);
        assert_eq!(names_via_the_pool(&provider).await, vec!["typed"]);
    }

    // A hand-typed SAVEPOINT outside a transaction behaves as BEGIN DEFERRED,
    // so it is pinned; releasing it is the same as COMMIT, which the statement
    // text cannot reveal. Rollback has to stay safe in that state.
    #[tokio::test]
    async fn rollback_is_safe_after_a_released_outermost_savepoint() {
        let (_directory, provider) = a_provider_on_a_real_file().await;

        provider
            .execute_query("held.db", "SAVEPOINT outermost")
            .await
            .expect("failed to open the outermost savepoint");
        assert!(provider.transaction_open_since().is_some());
        provider
            .execute_query("held.db", "INSERT INTO people (name) VALUES ('released')")
            .await
            .expect("failed to insert");
        provider
            .execute_query("held.db", "RELEASE outermost")
            .await
            .expect("failed to release the savepoint");

        provider
            .rollback_transaction()
            .await
            .expect("rollback must not fail when the release already committed");
        assert_eq!(names_via_the_pool(&provider).await, vec!["released"]);
    }

    #[tokio::test]
    async fn rolling_back_with_nothing_open_is_not_an_error() {
        let (_directory, provider) = a_provider_on_a_real_file().await;
        provider
            .rollback_transaction()
            .await
            .expect("rollback with nothing open must succeed");
        provider
            .rollback_transaction()
            .await
            .expect("rollback must stay safe when called twice");
    }

    #[tokio::test]
    async fn a_file_backed_provider_holds_transactions_and_an_in_memory_one_does_not() {
        let (_directory, provider) = a_provider_on_a_real_file().await;
        assert!(provider.holds_transactions());

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("failed to open an in-memory pool");
        let in_memory = SqliteProvider::new(pool, SqliteConnectOptions::new(), "mem".to_string());
        assert!(!in_memory.holds_transactions());
        assert!(
            in_memory
                .begin_transaction("mem", Some(Duration::from_secs(60)))
                .await
                .is_err(),
            "a driver that answers false must refuse rather than pretend"
        );
    }

    #[test]
    fn statement_classification_covers_sqlites_own_spellings() {
        use TransactionEffect::*;

        let cases: &[(&str, TransactionEffect)] = &[
            ("BEGIN", Begins),
            ("begin", Begins),
            ("  Begin  ", Begins),
            ("BEGIN;", Begins),
            ("BEGIN TRANSACTION", Begins),
            ("begin deferred", Begins),
            ("BEGIN IMMEDIATE", Begins),
            ("BEGIN EXCLUSIVE TRANSACTION;", Begins),
            ("SAVEPOINT sp", Begins),
            ("savepoint  sp ;", Begins),
            ("COMMIT", Ends),
            ("commit transaction;", Ends),
            ("END", Ends),
            ("end transaction", Ends),
            ("ROLLBACK", Ends),
            ("rollback;", Ends),
            ("ROLLBACK TRANSACTION", Ends),
            ("ROLLBACK TO sp", Leaves),
            ("rollback to savepoint sp", Leaves),
            ("ROLLBACK TRANSACTION TO SAVEPOINT sp;", Leaves),
            ("RELEASE sp", Leaves),
            ("release savepoint sp", Leaves),
            ("SELECT 1", Leaves),
            ("INSERT INTO people (name) VALUES ('x')", Leaves),
            // A trigger body contains BEGIN and END, and the statement is a
            // CREATE: only the leading keyword decides.
            (
                "CREATE TRIGGER t AFTER INSERT ON people BEGIN SELECT 1; END",
                Leaves,
            ),
            ("", Leaves),
            ("   ", Leaves),
        ];

        for (sql, expected) in cases {
            assert_eq!(
                &what_it_does_to_the_transaction(sql),
                expected,
                "misclassified {sql:?}"
            );
        }
    }
}
