use crate::db_migration::ConnectionSecrets;
use anyhow::Result;
use credentials_provider::CredentialsProvider;
use db_client::{
    ConnectionConfig, ConnectionId, DatabaseDriver, FkInfo, Folder, FolderId,
    KubernetesRelayCommand, KubernetesRelayCommandKind, KubernetesTarget, KubernetesTargetKind,
    KubernetesTunnel, KubernetesTunnelMode, KubernetesTunnelModeKind, MAX_FOLDER_DEPTH,
    RuntimeProvider, SshAuth, SshAuthMethod, SshTunnel,
    aerospike_provider::AerospikeProvider,
    cassandra_provider::CassandraProvider,
    clickhouse::ClickHouseProvider,
    mongo_provider::MongoProvider,
    mysql::MySqlProvider,
    on_runtime,
    postgres::PostgresProvider,
    provider::DbProvider,
    redis_provider::RedisProvider,
    schema::{
        CheckConstraintInfo, ColumnInfo, DatabaseInfo, EventInfo, IndexInfo, ProcedureInfo,
        SequenceInfo, TableInfo, TriggerInfo, UserInfo,
    },
    sqlite::SqliteProvider,
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, Task, TaskExt as _,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use util::ResultExt;

const MAX_QUERY_HISTORY: usize = 100;
const CONNECTIONS_FILE: &str = "db_connections.json";
const RUN_CONFIGS_FILE: &str = "db_run_configs.json";
const DDL_CACHE_FILE: &str = "db_ddl_cache.json";
const SCHEMA_CACHE_FILE: &str = "db_schema_cache.json";

/// Upper bounds for the background full-schema prefetch so a huge server does
/// not stall the worker or balloon memory. Beyond these the rest stays lazy.
const MAX_PREFETCH_DATABASES: usize = 50;
const MAX_PREFETCH_TABLES_PER_DATABASE: usize = 300;
/// Total describe_table calls a single prefetch may spend loading columns across
/// all databases, so a server with many large schemas stays responsive.
const MAX_PREFETCH_COLUMN_TABLES_TOTAL: usize = 1000;

/// Cached DDL for one connection so `Go to DDL` works while disconnected.
/// `tables` is keyed database -> table -> DDL to keep JSON map keys as strings.
#[derive(Default, Clone, Serialize, Deserialize)]
struct DdlCache {
    #[serde(default)]
    databases: HashMap<String, String>,
    #[serde(default)]
    tables: HashMap<String, HashMap<String, String>>,
}

/// Persisted full-schema snapshot for one connection, the source the completion
/// provider reads from so suggestions are instant and survive restarts/offline.
/// Map keys stay strings (database, then table) for plain JSON.
#[derive(Default, Clone, Serialize, Deserialize)]
struct SchemaCache {
    #[serde(default)]
    databases: Vec<DatabaseInfo>,
    #[serde(default)]
    tables: HashMap<String, Vec<TableInfo>>,
    #[serde(default)]
    views: HashMap<String, Vec<String>>,
    #[serde(default)]
    columns: HashMap<String, HashMap<String, Vec<ColumnInfo>>>,
}

/// The kind of a flattened schema entry, as surfaced to the go-to-object palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaObjectKind {
    Database,
    Table,
    View,
    Column,
}

/// One database/table/view/column entry from the prefetched schema cache,
/// flattened for fuzzy search. See `DatabaseStore::schema_objects`.
#[derive(Debug, Clone)]
pub struct SchemaObjectRef {
    pub kind: SchemaObjectKind,
    pub database: String,
    pub table: Option<String>,
    pub column: Option<String>,
}

impl SchemaObjectRef {
    /// Fully-qualified label used for both fuzzy matching and display, e.g.
    /// "db", "db.table", or "db.table.column".
    pub fn display_label(&self) -> String {
        match (&self.table, &self.column) {
            (Some(table), Some(column)) => format!("{}.{table}.{column}", self.database),
            (Some(table), None) => format!("{}.{table}", self.database),
            (None, _) => self.database.clone(),
        }
    }
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

/// Keychain key for a connection's SSH tunnel password. Separate entry from
/// the DB password so the two secrets don't collide or get confused.
fn ssh_connection_credentials_url(id: ConnectionId) -> String {
    format!("db_client://connection/{id}/ssh")
}

/// Returns a copy of `config` with both the DB password and the SSH tunnel
/// password cleared. The on-disk JSON must never hold either plaintext
/// secret; both live in the OS keychain.
fn redact_password(config: &ConnectionConfig) -> ConnectionConfig {
    let mut redacted = config.clone();
    redacted.password = String::new();
    redacted.ssh_password = String::new();
    redacted
}

fn read_only_error(label: &str) -> anyhow::Error {
    anyhow::anyhow!("Connection '{label}' is read-only — write and DDL statements are blocked.")
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

async fn store_connection_ssh_password(
    provider: &Arc<dyn CredentialsProvider>,
    config: &ConnectionConfig,
    cx: &AsyncApp,
) -> Result<()> {
    provider
        .write_credentials(
            &ssh_connection_credentials_url(config.id),
            config.ssh_username.as_deref().unwrap_or(""),
            config.ssh_password.as_bytes(),
            cx,
        )
        .await
}

async fn read_connection_ssh_password(
    provider: &Arc<dyn CredentialsProvider>,
    id: ConnectionId,
    cx: &AsyncApp,
) -> Result<Option<String>> {
    let credentials = provider
        .read_credentials(&ssh_connection_credentials_url(id), cx)
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
    pub db_procedures: HashMap<String, Vec<ProcedureInfo>>,
    pub db_sequences: HashMap<String, Vec<SequenceInfo>>,
    pub db_events: HashMap<String, Vec<EventInfo>>,
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
            db_procedures: HashMap::new(),
            db_sequences: HashMap::new(),
            db_events: HashMap::new(),
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
    ExecJobsChanged,
}

/// Whichever kind of tunnel keeps a connection's underlying driver reachable
/// on `127.0.0.1`, held alive for as long as the connection is open. Neither
/// variant's payload is read again after construction -- it exists only so
/// `Drop` tears the tunnel process down when the connection closes.
#[allow(dead_code)]
enum ActiveTunnel {
    Ssh(SshTunnel),
    Kubernetes(KubernetesTunnel),
}

pub struct DatabaseStore {
    pub connections: Vec<ActiveConnection>,
    pub folders: Vec<Folder>,
    pub query_history: Vec<String>,
    pub active_connection_id: Option<ConnectionId>,
    tunnels: HashMap<ConnectionId, ActiveTunnel>,
    ddl_cache: HashMap<ConnectionId, DdlCache>,
    schema_cache: HashMap<ConnectionId, SchemaCache>,
    prefetching_schema: HashSet<ConnectionId>,
    run_configurations: Vec<RunConfiguration>,
    pub exec_jobs: Vec<crate::sql_exec::ExecJob>,
    pub(crate) next_exec_job_id: usize,
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

/// One entry of the tree — a folder or a connection — for the drag-and-drop
/// reordering API on `DatabaseStore`. Folders and connections share one
/// `order` space per parent (see `next_order_in`), so a "sibling" list mixes
/// both kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeItemRef {
    Folder(FolderId),
    Connection(ConnectionId),
}

/// Where a dragged item lands relative to the anchor sibling it was dropped
/// next to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativePosition {
    Before,
    After,
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
                        if config.ssh_auth_method == SshAuthMethod::Password
                            && config.ssh_password.is_empty()
                        {
                            if let Some(password) =
                                read_connection_ssh_password(&provider, config.id, cx)
                                    .await
                                    .log_err()
                                    .flatten()
                            {
                                config.ssh_password = password;
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
                        store.hydrate_schema_caches();
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

            cx.spawn(async move |this, cx| {
                let cache = cx
                    .background_executor()
                    .spawn(async { load_schema_cache_from_disk() })
                    .await;
                if !cache.is_empty() {
                    this.update(cx, |store, cx| {
                        store.schema_cache = cache;
                        // Connections may already be loaded; fill their empty maps
                        // so completion is rich before any connect happens.
                        store.hydrate_schema_caches();
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();

            cx.spawn(async move |this, cx| {
                let configs = cx
                    .background_executor()
                    .spawn(async { load_run_configs_from_disk() })
                    .await;
                if !configs.is_empty() {
                    this.update(cx, |store, _| store.run_configurations = configs)
                        .ok();
                }
            })
            .detach();
        }

        Self {
            connections: Vec::new(),
            folders: Vec::new(),
            query_history: Vec::new(),
            active_connection_id: None,
            tunnels: HashMap::new(),
            ddl_cache: HashMap::new(),
            schema_cache: HashMap::new(),
            prefetching_schema: HashSet::new(),
            run_configurations: Vec::new(),
            exec_jobs: Vec::new(),
            next_exec_job_id: 0,
        }
    }

    fn persist_ddl_cache(&self, cx: &mut Context<Self>) {
        let cache = self.ddl_cache.clone();
        cx.background_executor()
            .spawn(async move { save_ddl_cache_to_disk(&cache).log_err() })
            .detach();
    }

    fn persist_schema_cache(&self, cx: &mut Context<Self>) {
        let cache = self.schema_cache.clone();
        cx.background_executor()
            .spawn(async move { save_schema_cache_to_disk(&cache).log_err() })
            .detach();
    }

    fn persist_run_configurations(&self, cx: &mut Context<Self>) {
        if cfg!(test) {
            return;
        }
        let configs = self.run_configurations.clone();
        cx.background_executor()
            .spawn(async move { save_run_configs_to_disk(&configs).log_err() })
            .detach();
    }

    pub fn run_configurations(&self) -> &[RunConfiguration] {
        &self.run_configurations
    }

    /// The saved run configuration for `path`, if any. `path` is compared as
    /// given (callers are expected to pass an absolute, canonicalized path,
    /// matching how it was stored).
    pub fn run_configuration_for_path(&self, path: &std::path::Path) -> Option<&RunConfiguration> {
        self.run_configurations
            .iter()
            .find(|config| config.file_path == path)
    }

    /// Saves `config`, replacing any existing configuration for the same file
    /// path (a file has at most one run configuration at a time).
    pub fn set_run_configuration(&mut self, config: RunConfiguration, cx: &mut Context<Self>) {
        self.run_configurations
            .retain(|existing| existing.file_path != config.file_path);
        self.run_configurations.push(config);
        self.persist_run_configurations(cx);
    }

    pub fn remove_run_configuration(&mut self, id: uuid::Uuid, cx: &mut Context<Self>) {
        self.run_configurations.retain(|config| config.id != id);
        self.persist_run_configurations(cx);
    }

    /// Fills each connection's empty schema maps from the persisted snapshot so
    /// completion has data offline and right after startup. Never overwrites
    /// live data already fetched from the server.
    fn hydrate_schema_caches(&mut self) {
        let cache = std::mem::take(&mut self.schema_cache);
        for conn in &mut self.connections {
            let Some(snapshot) = cache.get(&conn.config.id) else {
                continue;
            };
            if conn.databases.is_none() && !snapshot.databases.is_empty() {
                conn.databases = Some(snapshot.databases.clone());
            }
            for (database, tables) in &snapshot.tables {
                conn.expanded_databases
                    .entry(database.clone())
                    .or_insert_with(|| tables.clone());
            }
            for (database, views) in &snapshot.views {
                conn.db_views
                    .entry(database.clone())
                    .or_insert_with(|| views.clone());
            }
            for (database, tables) in &snapshot.columns {
                for (table, columns) in tables {
                    conn.expanded_tables
                        .entry((database.clone(), table.clone()))
                        .or_insert_with(|| columns.clone());
                }
            }
        }
        self.schema_cache = cache;
    }

    /// Loads the full schema (every database, its tables/views, and the columns
    /// of the primary database) into the in-memory maps in the background, then
    /// persists it, so the completion provider answers instantly and offline.
    /// Bounded by `MAX_PREFETCH_*` and idempotent per connection (a second call
    /// while one is in flight is a no-op). Columns of non-primary databases stay
    /// lazy to keep the round-trip count sane on large servers.
    pub fn prefetch_full_schema(
        &mut self,
        id: ConnectionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Ok(()));
        };
        if !self.prefetching_schema.insert(id) {
            return Task::ready(Ok(()));
        }
        let preferred_database = conn.config.database.clone().filter(|d| !d.is_empty());

        cx.spawn(async move |this, cx| {
            let outcome = async {
                let provider = this
                    .update(cx, |store, cx| store.ensure_connected(id, cx))?
                    .await?;
                let databases = provider.list_databases().await?;
                let primary = preferred_database
                    .clone()
                    .or_else(|| databases.first().map(|d| d.name.clone()));

                let mut cache = SchemaCache {
                    databases: databases.clone(),
                    ..Default::default()
                };
                for info in databases.iter().take(MAX_PREFETCH_DATABASES) {
                    let database = info.name.clone();
                    let tables = provider.list_tables(&database).await.unwrap_or_default();
                    let views = provider.list_views(&database).await.unwrap_or_default();
                    cache.tables.insert(database.clone(), tables);
                    cache.views.insert(database, views);
                }
                if databases.len() > MAX_PREFETCH_DATABASES {
                    log::info!(
                        "db_client: schema prefetch capped at {} of {} databases",
                        MAX_PREFETCH_DATABASES,
                        databases.len()
                    );
                }

                // Load columns primary-database-first, then the rest in listing
                // order, spending a shared budget so many large schemas can't
                // issue thousands of describe_table calls.
                let mut column_order: Vec<String> = Vec::new();
                if let Some(primary_database) = primary
                    .clone()
                    .filter(|database| cache.tables.contains_key(database))
                {
                    column_order.push(primary_database);
                }
                for info in databases.iter().take(MAX_PREFETCH_DATABASES) {
                    if !column_order.contains(&info.name) {
                        column_order.push(info.name.clone());
                    }
                }

                let mut column_budget = MAX_PREFETCH_COLUMN_TABLES_TOTAL;
                let mut budget_skipped: Vec<String> = Vec::new();
                for database in &column_order {
                    let Some(tables) = cache.tables.get(database) else {
                        continue;
                    };
                    if column_budget == 0 {
                        if !tables.is_empty() {
                            budget_skipped.push(database.clone());
                        }
                        continue;
                    }
                    let total = tables.len();
                    let per_database_limit = MAX_PREFETCH_TABLES_PER_DATABASE.min(column_budget);
                    let mut columns = HashMap::new();
                    for table in tables.iter().take(per_database_limit) {
                        if let Some(cols) =
                            provider.describe_table(database, &table.name).await.log_err()
                        {
                            columns.insert(table.name.clone(), cols);
                        }
                    }
                    column_budget = column_budget.saturating_sub(total.min(per_database_limit));
                    if total > per_database_limit {
                        log::info!(
                            "db_client: schema prefetch loaded columns for {} of {} tables in {database}",
                            per_database_limit,
                            total
                        );
                    }
                    if !columns.is_empty() {
                        cache.columns.insert(database.clone(), columns);
                    }
                }
                if !budget_skipped.is_empty() {
                    log::info!(
                        "db_client: schema prefetch column budget of {} exhausted; columns not loaded for {} databases: {}",
                        MAX_PREFETCH_COLUMN_TABLES_TOTAL,
                        budget_skipped.len(),
                        budget_skipped.join(", ")
                    );
                }

                this.update(cx, |store, cx| {
                    if let Some(conn) = store.connections.iter_mut().find(|c| c.config.id == id) {
                        conn.databases = Some(cache.databases.clone());
                        for (database, tables) in &cache.tables {
                            conn.expanded_databases.insert(database.clone(), tables.clone());
                        }
                        for (database, views) in &cache.views {
                            conn.db_views.insert(database.clone(), views.clone());
                        }
                        for (database, columns) in &cache.columns {
                            for (table, cols) in columns {
                                conn.expanded_tables
                                    .insert((database.clone(), table.clone()), cols.clone());
                            }
                        }
                    }
                    store.schema_cache.insert(id, cache);
                    store.persist_schema_cache(cx);
                    cx.emit(DatabaseStoreEvent::SchemaChanged);
                    cx.notify();
                })?;
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |store, _| {
                store.prefetching_schema.remove(&id);
            })
            .ok();
            outcome
        })
    }

    pub fn connections(&self) -> &[ActiveConnection] {
        &self.connections
    }

    /// Flattens the prefetched schema cache of `id` into a single searchable
    /// list of databases/tables/views/columns for the go-to-object palette.
    /// Reads only what `prefetch_full_schema` already loaded -- never issues
    /// a query itself, so an unprefetched connection simply returns nothing.
    pub fn schema_objects(&self, id: ConnectionId) -> Vec<SchemaObjectRef> {
        let Some(cache) = self.schema_cache.get(&id) else {
            return Vec::new();
        };
        let mut objects = Vec::new();
        for db in &cache.databases {
            objects.push(SchemaObjectRef {
                kind: SchemaObjectKind::Database,
                database: db.name.clone(),
                table: None,
                column: None,
            });
            if let Some(tables) = cache.tables.get(&db.name) {
                for table in tables {
                    objects.push(SchemaObjectRef {
                        kind: SchemaObjectKind::Table,
                        database: db.name.clone(),
                        table: Some(table.name.clone()),
                        column: None,
                    });
                }
            }
            if let Some(views) = cache.views.get(&db.name) {
                for view_name in views {
                    objects.push(SchemaObjectRef {
                        kind: SchemaObjectKind::View,
                        database: db.name.clone(),
                        table: Some(view_name.clone()),
                        column: None,
                    });
                }
            }
            if let Some(tables_columns) = cache.columns.get(&db.name) {
                for (table_name, columns) in tables_columns {
                    for column in columns {
                        objects.push(SchemaObjectRef {
                            kind: SchemaObjectKind::Column,
                            database: db.name.clone(),
                            table: Some(table_name.clone()),
                            column: Some(column.name.clone()),
                        });
                    }
                }
            }
        }
        objects
    }

    /// Resolves `connection` (an id or its label) to a `ConnectionId`, the
    /// same lookup `run_query_for_cli` uses so a connection can be named
    /// either way.
    pub fn resolve_connection_id(&self, connection: &str) -> Option<ConnectionId> {
        self.connections
            .iter()
            .find(|c| c.config.id.to_string() == connection || c.config.label == connection)
            .map(|c| c.config.id)
    }

    /// Cached column info for one table, read only from what
    /// `prefetch_full_schema` already loaded -- never issues a query itself.
    /// Returns an empty vec if the table hasn't been cached yet.
    pub fn cached_table_columns(
        &self,
        id: ConnectionId,
        database: &str,
        table: &str,
    ) -> Vec<ColumnInfo> {
        self.schema_cache
            .get(&id)
            .and_then(|cache| cache.columns.get(database))
            .and_then(|tables| tables.get(table))
            .cloned()
            .unwrap_or_default()
    }

    /// Every cached table and view name for one database, read only from
    /// what `prefetch_full_schema` already loaded.
    pub fn cached_table_names(&self, id: ConnectionId, database: &str) -> Vec<String> {
        let Some(cache) = self.schema_cache.get(&id) else {
            return Vec::new();
        };
        let mut names: Vec<String> = cache
            .tables
            .get(database)
            .map(|tables| tables.iter().map(|table| table.name.clone()).collect())
            .unwrap_or_default();
        if let Some(views) = cache.views.get(database) {
            names.extend(views.iter().cloned());
        }
        names
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

    /// Points `id`'s connection at `database` for subsequent queries, without
    /// touching any other connection field or persisting the change — used
    /// when invoking a saved run configuration that targets a specific
    /// database, so the run always lands where the user pinned it regardless
    /// of whichever database happened to be selected last.
    pub fn set_connection_database(
        &mut self,
        id: ConnectionId,
        database: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(connection) = self.connections.iter_mut().find(|c| c.config.id == id) {
            connection.config.database = Some(database);
            cx.notify();
        }
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
        self.tunnels.remove(&config_id);
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
        self.tunnels.remove(&id);
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        cx.spawn(async move |_this, cx| {
            let provider = cx.update(|cx| zed_credentials_provider::global(cx));
            provider
                .delete_credentials(&connection_credentials_url(id), cx)
                .await
                .log_err();
            provider
                .delete_credentials(&ssh_connection_credentials_url(id), cx)
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
                if config.ssh_auth_method == SshAuthMethod::Password
                    && !config.ssh_password.is_empty()
                {
                    store_connection_ssh_password(&provider, config, cx)
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

    /// Reads every connection's DB password and SSH tunnel password from the
    /// keychain, for an export bundle. A connection with neither secret set is
    /// skipped entirely.
    pub fn read_all_secrets(
        &self,
        cx: &mut Context<Self>,
    ) -> Task<Result<BTreeMap<ConnectionId, ConnectionSecrets>>> {
        let ids: Vec<ConnectionId> = self.connections.iter().map(|c| c.config.id).collect();
        cx.spawn(async move |_this, cx| {
            let provider = cx.update(|cx| zed_credentials_provider::global(cx));
            let mut secrets = BTreeMap::new();
            for id in ids {
                let password = read_connection_password(&provider, id, cx)
                    .await?
                    .unwrap_or_default();
                let ssh_password = read_connection_ssh_password(&provider, id, cx)
                    .await?
                    .unwrap_or_default();
                if !password.is_empty() || !ssh_password.is_empty() {
                    secrets.insert(
                        id,
                        ConnectionSecrets {
                            password,
                            ssh_password,
                        },
                    );
                }
            }
            Ok(secrets)
        })
    }

    /// Restores a folder tree and connections from an import bundle. Upserts by
    /// id: an existing folder/connection is updated in place, a new one is added,
    /// and nothing already present is dropped.
    pub fn restore_tree(
        &mut self,
        folders: Vec<Folder>,
        connections: Vec<ConnectionConfig>,
        cx: &mut Context<Self>,
    ) {
        for folder in folders {
            if let Some(existing) = self.folders.iter_mut().find(|f| f.id == folder.id) {
                *existing = folder;
            } else {
                self.folders.push(folder);
            }
        }
        for config in connections {
            if let Some(conn) = self
                .connections
                .iter_mut()
                .find(|c| c.config.id == config.id)
            {
                conn.config = config;
            } else {
                self.connections.push(ActiveConnection::new(config));
            }
        }
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        self.persist_connections(cx);
    }

    /// Writes decrypted DB and SSH tunnel passwords back to the keychain and
    /// into the in-memory configs, so an imported connection can connect (and
    /// tunnel) without a reload. Unknown connection ids are ignored.
    pub fn restore_secrets(
        &mut self,
        secrets: BTreeMap<ConnectionId, ConnectionSecrets>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let mut db_entries = Vec::new();
        let mut ssh_entries = Vec::new();
        for (id, secrets) in secrets {
            if let Some(conn) = self.connections.iter_mut().find(|c| c.config.id == id) {
                if !secrets.password.is_empty() {
                    conn.config.password = secrets.password.clone();
                    db_entries.push((id, conn.config.username.clone(), secrets.password));
                }
                if !secrets.ssh_password.is_empty() {
                    conn.config.ssh_password = secrets.ssh_password.clone();
                    ssh_entries.push((
                        id,
                        conn.config.ssh_username.clone().unwrap_or_default(),
                        secrets.ssh_password,
                    ));
                }
            }
        }
        cx.notify();
        cx.spawn(async move |_this, cx| {
            let provider = cx.update(|cx| zed_credentials_provider::global(cx));
            for (id, username, password) in db_entries {
                provider
                    .write_credentials(
                        &connection_credentials_url(id),
                        &username,
                        password.as_bytes(),
                        cx,
                    )
                    .await
                    .log_err();
            }
            for (id, username, ssh_password) in ssh_entries {
                provider
                    .write_credentials(
                        &ssh_connection_credentials_url(id),
                        &username,
                        ssh_password.as_bytes(),
                        cx,
                    )
                    .await
                    .log_err();
            }
            Ok(())
        })
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

    /// Reorders `folder_id` among its sibling folders (same `parent_id`) by
    /// swapping order with the neighbor in `direction` (-1 up, +1 down).
    /// No-op at the boundary. Mirrors `reorder_connection`.
    pub fn reorder_folder(&mut self, folder_id: FolderId, direction: i64, cx: &mut Context<Self>) {
        let Some(current) = self
            .folders
            .iter()
            .find(|f| f.id == folder_id)
            .map(|f| (f.parent_id, f.order))
        else {
            return;
        };
        let (parent, order) = current;
        let mut siblings: Vec<(FolderId, i64)> = self
            .folders
            .iter()
            .filter(|f| f.parent_id == parent)
            .map(|f| (f.id, f.order))
            .collect();
        siblings.sort_by_key(|(_, order)| *order);
        let Some(position) = siblings.iter().position(|(id, _)| *id == folder_id) else {
            return;
        };
        let target = position as i64 + direction;
        if target < 0 || target as usize >= siblings.len() {
            return;
        }
        let (neighbor_id, neighbor_order) = siblings[target as usize];
        for folder in self.folders.iter_mut() {
            if folder.id == folder_id {
                folder.order = neighbor_order;
            } else if folder.id == neighbor_id {
                folder.order = order;
            }
        }
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        self.persist_connections(cx);
    }

    fn tree_item_parent(&self, item: TreeItemRef) -> Option<Option<FolderId>> {
        match item {
            TreeItemRef::Folder(id) => self
                .folders
                .iter()
                .find(|f| f.id == id)
                .map(|f| f.parent_id),
            TreeItemRef::Connection(id) => self
                .connections
                .iter()
                .find(|c| c.config.id == id)
                .map(|c| c.config.folder_id),
        }
    }

    /// Folders and connections directly under `parent_id`, sorted by `order`.
    fn combined_siblings(&self, parent_id: Option<FolderId>) -> Vec<TreeItemRef> {
        let mut siblings: Vec<(TreeItemRef, i64)> = self
            .folders
            .iter()
            .filter(|f| f.parent_id == parent_id)
            .map(|f| (TreeItemRef::Folder(f.id), f.order))
            .chain(
                self.connections
                    .iter()
                    .filter(|c| c.config.folder_id == parent_id)
                    .map(|c| (TreeItemRef::Connection(c.config.id), c.config.order)),
            )
            .collect();
        siblings.sort_by_key(|(_, order)| *order);
        siblings.into_iter().map(|(item, _)| item).collect()
    }

    fn set_tree_item_order(&mut self, item: TreeItemRef, parent_id: Option<FolderId>, order: i64) {
        match item {
            TreeItemRef::Folder(id) => {
                if let Some(folder) = self.folders.iter_mut().find(|f| f.id == id) {
                    folder.parent_id = parent_id;
                    folder.order = order;
                }
            }
            TreeItemRef::Connection(id) => {
                if let Some(conn) = self.connections.iter_mut().find(|c| c.config.id == id) {
                    conn.config.folder_id = parent_id;
                    conn.config.order = order;
                }
            }
        }
    }

    /// Moves `item` to sit immediately `position` (before/after) `anchor` among
    /// `anchor`'s siblings, reparenting `item` under `anchor`'s parent if it
    /// wasn't already there. Rejects moves that would create a cycle or push a
    /// folder subtree past `MAX_FOLDER_DEPTH`, mirroring `move_folder`'s
    /// guards. Returns whether the move ran.
    pub fn reposition_item(
        &mut self,
        item: TreeItemRef,
        anchor: TreeItemRef,
        position: RelativePosition,
        cx: &mut Context<Self>,
    ) -> bool {
        if item == anchor {
            return false;
        }
        let Some(target_parent) = self.tree_item_parent(anchor) else {
            return false;
        };
        if let TreeItemRef::Folder(item_id) = item {
            if let Some(parent) = target_parent {
                if parent == item_id || self.is_descendant_of(parent, item_id) {
                    return false;
                }
                if self.folder_depth(parent) + self.subtree_height(item_id) > MAX_FOLDER_DEPTH {
                    return false;
                }
            } else if self.subtree_height(item_id) > MAX_FOLDER_DEPTH {
                return false;
            }
        }

        let mut siblings = self.combined_siblings(target_parent);
        siblings.retain(|sibling| *sibling != item);
        let Some(anchor_index) = siblings.iter().position(|sibling| *sibling == anchor) else {
            return false;
        };
        let insert_at = match position {
            RelativePosition::Before => anchor_index,
            RelativePosition::After => anchor_index + 1,
        };
        siblings.insert(insert_at, item);

        for (index, sibling) in siblings.into_iter().enumerate() {
            self.set_tree_item_order(sibling, target_parent, index as i64);
        }

        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        self.persist_connections(cx);
        true
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
            let connect_error = result.as_ref().err().map(|err| err.to_string());
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
                            this.tunnels.insert(id, tunnel);
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
                    this.prefetch_full_schema(id, cx).detach_and_log_err(cx);
                }
            })
            .ok();
            connect_task_result(connect_error)
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
        self.tunnels.remove(&id);
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
            let (tables, views, procedures, sequences, events) = futures::join!(
                provider.list_tables(&database),
                provider.list_views(&database),
                provider.list_procedures(&database),
                provider.list_sequences(&database),
                provider.list_events(&database)
            );
            let tables = tables?;
            let views = views.unwrap_or_default();
            let procedures = procedures.unwrap_or_default();
            let sequences = sequences.unwrap_or_default();
            let events = events.unwrap_or_default();
            this.update(cx, |this, cx| {
                let Some(conn) = this.connections.iter_mut().find(|c| c.config.id == id) else {
                    return;
                };
                conn.expanded_databases.insert(database.clone(), tables);
                conn.db_views.insert(database.clone(), views);
                conn.db_procedures.insert(database.clone(), procedures);
                conn.db_sequences.insert(database.clone(), sequences);
                conn.db_events.insert(database, events);
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
            let (columns, indexes, fks, triggers) = futures::join!(
                provider.describe_table(&database, &table),
                provider.list_indexes(&database, &table),
                provider.list_foreign_keys(&database, &table),
                provider.list_triggers(&database, &table)
            );
            let columns = columns?;
            let indexes = indexes.unwrap_or_default();
            let fks = fks.unwrap_or_default();
            let triggers = triggers.unwrap_or_default();
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
            conn.db_procedures.clear();
            conn.db_sequences.clear();
            conn.db_events.clear();
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
        if self.schema_cache.remove(&id).is_some() {
            self.persist_schema_cache(cx);
        }
        self.prefetch_full_schema(id, cx)
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
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        if conn.config.read_only && crate::db_agent_tools::requires_confirmation(&sql) {
            return Task::ready(Err(read_only_error(&conn.config.label)));
        }

        self.record_query_history(sql.clone(), cx);

        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.execute_query(&database, &sql).await
        })
    }

    /// Fetches a single Aerospike record by key. See
    /// [`db_client::provider::DbProvider::get_record`].
    pub fn get_record(
        &mut self,
        id: ConnectionId,
        namespace: String,
        set: String,
        key: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<Vec<(String, String)>>>> {
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.get_record(&namespace, &set, &key).await
        })
    }

    /// Writes bins to an Aerospike record by key. See
    /// [`db_client::provider::DbProvider::put_record`].
    pub fn put_record(
        &mut self,
        id: ConnectionId,
        namespace: String,
        set: String,
        key: String,
        bins: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        if conn.config.read_only {
            return Task::ready(Err(read_only_error(&conn.config.label)));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.put_record(&namespace, &set, &key, &bins).await
        })
    }

    /// Scans up to `limit` Aerospike records in a namespace/set. See
    /// [`db_client::provider::DbProvider::scan_records`].
    pub fn scan_records(
        &mut self,
        id: ConnectionId,
        namespace: String,
        set: String,
        limit: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<db_client::schema::QueryResult>> {
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.scan_records(&namespace, &set, limit).await
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

    pub fn list_check_constraints(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<CheckConstraintInfo>>> {
        if !self.connections.iter().any(|c| c.config.id == id) {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.list_check_constraints(&database, &table).await
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
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        if conn.config.read_only {
            return Task::ready(Err(read_only_error(&conn.config.label)));
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
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        if conn.config.read_only {
            return Task::ready(Err(read_only_error(&conn.config.label)));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.drop_table(&database, &table).await
        })
    }

    pub fn rename_table(
        &mut self,
        id: ConnectionId,
        database: String,
        old_name: String,
        new_name: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        if conn.config.read_only {
            return Task::ready(Err(read_only_error(&conn.config.label)));
        }
        cx.spawn(async move |this, cx| {
            let provider = this
                .update(cx, |store, cx| store.ensure_connected(id, cx))?
                .await?;
            provider.rename_table(&database, &old_name, &new_name).await
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

/// Maps `connect()`'s captured `build_provider` failure (if any) to the
/// `Task<Result<()>>` it returns to callers. Kept separate from `connect()`
/// itself so this mapping -- the exact thing that must reflect a real
/// connection failure instead of always resolving `Ok(())` -- is directly
/// unit-testable without needing a real provider connection attempt.
fn connect_task_result(connect_error: Option<String>) -> Result<()> {
    match connect_error {
        Some(message) => Err(anyhow::anyhow!(message)),
        None => Ok(()),
    }
}

async fn build_provider(
    config: &ConnectionConfig,
) -> Result<(Arc<dyn DbProvider>, Option<ActiveTunnel>)> {
    let (effective_config, tunnel) = if config.uses_ssh() {
        let ssh_host = config.ssh_host.as_deref().unwrap_or_default();
        let auth = match config.ssh_auth_method {
            SshAuthMethod::Password => SshAuth::Password(&config.ssh_password),
            SshAuthMethod::KeyFile => SshAuth::KeyFile(config.ssh_private_key_path.as_deref()),
        };
        let tunnel = SshTunnel::establish(
            ssh_host,
            config.ssh_port,
            config.ssh_username.as_deref(),
            auth,
            &config.host,
            config.port,
        )
        .await?;
        let local_port = tunnel.local_port();
        let mut modified = config.clone();
        modified.host = "127.0.0.1".to_string();
        modified.port = local_port;
        (modified, Some(ActiveTunnel::Ssh(tunnel)))
    } else if config.uses_kubernetes_tunnel() {
        let context = config.k8s_context.as_deref().unwrap_or_default();
        let mode = match config.k8s_tunnel_mode {
            KubernetesTunnelModeKind::PortForward => KubernetesTunnelMode::PortForward,
            KubernetesTunnelModeKind::Exec => {
                let relay = match config.k8s_relay_command {
                    KubernetesRelayCommandKind::Socat => KubernetesRelayCommand::Socat,
                    KubernetesRelayCommandKind::Nc => KubernetesRelayCommand::Nc,
                };
                KubernetesTunnelMode::Exec(relay)
            }
        };
        let target = match config.k8s_target_kind {
            KubernetesTargetKind::Pod => KubernetesTarget::Pod(config.k8s_target_name.clone()),
            KubernetesTargetKind::Service => {
                KubernetesTarget::Service(config.k8s_target_name.clone())
            }
        };
        let tunnel = KubernetesTunnel::establish(
            mode,
            context,
            &config.k8s_namespace,
            config.k8s_kubeconfig_path.as_deref(),
            target,
            &config.host,
            config.port,
        )
        .await?;
        let local_port = tunnel.local_port();
        let mut modified = config.clone();
        modified.host = "127.0.0.1".to_string();
        modified.port = local_port;
        (modified, Some(ActiveTunnel::Kubernetes(tunnel)))
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
        DatabaseDriver::MongoDB => {
            let config = effective_config.clone();
            Arc::new(on_runtime(async move { MongoProvider::connect(&config).await }).await?)
        }
        DatabaseDriver::Cassandra => {
            let config = effective_config.clone();
            Arc::new(on_runtime(async move { CassandraProvider::connect(&config).await }).await?)
        }
        DatabaseDriver::Aerospike => {
            let config = effective_config.clone();
            Arc::new(on_runtime(async move { AerospikeProvider::connect(&config).await }).await?)
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

fn schema_cache_file_path() -> std::path::PathBuf {
    paths::config_dir().join(SCHEMA_CACHE_FILE)
}

/// Loads the persisted schema cache, returning an empty map when the file is
/// missing or unreadable so a corrupt cache never blocks startup.
fn load_schema_cache_from_disk() -> HashMap<ConnectionId, SchemaCache> {
    let Ok(bytes) = std::fs::read(schema_cache_file_path()) else {
        return HashMap::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_schema_cache_to_disk(cache: &HashMap<ConnectionId, SchemaCache>) -> Result<()> {
    let json = serde_json::to_vec_pretty(cache)?;
    std::fs::write(schema_cache_file_path(), json)?;
    Ok(())
}

/// A saved association between a `.sql` file and the connection (and
/// optionally a specific database on it) it should always run against, so
/// running the same maintenance script twice never risks targeting the wrong
/// data source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunConfiguration {
    pub id: uuid::Uuid,
    pub name: String,
    pub file_path: std::path::PathBuf,
    pub connection_id: ConnectionId,
    pub database: Option<String>,
}

fn run_configs_file_path() -> std::path::PathBuf {
    paths::config_dir().join(RUN_CONFIGS_FILE)
}

/// Loads saved run configurations, returning an empty list when the file is
/// missing or unreadable so a corrupt file never blocks startup.
fn load_run_configs_from_disk() -> Vec<RunConfiguration> {
    let Ok(bytes) = std::fs::read(run_configs_file_path()) else {
        return Vec::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_run_configs_to_disk(configs: &[RunConfiguration]) -> Result<()> {
    let json = serde_json::to_vec_pretty(configs)?;
    std::fs::write(run_configs_file_path(), json)?;
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
    fn restore_tree_upserts_folders_and_connections(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        let folder = Folder::new("Imported".to_string(), None, 0);
        let folder_id = folder.id;
        let mut connection = ConnectionConfig::default();
        connection.label = "Imported DB".to_string();
        let connection_id = connection.id;

        store.update(cx, |store, cx| {
            store.restore_tree(vec![folder.clone()], vec![connection.clone()], cx);
            assert_eq!(store.folders().len(), 1);
            assert_eq!(store.connections().len(), 1);

            // Re-importing the same ids updates in place instead of duplicating.
            let mut updated_folder = folder.clone();
            updated_folder.name = "Renamed".to_string();
            let mut updated_connection = connection.clone();
            updated_connection.label = "Renamed DB".to_string();
            store.restore_tree(vec![updated_folder], vec![updated_connection], cx);

            assert_eq!(store.folders().len(), 1);
            assert_eq!(store.connections().len(), 1);
            assert_eq!(
                store
                    .folders()
                    .iter()
                    .find(|f| f.id == folder_id)
                    .unwrap()
                    .name,
                "Renamed"
            );
            assert_eq!(
                store
                    .connections()
                    .iter()
                    .find(|c| c.config.id == connection_id)
                    .unwrap()
                    .config
                    .label,
                "Renamed DB"
            );
        });
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
    fn reposition_item_reorders_same_parent_siblings(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            let a = store.add_folder("A".into(), None, cx).expect("a");
            let b = store.add_folder("B".into(), None, cx).expect("b");
            let c = store.add_folder("C".into(), None, cx).expect("c");

            // A, B, C -> drop A after C -> B, C, A.
            assert!(store.reposition_item(
                TreeItemRef::Folder(a),
                TreeItemRef::Folder(c),
                RelativePosition::After,
                cx,
            ));
            assert_eq!(
                store.combined_siblings(None),
                [
                    TreeItemRef::Folder(b),
                    TreeItemRef::Folder(c),
                    TreeItemRef::Folder(a),
                ]
            );

            // Drop A before B -> A, B, C again.
            assert!(store.reposition_item(
                TreeItemRef::Folder(a),
                TreeItemRef::Folder(b),
                RelativePosition::Before,
                cx,
            ));
            assert_eq!(
                store.combined_siblings(None),
                [
                    TreeItemRef::Folder(a),
                    TreeItemRef::Folder(b),
                    TreeItemRef::Folder(c),
                ]
            );
        });
    }

    #[gpui::test]
    fn reposition_item_interleaves_folders_and_connections(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            let folder = store.add_folder("F".into(), None, cx).expect("folder");
            let conn = config_in(None, 1);
            let conn_id = conn.id;
            store.connections.push(ActiveConnection::new(conn));

            // Root siblings today: folder (order 0), connection (order 1).
            assert_eq!(
                store.combined_siblings(None),
                [
                    TreeItemRef::Folder(folder),
                    TreeItemRef::Connection(conn_id)
                ]
            );

            // Drop the connection before the folder -> connection, folder.
            assert!(store.reposition_item(
                TreeItemRef::Connection(conn_id),
                TreeItemRef::Folder(folder),
                RelativePosition::Before,
                cx,
            ));
            assert_eq!(
                store.combined_siblings(None),
                [
                    TreeItemRef::Connection(conn_id),
                    TreeItemRef::Folder(folder)
                ]
            );
        });
    }

    #[gpui::test]
    fn reposition_item_moves_across_parents(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            let folder_a = store.add_folder("A".into(), None, cx).expect("a");
            let folder_b = store.add_folder("B".into(), None, cx).expect("b");
            let conn = config_in(Some(folder_a), 0);
            let conn_id = conn.id;
            store.connections.push(ActiveConnection::new(conn));
            let anchor = config_in(Some(folder_b), 0);
            let anchor_id = anchor.id;
            store.connections.push(ActiveConnection::new(anchor));

            // Drag the connection out of folder A and drop it after the
            // connection already sitting in folder B.
            assert!(store.reposition_item(
                TreeItemRef::Connection(conn_id),
                TreeItemRef::Connection(anchor_id),
                RelativePosition::After,
                cx,
            ));

            assert!(store.combined_siblings(Some(folder_a)).is_empty());
            assert_eq!(
                store.combined_siblings(Some(folder_b)),
                [
                    TreeItemRef::Connection(anchor_id),
                    TreeItemRef::Connection(conn_id)
                ]
            );
            let moved = store
                .connections
                .iter()
                .find(|c| c.config.id == conn_id)
                .expect("conn");
            assert_eq!(moved.config.folder_id, Some(folder_b));
        });
    }

    #[gpui::test]
    fn reposition_item_rejects_no_op_and_cycles(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            let parent = store.add_folder("parent".into(), None, cx).expect("parent");
            let child = store
                .add_folder("child".into(), Some(parent), cx)
                .expect("child");
            let sibling = store
                .add_folder("sibling".into(), None, cx)
                .expect("sibling");

            // Dropping an item relative to itself is a no-op.
            assert!(!store.reposition_item(
                TreeItemRef::Folder(sibling),
                TreeItemRef::Folder(sibling),
                RelativePosition::After,
                cx,
            ));

            // Dropping a folder next to its own descendant would orphan the
            // cycle — rejected, same guard as `move_folder`.
            assert!(!store.reposition_item(
                TreeItemRef::Folder(parent),
                TreeItemRef::Folder(child),
                RelativePosition::Before,
                cx,
            ));
            store.folders.iter().for_each(|f| {
                if f.id == parent {
                    assert_eq!(
                        f.parent_id, None,
                        "rejected move must not mutate the parent"
                    );
                }
            });
        });
    }

    #[gpui::test]
    fn reposition_item_handles_single_item_list(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            let only = store.add_folder("only".into(), None, cx).expect("only");
            let other_parent = store.add_folder("other".into(), None, cx).expect("other");

            // A single-item sibling list has no valid anchor other than itself
            // once "only" is excluded, but repositioning it relative to a
            // folder under a different (empty) parent must still work cleanly.
            assert!(store.reposition_item(
                TreeItemRef::Folder(only),
                TreeItemRef::Folder(other_parent),
                RelativePosition::After,
                cx,
            ));
            assert_eq!(
                store
                    .folders
                    .iter()
                    .find(|f| f.id == only)
                    .unwrap()
                    .parent_id,
                None
            );
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
    fn redacts_ssh_password_too() {
        let config = ConnectionConfig {
            ssh_password: "tunnel-secret".to_string(),
            ..ConnectionConfig::default()
        };
        let redacted = redact_password(&config);
        assert!(redacted.ssh_password.is_empty());
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

    #[gpui::test]
    async fn ssh_password_roundtrips_through_provider_independently_of_db_password(
        cx: &mut gpui::TestAppContext,
    ) {
        let provider: Arc<dyn CredentialsProvider> = Arc::new(MockCredentials::default());
        let config = ConnectionConfig {
            password: "db-secret".to_string(),
            ssh_auth_method: SshAuthMethod::Password,
            ssh_password: "tunnel-secret".to_string(),
            ..ConnectionConfig::default()
        };
        let async_cx = cx.to_async();

        store_connection_password(&provider, &config, &async_cx)
            .await
            .expect("write db password");
        store_connection_ssh_password(&provider, &config, &async_cx)
            .await
            .expect("write ssh password");

        let db_password = read_connection_password(&provider, config.id, &async_cx)
            .await
            .expect("read db password");
        let ssh_password = read_connection_ssh_password(&provider, config.id, &async_cx)
            .await
            .expect("read ssh password");
        assert_eq!(db_password.as_deref(), Some("db-secret"));
        assert_eq!(ssh_password.as_deref(), Some("tunnel-secret"));

        provider
            .delete_credentials(&ssh_connection_credentials_url(config.id), &async_cx)
            .await
            .expect("delete ssh password");
        let db_password_still_there = read_connection_password(&provider, config.id, &async_cx)
            .await
            .expect("read db password");
        let ssh_password_after_delete =
            read_connection_ssh_password(&provider, config.id, &async_cx)
                .await
                .expect("read ssh password");
        assert_eq!(
            db_password_still_there.as_deref(),
            Some("db-secret"),
            "deleting the SSH secret must not touch the unrelated DB password entry"
        );
        assert_eq!(ssh_password_after_delete, None);
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

    struct RoutineMockProvider;

    #[async_trait::async_trait]
    impl DbProvider for RoutineMockProvider {
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
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> Result<String> {
            Ok(String::new())
        }
        async fn list_procedures(&self, _database: &str) -> Result<Vec<ProcedureInfo>> {
            Ok(vec![ProcedureInfo {
                name: "recalc_totals".into(),
                kind: db_client::schema::ProcedureKind::Procedure,
                definition: Some("BEGIN UPDATE orders SET total = 0; END".into()),
            }])
        }
        async fn list_sequences(&self, _database: &str) -> Result<Vec<SequenceInfo>> {
            Ok(vec![SequenceInfo {
                name: "orders_id_seq".into(),
                current_value: Some(42),
                increment: Some(1),
            }])
        }
        async fn list_events(&self, _database: &str) -> Result<Vec<EventInfo>> {
            Ok(vec![EventInfo {
                name: "nightly_cleanup".into(),
                status: Some("ENABLED".into()),
                definition: Some("DELETE FROM sessions WHERE expired = 1".into()),
            }])
        }
    }

    #[gpui::test]
    fn toggle_database_expanded_loads_procedures_sequences_and_events(
        cx: &mut gpui::TestAppContext,
    ) {
        let store = cx.new(DatabaseStore::new);
        let config = ConnectionConfig::default();
        let id = config.id;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(RoutineMockProvider), cx);
            store
                .toggle_database_expanded(id, "shop".to_string(), cx)
                .detach();
        });
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let conn = store
                .connections()
                .iter()
                .find(|c| c.config.id == id)
                .unwrap();
            let procedures = conn.db_procedures.get("shop").expect("procedures cached");
            assert_eq!(procedures.len(), 1);
            assert_eq!(procedures[0].name, "recalc_totals");
            assert_eq!(
                procedures[0].definition.as_deref(),
                Some("BEGIN UPDATE orders SET total = 0; END")
            );

            let sequences = conn.db_sequences.get("shop").expect("sequences cached");
            assert_eq!(sequences.len(), 1);
            assert_eq!(sequences[0].name, "orders_id_seq");
            assert_eq!(sequences[0].current_value, Some(42));

            let events = conn.db_events.get("shop").expect("events cached");
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].name, "nightly_cleanup");
        });
    }

    #[gpui::test]
    fn refresh_schema_cache_clears_procedures_sequences_and_events(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        let config = ConnectionConfig::default();
        let id = config.id;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(RoutineMockProvider), cx);
            store
                .toggle_database_expanded(id, "shop".to_string(), cx)
                .detach();
        });
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            let conn = store
                .connections()
                .iter()
                .find(|c| c.config.id == id)
                .unwrap();
            assert!(!conn.db_procedures.is_empty());
        });

        store.update(cx, |store, cx| {
            store.refresh_schema_cache(id, cx).detach();
        });
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let conn = store
                .connections()
                .iter()
                .find(|c| c.config.id == id)
                .unwrap();
            assert!(
                conn.db_procedures.is_empty(),
                "refreshing the schema cache must drop stale cached routines"
            );
            assert!(conn.db_sequences.is_empty());
            assert!(conn.db_events.is_empty());
        });
    }

    struct MultiDbSchemaMockProvider;

    #[async_trait::async_trait]
    impl DbProvider for MultiDbSchemaMockProvider {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
            Ok(vec![
                DatabaseInfo {
                    name: "shop".into(),
                },
                DatabaseInfo {
                    name: "analytics".into(),
                },
            ])
        }
        async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>> {
            let name = if database == "shop" {
                "users"
            } else {
                "events"
            };
            Ok(vec![TableInfo {
                name: name.into(),
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
        let db_fetch = store.update(cx, |store, cx| {
            store.get_database_ddl(id, "shop".into(), cx)
        });
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
        let db_cached = store.update(cx, |store, cx| {
            store.get_database_ddl(id, "shop".into(), cx)
        });
        assert_eq!(
            db_cached.await.expect("cached while offline"),
            "CREATE DATABASE `shop`"
        );
    }

    // A real end-to-end #[gpui::test] driving connect() through an actual
    // (even locally-failing) provider connection attempt hits GPUI's
    // deterministic test scheduler's "parking forbidden" guard, since the
    // real connection attempt needs genuine async I/O to resolve -- exactly
    // why connect()'s network path is otherwise left to integration/manual
    // testing (see the comment on `ensure_connected_returns_existing_provider`
    // below). `connect_task_result` isolates the exact logic that was buggy
    // (the mapping from a captured failure to the Task's Ok/Err) so it can be
    // tested directly without any real connection attempt.
    #[test]
    fn connect_task_result_reports_failure_instead_of_always_ok() {
        assert!(connect_task_result(None).is_ok());
        let err = connect_task_result(Some("boom".to_string()))
            .expect_err("a captured connect error must not resolve to Ok(())");
        assert_eq!(err.to_string(), "boom");
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

    #[gpui::test]
    fn schema_objects_flattens_the_prefetched_cache_for_the_go_to_object_palette(
        cx: &mut gpui::TestAppContext,
    ) {
        let store = cx.new(DatabaseStore::new);
        let config = ConnectionConfig::default();
        let id = config.id;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(SchemaMockProvider), cx);
        });
        // Before the schema is prefetched there is nothing to flatten yet.
        store.read_with(cx, |store, _| {
            assert!(store.schema_objects(id).is_empty());
        });

        store.update(cx, |store, cx| {
            store.prefetch_full_schema(id, cx).detach();
        });
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let objects = store.schema_objects(id);
            let labels: Vec<String> = objects.iter().map(|o| o.display_label()).collect();
            assert!(
                labels.contains(&"shop".to_string()),
                "expected the database itself: {labels:?}"
            );
            assert!(
                labels.contains(&"shop.users".to_string()),
                "expected the table: {labels:?}"
            );
            assert!(
                labels.contains(&"shop.users.id".to_string()),
                "expected the column: {labels:?}"
            );
            assert_eq!(
                objects
                    .iter()
                    .find(|o| o.display_label() == "shop")
                    .map(|o| o.kind),
                Some(SchemaObjectKind::Database)
            );
            assert_eq!(
                objects
                    .iter()
                    .find(|o| o.display_label() == "shop.users")
                    .map(|o| o.kind),
                Some(SchemaObjectKind::Table)
            );
            assert_eq!(
                objects
                    .iter()
                    .find(|o| o.display_label() == "shop.users.id")
                    .map(|o| o.kind),
                Some(SchemaObjectKind::Column)
            );
        });
    }

    #[gpui::test]
    fn prefetch_full_schema_populates_index(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        let config = ConnectionConfig::default();
        let id = config.id;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(SchemaMockProvider), cx);
            store.prefetch_full_schema(id, cx).detach();
        });
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let conn = store
                .connections()
                .iter()
                .find(|c| c.config.id == id)
                .unwrap();
            assert!(conn.databases.is_some(), "databases indexed");
            assert_eq!(
                conn.expanded_databases.get("shop").map(|t| t.len()),
                Some(1),
                "tables indexed for every database"
            );
            assert!(
                conn.expanded_tables
                    .contains_key(&("shop".to_string(), "users".to_string())),
                "columns indexed for the primary database"
            );
            // Prefetch must not expand the tree.
            assert!(conn.expanded_database_set.is_empty());
            assert!(conn.expanded_table_set.is_empty());
        });
    }

    #[gpui::test]
    fn prefetch_full_schema_indexes_columns_for_all_databases(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        let config = ConnectionConfig::default();
        let id = config.id;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(MultiDbSchemaMockProvider), cx);
            store.prefetch_full_schema(id, cx).detach();
        });
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let conn = store
                .connections()
                .iter()
                .find(|c| c.config.id == id)
                .unwrap();
            assert!(
                conn.expanded_tables
                    .contains_key(&("shop".to_string(), "users".to_string())),
                "columns indexed for the primary database"
            );
            assert!(
                conn.expanded_tables
                    .contains_key(&("analytics".to_string(), "events".to_string())),
                "columns indexed for non-primary databases within the budget"
            );
        });
    }

    #[test]
    fn run_configuration_round_trips_through_serde() {
        let config = RunConfiguration {
            id: uuid::Uuid::new_v4(),
            name: "Weekly cleanup".to_string(),
            file_path: std::path::PathBuf::from("/scripts/weekly_cleanup.sql"),
            connection_id: ConnectionConfig::default().id,
            database: Some("analytics".to_string()),
        };
        let bytes = serde_json::to_vec(&config).expect("serialize");
        let restored: RunConfiguration = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(restored, config);
    }

    #[gpui::test]
    fn run_configuration_resolves_by_its_exact_file_path(cx: &mut gpui::TestAppContext) {
        let store = cx.new(DatabaseStore::new);
        let connection_id = ConnectionConfig::default().id;
        let path = std::path::PathBuf::from("/scripts/seed.sql");
        store.update(cx, |store, cx| {
            store.set_run_configuration(
                RunConfiguration {
                    id: uuid::Uuid::new_v4(),
                    name: "Seed".to_string(),
                    file_path: path.clone(),
                    connection_id,
                    database: None,
                },
                cx,
            );
        });
        store.read_with(cx, |store, _| {
            let found = store
                .run_configuration_for_path(&path)
                .expect("configuration must resolve by its exact path");
            assert_eq!(found.connection_id, connection_id);
            assert!(
                store
                    .run_configuration_for_path(std::path::Path::new("/scripts/other.sql"))
                    .is_none(),
                "an unrelated path must not resolve to someone else's configuration"
            );
        });
    }

    #[gpui::test]
    fn saving_a_second_run_configuration_for_the_same_file_replaces_the_first(
        cx: &mut gpui::TestAppContext,
    ) {
        let store = cx.new(DatabaseStore::new);
        let path = std::path::PathBuf::from("/scripts/seed.sql");
        let first_connection = ConnectionConfig::default().id;
        let second_connection = ConnectionConfig::default().id;
        store.update(cx, |store, cx| {
            store.set_run_configuration(
                RunConfiguration {
                    id: uuid::Uuid::new_v4(),
                    name: "Seed".to_string(),
                    file_path: path.clone(),
                    connection_id: first_connection,
                    database: None,
                },
                cx,
            );
            store.set_run_configuration(
                RunConfiguration {
                    id: uuid::Uuid::new_v4(),
                    name: "Seed".to_string(),
                    file_path: path.clone(),
                    connection_id: second_connection,
                    database: None,
                },
                cx,
            );
        });
        store.read_with(cx, |store, _| {
            assert_eq!(
                store
                    .run_configurations()
                    .iter()
                    .filter(|config| config.file_path == path)
                    .count(),
                1,
                "a file must have at most one run configuration at a time"
            );
            assert_eq!(
                store
                    .run_configuration_for_path(&path)
                    .unwrap()
                    .connection_id,
                second_connection
            );
        });
    }

    #[test]
    fn schema_cache_round_trips() {
        let id = ConnectionConfig::default().id;
        let mut cache: HashMap<ConnectionId, SchemaCache> = HashMap::new();
        let entry = cache.entry(id).or_default();
        entry.databases.push(DatabaseInfo {
            name: "shop".into(),
        });
        entry.tables.insert(
            "shop".into(),
            vec![TableInfo {
                name: "users".into(),
                kind: db_client::schema::TableKind::Table,
            }],
        );
        entry.columns.entry("shop".into()).or_default().insert(
            "users".into(),
            vec![ColumnInfo {
                name: "id".into(),
                data_type: "int".into(),
                is_nullable: false,
                column_key: Some("PRI".into()),
                default_value: None,
                extra: String::new(),
            }],
        );

        let bytes = serde_json::to_vec(&cache).expect("serialize");
        let restored: HashMap<ConnectionId, SchemaCache> =
            serde_json::from_slice(&bytes).expect("deserialize");
        let snapshot = restored.get(&id).expect("connection entry");
        assert_eq!(
            snapshot.databases.first().map(|d| d.name.as_str()),
            Some("shop")
        );
        assert_eq!(
            snapshot
                .tables
                .get("shop")
                .and_then(|t| t.first())
                .map(|t| t.name.as_str()),
            Some("users")
        );
        assert_eq!(
            snapshot
                .columns
                .get("shop")
                .and_then(|t| t.get("users"))
                .and_then(|c| c.first())
                .map(|c| c.name.as_str()),
            Some("id")
        );
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

    #[gpui::test]
    async fn execute_query_blocks_writes_but_allows_reads_on_a_read_only_connection(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut config = ConnectionConfig::default();
        config.label = "prod".into();
        config.read_only = true;
        let id = config.id;
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(CliMockProvider), cx);
        });

        let write = store.update(cx, |store, cx| {
            store.execute_query(id, String::new(), "UPDATE users SET name = 'x'".into(), cx)
        });
        let error = write
            .await
            .expect_err("a write must be rejected on a read-only connection");
        assert!(
            error.to_string().contains("read-only"),
            "the error should explain why the write was blocked: {error}"
        );

        let read = store.update(cx, |store, cx| {
            store.execute_query(id, String::new(), "SELECT 1".into(), cx)
        });
        read.await
            .expect("reads must still run on a read-only connection");
    }

    #[gpui::test]
    async fn truncate_and_drop_table_are_blocked_on_a_read_only_connection(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut config = ConnectionConfig::default();
        config.read_only = true;
        let id = config.id;
        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(CliMockProvider), cx);
        });

        let truncate = store.update(cx, |store, cx| {
            store.truncate_table(id, "shop".into(), "users".into(), cx)
        });
        assert!(
            truncate.await.is_err(),
            "Truncate Table must be blocked on a read-only connection"
        );

        let drop = store.update(cx, |store, cx| {
            store.drop_table(id, "shop".into(), "users".into(), cx)
        });
        assert!(
            drop.await.is_err(),
            "Drop Table must be blocked on a read-only connection"
        );
    }
}
