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
}
