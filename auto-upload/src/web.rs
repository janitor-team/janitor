//! Minimal web server for the auto-upload service.
//!
//! An axum server with a single `/metrics` route.

use axum::{response::Response, routing::get, Router};
use tracing::info;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::Result;

#[derive(utoipa::OpenApi)]
struct AutoUploadApi;

fn routes() -> OpenApiRouter {
    OpenApiRouter::with_openapi(<AutoUploadApi as utoipa::OpenApi>::openapi())
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
    router.route(
        "/openapi.json",
        get(move || async move { ([("content-type", "application/json")], spec) }),
    )
}

#[utoipa::path(
    get,
    path = "/metrics",
    responses((status = 200, description = "Prometheus metrics", body = String, content_type = "text/plain"))
)]
async fn metrics_handler() -> Response {
    janitor_service::metrics::metrics_handler().await
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
    use axum::http::{Request, StatusCode};
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
        assert_eq!(paths, vec!["/metrics"]);
    }

    #[tokio::test]
    async fn metrics_endpoint_responds_with_prometheus_format() {
        crate::install_metrics();
        let response = create_app()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            janitor_service::metrics::CONTENT_TYPE
        );
    }
}
