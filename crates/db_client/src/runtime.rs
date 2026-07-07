use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::Result;
use async_trait::async_trait;
use tokio::runtime::{Handle, Runtime};

use crate::QUERY_TIMEOUT;
use crate::provider::DbProvider;
use crate::schema::{
    ColumnInfo, DatabaseInfo, IndexInfo, ProcedureInfo, QueryResult, TableInfo, TriggerInfo,
    UserInfo,
};

fn runtime() -> Result<&'static Runtime> {
    static RUNTIME: OnceLock<std::result::Result<Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("db-client-tokio")
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!("failed to initialize database runtime: {error}"))
}

/// Drives `future` to completion on the shared Tokio runtime.
///
/// The database backends (sqlx, redis, reqwest) need a Tokio reactor for their
/// socket I/O and background pool tasks. GPUI's executor is not Tokio, so these
/// futures must run here instead of on the calling executor — otherwise they
/// panic with "this functionality requires a Tokio context".
pub async fn on_runtime<T, F>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    on_runtime_with_timeout(QUERY_TIMEOUT, future).await
}

/// Core of [`on_runtime`], taking the timeout explicitly so tests can exercise
/// the timeout-error path with a short duration instead of waiting out the
/// real 30-minute `QUERY_TIMEOUT`.
async fn on_runtime_with_timeout<T, F>(timeout: std::time::Duration, future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let handle: Handle = runtime()?.handle().clone();
    let bounded = async move {
        match tokio::time::timeout(timeout, future).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "Database call timed out after {} minutes",
                timeout.as_secs() / 60
            )),
        }
    };
    match handle.spawn(bounded).await {
        Ok(output) => output,
        Err(join_error) => Err(anyhow::anyhow!("database task failed: {join_error}")),
    }
}

/// Wraps a concrete [`DbProvider`] so every call runs on the shared Tokio
/// runtime via [`on_runtime`]. Consumers hold this behind `Arc<dyn DbProvider>`
/// and stay unaware that the backends require Tokio.
pub struct RuntimeProvider {
    inner: Arc<dyn DbProvider>,
}

impl RuntimeProvider {
    pub fn new(inner: Arc<dyn DbProvider>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl DbProvider for RuntimeProvider {
    async fn ping(&self) -> Result<()> {
        let inner = self.inner.clone();
        on_runtime(async move { inner.ping().await }).await
    }

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
        let inner = self.inner.clone();
        on_runtime(async move { inner.list_databases().await }).await
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>> {
        let inner = self.inner.clone();
        let database = database.to_owned();
        on_runtime(async move { inner.list_tables(&database).await }).await
    }

    async fn describe_table(&self, database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let inner = self.inner.clone();
        let database = database.to_owned();
        let table = table.to_owned();
        on_runtime(async move { inner.describe_table(&database, &table).await }).await
    }

    async fn execute_query(&self, database: &str, sql: &str) -> Result<QueryResult> {
        let inner = self.inner.clone();
        let database = database.to_owned();
        let sql = sql.to_owned();
        on_runtime(async move { inner.execute_query(&database, &sql).await }).await
    }

    async fn get_table_ddl(&self, database: &str, table: &str) -> Result<String> {
        let inner = self.inner.clone();
        let database = database.to_owned();
        let table = table.to_owned();
        on_runtime(async move { inner.get_table_ddl(&database, &table).await }).await
    }

    async fn get_database_ddl(&self, database: &str) -> Result<String> {
        let inner = self.inner.clone();
        let database = database.to_owned();
        on_runtime(async move { inner.get_database_ddl(&database).await }).await
    }

    async fn list_indexes(&self, database: &str, table: &str) -> Result<Vec<IndexInfo>> {
        let inner = self.inner.clone();
        let database = database.to_owned();
        let table = table.to_owned();
        on_runtime(async move { inner.list_indexes(&database, &table).await }).await
    }

    async fn list_procedures(&self, database: &str) -> Result<Vec<ProcedureInfo>> {
        let inner = self.inner.clone();
        let database = database.to_owned();
        on_runtime(async move { inner.list_procedures(&database).await }).await
    }

    async fn list_triggers(&self, database: &str, table: &str) -> Result<Vec<TriggerInfo>> {
        let inner = self.inner.clone();
        let database = database.to_owned();
        let table = table.to_owned();
        on_runtime(async move { inner.list_triggers(&database, &table).await }).await
    }

    async fn list_users(&self) -> Result<Vec<UserInfo>> {
        let inner = self.inner.clone();
        on_runtime(async move { inner.list_users().await }).await
    }

    async fn truncate_table(&self, database: &str, table: &str) -> Result<()> {
        let inner = self.inner.clone();
        let database = database.to_owned();
        let table = table.to_owned();
        on_runtime(async move { inner.truncate_table(&database, &table).await }).await
    }

    async fn drop_table(&self, database: &str, table: &str) -> Result<()> {
        let inner = self.inner.clone();
        let database = database.to_owned();
        let table = table.to_owned();
        on_runtime(async move { inner.drop_table(&database, &table).await }).await
    }

    async fn rename_table(&self, database: &str, old_name: &str, new_name: &str) -> Result<()> {
        let inner = self.inner.clone();
        let database = database.to_owned();
        let old_name = old_name.to_owned();
        let new_name = new_name.to_owned();
        on_runtime(async move { inner.rename_table(&database, &old_name, &new_name).await }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn on_runtime_drives_future_without_ambient_tokio() {
        // Plain block_on: no ambient Tokio runtime, mirroring GPUI's executor.
        let result = futures::executor::block_on(on_runtime(async {
            // tokio::spawn panics outside a runtime context, so its success
            // proves on_runtime placed us inside the shared Tokio runtime.
            let value = tokio::spawn(async { 21 * 2 })
                .await
                .map_err(|error| anyhow::anyhow!(error))?;
            anyhow::Ok(value)
        }));
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn on_runtime_propagates_inner_error() {
        let result: Result<()> =
            futures::executor::block_on(on_runtime(async { anyhow::bail!("inner failure") }));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("inner failure"));
    }

    #[test]
    fn on_runtime_aborts_a_call_that_never_resolves_once_its_timeout_elapses() {
        // A future that never completes stands in for a query that hangs on
        // an unresponsive server. Uses a short custom timeout rather than the
        // real 30-minute QUERY_TIMEOUT so the test itself completes quickly.
        let result: Result<()> = futures::executor::block_on(on_runtime_with_timeout(
            std::time::Duration::from_millis(50),
            std::future::pending(),
        ));
        let message = result.unwrap_err().to_string();
        assert!(
            message.contains("timed out"),
            "expected a timeout error, got: {message}"
        );
    }

    struct TokioProbeProvider {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl TokioProbeProvider {
        async fn assert_in_tokio(&self, method: &str) -> Result<()> {
            self.calls.lock().expect("poisoned").push(method.to_owned());
            // Panics if not on a Tokio runtime, so reaching Ok proves the
            // RuntimeProvider wrapper routed this call onto Tokio.
            tokio::spawn(async {})
                .await
                .map_err(|error| anyhow::anyhow!(error))?;
            Ok(())
        }
    }

    #[async_trait]
    impl DbProvider for TokioProbeProvider {
        async fn ping(&self) -> Result<()> {
            self.assert_in_tokio("ping").await
        }
        async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
            self.assert_in_tokio("list_databases").await?;
            Ok(Vec::new())
        }
        async fn list_tables(&self, _database: &str) -> Result<Vec<TableInfo>> {
            self.assert_in_tokio("list_tables").await?;
            Ok(Vec::new())
        }
        async fn describe_table(&self, _database: &str, _table: &str) -> Result<Vec<ColumnInfo>> {
            self.assert_in_tokio("describe_table").await?;
            Ok(Vec::new())
        }
        async fn execute_query(&self, _database: &str, _sql: &str) -> Result<QueryResult> {
            self.assert_in_tokio("execute_query").await?;
            Ok(QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected: 0,
                execution_time_ms: 0,
            })
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> Result<String> {
            self.assert_in_tokio("get_table_ddl").await?;
            Ok(String::new())
        }
    }

    #[test]
    fn runtime_provider_routes_calls_through_tokio() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let probe = Arc::new(TokioProbeProvider {
            calls: calls.clone(),
        });
        let provider = RuntimeProvider::new(probe);

        // block_on with no ambient Tokio runtime: every delegated call would
        // panic unless RuntimeProvider moved it onto the shared runtime.
        futures::executor::block_on(async {
            provider.ping().await.expect("ping");
            provider.list_databases().await.expect("list_databases");
            provider.list_tables("db").await.expect("list_tables");
            provider
                .describe_table("db", "t")
                .await
                .expect("describe_table");
            provider
                .execute_query("db", "SELECT 1")
                .await
                .expect("query");
        });

        let recorded = calls.lock().expect("poisoned").clone();
        assert_eq!(
            recorded,
            vec![
                "ping",
                "list_databases",
                "list_tables",
                "describe_table",
                "execute_query",
            ]
        );
    }
}
