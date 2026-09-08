use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::schema::{
    CheckConstraintInfo, ColumnInfo, DatabaseInfo, EventInfo, FkInfo, IndexInfo, ProcedureInfo,
    QueryResult, SequenceInfo, TableInfo, TriggerInfo, UserInfo,
};

/// Receives a read query's columns and rows one at a time, so a caller can
/// write them straight to a file without ever holding the full result set in
/// memory. `write_row` returning `Err` aborts the query mid-stream (used to
/// implement cancellation without a separate cancellation channel).
pub trait RowSink: Send {
    fn write_columns(&mut self, columns: &[String]) -> Result<()>;
    fn write_row(&mut self, row: &[Option<String>]) -> Result<()>;
}

#[async_trait]
pub trait DbProvider: Send + Sync {
    async fn ping(&self) -> Result<()>;
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>>;
    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>>;
    async fn describe_table(&self, database: &str, table: &str) -> Result<Vec<ColumnInfo>>;

    /// The columns of every table in one database, in one exchange with the
    /// server where the server can answer that way.
    ///
    /// Asking table by table is a round trip each, and a schema has hundreds of
    /// them: on a server reached over a slow link that is minutes of talking for
    /// something one query can answer. A provider that cannot do it in one goes
    /// on asking table by table, which is what this default does.
    async fn describe_database(
        &self,
        database: &str,
        tables: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<ColumnInfo>>> {
        let mut columns = std::collections::HashMap::new();
        for table in tables {
            if let Ok(found) = self.describe_table(database, table).await {
                columns.insert(table.clone(), found);
            }
        }
        Ok(columns)
    }
    async fn execute_query(&self, database: &str, sql: &str) -> Result<QueryResult>;
    async fn get_table_ddl(&self, database: &str, table: &str) -> Result<String>;

    /// Whether this driver can hold a transaction open across statements.
    ///
    /// Answered by the driver rather than assumed, because a transaction that
    /// is not really held is worse than no transaction at all: the statements
    /// after it would run and commit one by one while the reader believed they
    /// were staged and could still be rolled back. A driver that returns false
    /// must refuse the three calls below rather than pretend.
    fn holds_transactions(&self) -> bool {
        false
    }

    /// Opens a transaction, and pins whatever connection it lives on for as
    /// long as it is open.
    ///
    /// The pinning is the substance of this. A transaction belongs to one
    /// physical connection: statements sent on another are outside it, and the
    /// connection it began on sits inside it holding locks. So while this is
    /// open, every statement from the console goes to that one connection --
    /// and the connection is kept out of the pool, where an idle timeout or a
    /// maximum lifetime would otherwise close it and roll the work back with
    /// nothing said.
    ///
    /// `abandoned_after` is how long the server should wait on a silent
    /// connection before closing it itself, and it is not the same guard as
    /// the editor's own idle timer. The editor's timer cannot run if the
    /// editor is killed, and a transaction left open then would hold its locks
    /// against everyone else until a person noticed. Told to the server at the
    /// moment the transaction opens, it is the one guard that survives the
    /// editor not being there to keep its promises.
    async fn begin_transaction(&self, _database: &str, _abandoned_after: Duration) -> Result<()> {
        anyhow::bail!("this connection cannot hold a transaction open")
    }

    /// Commits the open transaction and releases the connection it was on.
    async fn commit_transaction(&self) -> Result<()> {
        anyhow::bail!("this connection cannot hold a transaction open")
    }

    /// Rolls the open transaction back and releases the connection it was on.
    ///
    /// Must be safe to call when nothing is open: it is what everything that
    /// goes wrong reaches for -- a window closing, a connection being edited,
    /// an idle transaction reaching its limit -- and none of those can first
    /// check and then act without a gap in between.
    async fn rollback_transaction(&self) -> Result<()> {
        Ok(())
    }

    /// When the open transaction was opened, or nothing where none is.
    ///
    /// The instant rather than a duration, so that whoever displays it decides
    /// how often to recount and nothing has to be refreshed here.
    fn transaction_open_since(&self) -> Option<std::time::Instant> {
        None
    }

    /// Like `execute_query`, but pushes rows to `sink` as they arrive instead
    /// of collecting them into a `QueryResult`, and is not bound by
    /// `MAX_RESULT_ROWS` — for "execute to file" exports of result sets too
    /// large for the grid. Returns the number of rows streamed.
    ///
    /// The default falls back to the capped `execute_query`, so drivers
    /// without a real streaming override (SQLite, ClickHouse, Redis) still
    /// work correctly, just capped the same as the grid. MySQL and
    /// PostgreSQL override this with a genuinely unbounded stream.
    async fn execute_query_streaming(
        &self,
        database: &str,
        sql: &str,
        sink: &mut dyn RowSink,
    ) -> Result<u64> {
        let result = self.execute_query(database, sql).await?;
        sink.write_columns(&result.columns)?;
        for row in &result.rows {
            sink.write_row(row)?;
        }
        Ok(result.rows.len() as u64)
    }

    async fn get_database_ddl(&self, database: &str) -> Result<String> {
        Ok(format!("CREATE DATABASE {database};\n"))
    }

    async fn list_views(&self, _database: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn list_indexes(&self, _database: &str, _table: &str) -> Result<Vec<IndexInfo>> {
        Ok(Vec::new())
    }

    async fn list_foreign_keys(&self, _database: &str, _table: &str) -> Result<Vec<FkInfo>> {
        Ok(Vec::new())
    }

    async fn list_check_constraints(
        &self,
        _database: &str,
        _table: &str,
    ) -> Result<Vec<CheckConstraintInfo>> {
        Ok(Vec::new())
    }

    async fn list_procedures(&self, _database: &str) -> Result<Vec<ProcedureInfo>> {
        Ok(Vec::new())
    }

    async fn list_triggers(&self, _database: &str, _table: &str) -> Result<Vec<TriggerInfo>> {
        Ok(Vec::new())
    }

    // Sequences and scheduled events are not universal SQL concepts: MySQL has
    // no native sequence object (before MariaDB) and PostgreSQL has no native
    // event scheduler, so most drivers keep the empty default here.
    async fn list_sequences(&self, _database: &str) -> Result<Vec<SequenceInfo>> {
        Ok(Vec::new())
    }

    async fn list_events(&self, _database: &str) -> Result<Vec<EventInfo>> {
        Ok(Vec::new())
    }

    async fn list_users(&self) -> Result<Vec<UserInfo>> {
        Ok(Vec::new())
    }

    async fn truncate_table(&self, database: &str, table: &str) -> Result<()> {
        self.execute_query(database, &format!("TRUNCATE TABLE {}", table))
            .await?;
        Ok(())
    }

    async fn drop_table(&self, database: &str, table: &str) -> Result<()> {
        self.execute_query(database, &format!("DROP TABLE {}", table))
            .await?;
        Ok(())
    }

    async fn rename_table(&self, database: &str, old_name: &str, new_name: &str) -> Result<()> {
        self.execute_query(
            database,
            &format!("ALTER TABLE {} RENAME TO {}", old_name, new_name),
        )
        .await?;
        Ok(())
    }

    /// Fetches a single record by key, for key-value drivers with no query
    /// language (Aerospike). Returns bin/field name-value pairs, or `None`
    /// when the key does not exist. The default errors clearly for every
    /// driver that has a real query language instead.
    async fn get_record(
        &self,
        _namespace: &str,
        _set: &str,
        _key: &str,
    ) -> Result<Option<Vec<(String, String)>>> {
        Err(anyhow::anyhow!(
            "This driver does not support key-based Get — use a query instead."
        ))
    }

    /// Writes `bins` to a record by key, for key-value drivers with no query
    /// language (Aerospike). See [`get_record`](Self::get_record).
    async fn put_record(
        &self,
        _namespace: &str,
        _set: &str,
        _key: &str,
        _bins: &[(String, String)],
    ) -> Result<()> {
        Err(anyhow::anyhow!(
            "This driver does not support key-based Put — use a query instead."
        ))
    }

    /// Scans up to `limit` records in `namespace`/`set`, for key-value
    /// drivers with no query language (Aerospike). See
    /// [`get_record`](Self::get_record).
    async fn scan_records(
        &self,
        _namespace: &str,
        _set: &str,
        _limit: usize,
    ) -> Result<QueryResult> {
        Err(anyhow::anyhow!(
            "This driver does not support Scan — use a query instead."
        ))
    }
}
