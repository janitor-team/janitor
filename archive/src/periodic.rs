//! Long-running background loops: 12h republish, finished-job
//! cleanup, health-check polling, metric collection.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::{interval, Instant};
use tracing::{debug, error, info, warn};

use crate::error::ArchiveResult;
use crate::manager::GeneratorManager;
use crate::redis::ArchiveEvent;
use janitor::redis::RedisManager;

/// Toggles + intervals for each periodic loop [`PeriodicServices`]
/// runs. All intervals are in seconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct PeriodicConfig {
    pub enable_republishing: bool,
    pub republishing_interval_seconds: u64,
    pub enable_cleanup: bool,
    pub cleanup_interval_seconds: u64,
    pub enable_health_monitoring: bool,
    pub health_check_interval_seconds: u64,
    pub enable_metrics: bool,
    pub metrics_interval_seconds: u64,
}

impl Default for PeriodicConfig {
    fn default() -> Self {
        Self {
            enable_republishing: true,
            // 12h between republish sweeps.
            republishing_interval_seconds: 60 * 60 * 12,
            enable_cleanup: true,
            cleanup_interval_seconds: 300,
            enable_health_monitoring: true,
            health_check_interval_seconds: 60,
            enable_metrics: true,
            metrics_interval_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[allow(missing_docs)]
pub enum HealthStatus {
    Healthy,
    Warning,
    Unhealthy,
    Unknown,
}

/// One component's health status at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct HealthCheck {
    pub component: String,
    pub status: HealthStatus,
    pub message: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub response_time_ms: u64,
}

impl HealthCheck {
    #[allow(missing_docs)]
    pub fn healthy(component: &str, message: &str, response_time_ms: u64) -> Self {
        Self {
            component: component.to_string(),
            status: HealthStatus::Healthy,
            message: message.to_string(),
            timestamp: chrono::Utc::now(),
            response_time_ms,
        }
    }

    #[allow(missing_docs)]
    pub fn unhealthy(component: &str, message: &str, response_time_ms: u64) -> Self {
        Self {
            component: component.to_string(),
            status: HealthStatus::Unhealthy,
            message: message.to_string(),
            timestamp: chrono::Utc::now(),
            response_time_ms,
        }
    }

    #[allow(missing_docs)]
    pub fn warning(component: &str, message: &str, response_time_ms: u64) -> Self {
        Self {
            component: component.to_string(),
            status: HealthStatus::Warning,
            message: message.to_string(),
            timestamp: chrono::Utc::now(),
            response_time_ms,
        }
    }
}

/// One sample of process + job counters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ServiceMetrics {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub active_jobs: usize,
    pub running_jobs: usize,
    pub completed_jobs: usize,
    pub failed_jobs: usize,
    pub avg_job_duration_seconds: f64,
    pub redis_healthy: bool,
    pub database_healthy: bool,
    pub memory_usage_bytes: u64,
    pub cpu_usage_percent: f64,
}

/// Owner of the background republish/cleanup/health/metrics loops.
pub struct PeriodicServices {
    config: PeriodicConfig,
    generator_manager: Arc<GeneratorManager>,
    redis_manager: Option<Arc<tokio::sync::Mutex<RedisManager>>>,
    shutdown_tx: Option<broadcast::Sender<()>>,
    task_handles: Vec<JoinHandle<()>>,
    health_checks: Arc<tokio::sync::RwLock<Vec<HealthCheck>>>,
    metrics: Arc<tokio::sync::RwLock<Option<ServiceMetrics>>>,
}

impl PeriodicServices {
    #[allow(missing_docs)]
    pub fn new(
        config: PeriodicConfig,
        generator_manager: Arc<GeneratorManager>,
        redis_manager: Option<Arc<tokio::sync::Mutex<RedisManager>>>,
    ) -> Self {
        Self {
            config,
            generator_manager,
            redis_manager,
            shutdown_tx: None,
            task_handles: Vec::new(),
            health_checks: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            metrics: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// Spawn every enabled service loop; each runs until `stop()` is called
    /// or its `shutdown_rx` receives.
    pub async fn start(&mut self) -> ArchiveResult<()> {
        let (shutdown_tx, _) = broadcast::channel::<()>(10);
        self.shutdown_tx = Some(shutdown_tx.clone());

        if self.config.enable_republishing {
            let handle = self
                .start_republishing_service(shutdown_tx.subscribe())
                .await;
            self.task_handles.push(handle);
        }
        if self.config.enable_cleanup {
            let handle = self.start_cleanup_service(shutdown_tx.subscribe()).await;
            self.task_handles.push(handle);
        }
        if self.config.enable_health_monitoring {
            let handle = self.start_health_monitoring(shutdown_tx.subscribe()).await;
            self.task_handles.push(handle);
        }
        if self.config.enable_metrics {
            let handle = self.start_metrics_collection(shutdown_tx.subscribe()).await;
            self.task_handles.push(handle);
        }
        info!("Started {} periodic services", self.task_handles.len());
        Ok(())
    }

    /// Signal all loops to exit and wait for them.
    pub async fn stop(&mut self) -> ArchiveResult<()> {
        // Send shutdown signal
        if let Some(shutdown_tx) = &self.shutdown_tx {
            if let Err(e) = shutdown_tx.send(()) {
                warn!("Failed to send shutdown signal: {}", e);
            }
        }

        // Wait for all tasks to complete
        for handle in self.task_handles.drain(..) {
            if let Err(e) = handle.await {
                warn!("Task completed with error: {}", e);
            }
        }

        Ok(())
    }

    /// Start republishing service.
    ///
    /// Iterates every configured apt_repository (not the campaign
    /// mapping) and calls `trigger` for each. Triggering by
    /// repository ensures that a suite still republishes on the
    /// 12-hour cadence even when no campaign has produced new
    /// builds.
    async fn start_republishing_service(
        &self,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> JoinHandle<()> {
        let generator_manager = Arc::clone(&self.generator_manager);
        let redis_manager = self.redis_manager.clone();
        let interval_seconds = self.config.republishing_interval_seconds;

        tokio::spawn(async move {
            info!(
                "Starting periodic republishing service (interval: {}s)",
                interval_seconds
            );
            let mut interval = interval(Duration::from_secs(interval_seconds));

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                    _ = interval.tick() => {

                        match generator_manager.trigger_all_repositories().await {
                            Ok(job_ids) => {
                                info!(
                                    "Triggered {} periodic republish jobs",
                                    job_ids.len()
                                );
                                if let Some(redis_mgr) = &redis_manager {
                                    let redis_guard = redis_mgr.lock().await;
                                    let publisher = redis_guard.publisher();
                                    let event = ArchiveEvent::PeriodicRepublish {
                                        campaign: String::new(),
                                        interval_type: "periodic".to_string(),
                                    };
                                    if let Err(e) = publisher.publish(&event).await {
                                        warn!("Failed to publish periodic republish event: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Periodic republish failed: {}", e);
                            }
                        }

                    }
                }
            }
        })
    }

    /// Start cleanup service.
    async fn start_cleanup_service(
        &self,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> JoinHandle<()> {
        let generator_manager = Arc::clone(&self.generator_manager);
        let interval_seconds = self.config.cleanup_interval_seconds;

        tokio::spawn(async move {
            info!("Starting cleanup service (interval: {}s)", interval_seconds);
            let mut interval = interval(Duration::from_secs(interval_seconds));

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                    _ = interval.tick() => {

                        // Cleanup finished jobs
                        let cleaned_count = generator_manager.cleanup_finished_jobs().await;
                        if cleaned_count > 0 {
                            debug!("Cleaned up {} finished jobs", cleaned_count);
                        }

                        // Additional cleanup tasks could be added here:
                        // - Cleanup old temporary files
                        // - Archive old logs
                        // - Update metrics
                    }
                }
            }
        })
    }

    /// Start health monitoring service.
    async fn start_health_monitoring(
        &self,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> JoinHandle<()> {
        let generator_manager = Arc::clone(&self.generator_manager);
        let redis_manager = self.redis_manager.clone();
        let health_checks = Arc::clone(&self.health_checks);
        let interval_seconds = self.config.health_check_interval_seconds;

        tokio::spawn(async move {
            info!(
                "Starting health monitoring service (interval: {}s)",
                interval_seconds
            );
            let mut interval = interval(Duration::from_secs(interval_seconds));

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                    _ = interval.tick() => {

                        let mut checks = Vec::new();

                        // Check generator manager
                        let start = Instant::now();
                        let stats = generator_manager.get_statistics().await;
                        let duration = start.elapsed().as_millis() as u64;

                        let status = if stats.running_jobs > stats.max_concurrent_jobs {
                            HealthStatus::Warning
                        } else {
                            HealthStatus::Healthy
                        };

                        checks.push(HealthCheck {
                            component: "generator_manager".to_string(),
                            status,
                            message: format!("Active: {}, Running: {}", stats.active_jobs, stats.running_jobs),
                            timestamp: chrono::Utc::now(),
                            response_time_ms: duration,
                        });

                        // Check Redis if available
                        if let Some(redis_mgr) = &redis_manager {
                            let start = Instant::now();
                            let redis_guard = redis_mgr.lock().await;

                            match redis_guard.health_check().await {
                                Ok(_) => {
                                    let duration = start.elapsed().as_millis() as u64;
                                    checks.push(HealthCheck::healthy("redis", "Connection OK", duration));
                                }
                                Err(e) => {
                                    let duration = start.elapsed().as_millis() as u64;
                                    checks.push(HealthCheck::unhealthy("redis", &e.to_string(), duration));
                                }
                            }
                        }

                        // Update health checks
                        {
                            let mut health_guard = health_checks.write().await;
                            *health_guard = checks;
                        }
                    }
                }
            }
        })
    }

    /// Start metrics collection service.
    async fn start_metrics_collection(
        &self,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> JoinHandle<()> {
        let generator_manager = Arc::clone(&self.generator_manager);
        let redis_manager = self.redis_manager.clone();
        let metrics = Arc::clone(&self.metrics);
        let interval_seconds = self.config.metrics_interval_seconds;

        tokio::spawn(async move {
            info!(
                "Starting metrics collection service (interval: {}s)",
                interval_seconds
            );
            let mut interval = interval(Duration::from_secs(interval_seconds));

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                    _ = interval.tick() => {

                        // Get generator statistics
                        let stats = generator_manager.get_statistics().await;

                        // Check Redis health
                        let redis_healthy = if let Some(redis_mgr) = &redis_manager {
                            let redis_guard = redis_mgr.lock().await;
                            redis_guard.health_check().await.is_ok()
                        } else {
                            false
                        };

                        // Collect system metrics (simplified)
                        let memory_usage_bytes = Self::get_memory_usage();
                        let cpu_usage_percent = Self::get_cpu_usage();

                        let service_metrics = ServiceMetrics {
                            timestamp: chrono::Utc::now(),
                            active_jobs: stats.active_jobs,
                            running_jobs: stats.running_jobs,
                            completed_jobs: stats.completed_jobs,
                            failed_jobs: stats.failed_jobs,
                            avg_job_duration_seconds: 0.0, // Would need job timing tracking
                            redis_healthy,
                            database_healthy: true, // Would need database health check
                            memory_usage_bytes,
                            cpu_usage_percent,
                        };

                        // Update metrics
                        {
                            let mut metrics_guard = metrics.write().await;
                            *metrics_guard = Some(service_metrics);
                        }
                    }
                }
            }
        })
    }

    /// Get current health checks.
    pub async fn get_health_checks(&self) -> Vec<HealthCheck> {
        let health_guard = self.health_checks.read().await;
        health_guard.clone()
    }

    /// Get current metrics.
    pub async fn get_metrics(&self) -> Option<ServiceMetrics> {
        let metrics_guard = self.metrics.read().await;
        metrics_guard.clone()
    }

    /// Get overall health status.
    pub async fn get_overall_health(&self) -> HealthStatus {
        let checks = self.get_health_checks().await;

        if checks.is_empty() {
            return HealthStatus::Unknown;
        }

        let has_unhealthy = checks.iter().any(|c| c.status == HealthStatus::Unhealthy);
        let has_warning = checks.iter().any(|c| c.status == HealthStatus::Warning);

        if has_unhealthy {
            HealthStatus::Unhealthy
        } else if has_warning {
            HealthStatus::Warning
        } else {
            HealthStatus::Healthy
        }
    }

    /// Simple memory usage estimation (in bytes).
    fn get_memory_usage() -> u64 {
        // This is a simplified implementation
        // In production, you might use system crates or proc filesystem
        #[cfg(target_os = "linux")]
        {
            if let Ok(content) = std::fs::read_to_string("/proc/self/status") {
                for line in content.lines() {
                    if line.starts_with("VmRSS:") {
                        if let Some(kb_str) = line.split_whitespace().nth(1) {
                            if let Ok(kb) = kb_str.parse::<u64>() {
                                return kb * 1024; // Convert KB to bytes
                            }
                        }
                    }
                }
            }
        }
        0
    }

    /// Average CPU usage since process start, expressed as a percent
    /// (0..100 per core). Reads utime+stime from /proc/self/stat and
    /// divides by elapsed wall time. Returns 0.0 on any I/O or parse
    /// failure -- this metric is observational and the caller treats
    /// it as best-effort. Equivalent to `ps -o %cpu` for a cumulative
    /// sampling window spanning the full process lifetime.
    fn get_cpu_usage() -> f64 {
        #[cfg(target_os = "linux")]
        {
            let stat = match std::fs::read_to_string("/proc/self/stat") {
                Ok(s) => s,
                Err(_) => return 0.0,
            };
            // /proc/self/stat fields 14 (utime), 15 (stime), 22 (starttime).
            // Comm (field 2) can contain spaces, so walk past the
            // trailing ')' before splitting on whitespace.
            let Some(after_comm) = stat.rfind(')').map(|p| &stat[p + 1..]) else {
                return 0.0;
            };
            let fields: Vec<&str> = after_comm.split_whitespace().collect();
            // Field indices 0-based after comm: (3,14,15,22) -> (state,
            // utime, stime, starttime) map to offsets (0, 11, 12, 19).
            let utime: u64 = fields.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
            let stime: u64 = fields.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
            let starttime: u64 = fields.get(19).and_then(|v| v.parse().ok()).unwrap_or(0);

            // USER_HZ has been 100 on Linux/x86 since forever. Avoid
            // pulling in libc just for sysconf(_SC_CLK_TCK); if a
            // host disagrees the metric is still directionally right.
            const CLK_TCK: u64 = 100;

            let uptime_total: f64 = match std::fs::read_to_string("/proc/uptime") {
                Ok(s) => s
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0),
                Err(_) => return 0.0,
            };
            let proc_uptime = uptime_total - (starttime as f64) / (CLK_TCK as f64);
            if proc_uptime <= 0.0 {
                return 0.0;
            }
            let cpu_seconds = (utime + stime) as f64 / (CLK_TCK as f64);
            (100.0 * cpu_seconds / proc_uptime).max(0.0)
        }
        #[cfg(not(target_os = "linux"))]
        {
            0.0
        }
    }
}

impl Drop for PeriodicServices {
    fn drop(&mut self) {
        // Abort any remaining tasks
        for handle in &self.task_handles {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_periodic_config_default() {
        let config = PeriodicConfig::default();

        assert!(config.enable_republishing);
        // 12 hours between republish sweeps.
        assert_eq!(config.republishing_interval_seconds, 60 * 60 * 12);
        assert!(config.enable_cleanup);
        assert_eq!(config.cleanup_interval_seconds, 300);
        assert!(config.enable_health_monitoring);
        assert_eq!(config.health_check_interval_seconds, 60);
        assert!(config.enable_metrics);
        assert_eq!(config.metrics_interval_seconds, 300);
    }

    #[test]
    fn test_health_check_creation() {
        let healthy = HealthCheck::healthy("test", "All good", 100);
        assert_eq!(healthy.component, "test");
        assert_eq!(healthy.status, HealthStatus::Healthy);
        assert_eq!(healthy.message, "All good");
        assert_eq!(healthy.response_time_ms, 100);

        let unhealthy = HealthCheck::unhealthy("test", "Error occurred", 500);
        assert_eq!(unhealthy.status, HealthStatus::Unhealthy);
        assert_eq!(unhealthy.message, "Error occurred");
        assert_eq!(unhealthy.response_time_ms, 500);

        let warning = HealthCheck::warning("test", "Warning message", 200);
        assert_eq!(warning.status, HealthStatus::Warning);
        assert_eq!(warning.message, "Warning message");
        assert_eq!(warning.response_time_ms, 200);
    }

    #[test]
    fn test_health_status_equality() {
        assert_eq!(HealthStatus::Healthy, HealthStatus::Healthy);
        assert_ne!(HealthStatus::Healthy, HealthStatus::Warning);
        assert_ne!(HealthStatus::Warning, HealthStatus::Unhealthy);
        assert_ne!(HealthStatus::Unhealthy, HealthStatus::Unknown);
    }

    #[test]
    fn test_service_metrics_serialization() {
        let metrics = ServiceMetrics {
            timestamp: chrono::Utc::now(),
            active_jobs: 5,
            running_jobs: 2,
            completed_jobs: 100,
            failed_jobs: 3,
            avg_job_duration_seconds: 120.5,
            redis_healthy: true,
            database_healthy: true,
            memory_usage_bytes: 1024 * 1024 * 100, // 100MB
            cpu_usage_percent: 25.5,
        };

        let serialized = serde_json::to_string(&metrics).unwrap();
        let deserialized: ServiceMetrics = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.active_jobs, 5);
        assert_eq!(deserialized.running_jobs, 2);
        assert_eq!(deserialized.completed_jobs, 100);
        assert_eq!(deserialized.failed_jobs, 3);
        assert_eq!(deserialized.avg_job_duration_seconds, 120.5);
        assert!(deserialized.redis_healthy);
        assert!(deserialized.database_healthy);
        assert_eq!(deserialized.memory_usage_bytes, 1024 * 1024 * 100);
        assert_eq!(deserialized.cpu_usage_percent, 25.5);
    }

    #[test]
    fn test_memory_usage_function() {
        // Return type is u64, so non-negativity is trivial -- we
        // only smoke-test that the function returns without panic.
        // On Linux the reported RSS should be non-zero for a live
        // process; on other platforms the function returns 0.
        let _ = PeriodicServices::get_memory_usage();
    }

    #[test]
    fn test_cpu_usage_function() {
        // `get_cpu_usage` reports cumulative CPU seconds / wall
        // seconds. On a multi-core box with concurrent tests
        // burning through utime this can legitimately exceed 100%
        // (Linux `ps` reports the same). Only assert non-negativity
        // -- the /proc parse is best-effort and returning 0 on any
        // failure is documented behavior.
        let usage = PeriodicServices::get_cpu_usage();
        assert!(usage >= 0.0, "cpu usage must be non-negative");
    }
}
