use anyhow::Result;
use credentials_provider::CredentialsProvider;
use db_client::{
    ConnectionConfig, ConnectionId, DatabaseDriver, FkInfo, Folder, FolderId, MAX_FOLDER_DEPTH,
    RuntimeProvider, SshTunnel,
    clickhouse::ClickHouseProvider,
    mysql::MySqlProvider,
    on_runtime,
    postgres::PostgresProvider,
    provider::DbProvider,
    redis_provider::RedisProvider,
    schema::{
        ColumnInfo, DatabaseInfo, IndexInfo, ProcedureInfo, TableInfo, TriggerInfo, UserInfo,
    },
    sqlite::SqliteProvider,
};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, Task, TaskExt as _};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use util::ResultExt;

const MAX_QUERY_HISTORY: usize = 100;
const CONNECTIONS_FILE: &str = "db_connections.json";
const DDL_CACHE_FILE: &str = "db_ddl_cache.json";

/// Cached DDL for one connection so `Go to DDL` works while disconnected.
/// `tables` is keyed database -> table -> DDL to keep JSON map keys as strings.
#[derive(Default, Clone, Serialize, Deserialize)]
struct DdlCache {
    #[serde(default)]
    databases: HashMap<String, String>,
    #[serde(default)]
    tables: HashMap<String, HashMap<String, String>>,
}

/// On-disk shape of the connection tree: the folder set plus the connections.
/// Older config files held a bare `[ConnectionConfig]` array; `load_tree_from_disk`
/// migrates those into this form.
#[derive(Default, Serialize, Deserialize)]
struct StoredTree {
    #[serde(default)]
    folders: Vec<Folder>,
    #[serde(default)]
    connections: Vec<ConnectionConfig>,
}

/// Parses the connection file in either the current object form or the legacy
/// bare-array form, migrating legacy flat `folder` names into `Folder` records
/// and assigning each connection a stable `folder_id`.
fn parse_stored_tree(bytes: &[u8]) -> Result<StoredTree> {
    if let Ok(tree) = serde_json::from_slice::<StoredTree>(bytes) {
        return Ok(tree);
    }
    let mut connections: Vec<ConnectionConfig> = serde_json::from_slice(bytes)?;
    Ok(migrate_legacy_connections(&mut connections))
}

/// Converts legacy flat `folder` strings into top-level `Folder` records and
/// points each connection at the matching `folder_id`. The legacy field is
/// cleared so it is not written back.
fn migrate_legacy_connections(connections: &mut [ConnectionConfig]) -> StoredTree {
    let mut folders: Vec<Folder> = Vec::new();
    for config in connections.iter_mut() {
        let Some(name) = config.folder.take() else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let id = folders
            .iter()
            .find(|folder| folder.name == name && folder.parent_id.is_none())
            .map(|folder| folder.id)
            .unwrap_or_else(|| {
                let order = folders.len() as i64;
                let folder = Folder::new(name.to_string(), None, order);
                let id = folder.id;
                folders.push(folder);
                id
            });
        config.folder_id = Some(id);
    }
    StoredTree {
        folders,
        connections: connections.to_vec(),
    }
}

/// Keychain key for a connection's password. The connection id keeps the entry
/// stable across label/host edits.
fn connection_credentials_url(id: ConnectionId) -> String {
    format!("db_client://connection/{id}")
}

/// Returns a copy of `config` with the password cleared. The on-disk JSON must
/// never hold the plaintext password; the secret lives in the OS keychain.
fn redact_password(config: &ConnectionConfig) -> ConnectionConfig {
    let mut redacted = config.clone();
    redacted.password = String::new();
    redacted
}

async fn store_connection_password(
    provider: &Arc<dyn CredentialsProvider>,
    config: &ConnectionConfig,
    cx: &AsyncApp,
) -> Result<()> {
    provider
        .write_credentials(
            &connection_credentials_url(config.id),
            &config.username,
            config.password.as_bytes(),
            cx,
        )
        .await
}

async fn read_connection_password(
    provider: &Arc<dyn CredentialsProvider>,
    id: ConnectionId,
    cx: &AsyncApp,
) -> Result<Option<String>> {
    let credentials = provider
        .read_credentials(&connection_credentials_url(id), cx)
        .await?;
    Ok(credentials.map(|(_username, password)| String::from_utf8_lossy(&password).into_owned()))
}

#[derive(Debug, Clone)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

#[derive(Clone)]
pub struct ActiveConnection {
    pub config: ConnectionConfig,
    pub status: ConnectionStatus,
    pub provider: Option<Arc<dyn DbProvider>>,
    pub databases: Option<Vec<DatabaseInfo>>,
    pub expanded_databases: HashMap<String, Vec<TableInfo>>,
    pub db_views: HashMap<String, Vec<String>>,
    pub expanded_tables: HashMap<(String, String), Vec<ColumnInfo>>,
    pub table_indexes: HashMap<(String, String), Vec<IndexInfo>>,
    pub table_fks: HashMap<(String, String), Vec<FkInfo>>,
    pub table_triggers: HashMap<(String, String), Vec<TriggerInfo>>,
    pub expanded_database_set: HashSet<String>,
    pub expanded_table_set: HashSet<(String, String)>,
}

impl ActiveConnection {
    fn new(config: ConnectionConfig) -> Self {
        Self {
            config,
            status: ConnectionStatus::Disconnected,
            provider: None,
            databases: None,
            expanded_databases: HashMap::new(),
            db_views: HashMap::new(),
            expanded_tables: HashMap::new(),
            table_indexes: HashMap::new(),
            table_fks: HashMap::new(),
            table_triggers: HashMap::new(),
            expanded_database_set: HashSet::new(),
            expanded_table_set: HashSet::new(),
        }
    }
}

pub enum DatabaseStoreEvent {
    ConnectionsChanged,
    SchemaChanged,
}

pub struct DatabaseStore {
    pub connections: Vec<ActiveConnection>,
    pub folders: Vec<Folder>,
    pub query_history: Vec<String>,
    pub active_connection_id: Option<ConnectionId>,
    ssh_tunnels: HashMap<ConnectionId, SshTunnel>,
    ddl_cache: HashMap<ConnectionId, DdlCache>,
}

/// App-global handle to the single workspace `DatabaseStore`, set when the
/// Database Explorer panel initializes. Lets non-UI callers (CLI, agent tools)
/// reach the live connections.
pub struct GlobalDatabaseStore(pub Entity<DatabaseStore>);

impl Global for GlobalDatabaseStore {}

/// Plain query result handed to non-UI callers (CLI, agent tools), free of any
/// `db_client` types so the consuming crates need not depend on it.
pub struct CliQueryOutput {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub rows_affected: u64,
    pub execution_time_ms: u64,
}

impl DatabaseStore {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Skip disk load under test: it would read the user's real config dir
        // (non-hermetic) and auto-connect, which drives Tokio work whose
        // cross-thread wakes trip GPUI's deterministic test scheduler.
        if !cfg!(test) {
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async { load_tree_from_disk() })
                    .await;
                if let Ok(StoredTree {
                    folders,
                    connections: mut configs,
                }) = result
                {
                    // Passwords are stored in the OS keychain, not the JSON. Read
                    // each one back before auto-connecting. A config that still
                    // carries a non-empty password is a legacy plaintext entry;
                    // keep it so the next save migrates it into the keychain.
                    let provider = cx.update(|cx| zed_credentials_provider::global(cx));
                    for config in &mut configs {
                        if config.password.is_empty() {
                            if let Some(password) =
                                read_connection_password(&provider, config.id, cx)
                                    .await
                                    .log_err()
                                    .flatten()
                            {
                                config.password = password;
                            }
                        }
                    }
                    let auto_connect_ids: Vec<_> = configs
                        .iter()
                        .filter(|c| c.auto_connect)
                        .map(|c| c.id)
                        .collect();

                    this.update(cx, |store, cx| {
                        store.folders = folders;
                        for config in configs {
                            store.connections.push(ActiveConnection::new(config));
                        }
                        if !store.connections.is_empty() || !store.folders.is_empty() {
                            cx.emit(DatabaseStoreEvent::ConnectionsChanged);
                            cx.notify();
                        }
                        for id in auto_connect_ids {
                            store.connect(id, cx).detach_and_log_err(cx);
                        }
                    })
                    .ok();
                }
            })
            .detach();
        }

        if !cfg!(test) {
            cx.spawn(async move |this, cx| {
                let cache = cx
                    .background_executor()
                    .spawn(async { load_ddl_cache_from_disk() })
                    .await;
                if !cache.is_empty() {
                    this.update(cx, |store, _| store.ddl_cache = cache).ok();
                }
            })
            .detach();
        }

        Self {
            connections: Vec::new(),
            folders: Vec::new(),
            query_history: Vec::new(),
            active_connection_id: None,
            ssh_tunnels: HashMap::new(),
            ddl_cache: HashMap::new(),
        }
    }

    fn persist_ddl_cache(&self, cx: &mut Context<Self>) {
        let cache = self.ddl_cache.clone();
        cx.background_executor()
            .spawn(async move { save_ddl_cache_to_disk(&cache).log_err() })
            .detach();
    }

    pub fn connections(&self) -> &[ActiveConnection] {
        &self.connections
    }

    pub fn active_connection_id(&self) -> Option<ConnectionId> {
        self.active_connection_id
    }

    /// Returns the app-global store handle if the Database Explorer panel has
    /// been initialized. External callers (CLI handler, agent tools) reach the
    /// store through here rather than the workspace-scoped panel.
    pub fn global(cx: &App) -> Option<Entity<DatabaseStore>> {
        cx.try_global::<GlobalDatabaseStore>().map(|g| g.0.clone())
    }

    /// Saved connections as `(id, label, driver)` tuples, without credentials.
    /// Used by the CLI/agent surfaces that must never expose passwords.
    pub fn connection_summaries(&self) -> Vec<(String, String, String)> {
        self.connections
            .iter()
            .map(|c| {
                (
                    c.config.id.to_string(),
                    c.config.label.clone(),
                    c.config.driver.to_string(),
                )
            })
            .collect()
    }

    /// Resolves `connection` (a connection id or its label), connects if the
    /// provider is not live yet, then runs `sql`. Used by the CLI handler and
    /// agent tools so they share one execution path with the panel.
    pub fn run_query_for_cli(
        &mut self,
        connection: String,
        database: Option<String>,
        sql: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<CliQueryOutput>> {
        let Some(conn) = self
            .connections
            .iter()
            .find(|c| c.config.id.to_string() == connection || c.config.label == connection)
        else {
            return Task::ready(Err(anyhow::anyhow!(
                "No database connection matching '{connection}'"
            )));
        };
        let id = conn.config.id;
        let already_connected = conn.provider.is_some();
        let database = database
            .filter(|database| !database.is_empty())
            .or_else(|| conn.config.database.clone())
            .unwrap_or_default();

        cx.spawn(async move |this, cx| {
            if !already_connected {
                let connect_task = this.update(cx, |store, cx| store.connect(id, cx))?;
                connect_task.await?;
            }
            let query_task =
                this.update(cx, |store, cx| store.execute_query(id, database, sql, cx))?;
            let result = query_task.await?;
            Ok(CliQueryOutput {
                columns: result.columns,
                rows: result.rows,
                rows_affected: result.rows_affected,
                execution_time_ms: result.execution_time_ms,
            })
        })
    }

    pub fn active_connection(&self) -> Option<&ActiveConnection> {
        let id = self.active_connection_id?;
        self.connections
            .iter()
            .find(|c| c.config.id == id && matches!(c.status, ConnectionStatus::Connected))
    }

    pub fn set_active_connection(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        self.active_connection_id = Some(id);
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
    }

    pub fn query_history(&self) -> &[String] {
        &self.query_history
    }

    #[cfg(test)]
    pub(crate) fn add_connected_for_test(
        &mut self,
        config: ConnectionConfig,
        provider: Arc<dyn DbProvider>,
        cx: &mut Context<Self>,
    ) {
        let id = config.id;
        let mut conn = ActiveConnection::new(config);
        conn.status = ConnectionStatus::Connected;
        conn.provider = Some(provider);
        self.connections.push(conn);
        self.active_connection_id = Some(id);
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
    }

    pub fn add_connection(&mut self, mut config: ConnectionConfig, cx: &mut Context<Self>) {
        config.order = self.next_order_in(config.folder_id);
        self.connections.push(ActiveConnection::new(config));
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        self.persist_connections(cx);
    }

    pub fn update_connection(&mut self, mut config: ConnectionConfig, cx: &mut Context<Self>) {
        let config_id = config.id;
        if let Some(conn) = self
            .connections
            .iter_mut()
            .find(|c| c.config.id == config_id)
        {
            // The edit form does not manage tree placement; keep the existing
            // folder and order so editing a connection never moves it.
            config.folder_id = conn.config.folder_id;
            config.order = conn.config.order;
            *conn = ActiveConnection::new(config);
        } else {
            return;
        }
        self.ssh_tunnels.remove(&config_id);
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        self.persist_connections(cx);
    }

    pub fn duplicate_connection(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        let Some(existing) = self.connections.iter().find(|c| c.config.id == id) else {
            return;
        };
        let mut new_config = existing.config.clone();
        new_config.id = uuid::Uuid::new_v4();
        new_config.label = format!("{} (copy)", new_config.label);
        new_config.auto_connect = false;
        self.add_connection(new_config, cx);
    }

    pub fn remove_connection(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        self.connections.retain(|c| c.config.id != id);
        self.ssh_tunnels.remove(&id);
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        cx.spawn(async move |_this, cx| {
            let provider = cx.update(|cx| zed_credentials_provider::global(cx));
            provider
                .delete_credentials(&connection_credentials_url(id), cx)
                .await
                .log_err();
        })
        .detach();
        self.persist_connections(cx);
    }

    fn persist_connections(&self, cx: &mut Context<Self>) {
        // Tests never touch the real config dir or OS keychain global.
        if cfg!(test) {
            return;
        }
        let configs: Vec<ConnectionConfig> =
            self.connections.iter().map(|c| c.config.clone()).collect();
        let folders = self.folders.clone();
        // Credentials I/O runs on the foreground (the keychain provider returns
        // non-Send futures); the JSON write is moved to a background thread.
        cx.spawn(async move |_this, cx| {
            let provider = cx.update(|cx| zed_credentials_provider::global(cx));
            for config in &configs {
                if !config.password.is_empty() {
                    store_connection_password(&provider, config, cx)
                        .await
                        .log_err();
                }
            }
            let redacted: Vec<ConnectionConfig> = configs.iter().map(redact_password).collect();
            cx.background_executor()
                .spawn(async move {
                    save_tree_to_disk(StoredTree {
                        folders,
                        connections: redacted,
                    })
                    .log_err();
                })
                .await;
        })
        .detach();
    }

    /// Depth of `folder_id` in the tree, where a top-level folder is depth 1.
    /// Walks parents with a visited guard so a cycle in stored data cannot loop
    /// forever. Returns 0 for an unknown id.
    pub fn folder_depth(&self, folder_id: FolderId) -> usize {
        let mut depth = 0;
        let mut current = Some(folder_id);
        let mut visited = HashSet::new();
        while let Some(id) = current {
            if !visited.insert(id) {
                break;
            }
            let Some(folder) = self.folders.iter().find(|f| f.id == id) else {
                break;
            };
            depth += 1;
            current = folder.parent_id;
        }
        depth
    }

    /// Greatest depth contained within `folder_id`, counting the folder itself
    /// as 1. Used to keep a moved subtree within `MAX_FOLDER_DEPTH`.
    fn subtree_height(&self, folder_id: FolderId) -> usize {
        let children: Vec<FolderId> = self
            .folders
            .iter()
            .filter(|f| f.parent_id == Some(folder_id))
            .map(|f| f.id)
            .collect();
        1 + children
            .into_iter()
            .map(|child| self.subtree_height(child))
            .max()
            .unwrap_or(0)
    }

    /// True when `folder_id` is `ancestor` or sits below it. Prevents moving a
    /// folder into its own subtree (which would orphan the cycle).
    fn is_descendant_of(&self, folder_id: FolderId, ancestor: FolderId) -> bool {
        let mut current = Some(folder_id);
        let mut visited = HashSet::new();
        while let Some(id) = current {
            if id == ancestor {
                return true;
            }
            if !visited.insert(id) {
                break;
            }
            current = self
                .folders
                .iter()
                .find(|f| f.id == id)
                .and_then(|f| f.parent_id);
        }
        false
    }

    fn next_order_in(&self, parent_id: Option<FolderId>) -> i64 {
        let max_folder = self
            .folders
            .iter()
            .filter(|f| f.parent_id == parent_id)
            .map(|f| f.order)
            .max();
        let max_connection = self
            .connections
            .iter()
            .filter(|c| c.config.folder_id == parent_id)
            .map(|c| c.config.order)
            .max();
        max_folder.max(max_connection).map_or(0, |order| order + 1)
    }

    pub fn folders(&self) -> &[Folder] {
        &self.folders
    }

    /// Creates a folder under `parent_id` and returns its id, or `None` when the
    /// new folder would exceed `MAX_FOLDER_DEPTH`.
    pub fn add_folder(
        &mut self,
        name: String,
        parent_id: Option<FolderId>,
        cx: &mut Context<Self>,
    ) -> Option<FolderId> {
        if let Some(parent) = parent_id {
            if self.folder_depth(parent) >= MAX_FOLDER_DEPTH {
                return None;
            }
        }
        let order = self.next_order_in(parent_id);
        let folder = Folder::new(name, parent_id, order);
        let id = folder.id;
        self.folders.push(folder);
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        self.persist_connections(cx);
        Some(id)
    }

    pub fn rename_folder(&mut self, id: FolderId, name: String, cx: &mut Context<Self>) {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        let Some(folder) = self.folders.iter_mut().find(|f| f.id == id) else {
            return;
        };
        folder.name = trimmed.to_string();
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        self.persist_connections(cx);
    }

    /// Moves `id` under `new_parent`, rejecting the move when it would create a
    /// cycle or push the subtree past `MAX_FOLDER_DEPTH`. Returns whether it ran.
    pub fn move_folder(
        &mut self,
        id: FolderId,
        new_parent: Option<FolderId>,
        cx: &mut Context<Self>,
    ) -> bool {
        if Some(id) == new_parent {
            return false;
        }
        if let Some(parent) = new_parent {
            if self.is_descendant_of(parent, id) {
                return false;
            }
            if self.folder_depth(parent) + self.subtree_height(id) > MAX_FOLDER_DEPTH {
                return false;
            }
        } else if self.subtree_height(id) > MAX_FOLDER_DEPTH {
            return false;
        }
        let order = self.next_order_in(new_parent);
        let Some(folder) = self.folders.iter_mut().find(|f| f.id == id) else {
            return false;
        };
        folder.parent_id = new_parent;
        folder.order = order;
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        self.persist_connections(cx);
        true
    }

    /// A folder is empty when it holds no child folders and no connections.
    pub fn folder_is_empty(&self, id: FolderId) -> bool {
        !self.folders.iter().any(|f| f.parent_id == Some(id))
            && !self
                .connections
                .iter()
                .any(|c| c.config.folder_id == Some(id))
    }

    /// Removes a folder only when it is empty. Returns false and changes nothing
    /// for a missing or non-empty folder, so a delete never destroys connections.
    pub fn remove_folder(&mut self, id: FolderId, cx: &mut Context<Self>) -> bool {
        if !self.folders.iter().any(|f| f.id == id) || !self.folder_is_empty(id) {
            return false;
        }
        self.folders.retain(|f| f.id != id);
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        self.persist_connections(cx);
        true
    }

    pub fn move_connection_to_folder(
        &mut self,
        connection_id: ConnectionId,
        folder_id: Option<FolderId>,
        cx: &mut Context<Self>,
    ) {
        let order = self.next_order_in(folder_id);
        let Some(conn) = self
            .connections
            .iter_mut()
            .find(|c| c.config.id == connection_id)
        else {
            return;
        };
        conn.config.folder_id = folder_id;
        conn.config.order = order;
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        self.persist_connections(cx);
    }

    /// Reorders `connection_id` within its current parent by swapping order with
    /// the neighbor in `direction` (-1 up, +1 down). No-op at the boundary.
    pub fn reorder_connection(
        &mut self,
        connection_id: ConnectionId,
        direction: i64,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self
            .connections
            .iter()
            .find(|c| c.config.id == connection_id)
            .map(|c| (c.config.folder_id, c.config.order))
        else {
            return;
        };
        let (parent, order) = current;
        let mut siblings: Vec<(ConnectionId, i64)> = self
            .connections
            .iter()
            .filter(|c| c.config.folder_id == parent)
            .map(|c| (c.config.id, c.config.order))
            .collect();
        siblings.sort_by_key(|(_, order)| *order);
        let Some(position) = siblings.iter().position(|(id, _)| *id == connection_id) else {
            return;
        };
        let target = position as i64 + direction;
        if target < 0 || target as usize >= siblings.len() {
            return;
        }
        let (neighbor_id, neighbor_order) = siblings[target as usize];
        for conn in self.connections.iter_mut() {
            if conn.config.id == connection_id {
                conn.config.order = neighbor_order;
            } else if conn.config.id == neighbor_id {
                conn.config.order = order;
            }
        }
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        self.persist_connections(cx);
    }

    pub fn connect(&mut self, id: ConnectionId, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(conn) = self.connections.iter_mut().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        conn.status = ConnectionStatus::Connecting;
        let config = conn.config.clone();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = build_provider(&config).await;
            this.update(cx, |this, cx| {
                let Some(conn) = this.connections.iter_mut().find(|c| c.config.id == id) else {
                    return;
                };
                let now_connected = matches!(result, Ok(_));
                match result {
                    Ok((provider, tunnel)) => {
                        conn.provider = Some(provider);
                        conn.status = ConnectionStatus::Connected;
                        if let Some(tunnel) = tunnel {
                            this.ssh_tunnels.insert(id, tunnel);
                        }
                        if this.active_connection_id.is_none() {
                            this.active_connection_id = Some(id);
                        }
                    }
                    Err(err) => {
                        conn.status = ConnectionStatus::Error(err.to_string());
                    }
                }
                cx.emit(DatabaseStoreEvent::ConnectionsChanged);
                cx.notify();
                if now_connected {
                    this.refresh_databases(id, cx).detach_and_log_err(cx);
                }
            })
            .ok();
            Ok(())
        })
    }

    /// Returns the connection's live provider, establishing the connection first
    /// if it is not connected yet. This is how every provider-backed operation
    /// gets its provider, so a missing connection never blocks an action — it is
    /// opened on demand.
    pub fn ensure_connected(
        &mut self,
        id: ConnectionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<dyn DbProvider>>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        if let Some(provider) = conn.provider.clone() {
            return Task::ready(Ok(provider));
        }
        cx.spawn(async move |this, cx| {
            let connect_task = this.update(cx, |store, cx| store.connect(id, cx))?;
            connect_task.await?;
            this.read_with(cx, |store, _| {
                store
                    .connections
                    .iter()
                    .find(|c| c.config.id == id)
                    .and_then(|c| c.provider.clone())
            })?
            .ok_or_else(|| anyhow::anyhow!("Failed to connect"))
        })
    }

    pub fn disconnect(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        let Some(conn) = self.connections.iter_mut().find(|c| c.config.id == id) else {
            return;
        };
        *conn = ActiveConnection {
            config: conn.config.clone(),
            ..ActiveConnection::new(conn.config.clone())
        };
        self.ssh_tunnels.remove(&id);
        if self.active_connection_id == Some(id) {
            self.active_connection_id = None;
        }
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
    }

    pub fn toggle_database_expanded(
        &mut self,
        id: ConnectionId,
        database: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(conn) = self.connections.iter_mut().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        if conn.expanded_database_set.contains(&database) {
            conn.expanded_database_set.remove(&database);
            cx.notify();
            return Task::ready(Ok(()));
        }
        if conn.expanded_databases.contains_key(&database) {
            conn.expanded_database_set.insert(database);
            cx.notify();
            return Task::ready(Ok(()));
        }
        conn.expanded_database_set.insert(database.clone());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            let tables = provider.list_tables(&database).await?;
            let views = provider.list_views(&database).await.unwrap_or_default();
            this.update(cx, |this, cx| {
                let Some(conn) = this.connections.iter_mut().find(|c| c.config.id == id) else {
                    return;
                };
                conn.expanded_databases.insert(database.clone(), tables);
                conn.db_views.insert(database, views);
                cx.emit(DatabaseStoreEvent::SchemaChanged);
                cx.notify();
            })
            .ok();
            Ok(())
        })
    }

    pub fn toggle_table_expanded(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let key = (database.clone(), table.clone());
        let Some(conn) = self.connections.iter_mut().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        if conn.expanded_table_set.contains(&key) {
            conn.expanded_table_set.remove(&key);
            cx.notify();
            return Task::ready(Ok(()));
        }
        if conn.expanded_tables.contains_key(&key) {
            conn.expanded_table_set.insert(key);
            cx.notify();
            return Task::ready(Ok(()));
        }
        conn.expanded_table_set.insert(key.clone());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            let columns = provider.describe_table(&database, &table).await?;
            let indexes = provider
                .list_indexes(&database, &table)
                .await
                .unwrap_or_default();
            let fks = provider
                .list_foreign_keys(&database, &table)
                .await
                .unwrap_or_default();
            let triggers = provider
                .list_triggers(&database, &table)
                .await
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                let Some(conn) = this.connections.iter_mut().find(|c| c.config.id == id) else {
                    return;
                };
                let key = (database, table);
                conn.expanded_tables.insert(key.clone(), columns);
                conn.table_indexes.insert(key.clone(), indexes);
                conn.table_fks.insert(key.clone(), fks);
                conn.table_triggers.insert(key, triggers);
                cx.emit(DatabaseStoreEvent::SchemaChanged);
                cx.notify();
            })
            .ok();
            Ok(())
        })
    }

    pub fn refresh_databases(
        &mut self,
        id: ConnectionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if !self.connections.iter().any(|c| c.config.id == id) {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        }

        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            let databases = provider.list_databases().await?;
            this.update(cx, |this, cx| {
                let Some(conn) = this.connections.iter_mut().find(|c| c.config.id == id) else {
                    return;
                };
                conn.databases = Some(databases);
                cx.emit(DatabaseStoreEvent::SchemaChanged);
                cx.notify();
            })
            .ok();
            Ok(())
        })
    }

    /// Loads the database list and the given database's tables/views into the
    /// schema cache for autocomplete, without marking any tree node expanded.
    /// No-op when the data is already cached.
    pub fn ensure_schema_for_completion(
        &mut self,
        id: ConnectionId,
        database: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Ok(()));
        };
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Ok(()));
        };
        let need_databases = conn.databases.is_none();
        let need_tables = !database.is_empty() && !conn.expanded_databases.contains_key(&database);
        if !need_databases && !need_tables {
            return Task::ready(Ok(()));
        }

        cx.spawn(async move |this, cx| {
            let databases = if need_databases {
                provider.list_databases().await.log_err()
            } else {
                None
            };
            let tables = if need_tables {
                provider.list_tables(&database).await.log_err()
            } else {
                None
            };
            let views = if need_tables {
                Some(provider.list_views(&database).await.unwrap_or_default())
            } else {
                None
            };
            this.update(cx, |this, cx| {
                let Some(conn) = this.connections.iter_mut().find(|c| c.config.id == id) else {
                    return;
                };
                if let Some(databases) = databases {
                    conn.databases = Some(databases);
                }
                if let Some(tables) = tables {
                    conn.expanded_databases.insert(database.clone(), tables);
                }
                if let Some(views) = views {
                    conn.db_views.insert(database, views);
                }
                cx.emit(DatabaseStoreEvent::SchemaChanged);
                cx.notify();
            })
            .ok();
            Ok(())
        })
    }

    /// Loads a table's columns into the schema cache for autocomplete, without
    /// marking the tree node expanded. No-op when already cached.
    pub fn ensure_columns_for_completion(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let key = (database.clone(), table.clone());
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Ok(()));
        };
        if conn.expanded_tables.contains_key(&key) {
            return Task::ready(Ok(()));
        }
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Ok(()));
        };

        cx.spawn(async move |this, cx| {
            let columns = provider.describe_table(&database, &table).await.log_err();
            this.update(cx, |this, cx| {
                let Some(conn) = this.connections.iter_mut().find(|c| c.config.id == id) else {
                    return;
                };
                if let Some(columns) = columns {
                    conn.expanded_tables.insert((database, table), columns);
                    cx.emit(DatabaseStoreEvent::SchemaChanged);
                    cx.notify();
                }
            })
            .ok();
            Ok(())
        })
    }

    /// Drops a connection's cached schema (databases, tables, views, columns,
    /// indexes, foreign keys, triggers) and collapses its tree, then reloads the
    /// database list so the next expand or completion sees the live schema. The
    /// connection itself stays open. This is the user-facing "Refresh".
    pub fn refresh_schema_cache(
        &mut self,
        id: ConnectionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if let Some(conn) = self.connections.iter_mut().find(|c| c.config.id == id) {
            conn.databases = None;
            conn.expanded_databases.clear();
            conn.db_views.clear();
            conn.expanded_tables.clear();
            conn.table_indexes.clear();
            conn.table_fks.clear();
            conn.table_triggers.clear();
            conn.expanded_database_set.clear();
            conn.expanded_table_set.clear();
            cx.emit(DatabaseStoreEvent::SchemaChanged);
            cx.notify();
        }
        if self.ddl_cache.remove(&id).is_some() {
            self.persist_ddl_cache(cx);
        }
        self.refresh_databases(id, cx)
    }

    /// Folds every connection's schema by clearing the expanded database/table
    /// sets. The cached metadata is kept so re-expanding is instant.
    pub fn collapse_all_schema(&mut self, cx: &mut Context<Self>) {
        for conn in &mut self.connections {
            conn.expanded_database_set.clear();
            conn.expanded_table_set.clear();
        }
        cx.notify();
    }

    /// Marks every known database of every connected connection as expanded,
    /// fetching the table/view lists that are not cached yet. Tables themselves
    /// are not expanded, so this stays bounded by the number of databases.
    pub fn expand_all_databases(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let mut to_fetch: Vec<(ConnectionId, String, Arc<dyn DbProvider>)> = Vec::new();
        for conn in &mut self.connections {
            let Some(databases) = conn.databases.clone() else {
                continue;
            };
            let provider = conn.provider.clone();
            for db in databases {
                conn.expanded_database_set.insert(db.name.clone());
                if !conn.expanded_databases.contains_key(&db.name)
                    && let Some(provider) = provider.clone()
                {
                    to_fetch.push((conn.config.id, db.name.clone(), provider));
                }
            }
        }
        cx.notify();
        if to_fetch.is_empty() {
            return Task::ready(Ok(()));
        }
        cx.spawn(async move |this, cx| {
            for (id, database, provider) in to_fetch {
                let tables = provider.list_tables(&database).await.unwrap_or_default();
                let views = provider.list_views(&database).await.unwrap_or_default();
                this.update(cx, |this, cx| {
                    if let Some(conn) = this.connections.iter_mut().find(|c| c.config.id == id) {
                        conn.expanded_databases.insert(database.clone(), tables);
                        conn.db_views.insert(database, views);
                        cx.emit(DatabaseStoreEvent::SchemaChanged);
                        cx.notify();
                    }
                })
                .ok();
            }
            Ok(())
        })
    }

    pub fn execute_query(
        &mut self,
        id: ConnectionId,
        database: String,
        sql: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<db_client::schema::QueryResult>> {
        if !self.connections.iter().any(|c| c.config.id == id) {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        }

        self.record_query_history(sql.clone(), cx);

        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.execute_query(&database, &sql).await
        })
    }

    pub fn describe_table(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<ColumnInfo>>> {
        if !self.connections.iter().any(|c| c.config.id == id) {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.describe_table(&database, &table).await
        })
    }

    pub fn get_table_ddl(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<String>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        if conn.provider.is_none()
            && let Some(ddl) = self.cached_table_ddl(id, &database, &table)
        {
            return Task::ready(Ok(ddl));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            let ddl = provider.get_table_ddl(&database, &table).await?;
            this.update(cx, |store, cx| {
                store
                    .ddl_cache
                    .entry(id)
                    .or_default()
                    .tables
                    .entry(database)
                    .or_default()
                    .insert(table, ddl.clone());
                store.persist_ddl_cache(cx);
            })
            .ok();
            Ok(ddl)
        })
    }

    fn cached_table_ddl(&self, id: ConnectionId, database: &str, table: &str) -> Option<String> {
        self.ddl_cache
            .get(&id)?
            .tables
            .get(database)?
            .get(table)
            .cloned()
    }

    pub fn get_database_ddl(
        &mut self,
        id: ConnectionId,
        database: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<String>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        if conn.provider.is_none()
            && let Some(ddl) = self
                .ddl_cache
                .get(&id)
                .and_then(|c| c.databases.get(&database).cloned())
        {
            return Task::ready(Ok(ddl));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            let ddl = provider.get_database_ddl(&database).await?;
            this.update(cx, |store, cx| {
                store
                    .ddl_cache
                    .entry(id)
                    .or_default()
                    .databases
                    .insert(database, ddl.clone());
                store.persist_ddl_cache(cx);
            })
            .ok();
            Ok(ddl)
        })
    }

    pub fn list_indexes(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<IndexInfo>>> {
        if !self.connections.iter().any(|c| c.config.id == id) {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.list_indexes(&database, &table).await
        })
    }

    pub fn list_foreign_keys(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<FkInfo>>> {
        if !self.connections.iter().any(|c| c.config.id == id) {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.list_foreign_keys(&database, &table).await
        })
    }

    pub fn list_procedures(
        &mut self,
        id: ConnectionId,
        database: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<ProcedureInfo>>> {
        if !self.connections.iter().any(|c| c.config.id == id) {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.list_procedures(&database).await
        })
    }

    pub fn list_triggers(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<TriggerInfo>>> {
        if !self.connections.iter().any(|c| c.config.id == id) {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.list_triggers(&database, &table).await
        })
    }

    pub fn list_users(
        &mut self,
        id: ConnectionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<UserInfo>>> {
        if !self.connections.iter().any(|c| c.config.id == id) {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.list_users().await
        })
    }

    pub fn truncate_table(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if !self.connections.iter().any(|c| c.config.id == id) {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.truncate_table(&database, &table).await
        })
    }

    pub fn drop_table(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if !self.connections.iter().any(|c| c.config.id == id) {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.drop_table(&database, &table).await
        })
    }

    fn record_query_history(&mut self, sql: String, cx: &mut Context<Self>) {
        let trimmed = sql.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        self.query_history.retain(|q| q != &trimmed);
        self.query_history.insert(0, trimmed);
        self.query_history.truncate(MAX_QUERY_HISTORY);
        cx.notify();
    }
}

impl EventEmitter<DatabaseStoreEvent> for DatabaseStore {}

/// Connects with `config` and pings the server, without storing the connection.
/// Used by the connection form's "Test Connection" button.
pub fn test_connection(config: ConnectionConfig, cx: &App) -> Task<Result<()>> {
    cx.background_spawn(async move {
        let (provider, _tunnel) = build_provider(&config).await?;
        provider.ping().await
    })
}

async fn build_provider(
    config: &ConnectionConfig,
) -> Result<(Arc<dyn DbProvider>, Option<SshTunnel>)> {
    let (effective_config, tunnel) = if config.uses_ssh() {
        let ssh_host = config.ssh_host.as_deref().unwrap_or_default();
        let tunnel = SshTunnel::establish(
            ssh_host,
            config.ssh_port,
            config.ssh_username.as_deref(),
            config.ssh_private_key_path.as_deref(),
            &config.host,
            config.port,
        )
        .await?;
        let local_port = tunnel.local_port();
        let mut modified = config.clone();
        modified.host = "127.0.0.1".to_string();
        modified.port = local_port;
        (modified, Some(tunnel))
    } else {
        (config.clone(), None)
    };

    let raw: Arc<dyn DbProvider> = match effective_config.driver {
        DatabaseDriver::MySQL => {
            let config = effective_config.clone();
            Arc::new(on_runtime(async move { MySqlProvider::connect(&config).await }).await?)
        }
        DatabaseDriver::PostgreSQL => {
            let config = effective_config.clone();
            Arc::new(on_runtime(async move { PostgresProvider::connect(&config).await }).await?)
        }
        DatabaseDriver::SQLite => {
            let config = effective_config.clone();
            Arc::new(on_runtime(async move { SqliteProvider::connect(&config).await }).await?)
        }
        DatabaseDriver::ClickHouse => {
            let config = effective_config.clone();
            Arc::new(on_runtime(async move { ClickHouseProvider::connect(&config).await }).await?)
        }
        DatabaseDriver::Redis => {
            let config = effective_config.clone();
            Arc::new(on_runtime(async move { RedisProvider::connect(&config).await }).await?)
        }
    };
    let provider: Arc<dyn DbProvider> = Arc::new(RuntimeProvider::new(raw));
    Ok((provider, tunnel))
}

fn connections_file_path() -> std::path::PathBuf {
    paths::config_dir().join(CONNECTIONS_FILE)
}

fn load_tree_from_disk() -> Result<StoredTree> {
    let bytes = std::fs::read(connections_file_path())?;
    parse_stored_tree(&bytes)
}

fn save_tree_to_disk(tree: StoredTree) -> Result<()> {
    let json = serde_json::to_vec_pretty(&tree)?;
    std::fs::write(connections_file_path(), json)?;
    Ok(())
}

fn ddl_cache_file_path() -> std::path::PathBuf {
    paths::config_dir().join(DDL_CACHE_FILE)
}

/// Loads the DDL cache, returning an empty map when the file is missing or
/// unreadable so a corrupt cache never blocks startup.
fn load_ddl_cache_from_disk() -> HashMap<ConnectionId, DdlCache> {
    let Ok(bytes) = std::fs::read(ddl_cache_file_path()) else {
        return HashMap::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_ddl_cache_to_disk(cache: &HashMap<ConnectionId, DdlCache>) -> Result<()> {
    let json = serde_json::to_vec_pretty(cache)?;
    std::fs::write(ddl_cache_file_path(), json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockCredentials {
        store: Mutex<HashMap<String, (String, Vec<u8>)>>,
    }

    impl CredentialsProvider for MockCredentials {
        fn read_credentials<'a>(
            &'a self,
            url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
            Box::pin(async move {
                Ok(self
                    .store
                    .lock()
                    .expect("credentials mock lock")
                    .get(url)
                    .cloned())
            })
        }

        fn write_credentials<'a>(
            &'a self,
            url: &'a str,
            username: &'a str,
            password: &'a [u8],
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            Box::pin(async move {
                self.store
                    .lock()
                    .expect("credentials mock lock")
                    .insert(url.to_string(), (username.to_string(), password.to_vec()));
                Ok(())
            })
        }

        fn delete_credentials<'a>(
            &'a self,
            url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            Box::pin(async move {
                self.store
                    .lock()
                    .expect("credentials mock lock")
                    .remove(url);
                Ok(())
            })
        }
    }

    fn config_in(folder_id: Option<FolderId>, order: i64) -> ConnectionConfig {
        ConnectionConfig {
            folder_id,
            order,
            ..ConnectionConfig::default()
        }
    }

    #[test]
    fn migrates_legacy_flat_folders_into_folder_records() {
        let mut a = ConnectionConfig::default();
        a.folder = Some("Work".to_string());
        let mut b = ConnectionConfig::default();
        b.folder = Some("Work".to_string());
        let mut c = ConnectionConfig::default();
        c.folder = Some("  ".to_string());
        let mut connections = vec![a, b, c];

        let tree = migrate_legacy_connections(&mut connections);

        // Two connections share one "Work" folder; the blank folder is dropped.
        assert_eq!(tree.folders.len(), 1);
        assert_eq!(tree.folders[0].name, "Work");
        assert!(tree.folders[0].parent_id.is_none());
        let folder_id = tree.folders[0].id;
        assert_eq!(tree.connections[0].folder_id, Some(folder_id));
        assert_eq!(tree.connections[1].folder_id, Some(folder_id));
        assert_eq!(tree.connections[2].folder_id, None);
        // The legacy field is cleared so it is not written back.
        assert!(tree.connections[0].folder.is_none());
    }

    #[test]
    fn parses_both_legacy_array_and_object_forms() {
        let legacy = br#"[{"id":"00000000-0000-0000-0000-000000000001","label":"x","driver":"MySQL","host":"h","port":3306,"username":"u","password":"","folder":"F"}]"#;
        let tree = parse_stored_tree(legacy).expect("legacy parse");
        assert_eq!(tree.connections.len(), 1);
        assert_eq!(tree.folders.len(), 1);

        let object = br#"{"folders":[],"connections":[]}"#;
        let tree = parse_stored_tree(object).expect("object parse");
        assert!(tree.connections.is_empty());
        assert!(tree.folders.is_empty());
    }

    #[gpui::test]
    fn folder_depth_and_guards(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            let root = store.add_folder("root".into(), None, cx).expect("root");
            let level2 = store.add_folder("l2".into(), Some(root), cx).expect("l2");
            let level3 = store.add_folder("l3".into(), Some(level2), cx).expect("l3");
            let level4 = store.add_folder("l4".into(), Some(level3), cx).expect("l4");
            let level5 = store.add_folder("l5".into(), Some(level4), cx).expect("l5");
            assert_eq!(store.folder_depth(level5), 5);
            // A sixth level exceeds MAX_FOLDER_DEPTH and is rejected.
            assert!(store.add_folder("l6".into(), Some(level5), cx).is_none());
            // Moving an ancestor into its own descendant is rejected (cycle).
            assert!(!store.move_folder(root, Some(level3), cx));
            // Moving a leaf to the top level is allowed.
            assert!(store.move_folder(level5, None, cx));
            assert_eq!(store.folder_depth(level5), 1);
        });
    }

    #[gpui::test]
    fn move_and_reorder_connections(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            let folder = store.add_folder("f".into(), None, cx).expect("folder");
            let first = config_in(None, 0);
            let second = config_in(None, 1);
            let first_id = first.id;
            let second_id = second.id;
            store.connections.push(ActiveConnection::new(first));
            store.connections.push(ActiveConnection::new(second));

            store.move_connection_to_folder(first_id, Some(folder), cx);
            let moved = store
                .connections
                .iter()
                .find(|c| c.config.id == first_id)
                .expect("conn");
            assert_eq!(moved.config.folder_id, Some(folder));

            // second is now alone at top level; reordering up is a no-op.
            store.reorder_connection(second_id, -1, cx);
            let order = store
                .connections
                .iter()
                .find(|c| c.config.id == second_id)
                .expect("conn")
                .config
                .order;
            assert_eq!(order, 1);
        });
    }

    #[gpui::test]
    fn remove_folder_only_deletes_empty(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            let parent = store.add_folder("parent".into(), None, cx).expect("parent");
            let child = store
                .add_folder("child".into(), Some(parent), cx)
                .expect("child");
            let conn = config_in(Some(child), 0);
            let conn_id = conn.id;
            store.connections.push(ActiveConnection::new(conn));

            // A folder with a connection is not empty and is not removed.
            assert!(!store.folder_is_empty(child));
            assert!(!store.remove_folder(child, cx));
            assert!(store.folders.iter().any(|f| f.id == child));
            assert_eq!(
                store
                    .connections
                    .iter()
                    .find(|c| c.config.id == conn_id)
                    .expect("conn")
                    .config
                    .folder_id,
                Some(child)
            );

            // A folder with a child folder is not empty either.
            assert!(!store.folder_is_empty(parent));
            assert!(!store.remove_folder(parent, cx));
            assert!(store.folders.iter().any(|f| f.id == parent));

            // Emptying the child lets it be deleted; the parent then becomes empty.
            store.move_connection_to_folder(conn_id, Some(parent), cx);
            assert!(store.folder_is_empty(child));
            assert!(store.remove_folder(child, cx));
            assert!(store.folders.iter().all(|f| f.id != child));
        });
    }

    #[test]
    fn redacts_password_but_keeps_other_fields() {
        let config = ConnectionConfig {
            password: "secret".to_string(),
            ..ConnectionConfig::default()
        };
        let redacted = redact_password(&config);
        assert!(redacted.password.is_empty());
        assert_eq!(redacted.username, config.username);
        assert_eq!(redacted.id, config.id);
    }

    #[test]
    fn credentials_url_is_stable_per_id() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            connection_credentials_url(id),
            format!("db_client://connection/{id}")
        );
    }

    #[gpui::test]
    async fn password_roundtrips_through_provider(cx: &mut gpui::TestAppContext) {
        let provider: Arc<dyn CredentialsProvider> = Arc::new(MockCredentials::default());
        let config = ConnectionConfig {
            password: "p@ss:w/rd ".to_string(),
            ..ConnectionConfig::default()
        };
        let async_cx = cx.to_async();

        store_connection_password(&provider, &config, &async_cx)
            .await
            .expect("write password");
        let loaded = read_connection_password(&provider, config.id, &async_cx)
            .await
            .expect("read password");
        assert_eq!(loaded.as_deref(), Some("p@ss:w/rd "));

        provider
            .delete_credentials(&connection_credentials_url(config.id), &async_cx)
            .await
            .expect("delete password");
        let after = read_connection_password(&provider, config.id, &async_cx)
            .await
            .expect("read password");
        assert_eq!(after, None);
    }

    struct SchemaMockProvider;

    #[async_trait::async_trait]
    impl DbProvider for SchemaMockProvider {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
            Ok(vec![DatabaseInfo {
                name: "shop".into(),
            }])
        }
        async fn list_tables(&self, _database: &str) -> Result<Vec<TableInfo>> {
            Ok(vec![TableInfo {
                name: "users".into(),
                kind: db_client::schema::TableKind::Table,
            }])
        }
        async fn describe_table(&self, _database: &str, _table: &str) -> Result<Vec<ColumnInfo>> {
            Ok(vec![ColumnInfo {
                name: "id".into(),
                data_type: "int".into(),
                is_nullable: false,
                column_key: Some("PRI".into()),
                default_value: None,
                extra: String::new(),
            }])
        }
        async fn execute_query(
            &self,
            _database: &str,
            _sql: &str,
        ) -> Result<db_client::schema::QueryResult> {
            Ok(db_client::schema::QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected: 0,
                execution_time_ms: 0,
            })
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> Result<String> {
            Ok(String::new())
        }
    }

    struct DdlMockProvider;

    #[async_trait::async_trait]
    impl DbProvider for DdlMockProvider {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
            Ok(Vec::new())
        }
        async fn list_tables(&self, _database: &str) -> Result<Vec<TableInfo>> {
            Ok(Vec::new())
        }
        async fn describe_table(&self, _database: &str, _table: &str) -> Result<Vec<ColumnInfo>> {
            Ok(Vec::new())
        }
        async fn execute_query(
            &self,
            _database: &str,
            _sql: &str,
        ) -> Result<db_client::schema::QueryResult> {
            Ok(db_client::schema::QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected: 0,
                execution_time_ms: 0,
            })
        }
        async fn get_table_ddl(&self, _database: &str, table: &str) -> Result<String> {
            Ok(format!("CREATE TABLE `{table}` (id INT)"))
        }
        async fn get_database_ddl(&self, database: &str) -> Result<String> {
            Ok(format!("CREATE DATABASE `{database}`"))
        }
    }

    #[gpui::test]
    async fn ddl_cache_serves_offline_after_fetch(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        let config = ConnectionConfig::default();
        let id = config.id;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(DdlMockProvider), cx);
        });

        let table_fetch = store.update(cx, |store, cx| {
            store.get_table_ddl(id, "shop".into(), "users".into(), cx)
        });
        assert_eq!(
            table_fetch.await.expect("connected fetch"),
            "CREATE TABLE `users` (id INT)"
        );
        let db_fetch = store.update(cx, |store, cx| store.get_database_ddl(id, "shop".into(), cx));
        assert_eq!(
            db_fetch.await.expect("connected fetch"),
            "CREATE DATABASE `shop`"
        );

        store.update(cx, |store, cx| store.disconnect(id, cx));

        let table_cached = store.update(cx, |store, cx| {
            store.get_table_ddl(id, "shop".into(), "users".into(), cx)
        });
        assert_eq!(
            table_cached.await.expect("cached while offline"),
            "CREATE TABLE `users` (id INT)"
        );
        let db_cached = store.update(cx, |store, cx| store.get_database_ddl(id, "shop".into(), cx));
        assert_eq!(
            db_cached.await.expect("cached while offline"),
            "CREATE DATABASE `shop`"
        );
    }

    #[gpui::test]
    async fn ensure_connected_returns_existing_provider(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        let config = ConnectionConfig::default();
        let id = config.id;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(DdlMockProvider), cx);
        });

        // An already-connected connection resolves to its live provider without
        // attempting a fresh connect (the None -> connect path hits a real
        // network and is covered by integration/manual testing instead).
        let provider = store.update(cx, |store, cx| store.ensure_connected(id, cx));
        assert!(provider.await.is_ok());
    }

    #[gpui::test]
    async fn refresh_clears_ddl_cache(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        let config = ConnectionConfig::default();
        let id = config.id;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(DdlMockProvider), cx);
        });
        let fetch = store.update(cx, |store, cx| {
            store.get_table_ddl(id, "shop".into(), "users".into(), cx)
        });
        fetch.await.expect("connected fetch");
        store.read_with(cx, |store, _| {
            assert!(store.cached_table_ddl(id, "shop", "users").is_some());
        });

        let refresh = store.update(cx, |store, cx| store.refresh_schema_cache(id, cx));
        refresh.await.ok();
        store.read_with(cx, |store, _| {
            assert!(store.cached_table_ddl(id, "shop", "users").is_none());
        });
    }

    #[test]
    fn ddl_cache_round_trips() {
        let id = ConnectionConfig::default().id;
        let mut cache: HashMap<ConnectionId, DdlCache> = HashMap::new();
        let entry = cache.entry(id).or_default();
        entry
            .tables
            .entry("shop".into())
            .or_default()
            .insert("users".into(), "CREATE TABLE users".into());
        entry
            .databases
            .insert("shop".into(), "CREATE DATABASE shop".into());

        let bytes = serde_json::to_vec(&cache).expect("serialize");
        let restored: HashMap<ConnectionId, DdlCache> =
            serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(
            restored
                .get(&id)
                .and_then(|c| c.tables.get("shop"))
                .and_then(|t| t.get("users")),
            Some(&"CREATE TABLE users".to_string())
        );
        assert_eq!(
            restored.get(&id).and_then(|c| c.databases.get("shop")),
            Some(&"CREATE DATABASE shop".to_string())
        );
    }

    #[gpui::test]
    fn completion_schema_cache_loads_and_refreshes(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        let config = ConnectionConfig::default();
        let id = config.id;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(SchemaMockProvider), cx);
            store
                .ensure_schema_for_completion(id, "shop".into(), cx)
                .detach();
        });
        cx.run_until_parked();

        store.update(cx, |store, cx| {
            let conn = store
                .connections()
                .iter()
                .find(|c| c.config.id == id)
                .unwrap();
            assert!(conn.databases.is_some(), "databases cached");
            assert_eq!(
                conn.expanded_databases.get("shop").map(|t| t.len()),
                Some(1)
            );
            // Loading the schema for completion must not expand the tree.
            assert!(conn.expanded_database_set.is_empty());

            store
                .ensure_columns_for_completion(id, "shop".into(), "users".into(), cx)
                .detach();
        });
        cx.run_until_parked();

        store.update(cx, |store, cx| {
            let conn = store
                .connections()
                .iter()
                .find(|c| c.config.id == id)
                .unwrap();
            assert!(
                conn.expanded_tables
                    .contains_key(&("shop".to_string(), "users".to_string())),
                "columns cached"
            );
            assert!(conn.expanded_table_set.is_empty());

            store.refresh_schema_cache(id, cx).detach();
            let conn = store
                .connections()
                .iter()
                .find(|c| c.config.id == id)
                .unwrap();
            assert!(conn.databases.is_none(), "cache cleared on refresh");
            assert!(conn.expanded_databases.is_empty());
            assert!(conn.expanded_tables.is_empty());
        });
    }

    struct CliMockProvider;

    #[async_trait::async_trait]
    impl DbProvider for CliMockProvider {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
            Ok(Vec::new())
        }
        async fn list_tables(&self, _database: &str) -> Result<Vec<TableInfo>> {
            Ok(Vec::new())
        }
        async fn describe_table(&self, _database: &str, _table: &str) -> Result<Vec<ColumnInfo>> {
            Ok(Vec::new())
        }
        async fn execute_query(
            &self,
            _database: &str,
            _sql: &str,
        ) -> Result<db_client::schema::QueryResult> {
            Ok(db_client::schema::QueryResult {
                columns: vec!["n".into()],
                rows: vec![vec![Some("1".into())]],
                rows_affected: 0,
                execution_time_ms: 7,
            })
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> Result<String> {
            Ok(String::new())
        }
    }

    #[gpui::test]
    fn global_store_is_set_and_retrievable(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        cx.update(|cx| assert!(DatabaseStore::global(cx).is_none()));
        cx.update(|cx| cx.set_global(GlobalDatabaseStore(store.clone())));
        cx.update(|cx| {
            assert_eq!(
                DatabaseStore::global(cx).map(|s| s.entity_id()),
                Some(store.entity_id())
            )
        });
    }

    #[gpui::test]
    async fn run_query_for_cli_resolves_by_id_and_label(cx: &mut gpui::TestAppContext) {
        let mut config = ConnectionConfig::default();
        config.label = "primary".into();
        let id = config.id;
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(CliMockProvider), cx);
            let summaries = store.connection_summaries();
            assert_eq!(summaries.len(), 1);
            assert_eq!(summaries[0].0, id.to_string());
            assert_eq!(summaries[0].1, "primary");
        });

        for connection in [id.to_string(), "primary".to_string()] {
            let task = store.update(cx, |store, cx| {
                store.run_query_for_cli(connection, None, "SELECT 1".into(), cx)
            });
            let output = task.await.expect("query runs through the cli path");
            assert_eq!(output.columns, vec!["n".to_string()]);
            assert_eq!(output.rows, vec![vec![Some("1".to_string())]]);
            assert_eq!(output.execution_time_ms, 7);
        }
    }
}
