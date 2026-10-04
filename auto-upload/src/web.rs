//! Minimal web server for the auto-upload service.
//!
//! An axum server with a single `/metrics` route that renders the Prometheus
//! registry.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use prometheus::{Encoder, TextEncoder};
use tracing::info;

use crate::error::Result;

/// Build the auto-upload web application.
pub fn create_app() -> Router {
    Router::new().route("/metrics", get(metrics_handler))
}

async fn metrics_handler() -> Response {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();

    match encoder.encode(&metric_families, &mut buffer) {
        Ok(_) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, encoder.format_type())],
            buffer,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to encode metrics: {}", e),
        )
            .into_response(),
    }
}

/// Bind and serve `app` on `listen_addr:port` until the connection is
/// closed. This is a thin wrapper over [`axum::serve`] so callers can
/// select on it alongside other tasks.
pub async fn run_web_server(app: Router, listen_addr: &str, port: u16) -> Result<()> {
    let addr = format!("{}:{}", listen_addr, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("Web server listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn metrics_endpoint_responds_with_prometheus_format() {
        let app = create_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .expect("content-type header")
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            content_type.contains("text/plain"),
            "unexpected content-type: {content_type}"
        );

        // Trigger a counter increment so the response is non-empty.
        crate::DEBSIGN_FAILED_COUNT.inc();

        let response = create_app()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("debsign_failed"),
            "metrics output missing debsign_failed:\n{text}"
        );
    }

    #[tokio::test]
    async fn metrics_endpoint_404s_on_unknown_route() {
        let app = create_app();
        let response = app
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
