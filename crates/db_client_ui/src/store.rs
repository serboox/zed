use anyhow::Result;
use db_client::{
    ConnectionConfig, ConnectionId, DatabaseDriver, SshTunnel,
    clickhouse::ClickHouseProvider,
    mysql::MySqlProvider,
    postgres::PostgresProvider,
    provider::DbProvider,
    redis_provider::RedisProvider,
    schema::{
        ColumnInfo, DatabaseInfo, IndexInfo, ProcedureInfo, TableInfo, TriggerInfo, UserInfo,
    },
    sqlite::SqliteProvider,
};
use gpui::{AppContext as _, Context, EventEmitter, Task, TaskExt as _};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use util::ResultExt;

const MAX_QUERY_HISTORY: usize = 100;
const CONNECTIONS_FILE: &str = "db_connections.json";

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
    pub expanded_tables: HashMap<(String, String), Vec<ColumnInfo>>,
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
            expanded_tables: HashMap::new(),
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
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { load_connections_from_disk() })
                .await;
            if let Ok(configs) = result {
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
        self.persist_connections(cx);
    }

    fn persist_connections(&self, cx: &mut Context<Self>) {
        let configs: Vec<ConnectionConfig> =
            self.connections.iter().map(|c| c.config.clone()).collect();
        cx.background_spawn(async move {
            save_connections_to_disk(configs).log_err();
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
            this.update(cx, |this, cx| {
                let Some(conn) = this.connections.iter_mut().find(|c| c.config.id == id) else {
                    return;
                };
                conn.expanded_databases.insert(database, tables);
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
            this.update(cx, |this, cx| {
                let Some(conn) = this.connections.iter_mut().find(|c| c.config.id == id) else {
                    return;
                };
                conn.expanded_tables.insert((database, table), columns);
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

    let provider: Arc<dyn DbProvider> = match effective_config.driver {
        DatabaseDriver::MySQL => MySqlProvider::connect(&effective_config)
            .await
            .map(|p| Arc::new(p) as Arc<dyn DbProvider>)?,
        DatabaseDriver::PostgreSQL => PostgresProvider::connect(&effective_config)
            .await
            .map(|p| Arc::new(p) as Arc<dyn DbProvider>)?,
        DatabaseDriver::SQLite => SqliteProvider::connect(&effective_config)
            .await
            .map(|p| Arc::new(p) as Arc<dyn DbProvider>)?,
        DatabaseDriver::ClickHouse => ClickHouseProvider::connect(&effective_config)
            .await
            .map(|p| Arc::new(p) as Arc<dyn DbProvider>)?,
        DatabaseDriver::Redis => RedisProvider::connect(&effective_config)
            .await
            .map(|p| Arc::new(p) as Arc<dyn DbProvider>)?,
    };
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
