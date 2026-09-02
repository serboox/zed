use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use smol::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
use smol::process::{Child, ChildStdin, ChildStdout};

use crate::definitions::Definition;

/// How long to wait for `initialize` to answer. Answering it does not wait for
/// indexing -- rust-analyzer replies with its capabilities right away and
/// reports indexing separately, through `$/progress`.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for `shutdown` to answer, and then for the process itself
/// to exit after `exit` is sent.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long of no progress activity, once at least one progress sequence has
/// opened and closed, before indexing is called finished. rust-analyzer's
/// startup can run more than one progress sequence back to back (fetching the
/// workspace, then indexing it), so the end of the first one is not enough.
const QUIET_PERIOD_AFTER_INDEXING: Duration = Duration::from_secs(2);

/// A symbol's identity for the purpose of this comparison: the index and the
/// server describe a definition very differently -- a grammar node kind on one
/// side, an LSP `SymbolKind` number on the other -- but they can agree on
/// where it is and what it is called, and that is the only thing that has to
/// agree for the two to be counted as the same finding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Identity {
    pub path: String,
    pub line: u32,
    pub name: String,
}

/// An error the server answered a request with, kept as its own type so a
/// caller can tell one refusal apart from another instead of reading a
/// formatted message.
#[derive(Debug, Clone)]
pub struct ServerRefused {
    pub code: i64,
    pub message: String,
}

impl ServerRefused {
    fn from(error: &Value) -> Self {
        Self {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message")
                .to_string(),
        }
    }

    /// Whether the refusal is "I have never heard of this file". The server's
    /// view of a project is the cargo graph; anything outside it -- a vendored
    /// copy, a scratch file, generated output -- is a file it was never going
    /// to answer about, which is a different thing from a request that failed.
    pub fn is_a_file_the_server_does_not_have(&self) -> bool {
        self.message.starts_with("file not found")
    }
}

impl std::fmt::Display for ServerRefused {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            out,
            "rust-analyzer answered with an error {}: {}",
            self.code, self.message
        )
    }
}

impl std::error::Error for ServerRefused {}

/// When the server does its thinking.
///
/// Priming ahead is the server's own default and what a person editing the
/// project gets: it works the whole workspace out before the first question, so
/// every answer afterwards is quick. It also holds all of that at once, and on
/// a project with this crate graph that does not fit in the machine the
/// measurements run on.
///
/// Lazily is the other end: nothing is computed until something is asked, so
/// the run fits, but the first question pays for what the priming would have
/// done -- which a stand with a short per-query limit will read as a timeout.
///
/// Neither is more honest than the other; they answer the same questions with
/// the same information. Each stand picks the end it can afford.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priming {
    Ahead,
    Lazily,
}

/// How many queries rust-analyzer may keep cached. Read from `RA_LRU_CAP` when
/// it is already set, so a run can widen or remove the bound deliberately.
pub fn lru_capacity() -> u32 {
    std::env::var("RA_LRU_CAP")
        .ok()
        .and_then(|set| set.parse().ok())
        .unwrap_or(DEFAULT_LRU_CAPACITY)
}

/// rust-analyzer's own default is 128. This is lower because the measuring
/// machine has 18 GiB and the default does not fit in it.
const DEFAULT_LRU_CAPACITY: u32 = 16;

impl Identity {
    pub fn of(definition: &Definition) -> Self {
        Self {
            path: definition.path.clone(),
            line: definition.line,
            name: definition.name.clone(),
        }
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}:{} {}", self.path, self.line, self.name)
    }
}

/// Converts an absolute `file://` URI, as rust-analyzer reports it, into the
/// index's own path form: relative to `root`, forward slashes. `None` for
/// anything that is not a `file://` URI under `root` at all -- a dependency, a
/// standard library source, or anything else outside the project the index
/// itself never sees and has no business being compared against.
pub fn relative_to_root(uri: &str, root: &Path) -> Option<String> {
    let absolute = url::Url::parse(uri).ok()?.to_file_path().ok()?;
    let inside = absolute.strip_prefix(root).ok()?;
    let relative = inside.to_string_lossy().replace('\\', "/");
    (!relative.is_empty()).then_some(relative)
}

/// Picks up to `count` of `names` as queries, at an even stride through the
/// list, so the sample is the same on every run over the same catalogue.
///
/// A full symbol name is used as the query, not a short prefix: this
/// comparison is about whether the two sides agree on the same definition, and
/// a full name keeps each side's own result count small, which keeps a
/// server- or index-side result cap from being what a divergence is actually
/// measuring.
pub fn sample_queries(names: &[String], count: usize) -> Vec<String> {
    if names.is_empty() || count == 0 {
        return Vec::new();
    }
    let stride = (names.len() / count).max(1);
    let mut picked = Vec::with_capacity(count.min(names.len()));
    let mut already_picked: HashSet<&str> = HashSet::new();
    let mut at = 0;
    while picked.len() < count && at < names.len() {
        let name = names[at].as_str();
        if already_picked.insert(name) {
            picked.push(name.to_string());
        }
        at += stride;
    }
    picked
}

/// Writes one LSP message: the `Content-Length` header, then the JSON body.
/// The one place the wire format is written, so every message goes out built
/// the same way.
pub async fn write_message(
    to: &mut (impl smol::io::AsyncWrite + Unpin),
    body: &[u8],
) -> std::io::Result<()> {
    to.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    to.write_all(body).await?;
    to.flush().await
}

/// Reads one LSP message: the headers up to the blank line, then exactly
/// `Content-Length` bytes of body. `Ok(None)` only for a clean end of stream
/// before any byte of a new message has arrived; an end of stream in the
/// middle of one is a truncated message, not a graceful close, and is an
/// error.
pub async fn read_message(
    from: &mut (impl smol::io::AsyncBufRead + Unpin),
) -> std::io::Result<Option<Vec<u8>>> {
    let mut content_length: Option<usize> = None;
    let mut first_line = true;
    loop {
        let mut line = String::new();
        let read = from.read_line(&mut line).await?;
        if read == 0 {
            if first_line {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the connection closed in the middle of a message's headers",
            ));
        }
        first_line = false;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().ok();
        }
    }
    let content_length = content_length.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "a message with no Content-Length header",
        )
    })?;
    let mut body = vec![0u8; content_length];
    from.read_exact(&mut body).await?;
    Ok(Some(body))
}

/// What arrived from rust-analyzer's stdout within a bounded wait: a message,
/// a clean close of the stream, or nothing within the time given.
enum Arrived {
    Message(Vec<u8>),
    ClosedByServer,
    Nothing,
}

#[derive(Deserialize)]
struct IncomingMessage {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

/// Tracks rust-analyzer's own work-done-progress tokens generically, by
/// whether each one has opened and closed -- not by any particular title, so
/// a version of rust-analyzer that renames or reshuffles its startup phases
/// does not silently break the wait.
#[derive(Default)]
struct ProgressTracker {
    open: HashSet<String>,
    ever_opened: bool,
    last_activity: Option<Instant>,
}

impl ProgressTracker {
    fn opened(&mut self, token: &Value) {
        if let Some(token) = token_key(token) {
            self.open.insert(token);
        }
        self.ever_opened = true;
        self.last_activity = Some(Instant::now());
    }

    fn on_progress(&mut self, params: Option<&Value>) {
        let Some(params) = params else { return };
        let Some(token) = params.get("token").and_then(token_key) else {
            return;
        };
        match params
            .get("value")
            .and_then(|value| value.get("kind"))
            .and_then(Value::as_str)
        {
            Some("begin") => {
                self.open.insert(token);
                self.ever_opened = true;
            }
            Some("end") => {
                self.open.remove(&token);
            }
            _ => {}
        }
        self.last_activity = Some(Instant::now());
    }

    /// Whether the server has reported any work at all yet. Told apart from
    /// "finished" so a wait that times out can say which of the two happened.
    fn has_started(&self) -> bool {
        self.ever_opened
    }

    fn finished_indexing(&self, quiet_for: Duration) -> bool {
        self.ever_opened
            && self.open.is_empty()
            && self
                .last_activity
                .is_some_and(|when| when.elapsed() >= quiet_for)
    }
}

fn token_key(token: &Value) -> Option<String> {
    match token {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The standard LSP `SymbolKind` numbering, for a readable report only -- it
/// plays no part in matching a server answer to an index one.
fn lsp_symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum_member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type_parameter",
        _ => "unknown",
    }
}

/// A running rust-analyzer process, spoken to over its own stdio, the same
/// protocol the editor itself uses.
///
/// The framing and the message loop are hand-rolled here rather than built on
/// the workspace's own `lsp::LanguageServer`: that type is written against a
/// running `gpui::AsyncApp` and spawns its plumbing through it, which would
/// mean pulling gpui, lsp-types and their dependents into a standalone
/// measurement binary that otherwise has none of them, just to run one
/// initialize handshake and a couple hundred requests. A small, obviously
/// correct framer, tested on its own, is the simpler choice here.
pub struct Server {
    stdin: ChildStdin,
    stdout: smol::io::BufReader<ChildStdout>,
    child: Child,
    root: PathBuf,
    next_id: u64,
    progress: ProgressTracker,
}

impl Server {
    /// Starts rust-analyzer over `root` and carries out the `initialize` /
    /// `initialized` handshake. Indexing itself is not waited for here --
    /// call [`Server::wait_until_indexed`] before trusting any answer.
    pub async fn start(root: &Path, priming: Priming) -> Result<Self> {
        let binary = which::which("rust-analyzer").context(
            "rust-analyzer is not on PATH; install it (for example `rustup component add \
             rust-analyzer`) before measuring recall against it",
        )?;

        let mut child = smol::process::Command::new(&binary)
            .current_dir(root)
            // Bound the server's query cache. Left alone, answering reference
            // queries across a project this size grows past 18 GiB and the
            // machine kills it, which measures the machine rather than the
            // server. Whatever this is set to is printed with the numbers, so
            // a constrained run never reads as an unconstrained one.
            .env("RA_LRU_CAP", lru_capacity().to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Not read: a short-lived comparison run has no use for
            // rust-analyzer's own logging, and leaving stderr piped but
            // undrained risks the pipe filling and rust-analyzer blocking on
            // a write to it.
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", binary.display()))?;

        let stdin = child
            .stdin
            .take()
            .context("rust-analyzer's stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .context("rust-analyzer's stdout was not piped")?;

        let mut server = Self {
            stdin,
            stdout: smol::io::BufReader::new(stdout),
            child,
            root: root.to_path_buf(),
            next_id: 1,
            progress: ProgressTracker::default(),
        };

        let root_uri = url::Url::from_directory_path(root).map_err(|()| {
            anyhow::anyhow!(
                "{} is not an absolute path rust-analyzer can use as a workspace root",
                root.display()
            )
        })?;
        let folder_name = root
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("workspace");

        server
            .call(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri.as_str(),
                    "capabilities": {"window": {"workDoneProgress": true}},
                    "workspaceFolders": [{"uri": root_uri.as_str(), "name": folder_name}],
                    // Left at their defaults on purpose: which files the server
                    // can answer about, and what it considers a reference, must
                    // be exactly what a person editing this project would get.
                    // Only what it computes *ahead of being asked* is changed.
                    "initializationOptions": {
                        // Whether the server works the whole project out before
                        // the first question or as each one arrives. See
                        // [`Priming`]: it is a trade of memory against the cost
                        // of the first answer, and the two stands want opposite
                        // ends of it.
                        "cachePriming": {"enable": matches!(priming, Priming::Ahead)},
                        "lru": {"capacity": lru_capacity()},
                    },
                }),
                INITIALIZE_TIMEOUT,
            )
            .await
            .context("rust-analyzer did not answer `initialize`")?;
        server
            .notify("initialized", json!({}))
            .await
            .context("telling rust-analyzer it is initialized")?;
        Ok(server)
    }

    /// Waits until every work-done-progress sequence rust-analyzer has opened
    /// on its own has also closed, and stayed quiet for a short grace period.
    ///
    /// This is deliberately not looking for one specific title such as
    /// "Indexing": rust-analyzer's own startup can run more than one such
    /// sequence (fetching the workspace, then indexing it), and a version that
    /// renames or reorders them should not silently break the wait. Answering
    /// `workspace/symbol` before this returns would be answering it from a
    /// partially built index -- exactly the failure this whole comparison
    /// exists to catch, since it would make the index look better than it is
    /// by making the server look worse than it is.
    pub async fn wait_until_indexed(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut said_anything = false;
        loop {
            if self.progress.finished_indexing(QUIET_PERIOD_AFTER_INDEXING) {
                return Ok(());
            }
            said_anything |= self.progress.has_started();
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "rust-analyzer {} within {timeout:?}; the timeout ended the wait, not the \
                 server -- treat this run as inconclusive, not a pass",
                if said_anything {
                    "did not finish indexing"
                } else {
                    "never reported any work at all"
                }
            );
            self.pump_one(remaining.min(QUIET_PERIOD_AFTER_INDEXING))
                .await?;
        }
    }

    /// Asks `workspace/symbol` for `query` and returns the answers that fall
    /// under the project root, in the index's own `Definition` shape. A
    /// result outside the root -- a dependency, the standard library -- is
    /// silently left out: it is not something the index could ever have
    /// found, and counting it would measure nothing real.
    pub async fn workspace_symbol(
        &mut self,
        query: &str,
        timeout: Duration,
    ) -> Result<Vec<Definition>> {
        let answered = self
            .call("workspace/symbol", json!({"query": query}), timeout)
            .await
            .with_context(|| format!("asking rust-analyzer for `{query}`"))?;
        let symbols = match answered {
            Value::Null => Vec::new(),
            Value::Array(symbols) => symbols,
            other => anyhow::bail!("`workspace/symbol` answered with {other}, not a list"),
        };

        let mut found = Vec::new();
        for symbol in &symbols {
            let Some(uri) = symbol.pointer("/location/uri").and_then(Value::as_str) else {
                continue;
            };
            let Some(path) = relative_to_root(uri, &self.root) else {
                continue;
            };
            let Some(name) = symbol.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(line) = symbol
                .pointer("/location/range/start/line")
                .and_then(Value::as_u64)
            else {
                continue;
            };
            found.push(Definition {
                path,
                name: name.to_string(),
                kind: symbol
                    .get("kind")
                    .and_then(Value::as_u64)
                    .map(lsp_symbol_kind_name)
                    .unwrap_or_default()
                    .to_string(),
                // The LSP protocol counts lines from zero; the index from one.
                line: line as u32 + 1,
                language: "rust".to_string(),
            });
        }
        Ok(found)
    }

    /// Asks `textDocument/references` for the symbol named `name` at
    /// `path`:`line`:`column` -- zero-based, as the LSP protocol counts both,
    /// not the one-based line the rest of the index uses -- and returns the
    /// references that fall under the project root, in the index's own
    /// `Definition` shape. `includeDeclaration` is `false`, to match what the
    /// index's own references query is asked to find: uses, not the
    /// definition itself.
    ///
    /// The response carries no name of its own -- a `Location` is only a
    /// place -- so every `Definition` returned is stamped with the `name`
    /// the caller asked about, which is the only name it could possibly be.
    pub async fn references(
        &mut self,
        path: &str,
        line: u32,
        column: u32,
        name: &str,
        timeout: Duration,
    ) -> Result<Vec<Definition>> {
        let uri = url::Url::from_file_path(self.root.join(path))
            .map_err(|()| anyhow::anyhow!("{path} is not a path under {}", self.root.display()))?;
        let answered = self
            .call(
                "textDocument/references",
                json!({
                    "textDocument": {"uri": uri.as_str()},
                    "position": {"line": line, "character": column},
                    "context": {"includeDeclaration": false},
                }),
                timeout,
            )
            .await
            .with_context(|| {
                format!("asking rust-analyzer for references to {name} at {path}:{line}:{column}")
            })?;
        let locations = match answered {
            Value::Null => Vec::new(),
            Value::Array(locations) => locations,
            other => anyhow::bail!("`textDocument/references` answered with {other}, not a list"),
        };

        let mut found = Vec::new();
        for location in &locations {
            let Some(uri) = location.get("uri").and_then(Value::as_str) else {
                continue;
            };
            let Some(path) = relative_to_root(uri, &self.root) else {
                continue;
            };
            let Some(line) = location
                .pointer("/range/start/line")
                .and_then(Value::as_u64)
            else {
                continue;
            };
            found.push(Definition {
                path,
                name: name.to_string(),
                kind: String::new(),
                line: line as u32 + 1,
                language: "rust".to_string(),
            });
        }
        Ok(found)
    }

    /// Whether the server has read `path` at all. `textDocument/documentSymbol`
    /// answers with an empty list, or refuses outright, for a file that is in
    /// no cargo target it builds -- a crate whose root is switched off for
    /// this platform, an optional module, a test target whose
    /// `required-features` are not on. Asking is the point: the alternative
    /// is reading the `#[cfg]` text and guessing, which is how a measurement
    /// starts flattering itself.
    pub async fn has_read(&mut self, path: &str, timeout: Duration) -> Result<bool> {
        let uri = url::Url::from_file_path(self.root.join(path))
            .map_err(|()| anyhow::anyhow!("{path} is not a path under {}", self.root.display()))?;
        let answered = self
            .call(
                "textDocument/documentSymbol",
                json!({"textDocument": {"uri": uri.as_str()}}),
                timeout,
            )
            .await;
        match answered {
            // An answer of any shape means the file is loaded. An empty list
            // is not the same as an unread file: `crates/util/src/test/git.rs`
            // legitimately declares no symbols, and reading that as "never
            // read" would excuse every wrong answer in it.
            Ok(_) => Ok(true),
            Err(error) => {
                let refused = error
                    .downcast_ref::<ServerRefused>()
                    .is_some_and(ServerRefused::is_a_file_the_server_does_not_have);
                if refused { Ok(false) } else { Err(error) }
            }
        }
    }

    /// Whether the server resolves the name at `path`:`line`:`column` --
    /// zero-based, as the protocol counts -- to a definition. Answering
    /// nothing means it has no such name there: an item its build switched
    /// off, or a file it never read.
    ///
    /// `textDocument/definition` and not `textDocument/hover`: hover is
    /// documentation, and the server can resolve a name perfectly well while
    /// having nothing to say about it, which would read as "does not know
    /// it" and quietly excuse a wrong answer. Resolution is the question, so
    /// resolution is what is asked.
    pub async fn resolves_at(
        &mut self,
        path: &str,
        line: u32,
        column: u32,
        timeout: Duration,
    ) -> Result<bool> {
        let uri = url::Url::from_file_path(self.root.join(path))
            .map_err(|()| anyhow::anyhow!("{path} is not a path under {}", self.root.display()))?;
        let answered = self
            .call(
                "textDocument/definition",
                json!({
                    "textDocument": {"uri": uri.as_str()},
                    "position": {"line": line, "character": column},
                }),
                timeout,
            )
            .await?;
        Ok(match answered {
            Value::Null => false,
            Value::Array(places) => !places.is_empty(),
            // A single `Location` or `LocationLink`, which the protocol also
            // allows, is one place and therefore an answer.
            _ => true,
        })
    }

    /// Asks rust-analyzer to shut down cleanly and waits for the process to
    /// exit, falling back to killing it if it does not. Dropping a `Server`
    /// without calling this still cannot leak the process -- it was spawned
    /// with `kill_on_drop` -- but a clean shutdown is what a well-behaved
    /// client does when nothing has gone wrong.
    pub async fn shut_down(mut self) -> Result<()> {
        self.call("shutdown", Value::Null, SHUTDOWN_TIMEOUT).await?;
        self.notify("exit", Value::Null).await?;

        let child = &mut self.child;
        let exited = smol::future::or(
            async move {
                child.status().await.ok();
                true
            },
            async {
                // A real wait for a subprocess this binary drives directly,
                // with no gpui executor in a standalone measurement binary to
                // schedule it on.
                #[allow(clippy::disallowed_methods)]
                async_io::Timer::after(SHUTDOWN_TIMEOUT).await;
                false
            },
        )
        .await;
        if !exited {
            if let Err(error) = self.child.kill() {
                log::warn!(
                    "could not stop rust-analyzer after it did not exit on its own: {error}"
                );
            }
        }
        Ok(())
    }

    async fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await
            .with_context(|| format!("sending `{method}` to rust-analyzer"))?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "rust-analyzer did not answer `{method}` within {timeout:?}"
            );
            if let Some((got_id, result)) = self.pump_one(remaining).await? {
                if got_id == id {
                    return Ok(result);
                }
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
            .with_context(|| format!("sending `{method}` to rust-analyzer"))
    }

    async fn write(&mut self, message: &Value) -> Result<()> {
        let body = serde_json::to_vec(message).context("encoding a message to rust-analyzer")?;
        write_message(&mut self.stdin, &body)
            .await
            .context("writing to rust-analyzer's stdin")
    }

    /// Reads and fully handles one incoming message within `timeout`. A
    /// response to one of our own requests is handed back to the caller to
    /// match by id; a notification or a request from the server is dealt with
    /// here and nothing is returned.
    async fn pump_one(&mut self, timeout: Duration) -> Result<Option<(u64, Value)>> {
        match self.next_within(timeout).await? {
            Arrived::Nothing => Ok(None),
            Arrived::ClosedByServer => {
                anyhow::bail!("rust-analyzer closed its output; it may have exited or crashed")
            }
            Arrived::Message(body) => {
                let message: IncomingMessage = serde_json::from_slice(&body)
                    .context("rust-analyzer sent a message that is not valid JSON-RPC")?;
                self.handle(message).await
            }
        }
    }

    async fn next_within(&mut self, timeout: Duration) -> Result<Arrived> {
        let stdout = &mut self.stdout;
        let read = async move {
            match read_message(stdout).await {
                Ok(Some(body)) => Ok(Arrived::Message(body)),
                Ok(None) => Ok(Arrived::ClosedByServer),
                Err(error) => {
                    Err(anyhow::Error::new(error).context("reading rust-analyzer's stdout"))
                }
            }
        };
        let waited_out = async {
            // Same reasoning as in `shut_down`: a real wait outside gpui.
            #[allow(clippy::disallowed_methods)]
            async_io::Timer::after(timeout).await;
            Ok(Arrived::Nothing)
        };
        smol::future::or(read, waited_out).await
    }

    async fn handle(&mut self, message: IncomingMessage) -> Result<Option<(u64, Value)>> {
        let IncomingMessage {
            id,
            method,
            params,
            result,
            error,
        } = message;
        if let Some(method) = method {
            match id {
                Some(id) => self.handle_server_request(&method, id, params).await?,
                None => self.handle_notification(&method, params.as_ref()),
            }
            return Ok(None);
        }

        if let Some(error) = error {
            anyhow::bail!(ServerRefused::from(&error));
        }
        let id = id
            .as_ref()
            .and_then(Value::as_u64)
            .context("rust-analyzer sent a response with no numeric id")?;
        Ok(Some((id, result.unwrap_or(Value::Null))))
    }

    async fn handle_server_request(
        &mut self,
        method: &str,
        id: Value,
        params: Option<Value>,
    ) -> Result<()> {
        let result = match method {
            "window/workDoneProgress/create" => {
                if let Some(token) = params.as_ref().and_then(|value| value.get("token")) {
                    self.progress.opened(token);
                }
                Value::Null
            }
            // rust-analyzer may ask for its own settings even though this
            // client never declared the `workspace.configuration`
            // capability; answering with one `null` per requested item keeps
            // it on its own defaults rather than leaving the request hanging.
            "workspace/configuration" => {
                let items = params
                    .as_ref()
                    .and_then(|value| value.get("items"))
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                Value::Array(vec![Value::Null; items])
            }
            _ => Value::Null,
        };
        self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
            .with_context(|| format!("answering rust-analyzer's `{method}` request"))
    }

    fn handle_notification(&mut self, method: &str, params: Option<&Value>) {
        if method == "$/progress" {
            self.progress.on_progress(params);
        }
    }
}

/// One query, and what each side answered for it.
pub struct QueryAnswers {
    pub query: String,
    pub the_server_found: Vec<Definition>,
    pub the_index_found: Vec<Definition>,
}

/// Where the two sides disagreed on one query.
pub struct QueryDivergence {
    /// Which of [`Comparison::per_query`] this divergence belongs to. Two
    /// sampled symbols can share a name, so the name alone does not identify
    /// a query -- and attributing one query's set-aside findings to another's
    /// would subtract a real wrong answer from the wrong figure.
    pub at: usize,
    pub query: String,
    pub the_server_found_and_the_index_missed: Vec<Identity>,
    pub the_index_found_and_the_server_did_not: Vec<Identity>,
}

/// Which of a Rust file's lines are inside a `use` declaration, one-based and
/// inclusive.
///
/// A language server answers `workspace/symbol` with definitions, re-exports
/// and modules alike; an index of definitions holds only the first. A symbol
/// the index knows at the place it is defined, which the server *also* lists at
/// the place it is re-exported, is otherwise counted as missing -- so the
/// comparison must be able to tell a re-export apart from a definition.
pub fn re_export_lines(contents: &[u8], grammar: &tree_sitter::Language) -> Vec<(u32, u32)> {
    line_spans_of(contents, grammar, &["use_declaration"])
}

/// Which of a Rust file's lines a macro invocation spans, one-based and
/// inclusive.
///
/// A macro body is an opaque token tree to the grammar, so whatever the macro
/// expands to is not in the parse tree and no query over that tree can find it.
/// A name the server reports from inside such a span is a miss the index cannot
/// close by any change to its queries, and separating those from the rest is
/// what tells a fixable gap apart from a structural one.
pub fn macro_body_lines(contents: &[u8], grammar: &tree_sitter::Language) -> Vec<(u32, u32)> {
    line_spans_of(contents, grammar, &["macro_invocation"])
}

/// Which of a Rust file's lines an attribute spans, one-based and inclusive.
///
/// Kept apart from `macro_body_lines` on purpose. Some attributes expand code
/// -- a `derive` writes the trait implementations the server then reports --
/// and some, `cfg` and `allow` among them, expand nothing at all. Counting the
/// two together would claim more for macros than the measurement shows, so the
/// split is reported rather than resolved here.
pub fn attribute_lines(contents: &[u8], grammar: &tree_sitter::Language) -> Vec<(u32, u32)> {
    line_spans_of(contents, grammar, &["attribute_item"])
}

/// The line spans of every node of one of `kinds`, one-based and inclusive.
/// A matching node's own children are not descended into: the outermost span is
/// the one that matters, and the inner ones are inside it anyway.
fn line_spans_of(
    contents: &[u8],
    grammar: &tree_sitter::Language,
    kinds: &[&str],
) -> Vec<(u32, u32)> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(grammar).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(contents, None) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    let mut walking = vec![tree.root_node()];
    while let Some(node) = walking.pop() {
        if kinds.contains(&node.kind()) {
            spans.push((
                node.start_position().row as u32 + 1,
                node.end_position().row as u32 + 1,
            ));
            continue;
        }
        for at in 0..node.named_child_count() as u32 {
            if let Some(child) = node.named_child(at) {
                walking.push(child);
            }
        }
    }
    spans
}

/// The result of comparing the index's answers with the server's, over every
/// query asked.
pub struct Comparison {
    pub queries: usize,
    /// How many distinct symbols the server found across every query, summed
    /// query by query. The denominator of `recall`.
    pub the_servers_findings: usize,
    /// How many of the server's findings the index also found.
    pub matched: usize,
    /// `matched / the_servers_findings`, or `1.0` when the server found
    /// nothing at all to measure against -- a vacuous truth the caller has to
    /// treat as its own kind of failure, since it means the sample measured
    /// nothing.
    pub recall: f64,
    /// How many symbols the index found that the server did not. Never
    /// counted against `recall`: an extra result is not automatically wrong.
    pub the_indexs_extra_findings: usize,
    /// Every query where the two sides did not fully agree, in the order the
    /// queries were asked.
    pub divergent_queries: Vec<QueryDivergence>,
    /// What each query on its own produced. Kept because a figure pooled over
    /// every finding lets one symbol decide the whole number: a single
    /// sampled name that answers three and a half thousand times counts as
    /// much as three and a half thousand ordinary ones, and a rename is
    /// always invoked on one symbol.
    pub per_query: Vec<QueryOutcome>,
}

/// What one query produced on each side.
#[derive(Debug, Clone)]
pub struct QueryOutcome {
    pub query: String,
    pub the_server_found: usize,
    pub matched: usize,
    pub extra: usize,
}

/// Compares the index's answers with the server's, query by query, on
/// identity alone: two findings are the same symbol when their path, name and
/// line agree, whatever order either side returned them in.
pub fn compare(answers: &[QueryAnswers]) -> Comparison {
    let mut matched = 0usize;
    let mut the_servers_findings = 0usize;
    let mut the_indexs_extra_findings = 0usize;
    let mut divergent_queries = Vec::new();
    let mut per_query = Vec::with_capacity(answers.len());

    for answer in answers {
        let server_identities: HashSet<Identity> =
            answer.the_server_found.iter().map(Identity::of).collect();
        let index_identities: HashSet<Identity> =
            answer.the_index_found.iter().map(Identity::of).collect();

        the_servers_findings += server_identities.len();
        let matched_here = server_identities.intersection(&index_identities).count();
        matched += matched_here;

        let mut missed: Vec<Identity> = server_identities
            .difference(&index_identities)
            .cloned()
            .collect();
        missed.sort();
        let mut extra: Vec<Identity> = index_identities
            .difference(&server_identities)
            .cloned()
            .collect();
        extra.sort();
        the_indexs_extra_findings += extra.len();
        per_query.push(QueryOutcome {
            query: answer.query.clone(),
            the_server_found: server_identities.len(),
            matched: matched_here,
            extra: extra.len(),
        });

        if !missed.is_empty() || !extra.is_empty() {
            divergent_queries.push(QueryDivergence {
                at: per_query.len() - 1,
                query: answer.query.clone(),
                the_server_found_and_the_index_missed: missed,
                the_index_found_and_the_server_did_not: extra,
            });
        }
    }

    let recall = if the_servers_findings == 0 {
        1.0
    } else {
        matched as f64 / the_servers_findings as f64
    };

    Comparison {
        queries: answers.len(),
        the_servers_findings,
        matched,
        recall,
        the_indexs_extra_findings,
        divergent_queries,
        per_query,
    }
}

impl fmt::Display for Comparison {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        const DIVERGENCES_SHOWN: usize = 20;

        writeln!(
            out,
            "asked {} queries; the server found {} symbols within the project, the index matched \
             {} of them -- {:.1}% recall",
            self.queries,
            self.the_servers_findings,
            self.matched,
            self.recall * 100.0
        )?;
        writeln!(
            out,
            "the index found {} symbols the server did not (not counted against recall)",
            self.the_indexs_extra_findings
        )?;
        writeln!(
            out,
            "{} of {} queries diverged",
            self.divergent_queries.len(),
            self.queries
        )?;
        for divergence in self.divergent_queries.iter().take(DIVERGENCES_SHOWN) {
            writeln!(out, "  query {:?}", divergence.query)?;
            for identity in &divergence.the_server_found_and_the_index_missed {
                writeln!(out, "    missing from the index: {identity}")?;
            }
            for identity in &divergence.the_index_found_and_the_server_did_not {
                writeln!(out, "    extra in the index:     {identity}")?;
            }
        }
        if self.divergent_queries.len() > DIVERGENCES_SHOWN {
            write!(
                out,
                "  ... and {} more diverging queries not shown",
                self.divergent_queries.len() - DIVERGENCES_SHOWN
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(path: &str, name: &str, line: u32) -> Definition {
        Definition {
            path: path.to_string(),
            name: name.to_string(),
            kind: "function_item".to_string(),
            line,
            language: "rust".to_string(),
        }
    }

    #[test]
    fn a_path_inside_the_root_comes_back_relative_with_forward_slashes() {
        let root = Path::new("/home/user/project");
        let uri = "file:///home/user/project/src/one.rs";
        assert_eq!(
            relative_to_root(uri, root).expect("a path under the root"),
            "src/one.rs"
        );
    }

    #[test]
    fn a_path_outside_the_root_is_rejected() {
        let root = Path::new("/home/user/project");
        assert!(relative_to_root("file:///home/user/elsewhere/one.rs", root).is_none());
        assert!(
            relative_to_root("file:///home/user/.cargo/registry/src/dep/lib.rs", root).is_none()
        );
        // The root itself, named exactly: nothing relative to report.
        assert!(relative_to_root("file:///home/user/project", root).is_none());
    }

    #[test]
    fn a_uri_that_is_not_a_file_uri_is_rejected() {
        let root = Path::new("/home/user/project");
        assert!(relative_to_root("not a uri at all", root).is_none());
        assert!(relative_to_root("https://example.com/one.rs", root).is_none());
    }

    #[test]
    fn the_sample_is_the_same_on_every_run() {
        let names: Vec<String> = (0..500).map(|at| format!("symbol_{at}")).collect();
        assert_eq!(sample_queries(&names, 200), sample_queries(&names, 200));
    }

    #[test]
    fn the_sample_asks_for_exactly_the_count_when_there_is_room() {
        let names: Vec<String> = (0..500).map(|at| format!("symbol_{at}")).collect();
        let picked = sample_queries(&names, 200);
        assert_eq!(picked.len(), 200);
    }

    #[test]
    fn every_picked_query_is_a_real_name_and_none_are_invented() {
        let names: Vec<String> = vec!["alpha", "beta", "gamma", "delta"]
            .into_iter()
            .map(str::to_string)
            .collect();
        for query in sample_queries(&names, 10) {
            assert!(names.contains(&query), "{query} is not one of {names:?}");
        }
    }

    #[test]
    fn sampling_fewer_names_than_asked_for_returns_what_there_is() {
        let names: Vec<String> = vec!["only_one"].into_iter().map(str::to_string).collect();
        assert_eq!(sample_queries(&names, 200), vec!["only_one".to_string()]);
    }

    #[test]
    fn sampling_nothing_returns_nothing() {
        assert!(sample_queries(&[], 200).is_empty());
        let names: Vec<String> = vec!["a".to_string()];
        assert!(sample_queries(&names, 0).is_empty());
    }

    #[test]
    fn a_written_message_reads_back_the_same_bytes() {
        smol::block_on(async {
            let mut writer = smol::io::Cursor::new(Vec::new());
            write_message(&mut writer, b"{\"hello\":true}")
                .await
                .expect("writing");
            let mut reader = smol::io::Cursor::new(writer.into_inner());
            let body = read_message(&mut reader)
                .await
                .expect("reading")
                .expect("a message, not a clean close");
            assert_eq!(body, b"{\"hello\":true}");
        });
    }

    #[test]
    fn two_messages_written_back_to_back_are_read_back_in_order() {
        smol::block_on(async {
            let mut writer = smol::io::Cursor::new(Vec::new());
            write_message(&mut writer, b"first")
                .await
                .expect("writing the first");
            write_message(&mut writer, b"second")
                .await
                .expect("writing the second");
            let mut reader = smol::io::Cursor::new(writer.into_inner());
            assert_eq!(
                read_message(&mut reader)
                    .await
                    .expect("reading")
                    .expect("a message"),
                b"first"
            );
            assert_eq!(
                read_message(&mut reader)
                    .await
                    .expect("reading")
                    .expect("a message"),
                b"second"
            );
        });
    }

    #[test]
    fn a_clean_close_before_any_message_is_not_an_error() {
        smol::block_on(async {
            let mut reader = smol::io::Cursor::new(Vec::<u8>::new());
            assert!(read_message(&mut reader).await.expect("reading").is_none());
        });
    }

    #[test]
    fn a_message_with_no_content_length_header_is_rejected() {
        smol::block_on(async {
            let mut reader = smol::io::Cursor::new(b"X-Other: 1\r\n\r\nbody".to_vec());
            let failure = read_message(&mut reader)
                .await
                .expect_err("no length header");
            assert_eq!(failure.kind(), std::io::ErrorKind::InvalidData);
        });
    }

    #[test]
    fn a_close_in_the_middle_of_the_headers_is_an_error_not_a_clean_end() {
        smol::block_on(async {
            // A header line with no terminating blank line: the stream ends
            // while a message is still being read, not between two messages.
            let mut reader = smol::io::Cursor::new(b"Content-Length: 5\r\n".to_vec());
            let failure = read_message(&mut reader)
                .await
                .expect_err("a truncated message is an error");
            assert_eq!(failure.kind(), std::io::ErrorKind::UnexpectedEof);
        });
    }

    #[test]
    fn a_close_in_the_middle_of_the_body_is_an_error() {
        smol::block_on(async {
            let mut reader = smol::io::Cursor::new(b"Content-Length: 10\r\n\r\ntoo short".to_vec());
            let failure = read_message(&mut reader)
                .await
                .expect_err("fewer body bytes than promised is an error");
            assert_eq!(failure.kind(), std::io::ErrorKind::UnexpectedEof);
        });
    }

    #[test]
    fn identical_answers_have_full_recall_and_no_divergence() {
        let answers = vec![QueryAnswers {
            query: "work".to_string(),
            the_server_found: vec![definition("src/one.rs", "work", 5)],
            the_index_found: vec![definition("src/one.rs", "work", 5)],
        }];
        let comparison = compare(&answers);
        assert_eq!(comparison.the_servers_findings, 1);
        assert_eq!(comparison.matched, 1);
        assert_eq!(comparison.recall, 1.0);
        assert_eq!(comparison.the_indexs_extra_findings, 0);
        assert!(comparison.divergent_queries.is_empty());
    }

    #[test]
    fn a_symbol_the_index_missed_lowers_recall_and_is_printed() {
        let answers = vec![QueryAnswers {
            query: "work".to_string(),
            the_server_found: vec![
                definition("src/one.rs", "work", 5),
                definition("src/two.rs", "work", 9),
            ],
            the_index_found: vec![definition("src/one.rs", "work", 5)],
        }];
        let comparison = compare(&answers);
        assert_eq!(comparison.the_servers_findings, 2);
        assert_eq!(comparison.matched, 1);
        assert_eq!(comparison.recall, 0.5);
        assert_eq!(comparison.divergent_queries.len(), 1);
        assert_eq!(
            comparison.divergent_queries[0].the_server_found_and_the_index_missed,
            vec![Identity::of(&definition("src/two.rs", "work", 9))]
        );
        assert!(
            comparison.divergent_queries[0]
                .the_index_found_and_the_server_did_not
                .is_empty()
        );
    }

    #[test]
    fn an_extra_index_answer_does_not_lower_recall_but_is_printed() {
        let answers = vec![QueryAnswers {
            query: "work".to_string(),
            the_server_found: vec![definition("src/one.rs", "work", 5)],
            the_index_found: vec![
                definition("src/one.rs", "work", 5),
                definition("src/one.rs", "work_harder", 12),
            ],
        }];
        let comparison = compare(&answers);
        assert_eq!(comparison.recall, 1.0, "an extra finding is not a miss");
        assert_eq!(comparison.the_indexs_extra_findings, 1);
        assert_eq!(comparison.divergent_queries.len(), 1);
        assert_eq!(
            comparison.divergent_queries[0].the_index_found_and_the_server_did_not,
            vec![Identity::of(&definition("src/one.rs", "work_harder", 12))]
        );
    }

    #[test]
    fn the_empty_case_is_a_vacuous_full_recall() {
        let comparison = compare(&[]);
        assert_eq!(comparison.queries, 0);
        assert_eq!(comparison.the_servers_findings, 0);
        assert_eq!(comparison.recall, 1.0);
        assert!(comparison.divergent_queries.is_empty());
    }

    #[test]
    fn a_query_where_the_server_found_nothing_either_is_not_a_divergence() {
        let answers = vec![QueryAnswers {
            query: "nonexistent".to_string(),
            the_server_found: Vec::new(),
            the_index_found: Vec::new(),
        }];
        let comparison = compare(&answers);
        assert!(comparison.divergent_queries.is_empty());
    }
}
