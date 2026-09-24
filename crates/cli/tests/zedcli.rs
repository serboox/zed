#![cfg(any(target_os = "linux", target_os = "freebsd"))]

use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use cli::{
    CliRequest, CliResponse, ConfigurationInfo, DbConnectionSummary, IpcHandshake, ProcessInfo,
    RunAction, RunInfo, RunState, WindowInfo, WorkspaceInfo, exit_status, ipc,
};
use smol::io::AsyncWriteExt as _;
use tempfile::TempDir;

/// An editor that answers one `zedcli` request the way the real one would: it
/// listens on the channel's socket in its data directory, connects back over
/// the address it is sent, and replies with what `answer` gives. It hands back
/// the request it was sent, so a test can check what the command asked for.
struct FakeEditor {
    data_dir: TempDir,
    served: JoinHandle<Option<CliRequest>>,
}

impl FakeEditor {
    fn answering(answer: impl FnOnce(&CliRequest) -> Vec<CliResponse> + Send + 'static) -> Self {
        let data_dir = TempDir::new().expect("a data directory");
        let socket = UnixDatagram::bind(socket_in(data_dir.path())).expect("the socket binds");
        let served = std::thread::spawn(move || {
            let mut buffer = [0u8; 1024];
            let length = socket.recv(&mut buffer).ok()?;
            let url = String::from_utf8_lossy(&buffer[..length]).to_string();
            let server_name = url.strip_prefix("zed-cli://")?.to_string();
            let handshake = ipc::IpcSender::<IpcHandshake>::connect(server_name).ok()?;
            let (request_tx, request_rx) = ipc::channel::<CliRequest>().ok()?;
            let (response_tx, response_rx) = ipc::channel::<CliResponse>().ok()?;
            handshake
                .send(IpcHandshake {
                    requests: request_tx,
                    responses: response_rx,
                })
                .ok()?;
            let request = request_rx.recv().ok()?;
            for response in answer(&request) {
                response_tx.send(response).ok()?;
            }
            Some(request)
        });
        Self { data_dir, served }
    }

    /// A socket that is there but never answers, the way a hung editor is.
    fn silent() -> (TempDir, UnixDatagram) {
        let data_dir = TempDir::new().expect("a data directory");
        let socket = UnixDatagram::bind(socket_in(data_dir.path())).expect("the socket binds");
        (data_dir, socket)
    }

    fn request(self) -> CliRequest {
        self.served
            .join()
            .expect("the fake editor does not panic")
            .expect("the fake editor was sent a request")
    }
}

fn socket_in(data_dir: &Path) -> PathBuf {
    data_dir.join(format!(
        "zed-{}.sock",
        *release_channel::RELEASE_CHANNEL_NAME
    ))
}

/// The CLI binary under the name that makes it `zedcli`.
fn zedcli_binary() -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("zedcli-e2e");
    std::fs::create_dir_all(&directory).expect("a directory for the binary");
    let link = directory.join("zedcli");
    // Tests run in parallel and each makes sure of the link; one that loses the
    // race to create it finds it already there.
    if let Err(error) = std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_cli"), &link)
        && error.kind() != std::io::ErrorKind::AlreadyExists
    {
        panic!("the link is made: {error}");
    }
    link
}

fn zedcli(data_dir: &Path, args: &[&str], stdin: Option<&str>, cwd: Option<&Path>) -> Output {
    smol::block_on(async {
        let mut command = smol::process::Command::new(zedcli_binary());
        command
            .arg("--user-data-dir")
            .arg(data_dir)
            .args(args)
            .stdin(match stdin {
                Some(_) => Stdio::piped(),
                None => Stdio::null(),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().expect("zedcli starts");
        if let (Some(text), Some(mut input)) = (stdin, child.stdin.take()) {
            input
                .write_all(text.as_bytes())
                .await
                .expect("stdin is written");
        }
        child.output().await.expect("zedcli finishes")
    })
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn a_window() -> WindowInfo {
    WindowInfo {
        id: 7,
        focused: true,
        workspaces: vec![WorkspaceInfo {
            active: true,
            projects: vec![PathBuf::from("/work/api")],
            active_path: Some(PathBuf::from("/work/api/cmd/main.go")),
        }],
    }
}

#[test]
fn windows_asks_for_the_windows_and_prints_them_as_a_table() {
    let editor = FakeEditor::answering(|_| {
        vec![
            CliResponse::Windows {
                items: vec![a_window()],
            },
            CliResponse::Exit { status: 0 },
        ]
    });
    let output = zedcli(editor.data_dir.path(), &["windows"], None, None);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(text.starts_with("WINDOW"), "a header first: {text}");
    assert!(
        text.contains("7") && text.contains("/work/api") && text.contains("cmd/main.go"),
        "the window, its project and its file: {text}"
    );
    assert_eq!(editor.request(), CliRequest::ListWindows);
}

#[test]
fn windows_as_json_is_the_editors_answer_verbatim() {
    let editor = FakeEditor::answering(|_| {
        vec![
            CliResponse::Windows {
                items: vec![a_window()],
            },
            CliResponse::Exit { status: 0 },
        ]
    });
    let output = zedcli(editor.data_dir.path(), &["windows", "--json"], None, None);
    let parsed: Vec<WindowInfo> =
        serde_json::from_slice(&output.stdout).expect("the output is the windows as JSON");
    assert_eq!(parsed, vec![a_window()]);
    editor.request();
}

#[test]
fn ps_names_the_directory_it_was_run_from_and_prints_each_process_of_a_run() {
    let project = TempDir::new().expect("a project directory");
    let editor = FakeEditor::answering(|_| {
        vec![
            CliResponse::Runs {
                runs: vec![RunInfo {
                    window: 7,
                    label: "api server".into(),
                    command: "go run ./cmd/api".into(),
                    state: RunState::Running,
                    pid: Some(100),
                    processes: vec![
                        ProcessInfo {
                            pid: 100,
                            parent: 1,
                            name: "bash".into(),
                            cpu_percent: Some(0.),
                            memory_bytes: 4 << 20,
                            threads: 1,
                            state: "S".into(),
                        },
                        ProcessInfo {
                            pid: 101,
                            parent: 100,
                            name: "cmd-api".into(),
                            cpu_percent: Some(1.9),
                            memory_bytes: 130 << 20,
                            threads: 13,
                            state: "S".into(),
                        },
                    ],
                }],
                debug_sessions: Vec::new(),
            },
            CliResponse::Exit { status: 0 },
        ]
    });
    let output = zedcli(editor.data_dir.path(), &["ps"], None, Some(project.path()));
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        text.contains("api server") && text.contains("running"),
        "{text}"
    );
    assert!(
        text.contains("└ cmd-api") && text.contains("1.9%") && text.contains("130 MB"),
        "the process the run started, with what it uses: {text}"
    );
    let CliRequest::ListRuns { selector } = editor.request() else {
        panic!("ps asks for the runs");
    };
    assert_eq!(
        selector.cwd.and_then(|cwd| cwd.canonicalize().ok()),
        project.path().canonicalize().ok(),
        "the window is chosen by the directory zedcli runs in"
    );
    assert!(!selector.all && selector.window.is_none());
}

#[test]
fn a_window_can_be_named_or_every_window_asked_for() {
    let editor = FakeEditor::answering(|_| {
        vec![
            CliResponse::Configurations {
                items: vec![ConfigurationInfo {
                    window: 7,
                    label: "api server".into(),
                    kind: "task".into(),
                    command: "go run ./cmd/api".into(),
                    running: true,
                }],
            },
            CliResponse::Exit { status: 0 },
        ]
    });
    let output = zedcli(
        editor.data_dir.path(),
        &["configs", "--window", "7"],
        None,
        None,
    );
    assert!(
        stdout(&output).contains("api server"),
        "{}",
        stdout(&output)
    );
    let CliRequest::ListConfigurations { selector } = editor.request() else {
        panic!("configs asks for the configurations");
    };
    assert_eq!(selector.window, Some(7));

    let editor = FakeEditor::answering(|_| {
        vec![
            CliResponse::Configurations { items: Vec::new() },
            CliResponse::Exit { status: 0 },
        ]
    });
    let output = zedcli(editor.data_dir.path(), &["configs", "--all"], None, None);
    assert!(stdout(&output).contains("No run configurations"));
    let CliRequest::ListConfigurations { selector } = editor.request() else {
        panic!("configs asks for the configurations");
    };
    assert!(selector.all);
}

#[test]
fn run_stop_and_restart_ask_for_their_own_action() {
    for (command, action) in [
        ("run", RunAction::Run),
        ("stop", RunAction::Stop),
        ("restart", RunAction::Restart),
    ] {
        let editor = FakeEditor::answering(|_| {
            vec![
                CliResponse::Stdout {
                    message: "Done 'api server'.".into(),
                },
                CliResponse::Exit { status: 0 },
            ]
        });
        let output = zedcli(editor.data_dir.path(), &[command, "api server"], None, None);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(stdout(&output).contains("Done 'api server'."));
        let CliRequest::ControlRun {
            configuration,
            action: asked,
            ..
        } = editor.request()
        else {
            panic!("{command} asks to control a run");
        };
        assert_eq!(configuration, "api server");
        assert_eq!(asked, action, "{command}");
    }
}

#[test]
fn the_status_the_editor_answers_is_the_status_zedcli_exits_with() {
    let editor = FakeEditor::answering(|_| {
        vec![
            CliResponse::Stderr {
                message: "No run configuration named 'nope'.".into(),
            },
            CliResponse::Exit {
                status: exit_status::NOT_FOUND,
            },
        ]
    });
    let output = zedcli(editor.data_dir.path(), &["run", "nope"], None, None);
    assert_eq!(output.status.code(), Some(exit_status::NOT_FOUND));
    assert!(stderr(&output).contains("No run configuration named 'nope'."));
    editor.request();
}

#[test]
fn sql_is_read_from_stdin_and_printed_as_csv() {
    let editor = FakeEditor::answering(|_| {
        vec![
            CliResponse::QueryResult {
                columns: vec!["id".into(), "name".into()],
                rows: vec![
                    vec![Some("1".into()), Some("Ada".into())],
                    vec![Some("2".into()), None],
                ],
                rows_affected: 0,
                execution_time_ms: 3,
            },
            CliResponse::Exit { status: 0 },
        ]
    });
    let output = zedcli(
        editor.data_dir.path(),
        &["db", "query", "-c", "local", "-d", "shop", "--csv"],
        Some("SELECT id, name FROM users"),
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "id,name\n1,Ada\n2,\n");
    assert_eq!(
        editor.request(),
        CliRequest::ExecuteQuery {
            connection: "local".into(),
            database: Some("shop".into()),
            sql: "SELECT id, name FROM users".into(),
        }
    );
}

#[test]
fn a_query_prints_a_table_with_its_row_count() {
    let editor = FakeEditor::answering(|_| {
        vec![
            CliResponse::QueryResult {
                columns: vec!["n".into()],
                rows: vec![vec![Some("1".into())]],
                rows_affected: 0,
                execution_time_ms: 2,
            },
            CliResponse::Exit { status: 0 },
        ]
    });
    let output = zedcli(
        editor.data_dir.path(),
        &["db", "query", "-c", "local", "SELECT 1 AS n"],
        None,
        None,
    );
    assert_eq!(stdout(&output), "n\n1\n(1 row, 2 ms)\n");
    editor.request();
}

#[test]
fn connections_are_listed_without_anything_secret() {
    let editor = FakeEditor::answering(|_| {
        vec![
            CliResponse::Connections {
                items: vec![DbConnectionSummary {
                    id: "5b1f".into(),
                    label: "local".into(),
                    driver: "MySQL".into(),
                }],
            },
            CliResponse::Exit { status: 0 },
        ]
    });
    let output = zedcli(
        editor.data_dir.path(),
        &["db", "connections", "--json"],
        None,
        None,
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(value[0]["label"], "local");
    assert_eq!(editor.request(), CliRequest::ListConnections);
}

#[test]
fn a_file_of_sql_that_cannot_be_read_is_a_bad_argument() {
    let data_dir = TempDir::new().expect("a data directory");
    let output = zedcli(
        data_dir.path(),
        &["db", "query", "-c", "local", "--file", "/no/such/file.sql"],
        None,
        None,
    );
    assert_eq!(output.status.code(), Some(exit_status::BAD_ARGUMENTS));
    assert!(
        stderr(&output).contains("cannot read"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_unknown_command_is_a_bad_argument() {
    let data_dir = TempDir::new().expect("a data directory");
    let output = zedcli(data_dir.path(), &["frobnicate"], None, None);
    assert_eq!(output.status.code(), Some(exit_status::BAD_ARGUMENTS));
}

#[test]
fn no_running_editor_is_said_as_such_and_starts_nothing() {
    let data_dir = TempDir::new().expect("a data directory");
    let output = zedcli(data_dir.path(), &["windows"], None, None);
    assert_eq!(output.status.code(), Some(exit_status::EDITOR_UNREACHABLE));
    assert!(
        stderr(&output).contains("no running editor"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_editor_that_never_answers_times_out() {
    let (data_dir, _socket) = FakeEditor::silent();
    let started = std::time::Instant::now();
    let output = zedcli(data_dir.path(), &["windows", "--timeout", "1"], None, None);
    assert_eq!(output.status.code(), Some(exit_status::EDITOR_UNREACHABLE));
    assert!(
        stderr(&output).contains("did not answer"),
        "{}",
        stderr(&output)
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the wait is bounded"
    );
}
