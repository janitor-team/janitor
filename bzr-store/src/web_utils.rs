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
use prometheus::{HistogramVec, IntCounterVec};
use std::sync::{Arc, OnceLock};
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
fn http_requests_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        prometheus::register_int_counter_vec!(
            "http_requests_total",
            "Total HTTP requests handled",
            &["method", "path", "status"]
        )
        .expect("register http_requests_total")
    })
}

/// Per-request latency histogram (seconds). Buckets include the slow
/// end (30s, 60s) because bzr smart-protocol pushes can block on big
/// fetches.
fn http_request_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        prometheus::register_histogram_vec!(
            prometheus::histogram_opts!(
                "http_request_duration_seconds",
                "HTTP request duration in seconds",
                vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]
            ),
            &["method", "path"]
        )
        .expect("register http_request_duration_seconds")
    })
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
    http_requests_total()
        .with_label_values(&[&method, &path, &status])
        .inc();
    http_request_duration_seconds()
        .with_label_values(&[&method, &path])
        .observe(elapsed);

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
            Some(pool) => sqlx::query("SELECT 1").fetch_one(pool).await.is_ok(),
            None => true,
        }
    }
}

/// `GET /health` — alive, independent of downstream state.
pub async fn health_handler(State(_): State<Arc<HealthChecker>>) -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// `GET /ready` — alive AND the configured database is reachable.
pub async fn ready_handler(State(checker): State<Arc<HealthChecker>>) -> impl IntoResponse {
    if checker.is_ready().await {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

/// `GET /metrics` — Prometheus text-format exposition of every
/// registered metric.
pub async fn metrics_handler() -> impl IntoResponse {
    use prometheus::{Encoder, TextEncoder};

    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        tracing::error!("failed to encode prometheus metrics: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("Content-Type", "text/plain")],
            "failed to gather metrics".to_string(),
        );
    }
    (
        StatusCode::OK,
        [("Content-Type", "text/plain; version=0.0.4")],
        String::from_utf8(buffer).unwrap_or_default(),
    )
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
    use tower::ServiceExt;

    #[tokio::test]
    async fn ready_without_db_is_ok() {
        let c = HealthChecker::new();
        assert!(c.is_ready().await);
    }

    #[tokio::test]
    async fn record_http_metrics_bumps_counters() {
        let before = http_requests_total()
            .with_label_values(&["GET", "/x", "200"])
            .get();

        let app = Router::new()
            .route("/x", get(|| async { "hi" }))
            .layer(axum::middleware::from_fn(record_http_metrics));

        let resp = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let after = http_requests_total()
            .with_label_values(&["GET", "/x", "200"])
            .get();
        assert_eq!(after, before + 1);
    }
}
