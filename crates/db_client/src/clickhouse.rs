use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::time::Instant;

use crate::connection::ConnectionConfig;
use crate::provider::DbProvider;
use crate::schema::{ColumnInfo, DatabaseInfo, QueryResult, TableInfo, TableKind};
use crate::MAX_RESULT_ROWS;

pub struct ClickHouseProvider {
    client: reqwest::Client,
    base_url: String,
    username: String,
    password: String,
}

impl ClickHouseProvider {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let base_url = format!("http://{}:{}", config.host, config.port);
        let client = reqwest::Client::builder()
            .build()
            .context("Failed to build HTTP client")?;

        let provider = Self {
            client,
            base_url,
            username: config.username.clone(),
            password: config.password.clone(),
        };

        provider
            .ping()
            .await
            .context("Failed to connect to ClickHouse")?;
        Ok(provider)
    }

    async fn query_json(
        &self,
        sql: &str,
        database: Option<&str>,
    ) -> Result<ClickHouseJsonResponse> {
        let url = if let Some(db) = database {
            format!(
                "{}/?database={}&default_format=JSONCompact",
                self.base_url,
                urlencoding::encode(db)
            )
        } else {
            format!("{}/?default_format=JSONCompact", self.base_url)
        };

        let full_sql = format!("{} FORMAT JSONCompact", sql.trim_end_matches(';'));
        let response = self
            .client
            .post(&url)
            .header("X-ClickHouse-User", &self.username)
            .header("X-ClickHouse-Key", &self.password)
            .body(full_sql)
            .send()
            .await
            .context("HTTP request to ClickHouse failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("ClickHouse error ({}): {}", status, body);
        }

        let body = response
            .text()
            .await
            .context("Failed to read ClickHouse response body")?;
        let parsed: ClickHouseJsonResponse =
            serde_json::from_str(&body).context("Failed to parse ClickHouse JSON response")?;
        Ok(parsed)
    }

    async fn execute_dml(&self, sql: &str, database: Option<&str>) -> Result<u64> {
        let url = if let Some(db) = database {
            format!("{}/?database={}", self.base_url, urlencoding::encode(db))
        } else {
            self.base_url.clone()
        };

        let response = self
            .client
            .post(&url)
            .header("X-ClickHouse-User", &self.username)
            .header("X-ClickHouse-Key", &self.password)
            .body(sql.to_string())
            .send()
            .await
            .context("HTTP request to ClickHouse failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("ClickHouse error ({}): {}", status, body);
        }

        let _body = response.text().await.unwrap_or_default();
        Ok(0)
    }
}

#[derive(Deserialize)]
struct ClickHouseJsonResponse {
    meta: Vec<ClickHouseColumnMeta>,
    data: Vec<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct ClickHouseColumnMeta {
    name: String,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    data_type: String,
}

fn is_read_query(sql: &str) -> bool {
    let upper = sql.trim().to_uppercase();
    upper.starts_with("SELECT")
        || upper.starts_with("SHOW")
        || upper.starts_with("DESCRIBE")
        || upper.starts_with("EXPLAIN")
        || upper.starts_with("DESC")
        || upper.starts_with("EXISTS")
        || upper.starts_with("WITH")
}

mod urlencoding {
    pub fn encode(input: &str) -> String {
        let mut output = String::with_capacity(input.len() * 3);
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
}

#[cfg(test)]
mod tests {
    use super::urlencoding;

    #[test]
    fn test_urlencoding_safe_chars() {
        assert_eq!(urlencoding::encode("my_db"), "my_db");
        assert_eq!(urlencoding::encode("abc-123"), "abc-123");
    }

    #[test]
    fn test_urlencoding_special_chars() {
        assert_eq!(urlencoding::encode("my db"), "my%20db");
        assert_eq!(urlencoding::encode("db/name"), "db%2fname");
    }
}

#[async_trait]
impl DbProvider for ClickHouseProvider {
    async fn ping(&self) -> Result<()> {
        let url = format!("{}/?query=SELECT+1", self.base_url);
        let response = self
            .client
            .get(&url)
            .header("X-ClickHouse-User", &self.username)
            .header("X-ClickHouse-Key", &self.password)
            .send()
            .await
            .context("Ping request failed")?;

        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Ping failed ({}): {}", status, body)
        }
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
        let resp = self
            .query_json("SELECT name FROM system.databases WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') ORDER BY name", None)
            .await?;
        Ok(resp
            .data
            .into_iter()
            .filter_map(|row| {
                let name = row.into_iter().next()?.as_str()?.to_string();
                Some(DatabaseInfo { name })
            })
            .collect())
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>> {
        let sql = format!(
            "SELECT name, engine FROM system.tables WHERE database = '{}' ORDER BY name",
            database.replace('\'', "\\'")
        );
        let resp = self.query_json(&sql, Some(database)).await?;
        Ok(resp
            .data
            .into_iter()
            .filter_map(|row| {
                let mut iter = row.into_iter();
                let name = iter.next()?.as_str()?.to_string();
                let engine = iter.next()?.as_str().unwrap_or("").to_string();
                let kind = if engine.contains("View") {
                    TableKind::View
                } else {
                    TableKind::Table
                };
                Some(TableInfo { name, kind })
            })
            .collect())
    }

    async fn list_views(&self, database: &str) -> Result<Vec<String>> {
        let sql = format!(
            "SELECT name FROM system.tables WHERE database = '{}' AND engine LIKE '%View%' ORDER BY name",
            database.replace('\'', "\\'")
        );
        let resp = self.query_json(&sql, Some(database)).await?;
        Ok(resp
            .data
            .into_iter()
            .filter_map(|row| row.into_iter().next()?.as_str().map(str::to_string))
            .collect())
    }

    async fn describe_table(&self, database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let sql = format!(
            "SELECT name, type, is_in_primary_key FROM system.columns WHERE database = '{}' AND table = '{}' ORDER BY position",
            database.replace('\'', "\\'"),
            table.replace('\'', "\\'"),
        );
        let resp = self.query_json(&sql, Some(database)).await?;
        Ok(resp
            .data
            .into_iter()
            .filter_map(|row| {
                let mut iter = row.into_iter();
                let name = iter.next()?.as_str()?.to_string();
                let data_type = iter.next()?.as_str()?.to_string();
                let is_pk = iter.next().and_then(|v| v.as_u64()).unwrap_or(0) == 1;
                Some(ColumnInfo {
                    name,
                    data_type,
                    is_nullable: false,
                    column_key: if is_pk { Some("PRI".to_string()) } else { None },
                    default_value: None,
                    extra: String::new(),
                })
            })
            .collect())
    }

    async fn get_table_ddl(&self, database: &str, table: &str) -> Result<String> {
        let sql = format!(
            "SHOW CREATE TABLE `{}`.`{}`",
            database.replace('`', "\\`"),
            table.replace('`', "\\`"),
        );
        let url = format!(
            "{}/?database={}&default_format=TabSeparatedRaw",
            self.base_url,
            urlencoding::encode(database)
        );
        let response = self
            .client
            .post(&url)
            .header("X-ClickHouse-User", &self.username)
            .header("X-ClickHouse-Key", &self.password)
            .body(sql)
            .send()
            .await
            .context("Failed to get table DDL")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("ClickHouse error ({}): {}", status, body);
        }

        let ddl = response
            .text()
            .await
            .context("Failed to read DDL response")?;
        Ok(ddl.trim().to_string())
    }

    async fn get_database_ddl(&self, database: &str) -> Result<String> {
        let sql = format!("SHOW CREATE DATABASE `{}`", database.replace('`', "\\`"));
        let url = format!(
            "{}/?database={}&default_format=TabSeparatedRaw",
            self.base_url,
            urlencoding::encode(database)
        );
        let response = self
            .client
            .post(&url)
            .header("X-ClickHouse-User", &self.username)
            .header("X-ClickHouse-Key", &self.password)
            .body(sql)
            .send()
            .await
            .context("Failed to get database DDL")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("ClickHouse error ({}): {}", status, body);
        }

        let ddl = response
            .text()
            .await
            .context("Failed to read database DDL response")?;
        Ok(ddl.trim().to_string())
    }

    async fn execute_query(&self, database: &str, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let db_opt = if database.is_empty() {
            None
        } else {
            Some(database)
        };
        let prefixed = format!(
            "{}{}",
            crate::application_name_comment(crate::DEFAULT_APPLICATION_NAME),
            sql
        );

        if is_read_query(sql) {
            let resp = self.query_json(&prefixed, db_opt).await?;
            let execution_time_ms = start.elapsed().as_millis() as u64;
            let columns: Vec<String> = resp.meta.into_iter().map(|m| m.name).collect();
            let rows: Vec<Vec<Option<String>>> = resp
                .data
                .into_iter()
                .take(MAX_RESULT_ROWS)
                .map(|row| {
                    row.into_iter()
                        .map(|val| match val {
                            serde_json::Value::Null => None,
                            serde_json::Value::String(s) => Some(s),
                            other => Some(other.to_string()),
                        })
                        .collect()
                })
                .collect();
            let rows_affected = rows.len() as u64;
            Ok(QueryResult {
                columns,
                rows,
                rows_affected,
                execution_time_ms,
            })
        } else {
            let rows_affected = self.execute_dml(&prefixed, db_opt).await?;
            Ok(QueryResult {
                columns: vec![],
                rows: vec![],
                rows_affected,
                execution_time_ms: start.elapsed().as_millis() as u64,
            })
        }
    }

    // The default `DbProvider::rename_table` generates `ALTER TABLE ...
    // RENAME TO ...`, which ClickHouse rejects (`ALTER TABLE` only renames
    // columns there); the table-level statement is the standalone `RENAME
    // TABLE ... TO ...` used here instead.
    async fn rename_table(&self, database: &str, old_name: &str, new_name: &str) -> Result<()> {
        let sql = format!(
            "RENAME TABLE `{}`.`{}` TO `{}`.`{}`",
            database.replace('`', "\\`"),
            old_name.replace('`', "\\`"),
            database.replace('`', "\\`"),
            new_name.replace('`', "\\`"),
        );
        self.execute_dml(&sql, Some(database)).await?;
        Ok(())
    }
}

/// Integration tests against a real ClickHouse server.
///
/// Set CLICKHOUSE_TEST_URL=clickhouse://host:port before running, then use
/// `cargo test -p db_client -- --include-ignored` to execute.
#[cfg(test)]
mod integration_tests {
    use super::ClickHouseProvider;
    use crate::provider::DbProvider;
    use crate::schema::TableKind;
    use crate::{ConnectionConfig, DatabaseDriver};
    use uuid::Uuid;

    fn test_config_from_env() -> Option<ConnectionConfig> {
        let url = std::env::var("CLICKHOUSE_TEST_URL").ok()?;
        let url = url.strip_prefix("clickhouse://")?;
        let (host, port_str) = url.split_once(':').unwrap_or((url, "8123"));
        let port: u16 = port_str.parse().unwrap_or(8123);

        Some(ConnectionConfig {
            id: Uuid::new_v4(),
            label: "test".to_string(),
            driver: DatabaseDriver::ClickHouse,
            host: host.to_string(),
            port,
            username: "default".to_string(),
            password: String::new(),
            database: None,
            auto_connect: false,
            ..ConnectionConfig::default()
        })
    }

    #[tokio::test]
    #[ignore]
    async fn test_ping() {
        let config = test_config_from_env()
            .expect("CLICKHOUSE_TEST_URL env var required for integration tests");
        let provider = ClickHouseProvider::connect(&config)
            .await
            .expect("Failed to connect");
        provider.ping().await.expect("Ping failed");
    }

    /// Runs `body` against a fresh scratch database, dropping it afterward
    /// regardless of the outcome.
    async fn with_scratch_database<'p, F, Fut, T>(provider: &'p ClickHouseProvider, body: F) -> T
    where
        F: FnOnce(&'p ClickHouseProvider, String) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let database = format!("zdbt_{}", Uuid::new_v4().simple());
        provider
            .execute_query("", &format!("CREATE DATABASE {database}"))
            .await
            .expect("Failed to create scratch database");

        let result = body(provider, database.clone()).await;

        provider
            .execute_query("", &format!("DROP DATABASE {database}"))
            .await
            .expect("Failed to clean up scratch database");

        result
    }

    #[tokio::test]
    #[ignore]
    async fn test_create_and_drop_database() {
        let config = test_config_from_env()
            .expect("CLICKHOUSE_TEST_URL env var required for integration tests");
        let provider = ClickHouseProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            let databases = provider
                .list_databases()
                .await
                .expect("Failed to list databases");
            assert!(databases.iter().any(|d| d.name == database));
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_create_table_insert_and_select() {
        let config = test_config_from_env()
            .expect("CLICKHOUSE_TEST_URL env var required for integration tests");
        let provider = ClickHouseProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE widgets (id UInt32, name String) ENGINE = MergeTree ORDER BY id",
                )
                .await
                .expect("Failed to create table");

            let tables = provider
                .list_tables(&database)
                .await
                .expect("Failed to list tables");
            assert!(tables.iter().any(|t| t.name == "widgets"));

            provider
                .execute_query(
                    &database,
                    "INSERT INTO widgets (id, name) VALUES (1, 'bolt')",
                )
                .await
                .expect("Failed to insert row");
            let result = provider
                .execute_query(&database, "SELECT name FROM widgets WHERE id = 1")
                .await
                .expect("Failed to select");
            assert_eq!(result.rows[0][0].as_deref(), Some("bolt"));
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_alter_table_add_column() {
        let config = test_config_from_env()
            .expect("CLICKHOUSE_TEST_URL env var required for integration tests");
        let provider = ClickHouseProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE widgets (id UInt32) ENGINE = MergeTree ORDER BY id",
                )
                .await
                .expect("Failed to create table");
            provider
                .execute_query(&database, "ALTER TABLE widgets ADD COLUMN weight UInt32")
                .await
                .expect("Failed to alter table");

            let columns = provider
                .describe_table(&database, "widgets")
                .await
                .expect("Failed to describe table");
            assert!(columns.iter().any(|c| c.name == "weight"));
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_create_and_query_a_view() {
        let config = test_config_from_env()
            .expect("CLICKHOUSE_TEST_URL env var required for integration tests");
        let provider = ClickHouseProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE items (id UInt32, price UInt32) ENGINE = MergeTree ORDER BY id",
                )
                .await
                .expect("Failed to create table");
            provider
                .execute_query(
                    &database,
                    "INSERT INTO items (id, price) VALUES (1, 150), (2, 50)",
                )
                .await
                .expect("Failed to insert rows");
            provider
                .execute_query(
                    &database,
                    "CREATE VIEW pricey_items AS SELECT * FROM items WHERE price > 100",
                )
                .await
                .expect("Failed to create view");

            let result = provider
                .execute_query(&database, "SELECT id FROM pricey_items")
                .await
                .expect("Failed to query the view");
            assert_eq!(result.rows.len(), 1);
            assert_eq!(result.rows[0][0].as_deref(), Some("1"));

            provider
                .execute_query(&database, "DROP VIEW pricey_items")
                .await
                .expect("Failed to drop view");
        })
        .await;
    }

    /// ClickHouse's `UPDATE`/`DELETE` are asynchronous mutations by default;
    /// `mutations_sync = 1` forces them to apply before the statement
    /// returns, so the very next `SELECT` sees the result deterministically
    /// instead of racing a background mutation.
    #[tokio::test]
    #[ignore]
    async fn test_update_and_delete_via_mutations() {
        let config = test_config_from_env()
            .expect("CLICKHOUSE_TEST_URL env var required for integration tests");
        let provider = ClickHouseProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE accounts (id UInt32, balance UInt32) ENGINE = MergeTree ORDER BY id",
                )
                .await
                .expect("Failed to create table");
            provider
                .execute_query(&database, "INSERT INTO accounts (id, balance) VALUES (1, 100)")
                .await
                .expect("Failed to insert row");

            provider
                .execute_query(
                    &database,
                    "ALTER TABLE accounts UPDATE balance = 250 WHERE id = 1 SETTINGS mutations_sync = 1",
                )
                .await
                .expect("Failed to update row");
            let after_update = provider
                .execute_query(&database, "SELECT balance FROM accounts WHERE id = 1")
                .await
                .expect("Failed to select after update");
            assert_eq!(after_update.rows[0][0].as_deref(), Some("250"));

            provider
                .execute_query(
                    &database,
                    "ALTER TABLE accounts DELETE WHERE id = 1 SETTINGS mutations_sync = 1",
                )
                .await
                .expect("Failed to delete row");
            let after_delete = provider
                .execute_query(&database, "SELECT balance FROM accounts WHERE id = 1")
                .await
                .expect("Failed to select after delete");
            assert!(after_delete.rows.is_empty(), "row should be gone after DELETE");
        })
        .await;
    }

    /// ClickHouse has no native `INSERT ... ON CONFLICT`; the idiomatic
    /// upsert substitute is `ReplacingMergeTree` plus `SELECT ... FINAL`,
    /// which collapses same-key rows down to the one with the highest
    /// version. Proves that collapsing behavior end to end.
    #[tokio::test]
    #[ignore]
    async fn test_upsert_via_replacing_merge_tree() {
        let config = test_config_from_env()
            .expect("CLICKHOUSE_TEST_URL env var required for integration tests");
        let provider = ClickHouseProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE counters (name String, hits UInt32, version UInt32) \
                     ENGINE = ReplacingMergeTree(version) ORDER BY name",
                )
                .await
                .expect("Failed to create table");

            provider
                .execute_query(
                    &database,
                    "INSERT INTO counters (name, hits, version) VALUES ('clicks', 1, 1)",
                )
                .await
                .expect("Failed first insert (insert path)");
            provider
                .execute_query(
                    &database,
                    "INSERT INTO counters (name, hits, version) VALUES ('clicks', 2, 2)",
                )
                .await
                .expect("Failed second insert (upsert path)");

            let result = provider
                .execute_query(
                    &database,
                    "SELECT hits FROM counters FINAL WHERE name = 'clicks'",
                )
                .await
                .expect("Failed to select with FINAL");
            assert_eq!(
                result.rows.len(),
                1,
                "FINAL must collapse the two versions down to a single row"
            );
            assert_eq!(
                result.rows[0][0].as_deref(),
                Some("2"),
                "the higher version must win"
            );
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_views_finds_a_created_view() {
        let config = test_config_from_env()
            .expect("CLICKHOUSE_TEST_URL env var required for integration tests");
        let provider = ClickHouseProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE source_items (id UInt32, price UInt32) ENGINE = MergeTree ORDER BY id",
                )
                .await
                .expect("Failed to create source table");
            provider
                .execute_query(
                    &database,
                    "CREATE VIEW pricey_items AS SELECT * FROM source_items WHERE price > 100",
                )
                .await
                .expect("Failed to create view");

            let views = provider
                .list_views(&database)
                .await
                .expect("Failed to list views");
            assert_eq!(views, vec!["pricey_items".to_string()]);
            let tables = provider
                .list_tables(&database)
                .await
                .expect("Failed to list tables");
            let pricey_items = tables
                .iter()
                .find(|t| t.name == "pricey_items")
                .expect("expected pricey_items among list_tables results");
            assert!(
                matches!(pricey_items.kind, TableKind::View),
                "the view must be classified as TableKind::View in list_tables too"
            );
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_truncate_table_removes_rows_but_keeps_the_table() {
        let config = test_config_from_env()
            .expect("CLICKHOUSE_TEST_URL env var required for integration tests");
        let provider = ClickHouseProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE crumbs (id UInt32) ENGINE = MergeTree ORDER BY id",
                )
                .await
                .expect("Failed to create table");
            provider
                .execute_query(&database, "INSERT INTO crumbs (id) VALUES (1), (2), (3)")
                .await
                .expect("Failed to insert rows");

            provider
                .truncate_table(&database, "crumbs")
                .await
                .expect("Failed to truncate table");

            let tables = provider
                .list_tables(&database)
                .await
                .expect("Failed to list tables");
            assert!(
                tables.iter().any(|t| t.name == "crumbs"),
                "truncate must not drop the table itself"
            );
            let remaining = provider
                .execute_query(&database, "SELECT id FROM crumbs")
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
            .expect("CLICKHOUSE_TEST_URL env var required for integration tests");
        let provider = ClickHouseProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE old_name (id UInt32) ENGINE = MergeTree ORDER BY id",
                )
                .await
                .expect("Failed to create table");

            provider
                .rename_table(&database, "old_name", "new_name")
                .await
                .expect("Failed to rename table");

            let tables = provider
                .list_tables(&database)
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
}
