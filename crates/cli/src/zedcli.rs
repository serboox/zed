use std::io::{IsTerminal as _, Read as _};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use clap::{Parser, Subcommand};
use cli::{
    CliRequest, CliResponse, ConfigurationInfo, DebugSessionInfo, IpcHandshake, RunAction, RunInfo,
    RunState, WindowInfo, WindowSelector, exit_status, ipc::IpcOneShotServer,
};
use serde_json::json;

/// Talk to the running editor from a terminal, a script or an agent.
#[derive(Parser, Debug)]
#[command(name = "zedcli", version, about)]
pub struct ZedCli {
    #[command(subcommand)]
    command: Command,
    /// Print machine-readable JSON instead of a table.
    #[arg(long, global = true)]
    json: bool,
    /// The window to act on, by the id `zedcli windows` shows.
    #[arg(long, global = true, conflicts_with_all = ["project", "all"])]
    window: Option<u64>,
    /// The window that has this project (or a folder inside it) open.
    #[arg(long, global = true, conflicts_with = "all")]
    project: Option<PathBuf>,
    /// Every window, instead of the one holding the current directory.
    #[arg(long, global = true)]
    all: bool,
    /// The editor's data directory, when it was started with `--user-data-dir`.
    #[arg(long, global = true)]
    user_data_dir: Option<PathBuf>,
    /// Seconds to wait for the editor to answer.
    #[arg(long, global = true, default_value_t = 30)]
    timeout: u64,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List the editor's windows, the projects open in each and the active file.
    Windows,
    /// List task runs (with their process trees) and debug sessions.
    Ps,
    /// List the run configurations of the selected window's projects.
    Configs,
    /// Start a run configuration, stopping a run of it that is still going.
    Run { configuration: String },
    /// Stop every run of a configuration, with everything it started.
    Stop { configuration: String },
    /// Stop a configuration's runs, then start it again.
    Restart { configuration: String },
    /// Database Explorer: saved connections and SQL.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
}

#[derive(Subcommand, Debug)]
enum DbCommand {
    /// List the saved connections (never their credentials).
    Connections,
    /// Run SQL against a saved connection and print the result.
    Query {
        /// Connection id or label, as `zedcli db connections` shows.
        #[arg(short, long)]
        connection: String,
        /// Database to run against; the connection's own by default.
        #[arg(short, long)]
        database: Option<String>,
        /// Read the SQL from this file.
        #[arg(short, long, conflicts_with = "sql")]
        file: Option<PathBuf>,
        /// Print rows as CSV.
        #[arg(long, conflicts_with_all = ["tsv", "json"])]
        csv: bool,
        /// Print rows as tab-separated values.
        #[arg(long, conflicts_with = "json")]
        tsv: bool,
        /// The SQL; read from stdin when neither this nor --file is given.
        sql: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowFormat {
    Table,
    Json,
    Csv,
    Tsv,
}

/// Runs `zedcli` with the arguments after the program name, and exits with the
/// status the editor answered.
pub fn main(args: impl IntoIterator<Item = std::ffi::OsString>) -> ! {
    let cli = match ZedCli::try_parse_from(std::iter::once("zedcli".into()).chain(args)) {
        Ok(cli) => cli,
        Err(error) => {
            let status = match error.use_stderr() {
                true => exit_status::BAD_ARGUMENTS,
                false => 0,
            };
            error.print().ok();
            std::process::exit(status);
        }
    };
    let status = match run(cli) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("zedcli: {error:#}");
            exit_status::EDITOR_UNREACHABLE
        }
    };
    std::process::exit(status);
}

fn run(cli: ZedCli) -> Result<i32> {
    let selector = WindowSelector {
        window: cli.window,
        project: cli.project.clone(),
        cwd: std::env::current_dir().ok(),
        all: cli.all,
    };
    let mut format = match cli.json {
        true => RowFormat::Json,
        false => RowFormat::Table,
    };
    let request = match cli.command {
        Command::Windows => CliRequest::ListWindows,
        Command::Ps => CliRequest::ListRuns { selector },
        Command::Configs => CliRequest::ListConfigurations { selector },
        Command::Run { configuration } => CliRequest::ControlRun {
            selector,
            configuration,
            action: RunAction::Run,
        },
        Command::Stop { configuration } => CliRequest::ControlRun {
            selector,
            configuration,
            action: RunAction::Stop,
        },
        Command::Restart { configuration } => CliRequest::ControlRun {
            selector,
            configuration,
            action: RunAction::Restart,
        },
        Command::Db { command } => match command {
            DbCommand::Connections => CliRequest::ListConnections,
            DbCommand::Query {
                connection,
                database,
                file,
                csv,
                tsv,
                sql,
            } => {
                if csv {
                    format = RowFormat::Csv;
                } else if tsv {
                    format = RowFormat::Tsv;
                }
                let sql = match (sql, file) {
                    (Some(sql), _) => sql,
                    (None, Some(file)) => match std::fs::read_to_string(&file) {
                        Ok(sql) => sql,
                        Err(error) => {
                            eprintln!("zedcli: cannot read {}: {error}", file.display());
                            return Ok(exit_status::BAD_ARGUMENTS);
                        }
                    },
                    (None, None) => {
                        if std::io::stdin().is_terminal() {
                            eprintln!(
                                "zedcli: give the SQL as an argument, with --file, or on stdin"
                            );
                            return Ok(exit_status::BAD_ARGUMENTS);
                        }
                        let mut sql = String::new();
                        std::io::stdin().read_to_string(&mut sql)?;
                        sql
                    }
                };
                if sql.trim().is_empty() {
                    eprintln!("zedcli: there is no SQL to run");
                    return Ok(exit_status::BAD_ARGUMENTS);
                }
                CliRequest::ExecuteQuery {
                    connection,
                    database,
                    sql,
                }
            }
        },
    };
    exchange(
        request,
        cli.user_data_dir,
        Duration::from_secs(cli.timeout),
        format,
    )
}

/// Sends one request to the running editor and prints what it answers.
fn exchange(
    request: CliRequest,
    user_data_dir: Option<PathBuf>,
    timeout: Duration,
    format: RowFormat,
) -> Result<i32> {
    let (server, server_name) =
        IpcOneShotServer::<IpcHandshake>::new().context("opening a channel for the editor")?;
    reach_the_editor(format!("zed-cli://{server_name}"), user_data_dir)?;

    let (accepted_tx, accepted_rx) = mpsc::channel();
    std::thread::spawn(move || {
        accepted_tx.send(server.accept()).ok();
    });
    let handshake = match accepted_rx.recv_timeout(timeout) {
        Ok(Ok((_, handshake))) => handshake,
        Ok(Err(error)) => return Err(anyhow!("the editor did not connect back: {error}")),
        Err(_) => {
            return Err(anyhow!(
                "the editor did not answer within {}s",
                timeout.as_secs()
            ));
        }
    };
    handshake
        .requests
        .send(request)
        .context("sending the request to the editor")?;

    let (response_tx, response_rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(response) = handshake.responses.recv() {
            if response_tx.send(response).is_err() {
                break;
            }
        }
    });
    loop {
        let response = match response_rx.recv_timeout(timeout) {
            Ok(response) => response,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(anyhow!(
                    "the editor stopped answering for {}s",
                    timeout.as_secs()
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow!(
                    "the editor closed the connection without an answer"
                ));
            }
        };
        match response {
            CliResponse::Exit { status } => return Ok(status),
            CliResponse::Stdout { message } => println!("{message}"),
            CliResponse::Stderr { message } => eprintln!("{message}"),
            CliResponse::Ping | CliResponse::PromptOpenBehavior => {}
            CliResponse::QueryResult {
                columns,
                rows,
                rows_affected,
                execution_time_ms,
            } => print!(
                "{}",
                rows_as(format, &columns, &rows, rows_affected, execution_time_ms)
            ),
            CliResponse::Connections { items } => {
                let text = match format {
                    RowFormat::Table => table(
                        &["ID", "LABEL", "DRIVER"],
                        items
                            .iter()
                            .map(|item| {
                                vec![item.id.clone(), item.label.clone(), item.driver.clone()]
                            })
                            .collect(),
                    ),
                    _ => json_line(&items),
                };
                print!("{text}");
            }
            CliResponse::Windows { items } => print!("{}", windows_as(format, &items)),
            CliResponse::Runs {
                runs,
                debug_sessions,
            } => print!("{}", runs_as(format, &runs, &debug_sessions)),
            CliResponse::Configurations { items } => {
                print!("{}", configurations_as(format, &items))
            }
        }
    }
}

/// Hands the editor the address to connect back to. Only an editor that is
/// already running is asked: starting a whole editor window is not something a
/// command meant to answer a question should do on its own.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn reach_the_editor(url: String, user_data_dir: Option<PathBuf>) -> Result<()> {
    use std::os::unix::net::UnixDatagram;

    let data_dir = user_data_dir.unwrap_or_else(|| paths::data_dir().clone());
    let socket = data_dir.join(format!(
        "zed-{}.sock",
        *release_channel::RELEASE_CHANNEL_NAME
    ));
    let sender = UnixDatagram::unbound()?;
    sender.connect(&socket).map_err(|error| {
        anyhow!(
            "no running editor answers at {} ({error}). Start Zed (Fast/DB dev) first.",
            socket.display()
        )
    })?;
    sender.send(url.as_bytes())?;
    Ok(())
}

/// Elsewhere the only way in is the one that starts an editor when none is
/// running, which a question must not do.
#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
fn reach_the_editor(_url: String, _user_data_dir: Option<PathBuf>) -> Result<()> {
    Err(anyhow!("zedcli only runs on Linux for now"))
}

fn json_line(value: &impl serde::Serialize) -> String {
    match serde_json::to_string_pretty(value) {
        Ok(text) => format!("{text}\n"),
        Err(error) => format!("{{\"error\": \"{error}\"}}\n"),
    }
}

/// Columns padded to their widest cell, a header, and nothing else: the shape
/// `ps` and `docker ps` print, which reads well and greps well.
fn table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in &rows {
        for (at, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(at) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }
    let line = |cells: Vec<String>| {
        let last = cells.len().saturating_sub(1);
        let mut text = String::new();
        for (at, cell) in cells.into_iter().enumerate() {
            text.push_str(&cell);
            if at < last {
                let width = widths.get(at).copied().unwrap_or(0);
                text.push_str(&" ".repeat(width.saturating_sub(cell.chars().count()) + 2));
            }
        }
        text.trim_end().to_string() + "\n"
    };
    let mut text = line(headers.iter().map(|header| header.to_string()).collect());
    for row in rows {
        text.push_str(&line(row));
    }
    text
}

fn cell(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("NULL")
}

fn delimited(value: &str, separator: char) -> String {
    let needs_quotes = value.contains(separator)
        || value.contains('"')
        || value.contains('\n')
        || value.contains('\r');
    match (separator, needs_quotes) {
        (',', true) => format!("\"{}\"", value.replace('"', "\"\"")),
        ('\t', true) => value
            .replace('\\', "\\\\")
            .replace('\t', "\\t")
            .replace('\n', "\\n")
            .replace('\r', "\\r"),
        _ => value.to_string(),
    }
}

fn rows_as(
    format: RowFormat,
    columns: &[String],
    rows: &[Vec<Option<String>>],
    rows_affected: u64,
    execution_time_ms: u64,
) -> String {
    match format {
        RowFormat::Json => {
            let objects: Vec<serde_json::Value> = rows
                .iter()
                .map(|row| {
                    let object: serde_json::Map<String, serde_json::Value> = columns
                        .iter()
                        .zip(row)
                        .map(|(column, value)| (column.clone(), json!(value)))
                        .collect();
                    serde_json::Value::Object(object)
                })
                .collect();
            json_line(&json!({
                "columns": columns,
                "rows": objects,
                "rows_affected": rows_affected,
                "execution_time_ms": execution_time_ms,
            }))
        }
        RowFormat::Csv | RowFormat::Tsv => {
            let separator = match format {
                RowFormat::Csv => ',',
                _ => '\t',
            };
            let join = |cells: Vec<String>| cells.join(&separator.to_string()) + "\n";
            let mut text = join(
                columns
                    .iter()
                    .map(|column| delimited(column, separator))
                    .collect(),
            );
            for row in rows {
                text.push_str(&join(
                    row.iter()
                        .map(|value| match value {
                            Some(value) => delimited(value, separator),
                            None => String::new(),
                        })
                        .collect(),
                ));
            }
            text
        }
        RowFormat::Table => {
            if columns.is_empty() {
                return format!("{rows_affected} rows affected ({execution_time_ms} ms)\n");
            }
            let headers: Vec<&str> = columns.iter().map(String::as_str).collect();
            let body = rows
                .iter()
                .map(|row| row.iter().map(|value| cell(value).to_string()).collect())
                .collect();
            let count = rows.len();
            let noun = match count {
                1 => "row",
                _ => "rows",
            };
            format!(
                "{}({count} {noun}, {execution_time_ms} ms)\n",
                table(&headers, body)
            )
        }
    }
}

fn windows_as(format: RowFormat, windows: &[WindowInfo]) -> String {
    if format != RowFormat::Table {
        return json_line(&windows);
    }
    let mut rows = Vec::new();
    for window in windows {
        for (at, workspace) in window.workspaces.iter().enumerate() {
            let projects = workspace
                .projects
                .iter()
                .map(|project| tilde(project))
                .collect::<Vec<_>>()
                .join(", ");
            rows.push(vec![
                match at {
                    0 => window.id.to_string(),
                    _ => String::new(),
                },
                match (at, window.focused) {
                    (0, true) => "*".to_string(),
                    _ => String::new(),
                },
                match workspace.active {
                    true => projects,
                    false => format!("({projects})"),
                },
                workspace
                    .active_path
                    .as_ref()
                    .map(|path| tilde(path))
                    .unwrap_or_default(),
            ]);
        }
        if window.workspaces.is_empty() {
            rows.push(vec![
                window.id.to_string(),
                String::new(),
                String::new(),
                String::new(),
            ]);
        }
    }
    table(&["WINDOW", "FOCUS", "PROJECTS", "ACTIVE FILE"], rows)
}

fn tilde(path: &std::path::Path) -> String {
    let home = util::paths::home_dir();
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn state_of(state: RunState) -> &'static str {
    match state {
        RunState::Running => "running",
        RunState::Succeeded => "succeeded",
        RunState::Failed => "failed",
        RunState::Unknown => "unknown",
    }
}

fn memory(bytes: u64) -> String {
    const MIB: f64 = 1024. * 1024.;
    match bytes as f64 / MIB {
        mib if mib >= 1024. => format!("{:.1} GB", mib / 1024.),
        mib => format!("{mib:.0} MB"),
    }
}

fn runs_as(format: RowFormat, runs: &[RunInfo], sessions: &[DebugSessionInfo]) -> String {
    if format != RowFormat::Table {
        return json_line(&json!({ "runs": runs, "debug_sessions": sessions }));
    }
    let mut rows = Vec::new();
    for run in runs {
        rows.push(vec![
            run.window.to_string(),
            run.label.clone(),
            run.pid.map(|pid| pid.to_string()).unwrap_or_default(),
            state_of(run.state).to_string(),
            String::new(),
            String::new(),
            run.command.clone(),
        ]);
        for process in run.processes.iter().skip(1) {
            let depth = depth_of(process.pid, &run.processes);
            rows.push(vec![
                String::new(),
                format!("{}└ {}", "  ".repeat(depth.saturating_sub(1)), process.name),
                process.pid.to_string(),
                process.state.clone(),
                process
                    .cpu_percent
                    .map(|cpu| format!("{cpu:.1}%"))
                    .unwrap_or_default(),
                memory(process.memory_bytes),
                format!("{} threads", process.threads),
            ]);
        }
    }
    let mut text = match rows.is_empty() {
        true => "No task runs.\n".to_string(),
        false => table(
            &["WINDOW", "RUN", "PID", "STATE", "CPU", "MEMORY", "COMMAND"],
            rows,
        ),
    };
    if !sessions.is_empty() {
        text.push('\n');
        text.push_str(&table(
            &["WINDOW", "DEBUG SESSION", "ADAPTER", "STATE"],
            sessions
                .iter()
                .map(|session| {
                    vec![
                        session.window.to_string(),
                        session.label.clone(),
                        session.adapter.clone(),
                        session.state.clone(),
                    ]
                })
                .collect(),
        ));
    }
    text
}

/// How many parents stand between a process and the root of its run.
fn depth_of(pid: u32, processes: &[cli::ProcessInfo]) -> usize {
    let Some(root) = processes.first().map(|process| process.pid) else {
        return 0;
    };
    let mut depth = 0;
    let mut at = pid;
    while at != root && depth < processes.len() {
        let Some(process) = processes.iter().find(|process| process.pid == at) else {
            break;
        };
        at = process.parent;
        depth += 1;
    }
    depth
}

fn configurations_as(format: RowFormat, items: &[ConfigurationInfo]) -> String {
    if format != RowFormat::Table {
        return json_line(&items);
    }
    if items.is_empty() {
        return "No run configurations.\n".to_string();
    }
    table(
        &["WINDOW", "CONFIGURATION", "KIND", "RUNNING", "COMMAND"],
        items
            .iter()
            .map(|item| {
                vec![
                    item.window.to_string(),
                    item.label.clone(),
                    item.kind.clone(),
                    match item.running {
                        true => "yes".to_string(),
                        false => String::new(),
                    },
                    item.command.clone(),
                ]
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_pads_every_column_to_its_widest_cell() {
        let text = table(
            &["ID", "LABEL"],
            vec![
                vec!["1".into(), "local".into()],
                vec!["22".into(), "prod-replica".into()],
            ],
        );
        assert_eq!(text, "ID  LABEL\n1   local\n22  prod-replica\n");
    }

    #[test]
    fn csv_quotes_only_what_needs_it() {
        let text = rows_as(
            RowFormat::Csv,
            &["id".into(), "note".into()],
            &[
                vec![Some("1".into()), Some("plain".into())],
                vec![Some("2".into()), Some("with, comma and \"quote\"".into())],
                vec![Some("3".into()), None],
            ],
            0,
            1,
        );
        assert_eq!(
            text,
            "id,note\n1,plain\n2,\"with, comma and \"\"quote\"\"\"\n3,\n"
        );
    }

    #[test]
    fn tsv_escapes_tabs_and_newlines_so_a_row_stays_one_line() {
        let text = rows_as(
            RowFormat::Tsv,
            &["note".into()],
            &[vec![Some("a\tb\nc".into())]],
            0,
            1,
        );
        assert_eq!(text, "note\na\\tb\\nc\n");
    }

    #[test]
    fn json_rows_are_objects_keyed_by_column_with_null_for_null() {
        let text = rows_as(
            RowFormat::Json,
            &["id".into(), "name".into()],
            &[vec![Some("1".into()), None]],
            0,
            4,
        );
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(value["rows"][0]["id"], "1");
        assert!(value["rows"][0]["name"].is_null());
        assert_eq!(value["execution_time_ms"], 4);
    }

    #[test]
    fn a_statement_without_rows_says_how_many_it_changed() {
        let text = rows_as(RowFormat::Table, &[], &[], 3, 12);
        assert_eq!(text, "3 rows affected (12 ms)\n");
    }

    #[test]
    fn a_process_tree_is_indented_by_depth() {
        let process = |pid, parent| cli::ProcessInfo {
            pid,
            parent,
            name: format!("p{pid}"),
            cpu_percent: None,
            memory_bytes: 0,
            threads: 1,
            state: "S".into(),
        };
        let tree = vec![process(10, 1), process(11, 10), process(12, 11)];
        assert_eq!(depth_of(10, &tree), 0);
        assert_eq!(depth_of(11, &tree), 1);
        assert_eq!(depth_of(12, &tree), 2);
    }

    #[test]
    fn global_flags_parse_after_the_subcommand() {
        let cli =
            ZedCli::try_parse_from(["zedcli", "ps", "--json", "--window", "7"]).expect("parses");
        assert!(cli.json);
        assert_eq!(cli.window, Some(7));
        assert!(matches!(cli.command, Command::Ps));
    }

    #[test]
    fn a_window_and_every_window_cannot_both_be_asked_for() {
        assert!(ZedCli::try_parse_from(["zedcli", "ps", "--all", "--window", "7"]).is_err());
    }
}
