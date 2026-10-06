mod router;

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result};
use futures::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _,
    FutureExt as _, channel::oneshot, future::Shared, io::BufReader,
};
use gpui::BackgroundExecutor;
use parking_lot::Mutex;
use serde_json::{Value, json};
use util::command::Stdio;

use crate::{CONTENT_LEN_HEADER, LanguageServerBinary};
use router::{ClientId, Outgoing, Router};

/// `ZED_SHARE_LANGUAGE_SERVERS=0` gives every window its own processes again.
static SHARING_ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var("ZED_SHARE_LANGUAGE_SERVERS").as_deref() != Ok("0"));
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);
static POOL: LazyLock<Mutex<Vec<Arc<Backend>>>> = LazyLock::new(Default::default);

/// Makes the next window that starts the server named `server_name` get a
/// fresh process, so a restart really restarts it. Windows still attached to
/// the old process keep it until they leave.
pub fn retire_shared_processes(server_name: &str) {
    for backend in POOL.lock().iter() {
        if backend.server_name == server_name {
            backend.retired.store(true, Ordering::Release);
        }
    }
}

pub(crate) fn sharing_enabled() -> bool {
    *SHARING_ENABLED
}

/// The two ends a window's `LanguageServer` uses in place of a process's
/// stdin and stdout.
pub(crate) struct Connection {
    pub(crate) to_server: async_pipe::PipeWriter,
    pub(crate) from_server: async_pipe::PipeReader,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Command {
    path: PathBuf,
    arguments: Vec<OsString>,
    env: BTreeMap<String, String>,
}

impl Command {
    fn of(binary: &LanguageServerBinary) -> Self {
        Self {
            path: binary.path.clone(),
            arguments: binary.arguments.clone(),
            env: binary.env.clone().unwrap_or_default().into_iter().collect(),
        }
    }
}

enum Event {
    Attach {
        client: ClientId,
        initialize_request: Value,
        to_client: async_channel::Sender<Value>,
    },
    FromClient(ClientId, Value),
    ClientGone(ClientId),
    FromServer(Value),
    ServerGone,
}

/// One running server process and the windows attached to it.
struct Backend {
    server_name: String,
    command: Command,
    initialization_options: Value,
    events: async_channel::Sender<Event>,
    /// Resolves once the server answered `initialize`, with whether it can
    /// take more workspace folders.
    ready: Shared<oneshot::Receiver<bool>>,
    closed: AtomicBool,
    /// Set when a window restarts the server: windows still attached keep
    /// using this process, new ones start a fresh one.
    retired: AtomicBool,
}

/// Connects a window to a server process shared with every other window that
/// starts the same server the same way. The process starts with the first
/// window and stops after the last one leaves.
pub(crate) fn connect(
    binary: &LanguageServerBinary,
    working_dir: &Path,
    server_name: &str,
    executor: BackgroundExecutor,
) -> Connection {
    let (to_server, client_output) = async_pipe::pipe();
    let (client_input, from_server) = async_pipe::pipe();
    let client = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    let command = Command::of(binary);
    let working_dir = working_dir.to_path_buf();
    let server_name = server_name.to_owned();

    executor
        .clone()
        .spawn(async move {
            if let Err(error) = run_client(
                client,
                command,
                working_dir,
                server_name.clone(),
                client_output,
                client_input,
                executor,
            )
            .await
            {
                log::warn!("shared language server {server_name}: {error:#}");
            }
        })
        .detach();

    Connection {
        to_server,
        from_server,
    }
}

async fn run_client(
    client: ClientId,
    command: Command,
    working_dir: PathBuf,
    server_name: String,
    client_output: async_pipe::PipeReader,
    mut client_input: async_pipe::PipeWriter,
    executor: BackgroundExecutor,
) -> Result<()> {
    let mut client_output = BufReader::new(client_output);
    let initialize_request = read_message(&mut client_output)
        .await?
        .context("the window closed before initializing")?;
    anyhow::ensure!(
        initialize_request["method"] == "initialize",
        "the first message was {}, not initialize",
        initialize_request["method"]
    );
    let initialization_options = initialize_request["params"]["initializationOptions"].clone();

    let (to_client, from_backend) = async_channel::unbounded::<Value>();
    executor
        .spawn(async move {
            while let Ok(message) = from_backend.recv().await {
                if write_message(&mut client_input, &message).await.is_err() {
                    break;
                }
            }
        })
        .detach();

    let mut attach = Event::Attach {
        client,
        initialize_request,
        to_client,
    };
    let mut attempts = 0;
    let backend = loop {
        let backend = choose_backend(
            &command,
            &initialization_options,
            &working_dir,
            &server_name,
            &executor,
        )
        .await?;
        match backend.events.send(attach).await {
            Ok(()) => break backend,
            // The process stopped between being chosen and being joined.
            Err(async_channel::SendError(event)) => {
                attempts += 1;
                anyhow::ensure!(
                    attempts < 3,
                    "the shared {server_name} server kept stopping"
                );
                attach = event;
            }
        }
    };

    while let Ok(Some(message)) = read_message(&mut client_output).await {
        if backend
            .events
            .send(Event::FromClient(client, message))
            .await
            .is_err()
        {
            break;
        }
    }
    backend.events.send(Event::ClientGone(client)).await.ok();
    Ok(())
}

/// Picks a running process this window can join, or starts one. Choosing
/// and starting happen under one lock, so two windows opening at once share
/// a process instead of starting two.
async fn choose_backend(
    command: &Command,
    initialization_options: &Value,
    working_dir: &Path,
    server_name: &str,
    executor: &BackgroundExecutor,
) -> Result<Arc<Backend>> {
    let mut passed_over = Vec::<Arc<Backend>>::new();
    loop {
        let candidate = {
            let mut pool = POOL.lock();
            let found = pool
                .iter()
                .find(|backend| {
                    backend.is_joinable()
                        && backend.server_name == server_name
                        && &backend.command == command
                        && &backend.initialization_options == initialization_options
                        && !passed_over.iter().any(|other| Arc::ptr_eq(other, backend))
                })
                .cloned();
            match found {
                Some(found) => found,
                None => {
                    let backend = start_backend(
                        command.clone(),
                        initialization_options.clone(),
                        working_dir,
                        server_name,
                        executor,
                    )?;
                    pool.push(backend.clone());
                    return Ok(backend);
                }
            }
        };
        if let Ok(true) = candidate.ready.clone().await
            && candidate.is_joinable()
        {
            log::info!("joining the running {server_name} language server");
            return Ok(candidate);
        }
        passed_over.push(candidate);
    }
}

impl Backend {
    fn is_joinable(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && !self.retired.load(Ordering::Acquire)
    }
}

fn start_backend(
    command: Command,
    initialization_options: Value,
    working_dir: &Path,
    server_name: &str,
    executor: &BackgroundExecutor,
) -> Result<Arc<Backend>> {
    log::info!(
        "starting shared language server {server_name}: {:?} {:?} in {working_dir:?}",
        command.path,
        command.arguments
    );
    let Spawned {
        stdin,
        stdout,
        stderr,
        child,
    } = spawn(&command, working_dir)?;

    let (events_tx, events_rx) = async_channel::unbounded::<Event>();
    let (ready_tx, ready_rx) = oneshot::channel();
    let backend = Arc::new(Backend {
        server_name: server_name.to_owned(),
        command,
        initialization_options,
        events: events_tx.clone(),
        ready: ready_rx.shared(),
        closed: AtomicBool::new(false),
        retired: AtomicBool::new(false),
    });

    executor
        .spawn({
            let events = events_tx;
            async move {
                let mut stdout = BufReader::new(stdout);
                while let Ok(Some(message)) = read_message(&mut stdout).await {
                    if events.send(Event::FromServer(message)).await.is_err() {
                        return;
                    }
                }
                events.send(Event::ServerGone).await.ok();
            }
        })
        .detach();

    let stderr_name = server_name.to_owned();
    executor
        .spawn(async move {
            let mut stderr = BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(read) = stderr.read_line(&mut line).await {
                if read == 0 {
                    break;
                }
                log::debug!("{stderr_name} stderr: {}", line.trim_end());
                line.clear();
            }
        })
        .detach();

    let loop_backend = backend.clone();
    let server_name = server_name.to_owned();
    let loop_executor = executor.clone();
    executor
        .spawn(async move {
            run_backend(
                loop_backend,
                child,
                stdin,
                events_rx,
                ready_tx,
                &server_name,
                loop_executor,
            )
            .await;
        })
        .detach();

    Ok(backend)
}

struct Spawned {
    stdin: Box<dyn AsyncWrite + Unpin + Send>,
    stdout: Box<dyn AsyncRead + Unpin + Send>,
    stderr: Box<dyn AsyncRead + Unpin + Send>,
    child: Option<util::command::Child>,
}

fn spawn(command: &Command, working_dir: &Path) -> Result<Spawned> {
    #[cfg(test)]
    if let Some(spawned) = tests::spawn_test_server(&command.path) {
        return Ok(spawned);
    }
    let mut process = util::command::new_command(&command.path);
    process
        .current_dir(working_dir)
        .args(&command.arguments)
        .envs(command.env.clone())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = process
        .spawn()
        .with_context(|| format!("failed to spawn {process:?}"))?;
    Ok(Spawned {
        stdin: Box::new(child.stdin.take().context("no stdin")?),
        stdout: Box::new(child.stdout.take().context("no stdout")?),
        stderr: Box::new(child.stderr.take().context("no stderr")?),
        child: Some(child),
    })
}

const SHUTDOWN_REQUEST_ID: &str = "shared-server-shutdown";
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

async fn run_backend(
    backend: Arc<Backend>,
    child: Option<util::command::Child>,
    mut stdin: Box<dyn AsyncWrite + Unpin + Send>,
    events: async_channel::Receiver<Event>,
    ready_tx: oneshot::Sender<bool>,
    server_name: &str,
    executor: BackgroundExecutor,
) {
    // A server that stops reading must not stall the windows' traffic.
    let (to_server, server_queue) = async_channel::unbounded::<Value>();
    let writer = executor.spawn({
        let server_name = server_name.to_owned();
        async move {
            while let Ok(message) = server_queue.recv().await {
                if write_message(&mut stdin, &message).await.is_err() {
                    log::warn!("{server_name}: could not write to the shared server");
                    break;
                }
            }
        }
    });

    let mut router = Router::new();
    let mut clients = collections::HashMap::<ClientId, async_channel::Sender<Value>>::default();
    let mut ready_tx = Some(ready_tx);
    let mut shutting_down = false;

    while let Ok(event) = events.recv().await {
        let outgoing = match event {
            Event::Attach {
                client,
                initialize_request,
                to_client,
            } => {
                clients.insert(client, to_client);
                router.attach(client, initialize_request)
            }
            Event::FromClient(client, message) => router.from_client(client, message),
            Event::ClientGone(client) => {
                clients.remove(&client);
                router.detach(client)
            }
            Event::FromServer(message) => router.from_server(message),
            Event::ServerGone => break,
        };
        for message in outgoing {
            match message {
                Outgoing::ToServer(message) => {
                    to_server.send(message).await.ok();
                }
                Outgoing::ToClient(client, message) => {
                    if let Some(sender) = clients.get(&client) {
                        sender.send(message).await.ok();
                    }
                }
                Outgoing::Ready {
                    accepts_more_folders,
                } => {
                    if let Some(ready_tx) = ready_tx.take() {
                        ready_tx.send(accepts_more_folders).ok();
                    }
                }
                Outgoing::Shutdown => shutting_down = true,
            }
        }
        if shutting_down {
            break;
        }
    }

    backend.closed.store(true, Ordering::Release);
    POOL.lock().retain(|other| !Arc::ptr_eq(other, &backend));
    if shutting_down {
        log::info!("the last window left; stopping the shared {server_name} server");
        to_server
            .send(json!({
                "jsonrpc": "2.0",
                "id": SHUTDOWN_REQUEST_ID,
                "method": "shutdown",
                "params": null,
            }))
            .await
            .ok();
        if !wait_for_shutdown_answer(&events, &executor).await {
            log::warn!("{server_name} did not answer shutdown in time");
        }
        to_server
            .send(json!({ "jsonrpc": "2.0", "method": "exit", "params": null }))
            .await
            .ok();
    }
    events.close();
    to_server.close();
    writer.await;
    drop(child);
}

/// Waits for the server's answer to the final `shutdown`. A window that tried
/// to join in the meantime is told the server is gone.
async fn wait_for_shutdown_answer(
    events: &async_channel::Receiver<Event>,
    executor: &BackgroundExecutor,
) -> bool {
    let mut timeout = executor.timer(SHUTDOWN_TIMEOUT).fuse();
    loop {
        let event = futures::select_biased! {
            event = events.recv().fuse() => event,
            _ = timeout => return false,
        };
        match event {
            Ok(Event::FromServer(message)) if message["id"] == SHUTDOWN_REQUEST_ID => return true,
            Ok(Event::Attach {
                initialize_request,
                to_client,
                ..
            }) => {
                to_client
                    .send(json!({
                        "jsonrpc": "2.0",
                        "id": initialize_request["id"],
                        "error": { "code": -32002, "message": "the shared server is stopping" },
                    }))
                    .await
                    .ok();
            }
            Ok(Event::ServerGone) | Err(_) => return false,
            Ok(_) => {}
        }
    }
}

async fn read_message<R>(reader: &mut BufReader<R>) -> Result<Option<Value>>
where
    R: AsyncRead + Unpin,
{
    let mut content_length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let header = line.trim_end();
        if header.is_empty() {
            if content_length.is_some() {
                break;
            }
            continue;
        }
        if let Some(length) = header.strip_prefix(CONTENT_LEN_HEADER) {
            content_length = Some(length.trim().parse::<usize>()?);
        }
    }
    let mut body = vec![0; content_length.unwrap_or(0)];
    reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

async fn write_message(writer: &mut (impl AsyncWrite + Unpin), message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    writer
        .write_all(format!("{CONTENT_LEN_HEADER}{}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        DEFAULT_LSP_REQUEST_TIMEOUT, DidChangeConfigurationParams, HoverParams, LanguageServer,
        LanguageServerId, LanguageServerName, Position, TextDocumentIdentifier,
        TextDocumentPositionParams, Uri, request,
    };
    use collections::HashMap;
    use gpui::TestAppContext;
    use std::{str::FromStr as _, sync::atomic::AtomicUsize};

    type TestServer = Box<dyn Fn() -> Spawned + Send>;

    static TEST_SERVERS: LazyLock<Mutex<HashMap<PathBuf, TestServer>>> =
        LazyLock::new(Default::default);

    pub(super) fn spawn_test_server(path: &Path) -> Option<Spawned> {
        TEST_SERVERS.lock().get(path).map(|spawn| spawn())
    }

    /// Registers a server at `path` that answers `initialize`, `hover` and
    /// `shutdown`, and reports every message it hears.
    fn scripted_server(
        path: &Path,
        executor: BackgroundExecutor,
    ) -> (Arc<AtomicUsize>, async_channel::Receiver<Value>) {
        let starts = Arc::new(AtomicUsize::new(0));
        let (heard_tx, heard_rx) = async_channel::unbounded();
        let spawn = {
            let starts = starts.clone();
            move || {
                starts.fetch_add(1, Ordering::SeqCst);
                let (stdin, server_input) = async_pipe::pipe();
                let (mut server_output, stdout) = async_pipe::pipe();
                let (_, stderr) = async_pipe::pipe();
                let heard = heard_tx.clone();
                executor
                    .spawn(async move {
                        let mut input = BufReader::new(server_input);
                        while let Ok(Some(message)) = read_message(&mut input).await {
                            heard.send(message.clone()).await.ok();
                            let result = match message["method"].as_str() {
                                Some("initialize") => json!({ "capabilities": {
                                    "hoverProvider": true,
                                    "workspace": { "workspaceFolders": {
                                        "supported": true, "changeNotifications": true,
                                    } },
                                } }),
                                Some("textDocument/hover") => json!({
                                    "contents": message["params"]["textDocument"]["uri"],
                                }),
                                _ => Value::Null,
                            };
                            if message.get("id").is_some() {
                                let reply =
                                    json!({ "jsonrpc": "2.0", "id": message["id"], "result": result });
                                write_message(&mut server_output, &reply).await.ok();
                            }
                        }
                    })
                    .detach();
                Spawned {
                    stdin: Box::new(stdin),
                    stdout: Box::new(stdout),
                    stderr: Box::new(stderr),
                    child: None,
                }
            }
        };
        TEST_SERVERS
            .lock()
            .insert(path.to_path_buf(), Box::new(spawn));
        (starts, heard_rx)
    }

    /// Tests run in parallel against one pool, so each uses its own server name.
    async fn start_window(
        server_name: &str,
        binary: &LanguageServerBinary,
        file_in_root: &str,
        cx: &mut TestAppContext,
    ) -> Arc<LanguageServer> {
        let server = LanguageServer::new(
            Arc::new(Mutex::new(None)),
            LanguageServerId(NEXT_CLIENT_ID.load(Ordering::Relaxed) as usize),
            LanguageServerName(server_name.to_owned().into()),
            binary.clone(),
            Path::new(file_in_root),
            None,
            None,
            &mut cx.to_async(),
        )
        .unwrap();
        cx.update(|cx| {
            let params = server.default_initialize_params(false, false, cx);
            server.initialize(
                params,
                DidChangeConfigurationParams {
                    settings: Value::Null,
                }
                .into(),
                DEFAULT_LSP_REQUEST_TIMEOUT,
                cx,
            )
        })
        .await
        .unwrap()
    }

    async fn hover(server: &LanguageServer, uri: &str) -> Value {
        let hover = server
            .request::<request::HoverRequest>(
                HoverParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier::new(Uri::from_str(uri).unwrap()),
                        position: Position::new(0, 0),
                    },
                    work_done_progress_params: Default::default(),
                },
                DEFAULT_LSP_REQUEST_TIMEOUT,
            )
            .await
            .into_response()
            .unwrap()
            .unwrap();
        serde_json::to_value(hover.contents).unwrap()
    }

    fn drain(heard: &async_channel::Receiver<Value>) -> Vec<String> {
        std::iter::from_fn(|| heard.try_recv().ok())
            .map(|message| message["method"].as_str().unwrap_or("<reply>").to_owned())
            .collect()
    }

    #[gpui::test]
    async fn test_two_windows_share_one_server_process(cx: &mut TestAppContext) {
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        let binary = LanguageServerBinary {
            path: "/test/two-windows/yaml-language-server".into(),
            arguments: Vec::new(),
            env: None,
        };
        let (starts, heard) = scripted_server(&binary.path, cx.executor());

        let first = start_window("yaml-two-windows", &binary, "/test/first/a.yaml", cx).await;
        cx.run_until_parked();
        assert_eq!(drain(&heard), ["initialize", "initialized"]);

        let second = start_window("yaml-two-windows", &binary, "/test/second/b.yaml", cx).await;
        cx.run_until_parked();
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "one process for both windows"
        );
        let joined = std::iter::from_fn(|| heard.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(
            joined.len(),
            1,
            "the second window must not initialize again"
        );
        assert_eq!(joined[0]["method"], "workspace/didChangeWorkspaceFolders");
        assert_eq!(
            joined[0]["params"]["event"]["added"][0]["uri"],
            "file:///test/second"
        );

        let (first_hover, second_hover) = futures::join!(
            hover(&first, "file:///test/first/a.yaml"),
            hover(&second, "file:///test/second/b.yaml"),
        );
        assert_eq!(first_hover, json!("file:///test/first/a.yaml"));
        assert_eq!(second_hover, json!("file:///test/second/b.yaml"));
        cx.run_until_parked();
        drain(&heard);

        if let Some(shutdown) = first.shutdown() {
            shutdown.await;
        }
        drop(first);
        cx.run_until_parked();
        let after_first = drain(&heard);
        assert!(
            !after_first.iter().any(|method| method == "shutdown"),
            "the server stays up for the other window, heard {after_first:?}"
        );

        if let Some(shutdown) = second.shutdown() {
            shutdown.await;
        }
        drop(second);
        cx.run_until_parked();
        let after_second = drain(&heard);
        assert!(
            after_second.iter().any(|method| method == "shutdown")
                && after_second.iter().any(|method| method == "exit"),
            "the last window stops the server, heard {after_second:?}"
        );
    }

    #[gpui::test]
    async fn test_a_restart_gets_a_fresh_process(cx: &mut TestAppContext) {
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        let binary = LanguageServerBinary {
            path: "/test/restart/yaml-language-server".into(),
            arguments: Vec::new(),
            env: None,
        };
        let (starts, _heard) = scripted_server(&binary.path, cx.executor());

        let _first = start_window("yaml-restart", &binary, "/test/restart-first/a.yaml", cx).await;
        retire_shared_processes("yaml-restart");
        let _second =
            start_window("yaml-restart", &binary, "/test/restart-second/b.yaml", cx).await;
        cx.run_until_parked();
        assert_eq!(
            starts.load(Ordering::SeqCst),
            2,
            "a restart must not hand the window the old process again"
        );
    }

    #[gpui::test]
    async fn test_windows_opening_together_share_one_process(cx: &mut TestAppContext) {
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        let binary = LanguageServerBinary {
            path: "/test/together/yaml-language-server".into(),
            arguments: Vec::new(),
            env: None,
        };
        let (starts, _heard) = scripted_server(&binary.path, cx.executor());
        let connect = |file: &str, cx: &mut TestAppContext| {
            LanguageServer::new(
                Arc::new(Mutex::new(None)),
                LanguageServerId(0),
                LanguageServerName("yaml-together".into()),
                binary.clone(),
                Path::new(file),
                None,
                None,
                &mut cx.to_async(),
            )
            .unwrap()
        };
        let first = connect("/test/together-first/a.yaml", cx);
        let second = connect("/test/together-second/b.yaml", cx);
        let initialize = |server: LanguageServer, cx: &mut TestAppContext| {
            cx.update(|cx| {
                let params = server.default_initialize_params(false, false, cx);
                server.initialize(
                    params,
                    DidChangeConfigurationParams {
                        settings: Value::Null,
                    }
                    .into(),
                    DEFAULT_LSP_REQUEST_TIMEOUT,
                    cx,
                )
            })
        };
        let first = initialize(first, cx);
        let second = initialize(second, cx);
        let (first, second) = futures::join!(first, second);
        first.unwrap();
        second.unwrap();
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "two windows opening at once must not start two processes"
        );
    }
}
