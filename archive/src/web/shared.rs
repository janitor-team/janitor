//! Minimal shared web helpers for the archive service: a plain
//! `/health` endpoint and a placeholder `/metrics` endpoint. Kept
//! local so this crate stays self-contained.

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

/// Prometheus metrics endpoint. Returns an empty text/plain body so
/// scrapers that hit `/metrics` succeed rather than 404. A future
/// change can encode a real prometheus registry.
#[utoipa::path(
    get,
    path = "/metrics",
    responses((status = 200, description = "Prometheus metrics", body = String, content_type = "text/plain"))
)]
pub async fn metrics_ok() -> Response {
    (
        StatusCode::OK,
        [("Content-Type", "text/plain; version=0.0.4")],
        String::new(),
    )
        .into_response()
}
