use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::TryStreamExt as _;
use sqlx::AssertSqlSafe;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode};
use sqlx::{Column as _, Row as _};
use std::time::Instant;

use crate::MAX_RESULT_ROWS;
use crate::connection::{ConnectionConfig, SslMode};
use crate::provider::DbProvider;
use crate::schema::{
    CheckConstraintInfo, ColumnInfo, DatabaseInfo, FkInfo, IndexInfo, ProcedureInfo, ProcedureKind,
    QueryResult, SequenceInfo, TableInfo, TableKind, TriggerInfo, UserInfo,
};

pub struct PostgresProvider {
    pool: PgPool,
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

impl PostgresProvider {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let opts = postgres_connect_options(config);
        // Single connection: `execute_query` relies on `SET search_path`
        // staying applied for the query that follows it, which only holds
        // when both run on the same physical connection. The metadata
        // queries are fully qualified, so serializing them through one
        // connection is acceptable for a single-user GUI client.
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .context("Failed to connect to PostgreSQL")?;
        Ok(Self { pool })
    }

    fn extract_cell(row: &sqlx::postgres::PgRow, index: usize) -> Option<String> {
        row.try_get::<Option<String>, _>(index)
            .ok()
            .flatten()
            .or_else(|| row.try_get::<i64, _>(index).ok().map(|v| v.to_string()))
            .or_else(|| row.try_get::<i32, _>(index).ok().map(|v| v.to_string()))
            .or_else(|| row.try_get::<i16, _>(index).ok().map(|v| v.to_string()))
            .or_else(|| row.try_get::<f64, _>(index).ok().map(|v| v.to_string()))
            .or_else(|| row.try_get::<f32, _>(index).ok().map(|v| v.to_string()))
            .or_else(|| row.try_get::<bool, _>(index).ok().map(|v| v.to_string()))
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
        let rows = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>)>(
            "-- name: DescribeTable :many
             SELECT column_name, data_type, is_nullable, column_default, ''
             FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = $2
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .bind(table)
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

    async fn get_table_ddl(&self, schema: &str, table: &str) -> Result<String> {
        let rows = sqlx::query_as::<_, (String, String, String, Option<String>)>(
            "-- name: GetTableDDL :many
             SELECT column_name, data_type, is_nullable, column_default
             FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = $2
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .context("Failed to get table structure for DDL")?;

        let mut ddl = format!("CREATE TABLE \"{}\".\"{}\" (\n", schema, table);
        let last = rows.len().saturating_sub(1);
        for (i, (col, dtype, nullable, default)) in rows.into_iter().enumerate() {
            let null_str = if nullable == "NO" { " NOT NULL" } else { "" };
            let default_str = if let Some(d) = default {
                format!(" DEFAULT {}", d)
            } else {
                String::new()
            };
            let comma = if i < last { "," } else { "" };
            ddl.push_str(&format!(
                "  \"{}\" {}{}{}{}\n",
                col, dtype, default_str, null_str, comma
            ));
        }
        ddl.push(')');
        Ok(ddl)
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

    async fn execute_query(&self, schema: &str, sql: &str) -> Result<QueryResult> {
        if !schema.is_empty() {
            let set_path = format!("SET search_path = \"{}\"", schema.replace('"', "\"\""));
            sqlx::query(AssertSqlSafe(set_path.as_str()))
                .execute(&self.pool)
                .await
                .context("Failed to set search_path")?;
        }

        let start = Instant::now();
        let trimmed_upper = sql.trim().to_uppercase();
        let is_read_query = trimmed_upper.starts_with("SELECT")
            || trimmed_upper.starts_with("SHOW")
            || trimmed_upper.starts_with("EXPLAIN")
            || trimmed_upper.starts_with("DESCRIBE")
            || trimmed_upper.starts_with("DESC")
            || trimmed_upper.starts_with("TABLE")
            || trimmed_upper.starts_with("WITH");
        let prefixed = format!(
            "{}{}",
            crate::application_name_comment(crate::DEFAULT_APPLICATION_NAME),
            sql
        );

        if is_read_query {
            let mut stream = sqlx::query(AssertSqlSafe(prefixed.as_str())).fetch(&self.pool);
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
                    .map(|index| Self::extract_cell(&row, index))
                    .collect();
                result_rows.push(decoded);

                if result_rows.len() >= MAX_RESULT_ROWS {
                    break;
                }
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
            let result = sqlx::query(AssertSqlSafe(prefixed.as_str()))
                .execute(&self.pool)
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

    async fn execute_query_streaming(
        &self,
        schema: &str,
        sql: &str,
        sink: &mut dyn crate::provider::RowSink,
    ) -> Result<u64> {
        if !schema.is_empty() {
            let set_path = format!("SET search_path = \"{}\"", schema.replace('"', "\"\""));
            sqlx::query(AssertSqlSafe(set_path.as_str()))
                .execute(&self.pool)
                .await
                .context("Failed to set search_path")?;
        }

        let trimmed_upper = sql.trim().to_uppercase();
        let is_read_query = trimmed_upper.starts_with("SELECT")
            || trimmed_upper.starts_with("SHOW")
            || trimmed_upper.starts_with("EXPLAIN")
            || trimmed_upper.starts_with("DESCRIBE")
            || trimmed_upper.starts_with("DESC")
            || trimmed_upper.starts_with("TABLE")
            || trimmed_upper.starts_with("WITH");
        let prefixed = format!(
            "{}{}",
            crate::application_name_comment(crate::DEFAULT_APPLICATION_NAME),
            sql
        );

        if !is_read_query {
            sqlx::query(AssertSqlSafe(prefixed.as_str()))
                .execute(&self.pool)
                .await
                .context("Query execution failed")?;
            return Ok(0);
        }

        // Unlike `execute_query`, this never breaks at `MAX_RESULT_ROWS` — the
        // whole point of "execute to file" is exporting result sets too large
        // for the grid. Cells are still capped for safety against a single
        // multi-megabyte value, but the row count itself is unbounded.
        let mut stream = sqlx::query(AssertSqlSafe(prefixed.as_str())).fetch(&self.pool);
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
                .map(|index| Self::extract_cell(&row, index))
                .collect();
            sink.write_row(&decoded)?;
            row_count += 1;
        }

        if columns.is_empty() {
            sink.write_columns(&[])?;
        }
        Ok(row_count)
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
        sqlx::query(AssertSqlSafe(rename_table_sql(database, old_name, new_name)))
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
