//! Minimal shared web helpers for the archive service: a plain
//! `/health` endpoint and a `/metrics` endpoint. Kept
//! local so this crate stays self-contained.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// `GET /health` -- plain `"ok"` body.
pub async fn health_ok() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// Prometheus metrics endpoint. Returns an empty text/plain body so
/// scrapers that hit `/metrics` succeed rather than 404.
pub async fn metrics_ok() -> Response {
    (
        StatusCode::OK,
        [("Content-Type", "text/plain; version=0.0.4")],
        String::new(),
    )
        .into_response()
}
