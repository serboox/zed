use std::path::PathBuf;

use anyhow::Result;
use collections::HashMap;
pub use ipc_channel::ipc;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct IpcHandshake {
    pub requests: ipc::IpcSender<CliRequest>,
    pub responses: ipc::IpcReceiver<CliResponse>,
}

/// Controls how CLI paths are opened — whether to reuse existing windows,
/// create new ones, or add to the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpenBehavior {
    /// Consult the user's `cli_default_open_behavior` setting.
    #[default]
    Default,
    /// Always create a new window. No matching against existing worktrees.
    /// Corresponds to `zed -n`.
    AlwaysNew,
    /// Create a new window unless opening a subpath of an existing project.
    PreferNewWindow,
    /// Match broadly including subdirectories, and fall back to any existing
    /// window if no worktree matched. Corresponds to `zed -a`.
    Add,
    /// Open directories as a new workspace in the current Zed window's sidebar.
    /// Reuse existing windows for files in open worktrees.
    /// Corresponds to `zed -e`.
    ExistingWindow,
    /// New window for directories, reuse existing window for files in open
    /// worktrees. The classic pre-sidebar behavior.
    /// Corresponds to `zed --classic`.
    Classic,
    /// Replace the content of an existing window with a new workspace.
    /// Corresponds to `zed -r`.
    Reuse,
}

/// The setting-level enum for configuring default behavior. This only has
/// two values because the other modes are always explicitly requested via
/// CLI flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliBehaviorSetting {
    /// Open directories as a new workspace in the current Zed window's sidebar.
    ExistingWindow,
    /// Open paths in a new window unless they are subpaths of an existing project.
    NewWindow,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum CliRequest {
    Open {
        paths: Vec<String>,
        urls: Vec<String>,
        diff_paths: Vec<[String; 2]>,
        diff_all: bool,
        wsl: Option<String>,
        wait: bool,
        #[serde(default)]
        open_behavior: OpenBehavior,
        env: Option<HashMap<String, String>>,
        user_data_dir: Option<String>,
        dev_container: bool,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    SetOpenBehavior {
        behavior: CliBehaviorSetting,
    },
    /// Runs a SQL query against a saved database connection and returns the
    /// result. `connection` is matched against a connection id or its label.
    ExecuteQuery {
        connection: String,
        database: Option<String>,
        sql: String,
    },
    /// Lists the saved database connections (no credentials).
    ListConnections,
    /// Lists every editor window, the projects open in it and its active file.
    ListWindows,
    /// Lists the task runs and debug sessions of the selected windows.
    ListRuns {
        selector: WindowSelector,
    },
    /// Lists the run configurations the selected windows' projects keep.
    ListConfigurations {
        selector: WindowSelector,
    },
    /// Runs, stops or restarts a run configuration of the selected window.
    ControlRun {
        selector: WindowSelector,
        configuration: String,
        action: RunAction,
    },
    /// Lists the API client's saved requests.
    ListApiRequests,
    /// Lists the API client's environments.
    ListApiEnvironments,
    /// Sends a saved request. `request` is its id or its `Collection/.../Name`
    /// path; `environment` an environment's id or name.
    SendApiRequest {
        request: String,
        environment: Option<String>,
        variables: Vec<(String, String)>,
        /// How long the server has to answer; the editor gives up after it.
        #[serde(default)]
        timeout_seconds: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiRequestInfo {
    pub id: String,
    /// `Collection/Folder/.../Request`.
    pub path: String,
    pub method: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiEnvironmentInfo {
    pub id: String,
    pub name: String,
    pub active: bool,
    /// Variable names only; values can be secrets.
    pub variables: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiTestInfo {
    pub name: String,
    pub passed: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiResponseInfo {
    pub method: String,
    pub url: String,
    pub environment: Option<String>,
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub elapsed_ms: u64,
    pub tests: Vec<ApiTestInfo>,
}

/// Which windows a request is about. With nothing set, the window whose project
/// holds `cwd` is chosen, and the focused window when none does.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowSelector {
    pub window: Option<u64>,
    pub project: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub all: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunAction {
    Run,
    Stop,
    Restart,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u64,
    pub focused: bool,
    pub workspaces: Vec<WorkspaceInfo>,
}

/// One project group of a window: a window can hold several in its sidebar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub active: bool,
    pub projects: Vec<PathBuf>,
    pub active_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunInfo {
    pub window: u64,
    pub label: String,
    pub command: String,
    pub state: RunState,
    /// The shell the run was started in; its tree is in `processes`.
    pub pid: Option<u32>,
    /// The run's processes, the shell first and every parent before its children.
    pub processes: Vec<ProcessInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
    /// Percentage of one core; `None` when the platform cannot say.
    pub cpu_percent: Option<f32>,
    pub memory_bytes: u64,
    pub threads: u64,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DebugSessionInfo {
    pub window: u64,
    pub label: String,
    pub adapter: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigurationInfo {
    pub window: u64,
    pub label: String,
    /// `task` or `debug`.
    pub kind: String,
    pub command: String,
    pub running: bool,
}

/// A saved database connection, without any credentials.
#[derive(Debug, Serialize, Deserialize)]
pub struct DbConnectionSummary {
    pub id: String,
    pub label: String,
    pub driver: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum CliResponse {
    Ping,
    Stdout {
        message: String,
    },
    Stderr {
        message: String,
    },
    Exit {
        status: i32,
    },
    PromptOpenBehavior,
    QueryResult {
        columns: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
        rows_affected: u64,
        execution_time_ms: u64,
    },
    Connections {
        items: Vec<DbConnectionSummary>,
    },
    Windows {
        items: Vec<WindowInfo>,
    },
    Runs {
        runs: Vec<RunInfo>,
        debug_sessions: Vec<DebugSessionInfo>,
    },
    Configurations {
        items: Vec<ConfigurationInfo>,
    },
    ApiRequests {
        items: Vec<ApiRequestInfo>,
    },
    ApiEnvironments {
        items: Vec<ApiEnvironmentInfo>,
    },
    ApiResponse {
        response: ApiResponseInfo,
    },
}

/// Exit statuses the editor answers with, beyond 0 for success and 1 for a
/// request that failed on its own terms.
pub mod exit_status {
    pub const FAILED: i32 = 1;
    pub const BAD_ARGUMENTS: i32 = 2;
    pub const EDITOR_UNREACHABLE: i32 = 3;
    pub const NOT_FOUND: i32 = 4;
}

/// When Zed started not as an *.app but as a binary (e.g. local development),
/// there's a possibility to tell it to behave "regularly".
///
/// Note that in the main zed binary, this variable is unset after it's read for the first time,
/// therefore it should always be accessed through the `FORCE_CLI_MODE` static.
pub const FORCE_CLI_MODE_ENV_VAR_NAME: &str = "ZED_FORCE_CLI_MODE";

/// Abstracts the transport for sending CLI responses (Zed → CLI).
///
/// Production code uses `IpcSender<CliResponse>`. Tests can provide in-memory
/// implementations to avoid OS-level IPC.
pub trait CliResponseSink: Send + 'static {
    fn send(&self, response: CliResponse) -> Result<()>;
}

impl CliResponseSink for ipc::IpcSender<CliResponse> {
    fn send(&self, response: CliResponse) -> Result<()> {
        ipc::IpcSender::send(self, response).map_err(|error| anyhow::anyhow!("{error}"))
    }
}
