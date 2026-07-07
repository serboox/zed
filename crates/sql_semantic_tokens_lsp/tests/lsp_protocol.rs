//! Exercises the real stdio LSP transport (not just the internal tokenizer
//! function) by spawning the compiled binary and speaking the protocol over
//! its stdin/stdout, exactly as Zed would.

use std::process::Stdio;

use serde_json::{Value, json};
use smol::io::{AsyncReadExt, AsyncWriteExt};
use smol::process::{Child, ChildStdin, ChildStdout, Command};

struct LspClient {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    next_id: i64,
}

impl LspClient {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sql_semantic_tokens_lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("failed to spawn sql_semantic_tokens_lsp");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    async fn write_message(&mut self, value: &Value) {
        let body = serde_json::to_vec(value).expect("serialize LSP message");
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin
            .write_all(header.as_bytes())
            .await
            .expect("write header");
        self.stdin.write_all(&body).await.expect("write body");
        self.stdin.flush().await.expect("flush stdin");
    }

    async fn read_message(&mut self) -> Value {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let mut byte = [0u8; 1];
            loop {
                self.stdout
                    .read_exact(&mut byte)
                    .await
                    .expect("read header byte");
                line.push(byte[0] as char);
                if line.ends_with("\r\n") {
                    break;
                }
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                content_length = Some(value.trim().parse().expect("parse Content-Length"));
            }
        }
        let content_length = content_length.expect("response missing Content-Length");
        let mut body = vec![0u8; content_length];
        self.stdout
            .read_exact(&mut body)
            .await
            .expect("read response body");
        serde_json::from_slice(&body).expect("parse response JSON")
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;
        let response = self.read_message().await;
        assert_eq!(response["id"], id, "response id must match the request id");
        response["result"].clone()
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await;
    }

    async fn shutdown_and_exit(mut self) {
        self.request("shutdown", Value::Null).await;
        self.notify("exit", Value::Null).await;
        let status = self.child.status().await.expect("wait for child exit");
        assert!(
            status.success(),
            "server should exit 0 after a clean shutdown"
        );
    }
}

/// Decodes the LSP semantic tokens relative-delta wire format back into
/// `(line, start_char, length, token_type_index)` tuples in absolute
/// coordinates, so the test can assert on real positions.
fn decode_relative(data: &[u64]) -> Vec<(u64, u64, u64, u64)> {
    let mut tokens = Vec::new();
    let mut line = 0u64;
    let mut start_char = 0u64;
    for chunk in data.chunks_exact(5) {
        let [delta_line, delta_start, length, token_type, _modifiers] = chunk else {
            unreachable!("chunks_exact(5) always yields slices of length 5")
        };
        if *delta_line == 0 {
            start_char += delta_start;
        } else {
            line += delta_line;
            start_char = *delta_start;
        }
        tokens.push((line, start_char, *length, *token_type));
    }
    tokens
}

const BUG_REPORT_SQL: &str = "SELECT *\nFROM app_push_report.app_content_mod_stats\nWHERE row_ID =\\''.$content_res['row_ID'].'\\' AND mod_int=\\''.$user_modulo.'\\' AND os=\\'android\\';';\n\nSELECT *\nFROM app_push_report.app_content_mod_stats;\n";

#[test]
fn semantic_tokens_full_over_real_stdio_transport_flags_second_select_as_keyword() {
    smol::block_on(async {
        let mut client = LspClient::spawn();

        let initialize_result = client
            .request(
                "initialize",
                json!({ "processId": null, "rootUri": null, "capabilities": {} }),
            )
            .await;
        let legend =
            initialize_result["capabilities"]["semanticTokensProvider"]["legend"]["tokenTypes"]
                .as_array()
                .expect("server must advertise a semantic tokens legend")
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .expect("legend entries are strings")
                        .to_string()
                })
                .collect::<Vec<_>>();
        let keyword_index = legend
            .iter()
            .position(|name| name == "keyword")
            .expect("legend must contain a 'keyword' token type")
            as u64;

        client.notify("initialized", json!({})).await;

        let uri = "file:///bug-report.sql";
        client
            .notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": "sql",
                        "version": 1,
                        "text": BUG_REPORT_SQL,
                    }
                }),
            )
            .await;

        let result = client
            .request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": uri } }),
            )
            .await;
        let data: Vec<u64> = result["data"]
            .as_array()
            .expect("semanticTokens/full must return a data array")
            .iter()
            .map(|value| value.as_u64().expect("token data entries are integers"))
            .collect();
        let tokens = decode_relative(&data);

        // The second `SELECT` sits on line 4 (0-based): line 0 is the first
        // SELECT, line 1 FROM, line 2 the malformed WHERE clause, line 3 blank.
        let second_select_is_keyword = tokens
            .iter()
            .any(|&(line, _start, _length, token_type)| line == 4 && token_type == keyword_index);
        assert!(
            second_select_is_keyword,
            "expected a keyword token on line 4 (the second SELECT), got tokens: {tokens:?} \
             with keyword index {keyword_index}"
        );

        client.shutdown_and_exit().await;
    });
}
