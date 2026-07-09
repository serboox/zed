use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::TryStreamExt as _;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::{Column as _, Row as _, ValueRef as _};
use std::path::Path;
use std::time::Instant;

use crate::connection::ConnectionConfig;
use crate::provider::DbProvider;
use crate::schema::{
    CheckConstraintInfo, ColumnInfo, DatabaseInfo, IndexInfo, QueryResult, TableInfo, TableKind,
    TriggerInfo,
};
use crate::{MAX_RESULT_ROWS, cap_cell};

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
        sqlx::query(&rename_table_sql(old_name, new_name))
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
            let mut stream = sqlx::query(sql).fetch(&self.pool);
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
        let provider = SqliteProvider {
            pool,
            db_name: "test".to_string(),
        };

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
}
