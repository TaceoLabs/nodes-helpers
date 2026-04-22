use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use alloy::{
    node_bindings::{Anvil, AnvilInstance},
    providers::Provider,
    transports::{
        RpcError, TransportErrorKind,
        http::reqwest::{self, Url},
    },
};
use axum::{
    Router,
    body::{Body, Bytes, HttpBody},
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use http_body::Frame;
use tokio::net::TcpListener;

use crate::{
    Environment,
    web3::{HttpRpcProvider, HttpRpcProviderBuilder, HttpRpcProviderConfig},
};

#[derive(Debug)]
enum HttpRpcAction {
    Respond {
        status: u16,
        response_body: &'static str,
        delay: Duration,
    },
    Timeout {
        delay: Duration,
    },
    CloseConnection,
}

#[derive(Debug)]
struct HttpRpcStep {
    expected_method: &'static str,
    action: HttpRpcAction,
}

impl HttpRpcStep {
    fn ok(expected_method: &'static str, response_body: &'static str) -> Self {
        Self {
            expected_method,
            action: HttpRpcAction::Respond {
                status: 200,
                response_body,
                delay: Duration::ZERO,
            },
        }
    }

    fn status(expected_method: &'static str, status: u16, response_body: &'static str) -> Self {
        Self {
            expected_method,
            action: HttpRpcAction::Respond {
                status,
                response_body,
                delay: Duration::ZERO,
            },
        }
    }

    fn close_connection(expected_method: &'static str) -> Self {
        Self {
            expected_method,
            action: HttpRpcAction::CloseConnection,
        }
    }

    fn timeout(expected_method: &'static str, delay: Duration) -> Self {
        Self {
            expected_method,
            action: HttpRpcAction::Timeout { delay },
        }
    }

    fn with_delay(mut self, delay: Duration) -> Self {
        if let HttpRpcAction::Respond {
            delay: step_delay, ..
        } = &mut self.action
        {
            *step_delay = delay;
        } else {
            panic!("only response steps can be delayed");
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WithWallet {
    Yes,
    No,
}

pub(crate) fn http_fixture(with_wallet: WithWallet) -> (AnvilInstance, HttpRpcProvider) {
    let anvil = Anvil::new().spawn();
    let mut http_provider_builder =
        HttpRpcProviderBuilder::with_config(&HttpRpcProviderConfig::with_default_values(vec![
            anvil.endpoint_url(),
        ]))
        .environment(Environment::Dev);
    if with_wallet == WithWallet::Yes {
        http_provider_builder =
            http_provider_builder.wallet(anvil.wallet().expect("anvil should have a wallet"));
    }
    let http_provider = http_provider_builder
        .chain_id(31_337)
        .build()
        .expect("Should be able to configure HTTP provider for local anvil");
    (anvil, http_provider)
}

fn retry_policy_config(max_times: usize) -> super::RetryPolicyConfig {
    super::RetryPolicyConfig {
        min_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
        max_times,
    }
}

fn build_test_http_provider(
    http_urls: Vec<Url>,
    timeout: Duration,
    retry_policy_config: super::RetryPolicyConfig,
) -> HttpRpcProvider {
    let mut config = HttpRpcProviderConfig::with_default_values(http_urls);
    config.timeout = timeout;
    config.retry_policy_config = retry_policy_config;
    HttpRpcProviderBuilder::with_config(&config)
        .environment(Environment::Dev)
        .build()
        .expect("HTTP provider should build")
}

async fn wait_for_count(counter: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while counter.load(Ordering::SeqCst) < expected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for expected count");
}

#[derive(Debug)]
struct ErrorBody {
    has_failed: bool,
}

impl HttpBody for ErrorBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.has_failed {
            Poll::Ready(None)
        } else {
            self.has_failed = true;
            Poll::Ready(Some(Err(std::io::Error::other(
                "simulated transport abort",
            ))))
        }
    }
}

#[derive(Debug)]
struct HttpRpcServerState {
    steps: Mutex<VecDeque<HttpRpcStep>>,
    request_count: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct HttpRpcServer {
    url: Url,
    request_count: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl HttpRpcServer {
    fn url(&self) -> Url {
        self.url.clone()
    }

    fn request_count(&self) -> &Arc<AtomicUsize> {
        &self.request_count
    }
}

impl Drop for HttpRpcServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_http_rpc(State(state): State<Arc<HttpRpcServerState>>, request: Bytes) -> Response {
    let request = String::from_utf8(request.to_vec()).expect("request should be valid UTF-8");
    let step = {
        let mut steps = state
            .steps
            .lock()
            .expect("server state should not be poisoned");
        steps
            .pop_front()
            .expect("server received more requests than configured")
    };

    state.request_count.fetch_add(1, Ordering::SeqCst);
    assert!(
        request.contains(step.expected_method),
        "expected method {} in request {request}",
        step.expected_method
    );

    match step.action {
        HttpRpcAction::Respond {
            status,
            response_body,
            delay,
        } => {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            (
                StatusCode::from_u16(status).expect("status should be valid"),
                response_body,
            )
                .into_response()
        }
        HttpRpcAction::Timeout { delay } => {
            tokio::time::sleep(delay).await;
            StatusCode::NO_CONTENT.into_response()
        }
        HttpRpcAction::CloseConnection => Response::new(Body::new(ErrorBody { has_failed: false })),
    }
}

async fn spawn_http_rpc_server(steps: impl IntoIterator<Item = HttpRpcStep>) -> HttpRpcServer {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("listener should bind");
    let url = Url::parse(&format!(
        "http://{}",
        listener
            .local_addr()
            .expect("listener should have a local address")
    ))
    .expect("listener URL should parse");
    let request_count = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(HttpRpcServerState {
        steps: Mutex::new(steps.into_iter().collect::<VecDeque<_>>()),
        request_count: Arc::clone(&request_count),
    });
    let app = Router::new()
        .route("/", post(handle_http_rpc))
        .with_state(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("server should serve requests");
    });

    HttpRpcServer {
        url,
        request_count,
        task,
    }
}

#[tokio::test]
async fn http_provider_retries_json_rpc_error_then_succeeds() {
    let server = spawn_http_rpc_server([
        HttpRpcStep::ok(
            "eth_blockNumber",
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32007,"message":"100/second request limit reached - reduce calls per second"}}"#,
        ),
        HttpRpcStep::ok("eth_blockNumber", r#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#),
    ])
    .await;
    let provider = build_test_http_provider(
        vec![server.url()],
        Duration::from_secs(1),
        retry_policy_config(1),
    );

    let block_number = provider
        .get_block_number()
        .await
        .expect("retryable JSON-RPC error should be retried");

    assert_eq!(block_number, 1);
    assert_eq!(server.request_count().load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn http_provider_does_not_retry_non_retryable_json_rpc_error() {
    let server = spawn_http_rpc_server([HttpRpcStep::ok(
        "eth_blockNumber",
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Invalid params"}}"#,
    )])
    .await;
    let provider = build_test_http_provider(
        vec![server.url()],
        Duration::from_secs(1),
        retry_policy_config(1),
    );

    let error = provider
        .get_block_number()
        .await
        .expect_err("non-retryable JSON-RPC error should fail immediately");

    assert!(matches!(error, RpcError::ErrorResp(err) if err.code == -32602));
    assert_eq!(server.request_count().load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn http_provider_retries_408_502_504() {
    for status in [408, 502, 504] {
        let server = spawn_http_rpc_server([
            HttpRpcStep::status("eth_blockNumber", status, ""),
            HttpRpcStep::ok(
                "eth_blockNumber",
                r#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#,
            ),
        ])
        .await;
        let provider = build_test_http_provider(
            vec![server.url()],
            Duration::from_secs(1),
            retry_policy_config(1),
        );

        let block_number = provider
            .get_block_number()
            .await
            .unwrap_or_else(|error| panic!("status {status} should be retried: {error:?}"));

        assert_eq!(block_number, 1);
        assert_eq!(server.request_count().load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn http_provider_does_not_retry_500() {
    let server = spawn_http_rpc_server([HttpRpcStep::status(
        "eth_blockNumber",
        500,
        "internal error",
    )])
    .await;
    let provider = build_test_http_provider(
        vec![server.url()],
        Duration::from_secs(1),
        retry_policy_config(1),
    );

    let error = provider
        .get_block_number()
        .await
        .expect_err("HTTP 500 should not be retried");

    assert!(matches!(
        error,
        RpcError::Transport(TransportErrorKind::HttpError(http_error)) if http_error.status == 500
    ));
    assert_eq!(server.request_count().load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn http_provider_prefers_slower_success_over_fast_json_rpc_error() {
    let fast_error_server = spawn_http_rpc_server([HttpRpcStep::ok(
        "eth_blockNumber",
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Invalid params"}}"#,
    )
    .with_delay(Duration::from_millis(1))])
    .await;
    let slow_success_server = spawn_http_rpc_server([HttpRpcStep::ok(
        "eth_blockNumber",
        r#"{"jsonrpc":"2.0","id":1,"result":"0x2"}"#,
    )
    .with_delay(Duration::from_millis(25))])
    .await;
    let provider = build_test_http_provider(
        vec![fast_error_server.url(), slow_success_server.url()],
        Duration::from_secs(1),
        retry_policy_config(1),
    );

    let block_number = provider
        .get_block_number()
        .await
        .expect("fallback should ignore JSON-RPC error responses and wait for success");

    assert_eq!(block_number, 2);
    assert_eq!(fast_error_server.request_count().load(Ordering::SeqCst), 1);
    assert_eq!(
        slow_success_server.request_count().load(Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn http_provider_retries_timeout_custom_error() {
    let server = spawn_http_rpc_server([
        HttpRpcStep::timeout("eth_blockNumber", Duration::from_secs(1)),
        HttpRpcStep::timeout("eth_blockNumber", Duration::from_secs(1)),
    ])
    .await;
    let provider = build_test_http_provider(
        vec![server.url()],
        Duration::from_millis(50),
        retry_policy_config(1),
    );

    let error = provider
        .get_block_number()
        .await
        .expect_err("request should time out after the configured retries");

    assert!(matches!(
        error,
        RpcError::Transport(kind)
            if kind
                .as_custom()
                .and_then(|error| error.downcast_ref::<reqwest::Error>())
                .is_some_and(reqwest::Error::is_timeout)
    ));
    wait_for_count(server.request_count().as_ref(), 2).await;
}

#[tokio::test]
async fn http_provider_does_not_retry_non_timeout_custom_error() {
    let server = spawn_http_rpc_server([HttpRpcStep::close_connection("eth_blockNumber")]).await;
    let provider = build_test_http_provider(
        vec![server.url()],
        Duration::from_secs(1),
        retry_policy_config(1),
    );

    let error = provider
        .get_block_number()
        .await
        .expect_err("non-timeout custom errors should not be retried");

    assert!(matches!(
        error,
        RpcError::Transport(kind)
            if kind
                .as_custom()
                .and_then(|error| error.downcast_ref::<reqwest::Error>())
                .is_some_and(|error| !error.is_timeout())
    ));
    assert_eq!(server.request_count().load(Ordering::SeqCst), 1);
}
