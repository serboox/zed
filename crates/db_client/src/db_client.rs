pub mod clickhouse;
pub mod connection;
pub mod mysql;
pub mod postgres;
pub mod provider;
pub mod redis_provider;
pub mod runtime;
pub mod schema;
pub mod sqlite;
pub mod ssh_tunnel;

pub use connection::{ConnectionConfig, ConnectionId, DatabaseDriver};
pub use mysql::{MAX_CELL_BYTES, is_cell_possibly_truncated};
pub use provider::DbProvider;
pub use runtime::{RuntimeProvider, on_runtime};
pub use schema::{
    ColumnInfo, DatabaseInfo, FkInfo, IndexInfo, ProcedureInfo, ProcedureKind, QueryResult,
    TableInfo, TableKind, TriggerInfo, UserInfo,
};
pub use ssh_tunnel::SshTunnel;

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
    if !is_read_only_query(trimmed) || has_limit_clause(trimmed) {
        return sql.to_string();
    }
    format!("{trimmed} LIMIT {limit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_limit_only_to_unbounded_read_queries() {
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
        // A column whose name merely contains "limit" is not a LIMIT clause.
        assert_eq!(
            apply_default_limit("SELECT delimiter FROM cfg", 500),
            "SELECT delimiter FROM cfg LIMIT 500"
        );
    }
}
