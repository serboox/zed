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

/// A parsed `db.<collection>.<method>(<args>)` mongo shell command. This is
/// the actual language MongoDB users type (in `mongosh`, Compass, and every
/// other Mongo client) — not a SQL dialect. Arguments are relaxed-JSON
/// (unquoted object keys allowed, single or double quoted strings), parsed by
/// [`ValueParser`], with the shell's `ObjectId(...)` constructor understood
/// for `_id` filters.
#[derive(Debug, Clone, PartialEq)]
pub enum MongoStatement {
    Find {
        collection: String,
        filter: Document,
        limit: Option<i64>,
    },
    FindOne {
        collection: String,
        filter: Document,
    },
    InsertOne {
        collection: String,
        document: Document,
    },
    InsertMany {
        collection: String,
        documents: Vec<Document>,
    },
    UpdateOne {
        collection: String,
        filter: Document,
        update: Document,
    },
    UpdateMany {
        collection: String,
        filter: Document,
        update: Document,
    },
    DeleteOne {
        collection: String,
        filter: Document,
    },
    DeleteMany {
        collection: String,
        filter: Document,
    },
    Aggregate {
        collection: String,
        pipeline: Vec<Document>,
    },
    CountDocuments {
        collection: String,
        filter: Document,
    },
}

impl MongoStatement {
    pub fn kind(&self) -> MongoOperationKind {
        match self {
            MongoStatement::Find { .. }
            | MongoStatement::FindOne { .. }
            | MongoStatement::Aggregate { .. }
            | MongoStatement::CountDocuments { .. } => MongoOperationKind::Read,
            MongoStatement::InsertOne { .. }
            | MongoStatement::InsertMany { .. }
            | MongoStatement::UpdateOne { .. }
            | MongoStatement::UpdateMany { .. }
            | MongoStatement::DeleteOne { .. }
            | MongoStatement::DeleteMany { .. } => MongoOperationKind::Write,
        }
    }
}

const SUPPORTED_METHODS: &str = "find, findOne, insertOne, insertMany, updateOne, updateMany, deleteOne, deleteMany, aggregate, countDocuments";

fn unsupported_command_error(text: &str) -> anyhow::Error {
    anyhow!(
        "Unsupported mongo shell command: '{}'. Supported: {}.",
        text.chars().take(80).collect::<String>(),
        SUPPORTED_METHODS
    )
}

/// Strips a case-insensitive `db.` prefix, mongo shell's fixed handle for the
/// current database (e.g. `db.users.find(...)`).
fn strip_db_prefix(text: &str) -> Option<&str> {
    if text.len() < 2 || !text.is_char_boundary(2) || !text[..2].eq_ignore_ascii_case("db") {
        return None;
    }
    text[2..].trim_start().strip_prefix('.')
}

/// Parses a mongo shell command of the form `db.<collection>.<method>(<args>)`,
/// optionally followed by chained calls like `.limit(10)`.
pub fn parse_mongo_shell_statement(text: &str) -> Result<MongoStatement> {
    let text = text.trim().trim_end_matches(';').trim();
    let rest = strip_db_prefix(text).ok_or_else(|| {
        anyhow!("expected a mongo shell command like db.<collection>.find({{...}})")
    })?;

    // `db.<collection>.<method>(...)` has a '.' before its first '(';
    // database-level calls like `db.help()` or `db.stats()` don't, and are
    // reported as unsupported rather than misparsed as a missing collection.
    let paren_idx = rest.find('(');
    let dot_idx = rest.find('.');
    let Some(dot) = dot_idx.filter(|&d| paren_idx.is_none_or(|p| d < p)) else {
        return Err(unsupported_command_error(text));
    };
    let collection = rest[..dot].trim();
    if collection.is_empty() || !collection.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("invalid collection name '{}'", collection);
    }

    let calls = parse_call_chain(rest[dot + 1..].trim())?;
    let (method, args) = calls
        .first()
        .ok_or_else(|| anyhow!("expected a method call like db.{}.find({{...}})", collection))?;

    match method.as_str() {
        "find" => {
            let filter = parse_optional_doc_arg(args)?;
            let limit = extract_limit(&calls[1..])?;
            Ok(MongoStatement::Find {
                collection: collection.to_string(),
                filter,
                limit,
            })
        }
        "findOne" => Ok(MongoStatement::FindOne {
            collection: collection.to_string(),
            filter: parse_optional_doc_arg(args)?,
        }),
        "insertOne" => Ok(MongoStatement::InsertOne {
            collection: collection.to_string(),
            document: parse_required_doc_arg(args)?,
        }),
        "insertMany" => Ok(MongoStatement::InsertMany {
            collection: collection.to_string(),
            documents: parse_array_of_docs_arg(args)?,
        }),
        "updateOne" | "updateMany" => {
            let (filter, update) = parse_two_doc_args(args)?;
            if method == "updateOne" {
                Ok(MongoStatement::UpdateOne {
                    collection: collection.to_string(),
                    filter,
                    update,
                })
            } else {
                Ok(MongoStatement::UpdateMany {
                    collection: collection.to_string(),
                    filter,
                    update,
                })
            }
        }
        "deleteOne" => Ok(MongoStatement::DeleteOne {
            collection: collection.to_string(),
            filter: parse_optional_doc_arg(args)?,
        }),
        "deleteMany" => Ok(MongoStatement::DeleteMany {
            collection: collection.to_string(),
            filter: parse_optional_doc_arg(args)?,
        }),
        "aggregate" => Ok(MongoStatement::Aggregate {
            collection: collection.to_string(),
            pipeline: parse_array_of_docs_arg(args)?,
        }),
        "countDocuments" => Ok(MongoStatement::CountDocuments {
            collection: collection.to_string(),
            filter: parse_optional_doc_arg(args)?,
        }),
        _ => Err(unsupported_command_error(text)),
    }
}

/// Splits `<method1>(<args1>).<method2>(<args2>)...` into its calls, respecting
/// parens and quoted strings nested inside each call's arguments.
fn parse_call_chain(text: &str) -> Result<Vec<(String, String)>> {
    let mut calls = Vec::new();
    let mut remaining = text.trim();
    loop {
        if remaining.is_empty() {
            break;
        }
        let open = remaining
            .find('(')
            .ok_or_else(|| anyhow!("expected a method call like .find({{...}}) near '{}'", remaining))?;
        let method = remaining[..open].trim();
        if method.is_empty() || !method.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("invalid method name '{}'", method);
        }
        let close = find_matching_paren(&remaining[open..])? + open;
        let args = remaining[open + 1..close].trim().to_string();
        calls.push((method.to_string(), args));
        let after = remaining[close + 1..].trim_start();
        if after.is_empty() {
            break;
        }
        remaining = after
            .strip_prefix('.')
            .ok_or_else(|| anyhow!("unexpected trailing text after method call: '{}'", after))?
            .trim_start();
    }
    Ok(calls)
}

/// Finds the index (within `s`, which must start with `'('`) of the matching
/// closing paren, skipping over parens that appear inside quoted strings.
fn find_matching_paren(s: &str) -> Result<usize> {
    let mut depth = 0i32;
    let mut in_string: Option<char> = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if let Some(quote) = in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == quote {
                in_string = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => in_string = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(i);
                }
            }
            _ => {}
        }
    }
    bail!("unterminated '(' — missing closing ')'")
}

fn parse_optional_doc_arg(args: &str) -> Result<Document> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Ok(Document::new());
    }
    let mut parser = ValueParser::new(trimmed);
    let value = parser.parse_value()?;
    parser.expect_end()?;
    match value {
        Bson::Document(doc) => Ok(doc),
        other => bail!("expected a filter document, found {}", bson_type_name(&other)),
    }
}

fn parse_required_doc_arg(args: &str) -> Result<Document> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        bail!("this method requires a document argument");
    }
    let mut parser = ValueParser::new(trimmed);
    let value = parser.parse_value()?;
    parser.expect_end()?;
    match value {
        Bson::Document(doc) => Ok(doc),
        other => bail!("expected a document, found {}", bson_type_name(&other)),
    }
}

fn parse_array_of_docs_arg(args: &str) -> Result<Vec<Document>> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        bail!("this method requires an array-of-documents argument");
    }
    let mut parser = ValueParser::new(trimmed);
    let value = parser.parse_value()?;
    parser.expect_end()?;
    match value {
        Bson::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Bson::Document(doc) => Ok(doc),
                other => Err(anyhow!(
                    "expected an array of documents, found {} inside the array",
                    bson_type_name(&other)
                )),
            })
            .collect(),
        other => bail!("expected an array of documents, found {}", bson_type_name(&other)),
    }
}

fn parse_two_doc_args(args: &str) -> Result<(Document, Document)> {
    let mut parser = ValueParser::new(args.trim());
    let first = parser.parse_value()?;
    parser.skip_ws();
    parser.expect(',')?;
    let second = parser.parse_value()?;
    parser.expect_end()?;
    let filter = match first {
        Bson::Document(doc) => doc,
        other => bail!(
            "expected a filter document as the first argument, found {}",
            bson_type_name(&other)
        ),
    };
    let update = match second {
        Bson::Document(doc) => doc,
        other => bail!(
            "expected an update document as the second argument, found {}",
            bson_type_name(&other)
        ),
    };
    Ok((filter, update))
}

/// Looks for a chained `.limit(<n>)` call, used by `find(...).limit(10)`.
fn extract_limit(chained_calls: &[(String, String)]) -> Result<Option<i64>> {
    for (method, args) in chained_calls {
        if method == "limit" {
            let n: i64 = args
                .trim()
                .parse()
                .context("limit(...) must be a plain integer")?;
            return Ok(Some(n));
        }
    }
    Ok(None)
}

/// A recursive-descent parser for mongo shell arguments: relaxed JSON that
/// additionally allows unquoted object keys and the `ObjectId("...")`
/// constructor mongo shell users write for `_id` filters.
struct ValueParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> ValueParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, expected: char) -> Result<()> {
        self.skip_ws();
        match self.bump() {
            Some(c) if c == expected => Ok(()),
            Some(c) => bail!("expected '{}', found '{}'", expected, c),
            None => bail!("expected '{}', found end of input", expected),
        }
    }

    fn expect_end(&mut self) -> Result<()> {
        self.skip_ws();
        if self.pos < self.input.len() {
            bail!("unexpected trailing characters: '{}'", &self.input[self.pos..]);
        }
        Ok(())
    }

    fn parse_value(&mut self) -> Result<Bson> {
        self.skip_ws();
        match self.peek() {
            Some('{') => self.parse_object().map(Bson::Document),
            Some('[') => self.parse_array().map(Bson::Array),
            Some('"') | Some('\'') => self.parse_string().map(Bson::String),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) if c.is_alphabetic() || c == '_' => self.parse_ident_value(),
            Some(c) => bail!("unexpected character '{}' in mongo shell argument", c),
            None => bail!("unexpected end of input while parsing a value"),
        }
    }

    fn parse_object(&mut self) -> Result<Document> {
        self.expect('{')?;
        let mut document = Document::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.bump();
            return Ok(document);
        }
        loop {
            self.skip_ws();
            let key = self.parse_key()?;
            self.skip_ws();
            self.expect(':')?;
            let value = self.parse_value()?;
            document.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {
                    self.bump();
                    break;
                }
                Some(c) => bail!("expected ',' or '}}' in object, found '{}'", c),
                None => bail!("unterminated object literal"),
            }
        }
        Ok(document)
    }

    fn parse_key(&mut self) -> Result<String> {
        self.skip_ws();
        match self.peek() {
            Some('"') | Some('\'') => self.parse_string(),
            Some(c) if c.is_alphabetic() || c == '_' || c == '$' => {
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c.is_alphanumeric() || c == '_' || c == '$' {
                        self.bump();
                    } else {
                        break;
                    }
                }
                Ok(self.input[start..self.pos].to_string())
            }
            Some(c) => bail!("expected an object key, found '{}'", c),
            None => bail!("unterminated object literal (missing key)"),
        }
    }

    fn parse_array(&mut self) -> Result<Vec<Bson>> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.bump();
            return Ok(items);
        }
        loop {
            let value = self.parse_value()?;
            items.push(value);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    break;
                }
                Some(c) => bail!("expected ',' or ']' in array, found '{}'", c),
                None => bail!("unterminated array literal"),
            }
        }
        Ok(items)
    }

    fn parse_string(&mut self) -> Result<String> {
        let quote = self
            .bump()
            .filter(|c| *c == '\'' || *c == '"')
            .ok_or_else(|| anyhow!("expected a quoted string"))?;
        let mut out = String::new();
        loop {
            match self.bump() {
                Some('\\') => match self.bump() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some(c) => out.push(c),
                    None => bail!("unterminated string escape"),
                },
                Some(c) if c == quote => break,
                Some(c) => out.push(c),
                None => bail!("unterminated string literal"),
            }
        }
        Ok(out)
    }

    fn parse_number(&mut self) -> Result<Bson> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.bump();
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.bump();
            } else {
                break;
            }
        }
        let mut is_float = false;
        if self.peek() == Some('.') {
            is_float = true;
            self.bump();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            is_float = true;
            self.bump();
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.bump();
            }
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        let text = &self.input[start..self.pos];
        if is_float {
            text.parse::<f64>()
                .map(Bson::Double)
                .map_err(|_| anyhow!("invalid number literal '{}'", text))
        } else {
            text.parse::<i64>()
                .map(Bson::Int64)
                .map_err(|_| anyhow!("invalid number literal '{}'", text))
        }
    }

    fn parse_ident_value(&mut self) -> Result<Bson> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' {
                self.bump();
            } else {
                break;
            }
        }
        let ident = &self.input[start..self.pos];
        match ident {
            "true" => Ok(Bson::Boolean(true)),
            "false" => Ok(Bson::Boolean(false)),
            "null" | "undefined" => Ok(Bson::Null),
            "ObjectId" => {
                let hex = self.parse_constructor_string_arg()?;
                mongodb::bson::oid::ObjectId::parse_str(&hex)
                    .map(Bson::ObjectId)
                    .map_err(|error| anyhow!("invalid ObjectId '{}': {}", hex, error))
            }
            "NumberLong" => {
                let value = self.parse_constructor_number_arg()?;
                value
                    .parse::<i64>()
                    .map(Bson::Int64)
                    .map_err(|_| anyhow!("invalid NumberLong value '{}'", value))
            }
            "NumberInt" => {
                let value = self.parse_constructor_number_arg()?;
                value
                    .parse::<i32>()
                    .map(Bson::Int32)
                    .map_err(|_| anyhow!("invalid NumberInt value '{}'", value))
            }
            other => bail!("unrecognized identifier '{}' in mongo shell argument", other),
        }
    }

    fn parse_constructor_string_arg(&mut self) -> Result<String> {
        self.skip_ws();
        self.expect('(')?;
        self.skip_ws();
        let value = self.parse_string()?;
        self.skip_ws();
        self.expect(')')?;
        Ok(value)
    }

    fn parse_constructor_number_arg(&mut self) -> Result<String> {
        self.skip_ws();
        self.expect('(')?;
        self.skip_ws();
        let value = match self.peek() {
            Some('"') | Some('\'') => self.parse_string()?,
            _ => {
                let start = self.pos;
                while matches!(self.peek(), Some(c) if c.is_ascii_digit() || c == '-') {
                    self.bump();
                }
                self.input[start..self.pos].to_string()
            }
        };
        self.skip_ws();
        self.expect(')')?;
        Ok(value)
    }
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
        let statement = parse_mongo_shell_statement(sql)?;

        let result = match statement {
            MongoStatement::Find {
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
            MongoStatement::FindOne { collection, filter } => {
                let coll = self.collection(database, &collection);
                let document = coll
                    .find_one(filter)
                    .await
                    .context("MongoDB findOne failed")?;
                documents_to_query_result(document.into_iter().collect())
            }
            MongoStatement::InsertOne {
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
            MongoStatement::InsertMany {
                collection,
                documents,
            } => {
                let coll = self.collection(database, &collection);
                let count = documents.len() as u64;
                coll.insert_many(documents)
                    .await
                    .context("MongoDB insertMany failed")?;
                QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: count,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::UpdateOne {
                collection,
                filter,
                update,
            } => {
                let coll = self.collection(database, &collection);
                let outcome = coll
                    .update_one(filter, update)
                    .await
                    .context("MongoDB updateOne failed")?;
                QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: outcome.modified_count,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::UpdateMany {
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
            MongoStatement::DeleteOne { collection, filter } => {
                let coll = self.collection(database, &collection);
                let outcome = coll
                    .delete_one(filter)
                    .await
                    .context("MongoDB deleteOne failed")?;
                QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: outcome.deleted_count,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::DeleteMany { collection, filter } => {
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
            MongoStatement::Aggregate {
                collection,
                pipeline,
            } => {
                let coll = self.collection(database, &collection);
                let mut cursor = coll
                    .aggregate(pipeline)
                    .await
                    .context("MongoDB aggregate failed")?;
                let mut documents = Vec::new();
                while let Some(document) = cursor
                    .try_next()
                    .await
                    .context("Failed to read a MongoDB aggregate result")?
                {
                    documents.push(mongodb::bson::from_document(document)?);
                }
                documents_to_query_result(documents)
            }
            MongoStatement::CountDocuments { collection, filter } => {
                let coll = self.collection(database, &collection);
                let count = coll
                    .count_documents(filter)
                    .await
                    .context("MongoDB countDocuments failed")?;
                QueryResult {
                    columns: vec!["count".to_string()],
                    rows: vec![vec![Some(count.to_string())]],
                    rows_affected: 0,
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
    fn parse_find_extracts_collection_filter_and_chained_limit() {
        let statement =
            parse_mongo_shell_statement("db.users.find({status: 'active'}).limit(10)").unwrap();
        assert_eq!(
            statement,
            MongoStatement::Find {
                collection: "users".to_string(),
                filter: doc! { "status": "active" },
                limit: Some(10),
            }
        );
        assert_eq!(statement.kind(), MongoOperationKind::Read);
    }

    #[test]
    fn parse_find_without_args_or_limit_uses_an_empty_filter() {
        let statement = parse_mongo_shell_statement("db.users.find()").unwrap();
        assert_eq!(
            statement,
            MongoStatement::Find {
                collection: "users".to_string(),
                filter: Document::new(),
                limit: None,
            }
        );
    }

    #[test]
    fn parse_find_one_extracts_a_filter_document() {
        let statement = parse_mongo_shell_statement("db.users.findOne({name: \"Ada\"})").unwrap();
        assert_eq!(
            statement,
            MongoStatement::FindOne {
                collection: "users".to_string(),
                filter: doc! { "name": "Ada" },
            }
        );
    }

    #[test]
    fn parse_insert_one_parses_a_document_with_unquoted_keys() {
        let statement =
            parse_mongo_shell_statement("db.users.insertOne({name: 'Ada', age: 30})").unwrap();
        assert_eq!(
            statement,
            MongoStatement::InsertOne {
                collection: "users".to_string(),
                document: doc! { "name": "Ada", "age": 30i64 },
            }
        );
        assert_eq!(statement.kind(), MongoOperationKind::Write);
    }

    #[test]
    fn parse_insert_many_parses_an_array_of_documents() {
        let statement =
            parse_mongo_shell_statement("db.users.insertMany([{name: 'Ada'}, {name: 'Grace'}])")
                .unwrap();
        assert_eq!(
            statement,
            MongoStatement::InsertMany {
                collection: "users".to_string(),
                documents: vec![doc! { "name": "Ada" }, doc! { "name": "Grace" }],
            }
        );
    }

    #[test]
    fn parse_insert_many_rejects_a_non_document_array_element() {
        let error =
            parse_mongo_shell_statement("db.users.insertMany([{name: 'Ada'}, 5])").unwrap_err();
        assert!(error.to_string().contains("expected an array of documents"));
    }

    #[test]
    fn parse_update_one_and_many_pass_the_update_document_through_unwrapped() {
        let one = parse_mongo_shell_statement(
            "db.users.updateOne({name: 'Ada'}, {$set: {status: 'inactive'}})",
        )
        .unwrap();
        assert_eq!(
            one,
            MongoStatement::UpdateOne {
                collection: "users".to_string(),
                filter: doc! { "name": "Ada" },
                update: doc! { "$set": { "status": "inactive" } },
            }
        );
        assert_eq!(one.kind(), MongoOperationKind::Write);

        let many = parse_mongo_shell_statement(
            "db.users.updateMany({status: 'active'}, {$set: {flag: true}})",
        )
        .unwrap();
        assert_eq!(
            many,
            MongoStatement::UpdateMany {
                collection: "users".to_string(),
                filter: doc! { "status": "active" },
                update: doc! { "$set": { "flag": true } },
            }
        );
    }

    #[test]
    fn parse_delete_one_and_many_support_an_optional_filter() {
        let one = parse_mongo_shell_statement("db.users.deleteOne({name: 'Ada'})").unwrap();
        assert_eq!(
            one,
            MongoStatement::DeleteOne {
                collection: "users".to_string(),
                filter: doc! { "name": "Ada" },
            }
        );
        assert_eq!(one.kind(), MongoOperationKind::Write);

        let many_unfiltered = parse_mongo_shell_statement("db.users.deleteMany({})").unwrap();
        assert_eq!(
            many_unfiltered,
            MongoStatement::DeleteMany {
                collection: "users".to_string(),
                filter: Document::new(),
            }
        );
    }

    #[test]
    fn parse_aggregate_extracts_the_pipeline_stages() {
        let statement = parse_mongo_shell_statement(
            "db.orders.aggregate([{$match: {status: 'shipped'}}, {$count: 'total'}])",
        )
        .unwrap();
        assert_eq!(
            statement,
            MongoStatement::Aggregate {
                collection: "orders".to_string(),
                pipeline: vec![
                    doc! { "$match": { "status": "shipped" } },
                    doc! { "$count": "total" },
                ],
            }
        );
        assert_eq!(statement.kind(), MongoOperationKind::Read);
    }

    #[test]
    fn parse_count_documents_extracts_a_filter() {
        let statement =
            parse_mongo_shell_statement("db.users.countDocuments({active: true})").unwrap();
        assert_eq!(
            statement,
            MongoStatement::CountDocuments {
                collection: "users".to_string(),
                filter: doc! { "active": true },
            }
        );
    }

    #[test]
    fn parse_object_id_constructor_produces_a_real_object_id_filter() {
        let statement = parse_mongo_shell_statement(
            "db.users.find({_id: ObjectId(\"507f1f77bcf86cd799439011\")})",
        )
        .unwrap();
        let expected_id =
            mongodb::bson::oid::ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        assert_eq!(
            statement,
            MongoStatement::Find {
                collection: "users".to_string(),
                filter: doc! { "_id": expected_id },
                limit: None,
            }
        );
    }

    #[test]
    fn parse_rejects_a_database_level_command_with_a_clear_unsupported_error() {
        let error = parse_mongo_shell_statement("db.help()").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Unsupported mongo shell command"));
        assert!(message.contains("db.help()"));
        assert!(message.contains("countDocuments"));
    }

    #[test]
    fn parse_rejects_an_unrecognized_method_on_a_collection() {
        let error = parse_mongo_shell_statement("db.users.drop()").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Unsupported mongo shell command"));
    }

    #[test]
    fn parse_rejects_text_that_does_not_start_with_db() {
        let error = parse_mongo_shell_statement("SELECT * FROM users").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("expected a mongo shell command")
        );
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
