//! Minimal LSP server exposing only `textDocument/semanticTokens/full` for
//! SQL buffers, using a MySQL-aware tokenizer that correctly understands
//! backslash-escaped quotes (`\'`) inside string literals — unlike the
//! bundled tree-sitter-sql grammar, which only recognizes `''`-doubling and
//! Postgres `E'...'` escapes. Zed's editor renders semantic token highlights
//! on top of (and overriding) the tree-sitter-derived colors for the same
//! byte ranges, so this fixes highlighting without touching the grammar.
//!
//! The wire format is hand-rolled `Content-Length`-framed JSON-RPC 2.0
//! rather than depending on the `lsp-server`/`lsp-types` crates, to avoid
//! pulling in a second `lsp-types` dependency alongside Zed's own forked
//! version already pinned in this workspace.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sql_semantic_tokens_lsp::{
    SemanticTokenKind, connection_driver_for_document, encode_relative, tokenize,
};

fn main() -> Result<()> {
    let stdin = io::stdin();
    let mut reader = io::BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    let mut documents: HashMap<String, String> = HashMap::new();
    let mut shutdown_requested = false;

    loop {
        let Some(message) = read_message(&mut reader)? else {
            break;
        };

        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();

        match method {
            Some("initialize") => {
                write_response(&mut writer, id, json!({ "capabilities": capabilities() }))?;
            }
            Some("initialized") => {}
            Some("textDocument/didOpen") => {
                if let Some((uri, text)) = text_document_item(&message, "textDocument") {
                    documents.insert(uri, text);
                }
            }
            Some("textDocument/didChange") => {
                if let Some(uri) = document_uri(&message)
                    && let Some(text) = full_sync_text(&message)
                {
                    documents.insert(uri, text);
                }
            }
            Some("textDocument/didClose") => {
                if let Some(uri) = document_uri(&message) {
                    documents.remove(&uri);
                }
            }
            Some("textDocument/semanticTokens/full") => {
                let uri = document_uri(&message);
                let text = uri
                    .as_ref()
                    .and_then(|uri| documents.get(uri))
                    .map(String::as_str)
                    .unwrap_or("");
                let is_cql = uri
                    .as_deref()
                    .and_then(connection_driver_for_document)
                    .as_deref()
                    == Some("Cassandra");
                let raw_tokens = tokenize(text, is_cql);
                let data = encode_relative(&raw_tokens);
                write_response(&mut writer, id, json!({ "data": data }))?;
            }
            Some("shutdown") => {
                shutdown_requested = true;
                write_response(&mut writer, id, Value::Null)?;
            }
            Some("exit") => {
                std::process::exit(if shutdown_requested { 0 } else { 1 });
            }
            Some(_) if id.is_some() => {
                // An unhandled request still needs a response, or the client
                // (Zed) would wait for one indefinitely.
                write_error_response(&mut writer, id, -32601, "method not found")?;
            }
            _ => {
                // Unhandled notification: nothing to respond to.
            }
        }
    }

    Ok(())
}

fn capabilities() -> Value {
    json!({
        "textDocumentSync": 1, // Full
        "semanticTokensProvider": {
            "legend": {
                "tokenTypes": SemanticTokenKind::LEGEND,
                "tokenModifiers": [],
            },
            "full": true,
        },
    })
}

fn document_uri(message: &Value) -> Option<String> {
    message
        .get("params")?
        .get("textDocument")?
        .get("uri")?
        .as_str()
        .map(str::to_string)
}

fn text_document_item(message: &Value, container: &str) -> Option<(String, String)> {
    let item = message.get("params")?.get(container)?;
    let uri = item.get("uri")?.as_str()?.to_string();
    let text = item.get("text")?.as_str()?.to_string();
    Some((uri, text))
}

/// Extracts the replacement text for a full-document `didChange`
/// notification (this server advertises `TextDocumentSyncKind::Full`, so
/// there is always exactly one `contentChanges` entry with no `range`).
fn full_sync_text(message: &Value) -> Option<String> {
    message
        .get("params")?
        .get("contentChanges")?
        .as_array()?
        .last()?
        .get("text")?
        .as_str()
        .map(str::to_string)
}

/// Reads one `Content-Length`-framed JSON-RPC message from `reader`.
/// Returns `Ok(None)` on a clean EOF (the client closed stdin).
fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut header_line = String::new();
        let bytes_read = reader
            .read_line(&mut header_line)
            .context("reading LSP header line")?;
        if bytes_read == 0 {
            return Ok(None);
        }
        let header_line = header_line.trim_end();
        if header_line.is_empty() {
            break;
        }
        if let Some(value) = header_line.strip_prefix("Content-Length:") {
            content_length = Some(
                value
                    .trim()
                    .parse()
                    .context("parsing Content-Length header")?,
            );
        }
    }

    let content_length = content_length.context("LSP message missing Content-Length header")?;
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .context("reading LSP message body")?;
    let value = serde_json::from_slice(&body).context("parsing LSP message body as JSON")?;
    Ok(Some(value))
}

fn write_message<W: Write>(writer: &mut W, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value).context("serializing LSP message")?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn write_response<W: Write>(writer: &mut W, id: Option<Value>, result: Value) -> Result<()> {
    let Some(id) = id else {
        // A response with no request id would be malformed; the caller only
        // reaches here for methods that are requests (always carry an id).
        bail!("attempted to write a response with no request id");
    };
    write_message(
        writer,
        &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    )
}

fn write_error_response<W: Write>(
    writer: &mut W,
    id: Option<Value>,
    code: i64,
    message: &str,
) -> Result<()> {
    let Some(id) = id else {
        bail!("attempted to write an error response with no request id");
    };
    write_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    )
}
