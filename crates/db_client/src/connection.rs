use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type ConnectionId = Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DatabaseDriver {
    MySQL,
    PostgreSQL,
    SQLite,
    ClickHouse,
    Redis,
}

impl fmt::Display for DatabaseDriver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DatabaseDriver::MySQL => write!(formatter, "MySQL"),
            DatabaseDriver::PostgreSQL => write!(formatter, "PostgreSQL"),
            DatabaseDriver::SQLite => write!(formatter, "SQLite"),
            DatabaseDriver::ClickHouse => write!(formatter, "ClickHouse"),
            DatabaseDriver::Redis => write!(formatter, "Redis"),
        }
    }
}

impl DatabaseDriver {
    pub fn default_port(self) -> u16 {
        match self {
            DatabaseDriver::MySQL => 3306,
            DatabaseDriver::PostgreSQL => 5432,
            DatabaseDriver::SQLite => 0,
            DatabaseDriver::ClickHouse => 8123,
            DatabaseDriver::Redis => 6379,
        }
    }

    pub fn is_file_based(self) -> bool {
        matches!(self, DatabaseDriver::SQLite)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: ConnectionId,
    pub label: String,
    pub driver: DatabaseDriver,
    /// Host/IP for network drivers; file path for file-based drivers (SQLite).
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub database: Option<String>,
    #[serde(default = "default_true")]
    pub auto_connect: bool,
    #[serde(default)]
    pub ssh_host: Option<String>,
    #[serde(default = "default_22")]
    pub ssh_port: u16,
    #[serde(default)]
    pub ssh_username: Option<String>,
    #[serde(default)]
    pub ssh_private_key_path: Option<String>,
}

impl ConnectionConfig {
    pub fn uses_ssh(&self) -> bool {
        self.ssh_host.is_some()
    }
}

fn default_true() -> bool {
    true
}

fn default_22() -> u16 {
    22
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            label: String::from("New Connection"),
            driver: DatabaseDriver::MySQL,
            host: String::from("localhost"),
            port: 3306,
            username: String::from("root"),
            password: String::new(),
            database: None,
            auto_connect: true,
            ssh_host: None,
            ssh_port: 22,
            ssh_username: None,
            ssh_private_key_path: None,
        }
    }
}
