//! Health and metrics handlers.
//!
//! Modelled on `git-store/src/web_utils.rs` so the two VCS stores share
//! the same observability surface (same prometheus metric names and
//! `/health` / `/ready` / `/metrics` routes).

use axum::{extract::State, http::StatusCode, response::IntoResponse};
use std::sync::Arc;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ready_without_db_is_ok() {
        let c = HealthChecker::new();
        assert!(c.is_ready().await);
    }
}
