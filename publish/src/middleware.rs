//! Axum middleware for the publish service.

use axum::middleware::Next;
use lazy_static::lazy_static;
use prometheus::{register_counter_vec, register_histogram_vec, CounterVec, HistogramVec};
use std::time::Instant;

lazy_static! {
    static ref HTTP_REQUEST_DURATION: HistogramVec = register_histogram_vec!(
        "http_request_duration_seconds",
        "HTTP request duration in seconds",
        &["method", "path", "status"]
    )
    .unwrap();
    static ref HTTP_REQUEST_COUNT: CounterVec = register_counter_vec!(
        "http_requests_total",
        "Total number of HTTP requests",
        &["method", "path", "status"]
    )
    .unwrap();
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
    let duration = start.elapsed();

    HTTP_REQUEST_DURATION
        .with_label_values(&[&method, &path, &status])
        .observe(duration.as_secs_f64());
    HTTP_REQUEST_COUNT
        .with_label_values(&[&method, &path, &status])
        .inc();

    response
}
