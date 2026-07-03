use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type ConnectionId = Uuid;
pub type FolderId = Uuid;

/// Maximum nesting depth for connection folders (root folder = depth 1).
pub const MAX_FOLDER_DEPTH: usize = 5;

/// A folder node in the connection tree. Folders hold only other folders and
/// connections; `parent_id == None` means the folder sits at the top level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub id: FolderId,
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<FolderId>,
    #[serde(default)]
    pub order: i64,
}

impl Folder {
    pub fn new(name: String, parent_id: Option<FolderId>, order: i64) -> Self {
        Self {
            id: Uuid::new_v4(),
            name,
            parent_id,
            order,
        }
    }
}

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

    /// Quotes an identifier (table/column/database name) for use in a
    /// generated SQL statement, escaping any embedded quote character by
    /// doubling it. MySQL uses backticks; every other SQL driver here uses
    /// ANSI double quotes.
    pub fn quote_identifier(self, name: &str) -> String {
        match self {
            DatabaseDriver::MySQL => format!("`{}`", name.replace('`', "``")),
            _ => format!("\"{}\"", name.replace('"', "\"\"")),
        }
    }
}

/// SSL/TLS mode for network drivers that support encrypted connections
/// (MySQL, PostgreSQL). Maps onto each driver's native SSL mode enum at
/// connect time — see `DatabaseDriver`-specific connect-options builders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SslMode {
    /// Establish an unencrypted connection.
    #[default]
    Disabled,
    /// Require an encrypted connection; fail if one cannot be established.
    Require,
    /// Require encryption and verify the server certificate against `ssl_ca_path`.
    VerifyCa,
    /// Like `VerifyCa`, but also verify the server host name matches the certificate.
    VerifyFull,
}

/// How the SSH tunnel authenticates to the bastion host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshAuthMethod {
    #[default]
    KeyFile,
    Password,
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
    /// Chooses between `ssh_private_key_path` and `ssh_password` for tunnel auth.
    #[serde(default)]
    pub ssh_auth_method: SshAuthMethod,
    /// Plaintext only in memory; redacted to empty before the connection tree
    /// is written to disk, same as `password`. The real secret lives in the
    /// OS keychain.
    #[serde(default)]
    pub ssh_password: String,
    #[serde(default)]
    pub ssl_mode: SslMode,
    #[serde(default)]
    pub ssl_ca_path: Option<String>,
    #[serde(default)]
    pub ssl_client_cert_path: Option<String>,
    #[serde(default)]
    pub ssl_client_key_path: Option<String>,
    /// Legacy flat folder name, kept only so old config files still load. New
    /// code groups by `folder_id`; migration converts this into a `Folder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Parent folder, or `None` for a top-level connection.
    #[serde(default)]
    pub folder_id: Option<FolderId>,
    /// Sort order within the parent folder (or the top level).
    #[serde(default)]
    pub order: i64,
    /// Optional hex color (e.g. `"#e74c3c"`) identifying the environment
    /// (LOCAL / STAGE / PROD). Shown as a colored circle next to the connection.
    #[serde(default)]
    pub env_color: Option<String>,
    /// When set, every write path for this connection (grid edits, row
    /// insert/delete, DDL/DML execution, Drop/Truncate, agent writes) is
    /// rejected instead of reaching the database.
    #[serde(default)]
    pub read_only: bool,
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
            ssh_auth_method: SshAuthMethod::KeyFile,
            ssh_password: String::new(),
            ssl_mode: SslMode::Disabled,
            ssl_ca_path: None,
            ssl_client_cert_path: None,
            ssl_client_key_path: None,
            folder: None,
            folder_id: None,
            order: 0,
            env_color: None,
            read_only: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_identifier_uses_backticks_for_mysql_and_escapes_embedded_backticks() {
        assert_eq!(
            DatabaseDriver::MySQL.quote_identifier("orders"),
            "`orders`"
        );
        assert_eq!(
            DatabaseDriver::MySQL.quote_identifier("weird`name"),
            "`weird``name`"
        );
    }

    #[test]
    fn quote_identifier_uses_double_quotes_for_non_mysql_drivers_and_escapes_embedded_quotes() {
        for driver in [
            DatabaseDriver::PostgreSQL,
            DatabaseDriver::SQLite,
            DatabaseDriver::ClickHouse,
            DatabaseDriver::Redis,
        ] {
            assert_eq!(driver.quote_identifier("orders"), "\"orders\"");
            assert_eq!(
                driver.quote_identifier("weird\"name"),
                "\"weird\"\"name\""
            );
        }
    }
}
