use std::collections::HashMap;

use aerospike::{
    AdminPolicy, AuthMode, Bin, Bins, Client, ClientPolicy, Key, PartitionFilter, QueryPolicy,
    ReadPolicy, Statement, Value, WritePolicy,
};
use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use futures::StreamExt;

use crate::connection::ConnectionConfig;
use crate::provider::DbProvider;
use crate::schema::{ColumnInfo, DatabaseInfo, QueryResult, TableInfo, TableKind};

/// Number of records sampled per set when inferring a pseudo-schema for
/// `describe_table`. Aerospike sets are schemaless (bins vary per record),
/// so this is a best-effort approximation, mirroring `MongoProvider`'s
/// `SCHEMA_SAMPLE_SIZE`.
const SCHEMA_SAMPLE_SIZE: usize = 100;

pub struct AerospikeProvider {
    client: Client,
}

impl AerospikeProvider {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let mut policy = ClientPolicy::default();
        if !config.username.is_empty() {
            policy
                .set_auth_mode(AuthMode::Internal(
                    config.username.clone(),
                    config.password.clone(),
                ))
                .context("Failed to configure Aerospike authentication")?;
        }
        let hosts = format!("{}:{}", config.host, config.port);
        let client = Client::new(&policy, &hosts)
            .await
            .context("Failed to connect to Aerospike")?;
        let provider = Self { client };
        provider.ping().await.context("Failed to ping Aerospike")?;
        Ok(provider)
    }

    /// Runs an info command against the first available cluster node.
    /// Aerospike's namespace/set metadata is only exposed this way — there
    /// is no query API for it, matching the server's own `asinfo` tool.
    async fn info(&self, commands: &[&str]) -> Result<HashMap<String, String>> {
        let node = self
            .client
            .nodes()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Aerospike cluster has no available nodes"))?;
        node.info(&AdminPolicy::default(), commands)
            .await
            .map_err(|error| anyhow!("Aerospike info command failed: {error}"))
    }
}

fn is_key_not_found(error: &aerospike::Error) -> bool {
    error.to_string().contains("KeyNotFoundError")
}

fn value_to_cell_text(value: &Value) -> String {
    match value {
        Value::Nil => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Nil => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::String(_) => "string",
        Value::Blob(_) => "blob",
        Value::List(_) => "list",
        Value::HashMap(_) => "map",
        Value::OrderedMap(_) => "map",
        Value::GeoJSON(_) => "geojson",
        _ => "mixed",
    }
}

/// Parses Aerospike's `namespaces\n` info response, a semicolon-separated
/// list of namespace names.
fn parse_namespaces(response: &str) -> Vec<String> {
    response
        .split(';')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parses Aerospike's `sets/<namespace>\n` info response: semicolon-
/// separated set descriptors, each a colon-separated list of `key=value`
/// pairs including `set=<name>`.
fn parse_sets(response: &str) -> Vec<String> {
    let mut names: Vec<String> = response
        .split(';')
        .filter(|descriptor| !descriptor.trim().is_empty())
        .filter_map(|descriptor| {
            descriptor
                .split(':')
                .find_map(|field| field.strip_prefix("set="))
        })
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    names
}

#[async_trait]
impl DbProvider for AerospikeProvider {
    async fn ping(&self) -> Result<()> {
        self.info(&["status"]).await?;
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
        let response = self
            .info(&["namespaces"])
            .await
            .context("Failed to list Aerospike namespaces")?;
        let listing = response
            .get("namespaces")
            .ok_or_else(|| anyhow!("Aerospike info response missing 'namespaces'"))?;
        Ok(parse_namespaces(listing)
            .into_iter()
            .map(|name| DatabaseInfo { name })
            .collect())
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>> {
        let command = format!("sets/{database}");
        let response = self
            .info(&[&command])
            .await
            .context("Failed to list Aerospike sets")?;
        let listing = response
            .get(&command)
            .ok_or_else(|| anyhow!("Aerospike info response missing '{command}'"))?;
        Ok(parse_sets(listing)
            .into_iter()
            .map(|name| TableInfo {
                name,
                kind: TableKind::Table,
            })
            .collect())
    }

    /// Aerospike sets have no fixed schema. This scans up to
    /// `SCHEMA_SAMPLE_SIZE` records and reports the union of bin
    /// names/inferred types seen — a best-effort approximation, not an
    /// authoritative schema, mirroring `MongoProvider::describe_table`.
    async fn describe_table(&self, database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let statement = Statement::new(database, table, Bins::All);
        let mut query_policy = QueryPolicy::default();
        query_policy.max_records = SCHEMA_SAMPLE_SIZE as u64;
        let recordset = self
            .client
            .query(&query_policy, PartitionFilter::all(), statement)
            .await
            .map_err(|error| {
                anyhow!("Failed to sample Aerospike set for schema inference: {error}")
            })?;

        let mut order: Vec<String> = Vec::new();
        let mut types: HashMap<String, &'static str> = HashMap::new();
        let mut presence: HashMap<String, usize> = HashMap::new();
        let mut sampled = 0usize;

        let mut stream = recordset.into_stream();
        while let Some(result) = stream.next().await {
            if sampled >= SCHEMA_SAMPLE_SIZE {
                break;
            }
            let record = result
                .map_err(|error| anyhow!("Failed to read a sampled Aerospike record: {error}"))?;
            sampled += 1;
            for (name, value) in &record.bins {
                if !order.contains(name) {
                    order.push(name.clone());
                }
                *presence.entry(name.clone()).or_insert(0) += 1;
                let observed_type = value_type_name(value);
                types
                    .entry(name.clone())
                    .and_modify(|existing| {
                        if *existing != observed_type {
                            *existing = "mixed";
                        }
                    })
                    .or_insert(observed_type);
            }
        }

        Ok(order
            .into_iter()
            .map(|name| {
                let is_nullable = presence.get(&name).copied().unwrap_or(0) < sampled;
                let data_type = types.get(&name).copied().unwrap_or("mixed").to_string();
                ColumnInfo {
                    name,
                    data_type,
                    is_nullable,
                    column_key: None,
                    default_value: None,
                    extra: format!("sampled from {sampled} record(s)"),
                }
            })
            .collect())
    }

    /// Aerospike has no query language — records are accessed by key or via
    /// the Get/Put/Scan form the Database Explorer provides instead. This
    /// exists only so `AerospikeProvider` satisfies `DbProvider`; nothing in
    /// the UI should reach it for Aerospike connections.
    async fn execute_query(&self, _database: &str, _query: &str) -> Result<QueryResult> {
        bail!(
            "Aerospike does not use a query language — use the Get/Put/Scan form in the Database Explorer instead."
        )
    }

    async fn get_table_ddl(&self, database: &str, table: &str) -> Result<String> {
        let columns = self.describe_table(database, table).await?;
        let mut summary = format!(
            "-- Aerospike set \"{table}\" is schemaless; there is no CREATE TABLE statement.\n-- Sampled bins:\n"
        );
        if columns.is_empty() {
            summary.push_str("--   (no records sampled)\n");
        }
        for column in columns {
            let nullable = if column.is_nullable {
                " (optional)"
            } else {
                ""
            };
            summary.push_str(&format!(
                "--   {}: {}{}\n",
                column.name, column.data_type, nullable
            ));
        }
        Ok(summary)
    }

    async fn get_database_ddl(&self, _database: &str) -> Result<String> {
        Ok("-- Aerospike is schemaless; there is no CREATE DATABASE statement.\n".to_string())
    }

    /// Fetches a single record by key, for the Database Explorer's Get form.
    async fn get_record(
        &self,
        namespace: &str,
        set: &str,
        key: &str,
    ) -> Result<Option<Vec<(String, String)>>> {
        let aero_key = Key::new(namespace, set, Value::from(key.to_string()))
            .context("Failed to build Aerospike key")?;
        match self
            .client
            .get(&ReadPolicy::default(), &aero_key, Bins::All)
            .await
        {
            Ok(record) => {
                let mut bins: Vec<(String, String)> = record
                    .bins
                    .into_iter()
                    .map(|(name, value)| (name, value_to_cell_text(&value)))
                    .collect();
                bins.sort_by(|a, b| a.0.cmp(&b.0));
                Ok(Some(bins))
            }
            Err(error) if is_key_not_found(&error) => Ok(None),
            Err(error) => Err(anyhow!("Aerospike get failed: {error}")),
        }
    }

    /// Writes `bins` to a record by key, for the Database Explorer's Put
    /// form. Creates the record if it does not already exist.
    async fn put_record(
        &self,
        namespace: &str,
        set: &str,
        key: &str,
        bins: &[(String, String)],
    ) -> Result<()> {
        let aero_key = Key::new(namespace, set, Value::from(key.to_string()))
            .context("Failed to build Aerospike key")?;
        let aero_bins: Vec<Bin> = bins
            .iter()
            .map(|(name, value)| Bin::new(name.clone(), Value::from(value.clone())))
            .collect();
        // `send_key` stores the user key alongside the record so it comes
        // back in `record.key.user_key` on scan/query -- without it,
        // `scan_records`' "key" column is always empty for records written
        // here.
        let mut write_policy = WritePolicy::default();
        write_policy.send_key = true;
        self.client
            .put(&write_policy, &aero_key, &aero_bins)
            .await
            .map_err(|error| anyhow!("Aerospike put failed: {error}"))
    }

    /// Scans up to `limit` records in `namespace`/`set`, for the Database
    /// Explorer's Scan form. A `Statement` with no filters runs in scan
    /// mode rather than a secondary-index query (see `Client::query`'s
    /// docs on the `aerospike` crate).
    async fn scan_records(&self, namespace: &str, set: &str, limit: usize) -> Result<QueryResult> {
        let statement = Statement::new(namespace, set, Bins::All);
        let mut query_policy = QueryPolicy::default();
        query_policy.max_records = limit as u64;
        let recordset = self
            .client
            .query(&query_policy, PartitionFilter::all(), statement)
            .await
            .map_err(|error| anyhow!("Aerospike scan failed: {error}"))?;

        let mut columns: Vec<String> = vec!["key".to_string()];
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        let mut stream = recordset.into_stream();
        while let Some(result) = stream.next().await {
            if rows.len() >= limit {
                break;
            }
            let record = result.map_err(|error| anyhow!("Aerospike scan failed: {error}"))?;
            let key_text = record
                .key
                .as_ref()
                .and_then(|key| key.user_key.as_ref())
                .map(value_to_cell_text)
                .unwrap_or_default();

            let mut row_bins: Vec<(String, String)> = record
                .bins
                .iter()
                .map(|(name, value)| (name.clone(), value_to_cell_text(value)))
                .collect();
            row_bins.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, _) in &row_bins {
                if !columns.contains(name) {
                    columns.push(name.clone());
                }
            }

            let mut row: Vec<Option<String>> = Vec::with_capacity(columns.len());
            row.push(Some(key_text));
            for column in &columns[1..] {
                row.push(
                    row_bins
                        .iter()
                        .find(|(name, _)| name == column)
                        .map(|(_, value)| value.clone()),
                );
            }
            rows.push(row);
        }

        // Earlier rows were built before every column was known, so pad
        // them out to the final column count instead of re-querying.
        for row in &mut rows {
            row.resize(columns.len(), None);
        }

        Ok(QueryResult {
            columns,
            rows,
            rows_affected: 0,
            execution_time_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_namespaces_splits_and_trims_semicolon_list() {
        assert_eq!(
            parse_namespaces("test;bar;  baz  "),
            vec!["test".to_string(), "bar".to_string(), "baz".to_string()]
        );
        assert_eq!(parse_namespaces(""), Vec::<String>::new());
    }

    #[test]
    fn parse_sets_extracts_set_names_and_dedupes() {
        let response = "ns=test:set=users:objects=10:tombstones=0;ns=test:set=users:objects=1;ns=test:set=orders:objects=3;";
        assert_eq!(
            parse_sets(response),
            vec!["orders".to_string(), "users".to_string()]
        );
    }

    #[test]
    fn parse_sets_ignores_descriptors_without_a_set_field() {
        assert_eq!(parse_sets(""), Vec::<String>::new());
        assert_eq!(parse_sets("ns=test:objects=0;"), Vec::<String>::new());
    }

    #[test]
    fn value_to_cell_text_formats_common_scalar_types() {
        assert_eq!(value_to_cell_text(&Value::Nil), "");
        assert_eq!(value_to_cell_text(&Value::Bool(true)), "true");
        assert_eq!(value_to_cell_text(&Value::Int(42)), "42");
        assert_eq!(value_to_cell_text(&Value::String("hi".to_string())), "hi");
    }

    #[test]
    fn value_type_name_covers_scalars_and_falls_back_to_mixed_for_geojson() {
        assert_eq!(value_type_name(&Value::Int(1)), "int");
        assert_eq!(value_type_name(&Value::String(String::new())), "string");
        assert_eq!(value_type_name(&Value::List(Vec::new())), "list");
    }
}
