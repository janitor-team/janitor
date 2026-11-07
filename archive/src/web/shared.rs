//! Minimal shared web helpers for the archive service: the `/health`
//! and `/metrics` endpoints.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// `GET /health` -- plain `"ok"` body.
#[utoipa::path(
    get,
    path = "/health",
    responses((status = 200, description = "Service is alive", body = String, content_type = "text/plain"))
)]
pub async fn health_ok() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// Prometheus metrics endpoint.
#[utoipa::path(
    get,
    path = "/metrics",
    responses((status = 200, description = "Prometheus metrics", body = String, content_type = "text/plain"))
)]
pub async fn metrics() -> Response {
    janitor_service::metrics::metrics_handler().await
}
