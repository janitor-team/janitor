//! Minimal web server for the auto-upload service.
//!
//! An axum server exposing `/metrics` plus the `/health` and `/ready`
//! endpoints every other janitor service serves, so monitoring does not
//! have to special-case this one.

use std::sync::Arc;

use axum::{response::Response, routing::get, Router};
use janitor::shared_web::{self, BaseAppState, BasicHealthChecker, HealthCheckHandler};
use tracing::info;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::Result;

/// State behind the health endpoints.
#[derive(Clone)]
pub struct AppState {
    health_checker: Arc<BasicHealthChecker>,
}

impl AppState {
    /// Build state with a health checker naming this service.
    pub fn new() -> Self {
        Self {
            health_checker: Arc::new(BasicHealthChecker::with_info(
                "auto-upload".to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
            )),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl BaseAppState for AppState {
    fn service_name(&self) -> &str {
        "auto-upload"
    }

    fn service_version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn health_checker(&self) -> Arc<dyn HealthCheckHandler> {
        self.health_checker.clone()
    }
}

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
    router
        .route(
            "/openapi.json",
            get(move || async move { ([("content-type", "application/json")], spec) }),
        )
        .merge(health_routes())
}

/// `/health` and `/ready`, served by every janitor service.
fn health_routes() -> Router {
    Router::new()
        .route(
            "/health",
            get(shared_web::health::health_check_handler::<AppState>),
        )
        .route(
            "/ready",
            get(shared_web::health::readiness_handler::<AppState>),
        )
        .with_state(AppState::new())
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
pub async fn run_web_server(app: Router, listen_addr: std::net::IpAddr, port: u16) -> Result<()> {
    let addr = std::net::SocketAddr::new(listen_addr, port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
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
