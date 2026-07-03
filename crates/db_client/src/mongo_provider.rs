use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use futures::stream::TryStreamExt;
use mongodb::bson::{Bson, Document, doc};
use mongodb::{Client, Collection};

use crate::connection::{ConnectionConfig, SslMode};
use crate::provider::DbProvider;
use crate::schema::{ColumnInfo, DatabaseInfo, QueryResult, TableInfo, TableKind};

/// Number of documents sampled per collection when inferring a pseudo-schema
/// for `describe_table`. MongoDB collections are schemaless, so this is a
/// best-effort approximation of "the columns you'd probably see", not an
/// authoritative schema the way `DESCRIBE`/`information_schema` are for SQL
/// engines.
const SCHEMA_SAMPLE_SIZE: i64 = 100;

pub struct MongoProvider {
    client: Client,
}

impl MongoProvider {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let uri = build_mongo_uri(config);
        let client = Client::with_uri_str(&uri)
            .await
            .context("Failed to connect to MongoDB")?;
        let provider = Self { client };
        provider.ping().await.context("Failed to ping MongoDB")?;
        Ok(provider)
    }

    fn collection(&self, database: &str, name: &str) -> Collection<Document> {
        self.client.database(database).collection(name)
    }
}

/// Builds a `mongodb://` connection string from the shared connection-form
/// fields. Percent-encodes the username/password the same way the Redis
/// provider does, since MongoDB connection strings follow the same URI rules.
fn build_mongo_uri(config: &ConnectionConfig) -> String {
    let mut uri = "mongodb://".to_string();
    if !config.username.is_empty() {
        uri.push_str(&percent_encode(&config.username));
        if !config.password.is_empty() {
            uri.push(':');
            uri.push_str(&percent_encode(&config.password));
        }
        uri.push('@');
    }
    uri.push_str(&config.host);
    uri.push(':');
    uri.push_str(&config.port.to_string());
    uri.push('/');
    if let Some(database) = config.database.as_deref().filter(|d| !d.is_empty()) {
        uri.push_str(database);
    }

    let tls_param = match config.ssl_mode {
        SslMode::Disabled => None,
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => Some("tls=true"),
    };
    if let Some(param) = tls_param {
        uri.push('?');
        uri.push_str(param);
    }
    uri
}

fn percent_encode(input: &str) -> String {
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

/// Whether a parsed statement reads or writes, used to enforce the read-only
/// connection/table flag the same way the SQL drivers' write-classifiers do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MongoOperationKind {
    Read,
    Write,
}

/// A tiny SQL-shaped subset translated onto MongoDB's document model, chosen
/// over accepting raw MongoDB extended-JSON so the console/grid experience
/// stays consistent with the SQL drivers (same `SELECT`/`INSERT`/`UPDATE`/
/// `DELETE` shapes users already type). This intentionally supports only a
/// single equality `WHERE` clause and a flat `SET`/column list — it is not a
/// SQL parser and does not aim to be one.
#[derive(Debug, Clone, PartialEq)]
pub enum MongoStatement {
    Select {
        collection: String,
        filter: Document,
        limit: Option<i64>,
    },
    Insert {
        collection: String,
        document: Document,
    },
    Update {
        collection: String,
        filter: Document,
        update: Document,
    },
    Delete {
        collection: String,
        filter: Document,
    },
}

impl MongoStatement {
    pub fn kind(&self) -> MongoOperationKind {
        match self {
            MongoStatement::Select { .. } => MongoOperationKind::Read,
            MongoStatement::Insert { .. }
            | MongoStatement::Update { .. }
            | MongoStatement::Delete { .. } => MongoOperationKind::Write,
        }
    }
}

fn parse_literal(raw: &str) -> Bson {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
    {
        return Bson::String(inner.to_string());
    }
    if let Some(inner) = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
    {
        return Bson::String(inner.to_string());
    }
    match trimmed {
        "true" => return Bson::Boolean(true),
        "false" => return Bson::Boolean(false),
        "null" => return Bson::Null,
        _ => {}
    }
    if let Ok(int) = trimmed.parse::<i64>() {
        return Bson::Int64(int);
    }
    if let Ok(float) = trimmed.parse::<f64>() {
        return Bson::Double(float);
    }
    Bson::String(trimmed.to_string())
}

/// Parses `<field> = <value>` into a single-key equality filter document.
/// Only a single equality clause is supported — deliberately, see
/// [`MongoStatement`]'s doc comment.
fn parse_equality_filter(clause: &str) -> Result<Document> {
    let (field, value) = clause
        .split_once('=')
        .ok_or_else(|| anyhow!("WHERE clause must be a single \"field = value\" equality"))?;
    Ok(doc! { field.trim().to_string(): parse_literal(value) })
}

/// Parses the tiny SQL-shaped subset described on [`MongoStatement`].
pub fn parse_mongo_statement(text: &str) -> Result<MongoStatement> {
    let text = text.trim().trim_end_matches(';').trim();
    let upper = text.to_uppercase();

    if let Some(rest) = strip_prefix_ci(text, "SELECT") {
        let from_idx = find_keyword(rest, "FROM")
            .ok_or_else(|| anyhow!("SELECT statement is missing FROM <collection>"))?;
        let after_from = rest[from_idx + 4..].trim();

        let (collection_and_where, limit) = match find_keyword(after_from, "LIMIT") {
            Some(idx) => {
                let limit_str = after_from[idx + 5..].trim();
                let limit = limit_str
                    .parse::<i64>()
                    .context("LIMIT must be a plain integer")?;
                (after_from[..idx].trim(), Some(limit))
            }
            None => (after_from, None),
        };

        let (collection, filter) = match find_keyword(collection_and_where, "WHERE") {
            Some(idx) => (
                collection_and_where[..idx].trim(),
                parse_equality_filter(collection_and_where[idx + 5..].trim())?,
            ),
            None => (collection_and_where.trim(), Document::new()),
        };
        if collection.is_empty() {
            bail!("SELECT statement is missing a collection name after FROM");
        }
        return Ok(MongoStatement::Select {
            collection: collection.to_string(),
            filter,
            limit,
        });
    }

    if let Some(rest) = strip_prefix_ci(text, "INSERT INTO") {
        let open_paren = rest
            .find('(')
            .ok_or_else(|| anyhow!("INSERT statement is missing a (columns) list"))?;
        let collection = rest[..open_paren].trim().to_string();
        let close_paren = rest[open_paren..]
            .find(')')
            .map(|i| open_paren + i)
            .ok_or_else(|| anyhow!("INSERT statement's (columns) list is unterminated"))?;
        let columns: Vec<String> = rest[open_paren + 1..close_paren]
            .split(',')
            .map(|s| s.trim().to_string())
            .collect();

        let values_idx = find_keyword(&rest[close_paren..], "VALUES")
            .map(|i| close_paren + i)
            .ok_or_else(|| anyhow!("INSERT statement is missing VALUES (...)"))?;
        let values_rest = &rest[values_idx + 6..];
        let values_open = values_rest
            .find('(')
            .ok_or_else(|| anyhow!("INSERT statement's VALUES list is missing an opening paren"))?;
        let values_close = values_rest[values_open..]
            .find(')')
            .map(|i| values_open + i)
            .ok_or_else(|| anyhow!("INSERT statement's VALUES list is unterminated"))?;
        let values: Vec<&str> = values_rest[values_open + 1..values_close]
            .split(',')
            .collect();

        if columns.len() != values.len() {
            bail!(
                "INSERT column count ({}) does not match value count ({})",
                columns.len(),
                values.len()
            );
        }
        let mut document = Document::new();
        for (column, value) in columns.into_iter().zip(values) {
            document.insert(column, parse_literal(value));
        }
        return Ok(MongoStatement::Insert {
            collection,
            document,
        });
    }

    if let Some(rest) = strip_prefix_ci(text, "UPDATE") {
        let set_idx =
            find_keyword(rest, "SET").ok_or_else(|| anyhow!("UPDATE statement is missing SET"))?;
        let collection = rest[..set_idx].trim().to_string();
        let after_set = rest[set_idx + 3..].trim();

        let (assignments, filter) = match find_keyword(after_set, "WHERE") {
            Some(idx) => (
                after_set[..idx].trim(),
                parse_equality_filter(after_set[idx + 5..].trim())?,
            ),
            None => (after_set, Document::new()),
        };

        let mut set_doc = Document::new();
        for assignment in assignments.split(',') {
            let (field, value) = assignment
                .split_once('=')
                .ok_or_else(|| anyhow!("SET clause must be a comma-separated \"field = value\" list"))?;
            set_doc.insert(field.trim().to_string(), parse_literal(value));
        }
        return Ok(MongoStatement::Update {
            collection,
            filter,
            update: doc! { "$set": set_doc },
        });
    }

    if let Some(rest) = strip_prefix_ci(text, "DELETE FROM") {
        let (collection, filter) = match find_keyword(rest, "WHERE") {
            Some(idx) => (
                rest[..idx].trim(),
                parse_equality_filter(rest[idx + 5..].trim())?,
            ),
            None => (rest.trim(), Document::new()),
        };
        if collection.is_empty() {
            bail!("DELETE statement is missing a collection name after FROM");
        }
        return Ok(MongoStatement::Delete {
            collection: collection.to_string(),
            filter,
        });
    }

    bail!(
        "unsupported statement (expected SELECT/INSERT INTO/UPDATE/DELETE FROM): {}",
        upper.chars().take(40).collect::<String>()
    );
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() < prefix.len() {
        return None;
    }
    if text[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(text[prefix.len()..].trim())
    } else {
        None
    }
}

/// Finds a whole-word, case-insensitive keyword occurrence (not inside a
/// longer identifier) — good enough for this deliberately tiny grammar, which
/// never needs to look inside quoted string literals for a keyword.
fn find_keyword(text: &str, keyword: &str) -> Option<usize> {
    let upper = text.to_uppercase();
    let keyword_upper = keyword.to_uppercase();
    let mut search_from = 0;
    while let Some(relative) = upper[search_from..].find(&keyword_upper) {
        let idx = search_from + relative;
        let before_ok = idx == 0 || !upper.as_bytes()[idx - 1].is_ascii_alphanumeric();
        let after_idx = idx + keyword_upper.len();
        let after_ok = after_idx >= upper.len() || !upper.as_bytes()[after_idx].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return Some(idx);
        }
        search_from = idx + 1;
    }
    None
}

/// Flattens a document's top-level fields into `QueryResult` columns, in
/// first-seen order across all rows. Nested objects/arrays render as their
/// JSON text in a single cell rather than being recursively flattened.
fn documents_to_query_result(documents: Vec<Document>) -> QueryResult {
    let mut columns: Vec<String> = Vec::new();
    for document in &documents {
        for key in document.keys() {
            if !columns.contains(key) {
                columns.push(key.clone());
            }
        }
    }
    let rows: Vec<Vec<Option<String>>> = documents
        .iter()
        .map(|document| {
            columns
                .iter()
                .map(|column| document.get(column).map(bson_to_cell_text))
                .collect()
        })
        .collect();
    QueryResult {
        columns,
        rows,
        rows_affected: 0,
        execution_time_ms: 0,
    }
}

fn bson_to_cell_text(value: &Bson) -> String {
    match value {
        Bson::String(s) => s.clone(),
        Bson::Null => String::new(),
        Bson::Boolean(b) => b.to_string(),
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(n) => n.to_string(),
        Bson::ObjectId(id) => id.to_hex(),
        other => other.to_string(),
    }
}

fn bson_type_name(value: &Bson) -> &'static str {
    match value {
        Bson::String(_) => "string",
        Bson::Boolean(_) => "bool",
        Bson::Int32(_) | Bson::Int64(_) => "int",
        Bson::Double(_) => "double",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Null => "null",
        _ => "mixed",
    }
}

#[async_trait]
impl DbProvider for MongoProvider {
    async fn ping(&self) -> Result<()> {
        self.client
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .await
            .context("MongoDB ping failed")?;
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
        let names = self
            .client
            .list_database_names()
            .await
            .context("Failed to list MongoDB databases")?;
        Ok(names.into_iter().map(|name| DatabaseInfo { name }).collect())
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>> {
        let mut names = self
            .client
            .database(database)
            .list_collection_names()
            .await
            .context("Failed to list MongoDB collections")?;
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| TableInfo {
                name,
                kind: TableKind::Table,
            })
            .collect())
    }

    /// MongoDB collections have no fixed schema. This samples up to
    /// `SCHEMA_SAMPLE_SIZE` documents and reports the union of the top-level
    /// field names/inferred BSON types seen — a best-effort approximation,
    /// not an authoritative schema. A field absent from some sampled
    /// documents is reported as nullable.
    async fn describe_table(&self, database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let collection = self.collection(database, table);
        let mut cursor = collection
            .find(Document::new())
            .limit(SCHEMA_SAMPLE_SIZE)
            .await
            .context("Failed to sample MongoDB collection for schema inference")?;

        let mut order: Vec<String> = Vec::new();
        let mut types: std::collections::HashMap<String, &'static str> =
            std::collections::HashMap::new();
        let mut presence: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut sampled = 0usize;

        while let Some(document) = cursor
            .try_next()
            .await
            .context("Failed to read a sampled MongoDB document")?
        {
            sampled += 1;
            for (key, value) in document.iter() {
                if !order.contains(key) {
                    order.push(key.clone());
                }
                *presence.entry(key.clone()).or_insert(0) += 1;
                types.entry(key.clone()).or_insert_with(|| bson_type_name(value));
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
                    extra: format!("sampled from {} document(s)", sampled),
                }
            })
            .collect())
    }

    async fn execute_query(&self, database: &str, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let statement = parse_mongo_statement(sql)?;

        let result = match statement {
            MongoStatement::Select {
                collection,
                filter,
                limit,
            } => {
                let coll = self.collection(database, &collection);
                let mut find = coll.find(filter);
                if let Some(limit) = limit {
                    find = find.limit(limit);
                }
                let mut cursor = find.await.context("MongoDB find failed")?;
                let mut documents = Vec::new();
                while let Some(document) = cursor
                    .try_next()
                    .await
                    .context("Failed to read a MongoDB result document")?
                {
                    documents.push(document);
                }
                documents_to_query_result(documents)
            }
            MongoStatement::Insert {
                collection,
                document,
            } => {
                let coll = self.collection(database, &collection);
                coll.insert_one(document)
                    .await
                    .context("MongoDB insertOne failed")?;
                QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: 1,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::Update {
                collection,
                filter,
                update,
            } => {
                let coll = self.collection(database, &collection);
                let outcome = coll
                    .update_many(filter, update)
                    .await
                    .context("MongoDB updateMany failed")?;
                QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: outcome.modified_count,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::Delete { collection, filter } => {
                let coll = self.collection(database, &collection);
                let outcome = coll
                    .delete_many(filter)
                    .await
                    .context("MongoDB deleteMany failed")?;
                QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: outcome.deleted_count,
                    execution_time_ms: 0,
                }
            }
        };

        Ok(QueryResult {
            execution_time_ms: start.elapsed().as_millis() as u64,
            ..result
        })
    }

    /// MongoDB has no `CREATE TABLE` analog. Reports a synthesized summary
    /// (sampled field list, document count) instead of fabricating fake DDL
    /// syntax — mirrors `RedisProvider::get_table_ddl`'s honesty for a
    /// schemaless engine.
    async fn get_table_ddl(&self, database: &str, table: &str) -> Result<String> {
        let columns = self.describe_table(database, table).await?;
        let coll = self.collection(database, table);
        let count = coll
            .estimated_document_count()
            .await
            .context("Failed to count MongoDB documents")?;

        let mut summary = format!(
            "-- MongoDB collection \"{}\" is schemaless; there is no CREATE TABLE statement.\n-- {} document(s), sampled schema below:\n",
            table, count
        );
        for column in columns {
            let nullable = if column.is_nullable { " (optional)" } else { "" };
            summary.push_str(&format!(
                "--   {}: {}{}\n",
                column.name, column.data_type, nullable
            ));
        }
        Ok(summary)
    }

    async fn get_database_ddl(&self, _database: &str) -> Result<String> {
        Ok("-- MongoDB is schemaless; there is no CREATE DATABASE statement.\n".to_string())
    }

    async fn truncate_table(&self, database: &str, table: &str) -> Result<()> {
        self.collection(database, table)
            .delete_many(Document::new())
            .await
            .context("MongoDB truncate (deleteMany({})) failed")?;
        Ok(())
    }

    async fn drop_table(&self, database: &str, table: &str) -> Result<()> {
        self.collection(database, table)
            .drop()
            .await
            .context("MongoDB drop collection failed")?;
        Ok(())
    }

    async fn rename_table(&self, database: &str, old_name: &str, new_name: &str) -> Result<()> {
        self.client
            .database("admin")
            .run_command(doc! {
                "renameCollection": format!("{}.{}", database, old_name),
                "to": format!("{}.{}", database, new_name),
            })
            .await
            .context("MongoDB renameCollection failed")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::DatabaseDriver;

    fn config(driver: DatabaseDriver) -> ConnectionConfig {
        ConnectionConfig {
            driver,
            host: "localhost".to_string(),
            port: 27017,
            username: "admin".to_string(),
            password: "p@ss word".to_string(),
            database: Some("mydb".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn build_mongo_uri_percent_encodes_credentials_and_includes_the_database() {
        let uri = build_mongo_uri(&config(DatabaseDriver::MongoDB));
        assert_eq!(uri, "mongodb://admin:p%40ss%20word@localhost:27017/mydb");
    }

    #[test]
    fn build_mongo_uri_omits_credentials_when_username_is_empty() {
        let mut cfg = config(DatabaseDriver::MongoDB);
        cfg.username.clear();
        cfg.password.clear();
        assert_eq!(build_mongo_uri(&cfg), "mongodb://localhost:27017/mydb");
    }

    #[test]
    fn build_mongo_uri_requests_tls_for_every_non_disabled_ssl_mode() {
        let mut cfg = config(DatabaseDriver::MongoDB);
        cfg.ssl_mode = SslMode::VerifyFull;
        assert!(build_mongo_uri(&cfg).ends_with("?tls=true"));
    }

    #[test]
    fn parse_select_extracts_collection_filter_and_limit() {
        let statement =
            parse_mongo_statement("SELECT * FROM users WHERE status = 'active' LIMIT 10").unwrap();
        assert_eq!(
            statement,
            MongoStatement::Select {
                collection: "users".to_string(),
                filter: doc! { "status": "active" },
                limit: Some(10),
            }
        );
        assert_eq!(statement.kind(), MongoOperationKind::Read);
    }

    #[test]
    fn parse_select_without_where_or_limit_uses_an_empty_filter() {
        let statement = parse_mongo_statement("select * from users").unwrap();
        assert_eq!(
            statement,
            MongoStatement::Select {
                collection: "users".to_string(),
                filter: Document::new(),
                limit: None,
            }
        );
    }

    #[test]
    fn parse_insert_zips_columns_and_values_by_position() {
        let statement =
            parse_mongo_statement("INSERT INTO users (name, age) VALUES ('Ada', 30)").unwrap();
        assert_eq!(
            statement,
            MongoStatement::Insert {
                collection: "users".to_string(),
                document: doc! { "name": "Ada", "age": 30i64 },
            }
        );
        assert_eq!(statement.kind(), MongoOperationKind::Write);
    }

    #[test]
    fn parse_insert_rejects_mismatched_column_and_value_counts() {
        let error = parse_mongo_statement("INSERT INTO users (name, age) VALUES ('Ada')")
            .unwrap_err();
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn parse_update_builds_a_set_document_and_optional_filter() {
        let statement =
            parse_mongo_statement("UPDATE users SET status = 'inactive' WHERE name = 'Ada'")
                .unwrap();
        assert_eq!(
            statement,
            MongoStatement::Update {
                collection: "users".to_string(),
                filter: doc! { "name": "Ada" },
                update: doc! { "$set": { "status": "inactive" } },
            }
        );
        assert_eq!(statement.kind(), MongoOperationKind::Write);
    }

    #[test]
    fn parse_delete_supports_an_optional_where_clause() {
        let statement = parse_mongo_statement("DELETE FROM users WHERE name = 'Ada'").unwrap();
        assert_eq!(
            statement,
            MongoStatement::Delete {
                collection: "users".to_string(),
                filter: doc! { "name": "Ada" },
            }
        );
        assert_eq!(statement.kind(), MongoOperationKind::Write);

        let unfiltered = parse_mongo_statement("DELETE FROM users").unwrap();
        assert_eq!(
            unfiltered,
            MongoStatement::Delete {
                collection: "users".to_string(),
                filter: Document::new(),
            }
        );
    }

    #[test]
    fn parse_rejects_unsupported_statement_shapes() {
        let error = parse_mongo_statement("DROP TABLE users").unwrap_err();
        assert!(error.to_string().contains("unsupported statement"));
    }

    #[test]
    fn parse_literal_infers_string_bool_null_int_and_float() {
        assert_eq!(parse_literal("'hi'"), Bson::String("hi".to_string()));
        assert_eq!(parse_literal("\"hi\""), Bson::String("hi".to_string()));
        assert_eq!(parse_literal("true"), Bson::Boolean(true));
        assert_eq!(parse_literal("null"), Bson::Null);
        assert_eq!(parse_literal("42"), Bson::Int64(42));
        assert_eq!(parse_literal("3.5"), Bson::Double(3.5));
        assert_eq!(parse_literal("bareword"), Bson::String("bareword".to_string()));
    }

    #[test]
    fn documents_to_query_result_flattens_top_level_fields_in_first_seen_order() {
        let documents = vec![
            doc! { "id": 1i64, "name": "Ada" },
            doc! { "id": 2i64, "name": "Grace", "extra": true },
        ];
        let result = documents_to_query_result(documents);
        assert_eq!(result.columns, vec!["id", "name", "extra"]);
        assert_eq!(
            result.rows,
            vec![
                vec![Some("1".to_string()), Some("Ada".to_string()), None],
                vec![
                    Some("2".to_string()),
                    Some("Grace".to_string()),
                    Some("true".to_string())
                ],
            ]
        );
    }
}
