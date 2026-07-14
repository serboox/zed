use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use futures::stream::TryStreamExt;
use mongodb::bson::{Bson, Document, doc};
use mongodb::options::IndexOptions;
use mongodb::{Client, Collection, IndexModel};

use crate::connection::{ConnectionConfig, SslMode};
use crate::provider::DbProvider;
use crate::schema::{ColumnInfo, DatabaseInfo, IndexInfo, QueryResult, TableInfo, TableKind};

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
    Distinct {
        collection: String,
        field: String,
        filter: Document,
    },
    ReplaceOne {
        collection: String,
        filter: Document,
        replacement: Document,
    },
    FindOneAndUpdate {
        collection: String,
        filter: Document,
        update: Document,
    },
    FindOneAndDelete {
        collection: String,
        filter: Document,
    },
    FindOneAndReplace {
        collection: String,
        filter: Document,
        replacement: Document,
    },
    BulkWrite {
        collection: String,
        operations: Vec<BulkWriteOp>,
    },
    Drop {
        collection: String,
    },
    CreateIndex {
        collection: String,
        keys: Document,
        options: Document,
    },
    DropIndex {
        collection: String,
        name: String,
    },
    GetIndexes {
        collection: String,
    },
    CollectionStats {
        collection: String,
    },
    EstimatedDocumentCount {
        collection: String,
    },
    RenameCollection {
        collection: String,
        new_name: String,
    },
    Help,
    DbStats,
    GetCollectionNames,
    ShowDatabases,
    ShowCollections,
}

/// A single operation inside a `bulkWrite([...])` array, e.g.
/// `{ insertOne: { document: {...} } }` or
/// `{ updateOne: { filter: {...}, update: {...}, upsert: true } }`.
#[derive(Debug, Clone, PartialEq)]
pub enum BulkWriteOp {
    InsertOne(Document),
    UpdateOne {
        filter: Document,
        update: Document,
        upsert: bool,
    },
    UpdateMany {
        filter: Document,
        update: Document,
        upsert: bool,
    },
    ReplaceOne {
        filter: Document,
        replacement: Document,
        upsert: bool,
    },
    DeleteOne(Document),
    DeleteMany(Document),
}

impl MongoStatement {
    pub fn kind(&self) -> MongoOperationKind {
        match self {
            MongoStatement::Find { .. }
            | MongoStatement::FindOne { .. }
            | MongoStatement::Aggregate { .. }
            | MongoStatement::CountDocuments { .. }
            | MongoStatement::Distinct { .. }
            | MongoStatement::GetIndexes { .. }
            | MongoStatement::CollectionStats { .. }
            | MongoStatement::EstimatedDocumentCount { .. }
            | MongoStatement::Help
            | MongoStatement::DbStats
            | MongoStatement::GetCollectionNames
            | MongoStatement::ShowDatabases
            | MongoStatement::ShowCollections => MongoOperationKind::Read,
            MongoStatement::InsertOne { .. }
            | MongoStatement::InsertMany { .. }
            | MongoStatement::UpdateOne { .. }
            | MongoStatement::UpdateMany { .. }
            | MongoStatement::DeleteOne { .. }
            | MongoStatement::DeleteMany { .. }
            | MongoStatement::ReplaceOne { .. }
            | MongoStatement::FindOneAndUpdate { .. }
            | MongoStatement::FindOneAndDelete { .. }
            | MongoStatement::FindOneAndReplace { .. }
            | MongoStatement::BulkWrite { .. }
            | MongoStatement::Drop { .. }
            | MongoStatement::CreateIndex { .. }
            | MongoStatement::DropIndex { .. }
            | MongoStatement::RenameCollection { .. } => MongoOperationKind::Write,
        }
    }
}

const SUPPORTED_METHODS: &str = "collection-level: find, findOne, insertOne, insertMany, updateOne, updateMany, deleteOne, deleteMany, replaceOne, findOneAndUpdate, findOneAndDelete, findOneAndReplace, aggregate, countDocuments, count, distinct, bulkWrite, drop, createIndex, dropIndex, getIndexes, stats, estimatedDocumentCount, renameCollection; database-level: db.help(), db.stats(), db.getCollectionNames(), show dbs, show collections";

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

/// Recognizes the bare shell helper phrases `show dbs`/`show databases` and
/// `show collections`/`show tables`, which don't start with `db.` at all —
/// unlike every other mongo shell command this parser understands.
fn parse_shell_helper_statement(text: &str) -> Option<MongoStatement> {
    match text.to_ascii_lowercase().as_str() {
        "show dbs" | "show databases" => Some(MongoStatement::ShowDatabases),
        "show collections" | "show tables" => Some(MongoStatement::ShowCollections),
        _ => None,
    }
}

/// Parses a database-level call like `db.help()`, `db.stats()`, or
/// `db.getCollectionNames()` — these have no dot before their first paren,
/// unlike `db.<collection>.<method>(...)` calls.
fn parse_database_level_statement(rest: &str, original_text: &str) -> Result<MongoStatement> {
    let calls = parse_call_chain(rest)?;
    let (method, args) = calls
        .first()
        .ok_or_else(|| unsupported_command_error(original_text))?;
    if calls.len() > 1 {
        bail!(
            "unexpected chained call after '{}(...)': database-level commands don't support chaining, found '{}'",
            method,
            original_text.chars().take(80).collect::<String>()
        );
    }
    match method.as_str() {
        "help" => {
            expect_no_args(args)?;
            Ok(MongoStatement::Help)
        }
        "stats" => {
            expect_no_args(args)?;
            Ok(MongoStatement::DbStats)
        }
        "getCollectionNames" => {
            expect_no_args(args)?;
            Ok(MongoStatement::GetCollectionNames)
        }
        _ => Err(unsupported_command_error(original_text)),
    }
}

/// Parses a mongo shell command of the form `db.<collection>.<method>(<args>)`,
/// optionally followed by chained calls like `.limit(10)`.
pub fn parse_mongo_shell_statement(text: &str) -> Result<MongoStatement> {
    let text = text.trim().trim_end_matches(';').trim();
    if let Some(statement) = parse_shell_helper_statement(text) {
        return Ok(statement);
    }

    let rest = strip_db_prefix(text).ok_or_else(|| {
        anyhow!("expected a mongo shell command like db.<collection>.find({{...}})")
    })?;

    // `db.<collection>.<method>(...)` has a '.' before its first '(';
    // database-level calls like `db.help()` or `db.stats()` don't.
    let paren_idx = rest.find('(');
    let dot_idx = rest.find('.');
    let Some(dot) = dot_idx.filter(|&d| paren_idx.is_none_or(|p| d < p)) else {
        return parse_database_level_statement(rest, text);
    };
    let collection = rest[..dot].trim();
    if collection.is_empty()
        || !collection
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!("invalid collection name '{}'", collection);
    }

    let calls = parse_call_chain(rest[dot + 1..].trim())?;
    let (method, args) = calls.first().ok_or_else(|| {
        anyhow!(
            "expected a method call like db.{}.find({{...}})",
            collection
        )
    })?;
    // Only `find(...)` supports a chained call (`.limit(n)`); every other
    // method silently ignoring trailing chained calls would let malformed
    // input like `db.users.drop().limit(1)` execute as if it were valid.
    if method != "find" && calls.len() > 1 {
        bail!(
            "unexpected chained call after '{}(...)': only find(...) supports chaining, found '{}'",
            method,
            text.chars().take(80).collect::<String>()
        );
    }

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
        "countDocuments" | "count" => Ok(MongoStatement::CountDocuments {
            collection: collection.to_string(),
            filter: parse_optional_doc_arg(args)?,
        }),
        "distinct" => {
            let (field, filter) = parse_distinct_args(args)?;
            Ok(MongoStatement::Distinct {
                collection: collection.to_string(),
                field,
                filter,
            })
        }
        "replaceOne" => {
            let (filter, replacement) = parse_two_doc_args(args)?;
            Ok(MongoStatement::ReplaceOne {
                collection: collection.to_string(),
                filter,
                replacement,
            })
        }
        "findOneAndUpdate" => {
            let (filter, update) = parse_two_doc_args(args)?;
            Ok(MongoStatement::FindOneAndUpdate {
                collection: collection.to_string(),
                filter,
                update,
            })
        }
        "findOneAndDelete" => Ok(MongoStatement::FindOneAndDelete {
            collection: collection.to_string(),
            filter: parse_optional_doc_arg(args)?,
        }),
        "findOneAndReplace" => {
            let (filter, replacement) = parse_two_doc_args(args)?;
            Ok(MongoStatement::FindOneAndReplace {
                collection: collection.to_string(),
                filter,
                replacement,
            })
        }
        "bulkWrite" => Ok(MongoStatement::BulkWrite {
            collection: collection.to_string(),
            operations: parse_bulk_write_ops(args)?,
        }),
        "drop" => {
            expect_no_args(args)?;
            Ok(MongoStatement::Drop {
                collection: collection.to_string(),
            })
        }
        "createIndex" => {
            let (keys, options) = parse_doc_and_optional_doc_args(args)?;
            Ok(MongoStatement::CreateIndex {
                collection: collection.to_string(),
                keys,
                options,
            })
        }
        "dropIndex" => Ok(MongoStatement::DropIndex {
            collection: collection.to_string(),
            name: parse_string_arg(args)?,
        }),
        "getIndexes" => {
            expect_no_args(args)?;
            Ok(MongoStatement::GetIndexes {
                collection: collection.to_string(),
            })
        }
        "stats" => {
            expect_no_args(args)?;
            Ok(MongoStatement::CollectionStats {
                collection: collection.to_string(),
            })
        }
        "estimatedDocumentCount" => {
            expect_no_args(args)?;
            Ok(MongoStatement::EstimatedDocumentCount {
                collection: collection.to_string(),
            })
        }
        "renameCollection" => Ok(MongoStatement::RenameCollection {
            collection: collection.to_string(),
            new_name: parse_string_arg(args)?,
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
        let open = remaining.find('(').ok_or_else(|| {
            anyhow!(
                "expected a method call like .find({{...}}) near '{}'",
                remaining
            )
        })?;
        let method = remaining[..open].trim();
        if method.is_empty()
            || !method
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
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
        other => bail!(
            "expected a filter document, found {}",
            bson_type_name(&other)
        ),
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
        other => bail!(
            "expected an array of documents, found {}",
            bson_type_name(&other)
        ),
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

/// Parses `(<document>)` or `(<document>, <document>)`, used by
/// `createIndex(keys, options?)` where the second argument is optional.
fn parse_doc_and_optional_doc_args(args: &str) -> Result<(Document, Document)> {
    let mut parser = ValueParser::new(args.trim());
    let first = parser.parse_value()?;
    let first_doc = match first {
        Bson::Document(doc) => doc,
        other => bail!(
            "expected a document as the first argument, found {}",
            bson_type_name(&other)
        ),
    };
    parser.skip_ws();
    let second_doc = if parser.peek() == Some(',') {
        parser.bump();
        let second = parser.parse_value()?;
        match second {
            Bson::Document(doc) => doc,
            other => bail!(
                "expected a document as the second argument, found {}",
                bson_type_name(&other)
            ),
        }
    } else {
        Document::new()
    };
    parser.expect_end()?;
    Ok((first_doc, second_doc))
}

/// Parses `(<field>)` or `(<field>, <filter>)`, used by `distinct(field, filter?)`.
fn parse_distinct_args(args: &str) -> Result<(String, Document)> {
    let mut parser = ValueParser::new(args.trim());
    let field = match parser.parse_value()? {
        Bson::String(s) => s,
        other => bail!(
            "expected a field name string as the first argument, found {}",
            bson_type_name(&other)
        ),
    };
    parser.skip_ws();
    let filter = if parser.peek() == Some(',') {
        parser.bump();
        match parser.parse_value()? {
            Bson::Document(doc) => doc,
            other => bail!(
                "expected a filter document as the second argument, found {}",
                bson_type_name(&other)
            ),
        }
    } else {
        Document::new()
    };
    parser.expect_end()?;
    Ok((field, filter))
}

/// Parses a single bare string argument, used by `dropIndex("name")` and
/// `renameCollection("newName")`.
fn parse_string_arg(args: &str) -> Result<String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        bail!("this method requires a string argument");
    }
    let mut parser = ValueParser::new(trimmed);
    let value = parser.parse_value()?;
    parser.expect_end()?;
    match value {
        Bson::String(s) => Ok(s),
        other => bail!("expected a string argument, found {}", bson_type_name(&other)),
    }
}

/// Verifies a no-argument method (e.g. `drop()`, `stats()`) was actually
/// called with no arguments.
fn expect_no_args(args: &str) -> Result<()> {
    if args.trim().is_empty() {
        Ok(())
    } else {
        bail!("this method does not take any arguments, found '{}'", args);
    }
}

/// Parses each element of a `bulkWrite([...])` array — a document with
/// exactly one top-level key naming the operation, e.g.
/// `{ insertOne: { document: {...} } }`.
fn parse_bulk_write_ops(args: &str) -> Result<Vec<BulkWriteOp>> {
    parse_array_of_docs_arg(args)?
        .into_iter()
        .map(parse_bulk_write_op)
        .collect()
}

fn parse_bulk_write_op(document: Document) -> Result<BulkWriteOp> {
    let mut fields = document.into_iter();
    let (op_name, op_value) = fields
        .next()
        .ok_or_else(|| anyhow!("each bulkWrite operation must have exactly one key naming the operation"))?;
    if fields.next().is_some() {
        bail!(
            "each bulkWrite operation must have exactly one key naming the operation, found extra keys alongside '{}'",
            op_name
        );
    }
    let op_doc = match op_value {
        Bson::Document(doc) => doc,
        other => bail!(
            "expected an operation document for '{}', found {}",
            op_name,
            bson_type_name(&other)
        ),
    };
    let get_document = |field: &str| -> Result<Document> {
        op_doc
            .get_document(field)
            .map(Document::clone)
            .map_err(|_| anyhow!("'{}' bulkWrite operation requires a '{}' field", op_name, field))
    };
    match op_name.as_str() {
        "insertOne" => Ok(BulkWriteOp::InsertOne(get_document("document")?)),
        "updateOne" | "updateMany" => {
            let filter = get_document("filter")?;
            let update = get_document("update")?;
            let upsert = op_doc.get_bool("upsert").unwrap_or(false);
            if op_name == "updateOne" {
                Ok(BulkWriteOp::UpdateOne {
                    filter,
                    update,
                    upsert,
                })
            } else {
                Ok(BulkWriteOp::UpdateMany {
                    filter,
                    update,
                    upsert,
                })
            }
        }
        "replaceOne" => Ok(BulkWriteOp::ReplaceOne {
            filter: get_document("filter")?,
            replacement: get_document("replacement")?,
            upsert: op_doc.get_bool("upsert").unwrap_or(false),
        }),
        "deleteOne" => Ok(BulkWriteOp::DeleteOne(get_document("filter")?)),
        "deleteMany" => Ok(BulkWriteOp::DeleteMany(get_document("filter")?)),
        other => bail!("unsupported bulkWrite operation '{}'", other),
    }
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
            bail!(
                "unexpected trailing characters: '{}'",
                &self.input[self.pos..]
            );
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
            other => bail!(
                "unrecognized identifier '{}' in mongo shell argument",
                other
            ),
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

/// Maps the recognized keys of a `createIndex(keys, options)` options
/// document (`unique`, `name`, `sparse`, `expireAfterSeconds`,
/// `partialFilterExpression`) onto the driver's `IndexOptions`. Other keys
/// are ignored rather than rejected, since mongosh accepts several
/// rarely-used index options this parser doesn't model.
fn build_index_options(options: &Document) -> Result<IndexOptions> {
    let expire_after_seconds = options
        .get_i64("expireAfterSeconds")
        .ok()
        .or_else(|| options.get_i32("expireAfterSeconds").ok().map(i64::from));
    let expire_after = expire_after_seconds
        .map(|seconds| -> Result<Duration> {
            let seconds: u64 = seconds
                .try_into()
                .map_err(|_| anyhow!("expireAfterSeconds must not be negative, got {seconds}"))?;
            Ok(Duration::from_secs(seconds))
        })
        .transpose()?;
    Ok(IndexOptions::builder()
        .unique(options.get_bool("unique").ok())
        .name(options.get_str("name").ok().map(str::to_string))
        .sparse(options.get_bool("sparse").ok())
        .partial_filter_expression(options.get_document("partialFilterExpression").ok().cloned())
        .expire_after(expire_after)
        .build())
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

/// Renders a single BSON value the way it would be written in a `mongosh`
/// literal (index key directions like `1`/`-1`, string index types like
/// `"text"`, booleans, etc.) — index specs only ever contain these simple
/// scalar shapes, so this does not need to handle the full BSON type space.
fn bson_shell_literal(value: &Bson) -> String {
    match value {
        Bson::Int32(v) => v.to_string(),
        Bson::Int64(v) => v.to_string(),
        Bson::Double(v) => v.to_string(),
        Bson::String(s) => format!("\"{}\"", s.replace('"', "\\\"")),
        Bson::Boolean(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// Renders a BSON document as a `mongosh`-style object literal, e.g.
/// `{ name: 1, age: -1 }`.
fn bson_shell_document(document: &Document) -> String {
    let fields = document
        .iter()
        .map(|(key, value)| format!("{key}: {}", bson_shell_literal(value)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{ {fields} }}")
}

/// Synthesizes the `db.<collection>.createIndex(...)` call that would
/// recreate `index`, mirroring the key/uniqueness/name options it actually
/// has — the closest Mongo equivalent to `SHOW CREATE TABLE` including a
/// table's indexes.
fn format_create_index(table: &str, index: &mongodb::IndexModel) -> String {
    let keys = bson_shell_document(&index.keys);
    let mut option_fields = Vec::new();
    if let Some(options) = &index.options {
        if options.unique == Some(true) {
            option_fields.push("unique: true".to_string());
        }
        if let Some(name) = &options.name {
            option_fields.push(format!("name: \"{}\"", name.replace('"', "\\\"")));
        }
    }
    if option_fields.is_empty() {
        format!("db.{table}.createIndex({keys});\n")
    } else {
        format!(
            "db.{table}.createIndex({keys}, {{ {} }});\n",
            option_fields.join(", ")
        )
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
        Ok(names
            .into_iter()
            .map(|name| DatabaseInfo { name })
            .collect())
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
                types
                    .entry(key.clone())
                    .or_insert_with(|| bson_type_name(value));
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

    // Includes the implicit `_id_` index (unlike `get_table_ddl`, which
    // omits it since it isn't something a user would `createIndex` again --
    // same reasoning MySQL/Postgres use for listing a PRIMARY KEY index here
    // but leaving it out of a "how would I recreate this" DDL summary).
    async fn list_indexes(&self, database: &str, table: &str) -> Result<Vec<IndexInfo>> {
        let coll = self.collection(database, table);
        let indexes = coll
            .list_indexes()
            .await
            .context("Failed to list MongoDB indexes")?
            .try_collect::<Vec<_>>()
            .await
            .context("Failed to read a MongoDB index definition")?;

        Ok(indexes
            .into_iter()
            .map(|index| {
                let name = index
                    .options
                    .as_ref()
                    .and_then(|options| options.name.clone())
                    .unwrap_or_default();
                let columns: Vec<String> = index.keys.iter().map(|(key, _)| key.clone()).collect();
                let unique = index
                    .options
                    .as_ref()
                    .and_then(|options| options.unique)
                    .unwrap_or(false);
                IndexInfo {
                    name,
                    columns,
                    unique,
                    index_type: "btree".to_string(),
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
            MongoStatement::Distinct {
                collection,
                field,
                filter,
            } => {
                let coll = self.collection(database, &collection);
                let values = coll
                    .distinct(&field, filter)
                    .await
                    .context("MongoDB distinct failed")?;
                QueryResult {
                    columns: vec!["distinct".to_string()],
                    rows: values
                        .iter()
                        .map(|value| vec![Some(bson_to_cell_text(value))])
                        .collect(),
                    rows_affected: 0,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::ReplaceOne {
                collection,
                filter,
                replacement,
            } => {
                let coll = self.collection(database, &collection);
                let outcome = coll
                    .replace_one(filter, replacement)
                    .await
                    .context("MongoDB replaceOne failed")?;
                QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: outcome.modified_count,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::FindOneAndUpdate {
                collection,
                filter,
                update,
            } => {
                let coll = self.collection(database, &collection);
                let document = coll
                    .find_one_and_update(filter, update)
                    .await
                    .context("MongoDB findOneAndUpdate failed")?;
                documents_to_query_result(document.into_iter().collect())
            }
            MongoStatement::FindOneAndDelete { collection, filter } => {
                let coll = self.collection(database, &collection);
                let document = coll
                    .find_one_and_delete(filter)
                    .await
                    .context("MongoDB findOneAndDelete failed")?;
                documents_to_query_result(document.into_iter().collect())
            }
            MongoStatement::FindOneAndReplace {
                collection,
                filter,
                replacement,
            } => {
                let coll = self.collection(database, &collection);
                let document = coll
                    .find_one_and_replace(filter, replacement)
                    .await
                    .context("MongoDB findOneAndReplace failed")?;
                documents_to_query_result(document.into_iter().collect())
            }
            MongoStatement::BulkWrite {
                collection,
                operations,
            } => {
                let coll = self.collection(database, &collection);
                let mut rows_affected = 0u64;
                let total_operations = operations.len();
                // The driver's native `Client::bulk_write` requires MongoDB
                // 8.0+ (this repo's integration stack runs mongo:7.0, and
                // older servers are common), so bulkWrite executes each
                // operation sequentially through the same per-op calls used
                // above rather than the native bulk command. Because of
                // this, a failure partway through has already durably
                // applied every operation before it — the error names how
                // many succeeded and how many rows they affected so a
                // partial bulkWrite is never silently invisible.
                for (index, operation) in operations.into_iter().enumerate() {
                    let outcome: Result<u64> = async {
                        Ok(match operation {
                            BulkWriteOp::InsertOne(document) => {
                                coll.insert_one(document).await?;
                                1
                            }
                            BulkWriteOp::UpdateOne {
                                filter,
                                update,
                                upsert,
                            } => {
                                let result = coll.update_one(filter, update).upsert(upsert).await?;
                                result.modified_count + result.upserted_id.is_some() as u64
                            }
                            BulkWriteOp::UpdateMany {
                                filter,
                                update,
                                upsert,
                            } => {
                                let result =
                                    coll.update_many(filter, update).upsert(upsert).await?;
                                result.modified_count + result.upserted_id.is_some() as u64
                            }
                            BulkWriteOp::ReplaceOne {
                                filter,
                                replacement,
                                upsert,
                            } => {
                                let result =
                                    coll.replace_one(filter, replacement).upsert(upsert).await?;
                                result.modified_count + result.upserted_id.is_some() as u64
                            }
                            BulkWriteOp::DeleteOne(filter) => coll.delete_one(filter).await?.deleted_count,
                            BulkWriteOp::DeleteMany(filter) => {
                                coll.delete_many(filter).await?.deleted_count
                            }
                        })
                    }
                    .await;
                    let already_affected = rows_affected;
                    let affected_by_this_operation = outcome.with_context(|| {
                        format!(
                            "MongoDB bulkWrite operation {} of {total_operations} failed; {already_affected} row(s) were already affected by the preceding operations in this batch",
                            index + 1
                        )
                    })?;
                    rows_affected += affected_by_this_operation;
                }
                QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::Drop { collection } => {
                let coll = self.collection(database, &collection);
                coll.drop().await.context("MongoDB drop failed")?;
                QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: 1,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::CreateIndex {
                collection,
                keys,
                options,
            } => {
                let coll = self.collection(database, &collection);
                let model = IndexModel::builder()
                    .keys(keys)
                    .options(build_index_options(&options)?)
                    .build();
                coll.create_index(model)
                    .await
                    .context("MongoDB createIndex failed")?;
                QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: 1,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::DropIndex { collection, name } => {
                let coll = self.collection(database, &collection);
                coll.drop_index(&name)
                    .await
                    .context("MongoDB dropIndex failed")?;
                QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: 1,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::GetIndexes { collection } => {
                let coll = self.collection(database, &collection);
                let indexes = coll
                    .list_indexes()
                    .await
                    .context("MongoDB getIndexes failed")?
                    .try_collect::<Vec<_>>()
                    .await
                    .context("Failed to read a MongoDB index definition")?;
                let rows = indexes
                    .into_iter()
                    .map(|index| {
                        let name = index
                            .options
                            .as_ref()
                            .and_then(|options| options.name.clone())
                            .unwrap_or_default();
                        let key_columns = index
                            .keys
                            .iter()
                            .map(|(key, _)| key.clone())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let unique = index
                            .options
                            .as_ref()
                            .and_then(|options| options.unique)
                            .unwrap_or(false);
                        vec![Some(name), Some(key_columns), Some(unique.to_string())]
                    })
                    .collect();
                QueryResult {
                    columns: vec![
                        "name".to_string(),
                        "columns".to_string(),
                        "unique".to_string(),
                    ],
                    rows,
                    rows_affected: 0,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::CollectionStats { collection } => {
                let stats = self
                    .client
                    .database(database)
                    .run_command(doc! { "collStats": collection })
                    .await
                    .context("MongoDB collStats failed")?;
                documents_to_query_result(vec![stats])
            }
            MongoStatement::EstimatedDocumentCount { collection } => {
                let coll = self.collection(database, &collection);
                let count = coll
                    .estimated_document_count()
                    .await
                    .context("MongoDB estimatedDocumentCount failed")?;
                QueryResult {
                    columns: vec!["count".to_string()],
                    rows: vec![vec![Some(count.to_string())]],
                    rows_affected: 0,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::RenameCollection {
                collection,
                new_name,
            } => {
                self.client
                    .database("admin")
                    .run_command(doc! {
                        "renameCollection": format!("{database}.{collection}"),
                        "to": format!("{database}.{new_name}"),
                    })
                    .await
                    .context("MongoDB renameCollection failed")?;
                QueryResult {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: 1,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::Help => QueryResult {
                columns: vec!["supported commands".to_string()],
                rows: vec![vec![Some(SUPPORTED_METHODS.to_string())]],
                rows_affected: 0,
                execution_time_ms: 0,
            },
            MongoStatement::DbStats => {
                let stats = self
                    .client
                    .database(database)
                    .run_command(doc! { "dbStats": 1 })
                    .await
                    .context("MongoDB dbStats failed")?;
                documents_to_query_result(vec![stats])
            }
            MongoStatement::GetCollectionNames | MongoStatement::ShowCollections => {
                let tables = self
                    .list_tables(database)
                    .await
                    .context("MongoDB getCollectionNames failed")?;
                QueryResult {
                    columns: vec!["name".to_string()],
                    rows: tables.into_iter().map(|t| vec![Some(t.name)]).collect(),
                    rows_affected: 0,
                    execution_time_ms: 0,
                }
            }
            MongoStatement::ShowDatabases => {
                let databases = self
                    .list_databases()
                    .await
                    .context("MongoDB show dbs failed")?;
                QueryResult {
                    columns: vec!["name".to_string()],
                    rows: databases.into_iter().map(|d| vec![Some(d.name)]).collect(),
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
    /// schemaless engine. The collection is implicitly created on first use,
    /// so `createCollection` is shown for symmetry with "how would I
    /// recreate this", not because Mongo requires it. Every collection's
    /// automatic `_id_` index is inherent, not something the user created
    /// (the same way `SHOW CREATE TABLE` doesn't list an implicit primary
    /// key as a separate `CREATE INDEX`), so it is omitted here too.
    async fn get_table_ddl(&self, database: &str, table: &str) -> Result<String> {
        let columns = self.describe_table(database, table).await?;
        let coll = self.collection(database, table);
        let count = coll
            .estimated_document_count()
            .await
            .context("Failed to count MongoDB documents")?;
        let indexes = coll
            .list_indexes()
            .await
            .context("Failed to list MongoDB indexes")?
            .try_collect::<Vec<_>>()
            .await
            .context("Failed to read a MongoDB index definition")?;

        let mut summary = format!(
            "-- MongoDB collection \"{}\" is schemaless; there is no CREATE TABLE statement.\n-- {} document(s), sampled schema below:\n",
            table, count
        );
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
        summary.push_str(&format!("\ndb.createCollection(\"{table}\");\n"));
        for index in indexes {
            let index_name = index.options.as_ref().and_then(|o| o.name.as_deref());
            if index_name == Some("_id_") {
                continue;
            }
            summary.push_str(&format_create_index(table, &index));
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
    fn parse_rejects_an_unrecognized_database_level_command_with_a_clear_unsupported_error() {
        let error = parse_mongo_shell_statement("db.eval()").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Unsupported mongo shell command"));
        assert!(message.contains("db.eval()"));
        assert!(message.contains("countDocuments"));
    }

    #[test]
    fn parse_rejects_an_unrecognized_method_on_a_collection() {
        let error = parse_mongo_shell_statement("db.users.mapReduce()").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Unsupported mongo shell command"));
    }

    #[test]
    fn parse_rejects_text_that_does_not_start_with_db() {
        let error = parse_mongo_shell_statement("SELECT * FROM users").unwrap_err();
        assert!(error.to_string().contains("expected a mongo shell command"));
    }

    #[test]
    fn parse_db_help_is_recognized_as_a_read() {
        let statement = parse_mongo_shell_statement("db.help()").unwrap();
        assert_eq!(statement, MongoStatement::Help);
        assert_eq!(statement.kind(), MongoOperationKind::Read);
    }

    #[test]
    fn parse_db_stats_is_recognized() {
        let statement = parse_mongo_shell_statement("db.stats()").unwrap();
        assert_eq!(statement, MongoStatement::DbStats);
    }

    #[test]
    fn parse_db_get_collection_names_is_recognized() {
        let statement = parse_mongo_shell_statement("db.getCollectionNames()").unwrap();
        assert_eq!(statement, MongoStatement::GetCollectionNames);
    }

    #[test]
    fn parse_db_help_rejects_arguments() {
        let error = parse_mongo_shell_statement("db.help(1)").unwrap_err();
        assert!(error.to_string().contains("does not take any arguments"));
    }

    #[test]
    fn parse_rejects_a_chained_call_after_a_non_find_method() {
        let error = parse_mongo_shell_statement("db.users.drop().limit(1)").unwrap_err();
        assert!(error.to_string().contains("unexpected chained call"));
    }

    #[test]
    fn parse_rejects_a_chained_call_after_a_database_level_command() {
        let error = parse_mongo_shell_statement("db.stats().limit(1)").unwrap_err();
        assert!(error.to_string().contains("unexpected chained call"));
    }

    #[test]
    fn parse_show_dbs_and_show_databases_are_recognized() {
        assert_eq!(
            parse_mongo_shell_statement("show dbs").unwrap(),
            MongoStatement::ShowDatabases
        );
        assert_eq!(
            parse_mongo_shell_statement("show databases").unwrap(),
            MongoStatement::ShowDatabases
        );
        assert_eq!(
            parse_mongo_shell_statement("show dbs").unwrap().kind(),
            MongoOperationKind::Read
        );
    }

    #[test]
    fn parse_show_collections_and_show_tables_are_recognized() {
        assert_eq!(
            parse_mongo_shell_statement("show collections").unwrap(),
            MongoStatement::ShowCollections
        );
        assert_eq!(
            parse_mongo_shell_statement("show tables").unwrap(),
            MongoStatement::ShowCollections
        );
    }

    #[test]
    fn parse_distinct_extracts_field_and_optional_filter() {
        let without_filter = parse_mongo_shell_statement("db.users.distinct(\"status\")").unwrap();
        assert_eq!(
            without_filter,
            MongoStatement::Distinct {
                collection: "users".to_string(),
                field: "status".to_string(),
                filter: Document::new(),
            }
        );
        assert_eq!(without_filter.kind(), MongoOperationKind::Read);

        let with_filter =
            parse_mongo_shell_statement("db.users.distinct('status', {active: true})").unwrap();
        assert_eq!(
            with_filter,
            MongoStatement::Distinct {
                collection: "users".to_string(),
                field: "status".to_string(),
                filter: doc! { "active": true },
            }
        );
    }

    #[test]
    fn parse_replace_one_extracts_filter_and_replacement() {
        let statement =
            parse_mongo_shell_statement("db.users.replaceOne({name: 'Ada'}, {name: 'Ada L.'})")
                .unwrap();
        assert_eq!(
            statement,
            MongoStatement::ReplaceOne {
                collection: "users".to_string(),
                filter: doc! { "name": "Ada" },
                replacement: doc! { "name": "Ada L." },
            }
        );
        assert_eq!(statement.kind(), MongoOperationKind::Write);
    }

    #[test]
    fn parse_find_one_and_update_delete_and_replace_variants() {
        let update = parse_mongo_shell_statement(
            "db.users.findOneAndUpdate({name: 'Ada'}, {$set: {active: false}})",
        )
        .unwrap();
        assert_eq!(
            update,
            MongoStatement::FindOneAndUpdate {
                collection: "users".to_string(),
                filter: doc! { "name": "Ada" },
                update: doc! { "$set": { "active": false } },
            }
        );

        let delete = parse_mongo_shell_statement("db.users.findOneAndDelete({name: 'Ada'})")
            .unwrap();
        assert_eq!(
            delete,
            MongoStatement::FindOneAndDelete {
                collection: "users".to_string(),
                filter: doc! { "name": "Ada" },
            }
        );

        let replace = parse_mongo_shell_statement(
            "db.users.findOneAndReplace({name: 'Ada'}, {name: 'Ada L.'})",
        )
        .unwrap();
        assert_eq!(
            replace,
            MongoStatement::FindOneAndReplace {
                collection: "users".to_string(),
                filter: doc! { "name": "Ada" },
                replacement: doc! { "name": "Ada L." },
            }
        );
    }

    #[test]
    fn parse_bulk_write_supports_every_operation_kind() {
        let statement = parse_mongo_shell_statement(
            "db.users.bulkWrite([\
                { insertOne: { document: { name: 'Ada' } } }, \
                { updateOne: { filter: { name: 'Ada' }, update: { $set: { active: true } }, upsert: true } }, \
                { updateMany: { filter: { active: true }, update: { $set: { flag: 1 } } } }, \
                { replaceOne: { filter: { name: 'Ada' }, replacement: { name: 'Ada L.' } } }, \
                { deleteOne: { filter: { name: 'Ada' } } }, \
                { deleteMany: { filter: { active: false } } }\
            ])",
        )
        .unwrap();
        assert_eq!(
            statement,
            MongoStatement::BulkWrite {
                collection: "users".to_string(),
                operations: vec![
                    BulkWriteOp::InsertOne(doc! { "name": "Ada" }),
                    BulkWriteOp::UpdateOne {
                        filter: doc! { "name": "Ada" },
                        update: doc! { "$set": { "active": true } },
                        upsert: true,
                    },
                    BulkWriteOp::UpdateMany {
                        filter: doc! { "active": true },
                        update: doc! { "$set": { "flag": 1i64 } },
                        upsert: false,
                    },
                    BulkWriteOp::ReplaceOne {
                        filter: doc! { "name": "Ada" },
                        replacement: doc! { "name": "Ada L." },
                        upsert: false,
                    },
                    BulkWriteOp::DeleteOne(doc! { "name": "Ada" }),
                    BulkWriteOp::DeleteMany(doc! { "active": false }),
                ],
            }
        );
        assert_eq!(statement.kind(), MongoOperationKind::Write);
    }

    #[test]
    fn parse_bulk_write_rejects_an_operation_with_more_than_one_key() {
        let error = parse_mongo_shell_statement(
            "db.users.bulkWrite([{ insertOne: { document: {} }, deleteOne: { filter: {} } }])",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exactly one key naming the operation")
        );
    }

    #[test]
    fn parse_bulk_write_rejects_an_unsupported_operation_name() {
        let error =
            parse_mongo_shell_statement("db.users.bulkWrite([{ renameOp: { filter: {} } }])")
                .unwrap_err();
        assert!(error.to_string().contains("unsupported bulkWrite operation"));
    }

    #[test]
    fn parse_drop_takes_no_arguments() {
        let statement = parse_mongo_shell_statement("db.users.drop()").unwrap();
        assert_eq!(
            statement,
            MongoStatement::Drop {
                collection: "users".to_string(),
            }
        );
        assert_eq!(statement.kind(), MongoOperationKind::Write);

        let error = parse_mongo_shell_statement("db.users.drop(1)").unwrap_err();
        assert!(error.to_string().contains("does not take any arguments"));
    }

    #[test]
    fn parse_create_index_extracts_keys_and_optional_options() {
        let without_options =
            parse_mongo_shell_statement("db.users.createIndex({email: 1})").unwrap();
        assert_eq!(
            without_options,
            MongoStatement::CreateIndex {
                collection: "users".to_string(),
                keys: doc! { "email": 1i64 },
                options: Document::new(),
            }
        );
        assert_eq!(without_options.kind(), MongoOperationKind::Write);

        let with_options = parse_mongo_shell_statement(
            "db.users.createIndex({email: 1}, {unique: true, name: 'email_idx'})",
        )
        .unwrap();
        assert_eq!(
            with_options,
            MongoStatement::CreateIndex {
                collection: "users".to_string(),
                keys: doc! { "email": 1i64 },
                options: doc! { "unique": true, "name": "email_idx" },
            }
        );
    }

    #[test]
    fn parse_drop_index_takes_a_bare_string_name() {
        let statement = parse_mongo_shell_statement("db.users.dropIndex(\"email_idx\")").unwrap();
        assert_eq!(
            statement,
            MongoStatement::DropIndex {
                collection: "users".to_string(),
                name: "email_idx".to_string(),
            }
        );
        assert_eq!(statement.kind(), MongoOperationKind::Write);
    }

    #[test]
    fn parse_get_indexes_and_stats_and_estimated_document_count_take_no_arguments() {
        assert_eq!(
            parse_mongo_shell_statement("db.users.getIndexes()").unwrap(),
            MongoStatement::GetIndexes {
                collection: "users".to_string(),
            }
        );
        assert_eq!(
            parse_mongo_shell_statement("db.users.stats()").unwrap(),
            MongoStatement::CollectionStats {
                collection: "users".to_string(),
            }
        );
        let estimated = parse_mongo_shell_statement("db.users.estimatedDocumentCount()").unwrap();
        assert_eq!(
            estimated,
            MongoStatement::EstimatedDocumentCount {
                collection: "users".to_string(),
            }
        );
        assert_eq!(estimated.kind(), MongoOperationKind::Read);
    }

    #[test]
    fn parse_count_is_an_alias_for_count_documents() {
        let statement = parse_mongo_shell_statement("db.users.count({active: true})").unwrap();
        assert_eq!(
            statement,
            MongoStatement::CountDocuments {
                collection: "users".to_string(),
                filter: doc! { "active": true },
            }
        );
    }

    #[test]
    fn parse_rename_collection_takes_a_bare_string_new_name() {
        let statement =
            parse_mongo_shell_statement("db.users.renameCollection(\"people\")").unwrap();
        assert_eq!(
            statement,
            MongoStatement::RenameCollection {
                collection: "users".to_string(),
                new_name: "people".to_string(),
            }
        );
        assert_eq!(statement.kind(), MongoOperationKind::Write);
    }

    #[test]
    fn parse_drop_index_rejects_a_non_string_argument() {
        let error = parse_mongo_shell_statement("db.users.dropIndex(1)").unwrap_err();
        assert!(error.to_string().contains("expected a string argument"));
    }

    #[test]
    fn build_index_options_maps_recognized_keys_only() {
        let options = build_index_options(&doc! {
            "unique": true,
            "name": "email_idx",
            "sparse": true,
            "expireAfterSeconds": 3600i64,
            "partialFilterExpression": { "deleted": false },
            "ignoredOption": "ignored",
        })
        .unwrap();
        assert_eq!(options.unique, Some(true));
        assert_eq!(options.name, Some("email_idx".to_string()));
        assert_eq!(options.sparse, Some(true));
        assert_eq!(options.expire_after, Some(Duration::from_secs(3600)));
        assert_eq!(
            options.partial_filter_expression,
            Some(doc! { "deleted": false })
        );
    }

    #[test]
    fn build_index_options_rejects_a_negative_expire_after_seconds() {
        let error =
            build_index_options(&doc! { "expireAfterSeconds": -1i64 }).unwrap_err();
        assert!(error.to_string().contains("must not be negative"));
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

/// Integration tests against a real MongoDB server.
///
/// Set MONGO_TEST_URL=mongodb://host:port before running, then use
/// `cargo test -p db_client --ignored -- mongo` to execute.
#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::connection::DatabaseDriver;
    use uuid::Uuid;

    fn test_config_from_env() -> Option<ConnectionConfig> {
        let url = std::env::var("MONGO_TEST_URL").ok()?;
        let url = url.strip_prefix("mongodb://")?;
        let (host, port_str) = url.split_once(':').unwrap_or((url, "27017"));
        let port: u16 = port_str.parse().unwrap_or(27017);

        Some(ConnectionConfig {
            id: Uuid::new_v4(),
            label: "test".to_string(),
            driver: DatabaseDriver::MongoDB,
            host: host.to_string(),
            port,
            username: String::new(),
            password: String::new(),
            database: None,
            auto_connect: false,
            ..ConnectionConfig::default()
        })
    }

    #[tokio::test]
    #[ignore]
    async fn get_table_ddl_reconstructs_created_indexes() {
        let config =
            test_config_from_env().expect("MONGO_TEST_URL env var required for integration tests");
        let provider = MongoProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zdbt_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let collection_name = "orders";

        let test_result: Result<()> = async {
            let collection = provider.collection(&database, collection_name);
            collection
                .insert_one(doc! { "customer_id": 1i64, "status": "open" })
                .await
                .context("Failed to seed a document")?;
            collection
                .create_index(
                    mongodb::IndexModel::builder()
                        .keys(doc! { "customer_id": 1 })
                        .build(),
                )
                .await
                .context("Failed to create a plain index")?;
            collection
                .create_index(
                    mongodb::IndexModel::builder()
                        .keys(doc! { "status": 1 })
                        .options(
                            mongodb::options::IndexOptions::builder()
                                .name("status_unique_idx".to_string())
                                .unique(true)
                                .build(),
                        )
                        .build(),
                )
                .await
                .context("Failed to create a unique named index")?;

            let ddl = provider.get_table_ddl(&database, collection_name).await?;

            assert!(
                ddl.contains("db.createCollection(\"orders\");"),
                "expected a createCollection call, got: {ddl}"
            );
            assert!(
                ddl.contains("db.orders.createIndex({ customer_id: 1 }, { name: \"customer_id_1\" });"),
                "expected the plain index (auto-named by the driver), got: {ddl}"
            );
            assert!(
                ddl.contains(
                    "db.orders.createIndex({ status: 1 }, { unique: true, name: \"status_unique_idx\" });"
                ) || ddl.contains(
                    "db.orders.createIndex({ status: 1 }, { name: \"status_unique_idx\", unique: true });"
                ),
                "expected the unique named index, got: {ddl}"
            );
            assert!(
                !ddl.contains("createIndex({ _id: 1 }"),
                "the implicit _id_ index must not be listed as a created index, got: {ddl}"
            );
            Ok(())
        }
        .await;

        provider
            .client
            .database(&database)
            .drop()
            .await
            .expect("Failed to drop scratch database");
        test_result.expect("Test assertions failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_ping() {
        let config =
            test_config_from_env().expect("MONGO_TEST_URL env var required for integration tests");
        let provider = MongoProvider::connect(&config)
            .await
            .expect("Failed to connect");
        provider.ping().await.expect("Ping failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_databases_and_collections_finds_scratch_data() {
        let config =
            test_config_from_env().expect("MONGO_TEST_URL env var required for integration tests");
        let provider = MongoProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zdbt_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let collection_name = "widgets";

        let test_result: Result<()> = async {
            provider
                .collection(&database, collection_name)
                .insert_one(doc! { "name": "bolt" })
                .await
                .context("Failed to seed a document")?;

            let databases = provider.list_databases().await?;
            assert!(databases.iter().any(|d| d.name == database));

            let tables = provider.list_tables(&database).await?;
            assert!(tables.iter().any(|t| t.name == collection_name));
            Ok(())
        }
        .await;

        provider
            .client
            .database(&database)
            .drop()
            .await
            .expect("Failed to drop scratch database");
        test_result.expect("Test assertions failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_insert_update_and_delete_document_lifecycle() {
        let config =
            test_config_from_env().expect("MONGO_TEST_URL env var required for integration tests");
        let provider = MongoProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zdbt_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let collection = "accounts";

        let test_result: Result<()> = async {
            provider
                .execute_query(
                    &database,
                    &format!(r#"db.{collection}.insertOne({{ id: 1, balance: 100 }})"#),
                )
                .await?;
            let after_insert = provider
                .execute_query(
                    &database,
                    &format!(r#"db.{collection}.findOne({{ id: 1 }})"#),
                )
                .await?;
            let balance_col = after_insert
                .columns
                .iter()
                .position(|c| c == "balance")
                .expect("balance column must be present");
            assert_eq!(after_insert.rows[0][balance_col].as_deref(), Some("100"));

            provider
                .execute_query(
                    &database,
                    &format!(
                        r#"db.{collection}.updateOne({{ id: 1 }}, {{ $set: {{ balance: 250 }} }})"#
                    ),
                )
                .await?;
            let after_update = provider
                .execute_query(
                    &database,
                    &format!(r#"db.{collection}.findOne({{ id: 1 }})"#),
                )
                .await?;
            let balance_col = after_update
                .columns
                .iter()
                .position(|c| c == "balance")
                .expect("balance column must be present");
            assert_eq!(after_update.rows[0][balance_col].as_deref(), Some("250"));

            provider
                .execute_query(
                    &database,
                    &format!(r#"db.{collection}.deleteOne({{ id: 1 }})"#),
                )
                .await?;
            let after_delete = provider
                .execute_query(
                    &database,
                    &format!(r#"db.{collection}.findOne({{ id: 1 }})"#),
                )
                .await?;
            assert!(
                after_delete.rows.is_empty(),
                "document should be gone after deleteOne"
            );
            Ok(())
        }
        .await;

        provider
            .client
            .database(&database)
            .drop()
            .await
            .expect("Failed to drop scratch database");
        test_result.expect("Test assertions failed");
    }

    /// The shell-statement parser behind `execute_query` doesn't expose
    /// `updateOne`'s `{ upsert: true }` option, so this drives the
    /// underlying driver collection directly -- same pattern the DDL test
    /// above uses for `create_index`.
    #[tokio::test]
    #[ignore]
    async fn test_upsert_via_update_options() {
        let config =
            test_config_from_env().expect("MONGO_TEST_URL env var required for integration tests");
        let provider = MongoProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zdbt_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let collection_name = "counters";

        let test_result: Result<()> = async {
            let collection = provider.collection(&database, collection_name);
            let upsert_options = mongodb::options::UpdateOptions::builder()
                .upsert(true)
                .build();

            collection
                .update_one(doc! { "name": "clicks" }, doc! { "$inc": { "hits": 1 } })
                .with_options(upsert_options.clone())
                .await
                .context("Failed first upsert (insert path)")?;
            collection
                .update_one(doc! { "name": "clicks" }, doc! { "$inc": { "hits": 1 } })
                .with_options(upsert_options)
                .await
                .context("Failed second upsert (update path)")?;

            let count = collection
                .count_documents(doc! { "name": "clicks" })
                .await
                .context("Failed to count documents")?;
            assert_eq!(count, 1, "upsert must not create a duplicate document");

            let document = collection
                .find_one(doc! { "name": "clicks" })
                .await
                .context("Failed to find the counter")?
                .expect("counter document must exist");
            assert_eq!(
                document.get_i32("hits").ok(),
                Some(2),
                "the second upsert must have taken the update branch, not re-inserted at 1"
            );
            Ok(())
        }
        .await;

        provider
            .client
            .database(&database)
            .drop()
            .await
            .expect("Failed to drop scratch database");
        test_result.expect("Test assertions failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_create_and_query_a_view() {
        let config =
            test_config_from_env().expect("MONGO_TEST_URL env var required for integration tests");
        let provider = MongoProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zdbt_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let source = "items";
        let view = "pricey_items";

        let test_result: Result<()> = async {
            provider
                .collection(&database, source)
                .insert_many([doc! { "price": 150 }, doc! { "price": 50 }])
                .await
                .context("Failed to seed source collection")?;

            provider
                .client
                .database(&database)
                .run_command(doc! {
                    "create": view,
                    "viewOn": source,
                    "pipeline": [doc! { "$match": { "price": { "$gt": 100 } } }],
                })
                .await
                .context("Failed to create view")?;

            let result = provider
                .execute_query(&database, &format!("db.{view}.find({{}})"))
                .await?;
            assert_eq!(
                result.rows.len(),
                1,
                "the view must only show the filtered document"
            );

            provider
                .client
                .database(&database)
                .run_command(doc! { "drop": view })
                .await
                .context("Failed to drop view")?;
            Ok(())
        }
        .await;

        provider
            .client
            .database(&database)
            .drop()
            .await
            .expect("Failed to drop scratch database");
        test_result.expect("Test assertions failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_indexes_finds_created_indexes() {
        let config =
            test_config_from_env().expect("MONGO_TEST_URL env var required for integration tests");
        let provider = MongoProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zdbt_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let collection_name = "orders";

        let test_result: Result<()> = async {
            let collection = provider.collection(&database, collection_name);
            collection
                .insert_one(doc! { "customer_id": 1i64, "status": "open" })
                .await
                .context("Failed to seed a document")?;
            collection
                .create_index(
                    mongodb::IndexModel::builder()
                        .keys(doc! { "customer_id": 1 })
                        .build(),
                )
                .await
                .context("Failed to create a plain index")?;
            collection
                .create_index(
                    mongodb::IndexModel::builder()
                        .keys(doc! { "status": 1 })
                        .options(
                            mongodb::options::IndexOptions::builder()
                                .name("status_unique_idx".to_string())
                                .unique(true)
                                .build(),
                        )
                        .build(),
                )
                .await
                .context("Failed to create a unique named index")?;

            let indexes = provider.list_indexes(&database, collection_name).await?;
            assert_eq!(
                indexes.len(),
                3,
                "expected the implicit _id_ index plus the two created ones, got {indexes:?}"
            );

            let id_index = indexes
                .iter()
                .find(|index| index.name == "_id_")
                .unwrap_or_else(|| panic!("expected the implicit _id_ index among {indexes:?}"));
            assert_eq!(id_index.columns, vec!["_id".to_string()]);

            let plain_index = indexes
                .iter()
                .find(|index| index.name == "customer_id_1")
                .unwrap_or_else(|| panic!("expected the auto-named plain index among {indexes:?}"));
            assert_eq!(plain_index.columns, vec!["customer_id".to_string()]);
            assert!(!plain_index.unique);

            let unique_index = indexes
                .iter()
                .find(|index| index.name == "status_unique_idx")
                .unwrap_or_else(|| panic!("expected the unique named index among {indexes:?}"));
            assert_eq!(unique_index.columns, vec!["status".to_string()]);
            assert!(unique_index.unique);

            Ok(())
        }
        .await;

        provider
            .client
            .database(&database)
            .drop()
            .await
            .expect("Failed to drop scratch database");
        test_result.expect("Test assertions failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_distinct_replace_one_and_find_one_and_x_execute_via_the_shell_parser() {
        let config =
            test_config_from_env().expect("MONGO_TEST_URL env var required for integration tests");
        let provider = MongoProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zdbt_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let collection = "users";

        let test_result: Result<()> = async {
            provider
                .execute_query(
                    &database,
                    &format!(
                        "db.{collection}.insertMany([{{name: 'Ada', status: 'active'}}, {{name: 'Grace', status: 'active'}}, {{name: 'Hopper', status: 'retired'}}])"
                    ),
                )
                .await
                .context("Failed to seed documents")?;

            let distinct = provider
                .execute_query(&database, &format!("db.{collection}.distinct('status')"))
                .await?;
            assert_eq!(distinct.columns, vec!["distinct".to_string()]);
            let mut distinct_values: Vec<String> = distinct
                .rows
                .into_iter()
                .map(|row| row.into_iter().next().flatten().unwrap_or_default())
                .collect();
            distinct_values.sort();
            assert_eq!(distinct_values, vec!["active".to_string(), "retired".to_string()]);

            let replace = provider
                .execute_query(
                    &database,
                    &format!(
                        "db.{collection}.replaceOne({{name: 'Hopper'}}, {{name: 'Hopper', status: 'active'}})"
                    ),
                )
                .await?;
            assert_eq!(replace.rows_affected, 1);

            let found_and_updated = provider
                .execute_query(
                    &database,
                    &format!(
                        "db.{collection}.findOneAndUpdate({{name: 'Ada'}}, {{$set: {{status: 'inactive'}}}})"
                    ),
                )
                .await?;
            assert_eq!(found_and_updated.rows.len(), 1);

            let after_update = provider
                .collection(&database, collection)
                .find_one(doc! { "name": "Ada" })
                .await?
                .expect("expected the Ada document to still exist");
            assert_eq!(after_update.get_str("status"), Ok("inactive"));

            let found_and_deleted = provider
                .execute_query(
                    &database,
                    &format!("db.{collection}.findOneAndDelete({{name: 'Grace'}})"),
                )
                .await?;
            assert_eq!(found_and_deleted.rows.len(), 1);
            assert!(
                provider
                    .collection(&database, collection)
                    .find_one(doc! { "name": "Grace" })
                    .await?
                    .is_none(),
                "expected Grace to have been deleted"
            );

            Ok(())
        }
        .await;

        provider
            .client
            .database(&database)
            .drop()
            .await
            .expect("Failed to drop scratch database");
        test_result.expect("Test assertions failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_bulk_write_executes_every_operation_kind_sequentially() {
        let config =
            test_config_from_env().expect("MONGO_TEST_URL env var required for integration tests");
        let provider = MongoProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zdbt_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let collection = "widgets";

        let test_result: Result<()> = async {
            provider
                .execute_query(
                    &database,
                    &format!(
                        "db.{collection}.insertMany([{{name: 'bolt', qty: 1}}, {{name: 'nut', qty: 1}}, {{name: 'washer', qty: 1}}])"
                    ),
                )
                .await
                .context("Failed to seed documents")?;

            let result = provider
                .execute_query(
                    &database,
                    &format!(
                        "db.{collection}.bulkWrite([\
                            {{ insertOne: {{ document: {{ name: 'screw', qty: 1 }} }} }}, \
                            {{ updateOne: {{ filter: {{ name: 'bolt' }}, update: {{ $set: {{ qty: 5 }} }} }} }}, \
                            {{ updateMany: {{ filter: {{ qty: 1 }}, update: {{ $set: {{ tagged: true }} }} }} }}, \
                            {{ deleteOne: {{ filter: {{ name: 'washer' }} }} }}, \
                            {{ replaceOne: {{ filter: {{ name: 'nut' }}, replacement: {{ name: 'nut', qty: 9 }} }} }}, \
                            {{ updateOne: {{ filter: {{ name: 'does-not-exist' }}, update: {{ $set: {{ qty: 1 }} }}, upsert: true }} }}\
                        ])"
                    ),
                )
                .await?;
            // insertOne (1) + updateOne on bolt (1) + updateMany over the 3
            // qty:1 docs at that point: nut, washer, screw (3) + deleteOne
            // (1) + replaceOne (1) + upserting updateOne, which modifies 0
            // existing documents (0) = 7.
            assert_eq!(result.rows_affected, 7);

            let coll = provider.collection(&database, collection);
            assert_eq!(
                coll.find_one(doc! { "name": "bolt" }).await?.and_then(|d| d.get_i32("qty").ok()),
                Some(5)
            );
            assert!(coll.find_one(doc! { "name": "washer" }).await?.is_none());
            assert_eq!(
                coll.find_one(doc! { "name": "nut" }).await?.and_then(|d| d.get_i32("qty").ok()),
                Some(9)
            );
            assert!(coll.find_one(doc! { "name": "screw" }).await?.is_some());
            assert!(
                coll.find_one(doc! { "name": "does-not-exist" }).await?.is_some(),
                "expected the upsert to have inserted a new document"
            );

            Ok(())
        }
        .await;

        provider
            .client
            .database(&database)
            .drop()
            .await
            .expect("Failed to drop scratch database");
        test_result.expect("Test assertions failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_drop_create_index_drop_index_and_get_indexes_execute_via_the_shell_parser() {
        let config =
            test_config_from_env().expect("MONGO_TEST_URL env var required for integration tests");
        let provider = MongoProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zdbt_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let collection = "accounts";

        let test_result: Result<()> = async {
            provider
                .execute_query(
                    &database,
                    &format!("db.{collection}.insertOne({{email: 'ada@example.com'}})"),
                )
                .await
                .context("Failed to seed a document")?;

            provider
                .execute_query(
                    &database,
                    &format!(
                        "db.{collection}.createIndex({{email: 1}}, {{unique: true, name: 'email_idx'}})"
                    ),
                )
                .await?;

            let indexes = provider
                .execute_query(&database, &format!("db.{collection}.getIndexes()"))
                .await?;
            assert!(
                indexes
                    .rows
                    .iter()
                    .any(|row| row.first() == Some(&Some("email_idx".to_string()))),
                "expected the created index among {:?}",
                indexes.rows
            );

            provider
                .execute_query(&database, &format!("db.{collection}.dropIndex('email_idx')"))
                .await?;
            let indexes_after_drop = provider
                .execute_query(&database, &format!("db.{collection}.getIndexes()"))
                .await?;
            assert!(
                !indexes_after_drop
                    .rows
                    .iter()
                    .any(|row| row.first() == Some(&Some("email_idx".to_string()))),
                "expected the dropped index to be gone, got {:?}",
                indexes_after_drop.rows
            );

            let dropped = provider
                .execute_query(&database, &format!("db.{collection}.drop()"))
                .await?;
            assert_eq!(dropped.rows_affected, 1);
            let remaining_collections = provider.list_tables(&database).await?;
            assert!(
                !remaining_collections.iter().any(|t| t.name == collection),
                "expected the collection to have been dropped"
            );

            Ok(())
        }
        .await;

        provider
            .client
            .database(&database)
            .drop()
            .await
            .expect("Failed to drop scratch database");
        test_result.expect("Test assertions failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_count_estimated_count_and_rename_collection_execute_via_the_shell_parser() {
        let config =
            test_config_from_env().expect("MONGO_TEST_URL env var required for integration tests");
        let provider = MongoProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zdbt_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let collection = "orders";
        let renamed_collection = "purchase_orders";

        let test_result: Result<()> = async {
            provider
                .execute_query(
                    &database,
                    &format!(
                        "db.{collection}.insertMany([{{status: 'open'}}, {{status: 'open'}}, {{status: 'closed'}}])"
                    ),
                )
                .await
                .context("Failed to seed documents")?;

            let count = provider
                .execute_query(&database, &format!("db.{collection}.count({{status: 'open'}})"))
                .await?;
            assert_eq!(count.rows, vec![vec![Some("2".to_string())]]);

            let estimated = provider
                .execute_query(&database, &format!("db.{collection}.estimatedDocumentCount()"))
                .await?;
            assert_eq!(estimated.rows, vec![vec![Some("3".to_string())]]);

            provider
                .execute_query(
                    &database,
                    &format!("db.{collection}.renameCollection('{renamed_collection}')"),
                )
                .await?;
            let tables = provider.list_tables(&database).await?;
            assert!(tables.iter().any(|t| t.name == renamed_collection));
            assert!(!tables.iter().any(|t| t.name == collection));

            Ok(())
        }
        .await;

        provider
            .client
            .database(&database)
            .drop()
            .await
            .expect("Failed to drop scratch database");
        test_result.expect("Test assertions failed");
    }

    #[tokio::test]
    #[ignore]
    async fn test_help_stats_and_show_helpers_execute_via_the_shell_parser() {
        let config =
            test_config_from_env().expect("MONGO_TEST_URL env var required for integration tests");
        let provider = MongoProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zdbt_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let collection = "orders";

        let test_result: Result<()> = async {
            provider
                .execute_query(&database, &format!("db.{collection}.insertOne({{status: 'open'}})"))
                .await
                .context("Failed to seed a document")?;

            let help = provider.execute_query(&database, "db.help()").await?;
            assert!(!help.rows.is_empty());

            let collection_stats = provider
                .execute_query(&database, &format!("db.{collection}.stats()"))
                .await?;
            assert!(!collection_stats.columns.is_empty());

            let db_stats = provider.execute_query(&database, "db.stats()").await?;
            assert!(!db_stats.columns.is_empty());

            let get_collection_names = provider
                .execute_query(&database, "db.getCollectionNames()")
                .await?;
            assert!(
                get_collection_names
                    .rows
                    .iter()
                    .any(|row| row.first() == Some(&Some(collection.to_string())))
            );

            let show_collections = provider.execute_query(&database, "show collections").await?;
            assert!(
                show_collections
                    .rows
                    .iter()
                    .any(|row| row.first() == Some(&Some(collection.to_string())))
            );

            let show_dbs = provider.execute_query(&database, "show dbs").await?;
            assert!(
                show_dbs
                    .rows
                    .iter()
                    .any(|row| row.first() == Some(&Some(database.clone())))
            );

            Ok(())
        }
        .await;

        provider
            .client
            .database(&database)
            .drop()
            .await
            .expect("Failed to drop scratch database");
        test_result.expect("Test assertions failed");
    }
}
