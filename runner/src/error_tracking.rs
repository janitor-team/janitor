//! Error tracking and logging for the runner.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Central error tracking system for the runner.
pub struct ErrorTracker {
    error_storage: Arc<RwLock<ErrorStorage>>,
    config: ErrorTrackingConfig,
}

/// Configuration for error tracking.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ErrorTrackingConfig {
    /// Maximum number of errors to keep in memory.
    pub max_errors_in_memory: usize,
    /// Whether to log errors to file.
    pub log_to_file: bool,
    /// Error log file path.
    pub error_log_path: Option<std::path::PathBuf>,
    /// Enable detailed stack traces.
    pub enable_stack_traces: bool,
    /// Minimum severity level to track.
    pub min_severity: ErrorSeverity,
    /// Enable error correlation by session/context.
    pub enable_correlation: bool,
}

impl Default for ErrorTrackingConfig {
    fn default() -> Self {
        Self {
            max_errors_in_memory: 10000,
            log_to_file: true,
            error_log_path: Some(std::path::PathBuf::from(
                "/var/log/janitor/runner-errors.log",
            )),
            enable_stack_traces: true,
            min_severity: ErrorSeverity::Warning,
            enable_correlation: true,
        }
    }
}

/// Error severity levels.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum ErrorSeverity {
    /// Debug-level error.
    Debug = 0,
    /// Informational error.
    Info = 1,
    /// Warning-level error.
    Warning = 2,
    /// Standard error.
    Error = 3,
    /// Critical error requiring attention.
    Critical = 4,
    /// Fatal error that causes system failure.
    Fatal = 5,
}

impl fmt::Display for ErrorSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorSeverity::Debug => write!(f, "DEBUG"),
            ErrorSeverity::Info => write!(f, "INFO"),
            ErrorSeverity::Warning => write!(f, "WARNING"),
            ErrorSeverity::Error => write!(f, "ERROR"),
            ErrorSeverity::Critical => write!(f, "CRITICAL"),
            ErrorSeverity::Fatal => write!(f, "FATAL"),
        }
    }
}

/// Error categories for classification.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ErrorCategory {
    /// Database-related errors.
    Database,
    /// Network connectivity errors.
    Network,
    /// File system errors.
    FileSystem,
    /// Version control system errors.
    VCS,
    /// Queue processing errors.
    Queue,
    /// Worker-related errors.
    Worker,
    /// Configuration errors.
    Configuration,
    /// Authentication errors.
    Authentication,
    /// Rate limiting errors.
    RateLimit,
    /// Timeout errors.
    Timeout,
    /// Resource exhaustion errors.
    Resource,
    /// Business logic errors.
    Business,
    /// Unknown or uncategorized errors.
    Unknown,
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorCategory::Database => write!(f, "database"),
            ErrorCategory::Network => write!(f, "network"),
            ErrorCategory::FileSystem => write!(f, "filesystem"),
            ErrorCategory::VCS => write!(f, "vcs"),
            ErrorCategory::Queue => write!(f, "queue"),
            ErrorCategory::Worker => write!(f, "worker"),
            ErrorCategory::Configuration => write!(f, "configuration"),
            ErrorCategory::Authentication => write!(f, "authentication"),
            ErrorCategory::RateLimit => write!(f, "rate_limit"),
            ErrorCategory::Timeout => write!(f, "timeout"),
            ErrorCategory::Resource => write!(f, "resource"),
            ErrorCategory::Business => write!(f, "business"),
            ErrorCategory::Unknown => write!(f, "unknown"),
        }
    }
}

/// Tracked error entry.
#[derive(Debug, Clone)]
pub struct TrackedError {
    /// Unique error identifier.
    pub id: String,
    /// When the error occurred.
    pub timestamp: DateTime<Utc>,
    /// Error severity level.
    pub severity: ErrorSeverity,
    /// Error category for classification.
    pub category: ErrorCategory,
    /// Component where the error occurred.
    pub component: String,
    /// Operation being performed when error occurred.
    pub operation: String,
    /// Error message.
    pub message: String,
    /// Additional error details.
    pub details: Option<String>,
    /// Stack trace if available.
    pub stack_trace: Option<String>,
    /// Additional context key-value pairs.
    pub context: HashMap<String, String>,
    /// Correlation ID for tracking related errors.
    pub correlation_id: Option<String>,
    /// User ID if applicable.
    pub user_id: Option<String>,
    /// Request ID if applicable.
    pub request_id: Option<String>,
    /// Number of retry attempts.
    pub retry_count: u32,
    /// Whether this error is transient.
    pub is_transient: bool,
}

/// Error pattern for detecting recurring issues.
#[derive(Debug, Clone)]
pub struct ErrorPattern {
    /// Error category.
    pub category: ErrorCategory,
    /// Component experiencing errors.
    pub component: String,
    /// Operation that's failing.
    pub operation: String,
    /// Total count of this error pattern.
    pub count: u64,
    /// When this pattern was first seen.
    pub first_seen: DateTime<Utc>,
    /// When this pattern was last seen.
    pub last_seen: DateTime<Utc>,
    /// Error rate per hour.
    pub rate_per_hour: f64,
    /// Sample error IDs for investigation.
    pub examples: Vec<String>,
}

/// Error storage and indexing.
#[derive(Debug, Default)]
struct ErrorStorage {
    errors: Vec<TrackedError>,
    errors_by_category: HashMap<ErrorCategory, Vec<String>>, // error IDs
    errors_by_component: HashMap<String, Vec<String>>,
    patterns: HashMap<String, ErrorPattern>, // pattern key -> pattern
    error_counts: HashMap<String, u64>,      // category:component:operation -> count
}

impl ErrorTracker {
    /// Create a new error tracker.
    pub fn new(config: ErrorTrackingConfig) -> Self {
        Self {
            error_storage: Arc::new(RwLock::new(ErrorStorage::default())),
            config,
        }
    }

    /// Track a new error.
    pub async fn track_error(&self, mut error: TrackedError) {
        // Generate ID if not provided
        if error.id.is_empty() {
            error.id = uuid::Uuid::new_v4().to_string();
        }

        // Filter by severity
        if error.severity < self.config.min_severity {
            return;
        }

        self.log_error(&error).await;

        crate::metrics::ERROR_OCCURRENCES_TOTAL
            .with_label_values(&[
                &error.category.to_string(),
                &error.component,
                &error.severity.to_string(),
            ])
            .inc();

        // Store the error
        let mut storage = self.error_storage.write().await;

        // Add to main storage
        storage.errors.push(error.clone());

        // Maintain size limit
        if storage.errors.len() > self.config.max_errors_in_memory {
            let remove_count = storage.errors.len() - self.config.max_errors_in_memory;
            storage.errors.drain(0..remove_count);
        }

        // Index by category
        storage
            .errors_by_category
            .entry(error.category.clone())
            .or_default()
            .push(error.id.clone());

        // Index by component
        storage
            .errors_by_component
            .entry(error.component.clone())
            .or_default()
            .push(error.id.clone());

        let pattern_key = format!("{}:{}:{}", error.category, error.component, error.operation);

        // First, ensure the pattern exists and get necessary data
        let (first_seen, _count) = {
            let pattern = storage
                .patterns
                .entry(pattern_key.clone())
                .or_insert_with(|| ErrorPattern {
                    category: error.category.clone(),
                    component: error.component.clone(),
                    operation: error.operation.clone(),
                    count: 0,
                    first_seen: error.timestamp,
                    last_seen: error.timestamp,
                    rate_per_hour: 0.0,
                    examples: Vec::new(),
                });

            pattern.count += 1;
            pattern.last_seen = error.timestamp;
            if pattern.examples.len() < 5 {
                pattern.examples.push(error.id.clone());
            }

            (pattern.first_seen, pattern.count)
        };

        // Update count tracking (separate borrow)
        *storage.error_counts.entry(pattern_key.clone()).or_insert(0) += 1;

        // Calculate rate (separate borrow)
        let hours_elapsed = (error.timestamp - first_seen).num_seconds() as f64 / 3600.0;
        if hours_elapsed > 0.0 {
            if let Some(pattern) = storage.patterns.get_mut(&pattern_key) {
                pattern.rate_per_hour = pattern.count as f64 / hours_elapsed;
            }
        }
    }

    /// Log error to configured outputs.
    async fn log_error(&self, error: &TrackedError) {
        // Always log to standard logging
        match error.severity {
            ErrorSeverity::Debug => log::debug!(
                "[{}:{}] {} - {}",
                error.category,
                error.component,
                error.operation,
                error.message
            ),
            ErrorSeverity::Info => log::info!(
                "[{}:{}] {} - {}",
                error.category,
                error.component,
                error.operation,
                error.message
            ),
            ErrorSeverity::Warning => log::warn!(
                "[{}:{}] {} - {}",
                error.category,
                error.component,
                error.operation,
                error.message
            ),
            ErrorSeverity::Error => log::error!(
                "[{}:{}] {} - {}",
                error.category,
                error.component,
                error.operation,
                error.message
            ),
            ErrorSeverity::Critical => log::error!(
                "CRITICAL [{}:{}] {} - {}",
                error.category,
                error.component,
                error.operation,
                error.message
            ),
            ErrorSeverity::Fatal => log::error!(
                "FATAL [{}:{}] {} - {}",
                error.category,
                error.component,
                error.operation,
                error.message
            ),
        }

        // Log to file if configured
        if self.config.log_to_file {
            if let Some(ref log_path) = self.config.error_log_path {
                if let Err(e) = self.log_error_to_file(error, log_path).await {
                    log::error!("Failed to write error to log file: {}", e);
                }
            }
        }
    }

    /// Log error to file.
    async fn log_error_to_file(
        &self,
        error: &TrackedError,
        log_path: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use tokio::io::AsyncWriteExt;

        let log_entry = serde_json::json!({
            "timestamp": error.timestamp.to_rfc3339(),
            "id": error.id,
            "severity": error.severity.to_string(),
            "category": error.category.to_string(),
            "component": error.component,
            "operation": error.operation,
            "message": error.message,
            "details": error.details,
            "context": error.context,
            "correlation_id": error.correlation_id,
            "user_id": error.user_id,
            "request_id": error.request_id,
            "retry_count": error.retry_count,
            "is_transient": error.is_transient,
            "stack_trace": if self.config.enable_stack_traces { error.stack_trace.as_ref() } else { None }
        });

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .await?;

        file.write_all(log_entry.to_string().as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;

        Ok(())
    }

    /// Get error statistics.
    pub async fn get_error_statistics(&self) -> ErrorStatistics {
        let storage = self.error_storage.read().await;

        let now = Utc::now();
        let one_hour_ago = now - chrono::Duration::hours(1);
        let one_day_ago = now - chrono::Duration::days(1);

        let total_errors = storage.errors.len();
        let errors_last_hour = storage
            .errors
            .iter()
            .filter(|e| e.timestamp >= one_hour_ago)
            .count();
        let errors_last_day = storage
            .errors
            .iter()
            .filter(|e| e.timestamp >= one_day_ago)
            .count();

        let mut by_category = HashMap::new();
        let mut by_severity = HashMap::new();
        let mut by_component = HashMap::new();

        for error in &storage.errors {
            *by_category.entry(error.category.clone()).or_insert(0) += 1;
            *by_severity.entry(error.severity).or_insert(0) += 1;
            *by_component.entry(error.component.clone()).or_insert(0) += 1;
        }

        ErrorStatistics {
            total_errors,
            errors_last_hour,
            errors_last_day,
            by_category,
            by_severity,
            by_component,
            top_patterns: storage
                .patterns
                .values()
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .take(10)
                .collect(),
        }
    }

    /// Get errors by category.
    pub async fn get_errors_by_category(&self, category: ErrorCategory) -> Vec<TrackedError> {
        let storage = self.error_storage.read().await;

        if let Some(error_ids) = storage.errors_by_category.get(&category) {
            storage
                .errors
                .iter()
                .filter(|e| error_ids.contains(&e.id))
                .cloned()
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Get recent errors.
    pub async fn get_recent_errors(&self, limit: usize) -> Vec<TrackedError> {
        let storage = self.error_storage.read().await;

        storage.errors.iter().rev().take(limit).cloned().collect()
    }

    /// Clear old errors.
    pub async fn cleanup_old_errors(&self, max_age: chrono::Duration) {
        let mut storage = self.error_storage.write().await;
        let cutoff = Utc::now() - max_age;

        storage.errors.retain(|e| e.timestamp >= cutoff);

        // Rebuild indices
        storage.errors_by_category.clear();
        storage.errors_by_component.clear();

        // Collect data first to avoid borrow conflicts
        let error_data: Vec<(String, ErrorCategory, String)> = storage
            .errors
            .iter()
            .map(|e| (e.id.clone(), e.category.clone(), e.component.clone()))
            .collect();

        for (id, category, component) in error_data {
            storage
                .errors_by_category
                .entry(category)
                .or_default()
                .push(id.clone());

            storage
                .errors_by_component
                .entry(component)
                .or_default()
                .push(id);
        }
    }

    /// Create a tracked error from standard error.
    pub fn create_tracked_error(
        &self,
        error: &dyn std::error::Error,
        category: ErrorCategory,
        component: &str,
        operation: &str,
    ) -> TrackedError {
        TrackedError {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            severity: ErrorSeverity::Error,
            category,
            component: component.to_string(),
            operation: operation.to_string(),
            message: error.to_string(),
            details: Some(format!("{:?}", error)),
            stack_trace: if self.config.enable_stack_traces {
                Some(format!("{:?}", error))
            } else {
                None
            },
            context: HashMap::new(),
            correlation_id: None,
            user_id: None,
            request_id: None,
            retry_count: 0,
            is_transient: false,
        }
    }
}

/// Error statistics summary.
#[derive(Debug, Clone)]
pub struct ErrorStatistics {
    /// Total number of errors.
    pub total_errors: usize,
    /// Errors in the last hour.
    pub errors_last_hour: usize,
    /// Errors in the last day.
    pub errors_last_day: usize,
    /// Error counts by category.
    pub by_category: HashMap<ErrorCategory, u64>,
    /// Error counts by severity.
    pub by_severity: HashMap<ErrorSeverity, u64>,
    /// Error counts by component.
    pub by_component: HashMap<String, u64>,
    /// Top error patterns.
    pub top_patterns: Vec<ErrorPattern>,
}

/// Helper macro for easy error tracking.
#[macro_export]
macro_rules! track_error {
    ($tracker:expr, $category:expr, $component:expr, $operation:expr, $message:expr) => {{
        let error = $crate::error_tracking::TrackedError {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now(),
            severity: $crate::error_tracking::ErrorSeverity::Error,
            category: $category,
            component: $component.to_string(),
            operation: $operation.to_string(),
            message: $message.to_string(),
            details: None,
            stack_trace: None,
            context: std::collections::HashMap::new(),
            correlation_id: None,
            user_id: None,
            request_id: None,
            retry_count: 0,
            is_transient: false,
        };
        $tracker.track_error(error).await;
    }};
    ($tracker:expr, $category:expr, $component:expr, $operation:expr, $message:expr, $severity:expr) => {{
        let error = $crate::error_tracking::TrackedError {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now(),
            severity: $severity,
            category: $category,
            component: $component.to_string(),
            operation: $operation.to_string(),
            message: $message.to_string(),
            details: None,
            stack_trace: None,
            context: std::collections::HashMap::new(),
            correlation_id: None,
            user_id: None,
            request_id: None,
            retry_count: 0,
            is_transient: false,
        };
        $tracker.track_error(error).await;
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_severity_ordering() {
        assert!(ErrorSeverity::Debug < ErrorSeverity::Warning);
        assert!(ErrorSeverity::Warning < ErrorSeverity::Error);
        assert!(ErrorSeverity::Error < ErrorSeverity::Critical);
        assert!(ErrorSeverity::Critical < ErrorSeverity::Fatal);
    }

    #[test]
    fn test_error_category_display() {
        assert_eq!(ErrorCategory::Database.to_string(), "database");
        assert_eq!(ErrorCategory::VCS.to_string(), "vcs");
        assert_eq!(ErrorCategory::Network.to_string(), "network");
    }

    #[tokio::test]
    async fn test_error_tracking_basic() {
        let config = ErrorTrackingConfig {
            log_to_file: false,
            ..Default::default()
        };
        let tracker = ErrorTracker::new(config);

        let error = TrackedError {
            id: "test-error-1".to_string(),
            timestamp: Utc::now(),
            severity: ErrorSeverity::Error,
            category: ErrorCategory::Database,
            component: "test-component".to_string(),
            operation: "test-operation".to_string(),
            message: "Test error message".to_string(),
            details: None,
            stack_trace: None,
            context: HashMap::new(),
            correlation_id: None,
            user_id: None,
            request_id: None,
            retry_count: 0,
            is_transient: false,
        };

        tracker.track_error(error).await;

        let stats = tracker.get_error_statistics().await;
        assert_eq!(stats.total_errors, 1);
        assert_eq!(stats.by_category.get(&ErrorCategory::Database), Some(&1));
    }

    #[tokio::test]
    async fn test_error_filtering_by_severity() {
        let config = ErrorTrackingConfig {
            min_severity: ErrorSeverity::Error,
            log_to_file: false,
            ..Default::default()
        };
        let tracker = ErrorTracker::new(config);

        // This should be ignored
        let warning_error = TrackedError {
            id: "warning-error".to_string(),
            timestamp: Utc::now(),
            severity: ErrorSeverity::Warning,
            category: ErrorCategory::Database,
            component: "test".to_string(),
            operation: "test".to_string(),
            message: "Warning message".to_string(),
            details: None,
            stack_trace: None,
            context: HashMap::new(),
            correlation_id: None,
            user_id: None,
            request_id: None,
            retry_count: 0,
            is_transient: false,
        };

        // This should be tracked
        let error_error = TrackedError {
            severity: ErrorSeverity::Error,
            ..warning_error.clone()
        };

        tracker.track_error(warning_error).await;
        tracker.track_error(error_error).await;

        let stats = tracker.get_error_statistics().await;
        assert_eq!(stats.total_errors, 1); // Only the Error level should be tracked
    }

    #[tokio::test]
    async fn test_error_pattern_detection() {
        let config = ErrorTrackingConfig {
            log_to_file: false,
            ..Default::default()
        };
        let tracker = ErrorTracker::new(config);

        // Track the same error pattern multiple times
        for i in 0..3 {
            let error = TrackedError {
                id: format!("error-{}", i),
                timestamp: Utc::now(),
                severity: ErrorSeverity::Error,
                category: ErrorCategory::Database,
                component: "connection-pool".to_string(),
                operation: "get_connection".to_string(),
                message: "Failed to get database connection".to_string(),
                details: None,
                stack_trace: None,
                context: HashMap::new(),
                correlation_id: None,
                user_id: None,
                request_id: None,
                retry_count: 0,
                is_transient: false,
            };
            tracker.track_error(error).await;
        }

        let stats = tracker.get_error_statistics().await;
        assert_eq!(stats.total_errors, 3);
        assert!(!stats.top_patterns.is_empty());

        let pattern = &stats.top_patterns[0];
        assert_eq!(pattern.count, 3);
        assert_eq!(pattern.component, "connection-pool");
        assert_eq!(pattern.operation, "get_connection");
    }
}
