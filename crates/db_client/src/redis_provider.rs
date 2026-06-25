use anyhow::{Context as _, Result};
use async_trait::async_trait;
use redis::AsyncCommands;
use std::time::Instant;

use crate::connection::ConnectionConfig;
use crate::provider::DbProvider;
use crate::schema::{ColumnInfo, DatabaseInfo, QueryResult, TableInfo, TableKind};

pub struct RedisProvider {
    client: redis::Client,
    db_index: i64,
}

impl RedisProvider {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let url = build_redis_url(config);
        let client = redis::Client::open(url).context("Failed to create Redis client")?;

        let db_index: i64 = config
            .database
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let provider = Self { client, db_index };
        provider.ping().await.context("Failed to connect to Redis")?;
        Ok(provider)
    }

    async fn connection(&self) -> Result<redis::aio::MultiplexedConnection> {
        let mut conn = self.client
            .get_multiplexed_async_connection()
            .await
            .context("Failed to get Redis connection")?;
        if self.db_index != 0 {
            redis::cmd("SELECT")
                .arg(self.db_index)
                .exec_async(&mut conn)
                .await
                .context("Failed to SELECT Redis database")?;
        }
        Ok(conn)
    }
}

fn build_redis_url(config: &ConnectionConfig) -> String {
    if config.password.is_empty() {
        format!("redis://{}:{}", config.host, config.port)
    } else {
        format!(
            "redis://:{}@{}:{}",
            urlencoding_encode(&config.password),
            config.host,
            config.port,
        )
    }
}

fn urlencoding_encode(input: &str) -> String {
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

fn redis_value_to_string(value: &redis::Value) -> Option<String> {
    match value {
        redis::Value::BulkString(bytes) => String::from_utf8(bytes.clone()).ok(),
        redis::Value::SimpleString(s) => Some(s.clone()),
        redis::Value::Int(n) => Some(n.to_string()),
        redis::Value::Nil => None,
        redis::Value::Boolean(b) => Some(b.to_string()),
        redis::Value::Double(d) => Some(d.to_string()),
        redis::Value::BigNumber(n) => Some(n.to_string()),
        redis::Value::VerbatimString { text, .. } => Some(text.clone()),
        redis::Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().filter_map(redis_value_to_string).collect();
            Some(parts.join(", "))
        }
        redis::Value::Map(pairs) => {
            let parts: Vec<String> = pairs.iter().map(|(k, v)| {
                let key = redis_value_to_string(k).unwrap_or_default();
                let val = redis_value_to_string(v).unwrap_or_default();
                format!("{}: {}", key, val)
            }).collect();
            Some(parts.join(", "))
        }
        _ => Some(format!("{:?}", value)),
    }
}

fn is_read_command(cmd: &str) -> bool {
    matches!(cmd.to_uppercase().as_str(),
        "GET" | "MGET" | "GETRANGE" | "STRLEN" | "HGET" | "HMGET" | "HGETALL" |
        "HKEYS" | "HVALS" | "HLEN" | "HEXISTS" | "LRANGE" | "LLEN" | "LINDEX" |
        "SMEMBERS" | "SCARD" | "SISMEMBER" | "SUNION" | "SINTER" | "SDIFF" |
        "ZRANGE" | "ZRANGEBYSCORE" | "ZRANK" | "ZCARD" | "ZSCORE" | "ZCOUNT" |
        "KEYS" | "SCAN" | "TYPE" | "TTL" | "PTTL" | "EXISTS" | "OBJECT" |
        "DEBUG" | "INFO" | "CONFIG" | "DBSIZE" | "TIME" | "PING" | "ECHO" |
        "RANDOMKEY" | "DUMP" | "PERSIST" | "EXPIRETIME" | "PEXPIRETIME"
    )
}

#[async_trait]
impl DbProvider for RedisProvider {
    async fn ping(&self) -> Result<()> {
        let mut conn = self.connection().await?;
        let _: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .context("PING failed")?;
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
        let mut conn = self.connection().await?;
        let info: String = redis::cmd("INFO")
            .arg("keyspace")
            .query_async(&mut conn)
            .await
            .context("INFO keyspace failed")?;

        let mut databases: Vec<DatabaseInfo> = (0..16)
            .map(|i| DatabaseInfo { name: format!("db{}", i) })
            .collect();

        for line in info.lines() {
            if let Some(rest) = line.strip_prefix("db") {
                if let Some(colon) = rest.find(':') {
                    let idx_str = &rest[..colon];
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        if idx < databases.len() {
                            let keys: u64 = rest[colon + 1..]
                                .split(',')
                                .find_map(|part| {
                                    part.trim().strip_prefix("keys=").and_then(|k| k.parse().ok())
                                })
                                .unwrap_or(0);
                            databases[idx].name = format!("db{} ({} keys)", idx, keys);
                        }
                    }
                }
            }
        }

        Ok(databases)
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>> {
        let mut conn = self.connection().await?;

        let db_index: i64 = database
            .trim_start_matches("db")
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(self.db_index);

        redis::cmd("SELECT")
            .arg(db_index)
            .exec_async(&mut conn)
            .await
            .context("SELECT database failed")?;

        let keys: Vec<String> = redis::cmd("KEYS")
            .arg("*")
            .query_async(&mut conn)
            .await
            .context("KEYS * failed")?;

        let mut tables = Vec::with_capacity(keys.len());
        for key in keys {
            let key_type: String = conn.key_type(&key).await.unwrap_or_else(|_| "string".to_string());
            let kind = if key_type == "hash" { TableKind::Table } else { TableKind::View };
            tables.push(TableInfo { name: key, kind });
        }

        tables.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tables)
    }

    async fn describe_table(&self, database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let mut conn = self.connection().await?;

        let db_index: i64 = database
            .trim_start_matches("db")
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(self.db_index);

        redis::cmd("SELECT")
            .arg(db_index)
            .exec_async(&mut conn)
            .await
            .context("SELECT database failed")?;

        let key_type: String = conn.key_type(table).await.context("TYPE failed")?;

        match key_type.as_str() {
            "hash" => {
                let fields: Vec<String> = conn.hkeys(table).await.context("HKEYS failed")?;
                Ok(fields.into_iter().map(|name| ColumnInfo {
                    name,
                    data_type: "string".to_string(),
                    is_nullable: true,
                    column_key: None,
                    default_value: None,
                    extra: String::new(),
                }).collect())
            }
            "list" => Ok(vec![ColumnInfo {
                name: "element".to_string(),
                data_type: "string".to_string(),
                is_nullable: false,
                column_key: None,
                default_value: None,
                extra: "list".to_string(),
            }]),
            "set" => Ok(vec![ColumnInfo {
                name: "member".to_string(),
                data_type: "string".to_string(),
                is_nullable: false,
                column_key: None,
                default_value: None,
                extra: "set".to_string(),
            }]),
            "zset" => Ok(vec![
                ColumnInfo { name: "member".to_string(), data_type: "string".to_string(), is_nullable: false, column_key: None, default_value: None, extra: "zset".to_string() },
                ColumnInfo { name: "score".to_string(), data_type: "float".to_string(), is_nullable: false, column_key: None, default_value: None, extra: String::new() },
            ]),
            _ => Ok(vec![ColumnInfo {
                name: "value".to_string(),
                data_type: "string".to_string(),
                is_nullable: true,
                column_key: None,
                default_value: None,
                extra: key_type,
            }]),
        }
    }

    async fn get_table_ddl(&self, database: &str, table: &str) -> Result<String> {
        let mut conn = self.connection().await?;

        let db_index: i64 = database
            .trim_start_matches("db")
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(self.db_index);

        redis::cmd("SELECT")
            .arg(db_index)
            .exec_async(&mut conn)
            .await
            .context("SELECT database failed")?;

        let key_type: String = conn.key_type(table).await.context("TYPE failed")?;
        let ttl: i64 = redis::cmd("TTL").arg(table).query_async(&mut conn).await.unwrap_or(-1);
        let ttl_note = if ttl < 0 { "no expiry".to_string() } else { format!("TTL {}s", ttl) };

        let content = match key_type.as_str() {
            "hash" => {
                let pairs: Vec<(String, String)> = conn.hgetall(table).await.context("HGETALL failed")?;
                let fields: Vec<String> = pairs.iter().map(|(k, v)| format!("  {} = {:?}", k, v)).collect();
                format!("-- hash ({})\nHSET {}\n{}", ttl_note, table, fields.join("\n"))
            }
            "list" => {
                let items: Vec<String> = redis::cmd("LRANGE").arg(table).arg(0i64).arg(-1i64)
                    .query_async(&mut conn).await.context("LRANGE failed")?;
                let elements: Vec<String> = items.iter().map(|v| format!("  {:?}", v)).collect();
                format!("-- list ({})\nRPUSH {}\n{}", ttl_note, table, elements.join("\n"))
            }
            "set" => {
                let members: Vec<String> = conn.smembers(table).await.context("SMEMBERS failed")?;
                let elements: Vec<String> = members.iter().map(|v| format!("  {:?}", v)).collect();
                format!("-- set ({})\nSADD {}\n{}", ttl_note, table, elements.join("\n"))
            }
            "zset" => {
                let pairs: Vec<(String, f64)> = redis::cmd("ZRANGE").arg(table).arg(0i64).arg(-1i64).arg("WITHSCORES")
                    .query_async(&mut conn).await.context("ZRANGE failed")?;
                let elements: Vec<String> = pairs.iter().map(|(m, s)| format!("  {:?} {}", m, s)).collect();
                format!("-- zset ({})\nZADD {}\n{}", ttl_note, table, elements.join("\n"))
            }
            _ => {
                let val: Option<String> = conn.get(table).await.context("GET failed")?;
                format!("-- string ({})\nSET {} {:?}", ttl_note, table, val.unwrap_or_default())
            }
        };

        Ok(content)
    }

    async fn execute_query(&self, database: &str, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let mut conn = self.connection().await?;

        let db_index: i64 = if !database.is_empty() {
            database
                .trim_start_matches("db")
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(self.db_index)
        } else {
            self.db_index
        };

        redis::cmd("SELECT")
            .arg(db_index)
            .exec_async(&mut conn)
            .await
            .context("SELECT database failed")?;

        let tokens: Vec<&str> = sql.split_whitespace().collect();
        if tokens.is_empty() {
            return Ok(QueryResult { columns: vec![], rows: vec![], rows_affected: 0, execution_time_ms: 0 });
        }

        let cmd_name = tokens[0];
        let is_read = is_read_command(cmd_name);

        let mut cmd = redis::cmd(cmd_name);
        for arg in &tokens[1..] {
            cmd.arg(*arg);
        }

        let value: redis::Value = cmd
            .query_async(&mut conn)
            .await
            .context("Redis command execution failed")?;

        let execution_time_ms = start.elapsed().as_millis() as u64;

        if !is_read {
            let rows_affected = match &value {
                redis::Value::Int(n) => *n as u64,
                redis::Value::SimpleString(_) => 1,
                _ => 0,
            };
            return Ok(QueryResult { columns: vec![], rows: vec![], rows_affected, execution_time_ms });
        }

        let (columns, rows) = format_redis_result(cmd_name, &value);
        let rows_affected = rows.len() as u64;
        Ok(QueryResult { columns, rows, rows_affected, execution_time_ms })
    }
}

fn format_redis_result(cmd: &str, value: &redis::Value) -> (Vec<String>, Vec<Vec<Option<String>>>) {
    let cmd_upper = cmd.to_uppercase();
    match value {
        redis::Value::Array(items) => {
            match cmd_upper.as_str() {
                "HGETALL" => {
                    let pairs: Vec<_> = items.chunks(2).filter_map(|chunk| {
                        if chunk.len() == 2 {
                            let field = redis_value_to_string(&chunk[0]);
                            let val = redis_value_to_string(&chunk[1]);
                            Some(vec![field, val])
                        } else {
                            None
                        }
                    }).collect();
                    (vec!["field".to_string(), "value".to_string()], pairs)
                }
                "ZRANGE" if items.len() % 2 == 0 => {
                    let pairs: Vec<_> = items.chunks(2).filter_map(|chunk| {
                        if chunk.len() == 2 {
                            Some(vec![redis_value_to_string(&chunk[0]), redis_value_to_string(&chunk[1])])
                        } else {
                            None
                        }
                    }).collect();
                    (vec!["member".to_string(), "score".to_string()], pairs)
                }
                _ => {
                    let rows: Vec<_> = items.iter().map(|v| vec![redis_value_to_string(v)]).collect();
                    (vec!["value".to_string()], rows)
                }
            }
        }
        redis::Value::Map(pairs) => {
            let rows: Vec<_> = pairs.iter().map(|(k, v)| {
                vec![redis_value_to_string(k), redis_value_to_string(v)]
            }).collect();
            (vec!["field".to_string(), "value".to_string()], rows)
        }
        _ => {
            let row = vec![redis_value_to_string(value)];
            (vec!["value".to_string()], vec![row])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_read_command, urlencoding_encode};

    #[test]
    fn test_is_read_command() {
        assert!(is_read_command("GET"));
        assert!(is_read_command("get"));
        assert!(is_read_command("HGETALL"));
        assert!(is_read_command("KEYS"));
        assert!(!is_read_command("SET"));
        assert!(!is_read_command("DEL"));
        assert!(!is_read_command("HSET"));
    }

    #[test]
    fn test_urlencoding_encode() {
        assert_eq!(urlencoding_encode("simple"), "simple");
        assert_eq!(urlencoding_encode("p@ss!"), "p%40ss%21");
        assert_eq!(urlencoding_encode(""), "");
    }
}
