use anyhow::Result;
use db_client::{
    ConnectionConfig, ConnectionId, DatabaseDriver, FkInfo, RuntimeProvider, SshTunnel,
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
use credentials_provider::CredentialsProvider;
use gpui::{App, AppContext as _, AsyncApp, Context, EventEmitter, Task, TaskExt as _};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use util::ResultExt;

const MAX_QUERY_HISTORY: usize = 100;
const CONNECTIONS_FILE: &str = "db_connections.json";

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
    pub query_history: Vec<String>,
    pub active_connection_id: Option<ConnectionId>,
    ssh_tunnels: HashMap<ConnectionId, SshTunnel>,
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
                    .spawn(async { load_connections_from_disk() })
                    .await;
                if let Ok(mut configs) = result {
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
                        for config in configs {
                            store.connections.push(ActiveConnection::new(config));
                        }
                        if !store.connections.is_empty() {
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

        Self {
            connections: Vec::new(),
            query_history: Vec::new(),
            active_connection_id: None,
            ssh_tunnels: HashMap::new(),
        }
    }

    pub fn connections(&self) -> &[ActiveConnection] {
        &self.connections
    }

    pub fn active_connection_id(&self) -> Option<ConnectionId> {
        self.active_connection_id
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

    pub fn add_connection(&mut self, config: ConnectionConfig, cx: &mut Context<Self>) {
        self.connections.push(ActiveConnection::new(config));
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
        self.persist_connections(cx);
    }

    pub fn update_connection(&mut self, config: ConnectionConfig, cx: &mut Context<Self>) {
        let config_id = config.id;
        if let Some(conn) = self.connections.iter_mut().find(|c| c.config.id == config_id) {
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
        let configs: Vec<ConnectionConfig> =
            self.connections.iter().map(|c| c.config.clone()).collect();
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
                    save_connections_to_disk(redacted).log_err();
                })
                .await;
        })
        .detach();
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
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };
        conn.expanded_database_set.insert(database.clone());
        cx.notify();

        cx.spawn(async move |this, cx| {
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
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };
        conn.expanded_table_set.insert(key.clone());
        cx.notify();

        cx.spawn(async move |this, cx| {
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
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };

        cx.spawn(async move |this, cx| {
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
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };

        self.record_query_history(sql.clone(), cx);

        cx.background_spawn(async move { provider.execute_query(&database, &sql).await })
    }

    pub fn describe_table(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<ColumnInfo>>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };
        cx.background_spawn(async move { provider.describe_table(&database, &table).await })
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
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };
        cx.background_spawn(async move { provider.get_table_ddl(&database, &table).await })
    }

    pub fn list_indexes(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<IndexInfo>>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };
        cx.background_spawn(async move { provider.list_indexes(&database, &table).await })
    }

    pub fn list_foreign_keys(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<FkInfo>>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };
        cx.background_spawn(async move { provider.list_foreign_keys(&database, &table).await })
    }

    pub fn list_procedures(
        &mut self,
        id: ConnectionId,
        database: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<ProcedureInfo>>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };
        cx.background_spawn(async move { provider.list_procedures(&database).await })
    }

    pub fn list_triggers(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<TriggerInfo>>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };
        cx.background_spawn(async move { provider.list_triggers(&database, &table).await })
    }

    pub fn list_users(
        &mut self,
        id: ConnectionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<UserInfo>>> {
        let Some(conn) = self.connections.iter().find(|c| c.config.id == id) else {
            return Task::ready(Err(anyhow::anyhow!("Connection not found")));
        };
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };
        cx.background_spawn(async move { provider.list_users().await })
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
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };
        cx.background_spawn(async move { provider.truncate_table(&database, &table).await })
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
        let Some(provider) = conn.provider.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Not connected")));
        };
        cx.background_spawn(async move { provider.drop_table(&database, &table).await })
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

fn load_connections_from_disk() -> Result<Vec<ConnectionConfig>> {
    let bytes = std::fs::read(connections_file_path())?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn save_connections_to_disk(configs: Vec<ConnectionConfig>) -> Result<()> {
    let json = serde_json::to_vec_pretty(&configs)?;
    std::fs::write(connections_file_path(), json)?;
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
}
