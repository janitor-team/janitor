//! Types and axum handlers for the `/health` and `/ready` endpoints.

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

/// Trait implemented by things that can answer health / liveness / readiness
/// questions on behalf of a service.
#[async_trait]
pub trait HealthCheckHandler: Send + Sync {
    /// Return the full health report.
    async fn check_health(&self) -> HealthCheck;

    /// Liveness probe. Defaults to `true`: if the handler is still responding,
    /// the process is at least alive.
    async fn check_liveness(&self) -> bool {
        true
    }

    /// Readiness probe. Defaults to "ready when healthy".
    async fn check_readiness(&self) -> bool {
        matches!(self.check_health().await.status, HealthStatus::Healthy)
    }
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

    /// Register a component checker; results are aggregated by [`check_health`].
    ///
    /// [`check_health`]: HealthCheckHandler::check_health
    pub fn add_component_check(mut self, checker: Arc<dyn ComponentChecker>) -> Self {
        self.component_checks.push(checker);
        self
    }
}

#[async_trait]
impl HealthCheckHandler for BasicHealthChecker {
    async fn check_health(&self) -> HealthCheck {
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
        match sqlx::query("SELECT 1").fetch_one(&self.pool).await {
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
        match self.client.get_connection() {
            Ok(mut conn) => {
                let result: Result<String, redis::RedisError> = redis::cmd("PING").query(&mut conn);
                match result {
                    Ok(_) => ComponentHealth {
                        name: self.name().to_string(),
                        status: HealthStatus::Healthy,
                        error: None,
                        details: None,
                    },
                    Err(e) => ComponentHealth {
                        name: self.name().to_string(),
                        status: HealthStatus::Unhealthy,
                        error: Some(e.to_string()),
                        details: None,
                    },
                }
            }
            Err(e) => ComponentHealth {
                name: self.name().to_string(),
                status: HealthStatus::Unhealthy,
                error: Some(e.to_string()),
                details: None,
            },
        }
    }
}

/// The axum handlers pull the health checker off state through this trait.
pub trait BaseAppState: Clone + Send + Sync + 'static {
    /// Service name (used in health JSON).
    fn service_name(&self) -> &str;

    /// Service version (used in health JSON).
    fn service_version(&self) -> &str;

    /// The health checker handle.
    fn health_checker(&self) -> Arc<dyn HealthCheckHandler>;
}

impl<T: BaseAppState> BaseAppState for Arc<T> {
    fn service_name(&self) -> &str {
        (**self).service_name()
    }

    fn service_version(&self) -> &str {
        (**self).service_version()
    }

    fn health_checker(&self) -> Arc<dyn HealthCheckHandler> {
        (**self).health_checker()
    }
}

/// Axum handler for `GET /health`. Returns the full health JSON.
pub async fn health_check_handler<S: BaseAppState>(State(state): State<S>) -> impl IntoResponse {
    let health = state.health_checker().check_health().await;
    let status_code = match health.status {
        HealthStatus::Healthy | HealthStatus::Degraded => StatusCode::OK,
        HealthStatus::Unhealthy => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status_code, Json(health))
}

/// Axum handler for `GET /ready`. Returns 200 when ready, 503 otherwise.
pub async fn readiness_handler<S: BaseAppState>(State(state): State<S>) -> impl IntoResponse {
    if state.health_checker().check_readiness().await {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}
