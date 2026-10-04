//! Logging and tracing integration for the runner.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Tracing and logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracingConfig {
    /// Log level (trace, debug, info, warn, error).
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Enable JSON formatted logs.
    #[serde(default)]
    pub json_format: bool,
    /// Enable console output.
    #[serde(default = "default_true")]
    pub console_output: bool,
    /// Log file configuration.
    pub file_output: Option<FileOutputConfig>,
    /// Structured logging configuration.
    pub structured_logging: StructuredLoggingConfig,
    /// Tracing configuration.
    pub tracing: TracingSpanConfig,
    /// Performance logging configuration.
    pub performance: PerformanceLoggingConfig,
}

/// File output configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOutputConfig {
    /// Log file path.
    pub path: PathBuf,
    /// Enable log rotation.
    #[serde(default = "default_true")]
    pub rotation_enabled: bool,
    /// Maximum file size in MB before rotation.
    #[serde(default = "default_max_file_size")]
    pub max_file_size_mb: u64,
    /// Number of rotated files to keep.
    #[serde(default = "default_max_files")]
    pub max_files: u32,
    /// Compress rotated files.
    #[serde(default)]
    pub compress: bool,
}

/// Structured logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredLoggingConfig {
    /// Include source code locations.
    #[serde(default)]
    pub include_source_location: bool,
    /// Include thread information.
    #[serde(default)]
    pub include_thread_info: bool,
    /// Include process information.
    #[serde(default = "default_true")]
    pub include_process_info: bool,
    /// Include hostname.
    #[serde(default = "default_true")]
    pub include_hostname: bool,
    /// Additional static fields to include in all log entries.
    #[serde(default)]
    pub static_fields: HashMap<String, String>,
}

/// Tracing span configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracingSpanConfig {
    /// Enable distributed tracing.
    #[serde(default)]
    pub enable_distributed_tracing: bool,
    /// Trace HTTP requests.
    #[serde(default = "default_true")]
    pub trace_http_requests: bool,
    /// Trace database operations.
    #[serde(default = "default_true")]
    pub trace_database_operations: bool,
    /// Trace VCS operations.
    #[serde(default)]
    pub trace_vcs_operations: bool,
    /// Trace queue operations.
    #[serde(default = "default_true")]
    pub trace_queue_operations: bool,
    /// Sample rate for traces (0.0 to 1.0).
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
}

/// Performance logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceLoggingConfig {
    /// Log slow operations.
    #[serde(default = "default_true")]
    pub log_slow_operations: bool,
    /// Threshold for slow operations in milliseconds.
    #[serde(default = "default_slow_operation_threshold")]
    pub slow_operation_threshold_ms: u64,
    /// Log performance metrics.
    #[serde(default)]
    pub log_performance_metrics: bool,
    /// Performance metrics interval in seconds.
    #[serde(default = "default_metrics_interval")]
    pub metrics_interval_seconds: u64,
}

// Default value functions
fn default_log_level() -> String {
    "info".to_string()
}
fn default_true() -> bool {
    true
}
fn default_max_file_size() -> u64 {
    100
} // 100MB
fn default_max_files() -> u32 {
    10
}
fn default_sample_rate() -> f64 {
    1.0
}
fn default_slow_operation_threshold() -> u64 {
    1000
} // 1 second
fn default_metrics_interval() -> u64 {
    60
} // 60 seconds

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            json_format: false,
            console_output: default_true(),
            file_output: None,
            structured_logging: StructuredLoggingConfig::default(),
            tracing: TracingSpanConfig::default(),
            performance: PerformanceLoggingConfig::default(),
        }
    }
}

impl Default for StructuredLoggingConfig {
    fn default() -> Self {
        Self {
            include_source_location: false,
            include_thread_info: false,
            include_process_info: default_true(),
            include_hostname: default_true(),
            static_fields: HashMap::new(),
        }
    }
}

impl Default for TracingSpanConfig {
    fn default() -> Self {
        Self {
            enable_distributed_tracing: false,
            trace_http_requests: default_true(),
            trace_database_operations: default_true(),
            trace_vcs_operations: false,
            trace_queue_operations: default_true(),
            sample_rate: default_sample_rate(),
        }
    }
}

impl Default for PerformanceLoggingConfig {
    fn default() -> Self {
        Self {
            log_slow_operations: default_true(),
            slow_operation_threshold_ms: default_slow_operation_threshold(),
            log_performance_metrics: false,
            metrics_interval_seconds: default_metrics_interval(),
        }
    }
}

/// Initialize tracing and logging system.
pub fn init_tracing(config: &TracingConfig) -> Result<(), TracingError> {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};

    let registry = Registry::default();

    let env_filter = EnvFilter::try_new(&config.log_level)
        .or_else(|_| EnvFilter::try_new("info"))
        .map_err(|e| TracingError::Configuration(format!("Invalid log level: {}", e)))?;

    let registry = registry.with(env_filter);

    // Add console output if enabled
    if config.console_output {
        if config.json_format {
            let json_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(true);
            let registry = registry.with(json_layer);
            registry.try_init().map_err(|e| {
                TracingError::Initialization(format!(
                    "Failed to initialize JSON console logging: {}",
                    e
                ))
            })?;
        } else {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_target(config.structured_logging.include_source_location)
                .with_thread_ids(config.structured_logging.include_thread_info)
                .with_thread_names(config.structured_logging.include_thread_info);
            let registry = registry.with(fmt_layer);
            registry.try_init().map_err(|e| {
                TracingError::Initialization(format!("Failed to initialize console logging: {}", e))
            })?;
        }
    } else {
        registry.try_init().map_err(|e| {
            TracingError::Initialization(format!("Failed to initialize logging: {}", e))
        })?;
    }

    // Initialize structured logging fields
    init_structured_logging(&config.structured_logging)?;

    // Initialize performance monitoring if enabled
    if config.performance.log_performance_metrics {
        init_performance_logging(&config.performance)?;
    }

    log::info!("Tracing and logging system initialized successfully");
    log::info!("Log level: {}", config.log_level);
    log::info!("JSON format: {}", config.json_format);
    log::info!(
        "Distributed tracing: {}",
        config.tracing.enable_distributed_tracing
    );

    Ok(())
}

/// Initialize structured logging with static fields.
fn init_structured_logging(config: &StructuredLoggingConfig) -> Result<(), TracingError> {
    if config.include_hostname {
        if let Ok(hostname) = hostname::get() {
            if let Some(hostname_str) = hostname.to_str() {
                log::info!("Hostname: {}", hostname_str);
            }
        }
    }

    if config.include_process_info {
        log::info!("Process ID: {}", std::process::id());
    }

    // Log static fields
    for (key, value) in &config.static_fields {
        log::info!("Static field {}: {}", key, value);
    }

    Ok(())
}

/// Initialize performance logging.
fn init_performance_logging(config: &PerformanceLoggingConfig) -> Result<(), TracingError> {
    log::info!("Performance logging enabled");
    log::info!(
        "Slow operation threshold: {}ms",
        config.slow_operation_threshold_ms
    );
    log::info!("Metrics interval: {}s", config.metrics_interval_seconds);

    // Start performance metrics logging task
    if config.log_performance_metrics {
        start_performance_metrics_task(config.metrics_interval_seconds);
    }

    Ok(())
}

/// Start background task for logging performance metrics.
fn start_performance_metrics_task(interval_seconds: u64) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_seconds));

        loop {
            interval.tick().await;
            log_performance_metrics().await;
        }
    });
}

/// Log current performance metrics.
async fn log_performance_metrics() {
    if let Ok(memory_info) = get_memory_usage() {
        tracing::info!(
            memory_used_mb = memory_info.used_mb,
            memory_available_mb = memory_info.available_mb,
            "Memory usage metrics"
        );
    }

    if let Ok(cpu_usage) = get_cpu_usage() {
        tracing::info!(cpu_usage_percent = cpu_usage, "CPU usage metrics");
    }

    tracing::info!("Performance metrics logged");
}

/// Memory usage information.
#[derive(Debug)]
struct MemoryInfo {
    used_mb: u64,
    available_mb: u64,
}

/// Get memory usage information.
fn get_memory_usage() -> Result<MemoryInfo, Box<dyn std::error::Error>> {
    // Simplified memory usage calculation
    // In production, you might want to use a more sophisticated approach
    Ok(MemoryInfo {
        used_mb: 100,       // Placeholder
        available_mb: 1024, // Placeholder
    })
}

/// Get CPU usage percentage.
fn get_cpu_usage() -> Result<f64, Box<dyn std::error::Error>> {
    // Simplified CPU usage calculation
    // In production, you might want to use a more sophisticated approach
    Ok(10.0) // Placeholder
}

/// Tracing middleware for HTTP requests.
pub async fn http_tracing_middleware(
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let user_agent = request
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    let start = std::time::Instant::now();

    let span = tracing::info_span!(
        "http_request",
        method = %method,
        uri = %uri,
        user_agent = %user_agent,
    );

    let response = tracing::instrument::Instrument::instrument(next.run(request), span).await;

    let duration = start.elapsed();
    let status = response.status();

    tracing::info!(
        method = ?method,
        uri = ?uri,
        status = %status,
        duration_ms = duration.as_millis(),
        user_agent = %user_agent,
        "HTTP request completed"
    );

    // Log slow requests
    if duration.as_millis() > 1000 {
        tracing::warn!(
            method = ?method,
            uri = ?uri,
            duration_ms = duration.as_millis(),
            "Slow HTTP request detected"
        );
    }

    response
}

/// Database operation tracing.
pub async fn trace_database_operation<F, T>(operation: &str, future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let span = tracing::info_span!("database_operation", operation = operation);
    let start = std::time::Instant::now();

    let result = tracing::instrument::Instrument::instrument(future, span).await;

    let duration = start.elapsed();
    tracing::debug!(
        operation = operation,
        duration_ms = duration.as_millis(),
        "Database operation completed"
    );

    if duration.as_millis() > 1000 {
        tracing::warn!(
            operation = operation,
            duration_ms = duration.as_millis(),
            "Slow database operation detected"
        );
    }

    result
}

/// VCS operation tracing.
pub async fn trace_vcs_operation<F, T>(
    operation: &str,
    vcs_type: &str,
    codebase: &str,
    future: F,
) -> T
where
    F: std::future::Future<Output = T>,
{
    let span = tracing::info_span!(
        "vcs_operation",
        operation = operation,
        vcs_type = vcs_type,
        codebase = codebase,
    );
    let start = std::time::Instant::now();

    let result = tracing::instrument::Instrument::instrument(future, span).await;

    let duration = start.elapsed();
    tracing::debug!(
        operation = operation,
        vcs_type = vcs_type,
        codebase = codebase,
        duration_ms = duration.as_millis(),
        "VCS operation completed"
    );

    if duration.as_millis() > 5000 {
        tracing::warn!(
            operation = operation,
            vcs_type = vcs_type,
            codebase = codebase,
            duration_ms = duration.as_millis(),
            "Slow VCS operation detected"
        );
    }

    result
}

/// Queue operation tracing.
pub async fn trace_queue_operation<F, T>(
    operation: &str,
    queue_id: Option<i64>,
    worker: Option<&str>,
    future: F,
) -> T
where
    F: std::future::Future<Output = T>,
{
    let span = tracing::info_span!(
        "queue_operation",
        operation = operation,
        queue_id = queue_id,
        worker = worker,
    );
    let start = std::time::Instant::now();

    let result = tracing::instrument::Instrument::instrument(future, span).await;

    let duration = start.elapsed();
    tracing::debug!(
        operation = operation,
        queue_id = queue_id,
        worker = worker,
        duration_ms = duration.as_millis(),
        "Queue operation completed"
    );

    result
}

/// Tracing error types.
#[derive(Debug, thiserror::Error)]
pub enum TracingError {
    /// Configuration-related errors.
    #[error("Configuration error: {0}")]
    Configuration(String),

    /// Initialization-related errors.
    #[error("Initialization error: {0}")]
    Initialization(String),

    /// Runtime-related errors.
    #[error("Runtime error: {0}")]
    Runtime(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tracing_config_default() {
        let config = TracingConfig::default();
        assert_eq!(config.log_level, "info");
        assert!(config.console_output);
        assert!(!config.json_format);
    }

    #[test]
    fn test_structured_logging_config() {
        let config = StructuredLoggingConfig::default();
        assert!(config.include_process_info);
        assert!(config.include_hostname);
        assert!(!config.include_source_location);
    }

    #[tokio::test]
    async fn test_trace_database_operation() {
        // Simple test of the tracing wrapper
        let result = trace_database_operation("test_query", async { 42 }).await;
        assert_eq!(result, 42);
    }
}
