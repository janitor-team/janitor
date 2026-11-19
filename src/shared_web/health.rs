//! Standardized health check endpoints for all Janitor services

use axum::{
    extract::State,
    response::IntoResponse,
    http::StatusCode,
    Json,
};
use serde::{Serialize, Deserialize};
use std::sync::Arc;
use async_trait::async_trait;

/// Health status for a service
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// Service is healthy and ready
    Healthy,
    /// Service is degraded but still operational
    Degraded,
    /// Service is unhealthy
    Unhealthy,
}

/// Health check response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    /// Overall health status
    pub status: HealthStatus,
    /// Service name
    pub service: String,
    /// Service version
    pub version: String,
    /// Individual component checks
    pub checks: Vec<ComponentHealth>,
    /// Timestamp of the check
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Health status of an individual component
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentHealth {
    /// Component name
    pub name: String,
    /// Component health status
    pub status: HealthStatus,
    /// Optional error message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Optional additional details
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Trait for implementing health checks
#[async_trait]
pub trait HealthCheckHandler: Send + Sync {
    /// Perform a comprehensive health check
    async fn check_health(&self) -> HealthCheck;
    
    /// Perform a simple liveness check
    async fn check_liveness(&self) -> bool {
        true // Default: if the handler is responding, we're alive
    }
    
    /// Perform a readiness check
    async fn check_readiness(&self) -> bool {
        // Default: ready if healthy
        matches!(self.check_health().await.status, HealthStatus::Healthy)
    }
}

/// Basic health checker that can be extended
pub struct BasicHealthChecker {
    service_name: String,
    service_version: String,
    component_checks: Vec<Arc<dyn ComponentChecker>>,
}

/// Trait for checking individual components
#[async_trait]
pub trait ComponentChecker: Send + Sync {
    /// Get the component name
    fn name(&self) -> &str;
    
    /// Check the component health
    async fn check(&self) -> ComponentHealth;
}

impl BasicHealthChecker {
    /// Create a new basic health checker
    pub fn new() -> Self {
        Self {
            service_name: env!("CARGO_PKG_NAME").to_string(),
            service_version: env!("CARGO_PKG_VERSION").to_string(),
            component_checks: Vec::new(),
        }
    }
    
    /// Create with custom service info
    pub fn with_info(service_name: String, service_version: String) -> Self {
        Self {
            service_name,
            service_version,
            component_checks: Vec::new(),
        }
    }
    
    /// Add a component checker
    pub fn add_component_check(mut self, checker: Arc<dyn ComponentChecker>) -> Self {
        self.component_checks.push(checker);
        self
    }
}

#[async_trait]
impl HealthCheckHandler for BasicHealthChecker {
    async fn check_health(&self) -> HealthCheck {
        let mut checks = Vec::new();
        let mut overall_status = HealthStatus::Healthy;
        
        // Run all component checks
        for checker in &self.component_checks {
            let component_health = checker.check().await;
            
            // Update overall status based on component status
            match component_health.status {
                HealthStatus::Unhealthy => overall_status = HealthStatus::Unhealthy,
                HealthStatus::Degraded => {
                    if overall_status == HealthStatus::Healthy {
                        overall_status = HealthStatus::Degraded;
                    }
                }
                HealthStatus::Healthy => {}
            }
            
            checks.push(component_health);
        }
        
        HealthCheck {
            status: overall_status,
            service: self.service_name.clone(),
            version: self.service_version.clone(),
            checks,
            timestamp: chrono::Utc::now(),
        }
    }
}

/// Database health checker
pub struct DatabaseHealthChecker {
    pool: sqlx::PgPool,
}

impl DatabaseHealthChecker {
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

/// Redis health checker
pub struct RedisHealthChecker {
    client: redis::Client,
}

impl RedisHealthChecker {
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
        // Try to get a connection
        match self.client.get_connection() {
            Ok(mut conn) => {
                // Use redis::cmd to create a PING command
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

/// Uptime health checker
pub struct UptimeHealthChecker {
    start_time: std::time::Instant,
}

impl UptimeHealthChecker {
    pub fn new() -> Self {
        Self {
            start_time: std::time::Instant::now(),
        }
    }
    
    pub fn uptime(&self) -> std::time::Duration {
        self.start_time.elapsed()
    }
}

#[async_trait]
impl ComponentChecker for UptimeHealthChecker {
    fn name(&self) -> &str {
        "uptime"
    }
    
    async fn check(&self) -> ComponentHealth {
        let uptime = self.uptime();
        
        ComponentHealth {
            name: self.name().to_string(),
            status: HealthStatus::Healthy,
            error: None,
            details: Some(serde_json::json!({
                "uptime_seconds": uptime.as_secs(),
                "uptime_human": format!("{}d {}h {}m {}s", 
                    uptime.as_secs() / 86400,
                    (uptime.as_secs() % 86400) / 3600,
                    (uptime.as_secs() % 3600) / 60,
                    uptime.as_secs() % 60
                )
            })),
        }
    }
}

/// Health check handler
pub async fn health_check_handler<S: super::BaseAppState>(
    State(state): State<S>,
) -> impl IntoResponse {
    let health_check = state.health_checker().check_health().await;
    let status_code = match health_check.status {
        HealthStatus::Healthy => StatusCode::OK,
        HealthStatus::Degraded => StatusCode::OK, // Still return 200 for degraded
        HealthStatus::Unhealthy => StatusCode::SERVICE_UNAVAILABLE,
    };
    
    (status_code, Json(health_check))
}

/// Liveness handler (for k8s liveness probe)
pub async fn liveness_handler<S: super::BaseAppState>(
    State(state): State<S>,
) -> impl IntoResponse {
    if state.health_checker().check_liveness().await {
        (StatusCode::OK, "alive")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not alive")
    }
}

/// Readiness handler (for k8s readiness probe)
pub async fn readiness_handler<S: super::BaseAppState>(
    State(state): State<S>,
) -> impl IntoResponse {
    if state.health_checker().check_readiness().await {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_basic_health_checker() {
        let checker = BasicHealthChecker::new();
        let health = checker.check_health().await;
        
        assert_eq!(health.status, HealthStatus::Healthy);
        assert!(health.checks.is_empty());
    }
    
    struct MockHealthyComponent;
    
    #[async_trait]
    impl ComponentChecker for MockHealthyComponent {
        fn name(&self) -> &str {
            "mock_healthy"
        }
        
        async fn check(&self) -> ComponentHealth {
            ComponentHealth {
                name: self.name().to_string(),
                status: HealthStatus::Healthy,
                error: None,
                details: None,
            }
        }
    }
    
    struct MockUnhealthyComponent;
    
    #[async_trait]
    impl ComponentChecker for MockUnhealthyComponent {
        fn name(&self) -> &str {
            "mock_unhealthy"
        }
        
        async fn check(&self) -> ComponentHealth {
            ComponentHealth {
                name: self.name().to_string(),
                status: HealthStatus::Unhealthy,
                error: Some("Mock error".to_string()),
                details: None,
            }
        }
    }
    
    #[tokio::test]
    async fn test_health_checker_with_components() {
        let checker = BasicHealthChecker::new()
            .add_component_check(Arc::new(MockHealthyComponent))
            .add_component_check(Arc::new(MockUnhealthyComponent));
        
        let health = checker.check_health().await;
        
        assert_eq!(health.status, HealthStatus::Unhealthy);
        assert_eq!(health.checks.len(), 2);
        assert_eq!(health.checks[0].status, HealthStatus::Healthy);
        assert_eq!(health.checks[1].status, HealthStatus::Unhealthy);
    }
    
    #[tokio::test]
    async fn test_uptime_health_checker() {
        let checker = UptimeHealthChecker::new();
        
        // Sleep briefly to ensure non-zero uptime
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        
        let health = checker.check().await;
        
        assert_eq!(health.name, "uptime");
        assert_eq!(health.status, HealthStatus::Healthy);
        assert!(health.error.is_none());
        assert!(health.details.is_some());
        
        // Verify uptime details structure
        let details = health.details.unwrap();
        assert!(details.get("uptime_seconds").is_some());
        assert!(details.get("uptime_human").is_some());
    }
    
    
}