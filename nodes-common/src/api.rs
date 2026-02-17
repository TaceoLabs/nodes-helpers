//! Health Check Endpoints
//!
//! This module defines the health and version endpoints.
//! - `/health` – general health check
//! - `/version` – version information about the service
//!
//! The endpoints include a `Cache-Control: no-cache` header to prevent caching of responses.

use axum::{
    Router,
    http::{HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::StartedServices;

/// Create a router containing the health and info endpoints.
///
/// All endpoints have `Cache-Control: no-cache` set.
pub fn routes(started_services: StartedServices, version_str: String) -> Router {
    Router::new()
        .route("/health", get(move || health(started_services)))
        .route("/version", get(move || version(version_str)))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ))
}

/// General health check endpoint.
///
/// Returns `200 OK` with a plain `"healthy"` response if all services already started.
/// Returns `503 Service Unavailable` with a plain `"starting"`response if one of the services did not start yet.
async fn health(started_services: StartedServices) -> impl IntoResponse {
    if started_services.all_started() {
        (StatusCode::OK, "healthy")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "starting")
    }
}

/// Responds with cargo package name, cargo package version, and the git hash of the repository that was used to build the binary.
///
/// Returns `200 OK` with a string response.
async fn version(version_str: String) -> impl IntoResponse {
    (StatusCode::OK, version_str)
}
