//! Axum middleware for the publish service.

use axum::middleware::Next;
use std::time::Instant;

const HTTP_REQUEST_DURATION: &str = "http_request_duration_seconds";
const HTTP_REQUEST_COUNT: &str = "http_requests_total";

pub(crate) fn describe_metrics() {
    metrics::describe_histogram!(HTTP_REQUEST_DURATION, "HTTP request duration in seconds");
    metrics::describe_counter!(HTTP_REQUEST_COUNT, "Total number of HTTP requests");
}

/// Record request count and latency per (method, path, status).
pub async fn metrics_middleware(
    req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    let start = Instant::now();
    let method = req.method().to_string();
    let path = req.uri().path().to_string();

    let response = next.run(req).await;
    let status = response.status().as_u16().to_string();
    let labels = [("method", method), ("path", path), ("status", status)];

    metrics::histogram!(HTTP_REQUEST_DURATION, &labels).record(start.elapsed());
    metrics::counter!(HTTP_REQUEST_COUNT, &labels).increment(1);

    response
}
