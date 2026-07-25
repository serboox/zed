pub mod aerospike_provider;
pub mod cassandra_provider;
pub mod clickhouse;
pub mod connection;
pub mod kubernetes_tunnel;
pub mod mongo_provider;
pub mod mysql;
pub mod postgres;
pub mod provider;
pub mod redis_provider;
pub mod runtime;
pub mod schema;
pub mod sqlite;
pub mod ssh_tunnel;

pub use connection::{
    ConnectionConfig, ConnectionId, DatabaseDriver, Folder, FolderId, KubernetesRelayCommandKind,
    KubernetesTargetKind, KubernetesTunnelModeKind, MAX_FOLDER_DEPTH, SshAuthMethod, SslMode,
};
pub use kubernetes_tunnel::{
    KubernetesRelayCommand, KubernetesTarget, KubernetesTunnel, KubernetesTunnelMode,
    kubernetes_tunnel_caveat,
};
pub use provider::DbProvider;
pub use runtime::{RuntimeProvider, on_runtime};
pub use schema::{
    CheckConstraintInfo, ColumnInfo, DatabaseInfo, FkInfo, IndexInfo, ProcedureInfo, ProcedureKind,
    QueryResult, TableInfo, TableKind, TriggerInfo, UserInfo,
};
pub use ssh_tunnel::{SshAuth, SshTunnel};

/// Client name reported to the server in the `ApplicationName` tag (useful for
/// query analytics). Neutral by default; override via `application_name_comment`
/// or change here to present as a specific GUI client.
pub const DEFAULT_APPLICATION_NAME: &str = "Zed";

/// Builds the leading comment prepended to user queries so servers can attribute
/// them via the `ApplicationName` tag. Read/write detection must use the original
/// SQL, since this comment is not a keyword.
pub fn application_name_comment(application_name: &str) -> String {
    format!("/* ApplicationName={application_name} */ ")
}

/// Default number of rows fetched for a result page, matching the common
/// "result pages" default of GUI database clients. Caps memory and render cost
/// for unbounded SELECTs.
pub const DEFAULT_PAGE_SIZE: usize = 500;

/// Hard ceiling on rows decoded from a single result, independent of any SQL
/// LIMIT. A safety net so a query that slips past the UI's default limit cannot
/// pull an unbounded result into memory and freeze the client. Shared by every
/// provider so the cap is uniform across drivers.
pub const MAX_RESULT_ROWS: usize = 500;

/// Uniform ceiling on how long any single database call (query execution,
/// connect, schema lookup, ...) may run before it is aborted with a timeout
/// error, applied once in [`runtime::on_runtime`] so it covers every
/// [`provider::DbProvider`] method for every current and future driver
/// without each one needing its own timeout. Long enough that a genuinely
/// slow query the user is deliberately waiting on is not cut off — if they
/// no longer want the result, disconnecting is the intended way to cancel.
pub const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Wording that identifies a lost or unusable connection across the drivers.
/// Matched case-insensitively against every error in a chain, because each
/// driver reports the same underlying socket death in its own words and only
/// sqlx exposes a typed variant for it.
const TRANSPORT_FAILURE_MARKERS: &[&str] = &[
    "broken pipe",
    "connection reset",
    "connection refused",
    "connection closed",
    "connection aborted",
    "connection unexpectedly closed",
    "server has gone away",
    "lost connection",
    "server closed the connection",
    "terminating connection",
    "no connection to the server",
    "pool timed out",
    "pool has been closed",
    "closed pool",
    "timed out",
    "unexpected end of file",
    "unexpected eof",
    "network is unreachable",
    "no route to host",
    "not connected",
];

fn mentions_transport_failure(message: &str) -> bool {
    let lowered = message.to_lowercase();
    TRANSPORT_FAILURE_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// True when `error` means the connection itself is gone -- a dead socket, a
/// closed or exhausted pool, a collapsed tunnel, a failed handshake, or the
/// uniform [`QUERY_TIMEOUT`] firing on a wedged call.
///
/// The distinction decides whether a cached provider is still worth anything:
/// a syntax or permission error arrives over a healthy connection and must not
/// throw it away, while a transport failure leaves a handle that can never
/// succeed again, so it has to be dropped and rebuilt.
pub fn is_transport_error(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        if let Some(sqlx_error) = cause.downcast_ref::<sqlx::Error>() {
            return match sqlx_error {
                sqlx::Error::Io(_)
                | sqlx::Error::Tls(_)
                | sqlx::Error::Protocol(_)
                | sqlx::Error::PoolTimedOut
                | sqlx::Error::PoolClosed
                | sqlx::Error::WorkerCrashed => true,
                // Reached the server over a working socket, so only the codes
                // the servers use to announce a dropped session count.
                sqlx::Error::Database(db_error) => mentions_transport_failure(db_error.message()),
                _ => false,
            };
        }
        if cause.downcast_ref::<std::io::Error>().is_some() {
            return true;
        }
    }
    mentions_transport_failure(&format!("{error:#}"))
}

/// True when `sql` only reads data, so it is safe to wrap or limit. A leading
/// keyword check mirrors the per-provider detection used for the read/write
/// split, kept here so the UI can reason about a statement without a provider.
pub fn is_read_only_query(sql: &str) -> bool {
    let upper = sql.trim_start().to_uppercase();
    upper.starts_with("SELECT")
        || upper.starts_with("WITH")
        || upper.starts_with("SHOW")
        || upper.starts_with("DESCRIBE")
        || upper.starts_with("DESC ")
        || upper.starts_with("EXPLAIN")
}

/// True when `sql` can be wrapped in a subquery for client-side paging.
/// Metadata statements such as SHOW/DESCRIBE/EXPLAIN may return rows, but most
/// databases do not allow them inside `SELECT * FROM (...)`.
pub fn is_pageable_query(sql: &str) -> bool {
    let upper = sql.trim_start().to_uppercase();
    upper.starts_with("SELECT") || upper.starts_with("WITH")
}

/// True when `sql` already contains a `LIMIT` keyword, so appending another
/// would produce invalid SQL. Tokenizes on non-alphanumerics so a column named
/// like "delimiter" is not mistaken for a LIMIT clause.
fn has_limit_clause(sql: &str) -> bool {
    sql.to_uppercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| word == "LIMIT")
}

/// Appends a default `LIMIT` to an unbounded read-only query so a `SELECT *`
/// over a huge table does not pull the whole table into memory and freeze the
/// client. Non-read statements, or queries that already set their own LIMIT,
/// are returned unchanged.
pub fn apply_default_limit(sql: &str, limit: usize) -> String {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if !is_pageable_query(trimmed) || has_limit_clause(trimmed) {
        return sql.to_string();
    }
    format!("{trimmed} LIMIT {limit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_limit_only_to_unbounded_read_queries() {
        assert!(is_read_only_query("SHOW CREATE TABLE instruments.splits"));
        assert!(!is_pageable_query("SHOW CREATE TABLE instruments.splits"));

        assert_eq!(
            apply_default_limit("SELECT * FROM users", 500),
            "SELECT * FROM users LIMIT 500"
        );
        // Trailing semicolon and whitespace are normalized before appending.
        assert_eq!(
            apply_default_limit("  SELECT * FROM users ;  ", 500),
            "SELECT * FROM users LIMIT 500"
        );
        // An existing LIMIT is respected — never produce "LIMIT x LIMIT y".
        assert_eq!(
            apply_default_limit("SELECT * FROM users LIMIT 10", 500),
            "SELECT * FROM users LIMIT 10"
        );
        // Writes are never limited.
        assert_eq!(
            apply_default_limit("UPDATE users SET name = 'a'", 500),
            "UPDATE users SET name = 'a'"
        );
        // Metadata statements can return rows, but they are not valid subqueries
        // and must be sent to the database unchanged.
        assert_eq!(
            apply_default_limit("SHOW CREATE TABLE instruments.splits", 500),
            "SHOW CREATE TABLE instruments.splits"
        );
        // A column whose name merely contains "limit" is not a LIMIT clause.
        assert_eq!(
            apply_default_limit("SELECT delimiter FROM cfg", 500),
            "SELECT delimiter FROM cfg LIMIT 500"
        );
    }

    #[test]
    fn transport_errors_are_told_apart_from_statement_errors() {
        // Server-side complaints arrive over a healthy socket: the provider
        // must survive them.
        assert!(!is_transport_error(&anyhow::anyhow!(
            "You have an error in your SQL syntax near 'SELCT'"
        )));
        assert!(!is_transport_error(&anyhow::anyhow!(
            "Access denied for user 'reader'@'localhost'"
        )));

        // Socket death, however worded by whichever driver.
        for message in [
            "Query execution failed: Broken pipe (os error 32)",
            "MySQL server has gone away",
            "Lost connection to MySQL server during query",
            "server closed the connection unexpectedly",
            "Database call timed out after 30 minutes",
        ] {
            assert!(
                is_transport_error(&anyhow::anyhow!(message)),
                "expected a transport failure for: {message}"
            );
        }

        // Wrapped in context the way the providers report it.
        let wrapped = anyhow::Error::new(sqlx::Error::PoolClosed).context("Query execution failed");
        assert!(is_transport_error(&wrapped));
        let io_failure =
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
                .context("list_databases failed");
        assert!(is_transport_error(&io_failure));

        // A typed database error keeps the connection: not transport.
        assert!(!is_transport_error(&anyhow::Error::new(
            sqlx::Error::RowNotFound
        )));
    }
}
