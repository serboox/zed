use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cli::{
    ApiEnvironmentInfo, ApiRequestInfo, ApiResponseInfo, ApiTestInfo, CliRequest, CliResponse,
    CliResponseSink, ConfigurationInfo, DebugSessionInfo, ProcessInfo, RunAction, RunInfo,
    RunState, WindowInfo, WindowSelector, WorkspaceInfo, exit_status,
};
use gpui::{AnyWindowHandle, App, AsyncApp, Entity, WindowHandle};
use run_configurations::configurations_file::Kind;
use run_configurations::{
    configurations_store, configurations_view, process_metrics, run_instances,
};
use terminal::TaskStatus;
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace};

/// Whether zedcli may ask this editor anything, as the reader set it.
#[derive(Clone, Debug, settings::RegisterSetting)]
pub struct ZedcliSettings {
    pub enabled: bool,
}

impl settings::Settings for ZedcliSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            enabled: content
                .zedcli
                .as_ref()
                .and_then(|configured| configured.enabled)
                .unwrap_or(true),
        }
    }
}

/// The token this editor wrote to its data directory for zedcli to present.
static TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Writes a fresh token beside the CLI socket, readable by this user only, for
/// every request that reaches the editor's state or data to carry. Written to
/// a new file and renamed into place, so a reader never sees half of one.
pub fn issue_token(data_dir: &Path) -> anyhow::Result<()> {
    use std::io::Write as _;

    let token = TOKEN.get_or_init(|| {
        format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        )
    });
    let path = cli::token_path(data_dir);
    let writing = path.with_extension("token.writing");
    match std::fs::remove_file(&writing) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let written = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Elsewhere the file sits in the user's own profile, which only they
        // can read.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&writing)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&writing, &path)
    })();
    if written.is_err()
        && let Err(error) = std::fs::remove_file(&writing)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("leaving {} behind: {error}", writing.display());
    }
    Ok(written?)
}

/// Compared in full whatever the first difference, so how long an answer
/// takes says nothing about how much of a guess was right.
fn token_is(given: &str) -> bool {
    let Some(token) = TOKEN.get() else {
        return false;
    };
    token.len() == given.len()
        && token
            .bytes()
            .zip(given.bytes())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

#[cfg(test)]
pub(crate) fn token_for_tests() -> String {
    TOKEN
        .get_or_init(|| "a-token-for-tests".to_string())
        .clone()
}

/// Lets a request through, unwrapped, or says why it is not: a request that
/// reaches the editor's state or data must carry this editor's token, and none
/// gets through while zedcli is turned off in the settings.
pub fn admit(request: CliRequest, cx: &mut AsyncApp) -> Result<CliRequest, (String, i32)> {
    match request {
        CliRequest::Authenticated { token, request } => {
            if !token_is(&token) {
                return Err((
                    "The token does not match this editor's: zedcli is reaching a different \
                     or restarted instance."
                        .to_string(),
                    exit_status::EDITOR_UNREACHABLE,
                ));
            }
            if matches!(*request, CliRequest::Authenticated { .. }) {
                return Err((
                    "A request is wrapped only once.".to_string(),
                    exit_status::BAD_ARGUMENTS,
                ));
            }
            let enabled = cx.update(|cx| {
                use settings::Settings as _;
                ZedcliSettings::get_global(cx).enabled
            });
            if !enabled {
                return Err((
                    "zedcli is turned off in this editor's settings (\"zedcli\": { \"enabled\": \
                     false })."
                        .to_string(),
                    exit_status::REFUSED,
                ));
            }
            Ok(*request)
        }
        request if request.needs_token() => Err((
            "This request must carry the editor's token; send it with zedcli.".to_string(),
            exit_status::EDITOR_UNREACHABLE,
        )),
        request => Ok(request),
    }
}

/// How far apart the two readings a processor share is worked out from are.
/// Long enough for a busy process to show, short enough not to keep the reader
/// waiting on a list.
const CPU_SAMPLED_OVER: Duration = Duration::from_millis(300);

/// How long a request waits for a project's run configuration files to be read:
/// this many looks, [`STORE_LOOKED_AT_EVERY`] apart. Counted rather than timed,
/// so the wait ends under a test clock too.
const STORE_LOOKS: usize = 200;
const STORE_LOOKED_AT_EVERY: Duration = Duration::from_millis(50);

/// The configuration store of every workspace in `windows`, once each has
/// read its files, or why one has not.
async fn read_stores(
    windows: &[WindowHandle<MultiWorkspace>],
    cx: &mut AsyncApp,
) -> Result<
    Vec<(
        u64,
        Entity<Workspace>,
        Entity<configurations_store::ConfigurationsStore>,
    )>,
    String,
> {
    let stores = cx.update(|cx| {
        let mut stores = Vec::new();
        for window in windows {
            for workspace in workspaces_of(window, cx) {
                let project = workspace.read(cx).project().clone();
                let store = configurations_store::store_for(&project, cx);
                stores.push((window.window_id().as_u64(), workspace, store));
            }
        }
        stores
    });
    for _ in 0..STORE_LOOKS {
        let all_read = cx.update(|cx| {
            stores
                .iter()
                .all(|(_, _, store)| store.read(cx).has_read_its_files())
        });
        if all_read {
            return Ok(stores);
        }
        cx.background_executor().timer(STORE_LOOKED_AT_EVERY).await;
    }
    Err("The project's run configurations are still being read; try again.".to_string())
}

pub(crate) fn say_and_exit(responses: &dyn CliResponseSink, message: String, status: i32) {
    responses.send(CliResponse::Stderr { message }).log_err();
    responses.send(CliResponse::Exit { status }).log_err();
}

fn editor_windows(cx: &App) -> Vec<WindowHandle<MultiWorkspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .collect()
}

fn workspaces_of(window: &WindowHandle<MultiWorkspace>, cx: &App) -> Vec<Entity<Workspace>> {
    window
        .read(cx)
        .map(|multi| multi.workspaces().cloned().collect())
        .unwrap_or_default()
}

fn project_roots(workspace: &Entity<Workspace>, cx: &App) -> Vec<PathBuf> {
    workspace
        .read(cx)
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect()
}

fn holds(roots: &[PathBuf], path: &Path) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// The windows `selector` names, or why it names none.
fn selected_windows(
    selector: &WindowSelector,
    cx: &App,
) -> Result<Vec<WindowHandle<MultiWorkspace>>, String> {
    let windows = editor_windows(cx);
    if windows.is_empty() {
        return Err("No editor window is open.".to_string());
    }
    if selector.all {
        return Ok(windows);
    }
    let roots_of = |window: &WindowHandle<MultiWorkspace>| -> Vec<PathBuf> {
        workspaces_of(window, cx)
            .iter()
            .flat_map(|workspace| project_roots(workspace, cx))
            .collect()
    };
    if let Some(id) = selector.window {
        return windows
            .into_iter()
            .find(|window| window.window_id().as_u64() == id)
            .map(|window| vec![window])
            .ok_or_else(|| format!("No editor window with id {id}. See `zedcli windows`."));
    }
    if let Some(project) = &selector.project {
        let project = project.canonicalize().unwrap_or_else(|_| project.clone());
        return windows
            .into_iter()
            .find(|window| holds(&roots_of(window), &project))
            .map(|window| vec![window])
            .ok_or_else(|| {
                format!(
                    "No editor window has {} open. See `zedcli windows`.",
                    project.display()
                )
            });
    }
    if let Some(cwd) = &selector.cwd
        && let Some(window) = windows.iter().find(|window| holds(&roots_of(window), cwd))
    {
        return Ok(vec![*window]);
    }
    let active = cx
        .active_window()
        .and_then(|window| window.downcast::<MultiWorkspace>());
    Ok(active.or(windows.first().copied()).into_iter().collect())
}

pub fn list_windows(responses: &dyn CliResponseSink, cx: &mut AsyncApp) {
    let items = cx.update(|cx| {
        let focused = cx.active_window().map(|window| window.window_id());
        editor_windows(cx)
            .iter()
            .map(|window| {
                let active_workspace = window
                    .read(cx)
                    .ok()
                    .map(|multi| multi.workspace().entity_id());
                WindowInfo {
                    id: window.window_id().as_u64(),
                    focused: focused == Some(window.window_id()),
                    workspaces: workspaces_of(window, cx)
                        .iter()
                        .map(|workspace| WorkspaceInfo {
                            active: active_workspace == Some(workspace.entity_id()),
                            projects: project_roots(workspace, cx),
                            active_path: active_path(workspace, cx),
                        })
                        .collect(),
                }
            })
            .collect::<Vec<_>>()
    });
    responses.send(CliResponse::Windows { items }).log_err();
    responses.send(CliResponse::Exit { status: 0 }).log_err();
}

fn active_path(workspace: &Entity<Workspace>, cx: &App) -> Option<PathBuf> {
    let workspace = workspace.read(cx);
    let project_path = workspace.active_item(cx)?.project_path(cx)?;
    workspace
        .project()
        .read(cx)
        .absolute_path(&project_path, cx)
}

struct RunSeen {
    window: u64,
    label: String,
    command: String,
    state: RunState,
    shell: Option<u32>,
}

pub async fn list_runs(
    selector: WindowSelector,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    let seen = cx.update(|cx| {
        let windows = selected_windows(&selector, cx)?;
        let mut runs = Vec::new();
        let mut sessions = Vec::new();
        for window in windows {
            let id = window.window_id().as_u64();
            for workspace in workspaces_of(&window, cx) {
                for terminal in run_instances::task_terminals(workspace.read(cx), cx) {
                    let terminal = terminal.read(cx);
                    let Some(task) = terminal.task() else {
                        continue;
                    };
                    let command =
                        std::iter::once(task.spawned_task.command.clone().unwrap_or_default())
                            .chain(task.spawned_task.args.iter().cloned())
                            .collect::<Vec<_>>()
                            .join(" ");
                    runs.push(RunSeen {
                        window: id,
                        label: task.spawned_task.label.clone(),
                        command,
                        state: match task.status {
                            TaskStatus::Running => RunState::Running,
                            TaskStatus::Completed { success: true } => RunState::Succeeded,
                            TaskStatus::Completed { success: false } => RunState::Failed,
                            TaskStatus::Unknown => RunState::Unknown,
                        },
                        shell: terminal
                            .pid_getter()
                            .map(|getter| getter.fallback_pid().as_u32()),
                    });
                }
                let project = workspace.read(cx).project().clone();
                let dap_store = project.read(cx).dap_store();
                for session in dap_store.read(cx).sessions() {
                    let session = session.read(cx);
                    let state = if session.is_terminated() {
                        "terminated"
                    } else if session.is_building() {
                        "building"
                    } else if session.is_started() {
                        "running"
                    } else {
                        "starting"
                    };
                    sessions.push(DebugSessionInfo {
                        window: id,
                        label: session
                            .label()
                            .map(|label| label.to_string())
                            .unwrap_or_default(),
                        adapter: session.adapter().to_string(),
                        state: state.to_string(),
                    });
                }
            }
        }
        Ok::<_, String>((runs, sessions))
    });
    let (seen, debug_sessions) = match seen {
        Ok(seen) => seen,
        Err(message) => return say_and_exit(responses, message, exit_status::NOT_FOUND),
    };

    let roots: Vec<Option<u32>> = seen
        .iter()
        .map(|run| {
            (run.state == RunState::Running)
                .then_some(run.shell)
                .flatten()
        })
        .collect();
    let executor = cx.background_executor().clone();
    let trees = cx
        .background_executor()
        .spawn(async move { trees_of(&roots, &executor).await })
        .await;

    let runs = seen
        .into_iter()
        .zip(trees)
        .map(|(run, processes)| RunInfo {
            window: run.window,
            label: run.label,
            command: run.command,
            state: run.state,
            pid: run.shell,
            processes,
        })
        .collect();
    responses
        .send(CliResponse::Runs {
            runs,
            debug_sessions,
        })
        .log_err();
    responses.send(CliResponse::Exit { status: 0 }).log_err();
}

/// Each run's process tree, read twice [`CPU_SAMPLED_OVER`] apart so each
/// process carries its share of a core rather than nothing.
async fn trees_of(
    roots: &[Option<u32>],
    executor: &gpui::BackgroundExecutor,
) -> Vec<Vec<ProcessInfo>> {
    if roots.iter().all(Option::is_none) {
        return roots.iter().map(|_| Vec::new()).collect();
    }
    let mut watchers: Vec<process_metrics::Watcher> = roots
        .iter()
        .map(|_| process_metrics::Watcher::default())
        .collect();
    let first = process_metrics::everything_running().unwrap_or_default();
    let first_at = Instant::now();
    for (watcher, root) in watchers.iter_mut().zip(roots) {
        if let Some(root) = root {
            watcher.metrics_of(*root, &first, first_at, process_metrics::machine_uptime());
        }
    }
    executor.timer(CPU_SAMPLED_OVER).await;
    let second = process_metrics::everything_running().unwrap_or_default();
    let second_at = Instant::now();
    watchers
        .iter_mut()
        .zip(roots)
        .map(|(watcher, root)| {
            let Some(root) = root else {
                return Vec::new();
            };
            watcher
                .metrics_of(*root, &second, second_at, process_metrics::machine_uptime())
                .map(|metrics| {
                    metrics
                        .tree
                        .into_iter()
                        .map(|process| ProcessInfo {
                            pid: process.pid,
                            parent: process.parent,
                            name: process.name.to_string(),
                            cpu_percent: process.cpu,
                            memory_bytes: process.memory,
                            threads: process.threads,
                            state: process.state.to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect()
}

pub async fn list_configurations(
    selector: WindowSelector,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    let windows = match cx.update(|cx| selected_windows(&selector, cx)) {
        Ok(windows) => windows,
        Err(message) => return say_and_exit(responses, message, exit_status::NOT_FOUND),
    };
    let stores = match read_stores(&windows, cx).await {
        Ok(stores) => stores,
        Err(message) => return say_and_exit(responses, message, exit_status::FAILED),
    };
    let items = cx.update(|cx| {
        let mut items = Vec::new();
        for (window, workspace, store) in &stores {
            for kind in [Kind::Task, Kind::Debug] {
                for configuration in &store.read(cx).of_kind(kind).configurations {
                    let running = configuration.task.as_ref().is_some_and(|task| {
                        !run_instances::runs_of(workspace.read(cx), task, cx).is_empty()
                    });
                    items.push(ConfigurationInfo {
                        window: *window,
                        label: configuration.label.clone(),
                        kind: match kind {
                            Kind::Task => "task",
                            Kind::Debug => "debug",
                        }
                        .to_string(),
                        command: configuration
                            .task
                            .as_ref()
                            .map(|task| {
                                std::iter::once(task.command.clone())
                                    .chain(task.args.iter().cloned())
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .unwrap_or_default(),
                        running,
                    });
                }
            }
        }
        items
    });
    responses
        .send(CliResponse::Configurations { items })
        .log_err();
    responses.send(CliResponse::Exit { status: 0 }).log_err();
}

#[derive(Clone)]
enum Found {
    Task(Entity<Workspace>, AnyWindowHandle, task::TaskTemplate),
    Debug(Entity<Workspace>, AnyWindowHandle, task::DebugScenario),
}

async fn act_on(
    found: Found,
    action: RunAction,
    configuration: &str,
    cx: &mut AsyncApp,
) -> Result<(), (String, i32)> {
    match found {
        Found::Task(workspace, window, task) => {
            if matches!(action, RunAction::Stop | RunAction::Restart) {
                let stopping = workspace.update(cx, |workspace, cx| {
                    run_instances::stop_every_run_of(workspace, &task, cx)
                });
                if !stopping.await {
                    return Err((
                        format!(
                            "Some processes of '{configuration}' are still running after SIGKILL."
                        ),
                        exit_status::FAILED,
                    ));
                }
            }
            if matches!(action, RunAction::Run | RunAction::Restart) {
                let Ok(mut window_cx) = window.update(cx, |_, window, cx| window.to_async(cx))
                else {
                    return Err((
                        "The window closed before the run could start.".to_string(),
                        exit_status::FAILED,
                    ));
                };
                let weak = workspace.downgrade();
                if !configurations_view::run_a_task(&weak, task, &mut window_cx).await {
                    return Err((
                        format!("'{configuration}' could not be started; the editor says why."),
                        exit_status::FAILED,
                    ));
                }
            }
        }
        Found::Debug(workspace, window, scenario) => {
            if action != RunAction::Run {
                return Err((
                    format!(
                        "'{configuration}' is a debug configuration: stop or restart it from the \
                         debugger."
                    ),
                    exit_status::BAD_ARGUMENTS,
                ));
            }
            let Ok(mut window_cx) = window.update(cx, |_, window, cx| window.to_async(cx)) else {
                return Err((
                    "The window closed before the session could start.".to_string(),
                    exit_status::FAILED,
                ));
            };
            let weak = workspace.downgrade();
            if !configurations_view::start_a_debug_session(&weak, scenario, &mut window_cx).await {
                return Err((
                    format!("'{configuration}' could not be started; the editor says why."),
                    exit_status::FAILED,
                ));
            }
        }
    }
    Ok(())
}

pub async fn control_run(
    selector: WindowSelector,
    configuration: String,
    action: RunAction,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    if selector.all {
        return say_and_exit(
            responses,
            "Name one window for run, stop and restart, not --all.".to_string(),
            exit_status::BAD_ARGUMENTS,
        );
    }
    let windows = match cx.update(|cx| selected_windows(&selector, cx)) {
        Ok(windows) => windows,
        Err(message) => return say_and_exit(responses, message, exit_status::NOT_FOUND),
    };
    let stores = match read_stores(&windows, cx).await {
        Ok(stores) => stores,
        Err(message) => return say_and_exit(responses, message, exit_status::FAILED),
    };
    let found = cx.update(|cx| {
        let mut found = Vec::new();
        for (_, workspace, store) in stores {
            let window = editor_windows(cx).into_iter().find(|window| {
                workspaces_of(window, cx)
                    .iter()
                    .any(|candidate| candidate.entity_id() == workspace.entity_id())
            });
            let Some(window) = window else {
                continue;
            };
            let store = store.read(cx);
            for kind in [Kind::Task, Kind::Debug] {
                for candidate in store
                    .of_kind(kind)
                    .configurations
                    .iter()
                    .filter(|candidate| candidate.label == configuration)
                {
                    if let Some(task) = &candidate.task {
                        found.push(Found::Task(workspace.clone(), window.into(), task.clone()));
                    } else if let Some(scenario) = &candidate.scenario {
                        found.push(Found::Debug(
                            workspace.clone(),
                            window.into(),
                            scenario.clone(),
                        ));
                    }
                }
            }
        }
        found
    });
    if found.is_empty() {
        return say_and_exit(
            responses,
            format!("No run configuration named '{configuration}'. See `zedcli configs`."),
            exit_status::NOT_FOUND,
        );
    }
    // Stopping every configuration of that name is what a reader asking to
    // stop it means; starting one of several is a guess.
    if found.len() > 1 && action == RunAction::Run {
        return say_and_exit(
            responses,
            format!(
                "{} run configurations are named '{configuration}'; rename one to start it \
                 from zedcli.",
                found.len()
            ),
            exit_status::NOT_FOUND,
        );
    }
    // Every run is stopped before any is started again: configurations that
    // share a name can share their runs' label, and stopping the second would
    // end the first one's new run.
    let steps = match action {
        RunAction::Restart => vec![RunAction::Stop, RunAction::Run],
        action => vec![action],
    };
    for step in steps {
        for found in &found {
            let step = match (action, step, found) {
                // A debug configuration is refused as a whole, not stopped
                // half-way through a restart.
                (RunAction::Restart, _, Found::Debug(..)) => RunAction::Restart,
                _ => step,
            };
            if let Err((message, status)) = act_on(found.clone(), step, &configuration, cx).await {
                return say_and_exit(responses, message, status);
            }
        }
    }
    let done = match action {
        RunAction::Run => "Started",
        RunAction::Stop => "Stopped",
        RunAction::Restart => "Restarted",
    };
    responses
        .send(CliResponse::Stdout {
            message: format!("{done} '{configuration}'."),
        })
        .log_err();
    responses.send(CliResponse::Exit { status: 0 }).log_err();
}

/// The API client's store once its saved collections have been read, counted
/// like [`STORE_LOOKS`] so the wait ends under a test clock too.
pub(crate) async fn loaded_api_store(
    cx: &mut AsyncApp,
) -> Option<Entity<api_client_ui::ApiClientStore>> {
    for _ in 0..STORE_LOOKS {
        let store = cx.update(|cx| api_client_ui::ApiClientStore::global(cx));
        if let Some(store) = store
            && store.read_with(cx, |store, _| store.is_loaded())
        {
            return Some(store);
        }
        cx.background_executor().timer(STORE_LOOKED_AT_EVERY).await;
    }
    None
}

pub(crate) const API_NOT_READY: &str = "The API client is not ready: no editor window is open, or the \
     saved collections are still being read.";

pub async fn list_api_requests(responses: &dyn CliResponseSink, cx: &mut AsyncApp) {
    let Some(store) = loaded_api_store(cx).await else {
        return say_and_exit(responses, API_NOT_READY.to_string(), exit_status::FAILED);
    };
    let mut items = store.read_with(cx, |store, _| {
        store
            .requests
            .iter()
            .map(|request| ApiRequestInfo {
                id: request.id.to_string(),
                path: store.path_of(request),
                method: request.method.as_str().to_string(),
                url: request.url.clone(),
            })
            .collect::<Vec<_>>()
    });
    items.sort_by(|left, right| left.path.cmp(&right.path));
    responses.send(CliResponse::ApiRequests { items }).log_err();
    responses.send(CliResponse::Exit { status: 0 }).log_err();
}

pub async fn list_api_environments(responses: &dyn CliResponseSink, cx: &mut AsyncApp) {
    let Some(store) = loaded_api_store(cx).await else {
        return say_and_exit(responses, API_NOT_READY.to_string(), exit_status::FAILED);
    };
    let items = store.read_with(cx, |store, _| {
        store
            .environments
            .iter()
            .map(|environment| ApiEnvironmentInfo {
                id: environment.id.to_string(),
                name: environment.name.clone(),
                active: store.active_environment_id == Some(environment.id),
                variables: environment
                    .variables
                    .iter()
                    .map(|variable| variable.key.clone())
                    .collect(),
            })
            .collect::<Vec<_>>()
    });
    responses
        .send(CliResponse::ApiEnvironments { items })
        .log_err();
    responses.send(CliResponse::Exit { status: 0 }).log_err();
}

/// The saved request `named` means: its id, its whole path, or a name no other
/// request shares.
pub(crate) fn api_request_named(
    store: &api_client_ui::ApiClientStore,
    named: &str,
) -> Result<api_client::RequestId, String> {
    if let Some(request) = store
        .requests
        .iter()
        .find(|request| request.id.to_string() == named)
    {
        return Ok(request.id);
    }
    let by_path: Vec<_> = store
        .requests
        .iter()
        .filter(|request| store.path_of(request) == named)
        .collect();
    match by_path.as_slice() {
        [only] => return Ok(only.id),
        [] => {}
        several => {
            return Err(format!(
                "{} requests share the path '{named}'; give one of their ids: {}",
                several.len(),
                several
                    .iter()
                    .map(|request| request.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let by_name: Vec<_> = store
        .requests
        .iter()
        .filter(|request| request.name == named)
        .collect();
    match by_name.as_slice() {
        [only] => Ok(only.id),
        [] => Err(format!(
            "No saved request '{named}'. See `zedcli api list`."
        )),
        several => Err(format!(
            "{} requests are named '{named}'; give the whole path: {}",
            several.len(),
            several
                .iter()
                .map(|request| store.path_of(request))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// How long a send waits for the server when the CLI did not say: the CLI's
/// own default wait, so the editor never outlasts the command that asked.
const API_SEND_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ApiSend {
    pub request: String,
    pub environment: Option<String>,
    pub variables: Vec<(String, String)>,
    pub timeout_seconds: u64,
    pub changes: cli::ApiRequestChanges,
}

pub async fn send_api_request(send: ApiSend, responses: &dyn CliResponseSink, cx: &mut AsyncApp) {
    let Some(store) = loaded_api_store(cx).await else {
        return say_and_exit(responses, API_NOT_READY.to_string(), exit_status::FAILED);
    };
    let chosen = store.read_with(cx, |store, _| {
        let request =
            api_request_named(store, &send.request).map_err(super::cli_api::Refusal::not_found)?;
        let environment = super::cli_api::environment_named(store, send.environment.as_deref())?;
        let edited = match send.changes == cli::ApiRequestChanges::default() {
            true => None,
            false => {
                let mut edited = store
                    .requests
                    .iter()
                    .find(|candidate| candidate.id == request)
                    .cloned()
                    .ok_or_else(|| {
                        super::cli_api::Refusal::not_found(
                            "The request no longer exists.".to_string(),
                        )
                    })?;
                super::cli_api::apply_changes(&mut edited, &send.changes)?;
                Some(edited)
            }
        };
        Ok::<_, super::cli_api::Refusal>((request, environment, edited))
    });
    let (request, environment, edited) = match chosen {
        Ok(chosen) => chosen,
        Err(refusal) => return say_and_exit(responses, refusal.message, refusal.status),
    };
    let variables = send.variables;
    let timeout_seconds = send.timeout_seconds;
    let sent = api_client_ui::headless_send::send(
        &store,
        api_client_ui::headless_send::HeadlessSend {
            request,
            environment,
            variables,
            // A little under the CLI's own wait, so the editor's answer, not
            // the CLI's give-up, is what the reader sees.
            timeout: match timeout_seconds {
                0 => API_SEND_TIMEOUT,
                seconds => Duration::from_secs(seconds).mul_f32(0.9),
            },
            edited,
        },
        cx,
    )
    .await;
    match sent {
        Ok(outcome) => {
            responses
                .send(CliResponse::ApiResponse {
                    response: ApiResponseInfo {
                        method: outcome.method,
                        url: outcome.url,
                        environment: outcome.environment_name,
                        status: outcome.response.status,
                        status_text: outcome.response.status_text,
                        headers: outcome.response.headers,
                        body: outcome.response.body,
                        elapsed_ms: outcome.response.elapsed_ms,
                        tests: outcome
                            .tests
                            .into_iter()
                            .map(|test| ApiTestInfo {
                                name: test.name,
                                passed: test.passed,
                                error: test.error,
                            })
                            .collect(),
                    },
                })
                .log_err();
            responses.send(CliResponse::Exit { status: 0 }).log_err();
        }
        Err(error) => say_and_exit(responses, format!("{error:#}"), exit_status::FAILED),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cli::{CliRequest, CliResponse, CliResponseSink, RunAction, RunState, WindowSelector};
    use futures::channel::mpsc;
    use gpui::{AppContext as _, TestAppContext, UpdateGlobal as _};
    use serde_json::json;
    use terminal::Terminal;
    use util::path;
    use workspace::{AppState, Workspace};

    use super::*;
    use crate::zed::open_listener::handle_cli_connection;
    use crate::zed::tests::init_test;

    struct Collect(std::sync::mpsc::Sender<CliResponse>);

    impl CliResponseSink for Collect {
        fn send(&self, response: CliResponse) -> anyhow::Result<()> {
            self.0
                .send(response)
                .map_err(|error| anyhow::anyhow!("{error}"))
        }
    }

    /// Sends one request through the same handler the socket feeds, and
    /// returns every response up to and including `Exit`.
    fn ask(
        cx: &mut TestAppContext,
        app_state: &Arc<AppState>,
        request: CliRequest,
    ) -> Vec<CliResponse> {
        let request = match request {
            CliRequest::Open { .. } => request,
            request => CliRequest::Authenticated {
                token: token_for_tests(),
                request: Box::new(request),
            },
        };
        ask_as_sent(cx, app_state, request)
    }

    /// The same, with the request sent exactly as given.
    fn ask_as_sent(
        cx: &mut TestAppContext,
        app_state: &Arc<AppState>,
        request: CliRequest,
    ) -> Vec<CliResponse> {
        cx.executor().allow_parking();
        let (request_tx, request_rx) = mpsc::unbounded::<CliRequest>();
        let (response_tx, response_rx) = std::sync::mpsc::channel::<CliResponse>();
        let sink: Box<dyn CliResponseSink> = Box::new(Collect(response_tx));
        let app_state = app_state.clone();
        cx.spawn(|mut cx| async move {
            handle_cli_connection((request_rx, sink), app_state, &mut cx).await;
        })
        .detach();
        request_tx
            .unbounded_send(request)
            .expect("the handler takes the request");
        let mut responses = Vec::new();
        for _ in 0..2_000 {
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(50));
            while let Ok(response) = response_rx.try_recv() {
                let is_exit = matches!(response, CliResponse::Exit { .. });
                responses.push(response);
                if is_exit {
                    return responses;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("no Exit among {responses:?}");
    }

    fn exit_of(responses: &[CliResponse]) -> Option<i32> {
        responses.iter().find_map(|response| match response {
            CliResponse::Exit { status } => Some(*status),
            _ => None,
        })
    }

    fn open(cx: &mut TestAppContext, app_state: &Arc<AppState>, project: &str) {
        let responses = ask(
            cx,
            app_state,
            CliRequest::Open {
                paths: vec![project.to_string()],
                urls: Vec::new(),
                diff_paths: Vec::new(),
                diff_all: false,
                wsl: None,
                wait: false,
                open_behavior: cli::OpenBehavior::AlwaysNew,
                env: None,
                user_data_dir: None,
                dev_container: false,
                cwd: None,
            },
        );
        assert_eq!(
            exit_of(&responses),
            Some(0),
            "{project} opens: {responses:?}"
        );
    }

    const TASKS: &str = r#"[
      { "label": "sleeper", "command": "sleep", "args": ["60"] },
      { "label": "unit tests", "command": "cargo test" },
      { "label": "broken", "command": "echo $ZED_NO_SUCH_VARIABLE" }
    ]"#;

    async fn two_projects(cx: &mut TestAppContext) -> Arc<AppState> {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/alpha"),
                json!({ ".zed": { "tasks.json": TASKS }, "src": { "main.rs": "" } }),
            )
            .await;
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/beta"), json!({ "lib.rs": "" }))
            .await;
        open(cx, &app_state, path!("/alpha"));
        open(cx, &app_state, path!("/beta"));
        app_state
    }

    fn window_with(cx: &mut TestAppContext, project: &str) -> (u64, Entity<Workspace>) {
        cx.update(|cx| {
            for window in editor_windows(cx) {
                for workspace in workspaces_of(&window, cx) {
                    if project_roots(&workspace, cx)
                        .iter()
                        .any(|root| root == Path::new(project))
                    {
                        return (window.window_id().as_u64(), workspace);
                    }
                }
            }
            panic!("no window has {project} open");
        })
    }

    fn selector_for(window: u64) -> WindowSelector {
        WindowSelector {
            window: Some(window),
            ..WindowSelector::default()
        }
    }

    #[gpui::test]
    async fn every_window_is_listed_with_its_project(cx: &mut TestAppContext) {
        let app_state = two_projects(cx).await;
        let responses = ask(cx, &app_state, CliRequest::ListWindows);
        let windows = responses
            .iter()
            .find_map(|response| match response {
                CliResponse::Windows { items } => Some(items.clone()),
                _ => None,
            })
            .expect("the windows are answered");
        let mut projects: Vec<PathBuf> = windows
            .iter()
            .flat_map(|window| window.workspaces.iter().flat_map(|w| w.projects.clone()))
            .collect();
        projects.sort();
        assert_eq!(
            projects,
            vec![
                PathBuf::from(path!("/alpha")),
                PathBuf::from(path!("/beta"))
            ]
        );
        assert_eq!(
            windows.iter().filter(|window| window.focused).count(),
            1,
            "exactly one window has focus: {windows:?}"
        );
        assert_eq!(exit_of(&responses), Some(0));
    }

    fn configurations(responses: &[CliResponse]) -> Vec<ConfigurationInfo> {
        responses
            .iter()
            .find_map(|response| match response {
                CliResponse::Configurations { items } => Some(items.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn configurations_of(
        cx: &mut TestAppContext,
        app_state: &Arc<AppState>,
        selector: WindowSelector,
    ) -> Vec<CliResponse> {
        ask(cx, app_state, CliRequest::ListConfigurations { selector })
    }

    #[gpui::test]
    async fn the_window_is_chosen_by_the_directory_the_command_ran_in(cx: &mut TestAppContext) {
        let app_state = two_projects(cx).await;
        let (alpha, _) = window_with(cx, path!("/alpha"));

        let from_inside_alpha = configurations_of(
            cx,
            &app_state,
            WindowSelector {
                cwd: Some(PathBuf::from(path!("/alpha/src"))),
                ..WindowSelector::default()
            },
        );
        let items = configurations(&from_inside_alpha);
        assert!(
            items.iter().all(|item| item.window == alpha)
                && items.iter().any(|item| item.label == "sleeper"),
            "a directory inside a project picks that project's window: {items:?}"
        );

        let from_beta = configurations_of(
            cx,
            &app_state,
            WindowSelector {
                project: Some(PathBuf::from(path!("/beta"))),
                ..WindowSelector::default()
            },
        );
        assert!(
            configurations(&from_beta).is_empty(),
            "beta keeps no configurations: {from_beta:?}"
        );

        let everywhere = configurations_of(
            cx,
            &app_state,
            WindowSelector {
                all: true,
                ..WindowSelector::default()
            },
        );
        assert_eq!(configurations(&everywhere).len(), 3);
    }

    #[gpui::test]
    async fn a_window_that_is_not_there_is_not_found(cx: &mut TestAppContext) {
        let app_state = two_projects(cx).await;
        let responses = ask(
            cx,
            &app_state,
            CliRequest::ListRuns {
                selector: selector_for(u64::MAX),
            },
        );
        assert_eq!(exit_of(&responses), Some(exit_status::NOT_FOUND));

        let responses = ask(
            cx,
            &app_state,
            CliRequest::ListRuns {
                selector: WindowSelector {
                    project: Some(PathBuf::from(path!("/nowhere"))),
                    ..WindowSelector::default()
                },
            },
        );
        assert_eq!(exit_of(&responses), Some(exit_status::NOT_FOUND));
    }

    #[gpui::test]
    async fn an_unknown_configuration_is_not_found(cx: &mut TestAppContext) {
        let app_state = two_projects(cx).await;
        let (alpha, _) = window_with(cx, path!("/alpha"));
        let responses = ask(
            cx,
            &app_state,
            CliRequest::ControlRun {
                selector: selector_for(alpha),
                configuration: "no such thing".into(),
                action: RunAction::Run,
            },
        );
        assert_eq!(exit_of(&responses), Some(exit_status::NOT_FOUND));
        assert!(
            responses.iter().any(|response| matches!(
                response,
                CliResponse::Stderr { message } if message.contains("no such thing")
            )),
            "{responses:?}"
        );
    }

    /// A real process on a real PTY, started the way a configuration's run is,
    /// in the centre of `workspace`.
    async fn a_run_in(
        cx: &mut TestAppContext,
        workspace: &Entity<Workspace>,
        label: &str,
    ) -> Entity<Terminal> {
        let template = task::TaskTemplate {
            label: label.to_string(),
            command: "sleep".to_string(),
            args: vec!["60".to_string()],
            ..task::TaskTemplate::default()
        };
        let resolved = template
            .resolve_task("run configurations", &task::TaskContext::default())
            .expect("the template resolves");
        let mut spawned = resolved.resolved;
        spawned.cwd = Some(std::env::temp_dir());
        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let terminal = project
            .update(cx, |project, cx| project.create_terminal_task(spawned, cx))
            .await
            .expect("the run starts");
        let window = cx.update(|cx| {
            editor_windows(cx)
                .into_iter()
                .find(|window| {
                    workspaces_of(window, cx)
                        .iter()
                        .any(|candidate| candidate.entity_id() == workspace.entity_id())
                })
                .expect("the workspace has a window")
        });
        window
            .update(cx, |_, window, cx| {
                let view = cx.new(|cx| {
                    terminal_view::TerminalView::new(
                        terminal.clone(),
                        workspace.downgrade(),
                        None,
                        project.downgrade(),
                        window,
                        cx,
                    )
                });
                workspace.update(cx, |workspace, cx| {
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                });
            })
            .expect("the window is open");
        cx.run_until_parked();
        terminal
    }

    fn is_running(terminal: &Entity<Terminal>, cx: &TestAppContext) -> bool {
        terminal.read_with(cx, |terminal, _| {
            terminal
                .task()
                .is_some_and(|task| task.status == TaskStatus::Running)
        })
    }

    #[gpui::test]
    async fn a_run_is_listed_with_its_process_and_stopped_with_everything_it_started(
        cx: &mut TestAppContext,
    ) {
        let app_state = two_projects(cx).await;
        let (alpha, workspace) = window_with(cx, path!("/alpha"));
        let run = a_run_in(cx, &workspace, "sleeper").await;

        let responses = ask(
            cx,
            &app_state,
            CliRequest::ListRuns {
                selector: selector_for(alpha),
            },
        );
        let runs = responses
            .iter()
            .find_map(|response| match response {
                CliResponse::Runs { runs, .. } => Some(runs.clone()),
                _ => None,
            })
            .expect("the runs are answered");
        let sleeper = runs
            .iter()
            .find(|run| run.label == "sleeper")
            .expect("the run is listed");
        assert_eq!(sleeper.state, RunState::Running);
        assert_eq!(sleeper.window, alpha);
        assert!(sleeper.pid.is_some(), "the run's shell is named");
        assert!(
            sleeper
                .processes
                .iter()
                .any(|process| process.name == "sleep"),
            "and the program it runs is in its tree: {:?}",
            sleeper.processes
        );

        let configured = configurations(&configurations_of(cx, &app_state, selector_for(alpha)));
        assert!(
            configured
                .iter()
                .any(|item| item.label == "sleeper" && item.running),
            "the configuration reads as running: {configured:?}"
        );

        let responses = ask(
            cx,
            &app_state,
            CliRequest::ControlRun {
                selector: selector_for(alpha),
                configuration: "sleeper".into(),
                action: RunAction::Stop,
            },
        );
        assert_eq!(exit_of(&responses), Some(0), "{responses:?}");
        for _ in 0..300 {
            if !is_running(&run, cx) {
                break;
            }
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!is_running(&run, cx), "Stop ends the run");
    }

    #[gpui::test]
    async fn control_names_one_window_and_says_when_a_run_did_not_start(cx: &mut TestAppContext) {
        let app_state = two_projects(cx).await;
        let (alpha, _) = window_with(cx, path!("/alpha"));
        let responses = ask(
            cx,
            &app_state,
            CliRequest::ControlRun {
                selector: WindowSelector {
                    all: true,
                    ..WindowSelector::default()
                },
                configuration: "sleeper".into(),
                action: RunAction::Stop,
            },
        );
        assert_eq!(
            exit_of(&responses),
            Some(exit_status::BAD_ARGUMENTS),
            "stopping in every window at once is refused: {responses:?}"
        );

        let responses = ask(
            cx,
            &app_state,
            CliRequest::ControlRun {
                selector: selector_for(alpha),
                configuration: "broken".into(),
                action: RunAction::Run,
            },
        );
        assert_eq!(
            exit_of(&responses),
            Some(exit_status::FAILED),
            "a run that could not be resolved is not reported as started: {responses:?}"
        );
    }

    /// Two configurations of one name: starting one of them would be a guess,
    /// so it is refused; stopping them stops both.
    #[gpui::test]
    async fn a_name_two_configurations_share_is_not_guessed_between(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/delta"),
                json!({ ".zed": { "tasks.json": r#"[
                    { "label": "twin", "command": "sleep", "args": ["60"] },
                    { "label": "twin", "command": "sleep", "args": ["61"] }
                ]"# } }),
            )
            .await;
        open(cx, &app_state, path!("/delta"));
        let (delta, _) = window_with(cx, path!("/delta"));
        let responses = ask(
            cx,
            &app_state,
            CliRequest::ControlRun {
                selector: selector_for(delta),
                configuration: "twin".into(),
                action: RunAction::Run,
            },
        );
        assert_eq!(
            exit_of(&responses),
            Some(exit_status::NOT_FOUND),
            "{responses:?}"
        );
        assert!(
            responses.iter().any(|response| matches!(
                response,
                CliResponse::Stderr { message } if message.contains("2 run configurations are named 'twin'")
            )),
            "{responses:?}"
        );
        let responses = ask(
            cx,
            &app_state,
            CliRequest::ControlRun {
                selector: selector_for(delta),
                configuration: "twin".into(),
                action: RunAction::Stop,
            },
        );
        assert_eq!(exit_of(&responses), Some(0), "{responses:?}");
    }

    #[gpui::test]
    async fn a_debug_configuration_is_only_started_from_here(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/gamma"),
                json!({ ".zed": { "debug.json": r#"[
                    { "label": "api (Delve)", "adapter": "Delve", "request": "launch", "program": "." }
                ]"# } }),
            )
            .await;
        open(cx, &app_state, path!("/gamma"));
        let (gamma, _) = window_with(cx, path!("/gamma"));
        let responses = ask(
            cx,
            &app_state,
            CliRequest::ControlRun {
                selector: selector_for(gamma),
                configuration: "api (Delve)".into(),
                action: RunAction::Stop,
            },
        );
        assert_eq!(
            exit_of(&responses),
            Some(exit_status::BAD_ARGUMENTS),
            "{responses:?}"
        );
    }

    #[gpui::test]
    async fn sql_without_an_open_window_says_the_explorer_is_not_ready(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let responses = ask(cx, &app_state, CliRequest::ListConnections);
        assert_eq!(exit_of(&responses), Some(exit_status::FAILED));
    }

    /// A server that answers one request with `201 Created` and hands back the
    /// request line it was sent.
    fn a_server_answering_once() -> (u16, std::thread::JoinHandle<String>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = listener.local_addr().expect("an address").port();
        let served = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return String::new();
            };
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let body = r#"{"id":42}"#;
            write!(
                stream,
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .ok();
            request.lines().next().unwrap_or_default().to_string()
        });
        (port, served)
    }

    /// An API client store holding one collection with two requests, the one
    /// the tests send pointed at `port`.
    fn an_api_store(cx: &mut TestAppContext, port: u16) -> Entity<api_client_ui::ApiClientStore> {
        let store = cx.new(api_client_ui::ApiClientStore::new);
        cx.update(|cx| cx.set_global(api_client_ui::GlobalApiClientStore(store.clone())));
        // Outside its own crate the store reads the (empty) config directory
        // first; what the test adds must come after, or that read replaces it.
        cx.run_until_parked();
        assert!(store.read_with(cx, |store, _| store.is_loaded()));
        store.update(cx, |store, cx| {
            let collection = store.create_collection("Shop".into(), cx);
            let folder = store.create_folder(collection, "Orders".into(), None, cx);
            let create = store.create_request(collection, "Create order".into(), folder, cx);
            store.create_request(collection, "List".into(), folder, cx);
            store.create_request(collection, "List".into(), None, cx);
            store.create_request(collection, "Health".into(), None, cx);
            store.create_request(collection, "Health".into(), None, cx);
            let delete = store.create_request(collection, "Delete order".into(), folder, cx);
            if let Some(request) = store
                .requests
                .iter_mut()
                .find(|request| request.id == delete)
            {
                request.method = api_client::HttpMethod::Delete;
                request.url = format!("http://127.0.0.1:{port}/orders/1");
            }
            if let Some(request) = store
                .requests
                .iter_mut()
                .find(|request| request.id == create)
            {
                request.url = format!("http://127.0.0.1:{port}/orders/{{{{id}}}}");
            }
            store.create_environment("staging".into(), cx);
        });
        store
    }

    #[gpui::test]
    async fn a_saved_request_is_sent_with_one_off_variables_and_kept_in_history(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let (port, served) = a_server_answering_once();
        let store = an_api_store(cx, port);

        let listed = ask(cx, &app_state, CliRequest::ListApiRequests);
        let paths: Vec<String> = listed
            .iter()
            .find_map(|response| match response {
                CliResponse::ApiRequests { items } => {
                    Some(items.iter().map(|item| item.path.clone()).collect())
                }
                _ => None,
            })
            .expect("the requests are listed");
        assert!(
            paths.contains(&"Shop/Orders/Create order".to_string()),
            "a request is named by where it sits: {paths:?}"
        );

        let responses = ask(
            cx,
            &app_state,
            CliRequest::SendApiRequest {
                request: "Create order".into(),
                environment: Some("staging".into()),
                variables: vec![("id".into(), "42".into())],
                timeout_seconds: 30,
                changes: Default::default(),
            },
        );
        assert_eq!(exit_of(&responses), Some(0), "{responses:?}");
        let response = responses
            .iter()
            .find_map(|response| match response {
                CliResponse::ApiResponse { response } => Some(response.clone()),
                _ => None,
            })
            .expect("the response is answered");
        assert_eq!(response.status, 201);
        assert_eq!(response.body, br#"{"id":42}"#.to_vec());
        assert_eq!(response.environment.as_deref(), Some("staging"));
        let request_line = served.join().expect("the server does not panic");
        assert!(
            request_line.starts_with("GET /orders/42 "),
            "the one-off value went into the URL: {request_line}"
        );
        assert_eq!(
            store.read_with(cx, |store, _| store.history.len()),
            1,
            "the send is in the history, as the Send button's is"
        );
    }

    #[gpui::test]
    async fn a_request_that_is_not_there_or_not_unique_is_not_found(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        an_api_store(cx, 9);
        let responses = ask(
            cx,
            &app_state,
            CliRequest::SendApiRequest {
                request: "Nowhere".into(),
                environment: None,
                variables: Vec::new(),
                timeout_seconds: 30,
                changes: Default::default(),
            },
        );
        assert_eq!(exit_of(&responses), Some(exit_status::NOT_FOUND));

        let responses = ask(
            cx,
            &app_state,
            CliRequest::SendApiRequest {
                request: "List".into(),
                environment: None,
                variables: Vec::new(),
                timeout_seconds: 30,
                changes: Default::default(),
            },
        );
        assert_eq!(exit_of(&responses), Some(exit_status::NOT_FOUND));
        assert!(
            responses.iter().any(|response| matches!(
                response,
                CliResponse::Stderr { message } if message.contains("Shop/Orders/List")
            )),
            "an ambiguous name lists the paths to choose from: {responses:?}"
        );

        let responses = ask(
            cx,
            &app_state,
            CliRequest::SendApiRequest {
                request: "Shop/Orders/Create order".into(),
                environment: Some("production".into()),
                variables: Vec::new(),
                timeout_seconds: 30,
                changes: Default::default(),
            },
        );
        assert_eq!(exit_of(&responses), Some(exit_status::NOT_FOUND));
    }

    #[gpui::test]
    async fn environments_are_listed_by_name_without_their_values(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let store = an_api_store(cx, 9);
        store.update(cx, |store, _| {
            if let Some(environment) = store.environments.first_mut() {
                environment.variables.push(api_client::Variable {
                    key: "token".into(),
                    initial_value: "s3cret".into(),
                    current_value: "s3cret".into(),
                    secret: true,
                    enabled: true,
                });
            }
        });
        let responses = ask(cx, &app_state, CliRequest::ListApiEnvironments);
        let text = format!("{responses:?}");
        assert!(text.contains("staging") && text.contains("token"), "{text}");
        assert!(
            !text.contains("s3cret"),
            "a value never leaves the editor: {text}"
        );
    }

    #[gpui::test]
    async fn two_requests_at_one_path_are_not_guessed_between(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        an_api_store(cx, 9);
        let responses = ask(
            cx,
            &app_state,
            CliRequest::SendApiRequest {
                request: "Shop/Health".into(),
                environment: None,
                variables: Vec::new(),
                timeout_seconds: 30,
                changes: Default::default(),
            },
        );
        assert_eq!(exit_of(&responses), Some(exit_status::NOT_FOUND));
        assert!(
            responses.iter().any(|response| matches!(
                response,
                CliResponse::Stderr { message } if message.contains("share the path")
            )),
            "{responses:?}"
        );
    }

    /// A server that takes the connection and never answers holds the editor
    /// no longer than the command's own wait.
    #[gpui::test]
    async fn a_server_that_never_answers_is_given_up_on(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = listener.local_addr().expect("an address").port();
        let silent = std::thread::spawn(move || {
            let held = listener.accept();
            std::thread::sleep(Duration::from_secs(5));
            drop(held);
        });
        an_api_store(cx, port);
        let responses = ask(
            cx,
            &app_state,
            CliRequest::SendApiRequest {
                request: "Shop/Orders/Create order".into(),
                environment: None,
                variables: vec![("id".into(), "1".into())],
                timeout_seconds: 1,
                changes: Default::default(),
            },
        );
        assert_eq!(
            exit_of(&responses),
            Some(exit_status::FAILED),
            "{responses:?}"
        );
        assert!(
            responses.iter().any(|response| matches!(
                response,
                CliResponse::Stderr { message } if message.contains("no answer within")
            )),
            "{responses:?}"
        );
        silent.join().ok();
    }

    /// Any method is sent, and changes given for one send go out with it
    /// without being saved into the request.
    #[gpui::test]
    async fn a_delete_is_sent_with_its_one_off_changes_and_nothing_is_saved(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let (port, served) = a_server_answering_once();
        let store = an_api_store(cx, port);
        let responses = ask(
            cx,
            &app_state,
            CliRequest::SendApiRequest {
                request: "Shop/Orders/Delete order".into(),
                environment: None,
                variables: Vec::new(),
                timeout_seconds: 30,
                changes: cli::ApiRequestChanges {
                    set_params: vec![("force".into(), "1".into())],
                    ..Default::default()
                },
            },
        );
        assert_eq!(exit_of(&responses), Some(0), "{responses:?}");
        let request_line = served.join().expect("the server does not panic");
        assert!(
            request_line.starts_with("DELETE /orders/1?force=1 "),
            "the method and the one-off parameter went out: {request_line}"
        );
        let saved_params = store.read_with(cx, |store, _| {
            store
                .requests
                .iter()
                .find(|request| request.name == "Delete order")
                .map(|request| request.params.len())
        });
        assert_eq!(saved_params, Some(0), "the saved request is left as it was");
    }

    fn manage(
        cx: &mut TestAppContext,
        app_state: &Arc<AppState>,
        operation: cli::ApiOperation,
    ) -> (Option<i32>, Option<cli::ApiData>, Vec<CliResponse>) {
        let responses = ask(cx, app_state, CliRequest::ManageApi { operation });
        let data = responses.iter().find_map(|response| match response {
            CliResponse::Api { data } => Some(data.clone()),
            _ => None,
        });
        (exit_of(&responses), data, responses)
    }

    /// Collections, folders and requests are made, read, changed, moved and
    /// deleted from the CLI, and each change lands in the store the panel shows.
    #[gpui::test]
    async fn collections_folders_and_requests_are_managed_from_the_cli(cx: &mut TestAppContext) {
        use cli::{ApiData, ApiOperation, ApiRequestChanges};
        let app_state = init_test(cx);
        let store = an_api_store(cx, 9);

        let (status, data, responses) = manage(
            cx,
            &app_state,
            ApiOperation::CreateCollection {
                name: "Billing".into(),
            },
        );
        assert_eq!(status, Some(0), "{responses:?}");
        assert!(matches!(data, Some(ApiData::Changed(changed)) if changed.path == "Billing"));
        let (status, ..) = manage(
            cx,
            &app_state,
            ApiOperation::CreateCollection {
                name: "Billing".into(),
            },
        );
        assert_eq!(
            status,
            Some(exit_status::FAILED),
            "a name is not taken twice"
        );

        let (status, data, responses) = manage(
            cx,
            &app_state,
            ApiOperation::CreateFolder {
                path: "Billing/Invoices".into(),
            },
        );
        assert_eq!(status, Some(0), "{responses:?}");
        assert!(
            matches!(&data, Some(ApiData::Changed(changed)) if changed.path == "Billing/Invoices"),
            "{data:?}"
        );

        let (status, _, responses) = manage(
            cx,
            &app_state,
            ApiOperation::CreateRequest {
                path: "Billing/Invoices/Create invoice".into(),
                changes: ApiRequestChanges {
                    method: Some("post".into()),
                    url: Some("https://billing.example.com/invoices".into()),
                    set_headers: vec![("X-Trace".into(), "1".into())],
                    body: Some(r#"{"amount": 100}"#.into()),
                    ..Default::default()
                },
            },
        );
        assert_eq!(status, Some(0), "{responses:?}");
        let saved = store.read_with(cx, |store, _| {
            store
                .requests
                .iter()
                .find(|request| store.path_of(request) == "Billing/Invoices/Create invoice")
                .cloned()
        });
        let saved = saved.expect("the request is in the store the panel shows");
        assert_eq!(saved.method, api_client::HttpMethod::Post);
        assert!(matches!(
            &saved.body,
            api_client::RequestBody::Raw { content_type: api_client::RawBodyContentType::Json, text }
                if text == r#"{"amount": 100}"#
        ));

        let (status, data, _) = manage(
            cx,
            &app_state,
            ApiOperation::ShowRequest {
                request: "Create invoice".into(),
            },
        );
        assert_eq!(status, Some(0));
        let Some(ApiData::Request(detail)) = data else {
            panic!("the request is shown: {data:?}");
        };
        assert_eq!(detail.method, "POST");
        assert_eq!(detail.body_kind, "json");
        assert_eq!(detail.headers.len(), 1);

        let (status, _, responses) = manage(
            cx,
            &app_state,
            ApiOperation::UpdateRequest {
                request: "Billing/Invoices/Create invoice".into(),
                rename: Some("New invoice".into()),
                move_to: Some("Billing".into()),
                changes: ApiRequestChanges {
                    remove_headers: vec!["x-trace".into()],
                    ..Default::default()
                },
            },
        );
        assert_eq!(status, Some(0), "{responses:?}");
        let moved = store.read_with(cx, |store, _| {
            store
                .requests
                .iter()
                .find(|request| request.name == "New invoice")
                .map(|request| (store.path_of(request), request.headers.len()))
        });
        assert_eq!(moved, Some(("Billing/New invoice".to_string(), 0)));

        let (status, data, _) = manage(cx, &app_state, ApiOperation::ListFolders);
        assert_eq!(status, Some(0));
        assert!(
            matches!(&data, Some(ApiData::Folders(folders))
                if folders.iter().any(|folder| folder.path == "Billing/Invoices" && folder.requests == 0)),
            "{data:?}"
        );

        let (status, ..) = manage(
            cx,
            &app_state,
            ApiOperation::DeleteCollection {
                collection: "Billing".into(),
                recursive: false,
            },
        );
        assert_eq!(
            status,
            Some(exit_status::FAILED),
            "a collection that holds something is not deleted without --recursive"
        );
        let (status, ..) = manage(
            cx,
            &app_state,
            ApiOperation::DeleteRequest {
                request: "Billing/New invoice".into(),
            },
        );
        assert_eq!(status, Some(0));
        let (status, ..) = manage(
            cx,
            &app_state,
            ApiOperation::DeleteCollection {
                collection: "Billing".into(),
                recursive: true,
            },
        );
        assert_eq!(status, Some(0));
        let (status, data, _) = manage(cx, &app_state, ApiOperation::ListCollections);
        assert_eq!(status, Some(0));
        assert!(
            matches!(&data, Some(ApiData::Collections(collections))
                if collections.iter().map(|collection| collection.name.as_str()).collect::<Vec<_>>() == ["Shop"]),
            "{data:?}"
        );
    }

    #[gpui::test]
    async fn a_bad_change_creates_nothing(cx: &mut TestAppContext) {
        use cli::{ApiOperation, ApiRequestChanges};
        let app_state = init_test(cx);
        let store = an_api_store(cx, 9);
        let before = store.read_with(cx, |store, _| store.requests.len());
        let (status, ..) = manage(
            cx,
            &app_state,
            ApiOperation::CreateRequest {
                path: "Shop/Broken".into(),
                changes: ApiRequestChanges {
                    method: Some("NOT A METHOD".into()),
                    ..Default::default()
                },
            },
        );
        assert_eq!(status, Some(exit_status::BAD_ARGUMENTS));
        assert_eq!(store.read_with(cx, |store, _| store.requests.len()), before);
    }

    /// A literal credential never leaves the editor, in a shown request or in
    /// a snippet; a `{{variable}}` reference is shown as written.
    #[gpui::test]
    async fn secrets_are_masked_in_a_shown_request_and_in_a_snippet(cx: &mut TestAppContext) {
        use cli::{ApiData, ApiOperation};
        let app_state = init_test(cx);
        let store = an_api_store(cx, 9);
        store.update(cx, |store, cx| {
            if let Some(environment) = store.environments.first_mut() {
                environment.variables.push(api_client::Variable {
                    key: "token".into(),
                    initial_value: "s3cret-token".into(),
                    current_value: "s3cret-token".into(),
                    secret: true,
                    enabled: true,
                });
            }
            let id = store
                .requests
                .iter()
                .find(|request| request.name == "Create order")
                .map(|request| request.id);
            if let Some(id) = id {
                store.update_request(id, cx, |request| {
                    request.headers.push(api_client::Header {
                        key: "Authorization".into(),
                        value: "Bearer {{token}}".into(),
                        enabled: true,
                        description: None,
                    });
                    request.headers.push(api_client::Header {
                        key: "X-Api-Key".into(),
                        value: "literal-k3y".into(),
                        enabled: true,
                        description: None,
                    });
                    request.params.push(api_client::QueryParam {
                        key: "api_key".into(),
                        value: "literal-param".into(),
                        enabled: true,
                        description: None,
                    });
                    request.url = request.url.replace("http://", "http://alice:hunter2@");
                    request.auth = api_client::AuthConfig::Basic {
                        username: "alice".into(),
                        password: "literal-basic".into(),
                    };
                });
            }
        });

        let (status, data, _) = manage(
            cx,
            &app_state,
            ApiOperation::ShowRequest {
                request: "Create order".into(),
            },
        );
        assert_eq!(status, Some(0));
        let shown = format!("{data:?}");
        assert!(shown.contains("Bearer {{token}}"), "{shown}");
        for literal in ["literal-k3y", "literal-param", "hunter2", "literal-basic"] {
            assert!(!shown.contains(literal), "{literal} is shown: {shown}");
        }

        let (status, data, responses) = manage(
            cx,
            &app_state,
            ApiOperation::Snippet {
                request: "Create order".into(),
                language: "curl".into(),
                environment: Some("staging".into()),
                variables: vec![("id".into(), "7".into())],
                changes: Default::default(),
            },
        );
        assert_eq!(status, Some(0), "{responses:?}");
        let Some(ApiData::Snippet(snippet)) = data else {
            panic!("a snippet is answered: {data:?}");
        };
        assert_eq!(snippet.label, "cURL");
        assert!(snippet.code.starts_with("curl"), "{}", snippet.code);
        assert!(snippet.code.contains("/orders/7"), "{}", snippet.code);
        assert!(
            !snippet.code.contains("s3cret-token"),
            "a secret variable is masked in code: {}",
            snippet.code
        );
        // "alice:literal-basic" as Basic auth puts it on the wire.
        let basic = "YWxpY2U6bGl0ZXJhbC1iYXNpYw==";
        for literal in [
            "literal-k3y",
            "literal-param",
            "hunter2",
            "literal-basic",
            basic,
        ] {
            assert!(
                !snippet.code.contains(literal),
                "{literal} is in the code: {}",
                snippet.code
            );
        }

        let (status, ..) = manage(
            cx,
            &app_state,
            ApiOperation::Snippet {
                request: "Create order".into(),
                language: "cobol".into(),
                environment: None,
                variables: Vec::new(),
                changes: Default::default(),
            },
        );
        assert_eq!(status, Some(exit_status::BAD_ARGUMENTS));
    }

    /// A request for the editor's state or data gets through only with this
    /// editor's token, and not at all while zedcli is turned off.
    #[gpui::test]
    async fn only_the_holder_of_the_token_is_answered(cx: &mut TestAppContext) {
        let app_state = two_projects(cx).await;
        token_for_tests();

        let bare = ask_as_sent(cx, &app_state, CliRequest::ListWindows);
        assert_eq!(
            exit_of(&bare),
            Some(exit_status::EDITOR_UNREACHABLE),
            "{bare:?}"
        );
        assert!(
            !bare
                .iter()
                .any(|response| matches!(response, CliResponse::Windows { .. })),
            "nothing is told without the token"
        );

        let guessed = ask_as_sent(
            cx,
            &app_state,
            CliRequest::Authenticated {
                token: "a-guess".into(),
                request: Box::new(CliRequest::ListConnections),
            },
        );
        assert_eq!(exit_of(&guessed), Some(exit_status::EDITOR_UNREACHABLE));

        let answered = ask(cx, &app_state, CliRequest::ListWindows);
        assert_eq!(exit_of(&answered), Some(0), "{answered:?}");

        cx.update(|cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.zedcli = Some(settings::ZedcliSettingsContent {
                        enabled: Some(false),
                    });
                });
            });
        });
        let turned_off = ask(cx, &app_state, CliRequest::ListWindows);
        assert_eq!(
            exit_of(&turned_off),
            Some(exit_status::REFUSED),
            "{turned_off:?}"
        );
    }
}
