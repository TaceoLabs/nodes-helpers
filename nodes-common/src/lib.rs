#![deny(missing_docs)]
#![deny(clippy::all, clippy::pedantic)]
#![deny(
    clippy::allow_attributes_without_reason,
    clippy::assertions_on_result_states,
    clippy::dbg_macro,
    clippy::decimal_literal_representation,
    clippy::exhaustive_enums,
    clippy::exhaustive_structs,
    clippy::iter_over_hash_type,
    clippy::let_underscore_must_use,
    clippy::missing_assert_message,
    clippy::print_stderr,
    clippy::print_stdout,
    clippy::undocumented_unsafe_blocks,
    clippy::unnecessary_safety_comment,
    clippy::unwrap_used
)]
#![allow(clippy::needless_pass_by_value, reason = "Needed for axum")]
//! Common utilities for MPC-node services.
//!
//! This crate provides building blocks shared across nodes in the MPC network.
//!
//! * [`Environment`] – represents the deployment environment (prod / staging / test).
//! * [`StartedServices`] – tracks whether all async background services have started,
//!   used to drive the `/health` endpoint.
//! * [`spawn_shutdown_task`] / [`default_shutdown_signal`] – wiring for graceful shutdown
//!   via `CTRL+C` or `SIGTERM`.
//! * [`version_info!`] – macro that returns a version string containing the crate name,
//!   semver version, and git hash.
//! # Optional Features
//!
//! * `api` (enabled by default) – exposes `/health` and `/version` Axum endpoints.
//! * `serde` (enabled by default) – ser/de implementation for [`Environment`].
//! * `aws` (enabled by default) – adds a method to create a localstack configuration used for testing
use core::fmt;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use tokio::signal;
use tokio_util::sync::CancellationToken;

pub use git_version;

#[cfg(feature = "api")]
/// See [`api::routes`] and [`api::routes_with_services`].
pub mod api;
#[cfg(feature = "postgres")]
pub mod postgres;

/// The environment the service is running in.
///
/// Main usage for the `Environment` is to call
/// [`Environment::assert_is_dev`]. Services that are intended
/// for `dev` only (like local secret-manager,...)
/// shall assert that they are called from the `dev` environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(
    clippy::exhaustive_enums,
    reason = "We only expect those four environments at the moment. Changing that is a breaking change."
)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum Environment {
    /// Production environment.
    Prod,
    /// Staging environment.
    Stage,
    /// Test environment. Used for deployed test nets not for local testing. Use `Dev` instead for local testing.
    Test,
    /// Local dev environment.
    Dev,
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let str = match self {
            Environment::Prod => "prod",
            Environment::Stage => "stage",
            Environment::Test => "test",
            Environment::Dev => "dev",
        };
        f.write_str(str)
    }
}

impl Environment {
    /// Asserts that the environment is the dev environment.
    ///
    /// # Panics
    ///
    /// Panics with `"Is not dev environment"` if `self` is not `Environment::Dev`.
    pub fn assert_is_dev(&self) {
        assert!(self.is_dev(), "Is not dev environment");
    }

    /// Returns `true` if the environment is the test environment.
    #[must_use]
    pub fn is_dev(&self) -> bool {
        matches!(self, Environment::Dev)
    }

    /// Returns `true` if the environment is not the test environment.
    #[must_use]
    pub fn is_not_dev(&self) -> bool {
        !self.is_dev()
    }
}

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

/// A struct that keeps track of the health of all async services started by the service.
///
/// Relevant for the `/health` route. Implementations should call [`StartedServices::new_service`] for their services and set the bool to `true` if the service started successfully.
#[derive(Debug, Clone, Default)]
pub struct StartedServices {
    external_service: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
}

impl StartedServices {
    /// Initializes all services as not started.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a new external service to the bookkeeping struct.
    ///
    /// Implementations should call this method for every async task that they start. The returned `AtomicBool` should then be set to `true` if the service is ready.
    #[must_use]
    #[allow(clippy::missing_panics_doc, reason = "Ok to panic for lock poisoning")]
    pub fn new_service(&self) -> Arc<AtomicBool> {
        let service = Arc::new(AtomicBool::default());
        self.external_service
            .lock()
            .expect("Not poisoned")
            .push(Arc::clone(&service));
        service
    }

    /// Returns `true` if all services did start. If there are no services started, this will also return `true`.
    #[must_use]
    #[allow(clippy::missing_panics_doc, reason = "Ok to panic for lock poisoning")]
    pub fn all_started(&self) -> bool {
        self.external_service
            .lock()
            .expect("Not poisoned")
            .iter()
            .all(|service| service.load(Ordering::Relaxed))
    }
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
    tokio::task::spawn({
        let is_graceful = Arc::clone(&is_graceful);
        async move {
            let _drop_guard = task_token.drop_guard_ref();
            tokio::select! {
                () = shutdown_signal => {
                    tracing::info!("received graceful shutdown");
                    is_graceful.store(true, Ordering::Relaxed);
                    task_token.cancel();
                }
                () = task_token.cancelled() => {}
            }
        }
    });
    (cancellation_token, is_graceful)
}

/// Returns a future that completes when the application should shut down.
///
/// On most systems, it completes when the user presses `CTRL+C`.
/// On Unix platforms, it also responds to the `SIGTERM` signal.
///
/// # Panics
///
/// Panics if the `CTRL+C` or `SIGTERM` signal handlers cannot be installed.
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
        () = ctrl_c => {},
        () = terminate => {},
    }
}

#[cfg(feature = "aws")]
/// Creates an AWS SDK configuration for connecting to a `LocalStack` instance.
///
/// This function is designed to facilitate testing and development by configuring
/// an AWS SDK client to connect to a `LocalStack` instance. It sets the region to
/// `us-east-1` and uses static test credentials. The endpoint URL can be customized
/// via the `TEST_AWS_ENDPOINT_URL` environment variable; if not set, it defaults
/// to `http://localhost:4566`.
pub async fn localstack_aws_config() -> aws_config::SdkConfig {
    use aws_config::Region;
    use aws_sdk_secretsmanager::config::Credentials;
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
