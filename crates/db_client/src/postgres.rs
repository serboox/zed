use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::TryStreamExt as _;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{Column as _, Row as _};
use std::time::Instant;

use crate::connection::ConnectionConfig;
use crate::{MAX_RESULT_ROWS, cap_cell};
use crate::provider::DbProvider;
use crate::schema::{
    ColumnInfo, DatabaseInfo, IndexInfo, ProcedureInfo, ProcedureKind, QueryResult, TableInfo,
    TableKind, TriggerInfo, UserInfo,
};

pub struct PostgresProvider {
    pool: PgPool,
}

impl PostgresProvider {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let opts = PgConnectOptions::new()
            .host(&config.host)
            .port(config.port)
            .username(&config.username)
            .password(&config.password)
            .database(config.database.as_deref().unwrap_or("postgres"));
        let pool = PgPoolOptions::new()
            .max_connections(5)
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

    async fn execute_query(&self, schema: &str, sql: &str) -> Result<QueryResult> {
        if !schema.is_empty() {
            let set_path = format!("SET search_path = \"{}\"", schema.replace('"', "\"\""));
            sqlx::query(&set_path)
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
            let mut stream = sqlx::query(&prefixed).fetch(&self.pool);
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
                    .map(|index| Self::extract_cell(&row, index).map(cap_cell))
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
        sqlx::query(&sql)
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
        sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .context("Failed to drop table")?;
        Ok(())
    }

    async fn rename_table(&self, database: &str, old_name: &str, new_name: &str) -> Result<()> {
        let sql = format!(
            "-- name: RenameTable :exec\nALTER TABLE \"{}\".\"{}\" RENAME TO \"{}\"",
            database.replace('"', "\"\""),
            old_name.replace('"', "\"\""),
            new_name.replace('"', "\"\""),
        );
        sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .context("Failed to rename table")?;
        Ok(())
    }
}
