//! Minimal shared web helpers for the archive service: a plain
//! `/health` endpoint, a placeholder `/metrics` endpoint, and a
//! middleware hook. Kept local so this crate stays self-contained.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Router,
};

/// `GET /health` -- plain `"ok"` body.
pub async fn health_ok() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// Prometheus metrics endpoint. Returns an empty text/plain body so
/// scrapers that hit `/metrics` succeed rather than 404. A future
/// change can encode a real prometheus registry.
pub async fn metrics_ok() -> Response {
    (
        StatusCode::OK,
        [("Content-Type", "text/plain; version=0.0.4")],
        String::new(),
    )
        .into_response()
}

/// No-op middleware application. Returns the router untouched and
/// leaves per-request concerns (timeouts, logging) to `tower_http`
/// layers callers explicitly opt into.
pub fn apply_standard_middleware<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    _web_config: &janitor::shared_config::WebConfig,
) -> Router<S> {
    router
}
