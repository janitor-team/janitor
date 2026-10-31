//! Types and axum handlers for the `/health` and `/ready` endpoints.

use crate::AppState;
use async_trait::async_trait;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Health status for the service overall or an individual component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// Fully operational.
    Healthy,
    /// Operational but with reduced capability.
    Degraded,
    /// Not operational.
    Unhealthy,
}

/// Full health-check response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    /// Overall status, computed from the individual component checks.
    pub status: HealthStatus,
    /// Service name (e.g. `"publish"`).
    pub service: String,
    /// Service version (typically `CARGO_PKG_VERSION`).
    pub version: String,
    /// Per-component results.
    pub checks: Vec<ComponentHealth>,
    /// When this check was produced.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Health of one component contributing to the overall service health.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentHealth {
    /// Component name.
    pub name: String,
    /// Status of this component.
    pub status: HealthStatus,
    /// Error detail if the component is not healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Additional structured details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Trait for checking an individual component.
#[async_trait]
pub trait ComponentChecker: Send + Sync {
    /// Name of the component this checker reports on.
    fn name(&self) -> &str;

    /// Perform the check.
    async fn check(&self) -> ComponentHealth;
}

/// Simple aggregating health checker.
pub struct BasicHealthChecker {
    service_name: String,
    service_version: String,
    component_checks: Vec<Arc<dyn ComponentChecker>>,
}

impl BasicHealthChecker {
    /// Construct with an explicit service name and version.
    pub fn with_info(service_name: String, service_version: String) -> Self {
        Self {
            service_name,
            service_version,
            component_checks: Vec::new(),
        }
    }

    /// Register a component checker; results are aggregated by
    /// [`Self::check_health`].
    pub fn add_component_check(mut self, checker: Arc<dyn ComponentChecker>) -> Self {
        self.component_checks.push(checker);
        self
    }

    /// Return the full health report.
    pub async fn check_health(&self) -> HealthCheck {
        let mut checks = Vec::with_capacity(self.component_checks.len());
        let mut overall = HealthStatus::Healthy;

        for checker in &self.component_checks {
            let component = checker.check().await;
            match component.status {
                HealthStatus::Unhealthy => overall = HealthStatus::Unhealthy,
                HealthStatus::Degraded if overall == HealthStatus::Healthy => {
                    overall = HealthStatus::Degraded;
                }
                _ => {}
            }
            checks.push(component);
        }

        HealthCheck {
            status: overall,
            service: self.service_name.clone(),
            version: self.service_version.clone(),
            checks,
            timestamp: chrono::Utc::now(),
        }
    }

    /// Readiness probe: ready when healthy.
    pub async fn check_readiness(&self) -> bool {
        matches!(self.check_health().await.status, HealthStatus::Healthy)
    }
}

/// Component checker that runs `SELECT 1` against a Postgres pool.
pub struct DatabaseHealthChecker {
    pool: sqlx::PgPool,
}

impl DatabaseHealthChecker {
    /// Construct from a pool.
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ComponentChecker for DatabaseHealthChecker {
    fn name(&self) -> &str {
        "database"
    }

    async fn check(&self) -> ComponentHealth {
        match janitor::database::health_check(&self.pool).await {
            Ok(_) => ComponentHealth {
                name: self.name().to_string(),
                status: HealthStatus::Healthy,
                error: None,
                details: Some(serde_json::json!({
                    "pool_size": self.pool.size(),
                    "idle_connections": self.pool.num_idle(),
                })),
            },
            Err(e) => ComponentHealth {
                name: self.name().to_string(),
                status: HealthStatus::Unhealthy,
                error: Some(e.to_string()),
                details: None,
            },
        }
    }
}

/// Component checker that PINGs Redis.
pub struct RedisHealthChecker {
    client: redis::Client,
}

impl RedisHealthChecker {
    /// Construct from a Redis client.
    pub fn new(client: redis::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ComponentChecker for RedisHealthChecker {
    fn name(&self) -> &str {
        "redis"
    }

    async fn check(&self) -> ComponentHealth {
        // The async client has default connect and reply timeouts; the blocking one has none.
        let result = match self.client.get_multiplexed_async_connection().await {
            Ok(mut conn) => redis::cmd("PING").query_async::<String>(&mut conn).await,
            Err(e) => Err(e),
        };
        ComponentHealth {
            name: self.name().to_string(),
            status: if result.is_ok() {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy
            },
            error: result.err().map(|e| e.to_string()),
            details: None,
        }
    }
}

/// Axum handler for `GET /health`. Returns the full health JSON.
pub async fn health_check_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let health = state.health_checker.check_health().await;
    let status_code = match health.status {
        HealthStatus::Healthy | HealthStatus::Degraded => StatusCode::OK,
        HealthStatus::Unhealthy => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status_code, Json(health))
}

/// Axum handler for `GET /ready`. Returns 200 when ready, 503 otherwise.
pub async fn readiness_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if state.health_checker.check_readiness().await {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_redis_check_does_not_hang() {
        // The kernel completes the TCP handshake, but nothing ever reads or answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("redis://{}/", listener.local_addr().unwrap());
        let checker = RedisHealthChecker::new(redis::Client::open(url).unwrap());

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            tx.send(rt.block_on(checker.check())).unwrap();
        });
        let health = rx.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(health.status, HealthStatus::Unhealthy);
        assert_eq!(health.error.as_deref(), Some("timed out"));
        drop(listener);
    }
}
