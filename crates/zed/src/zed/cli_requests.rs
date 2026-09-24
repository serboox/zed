use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cli::{
    ApiEnvironmentInfo, ApiRequestInfo, ApiResponseInfo, ApiTestInfo, CliResponse, CliResponseSink,
    ConfigurationInfo, DebugSessionInfo, ProcessInfo, RunInfo, RunState, WindowInfo,
    WindowSelector, WorkspaceInfo, exit_status,
};
use gpui::{App, AsyncApp, Entity, WindowHandle};
use run_configurations::configurations_file::Kind;
use run_configurations::{configurations_store, process_metrics, run_instances};
use terminal::TaskStatus;
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace};

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

fn say_and_exit(responses: &dyn CliResponseSink, message: String, status: i32) {
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

/// The API client's store once its saved collections have been read, counted
/// like [`STORE_LOOKS`] so the wait ends under a test clock too.
async fn loaded_api_store(cx: &mut AsyncApp) -> Option<Entity<api_client_ui::ApiClientStore>> {
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

const API_NOT_READY: &str = "The API client is not ready: no editor window is open, or the \
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
fn api_request_named(
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

/// The HTTP methods zedcli sends: the ones that ask for data.
const READ_METHODS: [api_client::HttpMethod; 3] = [
    api_client::HttpMethod::Get,
    api_client::HttpMethod::Head,
    api_client::HttpMethod::Options,
];

/// How long a send waits for the server when the CLI did not say: the CLI's
/// own default wait, so the editor never outlasts the command that asked.
const API_SEND_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn send_api_request(
    request: String,
    environment: Option<String>,
    variables: Vec<(String, String)>,
    timeout_seconds: u64,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    let Some(store) = loaded_api_store(cx).await else {
        return say_and_exit(responses, API_NOT_READY.to_string(), exit_status::FAILED);
    };
    let chosen = store.read_with(cx, |store, _| {
        let request = api_request_named(store, &request)?;
        let environment = match &environment {
            None => None,
            Some(named) => Some(
                store
                    .environments
                    .iter()
                    .find(|candidate| {
                        candidate.id.to_string() == *named || candidate.name == *named
                    })
                    .map(|candidate| candidate.id)
                    .ok_or_else(|| format!("No environment '{named}'. See `zedcli api envs`."))?,
            ),
        };
        Ok::<_, String>((request, environment))
    });
    let (request, environment) = match chosen {
        Ok(chosen) => chosen,
        Err(message) => return say_and_exit(responses, message, exit_status::NOT_FOUND),
    };
    // Only methods that ask for data rather than send it. Checked before the
    // request's own pre-request script runs, so nothing of a refused request
    // happens at all.
    let method = store.read_with(cx, |store, _| {
        store
            .requests
            .iter()
            .find(|candidate| candidate.id == request)
            .map(|candidate| candidate.method.clone())
    });
    if let Some(method) = method
        && !READ_METHODS.contains(&method)
    {
        return say_and_exit(
            responses,
            format!(
                "{} can change data, and zedcli only sends GET, HEAD and OPTIONS.",
                method.as_str()
            ),
            exit_status::REFUSED,
        );
    }
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
    use gpui::{AppContext as _, TestAppContext};
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

    /// Starting or stopping a run executes whatever the configuration says,
    /// which can change anything, so the CLI socket does neither.
    #[gpui::test]
    async fn run_control_is_refused(cx: &mut TestAppContext) {
        let app_state = two_projects(cx).await;
        let (alpha, workspace) = window_with(cx, path!("/alpha"));
        let run = a_run_in(cx, &workspace, "sleeper").await;
        for action in [RunAction::Run, RunAction::Stop, RunAction::Restart] {
            let responses = ask(
                cx,
                &app_state,
                CliRequest::ControlRun {
                    selector: selector_for(alpha),
                    configuration: "sleeper".into(),
                    action,
                },
            );
            assert_eq!(
                exit_of(&responses),
                Some(exit_status::REFUSED),
                "{action:?}: {responses:?}"
            );
        }
        assert!(is_running(&run, cx), "the run was left alone");
        run.update(cx, |terminal, _| terminal.kill_active_task());
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
    async fn a_run_is_listed_with_its_process(cx: &mut TestAppContext) {
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

        run.update(cx, |terminal, _| terminal.kill_active_task());
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

    /// A method that sends data is refused before anything of the request
    /// happens: the server is never even connected to.
    #[gpui::test]
    async fn a_request_that_could_change_data_is_never_sent(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        listener.set_nonblocking(true).expect("non-blocking");
        let port = listener.local_addr().expect("an address").port();
        let store = an_api_store(cx, port);
        let responses = ask(
            cx,
            &app_state,
            CliRequest::SendApiRequest {
                request: "Shop/Orders/Delete order".into(),
                environment: None,
                variables: Vec::new(),
                timeout_seconds: 30,
            },
        );
        assert_eq!(
            exit_of(&responses),
            Some(exit_status::REFUSED),
            "{responses:?}"
        );
        assert!(
            listener.accept().is_err(),
            "nothing connected to the server"
        );
        assert!(
            store.read_with(cx, |store, _| store.history.is_empty()),
            "and nothing was sent to be kept in the history"
        );
    }
}
