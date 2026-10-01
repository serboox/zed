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
    /// With `at`, `sql` is a script and only the statement at that position
    /// runs, the one a console would run with its cursor there.
    ExecuteQuery {
        connection: String,
        database: Option<String>,
        sql: String,
        #[serde(default)]
        at: Option<StatementPosition>,
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
    /// One of the requests below, carrying the token the editor wrote to its
    /// data directory. Every request that reads the editor's state or data
    /// must come this way; only the one who can read that file can ask.
    Authenticated {
        token: String,
        request: Box<CliRequest>,
    },
    /// Sends a saved request. `request` is its id or its `Collection/.../Name`
    /// path; `environment` an environment's id or name.
    SendApiRequest {
        request: String,
        environment: Option<String>,
        variables: Vec<(String, String)>,
        /// How long the server has to answer; the editor gives up after it.
        #[serde(default)]
        timeout_seconds: u64,
        /// Changes to the saved request for this one send; nothing is saved.
        #[serde(default)]
        changes: ApiRequestChanges,
    },
    /// Reads or changes the API client's saved collections, folders and
    /// requests, or writes a request out as code.
    ManageApi {
        operation: ApiOperation,
    },
}

/// Changes to a saved request given on the command line: saved with it by
/// `ApiOperation::CreateRequest` and `UpdateRequest`, applied to one send or
/// snippet only by the others.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiRequestChanges {
    pub method: Option<String>,
    pub url: Option<String>,
    /// A header of the same name, in any case, is replaced; otherwise added.
    pub set_headers: Vec<(String, String)>,
    pub remove_headers: Vec<String>,
    /// A query parameter of the same name is replaced; otherwise added.
    pub set_params: Vec<(String, String)>,
    pub remove_params: Vec<String>,
    /// A raw body; an empty one removes the body.
    pub body: Option<String>,
    /// `text`, `json`, `xml`, `html` or `javascript`: the raw body's type.
    pub content_type: Option<String>,
    pub description: Option<String>,
}

/// A collection is named by its id or its name, a folder by its id or its
/// `Collection/Folder/...` path, and a request by its id, its path or a name
/// no other request shares.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ApiOperation {
    ListCollections,
    ListFolders,
    ShowRequest {
        request: String,
    },
    CreateCollection {
        name: String,
    },
    RenameCollection {
        collection: String,
        name: String,
    },
    /// Refuses a collection that holds anything unless `recursive`.
    DeleteCollection {
        collection: String,
        recursive: bool,
    },
    /// `Collection/Parent/.../Name`: every folder before the last must exist.
    CreateFolder {
        path: String,
    },
    RenameFolder {
        folder: String,
        name: String,
    },
    /// Refuses a folder that holds anything unless `recursive`.
    DeleteFolder {
        folder: String,
        recursive: bool,
    },
    /// `Collection/Folder/.../Name`; the request goes into the collection
    /// itself when no folder is named.
    CreateRequest {
        path: String,
        changes: ApiRequestChanges,
    },
    UpdateRequest {
        request: String,
        rename: Option<String>,
        /// A collection's name or a folder's path to move the request into.
        move_to: Option<String>,
        changes: ApiRequestChanges,
    },
    DeleteRequest {
        request: String,
    },
    /// The request as code in `language`, resolved the way a send would be.
    Snippet {
        request: String,
        language: String,
        environment: Option<String>,
        variables: Vec<(String, String)>,
        changes: ApiRequestChanges,
    },
}

/// What a [`CliRequest::ManageApi`] answers with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ApiData {
    Collections(Vec<ApiCollectionInfo>),
    Folders(Vec<ApiFolderInfo>),
    Request(ApiRequestDetail),
    Snippet(ApiSnippetInfo),
    Changed(ApiChangeInfo),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiCollectionInfo {
    pub id: String,
    pub name: String,
    pub folders: u64,
    pub requests: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiFolderInfo {
    pub id: String,
    /// `Collection/Folder/...`.
    pub path: String,
    /// Everything under it, however deep.
    pub folders: u64,
    pub requests: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiPair {
    pub key: String,
    pub value: String,
    pub enabled: bool,
}

/// A saved request with every literal secret masked; `{{variable}}`
/// references are shown as written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiRequestDetail {
    pub id: String,
    pub path: String,
    pub method: String,
    pub url: String,
    pub description: Option<String>,
    pub params: Vec<ApiPair>,
    pub headers: Vec<ApiPair>,
    /// `none`, `json`, `text`, `form-data`, `graphql` and the like.
    pub body_kind: String,
    pub body: Option<String>,
    /// The auth scheme and whatever of it is not secret.
    pub auth: String,
    pub pre_request_script: bool,
    pub test_script: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiSnippetInfo {
    /// The language as the editor names it, e.g. `cURL` or `Python - requests`.
    pub label: String,
    pub code: String,
    /// Byte ranges of `code` in the editor's syntax colours; empty when the
    /// editor has no grammar for the language.
    pub highlights: Vec<ApiHighlight>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiHighlight {
    pub start: u64,
    pub end: u64,
    /// `0xRRGGBB`.
    pub color: u32,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiChangeInfo {
    /// `created`, `renamed`, `updated`, `moved` or `deleted`.
    pub action: String,
    /// `collection`, `folder` or `request`.
    pub kind: String,
    pub id: String,
    pub path: String,
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

/// A place in a SQL script that names the statement to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementPosition {
    /// Counted from 1.
    pub line: u32,
    /// Counted from 1; the start of the line's text when absent.
    pub column: Option<u32>,
    /// The innermost query at the position instead of the whole statement.
    pub innermost: bool,
}

/// Which windows a request is about. With nothing set, the window whose project
/// holds `cwd` is chosen, and the focused window when none does. When several
/// windows hold the path, the focused one is chosen if it is among them;
/// otherwise the request is refused, saying which windows they are.
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
    /// What the run is on the far machine, when it was sent over ssh and that
    /// machine answered: `processes` are then those of the local ssh client.
    #[serde(default)]
    pub remote: Option<RemoteRunInfo>,
}

/// The processes of a run on the machine it was sent to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteRunInfo {
    pub machine: String,
    /// The run's root there first, and every parent before its children.
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
    /// The editor's number for the session, which tells two sessions of one
    /// configuration apart.
    #[serde(default)]
    pub id: u32,
    pub label: String,
    pub adapter: String,
    pub state: String,
    /// The program being debugged, once the adapter has said which process it is.
    #[serde(default)]
    pub pid: Option<u32>,
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

impl CliRequest {
    /// Whether this request reaches the editor's state or data, and so must
    /// come wrapped in [`CliRequest::Authenticated`].
    pub fn needs_token(&self) -> bool {
        match self {
            CliRequest::Open { .. } | CliRequest::SetOpenBehavior { .. } => false,
            CliRequest::Authenticated { .. }
            | CliRequest::ExecuteQuery { .. }
            | CliRequest::ListConnections
            | CliRequest::ListWindows
            | CliRequest::ListRuns { .. }
            | CliRequest::ListConfigurations { .. }
            | CliRequest::ControlRun { .. }
            | CliRequest::ListApiRequests
            | CliRequest::ListApiEnvironments
            | CliRequest::SendApiRequest { .. }
            | CliRequest::ManageApi { .. } => true,
        }
    }
}

/// Where a running editor keeps the token its CLI requests must carry: next to
/// its socket, in its data directory, readable by its own user only.
pub fn token_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join(format!(
        "zedcli-{}.token",
        *release_channel::RELEASE_CHANNEL_NAME
    ))
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
    Api {
        data: ApiData,
    },
}

/// Exit statuses the editor answers with, beyond 0 for success and 1 for a
/// request that failed on its own terms.
pub mod exit_status {
    pub const FAILED: i32 = 1;
    pub const BAD_ARGUMENTS: i32 = 2;
    pub const EDITOR_UNREACHABLE: i32 = 3;
    pub const NOT_FOUND: i32 = 4;
    /// The request would change data, and only reads are let through.
    pub const REFUSED: i32 = 5;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A client from before a query could name a position asks without one, and
    /// the editor still reads the question.
    #[test]
    fn a_query_from_an_older_client_has_no_position() {
        let older = r#"{"ExecuteQuery":{"connection":"local","database":null,"sql":"SELECT 1"}}"#;
        let request: CliRequest = serde_json::from_str(older).expect("it reads");
        assert_eq!(
            request,
            CliRequest::ExecuteQuery {
                connection: "local".into(),
                database: None,
                sql: "SELECT 1".into(),
                at: None,
            }
        );
    }

    /// An editor from before sessions carried a number and a process says neither
    /// in its answer, and the answer still reads.
    #[test]
    fn a_debug_session_from_an_older_editor_still_reads() {
        let older = r#"{"window":7,"label":"Debug API","adapter":"Delve","state":"running"}"#;
        let session: DebugSessionInfo = serde_json::from_str(older).expect("it reads");
        assert_eq!(session.id, 0);
        assert_eq!(session.pid, None);
        assert_eq!(session.label, "Debug API");
    }
}
