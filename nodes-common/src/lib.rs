use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use aws_config::Region;
use aws_sdk_secretsmanager::config::Credentials;
use tokio::signal;
use tokio_util::sync::CancellationToken;

pub use git_version;

/// Macro to generate version information including the crate name, version, and git hash.
#[macro_export]
macro_rules! version_info {
    () => {
        format!(
            "{} {} ({})",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            option_env!("GIT_HASH")
                .unwrap_or($crate::git_version::git_version!(fallback = "UNKNOWN"))
        )
    };
}

/// Spawns a shutdown task and creates an associated [`CancellationToken`](https://docs.rs/tokio-util/latest/tokio_util/sync/struct.CancellationToken.html). This task will complete when either the provided `shutdown_signal` futures completes or if some other tasks cancels the shutdown token. The associated shutdown token will be cancelled either way.
///
/// Waiting for the shutdown token is the preferred way to wait for termination.
pub fn spawn_shutdown_task(
    shutdown_signal: impl Future<Output = ()> + Send + 'static,
) -> (CancellationToken, Arc<AtomicBool>) {
    let cancellation_token = CancellationToken::new();
    let is_graceful = Arc::new(AtomicBool::new(false));
    let task_token = cancellation_token.clone();
    tokio::spawn({
        let is_graceful = Arc::clone(&is_graceful);
        async move {
            tokio::select! {
                _ = shutdown_signal => {
                    tracing::info!("received graceful shutdown");
                    is_graceful.store(true, Ordering::Relaxed);
                    task_token.cancel();
                }
                _ = task_token.cancelled() => {}
            }
        }
    });
    (cancellation_token, is_graceful)
}

/// The default shutdown signal for the oprf-service. Triggered when pressing CTRL+C on most systems.
pub async fn default_shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Creates an AWS SDK configuration for connecting to a LocalStack instance.
///
/// This function is designed to facilitate testing and development by configuring
/// an AWS SDK client to connect to a LocalStack instance. It sets the region to
/// `us-east-1` and uses static test credentials. The endpoint URL can be customized
/// via the `TEST_AWS_ENDPOINT_URL` environment variable; if not set, it defaults
/// to `http://localhost:4566`.
pub async fn localstack_aws_config() -> aws_config::SdkConfig {
    let region_provider = Region::new("us-east-1");
    let credentials = Credentials::new("test", "test", None, None, "Static");
    // in case we don't want the standard url, we can configure it via the environment
    aws_config::from_env()
        .region(region_provider)
        .endpoint_url(
            std::env::var("TEST_AWS_ENDPOINT_URL").unwrap_or("http://localhost:4566".to_string()),
        )
        .credentials_provider(credentials)
        .load()
        .await
}
