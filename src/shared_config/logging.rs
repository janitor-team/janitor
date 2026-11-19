//! Shared logging configuration for Janitor services

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::shared_config::validation::Validator;
use crate::shared_config::{defaults::*, env::EnvParser, ConfigError, FromEnv, ValidationError};

/// Logging configuration used across Janitor services
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level (trace, debug, info, warn, error)
    #[serde(default = "default_log_level")]
    pub level: String,

    /// Output logs in JSON format
    #[serde(default = "default_false")]
    pub json_format: bool,

    /// Enable console output
    #[serde(default = "default_true")]
    pub console_output: bool,

    /// File output configuration
    pub file_output: Option<FileOutputConfig>,

    /// Enable colored output (only applies to console)
    #[serde(default = "default_true")]
    pub colored_output: bool,

    /// Include timestamps in logs
    #[serde(default = "default_true")]
    pub include_timestamps: bool,

    /// Include module paths in logs
    #[serde(default = "default_false")]
    pub include_module_path: bool,

    /// Include line numbers in logs
    #[serde(default = "default_false")]
    pub include_line_numbers: bool,

    /// Maximum log level for specific modules
    pub module_filters: Option<std::collections::HashMap<String, String>>,
}

/// File output configuration for logging
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOutputConfig {
    /// Path to the log file
    pub path: PathBuf,

    /// Maximum size of log file in bytes before rotation
    pub max_size_bytes: Option<u64>,

    /// Maximum number of rotated log files to keep
    pub max_files: Option<u32>,

    /// Enable log file compression
    #[serde(default = "default_false")]
    pub compress: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            json_format: default_false(),
            console_output: default_true(),
            file_output: None,
            colored_output: default_true(),
            include_timestamps: default_true(),
            include_module_path: default_false(),
            include_line_numbers: default_false(),
            module_filters: None,
        }
    }
}

impl FromEnv for LoggingConfig {
    fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_prefix("")
    }

    fn from_env_with_prefix(prefix: &str) -> Result<Self, ConfigError> {
        let parser = EnvParser::with_prefix(prefix);

        let mut config = Self {
            level: parser
                .get_string("LOG_LEVEL")
                .unwrap_or_else(default_log_level),
            json_format: parser
                .get_bool("LOG_JSON_FORMAT")?
                .unwrap_or_else(default_false),
            console_output: parser
                .get_bool("LOG_CONSOLE_OUTPUT")?
                .unwrap_or_else(default_true),
            colored_output: parser
                .get_bool("LOG_COLORED_OUTPUT")?
                .unwrap_or_else(default_true),
            include_timestamps: parser
                .get_bool("LOG_INCLUDE_TIMESTAMPS")?
                .unwrap_or_else(default_true),
            include_module_path: parser
                .get_bool("LOG_INCLUDE_MODULE_PATH")?
                .unwrap_or_else(default_false),
            include_line_numbers: parser
                .get_bool("LOG_INCLUDE_LINE_NUMBERS")?
                .unwrap_or_else(default_false),
            file_output: None,
            module_filters: None,
        };

        // File output configuration
        if let Some(log_file) = parser.get_string("LOG_FILE") {
            config.file_output = Some(FileOutputConfig {
                path: PathBuf::from(log_file),
                max_size_bytes: parser.get_u64("LOG_FILE_MAX_SIZE")?,
                max_files: parser.get_u32("LOG_FILE_MAX_FILES")?,
                compress: parser
                    .get_bool("LOG_FILE_COMPRESS")?
                    .unwrap_or_else(default_false),
            });
        }

        Ok(config)
    }
}

impl LoggingConfig {
    /// Validate the logging configuration
    pub fn validate(&self) -> Result<(), ValidationError> {
        let mut validator = Validator::new();

        // Validate log level
        let valid_levels = ["trace", "debug", "info", "warn", "error"];
        if !valid_levels.contains(&self.level.to_lowercase().as_str()) {
            validator.add_invalid_value(
                "level",
                &format!(
                    "Invalid log level '{}', must be one of: {}",
                    self.level,
                    valid_levels.join(", ")
                ),
            );
        }

        // Validate that at least one output is enabled
        if !self.console_output && self.file_output.is_none() {
            validator.add_invalid_value(
                "output",
                "At least one output method (console or file) must be enabled",
            );
        }

        // Validate file output configuration
        if let Some(ref file_config) = self.file_output {
            if let Some(parent) = file_config.path.parent() {
                if !parent.exists() {
                    validator.add_invalid_value(
                        "file_output.path",
                        &format!("Parent directory does not exist: {}", parent.display()),
                    );
                }
            }

            if let Some(max_size) = file_config.max_size_bytes {
                if max_size == 0 {
                    validator.add_invalid_value(
                        "file_output.max_size_bytes",
                        "Maximum file size must be greater than 0",
                    );
                }
            }

            if let Some(max_files) = file_config.max_files {
                if max_files == 0 {
                    validator.add_invalid_value(
                        "file_output.max_files",
                        "Maximum number of files must be greater than 0",
                    );
                }
            }
        }

        // Validate module filters
        if let Some(ref filters) = self.module_filters {
            for (module, level) in filters {
                if module.is_empty() {
                    validator.add_invalid_value("module_filters", "Module name cannot be empty");
                }

                if !valid_levels.contains(&level.to_lowercase().as_str()) {
                    validator.add_invalid_value(
                        "module_filters",
                        &format!(
                            "Invalid log level '{}' for module '{}', must be one of: {}",
                            level,
                            module,
                            valid_levels.join(", ")
                        ),
                    );
                }
            }
        }

        validator.finish()
    }

    /// Get the log level as a tracing::Level
    pub fn tracing_level(&self) -> Result<tracing::Level, ConfigError> {
        match self.level.to_lowercase().as_str() {
            "trace" => Ok(tracing::Level::TRACE),
            "debug" => Ok(tracing::Level::DEBUG),
            "info" => Ok(tracing::Level::INFO),
            "warn" => Ok(tracing::Level::WARN),
            "error" => Ok(tracing::Level::ERROR),
            _ => Err(ConfigError::ParseError {
                field: "level".to_string(),
                message: format!("Invalid log level: {}", self.level),
            }),
        }
    }
}

/// Initialize logging based on configuration
///
/// This initializes tracing with console and/or file output based on configuration.
/// Supports log rotation, compression, and multiple output formats.
/// Initialize logging from the old simple LoggingArgs for backward compatibility
/// This provides a migration path for services still using the simple logging
pub fn init_logging_from_simple_args(debug: bool, gcp: bool) -> Result<(), LoggingError> {
    #[cfg(feature = "gcp")]
    if gcp {
        // Use GCP stackdriver logging
        stackdriver_logger::init_with_cargo!("../../Cargo.toml");
        return Ok(());
    }

    #[cfg(not(feature = "gcp"))]
    if gcp {
        return Err(LoggingError::ValidationError(
            ValidationError::InvalidValue {
                field: "gcp".to_string(),
                message: "GCP logging requested but feature not enabled".to_string(),
            },
        ));
    }

    // Create a LoggingConfig from the simple args
    let config = LoggingConfig {
        level: if debug {
            "debug".to_string()
        } else {
            "info".to_string()
        },
        json_format: false,
        console_output: true,
        file_output: None,
        colored_output: true,
        include_timestamps: true,
        include_module_path: false,
        include_line_numbers: false,
        module_filters: None,
    };

    init_logging(&config)
}

/// Initialize logging with comprehensive configuration
pub fn init_logging(config: &LoggingConfig) -> Result<(), LoggingError> {
    use tracing_subscriber::{prelude::*, EnvFilter};

    // Validate configuration first
    config.validate().map_err(LoggingError::ValidationError)?;

    // Create environment filter
    let env_filter = if let Some(ref filters) = config.module_filters {
        let mut filter_string = config.level.clone();
        for (module, level) in filters {
            filter_string.push(',');
            filter_string.push_str(&format!("{}={}", module, level));
        }
        EnvFilter::new(filter_string)
    } else {
        EnvFilter::new(&config.level)
    };

    // Build layers based on configuration
    match (config.console_output, &config.file_output) {
        (true, Some(file_config)) => {
            // Both console and file output
            init_with_both_outputs(config, file_config, env_filter)?;
        }
        (true, None) => {
            // Console output only
            init_console_only(config, env_filter)?;
        }
        (false, Some(file_config)) => {
            // File output only
            init_file_only(config, file_config, env_filter)?;
        }
        (false, None) => {
            // Neither console nor file output (should not happen due to validation)
            tracing_subscriber::registry()
                .with(env_filter)
                .try_init()
                .map_err(|e| LoggingError::InitError(e.to_string()))?;
        }
    }

    Ok(())
}

/// Initialize with both console and file output
fn init_with_both_outputs(
    config: &LoggingConfig,
    file_config: &FileOutputConfig,
    env_filter: tracing_subscriber::EnvFilter,
) -> Result<(), LoggingError> {
    use tracing_subscriber::{fmt, prelude::*};

    // Create file appender
    let file_appender = create_file_appender(file_config)?;

    // Handle compression and file limits
    start_log_management_task(file_config)?;

    if config.json_format {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                fmt::layer()
                    .json()
                    .with_current_span(false)
                    .with_span_list(true),
            )
            .with(
                fmt::layer()
                    .json()
                    .with_current_span(false)
                    .with_span_list(true)
                    .with_writer(file_appender)
                    .with_ansi(false),
            )
            .try_init()
            .map_err(|e| LoggingError::InitError(e.to_string()))?;
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                fmt::layer()
                    .with_ansi(config.colored_output)
                    .with_target(config.include_module_path)
                    .with_line_number(config.include_line_numbers),
            )
            .with(
                fmt::layer()
                    .with_target(config.include_module_path)
                    .with_line_number(config.include_line_numbers)
                    .with_writer(file_appender)
                    .with_ansi(false),
            )
            .try_init()
            .map_err(|e| LoggingError::InitError(e.to_string()))?;
    }

    Ok(())
}

/// Initialize console output only
fn init_console_only(
    config: &LoggingConfig,
    env_filter: tracing_subscriber::EnvFilter,
) -> Result<(), LoggingError> {
    use tracing_subscriber::{fmt, prelude::*};

    if config.json_format {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                fmt::layer()
                    .json()
                    .with_current_span(false)
                    .with_span_list(true),
            )
            .try_init()
            .map_err(|e| LoggingError::InitError(e.to_string()))?;
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                fmt::layer()
                    .with_ansi(config.colored_output)
                    .with_target(config.include_module_path)
                    .with_line_number(config.include_line_numbers),
            )
            .try_init()
            .map_err(|e| LoggingError::InitError(e.to_string()))?;
    }

    Ok(())
}

/// Initialize file output only
fn init_file_only(
    config: &LoggingConfig,
    file_config: &FileOutputConfig,
    env_filter: tracing_subscriber::EnvFilter,
) -> Result<(), LoggingError> {
    use tracing_subscriber::{fmt, prelude::*};

    // Create file appender
    let file_appender = create_file_appender(file_config)?;

    // Handle compression and file limits
    start_log_management_task(file_config)?;

    if config.json_format {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                fmt::layer()
                    .json()
                    .with_current_span(false)
                    .with_span_list(true)
                    .with_writer(file_appender)
                    .with_ansi(false),
            )
            .try_init()
            .map_err(|e| LoggingError::InitError(e.to_string()))?;
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                fmt::layer()
                    .with_target(config.include_module_path)
                    .with_line_number(config.include_line_numbers)
                    .with_writer(file_appender)
                    .with_ansi(false),
            )
            .try_init()
            .map_err(|e| LoggingError::InitError(e.to_string()))?;
    }

    Ok(())
}

/// Create a file appender for logging
fn create_file_appender(
    file_config: &FileOutputConfig,
) -> Result<tracing_appender::rolling::RollingFileAppender, LoggingError> {
    use tracing_appender::rolling::{RollingFileAppender, Rotation};

    // Create parent directory if it doesn't exist
    if let Some(parent) = file_config.path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| LoggingError::IoError {
                path: parent.display().to_string(),
                message: format!("Failed to create log directory: {}", e),
            })?;
        }
    }

    // Determine log file name and directory
    let (log_dir, log_file_prefix) = if let Some(parent) = file_config.path.parent() {
        let file_name = file_config
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("app");
        (parent, file_name)
    } else {
        (std::path::Path::new("."), "app")
    };

    // Configure rotation based on max_size_bytes
    let rotation = if file_config.max_size_bytes.is_some() {
        // Use daily rotation when size limits are set
        // (tracing-appender doesn't support size-based rotation directly)
        Rotation::DAILY
    } else {
        Rotation::NEVER
    };

    // Create the rolling file appender
    Ok(RollingFileAppender::new(rotation, log_dir, log_file_prefix))
}

/// Start background task for log file management
fn start_log_management_task(file_config: &FileOutputConfig) -> Result<(), LoggingError> {
    if file_config.compress || file_config.max_files.is_some() {
        let file_config_clone = file_config.clone();
        let log_dir_clone = file_config
            .path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();

        tokio::spawn(async move {
            if let Err(e) = manage_log_files(&log_dir_clone, &file_config_clone).await {
                log::warn!("Error managing log files: {}", e);
            }
        });
    }
    Ok(())
}

/// Background task to manage log file compression and cleanup
async fn manage_log_files(
    log_dir: &std::path::Path,
    file_config: &FileOutputConfig,
) -> Result<(), std::io::Error> {
    use std::time::Duration;

    // Run cleanup every hour
    let mut interval = tokio::time::interval(Duration::from_secs(3600));

    loop {
        interval.tick().await;

        if let Err(e) = cleanup_log_files(log_dir, file_config).await {
            log::warn!("Failed to cleanup log files: {}", e);
        }
    }
}

/// Clean up old log files based on configuration
async fn cleanup_log_files(
    log_dir: &std::path::Path,
    file_config: &FileOutputConfig,
) -> Result<(), std::io::Error> {
    use std::fs;
    use std::time::SystemTime;

    let mut log_files = Vec::new();

    // Find all log files in the directory
    for entry in fs::read_dir(log_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            if let Some(file_name) = path.file_name().and_then(|s| s.to_str()) {
                // Check if this looks like a log file
                if file_name.contains(".log") || file_name.ends_with(".log") {
                    if let Ok(metadata) = entry.metadata() {
                        if let Ok(modified) = metadata.modified() {
                            log_files.push((path, modified));
                        }
                    }
                }
            }
        }
    }

    // Sort by modification time (oldest first)
    log_files.sort_by_key(|(_, modified)| *modified);

    // Compress old files if enabled
    if file_config.compress {
        for (path, modified) in &log_files {
            let age = SystemTime::now()
                .duration_since(*modified)
                .unwrap_or_default();

            // Compress files older than 1 day that aren't already compressed
            if age.as_secs() > 86400 && !path.extension().map_or(false, |ext| ext == "gz") {
                if let Err(e) = compress_log_file(path).await {
                    log::warn!("Failed to compress log file {}: {}", path.display(), e);
                }
            }
        }
    }

    // Remove old files if max_files is set
    if let Some(max_files) = file_config.max_files {
        if log_files.len() > max_files as usize {
            let files_to_remove = log_files.len() - max_files as usize;

            for (path, _) in log_files.iter().take(files_to_remove) {
                if let Err(e) = fs::remove_file(path) {
                    log::warn!("Failed to remove old log file {}: {}", path.display(), e);
                } else {
                    log::info!("Removed old log file: {}", path.display());
                }
            }
        }
    }

    Ok(())
}

/// Compress a log file using gzip
async fn compress_log_file(path: &std::path::Path) -> Result<(), std::io::Error> {
    use std::fs::File;
    use std::io::{Read, Write};

    let mut file = File::open(path)?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)?;

    let compressed_path = path.with_extension(format!(
        "{}.gz",
        path.extension().unwrap_or_default().to_string_lossy()
    ));

    let compressed_file = File::create(&compressed_path)?;
    let mut encoder =
        flate2::write::GzEncoder::new(compressed_file, flate2::Compression::default());
    encoder.write_all(&content)?;
    encoder.finish()?;

    // Remove original file after successful compression
    std::fs::remove_file(path)?;

    log::info!(
        "Compressed log file: {} -> {}",
        path.display(),
        compressed_path.display()
    );
    Ok(())
}

/// Errors that can occur during logging initialization
#[derive(Debug)]
pub enum LoggingError {
    /// Configuration validation error
    ValidationError(ValidationError),

    /// I/O error accessing log file
    IoError { path: String, message: String },

    /// Error creating log filter
    FilterError {
        module: String,
        level: String,
        message: String,
    },

    /// Tracing initialization error
    InitError(String),
}

impl std::fmt::Display for LoggingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoggingError::ValidationError(err) => {
                write!(f, "Logging configuration validation error: {}", err)
            }
            LoggingError::IoError { path, message } => {
                write!(f, "I/O error accessing log file '{}': {}", path, message)
            }
            LoggingError::FilterError {
                module,
                level,
                message,
            } => {
                write!(
                    f,
                    "Error creating filter for module '{}' with level '{}': {}",
                    module, level, message
                )
            }
            LoggingError::InitError(message) => {
                write!(f, "Tracing initialization error: {}", message)
            }
        }
    }
}

impl std::error::Error for LoggingError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_logging_config_validation() {
        let mut config = LoggingConfig::default();
        assert!(config.validate().is_ok());

        // Test invalid log level
        config.level = "invalid".to_string();
        assert!(config.validate().is_err());

        // Test no output enabled
        config.level = "info".to_string();
        config.console_output = false;
        config.file_output = None;
        assert!(config.validate().is_err());

        // Test invalid module filter
        config.console_output = true;
        let mut filters = HashMap::new();
        filters.insert("test".to_string(), "invalid".to_string());
        config.module_filters = Some(filters);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_tracing_level_conversion() {
        let config = LoggingConfig {
            level: "debug".to_string(),
            ..LoggingConfig::default()
        };

        assert_eq!(config.tracing_level().unwrap(), tracing::Level::DEBUG);

        let invalid_config = LoggingConfig {
            level: "invalid".to_string(),
            ..LoggingConfig::default()
        };

        assert!(invalid_config.tracing_level().is_err());
    }

    #[test]
    fn test_file_output_config() {
        use std::path::PathBuf;

        let file_config = FileOutputConfig {
            path: PathBuf::from("/tmp/test.log"),
            max_size_bytes: Some(1024 * 1024), // 1MB
            max_files: Some(5),
            compress: true,
        };

        let config = LoggingConfig {
            console_output: false,
            file_output: Some(file_config),
            ..LoggingConfig::default()
        };

        assert!(config.validate().is_ok());
        assert!(config.file_output.is_some());
        assert_eq!(config.file_output.as_ref().unwrap().max_files, Some(5));
        assert!(config.file_output.as_ref().unwrap().compress);
    }

    #[test]
    fn test_file_output_validation() {
        use std::path::PathBuf;

        // Test invalid max_size_bytes
        let file_config = FileOutputConfig {
            path: PathBuf::from("/tmp/test.log"),
            max_size_bytes: Some(0), // Invalid: should be > 0
            max_files: Some(5),
            compress: false,
        };

        let config = LoggingConfig {
            console_output: false,
            file_output: Some(file_config),
            ..LoggingConfig::default()
        };

        assert!(config.validate().is_err());

        // Test invalid max_files
        let file_config = FileOutputConfig {
            path: PathBuf::from("/tmp/test.log"),
            max_size_bytes: Some(1024),
            max_files: Some(0), // Invalid: should be > 0
            compress: false,
        };

        let config = LoggingConfig {
            console_output: false,
            file_output: Some(file_config),
            ..LoggingConfig::default()
        };

        assert!(config.validate().is_err());
    }
}
