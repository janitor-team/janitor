//! Health and metrics handlers.
//!
//! Modelled on `git-store/src/web_utils.rs` so the two VCS stores share
//! the same observability surface (same prometheus metric names and
//! `/health` / `/ready` / `/metrics` routes).

use axum::{extract::State, http::StatusCode, response::IntoResponse};
use std::sync::Arc;

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
    responses(
        (status = 200, description = "Prometheus metrics", body = String, content_type = "text/plain"),
        (status = 500, description = "Metrics could not be encoded", body = String, content_type = "text/plain"),
    )
)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ready_without_db_is_ok() {
        let c = HealthChecker::new();
        assert!(c.is_ready().await);
    }
}
