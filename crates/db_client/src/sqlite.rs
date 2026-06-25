use anyhow::{Context as _, Result};
use async_trait::async_trait;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::{Column as _, Row as _};
use std::path::Path;
use std::time::Instant;

use crate::connection::ConnectionConfig;
use crate::provider::DbProvider;
use crate::schema::{ColumnInfo, DatabaseInfo, IndexInfo, QueryResult, TableInfo, TableKind, TriggerInfo};

pub struct SqliteProvider {
    pool: SqlitePool,
    db_name: String,
}

impl SqliteProvider {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let path = &config.host;
        let url = format!("sqlite://{path}");
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .context("Failed to open SQLite database")?;
        let db_name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path)
            .to_string();
        Ok(Self { pool, db_name })
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
        let rows = sqlx::query(&sql)
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
        let index_rows = sqlx::query(&list_sql)
            .fetch_all(&self.pool)
            .await
            .context("Failed to list indexes")?;

        let mut indexes = Vec::new();
        for row in index_rows {
            let name: String = row.try_get("name").context("Missing index name")?;
            let unique_val: i64 = row.try_get("unique").unwrap_or(0);
            let safe_name = name.replace('\'', "''");
            let info_sql = format!("PRAGMA index_info('{safe_name}')");
            let col_rows = sqlx::query(&info_sql)
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
        sqlx::query(&format!("DELETE FROM \"{safe}\""))
            .execute(&self.pool)
            .await
            .context("Failed to truncate table")?;
        Ok(())
    }

    async fn drop_table(&self, _database: &str, table: &str) -> Result<()> {
        let safe = table.replace('"', "\"\"");
        sqlx::query(&format!("DROP TABLE \"{safe}\""))
            .execute(&self.pool)
            .await
            .context("Failed to drop table")?;
        Ok(())
    }

    async fn rename_table(&self, _database: &str, old_name: &str, new_name: &str) -> Result<()> {
        let safe_old = old_name.replace('"', "\"\"");
        let safe_new = new_name.replace('"', "\"\"");
        sqlx::query(&format!(
            "ALTER TABLE \"{safe_old}\" RENAME TO \"{safe_new}\""
        ))
        .execute(&self.pool)
        .await
        .context("Failed to rename table")?;
        Ok(())
    }

    async fn execute_query(&self, _database: &str, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let trimmed_upper = sql.trim().to_uppercase();
        let is_read_query = trimmed_upper.starts_with("SELECT")
            || trimmed_upper.starts_with("EXPLAIN")
            || trimmed_upper.starts_with("PRAGMA")
            || trimmed_upper.starts_with("WITH");

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
                .map(|col| col.name().to_string())
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
                                    row.try_get::<i32, _>(index).ok().map(|v| v.to_string())
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
}
