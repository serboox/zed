use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::TryStreamExt as _;
use smol::lock::Mutex as AsyncMutex;
use sqlx::AssertSqlSafe;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlSslMode};
use sqlx::{Column as _, Row as _, TypeInfo as _};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::MAX_RESULT_ROWS;

use crate::connection::{ConnectionConfig, SslMode};
use crate::provider::DbProvider;
use crate::schema::{
    CheckConstraintInfo, ColumnInfo, DatabaseInfo, EventInfo, FkInfo, IndexInfo, ProcedureInfo,
    ProcedureKind, QueryResult, QueryTiming, TableInfo, TableKind, TriggerInfo, UserInfo,
};

pub struct MySqlProvider {
    pool: RwLock<MySqlPool>,
    connect_options: MySqlConnectOptions,
    // Serializes every DB operation on the provider. The pool is already
    // `max_connections(1)`, so all physical access was serialized on that one
    // permit anyway; taking this lock first adds no real serialization cost.
    // What it buys: the health probe in `ensure_live_pool` never competes for
    // the connection with a legitimately long-running query issued by another
    // concurrent caller (which would otherwise make the probe time out and
    // trigger a spurious reconnect), and `reconnect` is inherently
    // single-flight -- two callers can never build and swap in two
    // replacement pools at once.
    op_lock: AsyncMutex<()>,
    /// The database this connection was last switched to. The switch is a round
    /// trip of its own, and on a distant server that costs as much as the query
    /// it precedes, so it is not repeated for a database already current.
    /// Cleared whenever the pool is replaced: a fresh connection has had no
    /// `USE` applied to it.
    current_database: Mutex<Option<String>>,
}

/// Bounds how long a silently dead physical connection can stall a caller
/// before this provider gives up on it and opens a replacement -- see
/// `ensure_live_pool`, which is the only place that runs a probe under this
/// bound.
///
/// `max_connections(1)` means the pool has exactly one physical connection.
/// sqlx's own health checks (`test_before_acquire`, and the ping run when a
/// connection is handed back to the pool) have no timeout of their own, and
/// the connection-return bookkeeping after every query runs in a detached
/// background task. So when the connection goes silently dead -- a frozen
/// SSH tunnel, a laptop sleep/resume, a dropped VPN: no FIN/RST, packets
/// just stop flowing -- that background task's health check blocks on a
/// socket read that will never return, forever holding the pool's only
/// connection permit. Cancelling the caller that triggered it does not
/// help, because that background task isn't tied to the caller's future.
/// From then on every later call on the pool blocks forever too, since
/// there is no second connection to fall back to. This was reproduced with
/// an integration test that freezes the TCP path (via SIGSTOP on a proxy)
/// right after a successful query and observes every later query hang
/// indefinitely -- see `test_query_recovers_after_connection_silently_freezes`.
///
/// This bound bounds a `SELECT 1` round trip on a connection that is
/// already open -- no new TCP handshake, TLS negotiation, or MySQL auth
/// exchange, just one packet each way -- so it stays tight no matter how
/// far away the server is. It must NOT also bound the full `reconnect`
/// that runs when the probe fails; that op is a fundamentally heavier one
/// with its own, more generous budget -- see `RECONNECT_TIMEOUT`.
///
/// The probe that uses this runs while holding `op_lock`, so it can never
/// lose a race for the single connection to a concurrent legitimate query
/// and thus never mistakes "busy" for "dead".
const CONNECTION_HEALTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds how long `reconnect` may take to open a brand-new physical
/// connection -- a fresh TCP handshake, TLS negotiation (when enabled), and
/// a full MySQL auth exchange -- once `ensure_live_pool`'s probe (bounded by
/// `CONNECTION_HEALTH_TIMEOUT`) has decided the current connection is dead.
///
/// This must be more generous than the health-check probe: the probe only
/// needs one round trip on a socket that is already open, while this builds
/// an entire connection from scratch. Over a real corporate network or VPN
/// path to a remote host, DNS resolution, TCP setup, TLS negotiation, and
/// MySQL's own greeting/auth round trips can legitimately add up to several
/// seconds even though the destination is perfectly healthy -- nothing like
/// the near-instant setup against a local Docker container. Reusing
/// `CONNECTION_HEALTH_TIMEOUT` (10s) for this used to make a slow-but-alive
/// remote host fail with "Timed out reconnecting to MySQL" even though it
/// would have connected fine given a little more time. 30 seconds gives that
/// legitimate case enough room while still failing within a bounded time
/// against a truly unreachable host.
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounds how long a caller waits for the NEXT row (or end-of-stream) once a
/// query is already streaming results. `CONNECTION_HEALTH_TIMEOUT` only
/// bounds a trivial `SELECT 1` on an otherwise-idle connection -- it never
/// runs concurrently with the caller's own query, since both are serialized
/// behind `op_lock` -- so it cannot catch a connection that goes silently
/// dead *while rows are still arriving*. Without a bound here,
/// `stream.try_next().await` blocked forever on exactly that stall, and
/// since `op_lock` is held for the whole call, every later call on this
/// provider -- including the next `ensure_live_pool`'s own reconnect attempt
/// -- then hung forever too, just trying to acquire that same permit.
/// Reproduced live against a real remote host with occasional network
/// flakiness.
///
/// Applied per-row rather than to the whole query, so a legitimately
/// large/slow result (`execute_query_streaming` has no row-count cap, for
/// "execute to file" exports) is never penalized as long as rows keep
/// arriving -- only a genuine stall trips it.
const ROW_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

fn mysql_ssl_mode(mode: SslMode) -> MySqlSslMode {
    match mode {
        SslMode::Disabled => MySqlSslMode::Disabled,
        SslMode::Require => MySqlSslMode::Required,
        SslMode::VerifyCa => MySqlSslMode::VerifyCa,
        SslMode::VerifyFull => MySqlSslMode::VerifyIdentity,
    }
}

pub(crate) fn mysql_connect_options(config: &ConnectionConfig) -> MySqlConnectOptions {
    let mut opts = MySqlConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .username(&config.username)
        .password(&config.password)
        .database(config.database.as_deref().unwrap_or(""))
        .ssl_mode(mysql_ssl_mode(config.ssl_mode));
    if let Some(ca_path) = &config.ssl_ca_path {
        opts = opts.ssl_ca(ca_path);
    }
    if let Some(cert_path) = &config.ssl_client_cert_path {
        opts = opts.ssl_client_cert(cert_path);
    }
    if let Some(key_path) = &config.ssl_client_key_path {
        opts = opts.ssl_client_key(key_path);
    }
    opts
}

impl MySqlProvider {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self> {
        let opts = mysql_connect_options(config);
        // Single connection: `execute_query` relies on `USE` staying applied
        // for the query that follows it, which only holds when both run on the
        // same physical connection. The metadata queries are fully qualified,
        // so serializing them through one connection is acceptable for a
        // single-user GUI client.
        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(opts.clone())
            .await
            .context("Failed to connect to MySQL")?;
        Ok(Self {
            pool: RwLock::new(pool),
            connect_options: opts,
            op_lock: AsyncMutex::new(()),
            current_database: Mutex::new(None),
        })
    }

    /// Returns a cheap clone of the pool currently in use, without probing
    /// that its connection is actually alive. The metadata/schema calls use
    /// this directly (they are always small, fast, well-known statements) and
    /// skip the liveness probe to keep schema browsing snappy on remote or
    /// tunneled connections. They still self-heal: the next `execute_query`,
    /// `execute_query_streaming`, or `ping` probes the connection and, on a
    /// silent death, swaps in a fresh pool via `reconnect`, after which this
    /// returns that fresh pool. A metadata call that lands on a
    /// silently-dead connection is bounded two ways so it can never hang
    /// forever holding `op_lock`: a connection dead *before* acquire is
    /// bounded by sqlx's acquire timeout, and one that dies *mid-fetch* is
    /// bounded by `bounded_metadata` (`ROW_FETCH_TIMEOUT`). Either way the
    /// call errors rather than wedging the provider, and the next probe-based
    /// call reconnects.
    /// Says the connection no longer has any database selected. Called whenever
    /// the pool is replaced.
    fn forget_the_current_database(&self) {
        *self
            .current_database
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    fn the_current_database(&self) -> Option<String> {
        self.current_database
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Switches the connection's database, unless it is already the one asked
    /// for. Must be called while holding `op_lock`, which is what makes the
    /// remembered name true of the one physical connection.
    async fn switch_to(&self, pool: &MySqlPool, database: &str) -> Result<()> {
        if !needs_a_switch(self.the_current_database().as_deref(), database) {
            return Ok(());
        }
        // Must use the text protocol; MySQL rejects `USE` in the
        // prepared-statement protocol (error 1295). Bounded so a connection
        // that dies here cannot hang forever holding `op_lock`.
        let use_stmt = format!("USE `{}`", database.replace('`', "``"));
        tokio::time::timeout(
            ROW_FETCH_TIMEOUT,
            sqlx::raw_sql(AssertSqlSafe(use_stmt.as_str())).execute(pool),
        )
        .await
        .context("Timed out switching database -- the connection stalled")?
        .context("Failed to switch database")?;
        *self
            .current_database
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(database.to_string());
        Ok(())
    }

    /// One attempt at the caller's SQL on the connection given, with the
    /// database switched first when it is not already the current one.
    async fn run_the_query(
        &self,
        pool: &MySqlPool,
        database: &str,
        sql: &str,
        pool_wait_ms: u64,
    ) -> Result<QueryResult> {
        self.switch_to(pool, database).await?;

        let start = Instant::now();
        let trimmed_upper = sql.trim().to_uppercase();
        let is_read_query = trimmed_upper.starts_with("SELECT")
            || trimmed_upper.starts_with("SHOW")
            || trimmed_upper.starts_with("DESCRIBE")
            || trimmed_upper.starts_with("EXPLAIN")
            || trimmed_upper.starts_with("DESC")
            || trimmed_upper.starts_with("WITH");
        // Stored-program DDL (procedures, functions, triggers, events) is
        // rejected by MySQL's prepared/binary protocol with error 1295 --
        // per MySQL's own list of statements permitted as prepared
        // statements, CREATE/ALTER/DROP PROCEDURE/FUNCTION/TRIGGER/EVENT are
        // simply absent from it. These must go through the text protocol,
        // the same way the `USE` statement above does.
        let requires_text_protocol =
            ["PROCEDURE", "FUNCTION", "TRIGGER", "EVENT"]
                .iter()
                .any(|keyword| {
                    trimmed_upper.starts_with(&format!("CREATE {keyword}"))
                        || trimmed_upper.starts_with(&format!("CREATE OR REPLACE {keyword}"))
                        || trimmed_upper.starts_with(&format!("ALTER {keyword}"))
                        || trimmed_upper.starts_with(&format!("DROP {keyword}"))
                        || trimmed_upper.starts_with(&format!("DROP TEMPORARY {keyword}"))
                });
        let prefixed = format!(
            "{}{}",
            crate::application_name_comment(crate::DEFAULT_APPLICATION_NAME),
            sql
        );

        if is_read_query {
            // Stream rows instead of buffering the whole result. Each row is
            // decoded and its cells capped before the next row is read, so a huge
            // result (many rows or multi-megabyte BLOB cells) cannot be pulled
            // into memory all at once and freeze the client.
            let mut stream = sqlx::raw_sql(AssertSqlSafe(prefixed.as_str())).fetch(pool);
            let mut columns: Vec<String> = Vec::new();
            let mut result_rows: Vec<Vec<Option<String>>> = Vec::new();
            // Set once the first row (or end-of-stream) arrives, splitting
            // `execute_ms` (submit the query, wait for the server to start
            // answering) from `streaming_ms` (pull and decode the rest).
            let mut execute_ms: Option<u64> = None;
            let mut first_row_at: Option<Instant> = None;

            loop {
                // Bound each row fetch so a connection that goes silently dead
                // mid-result cannot block forever while `op_lock` is held; the
                // guard then drops on return and the next query can reconnect.
                let row = match tokio::time::timeout(ROW_FETCH_TIMEOUT, stream.try_next()).await {
                    Ok(Ok(Some(row))) => row,
                    Ok(Ok(None)) => {
                        if execute_ms.is_none() {
                            execute_ms = Some(start.elapsed().as_millis() as u64);
                        }
                        break;
                    }
                    Ok(Err(error)) => return Err(error).context("Query execution failed"),
                    Err(_elapsed) => anyhow::bail!(
                        "Query results stopped streaming after {ROW_FETCH_TIMEOUT:?} — \
                         the database connection stalled mid-result"
                    ),
                };
                if execute_ms.is_none() {
                    execute_ms = Some(start.elapsed().as_millis() as u64);
                    first_row_at = Some(Instant::now());
                }
                if columns.is_empty() {
                    columns = row
                        .columns()
                        .iter()
                        .map(|column| column.name().to_string())
                        .collect();
                }

                let decoded: Vec<Option<String>> = (0..columns.len())
                    .map(|index| cell_to_string(&row, index))
                    .collect();
                result_rows.push(decoded);

                if result_rows.len() >= MAX_RESULT_ROWS {
                    break;
                }
            }

            let execution_time_ms = start.elapsed().as_millis() as u64;
            let rows_affected = result_rows.len() as u64;
            let streaming_ms = first_row_at.map(|instant| instant.elapsed().as_millis() as u64);
            Ok(QueryResult {
                raw_documents: None,
                columns,
                rows: result_rows,
                rows_affected,
                execution_time_ms,
                timing: Some(QueryTiming {
                    pool_wait_ms,
                    execute_ms: execute_ms.unwrap_or(execution_time_ms),
                    streaming_ms,
                }),
            })
        } else {
            let result = if requires_text_protocol {
                sqlx::raw_sql(AssertSqlSafe(prefixed.as_str()))
                    .execute(pool)
                    .await
                    .context("Query execution failed")?
            } else {
                sqlx::query(AssertSqlSafe(prefixed.as_str()))
                    .execute(pool)
                    .await
                    .context("Query execution failed")?
            };

            let execution_time_ms = start.elapsed().as_millis() as u64;
            Ok(QueryResult {
                raw_documents: None,
                columns: vec![],
                rows: vec![],
                rows_affected: result.rows_affected(),
                execution_time_ms,
                timing: Some(QueryTiming {
                    pool_wait_ms,
                    execute_ms: execution_time_ms,
                    streaming_ms: None,
                }),
            })
        }
    }

    fn current_pool(&self) -> MySqlPool {
        self.pool
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Bounds a small, well-known metadata/catalog query by `ROW_FETCH_TIMEOUT`
    /// so a connection that silently dies mid-fetch cannot hang forever while
    /// `op_lock` is held. These statements are always fast, so the bound never
    /// fires on a healthy connection -- it only turns a wedged connection into
    /// a prompt error instead of a permanent hang. Not used for arbitrary
    /// user SQL or data-affecting DDL, which can legitimately run long and
    /// must not be cut off client-side while still executing server-side.
    async fn bounded_metadata<T>(
        future: impl std::future::Future<Output = sqlx::Result<T>>,
        failure_context: &'static str,
    ) -> Result<T> {
        tokio::time::timeout(ROW_FETCH_TIMEOUT, future)
            .await
            .with_context(|| format!("{failure_context} -- the connection stalled"))?
            .context(failure_context)
    }

    /// Opens a brand-new pool against the original connection options and
    /// swaps it in for future calls.
    ///
    /// The stale pool is simply dropped, never closed: `Pool::close()` waits
    /// for every outstanding connection permit to be released, which a
    /// wedged pool (see `CONNECTION_HEALTH_TIMEOUT`) will never do. Dropping
    /// it leaks the one stuck background task/socket for the life of the
    /// process, which is an acceptable trade against hanging every future
    /// query on this connection forever.
    async fn reconnect(&self) -> Result<MySqlPool> {
        let fresh = tokio::time::timeout(
            RECONNECT_TIMEOUT,
            MySqlPoolOptions::new()
                .max_connections(1)
                .connect_with(self.connect_options.clone()),
        )
        .await
        .context("Timed out reconnecting to MySQL")?
        .context("Failed to reconnect to MySQL")?;
        self.forget_the_current_database();
        *self
            .pool
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = fresh.clone();
        Ok(fresh)
    }

    /// Confirms the current pool's connection is actually responsive within
    /// `CONNECTION_HEALTH_TIMEOUT`, transparently reconnecting (bounded by
    /// the separate, more generous `RECONNECT_TIMEOUT`) if not. Used by
    /// callers -- `ping`, `execute_query`, `execute_query_streaming` --
    /// where hanging forever on a silently dead connection is exactly the
    /// bug being guarded against; the probe itself never runs the caller's
    /// own (possibly long-running) SQL, so a legitimately slow query is
    /// never mistaken for a dead connection.
    ///
    /// Must be called while holding `op_lock`. That guarantees no other query
    /// is using the single connection, so the probe reflects the connection's
    /// real liveness rather than losing a race for the permit against a
    /// concurrent long query (which would otherwise cause a spurious
    /// reconnect and briefly break the single-connection invariant).
    async fn ensure_live_pool(&self) -> Result<MySqlPool> {
        let pool = self.current_pool();
        let probe = tokio::time::timeout(
            CONNECTION_HEALTH_TIMEOUT,
            sqlx::query("SELECT 1").execute(&pool),
        )
        .await;
        match probe {
            Ok(Ok(_)) => Ok(pool),
            _ => self.reconnect().await,
        }
    }

    /// Reads the grant statements for a single account. SHOW GRANTS does not
    /// accept bind parameters, so the account is quoted inline; single quotes
    /// are escaped to keep the statement well-formed.
    async fn show_grants(&self, user: &str, host: &str) -> Result<Vec<String>> {
        let sql = format!(
            "SHOW GRANTS FOR '{}'@'{}'",
            user.replace('\'', "''"),
            host.replace('\'', "''"),
        );
        let pool = self.current_pool();
        let query = sqlx::query(AssertSqlSafe(sql.as_str())).fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to read grants").await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                row.try_get::<Vec<u8>, _>(0)
                    .ok()
                    .map(bytes_to_string)
                    .or_else(|| row.try_get::<String, _>(0).ok())
            })
            .collect())
    }
}

/// Renders one cell as the text a reader sees, which is the text the `mysql`
/// client shows for the same value.
///
/// Rows arrive over the text protocol -- the one that client uses -- so the
/// server has already rendered every value: a `decimal` keeps the digits it was
/// declared with, a `double` comes as `1e30`, a moment comes as
/// `2026-08-02 16:28:50`. Read as text, that is exactly right, and it needs no
/// per-type formatting here to go wrong. It is read *unchecked* because sqlx
/// otherwise refuses the string on the column's declared type -- deliberately,
/// since decoding a `decimal` into a float would lose digits -- and every one of
/// those refusals used to leave the cell reading as absent.
fn cell_to_string(row: &sqlx::mysql::MySqlRow, index: usize) -> Option<String> {
    // A column of bytes is not a column of characters, and the client prints its
    // bytes as hex: read as text, an md5 comes out as mojibake.
    if holds_bytes(column_type_name(row, index)) {
        return cell_bytes(row, index).map(|bytes| written_hex(&bytes));
    }

    row.try_get_unchecked::<Option<String>, _>(index)
        .ok()
        .flatten()
        .or_else(|| cell_bytes(row, index).map(|bytes| written_bytes_or_text(&bytes)))
}

fn cell_bytes(row: &sqlx::mysql::MySqlRow, index: usize) -> Option<Vec<u8>> {
    row.try_get_unchecked::<Option<Vec<u8>>, _>(index)
        .ok()
        .flatten()
}

/// The columns that certainly hold bytes rather than characters: a fixed-width
/// one holds a hash or an id, and a bit string is bits.
///
/// The client decides this by the value's character set -- hex when that set is
/// the binary one -- but sqlx keeps the set to itself and names the type from the
/// column's binary *flag*, which MySQL also sets for a perfectly ordinary text
/// column declared with a `_bin` collation. Going by the name alone therefore
/// turns a name like `--crypto` in a `varchar(512) collate utf8mb4_bin` into hex,
/// so only the fixed-width kinds are decided here and the rest are decided by
/// what their bytes turn out to be.
fn holds_bytes(type_name: &str) -> bool {
    matches!(type_name, "BINARY" | "BIT")
}

/// Bytes from a column that may hold either. Text reads as text, which is what
/// the client does for every character column whatever its collation; anything
/// that is not text reads as hex, which is what it does for bytes.
///
/// The one place this parts company with the client: a variable-width binary
/// column holding text -- a `varbinary` with a word in it -- reads as that word
/// here and as hex there. Unreadable names in the grid are the worse error.
fn written_bytes_or_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => written_hex(bytes),
    }
}

/// Hex in the form the client uses, `0x` and upper case, so a value can be
/// compared with what that client prints and pasted back into a query.
fn written_hex(bytes: &[u8]) -> String {
    let digits: String = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
    format!("0x{digits}")
}

fn column_type_name(row: &sqlx::mysql::MySqlRow, index: usize) -> &str {
    row.columns()
        .get(index)
        .map_or("", |column| column.type_info().name())
}

// MySQL reports string columns from SHOW/information_schema with a binary
// collation on some servers, so sqlx types them as VARBINARY and a direct
// String decode fails. Reading the raw bytes works for both VARCHAR and
// VARBINARY; convert lossily so non-UTF-8 bytes never abort a query.
fn bytes_to_string(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod connect_options_tests {
    use super::{mysql_connect_options, needs_a_switch, the_connection_died};
    use crate::connection::{ConnectionConfig, SslMode};
    use sqlx::mysql::MySqlSslMode;

    fn base_config() -> ConnectionConfig {
        ConnectionConfig {
            driver: crate::connection::DatabaseDriver::MySQL,
            host: "db.example.com".to_string(),
            port: 3306,
            username: "root".to_string(),
            password: "secret".to_string(),
            ..Default::default()
        }
    }

    /// The switch to a database is a round trip, and on a distant server it
    /// costs as much as the query it precedes. It is worth doing once.
    #[test]
    fn a_database_already_current_is_not_switched_to_again() {
        assert!(needs_a_switch(None, "shop"), "nothing selected yet");
        assert!(needs_a_switch(Some("other"), "shop"), "a different one");
        assert!(
            !needs_a_switch(Some("shop"), "shop"),
            "the one already current must not be switched to again"
        );
        assert!(
            !needs_a_switch(None, ""),
            "a caller that named no database is asking for no switch"
        );
        assert!(
            !needs_a_switch(Some("shop"), ""),
            "and still is when one is current"
        );
    }

    /// Only a connection that died is worth sending a statement to twice. One
    /// the server rejected would be rejected again, and one that may have
    /// half-applied must never be repeated.
    #[test]
    fn only_a_dead_connection_is_worth_a_second_attempt() {
        let died = anyhow::Error::new(sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "gone",
        )));
        assert!(the_connection_died(&died));
        assert!(the_connection_died(&anyhow::Error::new(
            sqlx::Error::PoolClosed
        )));
        assert!(the_connection_died(&anyhow::Error::new(
            sqlx::Error::PoolTimedOut
        )));
        // A plain message carries no failure to read, so nothing is repeated.
        assert!(!the_connection_died(&anyhow::anyhow!(
            "syntax error near ')'"
        )));
        assert!(!the_connection_died(&anyhow::Error::new(
            sqlx::Error::RowNotFound
        )));
        // The context the query path adds must not hide the failure underneath.
        assert!(the_connection_died(
            &anyhow::Error::new(sqlx::Error::PoolClosed).context("Failed to run the query")
        ));
    }

    #[test]
    fn ssl_mode_disabled_by_default() {
        let opts = mysql_connect_options(&base_config());
        assert!(matches!(opts.get_ssl_mode(), MySqlSslMode::Disabled));
    }

    #[test]
    fn ssl_mode_require_maps_to_required() {
        let mut config = base_config();
        config.ssl_mode = SslMode::Require;
        let opts = mysql_connect_options(&config);
        assert!(matches!(opts.get_ssl_mode(), MySqlSslMode::Required));
    }

    #[test]
    fn ssl_mode_verify_ca_maps_to_verify_ca() {
        let mut config = base_config();
        config.ssl_mode = SslMode::VerifyCa;
        config.ssl_ca_path = Some("/tmp/ca.pem".to_string());
        let opts = mysql_connect_options(&config);
        assert!(matches!(opts.get_ssl_mode(), MySqlSslMode::VerifyCa));
    }

    #[test]
    fn ssl_mode_verify_full_maps_to_verify_identity() {
        let mut config = base_config();
        config.ssl_mode = SslMode::VerifyFull;
        let opts = mysql_connect_options(&config);
        assert!(matches!(opts.get_ssl_mode(), MySqlSslMode::VerifyIdentity));
    }
}

/// Whether a query for `wanted` has to switch the connection's database first.
///
/// The switch is a round trip, and on a distant server it costs as much as the
/// query after it, so it is worth not repeating for a database that is already
/// current. An empty name means the caller did not ask for one.
fn needs_a_switch(current: Option<&str>, wanted: &str) -> bool {
    !wanted.is_empty() && current != Some(wanted)
}

/// Whether this failure means the connection died rather than the statement
/// being wrong.
///
/// Only these are worth running again: a statement the server rejected will be
/// rejected the same way a second time, and one that may have half-applied must
/// never be sent twice. A connection that died carried nothing to the server.
fn the_connection_died(error: &anyhow::Error) -> bool {
    let Some(sqlx_error) = error.downcast_ref::<sqlx::Error>() else {
        return false;
    };
    match sqlx_error {
        sqlx::Error::Io(_) | sqlx::Error::PoolClosed | sqlx::Error::PoolTimedOut => true,
        sqlx::Error::Tls(_) | sqlx::Error::Protocol(_) => true,
        sqlx::Error::Database(database) => matches!(
            database.code().as_deref(),
            // Server gone away, connection lost during the query, the server
            // shutting down, and the client being killed.
            Some("2006" | "2013" | "1053" | "1317")
        ),
        _ => false,
    }
}

#[async_trait]
impl DbProvider for MySqlProvider {
    async fn ping(&self) -> Result<()> {
        let _guard = self.op_lock.lock().await;
        self.ensure_live_pool().await?;
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let query = sqlx::query_as::<_, (Vec<u8>,)>("SHOW DATABASES").fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to list databases").await?;
        Ok(rows
            .into_iter()
            .map(|(name,)| DatabaseInfo {
                name: bytes_to_string(name),
            })
            .collect())
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let query = sqlx::query_as::<_, (Vec<u8>, Vec<u8>)>(
            "SELECT TABLE_NAME, TABLE_TYPE \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? \
             ORDER BY TABLE_NAME",
        )
        .bind(database)
        .fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to list tables").await?;

        Ok(rows
            .into_iter()
            .map(|(name, table_type)| TableInfo {
                name: bytes_to_string(name),
                kind: if bytes_to_string(table_type) == "VIEW" {
                    TableKind::View
                } else {
                    TableKind::Table
                },
            })
            .collect())
    }

    async fn list_views(&self, database: &str) -> Result<Vec<String>> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        // SHOW FULL TABLES IN <db> does not support bind params.
        let escaped = database.replace('`', "``");
        let sql = format!(
            "-- name: ListViews :many\n\
             SHOW FULL TABLES IN `{escaped}` WHERE Table_type = 'VIEW'"
        );
        let query = sqlx::query(AssertSqlSafe(sql.as_str())).fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to list views").await?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                row.try_get::<Vec<u8>, _>(0)
                    .ok()
                    .map(bytes_to_string)
                    .or_else(|| row.try_get::<String, _>(0).ok())
            })
            .collect())
    }

    async fn list_foreign_keys(&self, database: &str, table: &str) -> Result<Vec<FkInfo>> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let query = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)>(
            "-- name: ListForeignKeys :many
             SELECT CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME
             FROM information_schema.KEY_COLUMN_USAGE
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND REFERENCED_TABLE_NAME IS NOT NULL
             ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION",
        )
        .bind(database)
        .bind(table)
        .fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to list foreign keys").await?;

        Ok(rows
            .into_iter()
            .map(|(name, from_col, to_table, to_col)| FkInfo {
                name: bytes_to_string(name),
                from_column: bytes_to_string(from_col),
                to_table: bytes_to_string(to_table),
                to_column: bytes_to_string(to_col),
            })
            .collect())
    }

    async fn list_check_constraints(
        &self,
        database: &str,
        table: &str,
    ) -> Result<Vec<CheckConstraintInfo>> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let query = sqlx::query_as::<_, (Vec<u8>, Vec<u8>)>(
            "-- name: ListCheckConstraints :many
             SELECT cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE
             FROM information_schema.CHECK_CONSTRAINTS cc
             JOIN information_schema.TABLE_CONSTRAINTS tc
               ON tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA
              AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME
             WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'CHECK'
             ORDER BY cc.CONSTRAINT_NAME",
        )
        .bind(database)
        .bind(table)
        .fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to list check constraints").await?;

        Ok(rows
            .into_iter()
            .map(|(name, expression)| CheckConstraintInfo {
                name: bytes_to_string(name),
                expression: bytes_to_string(expression),
            })
            .collect())
    }

    async fn describe_table(&self, database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let query =
            sqlx::query_as::<_, (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>, Vec<u8>)>(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, COLUMN_DEFAULT, EXTRA \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             ORDER BY ORDINAL_POSITION",
            )
            .bind(database)
            .bind(table)
            .fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to describe table").await?;

        Ok(rows
            .into_iter()
            .map(|(name, data_type, nullable, key, default_value, extra)| {
                let key = bytes_to_string(key);
                ColumnInfo {
                    name: bytes_to_string(name),
                    data_type: bytes_to_string(data_type),
                    is_nullable: bytes_to_string(nullable) == "YES",
                    column_key: if key.is_empty() { None } else { Some(key) },
                    default_value: default_value.map(bytes_to_string),
                    extra: bytes_to_string(extra),
                }
            })
            .collect())
    }

    /// Every table's columns in one exchange. MySQL keeps them all in one
    /// table of its own, so one query answers for a whole schema -- where asking
    /// table by table is a round trip each, and a schema has hundreds of them.
    async fn describe_database(
        &self,
        database: &str,
        _tables: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<ColumnInfo>>> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let query = sqlx::query_as::<
            _,
            (
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                Option<Vec<u8>>,
                Vec<u8>,
            ),
        >(
            "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, \
                    COLUMN_DEFAULT, EXTRA \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? \
             ORDER BY TABLE_NAME, ORDINAL_POSITION",
        )
        .bind(database)
        .fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to describe the schema").await?;

        let mut columns: std::collections::HashMap<String, Vec<ColumnInfo>> =
            std::collections::HashMap::new();
        for (table, name, data_type, nullable, key, default_value, extra) in rows {
            let key = bytes_to_string(key);
            columns
                .entry(bytes_to_string(table))
                .or_default()
                .push(ColumnInfo {
                    name: bytes_to_string(name),
                    data_type: bytes_to_string(data_type),
                    is_nullable: bytes_to_string(nullable) == "YES",
                    column_key: if key.is_empty() { None } else { Some(key) },
                    default_value: default_value.map(bytes_to_string),
                    extra: bytes_to_string(extra),
                });
        }
        Ok(columns)
    }

    async fn get_table_ddl(&self, database: &str, table: &str) -> Result<String> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let sql = format!(
            "SHOW CREATE TABLE `{}`.`{}`",
            database.replace('`', "``"),
            table.replace('`', "``")
        );
        let query = sqlx::query(AssertSqlSafe(sql.as_str())).fetch_one(&pool);
        let row = Self::bounded_metadata(query, "Failed to get table DDL").await?;
        row.try_get::<Vec<u8>, _>(1)
            .map(bytes_to_string)
            .or_else(|_| row.try_get::<String, _>(1))
            .context("Failed to read DDL from result")
    }

    async fn get_database_ddl(&self, database: &str) -> Result<String> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let sql = format!("SHOW CREATE DATABASE `{}`", database.replace('`', "``"));
        let query = sqlx::query(AssertSqlSafe(sql.as_str())).fetch_one(&pool);
        let row = Self::bounded_metadata(query, "Failed to get database DDL").await?;
        row.try_get::<Vec<u8>, _>(1)
            .map(bytes_to_string)
            .or_else(|_| row.try_get::<String, _>(1))
            .context("Failed to read database DDL from result")
    }

    async fn execute_query(&self, database: &str, sql: &str) -> Result<QueryResult> {
        let _guard = self.op_lock.lock().await;
        // The connection is not probed first. A probe is a round trip of its
        // own, and on a distant server it costs as much as the query it guards
        // -- measured against a stage server over a corporate link, a `SELECT 1`
        // and the query after it cost 195 ms each. So the query is sent straight
        // away, and only a failure meaning the connection died is answered by
        // reconnecting and sending it once more. A statement the server rejected
        // is never sent twice, and neither is one that may have half-applied.
        let pool_wait_start = Instant::now();
        let pool = self.current_pool();
        let pool_wait_ms = pool_wait_start.elapsed().as_millis() as u64;
        match self.run_the_query(&pool, database, sql, pool_wait_ms).await {
            Err(error) if the_connection_died(&error) => {
                let fresh = self.reconnect().await?;
                self.run_the_query(&fresh, database, sql, pool_wait_ms)
                    .await
            }
            answer => answer,
        }
    }

    async fn execute_query_streaming(
        &self,
        database: &str,
        sql: &str,
        sink: &mut dyn crate::provider::RowSink,
    ) -> Result<u64> {
        let _guard = self.op_lock.lock().await;
        let pool = self.ensure_live_pool().await?;

        if !database.is_empty() {
            // Bounded for the same reason as in `execute_query`: a silent
            // connection death here must not hang forever holding `op_lock`.
            let use_stmt = format!("USE `{}`", database.replace('`', "``"));
            tokio::time::timeout(
                ROW_FETCH_TIMEOUT,
                sqlx::raw_sql(AssertSqlSafe(use_stmt.as_str())).execute(&pool),
            )
            .await
            .context("Timed out switching database -- the connection stalled")?
            .context("Failed to switch database")?;
        }

        let trimmed_upper = sql.trim().to_uppercase();
        let is_read_query = trimmed_upper.starts_with("SELECT")
            || trimmed_upper.starts_with("SHOW")
            || trimmed_upper.starts_with("DESCRIBE")
            || trimmed_upper.starts_with("EXPLAIN")
            || trimmed_upper.starts_with("DESC")
            || trimmed_upper.starts_with("WITH");
        let prefixed = format!(
            "{}{}",
            crate::application_name_comment(crate::DEFAULT_APPLICATION_NAME),
            sql
        );

        if !is_read_query {
            sqlx::query(AssertSqlSafe(prefixed.as_str()))
                .execute(&pool)
                .await
                .context("Query execution failed")?;
            return Ok(0);
        }

        // Unlike `execute_query`, this never breaks at `MAX_RESULT_ROWS` — the
        // whole point of "execute to file" is exporting result sets too large
        // for the grid. Cells are still capped for safety against a single
        // multi-megabyte BLOB, but the row count itself is unbounded.
        let mut stream = sqlx::raw_sql(AssertSqlSafe(prefixed.as_str())).fetch(&pool);
        let mut columns: Vec<String> = Vec::new();
        let mut row_count: u64 = 0;

        loop {
            // Bound each row fetch so a connection that goes silently dead
            // mid-result cannot block forever while `op_lock` is held; the
            // guard then drops on return and the next query can reconnect.
            let row = match tokio::time::timeout(ROW_FETCH_TIMEOUT, stream.try_next()).await {
                Ok(Ok(Some(row))) => row,
                Ok(Ok(None)) => break,
                Ok(Err(error)) => return Err(error).context("Query execution failed"),
                Err(_elapsed) => anyhow::bail!(
                    "Query results stopped streaming after {ROW_FETCH_TIMEOUT:?} — \
                     the database connection stalled mid-result"
                ),
            };
            if columns.is_empty() {
                columns = row
                    .columns()
                    .iter()
                    .map(|column| column.name().to_string())
                    .collect();
                sink.write_columns(&columns)?;
            }

            let decoded: Vec<Option<String>> = (0..columns.len())
                .map(|index| cell_to_string(&row, index))
                .collect();
            sink.write_row(&decoded)?;
            row_count += 1;
        }

        if columns.is_empty() {
            sink.write_columns(&[])?;
        }
        Ok(row_count)
    }

    async fn list_indexes(&self, database: &str, table: &str) -> Result<Vec<IndexInfo>> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let query = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, i64, Vec<u8>)>(
            "-- name: ListIndexes :many
             SELECT INDEX_NAME,
                    GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX SEPARATOR ','),
                    MAX(CAST(NON_UNIQUE = 0 AS SIGNED)),
                    MAX(INDEX_TYPE)
             FROM information_schema.STATISTICS
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
             GROUP BY INDEX_NAME",
        )
        .bind(database)
        .bind(table)
        .fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to list indexes").await?;

        Ok(rows
            .into_iter()
            .map(|(name, cols_concat, unique_flag, index_type)| IndexInfo {
                name: bytes_to_string(name),
                columns: bytes_to_string(cols_concat)
                    .split(',')
                    .map(|s| s.to_string())
                    .collect(),
                unique: unique_flag == 1,
                index_type: bytes_to_string(index_type),
            })
            .collect())
    }

    async fn list_procedures(&self, database: &str) -> Result<Vec<ProcedureInfo>> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let query = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, Option<Vec<u8>>)>(
            "-- name: ListProcedures :many
             SELECT ROUTINE_NAME, ROUTINE_TYPE, ROUTINE_DEFINITION
             FROM information_schema.ROUTINES
             WHERE ROUTINE_SCHEMA = ?
             ORDER BY ROUTINE_TYPE, ROUTINE_NAME",
        )
        .bind(database)
        .fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to list procedures").await?;

        Ok(rows
            .into_iter()
            .map(|(name, routine_type, definition)| ProcedureInfo {
                name: bytes_to_string(name),
                kind: if bytes_to_string(routine_type) == "FUNCTION" {
                    ProcedureKind::Function
                } else {
                    ProcedureKind::Procedure
                },
                definition: definition.map(bytes_to_string),
            })
            .collect())
    }

    async fn list_triggers(&self, database: &str, table: &str) -> Result<Vec<TriggerInfo>> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let query = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)>(
            "-- name: ListTriggers :many
             SELECT TRIGGER_NAME, EVENT_MANIPULATION, ACTION_TIMING, EVENT_OBJECT_TABLE, ACTION_STATEMENT
             FROM information_schema.TRIGGERS
             WHERE TRIGGER_SCHEMA = ? AND EVENT_OBJECT_TABLE = ?
             ORDER BY TRIGGER_NAME",
        )
        .bind(database)
        .bind(table)
        .fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to list triggers").await?;

        Ok(rows
            .into_iter()
            .map(
                |(name, event, timing, table_name, definition)| TriggerInfo {
                    name: bytes_to_string(name),
                    event: bytes_to_string(event),
                    timing: bytes_to_string(timing),
                    table_name: bytes_to_string(table_name),
                    definition: Some(bytes_to_string(definition)),
                },
            )
            .collect())
    }

    async fn list_events(&self, database: &str) -> Result<Vec<EventInfo>> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let query = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, Option<Vec<u8>>)>(
            "-- name: ListEvents :many
             SELECT EVENT_NAME, STATUS, EVENT_DEFINITION
             FROM information_schema.EVENTS
             WHERE EVENT_SCHEMA = ?
             ORDER BY EVENT_NAME",
        )
        .bind(database)
        .fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to list events").await?;

        Ok(rows
            .into_iter()
            .map(|(name, status, definition)| EventInfo {
                name: bytes_to_string(name),
                status: Some(bytes_to_string(status)),
                definition: definition.map(bytes_to_string),
            })
            .collect())
    }

    async fn list_users(&self) -> Result<Vec<UserInfo>> {
        // `show_grants` deliberately does not take `op_lock`; it is a private
        // helper invoked only from here, already under this guard.
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let query = sqlx::query_as::<_, (Vec<u8>, Vec<u8>)>(
            "-- name: ListUsers :many
             SELECT User, Host FROM mysql.user ORDER BY User, Host",
        )
        .fetch_all(&pool);
        let rows = Self::bounded_metadata(query, "Failed to list users").await?;

        let mut users = Vec::with_capacity(rows.len());
        for (name, host) in rows {
            let name = bytes_to_string(name);
            let host = bytes_to_string(host);
            let grants = self.show_grants(&name, &host).await.unwrap_or_default();
            users.push(UserInfo { name, host, grants });
        }
        Ok(users)
    }

    async fn truncate_table(&self, database: &str, table: &str) -> Result<()> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let sql = format!(
            "TRUNCATE TABLE `{}`.`{}`",
            database.replace('`', "``"),
            table.replace('`', "``")
        );
        sqlx::query(AssertSqlSafe(sql.as_str()))
            .execute(&pool)
            .await
            .context("Failed to truncate table")?;
        Ok(())
    }

    async fn drop_table(&self, database: &str, table: &str) -> Result<()> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        let sql = format!(
            "DROP TABLE `{}`.`{}`",
            database.replace('`', "``"),
            table.replace('`', "``")
        );
        sqlx::query(AssertSqlSafe(sql.as_str()))
            .execute(&pool)
            .await
            .context("Failed to drop table")?;
        Ok(())
    }

    async fn rename_table(&self, database: &str, old_name: &str, new_name: &str) -> Result<()> {
        let _guard = self.op_lock.lock().await;
        let pool = self.current_pool();
        sqlx::query(AssertSqlSafe(rename_table_sql(
            database, old_name, new_name,
        )))
        .execute(&pool)
        .await
        .context("Failed to rename table")?;
        Ok(())
    }
}

fn rename_table_sql(database: &str, old_name: &str, new_name: &str) -> String {
    format!(
        "RENAME TABLE `{}`.`{}` TO `{}`.`{}`",
        database.replace('`', "``"),
        old_name.replace('`', "``"),
        database.replace('`', "``"),
        new_name.replace('`', "``")
    )
}

#[cfg(test)]
mod cell_tests {
    use super::{holds_bytes, written_bytes_or_text, written_hex};

    #[test]
    fn a_hash_is_written_the_way_the_client_writes_it() {
        let md5 = [
            0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8,
            0x42, 0x7e,
        ];
        assert_eq!(written_hex(&md5), "0xD41D8CD98F00B204E9800998ECF8427E");
    }

    #[test]
    fn bytes_that_read_as_text_are_still_written_as_hex() {
        assert_eq!(
            written_hex(b"utf8mb4_unicode_ci"),
            "0x757466386D62345F756E69636F64655F6369"
        );
    }

    /// MySQL sets a column's binary flag for a `_bin` collation as well as for
    /// real bytes, and sqlx names the type from that flag, so the name alone
    /// cannot say whether a value is characters. Only the fixed-width kinds may
    /// be decided by it.
    #[test]
    fn only_the_fixed_width_kinds_are_decided_by_the_type_name() {
        for bytes in ["BINARY", "BIT"] {
            assert!(holds_bytes(bytes), "{bytes} holds bytes whatever is in it");
        }
        for maybe in [
            "VARBINARY",
            "BLOB",
            "TINYBLOB",
            "LONGBLOB",
            "VARCHAR",
            "CHAR",
            "TEXT",
            "ENUM",
            "JSON",
        ] {
            assert!(
                !holds_bytes(maybe),
                "{maybe} has to be decided by its own bytes"
            );
        }
    }

    /// The case that broke: `varchar(512) character set utf8mb4 collate
    /// utf8mb4_bin` is a column of names, and MySQL's own client prints them as
    /// names. sqlx calls its type `VARBINARY`, which is not enough to go on.
    #[test]
    fn a_name_in_a_column_with_a_binary_collation_stays_a_name() {
        assert_eq!(written_bytes_or_text(b"--crypto"), "--crypto");
        assert_eq!(
            written_bytes_or_text(b"1000000_exchange"),
            "1000000_exchange"
        );
    }

    #[test]
    fn bytes_that_are_not_text_are_still_written_as_hex() {
        assert_eq!(written_bytes_or_text(&[0xff, 0xfe, 0x00]), "0xFFFE00");
    }
}

#[cfg(test)]
mod rename_table_tests {
    use super::*;

    #[test]
    fn rename_table_sql_qualifies_both_sides_with_the_database() {
        assert_eq!(
            rename_table_sql("shop", "users", "customers"),
            "RENAME TABLE `shop`.`users` TO `shop`.`customers`"
        );
    }

    #[test]
    fn rename_table_sql_escapes_embedded_backticks() {
        assert_eq!(
            rename_table_sql("sh`op", "us`ers", "cust`omers"),
            "RENAME TABLE `sh``op`.`us``ers` TO `sh``op`.`cust``omers`"
        );
    }
}

/// Integration tests against a real MySQL server.
///
/// Set MYSQL_TEST_URL=mysql://user:password@host:port/dbname before running,
/// then use `cargo test -p db_client -- --include-ignored` to execute.
#[cfg(test)]
mod integration_tests {
    use super::{MySqlProvider, ROW_FETCH_TIMEOUT};
    use crate::provider::DbProvider;
    use crate::schema::ProcedureKind;
    use crate::{ConnectionConfig, DatabaseDriver};
    use uuid::Uuid;

    fn test_config_from_env() -> Option<ConnectionConfig> {
        let url = std::env::var("MYSQL_TEST_URL").ok()?;
        // Parse mysql://user:password@host:port/database
        let url = url.strip_prefix("mysql://")?;
        let (userinfo, hostpart) = url.split_once('@')?;
        let (username, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let (hostport, database) = hostpart.split_once('/').unwrap_or((hostpart, ""));
        let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, "3306"));
        let port: u16 = port_str.parse().unwrap_or(3306);

        Some(ConnectionConfig {
            id: Uuid::new_v4(),
            label: "test".to_string(),
            driver: DatabaseDriver::MySQL,
            host: host.to_string(),
            port,
            username: username.to_string(),
            password: password.to_string(),
            database: if database.is_empty() {
                None
            } else {
                Some(database.to_string())
            },
            auto_connect: false,
            ..ConnectionConfig::default()
        })
    }

    /// An unsigned column is a number, and has to read as one. sqlx refuses an
    /// `i64` decode for an unsigned column but accepts a `bool` decode for every
    /// integer width, so any `bool` attempt before the unsigned integers turns
    /// counters and ids into `true`/`false`.
    #[tokio::test]
    #[ignore]
    async fn test_unsigned_columns_read_as_numbers() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        provider
            .execute_query("", "DROP TABLE IF EXISTS zed_unsigned_probe")
            .await
            .expect("failed to drop the probe table");
        provider
            .execute_query(
                "",
                "CREATE TABLE zed_unsigned_probe (\
                   ci smallint unsigned NOT NULL AUTO_INCREMENT PRIMARY KEY,\
                   tiny tinyint unsigned NOT NULL,\
                   big bigint unsigned NOT NULL,\
                   signed_small smallint NOT NULL,\
                   flagish tinyint(1) NOT NULL\
                 )",
            )
            .await
            .expect("failed to create the probe table");
        provider
            .execute_query(
                "",
                "INSERT INTO zed_unsigned_probe (ci, tiny, big, signed_small, flagish) \
                 VALUES (7, 1, 18446744073709551615, -3, 1), (8, 0, 0, 0, 0)",
            )
            .await
            .expect("failed to seed the probe table");

        let result = provider
            .execute_query(
                "",
                "SELECT ci, tiny, big, signed_small, flagish FROM zed_unsigned_probe ORDER BY ci",
            )
            .await
            .expect("failed to read the probe table");

        provider
            .execute_query("", "DROP TABLE zed_unsigned_probe")
            .await
            .expect("failed to drop the probe table");

        let rendered: Vec<Vec<String>> = result
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.clone().unwrap_or_else(|| "NULL".to_string()))
                    .collect()
            })
            .collect();

        assert_eq!(
            rendered,
            vec![
                vec![
                    "7".to_string(),
                    "1".to_string(),
                    "18446744073709551615".to_string(),
                    "-3".to_string(),
                    "1".to_string(),
                ],
                vec![
                    "8".to_string(),
                    "0".to_string(),
                    "0".to_string(),
                    "0".to_string(),
                    "0".to_string(),
                ],
            ],
            "every number has to read as a number, flags included"
        );
    }

    /// What the grid shows has to be what the `mysql` client shows. Before this,
    /// a hash came out as mojibake, and a moment, a bit string and a decimal came
    /// out as nothing at all -- on columns that cannot even be null.
    ///
    /// The expected strings here were taken from that client, run against the
    /// same rows with `--binary-as-hex`, which is what it does on a terminal.
    #[tokio::test]
    #[ignore]
    async fn test_cells_read_the_way_the_mysql_client_prints_them() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        provider
            .execute_query("", "DROP TABLE IF EXISTS zed_moment_probe")
            .await
            .expect("failed to drop the probe table");
        provider
            .execute_query(
                "",
                "CREATE TABLE zed_moment_probe (\
                   id int unsigned NOT NULL PRIMARY KEY,\
                   md5 binary(16) DEFAULT NULL,\
                   blobbed varbinary(32) DEFAULT NULL,\
                   named varchar(64) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin DEFAULT NULL,\
                   a_bit bit(8) DEFAULT NULL,\
                   money decimal(20,6) DEFAULT NULL,\
                   made_at timestamp NOT NULL,\
                   exactly datetime(6) DEFAULT NULL,\
                   the_day date DEFAULT NULL,\
                   the_hour time DEFAULT NULL\
                 )",
            )
            .await
            .expect("failed to create the probe table");
        provider
            .execute_query(
                "",
                "INSERT INTO zed_moment_probe \
                 (id, md5, blobbed, named, a_bit, money, made_at, exactly, the_day, the_hour) \
                 VALUES \
                 (1, UNHEX('d41d8cd98f00b204e9800998ecf8427e'), 'utf8mb4_unicode_ci', '--crypto', \
                  b'10101010', 12345678901234.567890, \
                  '2026-08-02 16:28:50', '2026-08-02 16:28:50.004500', '2026-08-02', '16:28:50'), \
                 (2, NULL, NULL, NULL, NULL, NULL, '2026-08-02 16:28:51', NULL, NULL, NULL)",
            )
            .await
            .expect("failed to seed the probe table");

        let result = provider
            .execute_query(
                "",
                "SELECT id, md5, blobbed, named, a_bit, money, made_at, exactly, the_day, \
                        the_hour \
                 FROM zed_moment_probe ORDER BY id",
            )
            .await
            .expect("failed to read the probe table");

        provider
            .execute_query("", "DROP TABLE zed_moment_probe")
            .await
            .expect("failed to drop the probe table");

        let rendered: Vec<Vec<String>> = result
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.clone().unwrap_or_else(|| "NULL".to_string()))
                    .collect()
            })
            .collect();

        assert_eq!(
            rendered,
            vec![
                vec![
                    "1".to_string(),
                    "0xD41D8CD98F00B204E9800998ECF8427E".to_string(),
                    "utf8mb4_unicode_ci".to_string(),
                    "--crypto".to_string(),
                    "0xAA".to_string(),
                    "12345678901234.567890".to_string(),
                    "2026-08-02 16:28:50".to_string(),
                    "2026-08-02 16:28:50.004500".to_string(),
                    "2026-08-02".to_string(),
                    "16:28:50".to_string(),
                ],
                vec![
                    "2".to_string(),
                    "NULL".to_string(),
                    "NULL".to_string(),
                    "NULL".to_string(),
                    "NULL".to_string(),
                    "NULL".to_string(),
                    "2026-08-02 16:28:51".to_string(),
                    "NULL".to_string(),
                    "NULL".to_string(),
                    "NULL".to_string(),
                ],
            ],
            "every value reads the way the mysql client prints it, and only a \
             real absence reads as absent"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_ping() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        provider.ping().await.expect("Ping failed");
    }

    // Regression test: a cleanly killed connection (a proper TCP close, e.g.
    // `wait_timeout` expiry or an admin `KILL <connection_id>`) must self-heal
    // through sqlx's own `test_before_acquire` ping -- no reconnect logic in
    // `MySqlProvider` is even needed for this case. This is here to lock in
    // that expectation and to contrast with the *silent* death simulated in
    // `test_query_recovers_after_connection_silently_freezes`, which is the
    // case that actually needed a fix.
    #[tokio::test]
    #[ignore]
    async fn test_query_survives_a_cleanly_killed_connection() {
        use sqlx::Connection as _;

        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        // Bypass execute_query's generic cell decoder here -- decoding
        // CONNECTION_ID()'s BIGINT UNSIGNED as bool/string/etc is a separate,
        // pre-existing quirk unrelated to what this test investigates.
        let connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(&provider.current_pool())
            .await
            .expect("failed to read CONNECTION_ID()");

        let mut killer =
            sqlx::mysql::MySqlConnection::connect_with(&super::mysql_connect_options(&config))
                .await
                .expect("failed to open killer connection");
        sqlx::query(sqlx::AssertSqlSafe(format!("KILL {connection_id}")))
            .execute(&mut killer)
            .await
            .expect("KILL failed");
        killer.close().await.ok();

        // Give the server a moment to actually tear down the socket.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let after_kill = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.execute_query("", "SELECT 1 AS after_kill"),
        )
        .await
        .expect("query hung for >10s instead of self-healing through sqlx's own ping check")
        .expect("query after a cleanly killed connection should self-heal and succeed");
        assert_eq!(after_kill.rows[0][0].as_deref(), Some("1"));

        let after_that = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.execute_query("", "SELECT 2 AS after_that"),
        )
        .await
        .expect("query hung for >10s")
        .expect("a further query should keep working normally");
        assert_eq!(after_that.rows[0][0].as_deref(), Some("2"));
    }

    // Regression test: a SELECT returning more rows than MAX_RESULT_ROWS
    // makes `execute_query` break out of the row loop and drop the row
    // stream before the server-side result set is fully drained. sqlx-mysql
    // is expected to drain the leftover rows itself on the next command
    // (`MySqlStream::wait_until_ready`), so this must not corrupt the
    // connection for later queries on the same (single) pooled connection.
    #[tokio::test]
    #[ignore]
    async fn test_query_survives_streaming_row_cap_cutoff() {
        use crate::MAX_RESULT_ROWS;

        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        let database = format!("zdbt_cutoff_{}", uuid::Uuid::new_v4().simple());
        provider
            .execute_query("", &format!("CREATE DATABASE `{database}`"))
            .await
            .expect("failed to create scratch database");

        provider
            .execute_query(&database, "CREATE TABLE big (id INT NOT NULL PRIMARY KEY)")
            .await
            .expect("failed to create scratch table");

        let row_count = MAX_RESULT_ROWS + 200;
        let values: Vec<String> = (0..row_count).map(|i| format!("({i})")).collect();
        provider
            .execute_query(
                &database,
                &format!("INSERT INTO big (id) VALUES {}", values.join(",")),
            )
            .await
            .expect("failed to seed scratch table");

        let first = provider
            .execute_query(&database, "SELECT id FROM big")
            .await
            .expect("first (cut-off) query failed outright");
        assert_eq!(
            first.rows.len(),
            MAX_RESULT_ROWS,
            "sanity: the cutoff should have kicked in"
        );

        let second = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.execute_query(&database, "SELECT 1 AS second"),
        )
        .await
        .expect("query hung for >10s right after the streaming cutoff")
        .expect("query right after the streaming cutoff should succeed");
        assert_eq!(second.rows[0][0].as_deref(), Some("1"));

        let third = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.execute_query(&database, "SELECT 2 AS third"),
        )
        .await
        .expect("query hung for >10s")
        .expect("a further query should keep working normally");
        assert_eq!(third.rows[0][0].as_deref(), Some("2"));

        provider
            .execute_query("", &format!("DROP DATABASE `{database}`"))
            .await
            .expect("failed to clean up scratch database");
    }

    /// Returns a loopback TCP port that was free at the moment of the call, by
    /// binding an ephemeral port and immediately releasing it. There is a tiny
    /// window before the caller rebinds it, but that is acceptable for a local
    /// ignored integration test and avoids the flakiness of a hard-coded port.
    fn free_loopback_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .expect("failed to bind an ephemeral loopback port")
            .local_addr()
            .expect("failed to read the bound address")
            .port()
    }

    /// A `socat` TCP proxy bound to loopback, used to sever the pool's
    /// connection at the transport level without touching the real MySQL
    /// server. It is panic-safe: `Drop` force-kills the child even if the
    /// test unwinds mid-way, so a frozen `socat` is never left behind.
    struct LoopbackProxy {
        child: smol::process::Child,
    }

    impl LoopbackProxy {
        fn spawn(port: u16, target: &str) -> Self {
            let child = smol::process::Command::new("socat")
                .arg(format!("TCP-LISTEN:{port},bind=127.0.0.1,reuseaddr"))
                .arg(format!("TCP:{target}"))
                .stdout(smol::process::Stdio::null())
                .stderr(smol::process::Stdio::null())
                .spawn()
                .expect(
                    "failed to spawn `socat` -- required for this test to simulate a frozen \
                     connection; install it (e.g. `apt install socat`) to run this test",
                );
            Self { child }
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }

        /// Suspends the proxy with SIGSTOP so packets stop flowing without any
        /// TCP close -- the transport-level equivalent of a frozen tunnel or a
        /// sleeping laptop. A plain `Child::kill()` only sends SIGKILL, so this
        /// needs a direct syscall.
        fn freeze(&self) {
            // Safety: `kill(2)` with the pid of a child we just spawned and a
            // fixed signal number has no memory-safety implications.
            let result = unsafe { libc::kill(self.pid() as libc::pid_t, libc::SIGSTOP) };
            assert_eq!(
                result,
                0,
                "SIGSTOP on socat pid {} failed: {}",
                self.pid(),
                std::io::Error::last_os_error()
            );
        }
    }

    impl Drop for LoopbackProxy {
        fn drop(&mut self) {
            // Best-effort teardown, also covering the panic/early-return path.
            // SIGKILL terminates a SIGSTOP-frozen process too; async-process's
            // global reaper collects the resulting zombie.
            // Safety: same as `freeze` -- a fixed signal to our own child pid.
            unsafe {
                libc::kill(self.pid() as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    // Regression test for the bug reported as "I run a query, then another
    // one, and everything breaks -- the connection dies and nothing works
    // after that". Simulates a *silently* dead connection: packets just stop
    // flowing, no FIN/RST -- e.g. a frozen SSH tunnel, a laptop
    // sleep/resume, a dropped VPN/wifi link -- by routing the pool through a
    // `socat` proxy and freezing that proxy with SIGSTOP (its process is
    // suspended, so the kernel never sends a close on its behalf, unlike a
    // clean `KILL <connection_id>` or process kill).
    //
    // Before the fix in `MySqlProvider`, this hangs forever: sqlx's own
    // health checks (`test_before_acquire`, the ping the pool runs when a
    // connection is returned) have no timeout, and the connection-return
    // bookkeeping runs in a background task detached from the caller, so
    // cancelling the caller never frees the pool's one permit. With
    // `max_connections(1)` that wedges every future call permanently.
    #[tokio::test]
    #[ignore]
    async fn test_query_recovers_after_connection_silently_freezes() {
        let real_config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let target = format!("{}:{}", real_config.host, real_config.port);
        let proxy_port = free_loopback_port();

        let proxy = LoopbackProxy::spawn(proxy_port, &target);
        // Give socat a moment to bind before connecting through it.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let mut proxied_config = real_config.clone();
        proxied_config.host = "127.0.0.1".to_string();
        proxied_config.port = proxy_port;

        let provider = MySqlProvider::connect(&proxied_config)
            .await
            .expect("Failed to connect through the loopback proxy");

        provider
            .execute_query("", "SELECT 1 AS warm_up")
            .await
            .expect("warm-up query through the proxy failed");

        proxy.freeze();

        // Worst case here is additive: the health-check probe fails after
        // CONNECTION_HEALTH_TIMEOUT (10s), then `reconnect` dials the same
        // still-frozen proxy and also fails, bounded by the more generous
        // RECONNECT_TIMEOUT (30s) -- up to 40s total. The outer bound and
        // the sanity assertion below both need headroom above that, not
        // just above CONNECTION_HEALTH_TIMEOUT alone.
        let started = std::time::Instant::now();
        let after_freeze = tokio::time::timeout(
            std::time::Duration::from_secs(50),
            provider.execute_query("", "SELECT 1 AS after_freeze"),
        )
        .await
        .expect(
            "query hung for >50s instead of failing fast -- the pool's single connection got \
             wedged forever by the frozen proxy",
        );
        assert!(
            after_freeze.is_err(),
            "expected the query to fail once the connection went silently dead, got: \
             {after_freeze:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(50),
            "query on a dead connection must fail within CONNECTION_HEALTH_TIMEOUT + \
             RECONNECT_TIMEOUT bounds, not hang until the outer test timeout"
        );

        // Simulate the network path recovering: drop the frozen proxy (its
        // `Drop` SIGKILLs it) and stand up a fresh, healthy one on the same
        // port.
        drop(proxy);
        let _healthy_proxy = LoopbackProxy::spawn(proxy_port, &target);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let after_recovery = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            provider.execute_query("", "SELECT 1 AS after_recovery"),
        )
        .await
        .expect("query hung for >30s after the network path recovered")
        .expect(
            "query must succeed again once the network recovered -- the provider should have \
             reconnected transparently instead of staying wedged to the dead connection",
        );
        assert_eq!(after_recovery.rows[0][0].as_deref(), Some("1"));
    }

    // Regression test for a stall *while a query is already in flight and
    // awaiting its result*, as opposed to `test_query_recovers_after_
    // connection_silently_freezes` above (which freezes the connection
    // before the next call even starts, so `ensure_live_pool`'s own probe
    // catches it). `ensure_live_pool`'s probe cannot catch this case: it
    // never runs concurrently with the caller's own query (both are
    // serialized behind `op_lock`), so a connection that dies while a
    // query's response is still being awaited, after the probe already
    // passed, previously had nothing bounding `stream.try_next().await` --
    // it blocked forever, and since `op_lock` is held for the whole call,
    // every later call on this provider (including the next
    // `ensure_live_pool`'s reconnect attempt) hung forever too, just trying
    // to acquire that same permit. This is the "первый запрос вернул мусор,
    // а второй вообще не работает, даже после перезапуска" failure Sergei
    // hit live against a real, occasionally flaky remote host.
    //
    // Uses `SLEEP(90)` (comfortably longer than both the freeze delay below
    // and `ROW_FETCH_TIMEOUT`) so the server genuinely cannot have sent any
    // response bytes yet when the proxy freezes -- this avoids relying on
    // MySQL flushing a small result incrementally row-by-row, which it does
    // not reliably do (a `SLEEP(1)`-per-row variant of this test was tried
    // first and the whole tiny result arrived in one burst regardless of
    // per-row delay, making the freeze a no-op).
    #[tokio::test]
    #[ignore]
    async fn test_query_recovers_after_connection_freezes_mid_stream() {
        let real_config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let target = format!("{}:{}", real_config.host, real_config.port);
        let proxy_port = free_loopback_port();

        let proxy = LoopbackProxy::spawn(proxy_port, &target);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let mut proxied_config = real_config.clone();
        proxied_config.host = "127.0.0.1".to_string();
        proxied_config.port = proxy_port;

        let provider = MySqlProvider::connect(&proxied_config)
            .await
            .expect("Failed to connect through the loopback proxy");

        // Freeze shortly after issuing the query -- well past the point
        // `ensure_live_pool`'s own pre-flight probe has certainly already
        // passed (the connection was healthy at that point), but nowhere
        // near enough time for the server's 90s `SLEEP` to have completed.
        let freeze_proxy = std::sync::Arc::new(proxy);
        let freeze_handle = {
            let freeze_proxy = freeze_proxy.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                freeze_proxy.freeze();
            })
        };

        // Worst case is additive: the ~0.5s freeze delay plus
        // ROW_FETCH_TIMEOUT (60s) for the stalled fetch to give up.
        // Generous headroom above that to avoid flakiness without masking a
        // genuine hang.
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(75),
            provider.execute_query("", "SELECT SLEEP(90) AS sleep_result"),
        )
        .await
        .expect(
            "query awaiting its result hung for >75s instead of failing fast -- op_lock stayed \
             held forever, wedging the provider",
        );

        let error =
            result.expect_err("expected the query to fail once the connection froze in flight");
        assert!(
            error.chain().any(|cause| cause
                .to_string()
                .contains("Query results stopped streaming")),
            "expected the row-fetch timeout's own error context in the chain, got: {error:#}"
        );
        assert!(
            started.elapsed() >= ROW_FETCH_TIMEOUT,
            "failed too fast ({:?}) to be ROW_FETCH_TIMEOUT firing -- likely an unrelated \
             error rather than the in-flight stall this test targets",
            started.elapsed()
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(75),
            "an in-flight stall must fail within ROW_FETCH_TIMEOUT, not hang until the outer \
             test timeout"
        );

        freeze_handle.abort();

        // The provider must not still be wedged: a fresh, unrelated query
        // right after must succeed once the network path recovers, proving
        // `op_lock` was actually released (not just that this one call
        // eventually errored).
        drop(freeze_proxy);
        let _healthy_proxy = LoopbackProxy::spawn(proxy_port, &target);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let after_recovery = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            provider.execute_query("", "SELECT 1 AS after_recovery"),
        )
        .await
        .expect("query hung for >30s after the network path recovered")
        .expect(
            "a fresh query must succeed once the network recovered -- op_lock must have been \
             released when the mid-stream stall timed out, not left held forever",
        );
        assert_eq!(after_recovery.rows[0][0].as_deref(), Some("1"));
    }

    /// A TCP proxy that simulates a real, healthy-but-slow network path (a
    /// loaded corporate VPN/WAN link to a remote host), as opposed to
    /// `LoopbackProxy` (frozen: silently dead) or plain immediate forwarding
    /// (a local Docker path). It stays bound for the whole test: each newly
    /// accepted connection is relayed to `target` only after whatever delay
    /// is current at accept time (`set_delay`), so the same proxy can serve
    /// a fast warm-up connection and then a slow one later without ever
    /// re-binding the port.
    ///
    /// `sever_current_connection` drops (and so cancels, per
    /// `smol::Task`'s cancel-on-drop) the relay task for whichever
    /// connection is currently active, closing both of its sockets. This
    /// forces the pool's existing connection to appear dead without
    /// touching the listener itself, so the next connection accepted on the
    /// same port is the provider's own `reconnect`.
    struct SlowLoopbackProxy {
        _listener_task: tokio::task::JoinHandle<()>,
        active_relay: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
        delay_millis: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    impl SlowLoopbackProxy {
        async fn spawn(port: u16, target: String) -> Self {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .expect("failed to bind the slow proxy's loopback port");

            let active_relay: std::sync::Arc<
                std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
            > = Default::default();
            let delay_millis = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

            let listener_task = {
                let active_relay = active_relay.clone();
                let delay_millis = delay_millis.clone();
                tokio::spawn(async move {
                    loop {
                        let Ok((inbound, _)) = listener.accept().await else {
                            break;
                        };
                        let target = target.clone();
                        let delay = std::time::Duration::from_millis(
                            delay_millis.load(std::sync::atomic::Ordering::SeqCst),
                        );
                        let relay_task = tokio::spawn(Self::relay(inbound, target, delay));
                        let previous = active_relay
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .replace(relay_task);
                        // A leftover handle here only happens if a new
                        // connection arrived before the previous one was
                        // severed; abort it so it cannot keep relaying
                        // stale traffic alongside the new connection.
                        if let Some(previous) = previous {
                            previous.abort();
                        }
                    }
                })
            };

            Self {
                _listener_task: listener_task,
                active_relay,
                delay_millis,
            }
        }

        fn set_delay(&self, delay: std::time::Duration) {
            self.delay_millis.store(
                delay.as_millis() as u64,
                std::sync::atomic::Ordering::SeqCst,
            );
        }

        /// Aborts the relay task for whichever connection is currently
        /// active, closing both of its sockets. Tokio's `JoinHandle` does
        /// not cancel its task on drop (unlike smol's `Task`), so this
        /// aborts explicitly rather than relying on the handle going out of
        /// scope.
        fn sever_current_connection(&self) {
            if let Some(relay_task) = self
                .active_relay
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                relay_task.abort();
            }
        }

        async fn relay(
            mut inbound: tokio::net::TcpStream,
            target: String,
            delay: std::time::Duration,
        ) {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let Ok(mut outbound) = tokio::net::TcpStream::connect(&target).await else {
                return;
            };
            // Whichever side closes first ends this with an EOF/error; that
            // is expected connection teardown here, not a signal to assert
            // on.
            tokio::io::copy_bidirectional(&mut inbound, &mut outbound)
                .await
                .ok();
        }
    }

    // Regression test for the report "SHOW CREATE TABLE works instantly
    // against a local test MySQL but fails against the real remote stage
    // host with 'Timed out reconnecting to MySQL' / 'deadline has
    // elapsed'". The remote host was healthy -- just slow to complete a
    // fresh TCP+TLS+auth handshake over a real corporate/VPN network path --
    // but `reconnect` shared its timeout with the `SELECT 1` health-check
    // probe (`CONNECTION_HEALTH_TIMEOUT`, 10s), which is only ever bounding
    // a round trip on an already-open connection and has no business
    // bounding a full handshake from scratch too. This simulates that class
    // of slow-but-working path locally via a proxy that delays relaying a
    // freshly accepted connection, and asserts the reconnect succeeds within
    // the new, separate `RECONNECT_TIMEOUT` budget.
    #[tokio::test]
    #[ignore]
    async fn test_reconnect_succeeds_over_a_slow_but_working_network_path() {
        let real_config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let target = format!("{}:{}", real_config.host, real_config.port);
        let proxy_port = free_loopback_port();

        let proxy = SlowLoopbackProxy::spawn(proxy_port, target).await;

        let mut proxied_config = real_config.clone();
        proxied_config.host = "127.0.0.1".to_string();
        proxied_config.port = proxy_port;

        let provider = MySqlProvider::connect(&proxied_config)
            .await
            .expect("Failed to connect through the slow proxy");

        provider
            .execute_query("", "SELECT 1 AS warm_up")
            .await
            .expect("warm-up query through the proxy failed");

        // Make the *next* connection -- the one `reconnect` opens -- take
        // longer than the old, too-tight 10s reconnect budget but
        // comfortably less than the new one, then kill the existing
        // connection so the next query is forced through
        // `ensure_live_pool` -> `reconnect`.
        let slow_reconnect_delay = std::time::Duration::from_secs(15);
        proxy.set_delay(slow_reconnect_delay);
        proxy.sever_current_connection();

        let started = std::time::Instant::now();
        let after_reconnect = tokio::time::timeout(
            std::time::Duration::from_secs(40),
            provider.execute_query("", "SELECT 1 AS after_reconnect"),
        )
        .await
        .expect("query hung past the outer 40s test bound")
        .expect(
            "reconnecting over a slow-but-working network path should succeed -- if this failed \
             with 'Timed out reconnecting to MySQL', RECONNECT_TIMEOUT is too tight for a real \
             WAN TCP+TLS+auth handshake",
        );
        assert_eq!(after_reconnect.rows[0][0].as_deref(), Some("1"));
        assert!(
            started.elapsed() >= slow_reconnect_delay,
            "sanity: the reconnect should have actually waited out the slow path, took {:?}",
            started.elapsed()
        );
    }

    // Regression test for the concurrency hazard the review flagged: a
    // legitimate long query holding the single connection must NOT make a
    // concurrent second call mistake the busy connection for a dead one and
    // spuriously reconnect (which would break the single-connection
    // invariant and briefly run two MySQL sessions for one provider).
    //
    // Task A runs `SELECT SLEEP(N)` with N > CONNECTION_HEALTH_TIMEOUT while
    // task B issues a normal query. `op_lock` must serialize them onto the
    // same physical connection: B waits for A rather than reconnecting, so
    // the server-side connection id is unchanged across the whole episode.
    // Before `op_lock`, B's liveness probe would lose the race for the
    // permit, time out at CONNECTION_HEALTH_TIMEOUT, and reconnect -- the
    // connection id would change.
    #[tokio::test]
    #[ignore]
    async fn test_concurrent_slow_query_does_not_trigger_spurious_reconnect() {
        use std::sync::Arc;

        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = Arc::new(
            MySqlProvider::connect(&config)
                .await
                .expect("Failed to connect"),
        );

        let connection_id = |provider: Arc<MySqlProvider>| async move {
            provider
                .execute_query("", "SELECT CAST(CONNECTION_ID() AS CHAR) AS id")
                .await
                .expect("failed to read CONNECTION_ID()")
                .rows[0][0]
                .clone()
                .expect("CONNECTION_ID() returned NULL")
        };

        let id_before = connection_id(provider.clone()).await;

        // Task A occupies the single connection for longer than the health
        // probe's timeout.
        let slow = tokio::spawn({
            let provider = provider.clone();
            async move {
                provider
                    .execute_query("", "SELECT SLEEP(14)")
                    .await
                    .expect("slow query failed")
            }
        });

        // Let task A actually acquire `op_lock` and start sleeping first.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // Task B must wait for A (serialized), not reconnect. Bound generously
        // so a genuine hang still fails the test rather than stalling forever.
        let id_during = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            connection_id(provider.clone()),
        )
        .await
        .expect("concurrent query hung for >30s");

        slow.await.expect("slow task panicked");
        let id_after = connection_id(provider.clone()).await;

        assert_eq!(
            id_before, id_during,
            "a concurrent query ran on a different connection id -- the provider spuriously \
             reconnected while a legitimate slow query held the connection"
        );
        assert_eq!(
            id_before, id_after,
            "the connection id changed across the episode -- an unexpected reconnect happened"
        );
    }

    // Regression test tied to task #30's report ("Zed crash triggered by
    // multi-row MySQL INSERT"). Builds a large multi-row INSERT with literal
    // (non-bind-parameter) VALUES tuples, matching how db_client_ui's
    // data_import/table_copy features build their INSERT statements, and
    // checks that both a comfortably-sized batch and a batch deliberately
    // oversized past `max_allowed_packet` leave the connection usable
    // afterward -- i.e. this class of bug is not the same root cause as the
    // connection-wedging fixed by `MySqlProvider::ensure_live_pool`.
    #[tokio::test]
    #[ignore]
    async fn test_large_multi_row_insert_leaves_connection_usable() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        let database = format!("zdbt_bulk_{}", uuid::Uuid::new_v4().simple());
        provider
            .execute_query("", &format!("CREATE DATABASE `{database}`"))
            .await
            .expect("failed to create scratch database");
        provider
            .execute_query(
                &database,
                "CREATE TABLE bulk (id INT NOT NULL PRIMARY KEY, name VARCHAR(64) NOT NULL)",
            )
            .await
            .expect("failed to create scratch table");

        let insert_sql = |row_count: usize| {
            let values: Vec<String> = (0..row_count)
                .map(|i| format!("({i}, 'row-{i}')"))
                .collect();
            format!("INSERT INTO bulk (id, name) VALUES {}", values.join(","))
        };

        let comfortable = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            provider.execute_query(&database, &insert_sql(20_000)),
        )
        .await
        .expect("comfortably-sized multi-row INSERT hung for >30s")
        .expect("comfortably-sized multi-row INSERT should succeed");
        assert_eq!(comfortable.rows_affected, 20_000);

        let after_comfortable = provider
            .execute_query(&database, "SELECT COUNT(*) AS n FROM bulk")
            .await
            .expect("follow-up query after the comfortable INSERT should succeed");
        assert_eq!(after_comfortable.rows[0][0].as_deref(), Some("20000"));

        // ~85MB of SQL text, comfortably past the server's default 64MB
        // `max_allowed_packet` -- this must surface as a clean database
        // error, not hang or corrupt the connection.
        let oversized = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            provider.execute_query(&database, &insert_sql(3_500_000)),
        )
        .await
        .expect("oversized multi-row INSERT hung for >30s instead of erroring");
        assert!(
            oversized.is_err(),
            "an INSERT past max_allowed_packet should return a database error"
        );

        let after_oversized = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.execute_query(&database, "SELECT COUNT(*) AS n FROM bulk"),
        )
        .await
        .expect("follow-up query hung for >10s after the oversized INSERT error")
        .expect("follow-up query after the oversized INSERT's error should still succeed");
        assert_eq!(after_oversized.rows[0][0].as_deref(), Some("20000"));

        provider
            .execute_query("", &format!("DROP DATABASE `{database}`"))
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_databases() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let databases = provider
            .list_databases()
            .await
            .expect("Failed to list databases");
        assert!(!databases.is_empty(), "Expected at least one database");
        assert!(
            databases.iter().any(|db| db.name == "information_schema"),
            "information_schema should always be present"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_tables() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let tables = provider
            .list_tables("information_schema")
            .await
            .expect("Failed to list tables");
        assert!(!tables.is_empty(), "information_schema should have tables");
        assert!(
            tables.iter().any(|t| t.name == "TABLES"),
            "TABLES should exist in information_schema"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_describe_table() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let columns = provider
            .describe_table("information_schema", "TABLES")
            .await
            .expect("Failed to describe table");
        assert!(!columns.is_empty(), "TABLES should have columns");
        assert!(
            columns.iter().any(|c| c.name == "TABLE_NAME"),
            "TABLE_NAME column should exist"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_database_ddl() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let ddl = provider
            .get_database_ddl("information_schema")
            .await
            .expect("Failed to get database DDL");
        assert!(
            ddl.to_uppercase().contains("CREATE DATABASE"),
            "DDL should contain CREATE DATABASE, got: {ddl}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_execute_select_query() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let result = provider
            .execute_query(
                "information_schema",
                "SELECT 1 AS value, 'hello' AS greeting",
            )
            .await
            .expect("Failed to execute query");
        assert_eq!(result.columns, vec!["value", "greeting"]);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0].as_deref(), Some("1"));
        assert_eq!(result.rows[0][1].as_deref(), Some("hello"));
    }

    // Verifies the streaming decode bounds memory: an unbounded SELECT over a
    // very large table must stop at the hard row cap instead of pulling the
    // whole table. This is the regression guard for the "app freezes / OS
    // offers to kill it on a big query" report.
    #[tokio::test]
    #[ignore]
    async fn test_unbounded_select_is_bounded() {
        use crate::MAX_RESULT_ROWS;
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let result = provider
            .execute_query("instruments", "SELECT * FROM company_owners")
            .await
            .expect("Failed to execute query");

        assert!(
            result.rows.len() <= MAX_RESULT_ROWS,
            "unbounded SELECT must stop at the hard row cap, got {} rows",
            result.rows.len()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_users_populates_grants() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let users = provider.list_users().await.expect("Failed to list users");
        assert!(!users.is_empty(), "Expected at least one MySQL account");
        // A privileged test account should see grants for at least one user.
        assert!(
            users.iter().any(|user| !user.grants.is_empty()),
            "Expected SHOW GRANTS to populate grants for at least one account"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_execute_show_databases() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let result = provider
            .execute_query("", "SHOW DATABASES")
            .await
            .expect("Failed to execute SHOW DATABASES");
        assert!(!result.columns.is_empty());
        assert!(!result.rows.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn test_execute_show_create_table() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let database = format!("zed_db_client_test_{}", Uuid::new_v4().simple());
        let table = "show_create_smoke";
        let quoted_database = format!("`{}`", database.replace('`', "``"));
        let quoted_table = format!("`{}`", table.replace('`', "``"));

        provider
            .execute_query("", &format!("CREATE DATABASE {quoted_database}"))
            .await
            .expect("Failed to create temporary database");

        let test_result = async {
            provider
                .execute_query(
                    &database,
                    &format!(
                        "CREATE TABLE {quoted_table} (
                            id INT NOT NULL PRIMARY KEY,
                            name VARCHAR(64) NULL
                        )"
                    ),
                )
                .await?;
            provider
                .execute_query(&database, &format!("SHOW CREATE TABLE {quoted_table}"))
                .await
        }
        .await;

        provider
            .execute_query("", &format!("DROP DATABASE {quoted_database}"))
            .await
            .expect("Failed to clean up temporary database");

        let result = test_result.expect("Failed to execute SHOW CREATE TABLE");
        assert!(result.columns.len() >= 2);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0].as_deref(), Some(table));
        assert!(
            result.rows[0].iter().flatten().any(|cell| {
                cell.contains("CREATE TABLE") && cell.contains(&format!("`{table}`"))
            }),
            "SHOW CREATE TABLE should return a DDL cell"
        );
    }

    // Regression test for the "this functionality requires a Tokio context"
    // panic: connecting from a non-Tokio executor (as GPUI does) must work
    // through RuntimeProvider. A plain #[test] with futures::executor::block_on
    // means there is no ambient Tokio runtime, mirroring the real call site.
    #[test]
    #[ignore]
    fn test_connect_from_non_tokio_executor() {
        let Some(config) = test_config_from_env() else {
            panic!("MYSQL_TEST_URL env var required for integration tests");
        };
        futures::executor::block_on(async move {
            let raw = crate::on_runtime(async move { MySqlProvider::connect(&config).await })
                .await
                .expect("connect via runtime");
            let provider = crate::RuntimeProvider::new(std::sync::Arc::new(raw));
            provider.ping().await.expect("ping via runtime");
            let result = provider
                .execute_query("", "SELECT 1 AS one")
                .await
                .expect("query via runtime");
            assert!(!result.columns.is_empty());
        });
    }

    // Regression test: `is_read_query` must recognize a CTE (`WITH ...
    // SELECT`) as a read query, matching db_client::is_read_only_query and
    // the Postgres provider's identical check — otherwise the query falls
    // into the non-streaming `.execute()` path and the grid silently shows
    // zero columns and zero rows even though the query succeeded.
    #[tokio::test]
    #[ignore]
    async fn test_with_cte_returns_rows() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let result = provider
            .execute_query("", "WITH one AS (SELECT 1 AS n) SELECT n FROM one")
            .await
            .expect("Failed to execute query");

        assert_eq!(result.columns, vec!["n".to_string()]);
        assert_eq!(result.rows.len(), 1);
    }

    // Gates the "NULL decodes as 0" hypothesis from the grid UX audit for
    // MySQL specifically -- do not assume the fix SQLite needed applies here
    // without empirical proof, per that audit's own correction.
    #[tokio::test]
    #[ignore]
    async fn test_null_cells_decode_as_none() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");
        let result = provider
            .execute_query(
                "",
                "SELECT CAST(NULL AS CHAR) AS text_col, CAST(NULL AS SIGNED) AS int_col",
            )
            .await
            .expect("Failed to execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0][0], None,
            "a NULL text column must decode to None, not Some(\"0\")/Some(\"\")"
        );
        assert_eq!(
            result.rows[0][1], None,
            "a NULL integer column must decode to None, not Some(\"0\")"
        );
    }

    /// Runs `body` against a fresh scratch database, dropping the database
    /// afterward regardless of the outcome -- mirrors the cleanup pattern in
    /// `test_execute_show_create_table` so every CRUD test below gets its
    /// own isolated database without leaking one on failure.
    async fn with_scratch_database<'p, F, Fut, T>(provider: &'p MySqlProvider, body: F) -> T
    where
        F: FnOnce(&'p MySqlProvider, String) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let database = format!("zdbt_{}", Uuid::new_v4().simple());
        let quoted_database = format!("`{database}`");
        provider
            .execute_query("", &format!("CREATE DATABASE {quoted_database}"))
            .await
            .expect("Failed to create scratch database");

        let result = body(provider, database.clone()).await;

        provider
            .execute_query("", &format!("DROP DATABASE `{database}`"))
            .await
            .expect("Failed to clean up scratch database");

        result
    }

    #[tokio::test]
    #[ignore]
    async fn test_create_alter_and_drop_table() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE widgets (id INT NOT NULL PRIMARY KEY, name VARCHAR(64) NOT NULL)",
                )
                .await
                .expect("Failed to create table");

            let columns_before = provider
                .describe_table(&database, "widgets")
                .await
                .expect("Failed to describe table");
            assert_eq!(columns_before.len(), 2);

            provider
                .execute_query(&database, "ALTER TABLE widgets ADD COLUMN weight INT NULL")
                .await
                .expect("Failed to alter table");

            let columns_after = provider
                .describe_table(&database, "widgets")
                .await
                .expect("Failed to describe table after ALTER");
            assert_eq!(columns_after.len(), 3);
            assert!(columns_after.iter().any(|c| c.name == "weight"));

            provider
                .drop_table(&database, "widgets")
                .await
                .expect("Failed to drop table");

            let tables = provider
                .list_tables(&database)
                .await
                .expect("Failed to list tables");
            assert!(
                !tables.iter().any(|t| t.name == "widgets"),
                "widgets must be gone after DROP TABLE"
            );
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_create_and_drop_index() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE indexed_widgets (id INT NOT NULL PRIMARY KEY, sku VARCHAR(64) NOT NULL)",
                )
                .await
                .expect("Failed to create table");
            provider
                .execute_query(
                    &database,
                    "CREATE UNIQUE INDEX sku_idx ON indexed_widgets (sku)",
                )
                .await
                .expect("Failed to create index");

            let indexes = provider
                .list_indexes(&database, "indexed_widgets")
                .await
                .expect("Failed to list indexes");
            let sku_index = indexes
                .iter()
                .find(|i| i.name == "sku_idx")
                .expect("sku_idx should be listed");
            assert!(sku_index.unique, "sku_idx was created as UNIQUE");
            assert_eq!(sku_index.columns, vec!["sku".to_string()]);

            provider
                .execute_query(&database, "DROP INDEX sku_idx ON indexed_widgets")
                .await
                .expect("Failed to drop index");

            let indexes_after = provider
                .list_indexes(&database, "indexed_widgets")
                .await
                .expect("Failed to list indexes after drop");
            assert!(!indexes_after.iter().any(|i| i.name == "sku_idx"));
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_create_query_and_drop_a_view() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE items (id INT NOT NULL PRIMARY KEY, price INT NOT NULL)",
                )
                .await
                .expect("Failed to create table");
            provider
                .execute_query(&database, "INSERT INTO items (id, price) VALUES (1, 150)")
                .await
                .expect("Failed to insert row");
            provider
                .execute_query(
                    &database,
                    "CREATE VIEW pricey_items AS SELECT * FROM items WHERE price > 100",
                )
                .await
                .expect("Failed to create view");

            let views = provider
                .list_views(&database)
                .await
                .expect("Failed to list views");
            assert!(views.iter().any(|v| v == "pricey_items"));

            let result = provider
                .execute_query(&database, "SELECT id, price FROM pricey_items")
                .await
                .expect("Failed to query the view");
            assert_eq!(result.rows.len(), 1);
            assert_eq!(result.rows[0][0].as_deref(), Some("1"));

            provider
                .execute_query(&database, "DROP VIEW pricey_items")
                .await
                .expect("Failed to drop view");
            let views_after = provider
                .list_views(&database)
                .await
                .expect("Failed to list views after drop");
            assert!(!views_after.iter().any(|v| v == "pricey_items"));
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_insert_update_and_delete_row_lifecycle() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE accounts (id INT NOT NULL PRIMARY KEY, balance INT NOT NULL)",
                )
                .await
                .expect("Failed to create table");

            provider
                .execute_query(
                    &database,
                    "INSERT INTO accounts (id, balance) VALUES (1, 100)",
                )
                .await
                .expect("Failed to insert row");
            let after_insert = provider
                .execute_query(&database, "SELECT balance FROM accounts WHERE id = 1")
                .await
                .expect("Failed to select after insert");
            assert_eq!(after_insert.rows[0][0].as_deref(), Some("100"));

            provider
                .execute_query(&database, "UPDATE accounts SET balance = 250 WHERE id = 1")
                .await
                .expect("Failed to update row");
            let after_update = provider
                .execute_query(&database, "SELECT balance FROM accounts WHERE id = 1")
                .await
                .expect("Failed to select after update");
            assert_eq!(after_update.rows[0][0].as_deref(), Some("250"));

            provider
                .execute_query(&database, "DELETE FROM accounts WHERE id = 1")
                .await
                .expect("Failed to delete row");
            let after_delete = provider
                .execute_query(&database, "SELECT balance FROM accounts WHERE id = 1")
                .await
                .expect("Failed to select after delete");
            assert!(
                after_delete.rows.is_empty(),
                "row should be gone after DELETE"
            );
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_upsert_via_on_duplicate_key_update() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE counters (name VARCHAR(64) NOT NULL PRIMARY KEY, hits INT NOT NULL)",
                )
                .await
                .expect("Failed to create table");

            let upsert_sql = "INSERT INTO counters (name, hits) VALUES ('clicks', 1) \
                 ON DUPLICATE KEY UPDATE hits = hits + 1";
            provider
                .execute_query(&database, upsert_sql)
                .await
                .expect("Failed first upsert (insert path)");
            provider
                .execute_query(&database, upsert_sql)
                .await
                .expect("Failed second upsert (update path)");

            let result = provider
                .execute_query(&database, "SELECT hits FROM counters WHERE name = 'clicks'")
                .await
                .expect("Failed to select counter");
            assert_eq!(result.rows.len(), 1, "upsert must not create a duplicate row");
            assert_eq!(
                result.rows[0][0].as_deref(),
                Some("2"),
                "the second upsert must have taken the UPDATE branch, not re-inserted at 1"
            );
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_foreign_keys_finds_a_declared_fk() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE authors (id INT NOT NULL PRIMARY KEY)",
                )
                .await
                .expect("Failed to create authors table");
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE posts (\
                         id INT NOT NULL PRIMARY KEY, \
                         author_id INT NOT NULL, \
                         CONSTRAINT fk_posts_author FOREIGN KEY (author_id) REFERENCES authors (id)\
                     )",
                )
                .await
                .expect("Failed to create posts table");

            let fks = provider
                .list_foreign_keys(&database, "posts")
                .await
                .expect("Failed to list foreign keys");
            assert_eq!(fks.len(), 1);
            assert_eq!(fks[0].name, "fk_posts_author");
            assert_eq!(fks[0].from_column, "author_id");
            assert_eq!(fks[0].to_table, "authors");
            assert_eq!(fks[0].to_column, "id");
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_check_constraints_finds_a_declared_check() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE products (\
                         id INT NOT NULL PRIMARY KEY, \
                         price INT NOT NULL, \
                         CONSTRAINT chk_price_positive CHECK (price > 0)\
                     )",
                )
                .await
                .expect("Failed to create products table");

            let checks = provider
                .list_check_constraints(&database, "products")
                .await
                .expect("Failed to list check constraints");
            assert_eq!(checks.len(), 1);
            assert_eq!(checks[0].name, "chk_price_positive");
            assert!(
                checks[0].expression.contains("price"),
                "expected the check expression to reference `price`, got {}",
                checks[0].expression
            );
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_procedures_finds_a_created_procedure_and_function() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE PROCEDURE greet(IN who VARCHAR(64)) \
                     BEGIN SELECT CONCAT('Hello, ', who) AS greeting; END",
                )
                .await
                .expect("Failed to create procedure");
            provider
                .execute_query(
                    &database,
                    "CREATE FUNCTION double_it(n INT) RETURNS INT DETERMINISTIC \
                     BEGIN RETURN n * 2; END",
                )
                .await
                .expect("Failed to create function");

            let procedures = provider
                .list_procedures(&database)
                .await
                .expect("Failed to list procedures");
            let names: Vec<&str> = procedures.iter().map(|p| p.name.as_str()).collect();
            assert!(names.contains(&"greet"), "expected `greet` among {names:?}");
            assert!(
                names.contains(&"double_it"),
                "expected `double_it` among {names:?}"
            );
            let greet = procedures.iter().find(|p| p.name == "greet").unwrap();
            assert_eq!(greet.kind, ProcedureKind::Procedure);
            let double_it = procedures.iter().find(|p| p.name == "double_it").unwrap();
            assert_eq!(double_it.kind, ProcedureKind::Function);
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_triggers_finds_a_created_trigger() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE audit_log (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, message VARCHAR(255))",
                )
                .await
                .expect("Failed to create audit_log table");
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE widgets (id INT NOT NULL PRIMARY KEY, name VARCHAR(64))",
                )
                .await
                .expect("Failed to create widgets table");
            provider
                .execute_query(
                    &database,
                    "CREATE TRIGGER widgets_after_insert AFTER INSERT ON widgets FOR EACH ROW \
                     INSERT INTO audit_log (message) VALUES ('widget inserted')",
                )
                .await
                .expect("Failed to create trigger");

            let triggers = provider
                .list_triggers(&database, "widgets")
                .await
                .expect("Failed to list triggers");
            assert_eq!(triggers.len(), 1);
            assert_eq!(triggers[0].name, "widgets_after_insert");
            assert_eq!(triggers[0].event, "INSERT");
            assert_eq!(triggers[0].timing, "AFTER");
            assert_eq!(triggers[0].table_name, "widgets");
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_events_finds_a_created_event() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE EVENT nightly_cleanup ON SCHEDULE EVERY 1 DAY DISABLE DO SELECT 1",
                )
                .await
                .expect("Failed to create event");

            let events = provider
                .list_events(&database)
                .await
                .expect("Failed to list events");
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].name, "nightly_cleanup");
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_users_finds_the_connected_root_user() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        let users = provider.list_users().await.expect("Failed to list users");
        assert!(
            users.iter().any(|user| user.name == "root"),
            "expected the connected root user among {:?}",
            users.iter().map(|u| &u.name).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_truncate_table_removes_rows_but_keeps_the_table() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE crumbs (id INT NOT NULL PRIMARY KEY)",
                )
                .await
                .expect("Failed to create table");
            provider
                .execute_query(&database, "INSERT INTO crumbs (id) VALUES (1), (2), (3)")
                .await
                .expect("Failed to insert rows");

            provider
                .truncate_table(&database, "crumbs")
                .await
                .expect("Failed to truncate table");

            let tables = provider
                .list_tables(&database)
                .await
                .expect("Failed to list tables");
            assert!(
                tables.iter().any(|t| t.name == "crumbs"),
                "truncate must not drop the table itself"
            );
            let remaining = provider
                .execute_query(&database, "SELECT id FROM crumbs")
                .await
                .expect("Failed to select from truncated table");
            assert!(
                remaining.rows.is_empty(),
                "truncate must remove every row, got {:?}",
                remaining.rows
            );
        })
        .await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_rename_table_changes_the_visible_name() {
        let config =
            test_config_from_env().expect("MYSQL_TEST_URL env var required for integration tests");
        let provider = MySqlProvider::connect(&config)
            .await
            .expect("Failed to connect");

        with_scratch_database(&provider, |provider, database| async move {
            provider
                .execute_query(
                    &database,
                    "CREATE TABLE old_name (id INT NOT NULL PRIMARY KEY)",
                )
                .await
                .expect("Failed to create table");

            provider
                .rename_table(&database, "old_name", "new_name")
                .await
                .expect("Failed to rename table");

            let tables = provider
                .list_tables(&database)
                .await
                .expect("Failed to list tables");
            let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
            assert!(
                !names.contains(&"old_name"),
                "the old name must no longer be listed, got {names:?}"
            );
            assert!(
                names.contains(&"new_name"),
                "the new name must be listed, got {names:?}"
            );
        })
        .await;
    }
}
