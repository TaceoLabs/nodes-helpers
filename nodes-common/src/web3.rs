//! HTTP RPC provider utilities for interacting with Ethereum nodes.
//!
//! This module provides configurable HTTP RPC providers built on top of
//! [`alloy`] transports. It supports:
//!
//! - HTTP RPC with automatic retry and exponential backoff
//! - Multiple HTTP endpoints with automatic failover
//! - Optional wallet integration for transaction signing
//!
//! Use [`HttpRpcProviderBuilder`] to build an HTTP RPC provider.
//! HTTP transports are wrapped with retry and fallback layers to improve
//! reliability when interacting with RPC endpoints.
use std::{
    num::NonZeroUsize,
    task::{Context, Poll},
    time::Duration,
};

use alloy::{
    network::EthereumWallet,
    primitives::ChainId,
    providers::{
        DynProvider, Provider, ProviderBuilder,
        fillers::{BlobGasFiller, ChainIdFiller, NonceManager, SimpleNonceManager},
    },
    rpc::{
        client::RpcClient,
        json_rpc::{RequestPacket, ResponsePacket},
    },
    transports::{
        RpcError, Transport, TransportError, TransportErrorKind, TransportFut,
        http::{
            Http,
            reqwest::{self, Url},
        },
        layers::{FallbackLayer, OrRetryPolicyFn, RateLimitRetryPolicy, RetryPolicy},
    },
};
use backon::{ExponentialBuilder, Retryable as _};
use serde::Deserialize;
use tower::{Layer, Service};

use crate::Environment;

pub mod erc165;

/// A dedicated HTTP RPC provider.
///
/// This provider should be used for regular RPC calls, transaction
/// submission, and helpers such as ERC-165 queries.
#[derive(Clone)]
pub struct HttpRpcProvider(DynProvider);

/// Configuration for building an [`HttpRpcProvider`].
///
/// Multiple HTTP endpoints can be provided to enable automatic failover.
/// Retry behavior can be tuned via [`RetryPolicyConfig`].
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct HttpRpcProviderConfig {
    /// List of HTTP RPC endpoints used for requests.
    ///
    /// Uses alloy's [`FallbackService`](https://docs.rs/alloy/latest/alloy/providers/transport/layers/struct.FallbackLayer.html) and configures each endpoint as one potential transport.
    pub http_urls: Vec<Url>,
    /// Optional chain ID used by the provider.
    ///
    /// If provided, the [`ChainIdFiller`] will automatically populate
    /// transactions with this value.
    #[serde(default)]
    pub chain_id: Option<ChainId>,
    /// The timeout for HTTP requests to the RPC.
    ///
    /// Defaults to **10 seconds**.
    #[serde(default = "HttpRpcProviderConfig::default_timeout")]
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    /// The poll interval for the confirmation heartbeat for alloy.
    ///
    /// Uses alloy's default setting if omitted. For `dev` environment 250ms
    /// and for all other environments 7s.
    #[serde(default)]
    #[serde(with = "humantime_serde")]
    pub confirmations_poll_interval: Option<Duration>,
    /// Retry configuration applied to RPC requests.
    #[serde(default)]
    pub retry_policy_config: RetryPolicyConfig,
}

/// Configuration for RPC retry behavior.
///
/// Requests that fail with retryable errors will be retried using
/// exponential backoff.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct RetryPolicyConfig {
    /// Minimum delay between retries.
    ///
    /// Defaults to **1 second**.
    #[serde(default = "RetryPolicyConfig::default_min_delay")]
    #[serde(with = "humantime_serde")]
    pub min_delay: Duration,

    /// Maximum delay between retries.
    ///
    /// Defaults to **8 seconds**.
    #[serde(default = "RetryPolicyConfig::default_max_delay")]
    #[serde(with = "humantime_serde")]
    pub max_delay: Duration,

    /// Maximum number of retry attempts.
    ///
    /// Defaults to **5 retries**.
    #[serde(default = "RetryPolicyConfig::default_max_times")]
    pub max_times: usize,
}

impl HttpRpcProviderConfig {
    /// Creates a new configuration using default retry settings.
    #[must_use]
    pub fn with_default_values(http_urls: Vec<Url>) -> Self {
        Self {
            http_urls,
            timeout: Self::default_timeout(),
            confirmations_poll_interval: None,
            chain_id: None,
            retry_policy_config: RetryPolicyConfig::default(),
        }
    }

    /// Default timeout for HTTP requests to the RPC: 10 seconds
    fn default_timeout() -> Duration {
        Duration::from_secs(10)
    }
}

impl RetryPolicyConfig {
    /// Default minimum delay between retries: 1 second
    fn default_min_delay() -> Duration {
        Duration::from_secs(1)
    }

    /// Default maximum delay between retries: 8 seconds
    fn default_max_delay() -> Duration {
        Duration::from_secs(8)
    }

    /// Default maximum number of retry attempts: 5
    fn default_max_times() -> usize {
        5
    }

    /// Initialize a `RetryPolicyConfig` with default values
    fn with_default_values() -> Self {
        Self {
            min_delay: Self::default_min_delay(),
            max_delay: Self::default_max_delay(),
            max_times: Self::default_max_times(),
        }
    }
}

impl Default for RetryPolicyConfig {
    fn default() -> Self {
        Self::with_default_values()
    }
}

fn build_transport_stack<S>(
    transports: Vec<S>,
    retry_policy_config: &RetryPolicyConfig,
) -> impl Transport + Clone
where
    S: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
{
    let retry_layer = RetryLayer::new(http_retry_policy(), retry_policy_config);
    let retrying_transports = transports
        .into_iter()
        .map(|transport| retry_layer.layer(transport))
        .collect::<Vec<_>>();
    let transport_count =
        NonZeroUsize::new(retrying_transports.len()).expect("transport stack must not be empty");

    // Retry each transport before fallback so JSON-RPC error responses cannot
    // win the fallback race against a slower healthy endpoint.
    FallbackLayer::default()
        .with_active_transport_count(transport_count)
        .layer(retrying_transports)
}

fn http_retry_policy() -> OrRetryPolicyFn {
    // Configure retry policy.
    //
    // The RateLimitRetryPolicy already handles 503 Service Unavailable and other common RPC errors.
    // We additionally check for other common transient errors:
    //   - 408 Request Timeout
    //   - 502 Bad Gateway
    //   - 504 Gateway Timeout
    RateLimitRetryPolicy::default().or(|error: &TransportError| match error {
        RpcError::Transport(TransportErrorKind::HttpError(e)) => {
            matches!(e.status, 408 | 502 | 504)
        }
        RpcError::Transport(kind) => kind
            .as_custom()
            .and_then(|error| error.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout),
        _ => false,
    })
}

/// Builder for constructing an [`HttpRpcProvider`].
///
/// The builder configures retry behavior, fallback transports, optional
/// wallet integration, and provider fillers before creating the provider.
pub struct HttpRpcProviderBuilder {
    http_urls: Vec<Url>,
    retry_policy_config: RetryPolicyConfig,
    chain_id: Option<ChainId>,
    timeout: Duration,
    confirmations_poll_interval: Option<Duration>,
    is_local: bool,
    wallet: Option<EthereumWallet>,
}

impl From<HttpRpcProviderConfig> for HttpRpcProviderBuilder {
    fn from(value: HttpRpcProviderConfig) -> Self {
        Self::from(&value)
    }
}

impl From<&HttpRpcProviderConfig> for HttpRpcProviderBuilder {
    fn from(value: &HttpRpcProviderConfig) -> Self {
        Self::with_config(value)
    }
}

impl HttpRpcProviderBuilder {
    /// Creates a new builder from the given configuration.
    ///
    /// # Panics
    ///
    /// Panics if `config.http_urls` is empty. At least one HTTP endpoint
    /// must be provided so that a transport stack can be constructed.
    #[must_use]
    pub fn with_config(config: &HttpRpcProviderConfig) -> Self {
        assert!(!config.http_urls.is_empty(), "http URLs must not be empty");
        Self {
            http_urls: config.http_urls.clone(),
            retry_policy_config: config.retry_policy_config.clone(),
            timeout: config.timeout,
            chain_id: config.chain_id,
            is_local: false,
            wallet: None,
            confirmations_poll_interval: config.confirmations_poll_interval,
        }
    }

    /// Creates a new builder using default retry settings.
    ///
    /// # Example
    ///
    /// ```
    /// use alloy::transports::http::reqwest::Url;
    /// use taceo_nodes_common::web3::HttpRpcProviderBuilder;
    ///
    /// let builder = HttpRpcProviderBuilder::with_default_values(vec![
    ///     Url::parse("http://127.0.0.1:8545").unwrap(),
    /// ]);
    /// ```
    #[must_use]
    pub fn with_default_values(http_urls: Vec<Url>) -> Self {
        Self::with_config(&HttpRpcProviderConfig::with_default_values(http_urls))
    }

    /// Configures the environment used by the provider.
    #[must_use]
    pub fn environment(mut self, environment: Environment) -> Self {
        self.is_local = environment.is_dev();
        self
    }

    /// Sets the timeout for HTTP RPC requests.
    #[must_use]
    pub fn http_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the poll interval in which alloy fetches blocks for transaction confirmations.
    #[must_use]
    pub fn confirmations_poll_interval(mut self, confirmations_poll_interval: Duration) -> Self {
        self.confirmations_poll_interval = Some(confirmations_poll_interval);
        self
    }

    /// Sets the chain ID used by the provider.
    #[must_use]
    pub fn chain_id(mut self, chain_id: ChainId) -> Self {
        self.chain_id = Some(chain_id);
        self
    }

    /// Configures the retry behavior for HTTP RPC requests.
    #[must_use]
    pub fn retry_policy(mut self, retry_policy_config: RetryPolicyConfig) -> Self {
        self.retry_policy_config = retry_policy_config;
        self
    }

    /// Adds a wallet used for signing transactions.
    #[must_use]
    pub fn wallet(mut self, wallet: EthereumWallet) -> Self {
        self.wallet = Some(wallet);
        self
    }

    /// Builds the [`HttpRpcProvider`].
    ///
    /// Uses [`SimpleNonceManager::default()`] for nonce management. Use
    /// [`Self::build_with_nonce_manager`] to provide a custom nonce manager.
    ///
    /// # Errors
    ///
    /// Returns a [`TransportError`] if the HTTP transport stack cannot be
    /// initialized, including failures to create the underlying reqwest client.
    pub fn build(self) -> Result<HttpRpcProvider, TransportError> {
        self.build_with_nonce_manager(SimpleNonceManager::default())
    }

    /// Builds the [`HttpRpcProvider`] using the provided nonce manager.
    ///
    /// This allows callers to customize how transaction nonces are tracked
    /// while keeping the rest of the builder configuration unchanged.
    ///
    /// # Errors
    ///
    /// Returns a [`TransportError`] if the HTTP transport stack cannot be
    /// initialized, including failures to create the underlying reqwest client.
    pub fn build_with_nonce_manager<N: NonceManager + 'static>(
        self,
        nonce_manager: N,
    ) -> Result<HttpRpcProvider, TransportError> {
        let HttpRpcProviderBuilder {
            http_urls,
            retry_policy_config,
            chain_id,
            timeout,
            is_local,
            wallet,
            confirmations_poll_interval,
        } = self;

        let reqwest = reqwest::ClientBuilder::new()
            .timeout(timeout)
            .build()
            .map_err(TransportErrorKind::custom)?;

        let transports = http_urls
            .into_iter()
            .map(|url| Http::with_client(reqwest.clone(), url))
            .collect::<Vec<_>>();
        let transport = build_transport_stack(transports, &retry_policy_config);

        let client = RpcClient::builder().transport(transport, is_local);
        let client = if let Some(confirmations_poll_interval) = confirmations_poll_interval {
            client.with_poll_interval(confirmations_poll_interval)
        } else {
            client
        };

        let http_provider_builder = ProviderBuilder::new()
            .filler(ChainIdFiller::new(chain_id))
            .filler(BlobGasFiller::default())
            .with_nonce_management(nonce_manager)
            .with_gas_estimation();

        let provider = if let Some(wallet) = wallet {
            http_provider_builder
                .wallet(wallet)
                .connect_client(client)
                .erased()
        } else {
            http_provider_builder.connect_client(client).erased()
        };

        Ok(HttpRpcProvider(provider))
    }
}

impl HttpRpcProvider {
    /// Returns the HTTP RPC provider.
    #[must_use]
    pub fn inner(&self) -> DynProvider {
        self.0.clone()
    }
}

#[derive(Debug, Clone)]
struct RetryLayer {
    policy: OrRetryPolicyFn,
    backoff: ExponentialBuilder,
}

impl RetryLayer {
    /// Creates a new retry layer using the provided retry policy and configuration.
    ///
    /// The retry behavior is implemented using exponential backoff with jitter.
    ///
    /// The following parameters are taken from [`RetryPolicyConfig`]:
    ///
    /// - minimum retry delay
    /// - maximum retry delay
    /// - maximum number of retry attempts
    pub fn new(policy: OrRetryPolicyFn, config: &RetryPolicyConfig) -> Self {
        let backoff = ExponentialBuilder::default()
            .with_min_delay(config.min_delay)
            .with_max_delay(config.max_delay)
            .with_max_times(config.max_times)
            .with_jitter();
        Self { policy, backoff }
    }
}

impl<S> Layer<S> for RetryLayer {
    type Service = RetryService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RetryService {
            inner,
            policy: self.policy.clone(),
            backoff: self.backoff,
        }
    }
}

/// Tower service that wraps each request in a retry loop with exponential backoff.
#[derive(Debug, Clone)]
struct RetryService<S> {
    inner: S,
    policy: OrRetryPolicyFn,
    backoff: ExponentialBuilder,
}

impl<S> Service<RequestPacket> for RetryService<S>
where
    S: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        let service = self.clone();
        let backoff = self.backoff;
        let policy = self.policy.clone();

        Box::pin(async move {
            (|| service.clone().call_and_parse_error(request.clone()))
                .retry(backoff)
                .sleep(tokio::time::sleep)
                .when(|e| policy.should_retry(e))
                .notify(|_, duration| tracing::debug!("Retrying RPC request after: {duration:?}"))
                // Adjust the backoff duration based on the policy and the current hint:
                // - If `dur` is `None`, we stop retrying (max attempts reached).
                // - If `dur` is `Some(d)` and the policy provides a backoff hint, use the policy hint.
                // - If `dur` is `Some(d)` and the policy hint is `None`, use the original `d`.
                .adjust(|e, dur| dur.and_then(|d| policy.backoff_hint(e).or(Some(d))))
                .await
        })
    }
}

impl<S> RetryService<S>
where
    S: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
{
    async fn call_and_parse_error(
        mut self,
        request: RequestPacket,
    ) -> Result<ResponsePacket, RpcError<TransportErrorKind>> {
        let resp = self.inner.call(request).await?;
        if let Some(e) = resp.as_error() {
            Err(TransportError::ErrorResp(e.to_owned()))
        } else {
            Ok(resp)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
