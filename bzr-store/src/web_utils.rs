//! Health, metrics and middleware helpers.
//!
//! Modelled on `git-store/src/web_utils.rs` so the two VCS stores share
//! the same observability surface (same prometheus metric names and
//! `/health` / `/ready` / `/metrics` routes).

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Router,
};
use janitor::shared_config::WebConfig;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

/// Per-request counter labelled by method / route / status. Named the
/// same as the Python `aiohttp_openmetrics` counterpart so dashboards
/// keep working.
const HTTP_REQUESTS_TOTAL: &str = "http_requests_total";

/// Per-request latency histogram (seconds). Buckets include the slow
/// end (30s, 60s) because bzr smart-protocol pushes can block on big
/// fetches.
const HTTP_REQUEST_DURATION_SECONDS: &str = "http_request_duration_seconds";

/// Histogram bucket overrides for this service.
pub const METRICS_BUCKETS: janitor_service::metrics::BucketOverrides = &[(
    HTTP_REQUEST_DURATION_SECONDS,
    &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0],
)];

/// Install the metrics recorder and describe this service's metrics.
pub fn install_metrics() {
    janitor_service::metrics::install(METRICS_BUCKETS);
    metrics::describe_counter!(HTTP_REQUESTS_TOTAL, "Total HTTP requests handled");
    metrics::describe_histogram!(
        HTTP_REQUEST_DURATION_SECONDS,
        "HTTP request duration in seconds"
    );
}

/// axum middleware that records request-count and latency metrics. The
/// path label uses `MatchedPath` when available so `/foo/{id}` buckets
/// identically for every `id`.
pub async fn record_http_metrics(req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_string();
    let path = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());

    let started = Instant::now();
    let response = next.run(req).await;
    let elapsed = started.elapsed().as_secs_f64();

    let status = response.status().as_u16().to_string();
    metrics::counter!(
        HTTP_REQUESTS_TOTAL,
        "method" => method.clone(),
        "path" => path.clone(),
        "status" => status
    )
    .increment(1);
    metrics::histogram!(HTTP_REQUEST_DURATION_SECONDS, "method" => method, "path" => path)
        .record(elapsed);

    response
}

/// Backs `/health` and `/ready`. `/ready` runs `SELECT 1` when a pool
/// is attached; `/health` always reports alive.
#[derive(Default)]
pub struct HealthChecker {
    db: Option<sqlx::PgPool>,
}

impl HealthChecker {
    /// Create a new health checker with no database dependency.
    pub fn new() -> Self {
        Self { db: None }
    }

    /// Attach a database pool: `/ready` will probe it with `SELECT 1`.
    pub fn with_db(mut self, pool: sqlx::PgPool) -> Self {
        self.db = Some(pool);
        self
    }

    async fn is_ready(&self) -> bool {
        match &self.db {
            Some(pool) => janitor::database::health_check(pool).await.is_ok(),
            None => true,
        }
    }
}

/// `GET /health` — alive, independent of downstream state.
#[utoipa::path(
    get,
    path = "/health",
    responses((status = 200, description = "Service is alive", body = String, content_type = "text/plain"))
)]
pub async fn health_handler(State(_): State<Arc<HealthChecker>>) -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// `GET /ready` — alive AND the configured database is reachable.
#[utoipa::path(
    get,
    path = "/ready",
    responses(
        (status = 200, description = "Service is ready", body = String, content_type = "text/plain"),
        (status = 503, description = "Service is not ready", body = String, content_type = "text/plain"),
    )
)]
pub async fn ready_handler(State(checker): State<Arc<HealthChecker>>) -> impl IntoResponse {
    if checker.is_ready().await {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

/// `GET /metrics` — Prometheus text-format exposition of every
/// registered metric.
#[utoipa::path(
    get,
    path = "/metrics",
    responses((status = 200, description = "Prometheus metrics", body = String, content_type = "text/plain"))
)]
pub async fn metrics_handler() -> axum::response::Response {
    janitor_service::metrics::metrics_handler().await
}

/// Attach the standard middleware stack (metrics, body limit, timeout,
/// optional compression / CORS / request logging) to a router.
pub fn apply_standard_middleware<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    config: &WebConfig,
) -> Router<S> {
    let mut router = router;

    router = router.layer(axum::middleware::from_fn(record_http_metrics));
    router = router.layer(RequestBodyLimitLayer::new(config.max_request_size_bytes));
    router = router.layer(TimeoutLayer::new(Duration::from_secs(
        config.request_timeout_seconds,
    )));

    if config.enable_compression {
        router = router.layer(CompressionLayer::new());
    }

    if config.enable_cors {
        let cors = CorsLayer::new()
            .allow_methods(Any)
            .allow_origin(Any)
            .allow_headers(Any);
        router = router.layer(cors);
    }

    if config.enable_request_logging {
        router = router.layer(TraceLayer::new_for_http());
    }

    router
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, routing::get, Router};
    use janitor_service::testing::{capture_async, Value};
    use tower::ServiceExt;

    #[tokio::test]
    async fn ready_without_db_is_ok() {
        let c = HealthChecker::new();
        assert!(c.is_ready().await);
    }

    #[test]
    fn record_http_metrics_records_request() {
        let (status, snapshot) = capture_async(async {
            let app = Router::new()
                .route("/x/{id}", get(|| async { "hi" }))
                .layer(axum::middleware::from_fn(record_http_metrics));
            app.oneshot(Request::builder().uri("/x/1").body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        });
        assert_eq!(status, 200);
        let duration_key = r#"http_request_duration_seconds{method="GET",path="/x/{id}"}"#;
        let count_key = r#"http_requests_total{method="GET",path="/x/{id}",status="200"}"#;
        assert_eq!(
            snapshot.keys().collect::<Vec<_>>(),
            vec![duration_key, count_key]
        );
        assert_eq!(snapshot[count_key], Value::Counter(1));
        let Value::Histogram(durations) = &snapshot[duration_key] else {
            panic!("expected histogram, got {:?}", snapshot[duration_key]);
        };
        assert_eq!(durations.len(), 1);
    }
}
