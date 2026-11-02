//! Minimal shared web helpers for the archive service: a plain
//! `/health` endpoint and the `/metrics` endpoint.

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

/// `GET /metrics` -- the default Prometheus registry, served by
/// janitor-service's `metrics_handler`.
#[utoipa::path(
    get,
    path = "/metrics",
    responses(
        (status = 200, description = "Prometheus metrics", body = String, content_type = "text/plain"),
        (status = 500, description = "Metrics could not be encoded", body = String, content_type = "text/plain"),
    )
)]
pub async fn metrics_ok() -> Response {
    janitor_service::prometheus::metrics_handler().await
}
