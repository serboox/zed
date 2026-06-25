use anyhow::{Context as _, Result};
use async_trait::async_trait;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use sqlx::{Column as _, Row as _};
use std::time::Instant;

use crate::connection::ConnectionConfig;
use crate::provider::DbProvider;
use crate::schema::{ColumnInfo, DatabaseInfo, IndexInfo, ProcedureInfo, ProcedureKind, QueryResult, TableInfo, TableKind, TriggerInfo, UserInfo};

pub struct MySqlProvider {
    pool: MySqlPool,
}

impl MySqlProvider {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let url = format!(
            "mysql://{}:{}@{}:{}/{}",
            urlencoding_encode(&config.username),
            urlencoding_encode(&config.password),
            config.host,
            config.port,
            config.database.as_deref().unwrap_or(""),
        );
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .context("Failed to connect to MySQL")?;
        Ok(Self { pool })
    }
}

pub(crate) fn urlencoding_encode(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(byte as char);
            }
            other => {
                output.push('%');
                output.push(char::from_digit((other >> 4) as u32, 16).unwrap_or('0'));
                output.push(char::from_digit((other & 0xf) as u32, 16).unwrap_or('0'));
            }
        }
    }
    output
}

#[cfg(test)]
mod encoding_tests {
    use super::urlencoding_encode;

    #[test]
    fn test_urlencoding_alphanumeric_passthrough() {
        assert_eq!(urlencoding_encode("root"), "root");
        assert_eq!(urlencoding_encode("MyUser123"), "MyUser123");
        assert_eq!(urlencoding_encode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn test_urlencoding_special_chars() {
        assert_eq!(urlencoding_encode("p@ss!"), "p%40ss%21");
        assert_eq!(urlencoding_encode("pa ss"), "pa%20ss");
        assert_eq!(urlencoding_encode(""), "");
    }

    #[test]
    fn test_urlencoding_at_sign() {
        assert_eq!(urlencoding_encode("user@host"), "user%40host");
    }

    #[test]
    fn test_urlencoding_slash() {
        assert_eq!(urlencoding_encode("pass/word"), "pass%2fword");
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
        let rows = sqlx::query_as::<_, (String,)>("SHOW DATABASES")
            .fetch_all(&self.pool)
            .await
            .context("Failed to list databases")?;
        Ok(rows
            .into_iter()
            .map(|(name,)| DatabaseInfo { name })
            .collect())
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>> {
        let rows = sqlx::query_as::<_, (String, String)>(
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
                name,
                kind: if table_type == "VIEW" {
                    TableKind::View
                } else {
                    TableKind::Table
                },
            })
            .collect())
    }

    async fn describe_table(&self, database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let rows = sqlx::query_as::<_, (String, String, String, String, Option<String>, String)>(
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
            .map(|(name, data_type, nullable, key, default_value, extra)| ColumnInfo {
                name,
                data_type,
                is_nullable: nullable == "YES",
                column_key: if key.is_empty() { None } else { Some(key) },
                default_value,
                extra,
            })
            .collect())
    }

    async fn get_table_ddl(&self, database: &str, table: &str) -> Result<String> {
        let sql = format!("SHOW CREATE TABLE `{}`.`{}`", database.replace('`', "``"), table.replace('`', "``"));
        let row = sqlx::query(&sql)
            .fetch_one(&self.pool)
            .await
            .context("Failed to get table DDL")?;
        let ddl: String = row.try_get(1).context("Failed to read DDL from result")?;
        Ok(ddl)
    }

    async fn execute_query(&self, database: &str, sql: &str) -> Result<QueryResult> {
        if !database.is_empty() {
            let use_stmt = format!("USE `{}`", database.replace('`', "``"));
            sqlx::query(&use_stmt)
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
            || trimmed_upper.starts_with("DESC");

        if is_read_query {
            let rows = sqlx::query(sql)
                .fetch_all(&self.pool)
                .await
                .context("Query execution failed")?;

            let execution_time_ms = start.elapsed().as_millis() as u64;

            if rows.is_empty() {
                return Ok(QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: 0,
                    execution_time_ms,
                });
            }

            let columns: Vec<String> = rows[0]
                .columns()
                .iter()
                .map(|column| column.name().to_string())
                .collect();

            let result_rows: Vec<Vec<Option<String>>> = rows
                .iter()
                .map(|row| {
                    (0..columns.len())
                        .map(|index| {
                            row.try_get::<Option<String>, _>(index)
                                .ok()
                                .flatten()
                                .or_else(|| {
                                    row.try_get::<i64, _>(index).ok().map(|v| v.to_string())
                                })
                                .or_else(|| {
                                    row.try_get::<f64, _>(index).ok().map(|v| v.to_string())
                                })
                                .or_else(|| {
                                    row.try_get::<bool, _>(index).ok().map(|v| v.to_string())
                                })
                        })
                        .collect()
                })
                .collect();

            Ok(QueryResult {
                columns,
                rows: result_rows,
                rows_affected: rows.len() as u64,
                execution_time_ms,
            })
        } else {
            let result = sqlx::query(sql)
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
        let rows = sqlx::query_as::<_, (String, String, i64, String)>(
            "-- name: ListIndexes :many
             SELECT INDEX_NAME,
                    GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX SEPARATOR ','),
                    MAX(CAST(NON_UNIQUE = 0 AS UNSIGNED)),
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
                name,
                columns: cols_concat.split(',').map(|s| s.to_string()).collect(),
                unique: unique_flag == 1,
                index_type,
            })
            .collect())
    }

    async fn list_procedures(&self, database: &str) -> Result<Vec<ProcedureInfo>> {
        let rows = sqlx::query_as::<_, (String, String, Option<String>)>(
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
                name,
                kind: if routine_type == "FUNCTION" {
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
            .map(|(name, event, timing, table_name, definition)| TriggerInfo {
                name,
                event,
                timing,
                table_name,
                definition: Some(definition),
            })
            .collect())
    }

    async fn list_users(&self) -> Result<Vec<UserInfo>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "-- name: ListUsers :many
             SELECT User, Host FROM mysql.user ORDER BY User, Host",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list users")?;

        Ok(rows
            .into_iter()
            .map(|(name, host)| UserInfo {
                name,
                host,
                grants: Vec::new(),
            })
            .collect())
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
    use crate::{ConnectionConfig, DatabaseDriver};
    use crate::provider::DbProvider;
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
        let config = test_config_from_env()
            .expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        provider.ping().await.expect("Ping failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_databases() {
        let config = test_config_from_env()
            .expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let databases = provider.list_databases().await.expect("Failed to list databases");
        assert!(!databases.is_empty(), "Expected at least one database");
        assert!(
            databases.iter().any(|db| db.name == "information_schema"),
            "information_schema should always be present"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_tables() {
        let config = test_config_from_env()
            .expect("MYSQL_TEST_URL env var required for integration tests");
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
        let config = test_config_from_env()
            .expect("MYSQL_TEST_URL env var required for integration tests");
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
    async fn test_execute_select_query() {
        let config = test_config_from_env()
            .expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let result = provider
            .execute_query("information_schema", "SELECT 1 AS value, 'hello' AS greeting")
            .await
            .expect("Failed to execute query");
        assert_eq!(result.columns, vec!["value", "greeting"]);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0].as_deref(), Some("1"));
        assert_eq!(result.rows[0][1].as_deref(), Some("hello"));
    }

    #[tokio::test]
    #[ignore]
    async fn test_execute_show_databases() {
        let config = test_config_from_env()
            .expect("MYSQL_TEST_URL env var required for integration tests");
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
}
