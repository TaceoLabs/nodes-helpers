//! Axum middleware layer for [Unkey](https://unkey.com) API key verification.
//!
//! Add [`UnkeyLayer`] to your router to authenticate requests with Unkey.
//! The layer reads the bearer token from the `Authorization` header and verifies it
//! against the Unkey API. Valid keys are forwarded; invalid or missing keys receive
//! a `401 Unauthorized` response (or `429 Too Many Requests` when rate-limited).

use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use headers::{Authorization, Header, authorization::Bearer};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tower::{Layer, Service};

const DEFAULT_VERIFY_URL: &str = "https://api.unkey.com/v2/keys.verifyKey";

#[derive(Serialize, Deserialize)]
struct VerifyKeyRequest {
    key: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifyKeyResponse {
    data: VerifyKeyData,
}

#[derive(Serialize, Deserialize)]
struct VerifyKeyData {
    valid: bool,
    code: String,
}

enum UnkeyError {
    Missing,
    Invalid,
    RateLimited,
    Internal,
}

impl IntoResponse for UnkeyError {
    fn into_response(self) -> Response {
        match self {
            UnkeyError::Missing | UnkeyError::Invalid => StatusCode::UNAUTHORIZED.into_response(),
            UnkeyError::RateLimited => StatusCode::TOO_MANY_REQUESTS.into_response(),
            UnkeyError::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

/// Tower [`Layer`] that verifies requests against the [Unkey](https://unkey.com) API.
///
/// Extracts the bearer token from the `Authorization` header of each incoming request and
/// calls `POST https://api.unkey.com/v2/keys.verifyKey`. Valid keys are forwarded to the
/// inner service; invalid or missing keys short-circuit with an appropriate HTTP error.
///
/// # Example
///
/// ```no_run
/// use axum::{Router, routing::get};
/// use secrecy::SecretString;
/// use taceo_nodes_common::api::unkey::UnkeyLayer;
///
/// #[tokio::main]
/// async fn main() {
///     let verify_key = SecretString::from("unkey_root_key");
///     let layer = UnkeyLayer::new(reqwest::Client::new(), verify_key);
///
///     let app = Router::new()
///         .route("/secret", get(|| async { "hello" }))
///         .layer(layer);
///
///     let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
///     axum::serve(listener, app).await.unwrap();
/// }
/// ```
#[allow(
    clippy::module_name_repetitions,
    reason = "The `unkey` prefix is necessary for clarity when the type is imported from this module"
)]
#[non_exhaustive]
#[derive(Clone)]
pub struct UnkeyLayer {
    client: reqwest::Client,
    verify_key: Arc<SecretString>,
    verify_url: String,
}

impl UnkeyLayer {
    /// Creates a new [`UnkeyLayer`].
    ///
    /// `verify_key` is the Unkey root/verify key used to authenticate the verification request.
    #[must_use]
    pub fn new(client: reqwest::Client, verify_key: SecretString) -> Self {
        Self {
            client,
            verify_key: Arc::new(verify_key),
            verify_url: DEFAULT_VERIFY_URL.to_owned(),
        }
    }

    /// Sets the Unkey verification URL. Defaults to `https://api.unkey.com/v2/keys.verifyKey`.
    #[must_use]
    pub fn with_verify_url(mut self, url: String) -> Self {
        self.verify_url = url;
        self
    }
}

impl<S> Layer<S> for UnkeyLayer {
    type Service = UnkeyService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        UnkeyService {
            inner,
            client: self.client.clone(),
            verify_key: self.verify_key.clone(),
            verify_url: self.verify_url.clone(),
        }
    }
}

/// Tower [`Service`] produced by [`UnkeyLayer`].
#[allow(
    clippy::module_name_repetitions,
    reason = "The `unkey` prefix is necessary for clarity when the type is imported from this module"
)]
#[non_exhaustive]
#[derive(Clone)]
pub struct UnkeyService<S> {
    inner: S,
    client: reqwest::Client,
    verify_key: Arc<SecretString>,
    verify_url: String,
}

impl<S> Service<Request<Body>> for UnkeyService<S>
where
    S: Service<Request<Body>, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Response, S::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let api_key = extract_bearer_token(&req);

        // Take the ready inner service; put a fresh clone in its place for the next call.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let client = self.client.clone();
        let verify_key = self.verify_key.clone();
        let verify_url = self.verify_url.clone();

        Box::pin(async move {
            let Some(api_key) = api_key else {
                return Ok(UnkeyError::Missing.into_response());
            };
            match verify_api_key(&client, &verify_key, &verify_url, &api_key).await {
                Ok(()) => inner.call(req).await,
                Err(err) => Ok(err.into_response()),
            }
        })
    }
}

fn extract_bearer_token(req: &Request<Body>) -> Option<String> {
    Authorization::<Bearer>::decode(
        &mut req
            .headers()
            .get_all(Authorization::<Bearer>::name())
            .iter(),
    )
    .ok()
    .map(|auth| auth.token().to_owned())
}

async fn verify_api_key(
    client: &reqwest::Client,
    verify_key: &SecretString,
    verify_url: &str,
    api_key: &str,
) -> Result<(), UnkeyError> {
    let resp = client
        .post(verify_url)
        .bearer_auth(verify_key.expose_secret())
        .json(&VerifyKeyRequest {
            key: api_key.to_owned(),
        })
        .send()
        .await
        .map_err(|e| {
            tracing::error!("Unkey request failed: {e}");
            UnkeyError::Internal
        })?
        .error_for_status()
        .map_err(|e| {
            tracing::error!("Unkey returned error status: {e}");
            UnkeyError::Internal
        })?;

    let body = resp.json::<VerifyKeyResponse>().await.map_err(|e| {
        tracing::error!("Failed to parse Unkey response: {e}");
        UnkeyError::Internal
    })?;

    if !body.data.valid {
        if body.data.code == "RATE_LIMITED" {
            return Err(UnkeyError::RateLimited);
        }
        return Err(UnkeyError::Invalid);
    }
    Ok(())
}

#[cfg(all(test, feature = "test-utils"))]
mod tests {
    use super::*;
    use axum::{Router, http::StatusCode, routing::post};
    use axum_test::TestServer;
    use secrecy::SecretString;

    const VALID_KEY: &str = "valid_api_key";
    const RATE_LIMITED_KEY: &str = "rate_limited_api_key";

    async fn mock_verify_handler(
        axum::extract::Json(body): axum::extract::Json<VerifyKeyRequest>,
    ) -> axum::extract::Json<VerifyKeyResponse> {
        axum::extract::Json(match body.key.as_str() {
            VALID_KEY => VerifyKeyResponse {
                data: VerifyKeyData {
                    valid: true,
                    code: String::new(),
                },
            },
            RATE_LIMITED_KEY => VerifyKeyResponse {
                data: VerifyKeyData {
                    valid: false,
                    code: "RATE_LIMITED".to_owned(),
                },
            },
            _ => VerifyKeyResponse {
                data: VerifyKeyData {
                    valid: false,
                    code: "INVALID_KEY".to_owned(),
                },
            },
        })
    }

    struct MockUnkeyApi {
        _server: TestServer,
        verify_url: String,
    }

    impl MockUnkeyApi {
        fn start() -> Self {
            let router = Router::new().route("/v2/keys.verifyKey", post(mock_verify_handler));
            let (server, addr) = crate::test_utils::test_server(router);
            let verify_url = format!("{addr}/v2/keys.verifyKey");
            Self {
                _server: server,
                verify_url,
            }
        }
    }

    fn make_app(mock: &MockUnkeyApi) -> TestServer {
        let layer = UnkeyLayer::new(
            reqwest::Client::new(),
            SecretString::from("test_verify_key"),
        )
        .with_verify_url(mock.verify_url.clone());

        let app = Router::new()
            .route(
                "/protected",
                axum::routing::get(|| async { StatusCode::OK }),
            )
            .layer(layer);

        TestServer::new(app)
    }

    #[tokio::test]
    async fn valid_key_is_forwarded() {
        let mock = MockUnkeyApi::start();
        let server = make_app(&mock);

        server
            .get("/protected")
            .add_header(
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderValue::from_static("Bearer valid_api_key"),
            )
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn missing_key_returns_unauthorized() {
        let mock = MockUnkeyApi::start();
        let server = make_app(&mock);

        server
            .get("/protected")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn invalid_key_returns_unauthorized() {
        let mock = MockUnkeyApi::start();
        let server = make_app(&mock);

        server
            .get("/protected")
            .add_header(
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderValue::from_static("Bearer wrong_key"),
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rate_limited_key_returns_too_many_requests() {
        let mock = MockUnkeyApi::start();
        let server = make_app(&mock);

        server
            .get("/protected")
            .add_header(
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderValue::from_static("Bearer rate_limited_api_key"),
            )
            .await
            .assert_status(StatusCode::TOO_MANY_REQUESTS);
    }
}
