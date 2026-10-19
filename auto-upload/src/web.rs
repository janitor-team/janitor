//! Minimal web server for the auto-upload service.
//!
//! An axum server with `/health`, `/ready` and a `/metrics` route that
//! renders the Prometheus registry.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use prometheus::{Encoder, TextEncoder};
use tracing::info;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::Result;

#[derive(utoipa::OpenApi)]
struct AutoUploadApi;

fn routes() -> OpenApiRouter {
    OpenApiRouter::with_openapi(<AutoUploadApi as utoipa::OpenApi>::openapi())
        .routes(routes!(health_handler))
        .routes(routes!(metrics_handler))
}

/// OpenAPI document for the auto-upload web application.
pub fn openapi() -> utoipa::openapi::OpenApi {
    routes().into_openapi()
}

/// Build the auto-upload web application.
pub fn create_app() -> Router {
    let (router, openapi) = routes().split_for_parts();
    let spec = openapi
        .to_json()
        .expect("OpenAPI document can be serialized");
    router
        .route(
            "/openapi.json",
            get(move || async move { ([("content-type", "application/json")], spec) }),
        )
        .route("/ready", get(health_handler))
}

#[utoipa::path(
    get,
    path = "/health",
    responses((status = 200, description = "Service is alive", body = String, content_type = "text/plain"))
)]
async fn health_handler() -> &'static str {
    "ok"
}

#[utoipa::path(
    get,
    path = "/metrics",
    responses(
        (status = 200, description = "Prometheus metrics", body = String, content_type = "text/plain"),
        (status = 500, description = "Metrics could not be encoded", body = String, content_type = "text/plain"),
    )
)]
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
    async fn openapi_json_is_served() {
        let response = create_app()
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(doc["info"]["title"], "janitor-auto-upload");
        let paths: Vec<&String> = doc["paths"].as_object().unwrap().keys().collect();
        assert_eq!(paths, vec!["/health", "/metrics"]);
    }

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

        // Force registration of the lazily initialised counter without
        // changing its value; upload::tests asserts on counter deltas.
        std::sync::LazyLock::force(&crate::DEBSIGN_FAILED_COUNT);

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
    async fn health_and_ready_respond_ok() {
        for path in ["/health", "/ready"] {
            let response = create_app()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(&body[..], b"ok");
        }
    }
}
