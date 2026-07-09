use std::future::Future;
use std::sync::OnceLock;

use anyhow::Result;
use tokio::runtime::Runtime;

fn runtime() -> Result<&'static Runtime> {
    static RUNTIME: OnceLock<std::result::Result<Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("api-client-tokio")
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!("failed to initialize network runtime: {error}"))
}

/// Drives `future` to completion on a dedicated Tokio runtime.
///
/// `tonic`'s HTTP/2 transport and `reqwest`'s DNS resolver both need a
/// Tokio reactor for their socket I/O. GPUI's executor is not Tokio, so
/// gRPC and HTTP calls must run here instead of on the calling executor --
/// otherwise they panic with "there is no reactor running" (mirrors
/// `db_client::runtime::on_runtime`, which solves the identical problem for
/// sqlx/redis).
pub async fn on_network_runtime<T, F>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let handle = runtime()?.handle().clone();
    match handle.spawn(future).await {
        Ok(result) => result,
        Err(join_error) => Err(anyhow::anyhow!("network task failed: {join_error}")),
    }
}

/// Fire-and-forget variant of [`on_network_runtime`] for long-lived work (a
/// streaming call pumping responses into a channel) that the caller does
/// not want to block on. Errors from `future` must be reported through
/// whatever channel/sink the caller gave it -- there is nothing here to
/// propagate a return value to.
pub fn spawn_detached_on_network_runtime<F>(future: F) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    runtime()?.handle().spawn(future);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_network_runtime_drives_future_without_ambient_tokio() {
        let result = futures::executor::block_on(on_network_runtime(async {
            let value = tokio::spawn(async { 21 * 2 })
                .await
                .map_err(|error| anyhow::anyhow!(error))?;
            anyhow::Ok(value)
        }));
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn on_network_runtime_propagates_inner_error() {
        let result: Result<()> = futures::executor::block_on(on_network_runtime(async {
            anyhow::bail!("inner failure")
        }));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("inner failure"));
    }
}
