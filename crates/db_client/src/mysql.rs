use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::TryStreamExt as _;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlSslMode};
use sqlx::{Column as _, Row as _};
use std::time::Instant;

pub use crate::{MAX_CELL_BYTES, is_cell_possibly_truncated};
use crate::{MAX_RESULT_ROWS, cap_cell};

use crate::connection::{ConnectionConfig, SslMode};
use crate::provider::DbProvider;
use crate::schema::{
    ColumnInfo, DatabaseInfo, FkInfo, IndexInfo, ProcedureInfo, ProcedureKind, QueryResult,
    TableInfo, TableKind, TriggerInfo, UserInfo,
};

pub struct MySqlProvider {
    pool: MySqlPool,
}

fn mysql_ssl_mode(mode: SslMode) -> MySqlSslMode {
    match mode {
        SslMode::Disabled => MySqlSslMode::Disabled,
        SslMode::Require => MySqlSslMode::Required,
        SslMode::VerifyCa => MySqlSslMode::VerifyCa,
        SslMode::VerifyFull => MySqlSslMode::VerifyIdentity,
    }
}

pub(crate) fn mysql_connect_options(config: &ConnectionConfig) -> MySqlConnectOptions {
    let mut opts = MySqlConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .username(&config.username)
        .password(&config.password)
        .database(config.database.as_deref().unwrap_or(""))
        .ssl_mode(mysql_ssl_mode(config.ssl_mode));
    if let Some(ca_path) = &config.ssl_ca_path {
        opts = opts.ssl_ca(ca_path);
    }
    if let Some(cert_path) = &config.ssl_client_cert_path {
        opts = opts.ssl_client_cert(cert_path);
    }
    if let Some(key_path) = &config.ssl_client_key_path {
        opts = opts.ssl_client_key(key_path);
    }
    opts
}

impl MySqlProvider {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let opts = mysql_connect_options(config);
        // Single connection: `execute_query` relies on `USE` staying applied
        // for the query that follows it, which only holds when both run on the
        // same physical connection. The metadata queries are fully qualified,
        // so serializing them through one connection is acceptable for a
        // single-user GUI client.
        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .context("Failed to connect to MySQL")?;
        Ok(Self { pool })
    }

    /// Reads the grant statements for a single account. SHOW GRANTS does not
    /// accept bind parameters, so the account is quoted inline; single quotes
    /// are escaped to keep the statement well-formed.
    async fn show_grants(&self, user: &str, host: &str) -> Result<Vec<String>> {
        let sql = format!(
            "SHOW GRANTS FOR '{}'@'{}'",
            user.replace('\'', "''"),
            host.replace('\'', "''"),
        );
        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .context("Failed to read grants")?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                row.try_get::<Vec<u8>, _>(0)
                    .ok()
                    .map(bytes_to_string)
                    .or_else(|| row.try_get::<String, _>(0).ok())
            })
            .collect())
    }
}

// MySQL reports string columns from SHOW/information_schema with a binary
// collation on some servers, so sqlx types them as VARBINARY and a direct
// String decode fails. Reading the raw bytes works for both VARCHAR and
// VARBINARY; convert lossily so non-UTF-8 bytes never abort a query.
fn bytes_to_string(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod connect_options_tests {
    use super::mysql_connect_options;
    use crate::connection::{ConnectionConfig, SslMode};
    use sqlx::mysql::MySqlSslMode;

    fn base_config() -> ConnectionConfig {
        ConnectionConfig {
            driver: crate::connection::DatabaseDriver::MySQL,
            host: "db.example.com".to_string(),
            port: 3306,
            username: "root".to_string(),
            password: "secret".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn ssl_mode_disabled_by_default() {
        let opts = mysql_connect_options(&base_config());
        assert!(matches!(opts.get_ssl_mode(), MySqlSslMode::Disabled));
    }

    #[test]
    fn ssl_mode_require_maps_to_required() {
        let mut config = base_config();
        config.ssl_mode = SslMode::Require;
        let opts = mysql_connect_options(&config);
        assert!(matches!(opts.get_ssl_mode(), MySqlSslMode::Required));
    }

    #[test]
    fn ssl_mode_verify_ca_maps_to_verify_ca() {
        let mut config = base_config();
        config.ssl_mode = SslMode::VerifyCa;
        config.ssl_ca_path = Some("/tmp/ca.pem".to_string());
        let opts = mysql_connect_options(&config);
        assert!(matches!(opts.get_ssl_mode(), MySqlSslMode::VerifyCa));
    }

    #[test]
    fn ssl_mode_verify_full_maps_to_verify_identity() {
        let mut config = base_config();
        config.ssl_mode = SslMode::VerifyFull;
        let opts = mysql_connect_options(&config);
        assert!(matches!(opts.get_ssl_mode(), MySqlSslMode::VerifyIdentity));
    }
}

#[async_trait]
impl DbProvider for MySqlProvider {
    async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .context("Ping failed")?;
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
        let rows = sqlx::query_as::<_, (Vec<u8>,)>("SHOW DATABASES")
            .fetch_all(&self.pool)
            .await
            .context("Failed to list databases")?;
        Ok(rows
            .into_iter()
            .map(|(name,)| DatabaseInfo {
                name: bytes_to_string(name),
            })
            .collect())
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>> {
        let rows = sqlx::query_as::<_, (Vec<u8>, Vec<u8>)>(
            "SELECT TABLE_NAME, TABLE_TYPE \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? \
             ORDER BY TABLE_NAME",
        )
        .bind(database)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list tables")?;

        Ok(rows
            .into_iter()
            .map(|(name, table_type)| TableInfo {
                name: bytes_to_string(name),
                kind: if bytes_to_string(table_type) == "VIEW" {
                    TableKind::View
                } else {
                    TableKind::Table
                },
            })
            .collect())
    }

    async fn list_views(&self, database: &str) -> Result<Vec<String>> {
        // SHOW FULL TABLES IN <db> does not support bind params.
        let escaped = database.replace('`', "``");
        let sql = format!(
            "-- name: ListViews :many\n\
             SHOW FULL TABLES IN `{escaped}` WHERE Table_type = 'VIEW'"
        );
        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .context("Failed to list views")?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                row.try_get::<Vec<u8>, _>(0)
                    .ok()
                    .map(bytes_to_string)
                    .or_else(|| row.try_get::<String, _>(0).ok())
            })
            .collect())
    }

    async fn list_foreign_keys(&self, database: &str, table: &str) -> Result<Vec<FkInfo>> {
        let rows = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)>(
            "-- name: ListForeignKeys :many
             SELECT CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME
             FROM information_schema.KEY_COLUMN_USAGE
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND REFERENCED_TABLE_NAME IS NOT NULL
             ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION",
        )
        .bind(database)
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list foreign keys")?;

        Ok(rows
            .into_iter()
            .map(|(name, from_col, to_table, to_col)| FkInfo {
                name: bytes_to_string(name),
                from_column: bytes_to_string(from_col),
                to_table: bytes_to_string(to_table),
                to_column: bytes_to_string(to_col),
            })
            .collect())
    }

    async fn describe_table(&self, database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let rows =
            sqlx::query_as::<_, (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>, Vec<u8>)>(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, COLUMN_DEFAULT, EXTRA \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             ORDER BY ORDINAL_POSITION",
            )
            .bind(database)
            .bind(table)
            .fetch_all(&self.pool)
            .await
            .context("Failed to describe table")?;

        Ok(rows
            .into_iter()
            .map(|(name, data_type, nullable, key, default_value, extra)| {
                let key = bytes_to_string(key);
                ColumnInfo {
                    name: bytes_to_string(name),
                    data_type: bytes_to_string(data_type),
                    is_nullable: bytes_to_string(nullable) == "YES",
                    column_key: if key.is_empty() { None } else { Some(key) },
                    default_value: default_value.map(bytes_to_string),
                    extra: bytes_to_string(extra),
                }
            })
            .collect())
    }

    async fn get_table_ddl(&self, database: &str, table: &str) -> Result<String> {
        let sql = format!(
            "SHOW CREATE TABLE `{}`.`{}`",
            database.replace('`', "``"),
            table.replace('`', "``")
        );
        let row = sqlx::query(&sql)
            .fetch_one(&self.pool)
            .await
            .context("Failed to get table DDL")?;
        row.try_get::<Vec<u8>, _>(1)
            .map(bytes_to_string)
            .or_else(|_| row.try_get::<String, _>(1))
            .context("Failed to read DDL from result")
    }

    async fn get_database_ddl(&self, database: &str) -> Result<String> {
        let sql = format!("SHOW CREATE DATABASE `{}`", database.replace('`', "``"));
        let row = sqlx::query(&sql)
            .fetch_one(&self.pool)
            .await
            .context("Failed to get database DDL")?;
        row.try_get::<Vec<u8>, _>(1)
            .map(bytes_to_string)
            .or_else(|_| row.try_get::<String, _>(1))
            .context("Failed to read database DDL from result")
    }

    async fn execute_query(&self, database: &str, sql: &str) -> Result<QueryResult> {
        if !database.is_empty() {
            // USE must use the text protocol; MySQL rejects it in the
            // prepared-statement protocol (error 1295). The single-connection
            // pool keeps it applied for the query below.
            let use_stmt = format!("USE `{}`", database.replace('`', "``"));
            sqlx::raw_sql(&use_stmt)
                .execute(&self.pool)
                .await
                .context("Failed to switch database")?;
        }

        let start = Instant::now();
        let trimmed_upper = sql.trim().to_uppercase();
        let is_read_query = trimmed_upper.starts_with("SELECT")
            || trimmed_upper.starts_with("SHOW")
            || trimmed_upper.starts_with("DESCRIBE")
            || trimmed_upper.starts_with("EXPLAIN")
            || trimmed_upper.starts_with("DESC")
            || trimmed_upper.starts_with("WITH");
        let prefixed = format!(
            "{}{}",
            crate::application_name_comment(crate::DEFAULT_APPLICATION_NAME),
            sql
        );

        if is_read_query {
            // Stream rows instead of buffering the whole result. Each row is
            // decoded and its cells capped before the next row is read, so a huge
            // result (many rows or multi-megabyte BLOB cells) cannot be pulled
            // into memory all at once and freeze the client.
            let mut stream = sqlx::raw_sql(&prefixed).fetch(&self.pool);
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
                    .map(|index| {
                        row.try_get::<Option<String>, _>(index)
                            .ok()
                            .flatten()
                            .or_else(|| row.try_get::<i64, _>(index).ok().map(|v| v.to_string()))
                            .or_else(|| row.try_get::<f64, _>(index).ok().map(|v| v.to_string()))
                            .or_else(|| row.try_get::<bool, _>(index).ok().map(|v| v.to_string()))
                            .or_else(|| {
                                row.try_get::<Option<Vec<u8>>, _>(index)
                                    .ok()
                                    .flatten()
                                    .map(bytes_to_string)
                            })
                            .map(cap_cell)
                    })
                    .collect();
                result_rows.push(decoded);

                if result_rows.len() >= MAX_RESULT_ROWS {
                    break;
                }
            }

            let execution_time_ms = start.elapsed().as_millis() as u64;
            let rows_affected = result_rows.len() as u64;
            Ok(QueryResult {
                columns,
                rows: result_rows,
                rows_affected,
                execution_time_ms,
            })
        } else {
            let result = sqlx::query(&prefixed)
                .execute(&self.pool)
                .await
                .context("Query execution failed")?;

            Ok(QueryResult {
                columns: vec![],
                rows: vec![],
                rows_affected: result.rows_affected(),
                execution_time_ms: start.elapsed().as_millis() as u64,
            })
        }
    }

    async fn list_indexes(&self, database: &str, table: &str) -> Result<Vec<IndexInfo>> {
        let rows = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, i64, Vec<u8>)>(
            "-- name: ListIndexes :many
             SELECT INDEX_NAME,
                    GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX SEPARATOR ','),
                    MAX(CAST(NON_UNIQUE = 0 AS SIGNED)),
                    MAX(INDEX_TYPE)
             FROM information_schema.STATISTICS
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
             GROUP BY INDEX_NAME",
        )
        .bind(database)
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list indexes")?;

        Ok(rows
            .into_iter()
            .map(|(name, cols_concat, unique_flag, index_type)| IndexInfo {
                name: bytes_to_string(name),
                columns: bytes_to_string(cols_concat)
                    .split(',')
                    .map(|s| s.to_string())
                    .collect(),
                unique: unique_flag == 1,
                index_type: bytes_to_string(index_type),
            })
            .collect())
    }

    async fn list_procedures(&self, database: &str) -> Result<Vec<ProcedureInfo>> {
        let rows = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, Option<Vec<u8>>)>(
            "-- name: ListProcedures :many
             SELECT ROUTINE_NAME, ROUTINE_TYPE, ROUTINE_DEFINITION
             FROM information_schema.ROUTINES
             WHERE ROUTINE_SCHEMA = ?
             ORDER BY ROUTINE_TYPE, ROUTINE_NAME",
        )
        .bind(database)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list procedures")?;

        Ok(rows
            .into_iter()
            .map(|(name, routine_type, definition)| ProcedureInfo {
                name: bytes_to_string(name),
                kind: if bytes_to_string(routine_type) == "FUNCTION" {
                    ProcedureKind::Function
                } else {
                    ProcedureKind::Procedure
                },
                definition: definition.map(bytes_to_string),
            })
            .collect())
    }

    async fn list_triggers(&self, database: &str, table: &str) -> Result<Vec<TriggerInfo>> {
        let rows = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)>(
            "-- name: ListTriggers :many
             SELECT TRIGGER_NAME, EVENT_MANIPULATION, ACTION_TIMING, EVENT_OBJECT_TABLE, ACTION_STATEMENT
             FROM information_schema.TRIGGERS
             WHERE TRIGGER_SCHEMA = ? AND EVENT_OBJECT_TABLE = ?
             ORDER BY TRIGGER_NAME",
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
                    name: bytes_to_string(name),
                    event: bytes_to_string(event),
                    timing: bytes_to_string(timing),
                    table_name: bytes_to_string(table_name),
                    definition: Some(bytes_to_string(definition)),
                },
            )
            .collect())
    }

    async fn list_users(&self) -> Result<Vec<UserInfo>> {
        let rows = sqlx::query_as::<_, (Vec<u8>, Vec<u8>)>(
            "-- name: ListUsers :many
             SELECT User, Host FROM mysql.user ORDER BY User, Host",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list users")?;

        let mut users = Vec::with_capacity(rows.len());
        for (name, host) in rows {
            let name = bytes_to_string(name);
            let host = bytes_to_string(host);
            let grants = self.show_grants(&name, &host).await.unwrap_or_default();
            users.push(UserInfo { name, host, grants });
        }
        Ok(users)
    }

    async fn truncate_table(&self, database: &str, table: &str) -> Result<()> {
        let sql = format!(
            "TRUNCATE TABLE `{}`.`{}`",
            database.replace('`', "``"),
            table.replace('`', "``")
        );
        sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .context("Failed to truncate table")?;
        Ok(())
    }

    async fn drop_table(&self, database: &str, table: &str) -> Result<()> {
        let sql = format!(
            "DROP TABLE `{}`.`{}`",
            database.replace('`', "``"),
            table.replace('`', "``")
        );
        sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .context("Failed to drop table")?;
        Ok(())
    }

    async fn rename_table(&self, database: &str, old_name: &str, new_name: &str) -> Result<()> {
        let sql = format!(
            "RENAME TABLE `{}`.`{}` TO `{}`.`{}`",
            database.replace('`', "``"),
            old_name.replace('`', "``"),
            database.replace('`', "``"),
            new_name.replace('`', "``")
        );
        sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .context("Failed to rename table")?;
        Ok(())
    }
}

/// Integration tests against a real MySQL server.
///
/// Set MYSQL_TEST_URL=mysql://user:password@host:port/dbname before running,
/// then use `cargo test -p db_client -- --include-ignored` to execute.
#[cfg(test)]
mod integration_tests {
    use super::MySqlProvider;
    use crate::provider::DbProvider;
    use crate::{ConnectionConfig, DatabaseDriver};
    use uuid::Uuid;

    fn test_config_from_env() -> Option<ConnectionConfig> {
        let url = std::env::var("MYSQL_TEST_URL").ok()?;
        // Parse mysql://user:password@host:port/database
        let url = url.strip_prefix("mysql://")?;
        let (userinfo, hostpart) = url.split_once('@')?;
        let (username, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let (hostport, database) = hostpart.split_once('/').unwrap_or((hostpart, ""));
        let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, "3306"));
        let port: u16 = port_str.parse().unwrap_or(3306);

        Some(ConnectionConfig {
            id: Uuid::new_v4(),
            label: "test".to_string(),
            driver: DatabaseDriver::MySQL,
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

    #[tokio::test]
    #[ignore]
    async fn test_ping() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        provider.ping().await.expect("Ping failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_databases() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let databases = provider
            .list_databases()
            .await
            .expect("Failed to list databases");
        assert!(!databases.is_empty(), "Expected at least one database");
        assert!(
            databases.iter().any(|db| db.name == "information_schema"),
            "information_schema should always be present"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_tables() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let tables = provider
            .list_tables("information_schema")
            .await
            .expect("Failed to list tables");
        assert!(!tables.is_empty(), "information_schema should have tables");
        assert!(
            tables.iter().any(|t| t.name == "TABLES"),
            "TABLES should exist in information_schema"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_describe_table() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let columns = provider
            .describe_table("information_schema", "TABLES")
            .await
            .expect("Failed to describe table");
        assert!(!columns.is_empty(), "TABLES should have columns");
        assert!(
            columns.iter().any(|c| c.name == "TABLE_NAME"),
            "TABLE_NAME column should exist"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_database_ddl() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let ddl = provider
            .get_database_ddl("information_schema")
            .await
            .expect("Failed to get database DDL");
        assert!(
            ddl.to_uppercase().contains("CREATE DATABASE"),
            "DDL should contain CREATE DATABASE, got: {ddl}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_execute_select_query() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let result = provider
            .execute_query(
                "information_schema",
                "SELECT 1 AS value, 'hello' AS greeting",
            )
            .await
            .expect("Failed to execute query");
        assert_eq!(result.columns, vec!["value", "greeting"]);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0].as_deref(), Some("1"));
        assert_eq!(result.rows[0][1].as_deref(), Some("hello"));
    }

    // Verifies the streaming decode bounds memory: an unbounded SELECT over a
    // very large table must stop at the hard row cap instead of pulling the
    // whole table, and every cell must respect the byte cap. This is the
    // regression guard for the "app freezes / OS offers to kill it on a big
    // query" report.
    #[tokio::test]
    #[ignore]
    async fn test_unbounded_select_is_bounded() {
        use crate::{MAX_CELL_BYTES, MAX_RESULT_ROWS};
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let result = provider
            .execute_query("instruments", "SELECT * FROM company_owners")
            .await
            .expect("Failed to execute query");

        assert!(
            result.rows.len() <= MAX_RESULT_ROWS,
            "unbounded SELECT must stop at the hard row cap, got {} rows",
            result.rows.len()
        );
        for row in &result.rows {
            for cell in row.iter().flatten() {
                assert!(
                    cell.len() <= MAX_CELL_BYTES + 4,
                    "cell exceeded the byte cap: {} bytes",
                    cell.len()
                );
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_users_populates_grants() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let users = provider.list_users().await.expect("Failed to list users");
        assert!(!users.is_empty(), "Expected at least one MySQL account");
        // A privileged test account should see grants for at least one user.
        assert!(
            users.iter().any(|user| !user.grants.is_empty()),
            "Expected SHOW GRANTS to populate grants for at least one account"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_execute_show_databases() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let result = provider
            .execute_query("", "SHOW DATABASES")
            .await
            .expect("Failed to execute SHOW DATABASES");
        assert!(!result.columns.is_empty());
        assert!(!result.rows.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn test_execute_show_create_table() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zed_db_client_test_{}", Uuid::new_v4().simple());
        let table = "show_create_smoke";
        let quoted_database = format!("`{}`", database.replace('`', "``"));
        let quoted_table = format!("`{}`", table.replace('`', "``"));

        provider
            .execute_query("", &format!("CREATE DATABASE {quoted_database}"))
            .await
            .expect("Failed to create temporary database");

        let test_result = async {
            provider
                .execute_query(
                    &database,
                    &format!(
                        "CREATE TABLE {quoted_table} (
                            id INT NOT NULL PRIMARY KEY,
                            name VARCHAR(64) NULL
                        )"
                    ),
                )
                .await?;
            provider
                .execute_query(&database, &format!("SHOW CREATE TABLE {quoted_table}"))
                .await
        }
        .await;

        provider
            .execute_query("", &format!("DROP DATABASE {quoted_database}"))
            .await
            .expect("Failed to clean up temporary database");

        let result = test_result.expect("Failed to execute SHOW CREATE TABLE");
        assert!(result.columns.len() >= 2);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0].as_deref(), Some(table));
        assert!(
            result.rows[0].iter().flatten().any(|cell| {
                cell.contains("CREATE TABLE") && cell.contains(&format!("`{table}`"))
            }),
            "SHOW CREATE TABLE should return a DDL cell"
        );
    }

    // Regression test for the "this functionality requires a Tokio context"
    // panic: connecting from a non-Tokio executor (as GPUI does) must work
    // through RuntimeProvider. A plain #[test] with futures::executor::block_on
    // means there is no ambient Tokio runtime, mirroring the real call site.
    #[test]
    #[ignore]
    fn test_connect_from_non_tokio_executor() {
        let Some(config) = test_config_from_env() else {
            panic!("MYSQL_TEST_URL env var required for integration tests");
        };
        futures::executor::block_on(async move {
            let raw = crate::on_runtime(async move { MySqlProvider::connect(&config).await })
                .await
                .expect("connect via runtime");
            let provider = crate::RuntimeProvider::new(std::sync::Arc::new(raw));
            provider.ping().await.expect("ping via runtime");
            let result = provider
                .execute_query("", "SELECT 1 AS one")
                .await
                .expect("query via runtime");
            assert!(!result.columns.is_empty());
        });
    }

    // Regression test: `is_read_query` must recognize a CTE (`WITH ...
    // SELECT`) as a read query, matching db_client::is_read_only_query and
    // the Postgres provider's identical check — otherwise the query falls
    // into the non-streaming `.execute()` path and the grid silently shows
    // zero columns and zero rows even though the query succeeded.
    #[tokio::test]
    #[ignore]
    async fn test_with_cte_returns_rows() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let result = provider
            .execute_query("", "WITH one AS (SELECT 1 AS n) SELECT n FROM one")
            .await
            .expect("Failed to execute query");

        assert_eq!(result.columns, vec!["n".to_string()]);
        assert_eq!(result.rows.len(), 1);
    }

    // Gates the "NULL decodes as 0" hypothesis from the grid UX audit for
    // MySQL specifically -- do not assume the fix SQLite needed applies here
    // without empirical proof, per that audit's own correction.
    #[tokio::test]
    #[ignore]
    async fn test_null_cells_decode_as_none() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let result = provider
            .execute_query("", "SELECT CAST(NULL AS CHAR) AS text_col, CAST(NULL AS SIGNED) AS int_col")
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
}
