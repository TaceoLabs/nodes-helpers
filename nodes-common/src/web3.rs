//! RPC provider utilities for interacting with Ethereum nodes.
//!
//! This module provides a configurable RPC provider built on top of
//! [`alloy`] transports. It supports:
//!
//! - HTTP RPC with automatic retry and exponential backoff
//! - Multiple HTTP endpoints with automatic failover
//! - WebSocket RPC for subscriptions
//! - Optional wallet integration for transaction signing
//!
//! The [`RpcProviderBuilder`] constructs a [`RpcProvider`] using the provided
//! [`RpcProviderConfig`]. HTTP transports are wrapped with retry and fallback
//! layers to improve reliability when interacting with RPC endpoints.
//!
//! ⚠️ **Attention**
//!
//! The WebSocket RPC connection should **only be used for subscriptions**
//! (e.g. `eth_subscribe`). The underlying [`alloy`] WebSocket client
//! maintains a heartbeat to keep the connection alive.
//!
//! Using the WebSocket provider for normal RPC requests may lead to
//! **increased infrastructure costs**, as some RPC providers meter
//! WebSocket traffic differently from HTTP requests.
use std::{
    num::NonZeroUsize,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use alloy::{
    network::EthereumWallet,
    primitives::ChainId,
    providers::{
        DynProvider, Provider, ProviderBuilder, WsConnect,
        fillers::{BlobGasFiller, ChainIdFiller},
    },
    rpc::{client::RpcClient, json_rpc::RequestPacket},
    transports::{
        RpcError, TransportError, TransportErrorKind,
        http::{
            Http,
            reqwest::{self, Url},
        },
        layers::{FallbackLayer, OrRetryPolicyFn, RateLimitRetryPolicy, RetryPolicy},
    },
};
use backon::{BackoffBuilder as _, ExponentialBuilder, Retryable as _};
use serde::Deserialize;
use tower::{Layer, Service, ServiceBuilder};

use crate::Environment;

/// A wrapper around HTTP and WebSocket providers.
///
/// The HTTP provider is intended for standard RPC calls and transaction
/// submission, while the WebSocket provider is primarily used for
/// subscriptions and event streams.
///
/// ⚠️ **Attention**
///
/// The WebSocket provider should **only be used for subscriptions**
/// (e.g. `eth_subscribe`). Alloy maintains an internal heartbeat on
/// WebSocket connections to keep them alive. Using `WebSockets` for
/// ordinary RPC calls may lead to **increased RPC provider costs**.
///
/// For regular RPC requests prefer [`RpcProvider::http`].
#[derive(Clone)]
pub struct RpcProvider {
    http_provider: DynProvider,
    ws_provider: DynProvider,
}

/// Configuration for building an [`RpcProvider`].
///
/// The configuration specifies the HTTP RPC endpoints used for requests
/// as well as the WebSocket endpoint used for subscriptions. Multiple
/// HTTP endpoints can be provided to enable automatic failover.
///
/// Retry behavior can be tuned via [`RetryPolicyConfig`].
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct RpcProviderConfig {
    /// List of HTTP RPC endpoints used for requests.
    ///
    /// Uses alloy's [`FallbackService`](https://docs.rs/alloy/latest/alloy/providers/transport/layers/struct.FallbackLayer.html) and configures each endpoint as one potential transport.
    pub http_urls: Vec<Url>,
    /// WebSocket RPC endpoint used for subscriptions.
    pub ws_url: Url,
    /// Optional chain ID used by the provider.
    ///
    /// If provided, the [`ChainIdFiller`] will automatically populate
    /// transactions with this value.
    pub chain_id: Option<ChainId>,
    /// The timeout for HTTP requests to the RPC.
    ///
    /// Defaults to **10 seconds**.
    #[serde(default = "RpcProviderConfig::default_timeout")]
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
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

impl RpcProviderConfig {
    /// Creates a new configuration using default retry settings.
    #[must_use]
    pub fn with_default_values(http_urls: Vec<Url>, ws_url: Url) -> Self {
        Self {
            http_urls,
            ws_url,
            timeout: Self::default_timeout(),
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

/// Builder for constructing an [`RpcProvider`].
///
/// The builder configures retry behavior, fallback transports, optional
/// wallet integration, and provider fillers before establishing
/// connections to the RPC endpoints.
pub struct RpcProviderBuilder {
    http_urls: Vec<Url>,
    ws_rpc_url: Url,
    retry_policy_config: RetryPolicyConfig,
    chain_id: Option<ChainId>,
    timeout: Duration,
    is_local: bool,
    wallet: Option<EthereumWallet>,
}

impl From<RpcProviderConfig> for RpcProviderBuilder {
    fn from(value: RpcProviderConfig) -> Self {
        Self::from(&value)
    }
}

impl From<&RpcProviderConfig> for RpcProviderBuilder {
    fn from(value: &RpcProviderConfig) -> Self {
        Self::with_config(value)
    }
}

impl RpcProviderBuilder {
    /// Creates a new builder from the given configuration.
    ///
    /// This only stores the configuration. All transport, retry, and
    /// fallback logic is constructed during [`Self::build`].
    ///
    /// # Panics
    ///
    /// Panics if `config.http_urls` is empty. At least one HTTP endpoint
    /// must be provided so that a transport stack can be constructed.
    #[must_use]
    pub fn with_config(config: &RpcProviderConfig) -> Self {
        assert!(!config.http_urls.is_empty(), "http URLs must not be empty");
        Self {
            http_urls: config.http_urls.clone(),
            ws_rpc_url: config.ws_url.clone(),
            retry_policy_config: config.retry_policy_config.clone(),
            timeout: config.timeout,
            chain_id: config.chain_id,
            is_local: false,
            wallet: None,
        }
    }

    /// Creates a new builder using default retry settings from the configuration.
    ///
    /// # Arguments
    ///
    /// * `http_urls` - A vector of HTTP RPC endpoint URLs. These endpoints will be
    ///   used with automatic failover via a fallback layer.
    /// * `ws_url` - A WebSocket RPC endpoint used for subscriptions only.
    ///
    /// # Behavior
    ///
    /// This function:
    /// - Creates a `RpcProviderConfig` with the provided URLs.
    /// - Applies default values for the retry policy (`min_delay = 1s`, `max_delay = 8s`, `max_times = 5`).
    /// - Initializes the builder with the default configuration.
    ///
    /// # Notes
    ///
    /// ⚠️ The `ws_url` should **only** be used for subscriptions.
    ///
    /// # Example
    ///
    /// ```
    /// use alloy::transports::http::reqwest::Url;
    /// use taceo_nodes_common::web3::RpcProviderBuilder;
    ///
    /// let builder = RpcProviderBuilder::with_default_values(
    ///     vec![
    ///         Url::parse("http://127.0.0.1:8545").unwrap()
    ///     ],
    ///     Url::parse("ws://127.0.0.1:8546").unwrap(),
    /// );
    /// ```
    #[must_use]
    pub fn with_default_values(http_urls: Vec<Url>, ws_url: Url) -> Self {
        Self::with_config(&RpcProviderConfig::with_default_values(http_urls, ws_url))
    }

    /// Configures the environment used by the provider.
    ///
    /// When running in a development environment, the provider enables
    /// additional local-node optimizations.
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

    /// Sets the chain ID used by the provider.
    ///
    /// The chain ID will be automatically applied to all transactions via
    /// the [`ChainIdFiller`].
    #[must_use]
    pub fn chain_id(mut self, chain_id: ChainId) -> Self {
        self.chain_id = Some(chain_id);
        self
    }

    /// Configures the retry behavior for HTTP RPC requests.
    ///
    /// The provided [`RetryPolicyConfig`] determines the minimum and maximum
    /// delay between retries, as well as the maximum number of retry attempts.
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

    /// Builds the [`RpcProvider`].
    ///
    /// This method constructs all runtime components including:
    ///
    /// - HTTP transports for each configured RPC endpoint
    /// - fallback transport layer for failover
    /// - retry layer with exponential backoff
    /// - HTTP provider for RPC requests
    /// - WebSocket provider for subscriptions
    ///
    /// # Errors
    ///
    /// Returns a [`TransportError`] if establishing the WebSocket connection
    /// fails or if the HTTP provider cannot be properly initialized.
    ///
    /// # Panics
    ///
    /// If the method fails to build the `reqwest` client for HTTP requests.
    /// This can happen due to:
    /// * a TLS backend cannot be initialized,
    /// * the resolver cannot load the system configuration.
    pub async fn build(self) -> Result<RpcProvider, TransportError> {
        let Self {
            http_urls,
            retry_policy_config,
            chain_id,
            timeout,
            is_local,
            wallet,
            ws_rpc_url,
        } = self;

        let reqwest = reqwest::ClientBuilder::new()
            .timeout(timeout)
            .build()
            .expect("Failed to build reqwest HTTP client");
        // Build HTTP transports
        let transports = http_urls
            .into_iter()
            .map(|url| Http::with_client(reqwest.clone(), url))
            .collect::<Vec<_>>();
        let transport_count =
            NonZeroUsize::try_from(transports.len()).expect("Checked non-empty in with_config");

        // Configure fallback layer
        let fallback_layer = FallbackLayer::default().with_active_transport_count(transport_count);

        // Configure retry policy
        let retry_policy =
            RateLimitRetryPolicy::default().or(|error: &TransportError| match error {
                RpcError::Transport(TransportErrorKind::HttpError(e)) => {
                    matches!(e.status, 408 | 502 | 504)
                }
                _ => false,
            });

        let retry_layer = RetryLayer::new(retry_policy, &retry_policy_config);

        // Build transport stack
        let transport = ServiceBuilder::new()
            .layer(retry_layer)
            .layer(fallback_layer)
            .service(transports);

        let client = RpcClient::builder().transport(transport, is_local);

        // Configure HTTP provider
        let http_provider_builder = ProviderBuilder::new()
            .filler(ChainIdFiller::new(chain_id))
            .filler(BlobGasFiller::default())
            .with_simple_nonce_management()
            .with_gas_estimation();

        let http_provider = if let Some(wallet) = wallet {
            http_provider_builder
                .wallet(wallet)
                .connect_client(client)
                .erased()
        } else {
            http_provider_builder.connect_client(client).erased()
        };

        // Build WebSocket provider
        let ws_provider = ProviderBuilder::new()
            .connect_ws(WsConnect::new(ws_rpc_url))
            .await?
            .erased();

        Ok(RpcProvider {
            http_provider,
            ws_provider,
        })
    }
}
impl RpcProvider {
    /// Returns the HTTP RPC provider.
    ///
    /// This provider should be used for **all regular RPC calls** and
    /// transaction submission.
    #[must_use]
    pub fn http(&self) -> DynProvider {
        self.http_provider.clone()
    }

    /// Returns the WebSocket RPC provider.
    ///
    /// ⚠️ This provider should only be used for **subscriptions**
    /// such as `eth_subscribe`.
    #[must_use]
    pub fn subscriptions(&self) -> DynProvider {
        self.ws_provider.clone()
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
    S: Service<
            RequestPacket,
            Response = alloy::rpc::json_rpc::ResponsePacket,
            Error = TransportError,
        > + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
{
    type Response = alloy::rpc::json_rpc::ResponsePacket;
    type Error = TransportError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        let service = self.clone();
        let backoff = self.backoff;
        let policy = self.policy.clone();

        Box::pin(async move {
            (|| service.clone().call_and_parse_error(request.clone()))
                .retry(backoff.build())
                .sleep(tokio::time::sleep)
                .when(|e| policy.should_retry(e))
                .notify(|_, duration| tracing::debug!("Retrying RPC request after: {duration:?}"))
                .adjust(|e, dur| policy.backoff_hint(e).or(dur))
                .await
        })
    }
}

impl<S> RetryService<S>
where
    S: Service<
            RequestPacket,
            Response = alloy::rpc::json_rpc::ResponsePacket,
            Error = TransportError,
        > + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
{
    async fn call_and_parse_error(
        mut self,
        request: RequestPacket,
    ) -> Result<alloy::rpc::json_rpc::ResponsePacket, RpcError<TransportErrorKind>> {
        let resp = self.inner.call(request.clone()).await?;
        if let Some(e) = resp.as_error() {
            Err(TransportError::ErrorResp(e.clone()))
        } else {
            Ok(resp)
        }
    }
}
