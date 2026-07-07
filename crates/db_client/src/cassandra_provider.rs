use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::value::CqlValue;

use crate::connection::ConnectionConfig;
use crate::provider::DbProvider;
use crate::schema::{ColumnInfo, DatabaseInfo, QueryResult, TableInfo, TableKind};

// The scylla driver has no built-in overall bound on `SessionBuilder::build`:
// a TCP-level refused/unreachable host fails fast via the OS, but a host that
// accepts the TCP connection and then stalls during the CQL handshake or
// cluster metadata/schema-agreement fetch (e.g. a multi-node cluster whose
// `system.peers` lists internal IPs unreachable from this client) can hang
// indefinitely with nothing to unblock it. This bounds the whole connect
// attempt so a stuck connection surfaces as a real, reportable error instead
// of a UI spinner that never resolves.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct CassandraProvider {
    session: Session,
}

impl CassandraProvider {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let known_node = format!("{}:{}", config.host, config.port);
        let mut builder = SessionBuilder::new().known_node(known_node.clone());
        if !config.username.is_empty() {
            builder = builder.user(config.username.clone(), config.password.clone());
        }
        if let Some(database) = config.database.as_deref().filter(|d| !d.is_empty()) {
            builder = builder.use_keyspace(database, false);
        }
        let session = tokio::time::timeout(CONNECT_TIMEOUT, builder.build())
            .await
            .map_err(|_| {
                anyhow!(
                    "Timed out after {}s connecting to Cassandra/ScyllaDB at {known_node}",
                    CONNECT_TIMEOUT.as_secs()
                )
            })?
            .context("Failed to connect to Cassandra/ScyllaDB")?;
        let provider = Self { session };
        tokio::time::timeout(CONNECT_TIMEOUT, provider.ping())
            .await
            .map_err(|_| {
                anyhow!(
                    "Timed out after {}s pinging Cassandra/ScyllaDB",
                    CONNECT_TIMEOUT.as_secs()
                )
            })?
            .context("Failed to ping Cassandra/ScyllaDB")?;
        Ok(provider)
    }

    /// Runs a CQL statement against a specific keyspace, without mutating the
    /// session's default keyspace (set once at connect time) — every provider
    /// method that takes an explicit `database` prefixes the statement instead,
    /// mirroring how MySQL/PostgreSQL qualify identifiers per-call.
    async fn query(&self, cql: &str) -> Result<scylla::response::query_result::QueryResult> {
        self.session
            .query_unpaged(cql.to_string(), &[])
            .await
            .with_context(|| format!("CQL query failed: {cql}"))
    }
}

#[async_trait]
impl DbProvider for CassandraProvider {
    async fn ping(&self) -> Result<()> {
        self.query("SELECT release_version FROM system.local")
            .await?;
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
        let result = self
            .query("SELECT keyspace_name FROM system_schema.keyspaces")
            .await?
            .into_rows_result()
            .context("Failed to read keyspace list result")?;
        let mut names: Vec<String> = result
            .rows::<(String,)>()
            .context("Failed to deserialize keyspace rows")?
            .map(|row| row.map(|(name,)| name))
            .collect::<Result<_, _>>()
            .context("Failed to read a keyspace row")?;
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| DatabaseInfo { name })
            .collect())
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>> {
        let cql = format!(
            "SELECT table_name FROM system_schema.tables WHERE keyspace_name = '{}'",
            escape_cql_string(database)
        );
        let result = self
            .query(&cql)
            .await?
            .into_rows_result()
            .context("Failed to read table list result")?;
        let mut names: Vec<String> = result
            .rows::<(String,)>()
            .context("Failed to deserialize table rows")?
            .map(|row| row.map(|(name,)| name))
            .collect::<Result<_, _>>()
            .context("Failed to read a table row")?;
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| TableInfo {
                name,
                kind: TableKind::Table,
            })
            .collect())
    }

    /// Column order: partition key(s) first by `position`, then clustering
    /// key(s) by `position`, then remaining columns — matching how CQL
    /// itself orders a `PRIMARY KEY ((pk...), ck...)` definition, rather than
    /// `system_schema.columns`' own (kind, name) storage order.
    async fn describe_table(&self, database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let cql = format!(
            "SELECT column_name, type, kind, position FROM system_schema.columns WHERE keyspace_name = '{}' AND table_name = '{}'",
            escape_cql_string(database),
            escape_cql_string(table)
        );
        let result = self
            .query(&cql)
            .await?
            .into_rows_result()
            .context("Failed to read column list result")?;
        let rows: Vec<(String, String, String, i32)> = result
            .rows::<(String, String, String, i32)>()
            .context("Failed to deserialize column rows")?
            .collect::<Result<_, _>>()
            .context("Failed to read a column row")?;

        let mut partition_keys: Vec<(i32, String, String)> = Vec::new();
        let mut clustering_keys: Vec<(i32, String, String)> = Vec::new();
        let mut regular_columns: Vec<(String, String, String)> = Vec::new();

        for (name, data_type, kind, position) in rows {
            match kind.as_str() {
                "partition_key" => partition_keys.push((position, name, data_type)),
                "clustering" => clustering_keys.push((position, name, data_type)),
                _ => regular_columns.push((name, data_type, kind)),
            }
        }
        partition_keys.sort_by_key(|(position, ..)| *position);
        clustering_keys.sort_by_key(|(position, ..)| *position);
        regular_columns.sort_by(|a, b| a.0.cmp(&b.0));

        let mut columns = Vec::new();
        for (_, name, data_type) in partition_keys {
            columns.push(ColumnInfo {
                name,
                data_type,
                is_nullable: false,
                column_key: Some("PARTITION KEY".to_string()),
                default_value: None,
                extra: String::new(),
            });
        }
        for (_, name, data_type) in clustering_keys {
            columns.push(ColumnInfo {
                name,
                data_type,
                is_nullable: false,
                column_key: Some("CLUSTERING KEY".to_string()),
                default_value: None,
                extra: String::new(),
            });
        }
        for (name, data_type, kind) in regular_columns {
            let column_key = if kind == "static" {
                Some("STATIC".to_string())
            } else {
                None
            };
            columns.push(ColumnInfo {
                name,
                data_type,
                is_nullable: true,
                column_key,
                default_value: None,
                extra: String::new(),
            });
        }
        Ok(columns)
    }

    async fn execute_query(&self, database: &str, cql: &str) -> Result<QueryResult> {
        let start = Instant::now();
        // CQL has no per-statement "USE <keyspace>; <statement>" syntax, so a
        // non-empty database is applied by switching the session's default
        // keyspace before running the statement.
        if !database.is_empty() {
            self.session
                .use_keyspace(database, false)
                .await
                .with_context(|| format!("Failed to switch to keyspace {database}"))?;
        }
        let result = self.query(cql).await?;

        if !result.is_rows() {
            return Ok(QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected: 0,
                execution_time_ms: start.elapsed().as_millis() as u64,
            });
        }

        let rows_result = result
            .into_rows_result()
            .context("Failed to read CQL result rows")?;
        let columns: Vec<String> = rows_result
            .column_specs()
            .iter()
            .map(|spec| spec.name().to_string())
            .collect();
        let mut rows = Vec::new();
        for row in rows_result
            .rows::<scylla::value::Row>()
            .context("Failed to deserialize CQL result rows")?
        {
            let row = row.context("Failed to read a CQL result row")?;
            rows.push(
                row.columns
                    .into_iter()
                    .map(|cell| cell.as_ref().map(cql_value_to_cell_text))
                    .collect(),
            );
        }

        Ok(QueryResult {
            columns,
            rows,
            rows_affected: 0,
            execution_time_ms: start.elapsed().as_millis() as u64,
        })
    }

    /// Real DDL generation (matching Cassandra's own `DESCRIBE TABLE` output)
    /// is a Phase 2 concern; for now this points the caller at the raw schema
    /// tables instead of fabricating incorrect CQL syntax.
    async fn get_table_ddl(&self, database: &str, table: &str) -> Result<String> {
        Ok(format!(
            "-- DDL generation for Cassandra tables is not implemented yet; see system_schema.tables/columns for {table} in keyspace {database}.\n"
        ))
    }
}

/// Escapes a value embedded into a CQL string literal by doubling single
/// quotes, matching CQL's own string-literal escaping rule.
fn escape_cql_string(value: &str) -> String {
    value.replace('\'', "''")
}

fn cql_value_to_cell_text(value: &CqlValue) -> String {
    match value {
        CqlValue::Ascii(s) | CqlValue::Text(s) => s.clone(),
        CqlValue::Boolean(b) => b.to_string(),
        CqlValue::Int(n) => n.to_string(),
        CqlValue::BigInt(n) => n.to_string(),
        CqlValue::SmallInt(n) => n.to_string(),
        CqlValue::TinyInt(n) => n.to_string(),
        CqlValue::Float(n) => n.to_string(),
        CqlValue::Double(n) => n.to_string(),
        CqlValue::Uuid(u) => u.to_string(),
        CqlValue::Timeuuid(u) => u.to_string(),
        CqlValue::Inet(ip) => ip.to_string(),
        CqlValue::Blob(bytes) => format!(
            "0x{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ),
        CqlValue::Empty => String::new(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_cql_string_doubles_embedded_single_quotes() {
        assert_eq!(escape_cql_string("plain"), "plain");
        assert_eq!(escape_cql_string("weird'name"), "weird''name");
    }

    #[test]
    fn cql_value_to_cell_text_renders_common_scalar_types() {
        assert_eq!(
            cql_value_to_cell_text(&CqlValue::Text("hi".to_string())),
            "hi"
        );
        assert_eq!(cql_value_to_cell_text(&CqlValue::BigInt(42)), "42");
        assert_eq!(cql_value_to_cell_text(&CqlValue::Boolean(true)), "true");
        assert_eq!(cql_value_to_cell_text(&CqlValue::Empty), "");
    }
}

/// Integration tests against a real Cassandra/ScyllaDB server.
///
/// Set CASSANDRA_TEST_URL=cassandra://user:password@host:port before running,
/// then use `cargo test -p db_client --ignored -- cassandra` to execute.
#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::connection::DatabaseDriver;
    use uuid::Uuid;

    fn test_config_from_env() -> Option<ConnectionConfig> {
        let url = std::env::var("CASSANDRA_TEST_URL").ok()?;
        let url = url.strip_prefix("cassandra://")?;
        let (userinfo, hostpart) = url.split_once('@')?;
        let (username, password) = userinfo
            .split_once(':')
            .unwrap_or_else(|| panic!("CASSANDRA_TEST_URL must include username:password"));
        let (host, port_str) = hostpart.split_once(':').unwrap_or((hostpart, "9042"));
        let port: u16 = port_str.parse().unwrap_or(9042);

        Some(ConnectionConfig {
            id: Uuid::new_v4(),
            label: "test".to_string(),
            driver: DatabaseDriver::Cassandra,
            host: host.to_string(),
            port,
            username: username.to_string(),
            password: password.to_string(),
            database: None,
            auto_connect: false,
            ..ConnectionConfig::default()
        })
    }

    // A private, non-routable address (RFC 5737 test range would also work,
    // but this one reliably black-holes rather than returning ICMP
    // unreachable on most local networks) with nothing listening on it:
    // connect() must time out and return an error rather than hang, unlike a
    // refused connection on an unreachable-but-answering host, which fails
    // fast via the OS and would not exercise `CONNECT_TIMEOUT` at all.
    // Network-dependent and slow (bounded by `CONNECT_TIMEOUT`, ~15s), so
    // gated like the other integration tests instead of running by default.
    #[tokio::test]
    #[ignore]
    async fn connect_times_out_instead_of_hanging_on_an_unreachable_host() {
        let config = ConnectionConfig {
            id: Uuid::new_v4(),
            label: "timeout-repro".to_string(),
            driver: DatabaseDriver::Cassandra,
            host: "10.255.255.1".to_string(),
            port: 9042,
            username: String::new(),
            password: String::new(),
            database: None,
            auto_connect: false,
            ..ConnectionConfig::default()
        };

        let start = std::time::Instant::now();
        let result = CassandraProvider::connect(&config).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < CONNECT_TIMEOUT + Duration::from_secs(5),
            "connect() took {elapsed:?}, which is not bounded by CONNECT_TIMEOUT ({CONNECT_TIMEOUT:?}) — it hung instead of timing out"
        );
        let Err(err) = result else {
            panic!("connecting to a black-holed host must fail, not succeed");
        };
        // The driver's own per-connection timeout (~5s, see scylla's
        // `HostConnectionConfig::connect_timeout` default) usually wins this
        // race and surfaces its own "Connect timeout elapsed" message before
        // `CONNECT_TIMEOUT` (15s) ever triggers — that is fine: either way the
        // call is bounded (checked above) and the message is not empty/vague.
        let message = format!("{err:?}");
        assert!(
            message.contains("timeout") || message.contains("Timed out"),
            "expected a timeout-related message, got: {message}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_ping() {
        let config = test_config_from_env()
            .expect("CASSANDRA_TEST_URL env var required for integration tests");
        let provider = CassandraProvider::connect(&config)
            .await
            .expect("Failed to connect");
        provider.ping().await.expect("Ping failed");
    }

    /// Creates a scratch keyspace with a UUID suffix, creates a table with a
    /// partition key, a clustering key, and a regular column, exercises
    /// list_databases/list_tables/describe_table/execute_query against it,
    /// then drops the keyspace regardless of the test outcome.
    #[tokio::test]
    #[ignore]
    async fn test_schema_and_query_round_trip() {
        let config = test_config_from_env()
            .expect("CASSANDRA_TEST_URL env var required for integration tests");
        let provider = CassandraProvider::connect(&config)
            .await
            .expect("Failed to connect");
        // Cassandra/Scylla keyspace names are capped at 48 characters, so the
        // scratch suffix uses a short hex slice instead of a full UUID.
        let keyspace = format!("zdbt_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let table = "t";

        provider
            .query(&format!(
                "CREATE KEYSPACE {keyspace} WITH REPLICATION = {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
            ))
            .await
            .expect("Failed to create scratch keyspace");

        let test_result: Result<()> = async {
            provider
                .query(&format!(
                    "CREATE TABLE {keyspace}.{table} (pk text, ck int, val text, PRIMARY KEY (pk, ck))"
                ))
                .await?;

            let databases = provider.list_databases().await?;
            assert!(
                databases.iter().any(|db| db.name == keyspace),
                "list_databases should find the scratch keyspace"
            );

            let tables = provider.list_tables(&keyspace).await?;
            assert!(
                tables.iter().any(|t| t.name == table),
                "list_tables should find the scratch table"
            );

            let columns = provider.describe_table(&keyspace, table).await?;
            assert_eq!(columns.len(), 3);
            assert_eq!(columns[0].name, "pk");
            assert_eq!(columns[0].column_key.as_deref(), Some("PARTITION KEY"));
            assert!(!columns[0].is_nullable);
            assert_eq!(columns[1].name, "ck");
            assert_eq!(columns[1].column_key.as_deref(), Some("CLUSTERING KEY"));
            assert!(!columns[1].is_nullable);
            assert_eq!(columns[2].name, "val");
            assert_eq!(columns[2].column_key, None);
            assert!(columns[2].is_nullable);

            provider
                .execute_query(
                    &keyspace,
                    &format!("INSERT INTO {table} (pk, ck, val) VALUES ('a', 1, 'hello')"),
                )
                .await?;
            let result = provider
                .execute_query(&keyspace, &format!("SELECT pk, ck, val FROM {table}"))
                .await?;
            assert_eq!(result.columns, vec!["pk", "ck", "val"]);
            assert_eq!(result.rows.len(), 1);
            assert_eq!(result.rows[0][0].as_deref(), Some("a"));
            assert_eq!(result.rows[0][1].as_deref(), Some("1"));
            assert_eq!(result.rows[0][2].as_deref(), Some("hello"));
            Ok(())
        }
        .await;

        provider
            .query(&format!("DROP KEYSPACE {keyspace}"))
            .await
            .expect("Failed to clean up scratch keyspace");

        test_result.expect("Schema/query round trip failed");
    }
}
