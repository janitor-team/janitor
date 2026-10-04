//! Helpers and utilities for parsing environment variables with reduced boilerplate
//!
//! This module provides helper functions that eliminate the repetitive patterns
//! found in the FromEnv implementations across runner/src/config.rs, site/src/config.rs,
//! and archive/src/config.rs. Instead of 50-100 lines of similar parsing logic,
//! these utilities reduce it significantly.

use crate::shared_config::{ConfigError, EnvParser, FromEnv};
use std::path::PathBuf;

/// Helper functions for common environment variable parsing patterns
pub mod helpers {
    use super::*;

    /// Parse comma-separated string into Vec<String>
    pub fn parse_csv_string(parser: &EnvParser, key: &str, default: Vec<String>) -> Vec<String> {
        parser
            .get_string(key)
            .map(|s| s.split(',').map(|item| item.trim().to_string()).collect())
            .unwrap_or(default)
    }

    /// Parse optional PathBuf from string
    pub fn parse_optional_path(parser: &EnvParser, key: &str) -> Option<PathBuf> {
        parser.get_string(key).map(PathBuf::from)
    }

    /// Parse PathBuf with default
    pub fn parse_path_with_default(parser: &EnvParser, key: &str, default: PathBuf) -> PathBuf {
        parser.get_string(key).map(PathBuf::from).unwrap_or(default)
    }

    /// Parse multiple environment variables with fallback
    pub fn parse_with_fallback(parser: &EnvParser, keys: &[&str]) -> Option<String> {
        for key in keys {
            if let Some(value) = parser.get_string(key) {
                return Some(value);
            }
        }
        None
    }

    /// Parse environment variable with complex conditional logic for session secrets
    pub fn parse_session_secret(parser: &EnvParser, debug_mode: bool) -> String {
        parser.get_string("SESSION_SECRET").unwrap_or_else(|| {
            if debug_mode {
                "debug-secret-key-not-for-production-use-only".to_string()
            } else {
                eprintln!("ERROR: SESSION_SECRET environment variable is required in production");
                std::process::exit(1);
            }
        })
    }

    /// Parse comma-separated IPs with default debug IPs
    pub fn parse_debug_ips(parser: &EnvParser) -> Vec<String> {
        parser
            .get_string("DEBUG_TOOLBAR_ALLOWED_IPS")
            .map(|ips| ips.split(',').map(|ip| ip.trim().to_string()).collect())
            .unwrap_or_else(|| vec!["127.0.0.1".to_string(), "::1".to_string()])
    }

    /// Parse string with default value
    pub fn parse_string(parser: &EnvParser, key: &str, default: String) -> String {
        parser.get_string(key).unwrap_or(default)
    }

    /// Parse boolean with default value (handles errors gracefully)
    pub fn parse_bool(parser: &EnvParser, key: &str, default: bool) -> bool {
        parser
            .get_bool(key)
            .unwrap_or(Some(default))
            .unwrap_or(default)
    }

    /// Parse u32 with default value (handles errors gracefully)
    pub fn parse_u32(parser: &EnvParser, key: &str, default: u32) -> u32 {
        parser
            .get_u32(key)
            .unwrap_or(Some(default))
            .unwrap_or(default)
    }

    /// Parse u64 with default value (handles errors gracefully)
    pub fn parse_u64(parser: &EnvParser, key: &str, default: u64) -> u64 {
        parser
            .get_u64(key)
            .unwrap_or(Some(default))
            .unwrap_or(default)
    }

    /// Parse f64 with default value (handles errors gracefully)
    pub fn parse_f64(parser: &EnvParser, key: &str, default: f64) -> f64 {
        parser
            .get_f64(key)
            .unwrap_or(Some(default))
            .unwrap_or(default)
    }

    /// Parse u16 with default value (handles errors gracefully)
    pub fn parse_u16(parser: &EnvParser, key: &str, default: u16) -> u16 {
        parser
            .get_u16(key)
            .unwrap_or(Some(default))
            .unwrap_or(default)
    }
}

/// Standard FromEnv implementation template for services that extend ServiceConfig
///
/// This provides a reusable pattern for the common structure:
/// 1. Load base ServiceConfig
/// 2. Create EnvParser
/// 3. Parse fields
/// 4. Build result struct
///
/// Example usage:
/// ```rust,ignore
/// impl FromEnv for MyConfig {
///     fn from_env() -> Result<Self, ConfigError> {
///         Self::from_env_with_prefix("")
///     }
///     
///     fn from_env_with_prefix(prefix: &str) -> Result<Self, ConfigError> {
///         let (base, parser) = load_base_config_and_parser(prefix)?;
///         
///         // Instead of repetitive parser.get_*()?.unwrap_or() patterns:
///         let name = helpers::parse_string(&parser, "SERVICE_NAME", "default".to_string());
///         let enabled = helpers::parse_bool(&parser, "ENABLED", true);
///         let port = helpers::parse_u16(&parser, "PORT", 8080);
///         
///         Ok(MyConfig { base, name, enabled, port })
///     }
/// }
/// ```
pub fn load_base_config_and_parser(
    prefix: &str,
) -> Result<(crate::shared_config::ServiceConfig, EnvParser), ConfigError> {
    use crate::shared_config::ServiceConfig;

    let base = ServiceConfig::from_env_with_prefix(prefix)?;
    let parser = EnvParser::with_prefix(prefix);
    Ok((base, parser))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_config::{FromEnv, ServiceConfig};

    // Demonstration of simplified config using the helper functions
    #[derive(Debug, Clone)]
    struct ExampleConfig {
        #[allow(dead_code)] // demonstration struct; field only populated in tests
        base: ServiceConfig,
        service_name: String,
        debug_mode: bool,
        max_connections: u32,
        timeout_seconds: u64,
        api_key: Option<String>,
    }

    impl FromEnv for ExampleConfig {
        fn from_env() -> Result<Self, ConfigError> {
            Self::from_env_with_prefix("")
        }

        fn from_env_with_prefix(prefix: &str) -> Result<Self, ConfigError> {
            let (base, parser) = load_base_config_and_parser(prefix)?;

            // This is much cleaner than repetitive parser.get_*()?.unwrap_or() calls
            let service_name =
                helpers::parse_string(&parser, "SERVICE_NAME", "example-service".to_string());
            let debug_mode = helpers::parse_bool(&parser, "DEBUG_MODE", false);
            let max_connections = helpers::parse_u32(&parser, "MAX_CONNECTIONS", 10);
            let timeout_seconds = helpers::parse_u64(&parser, "TIMEOUT_SECONDS", 30);
            let api_key = parser.get_string("API_KEY");

            Ok(ExampleConfig {
                base,
                service_name,
                debug_mode,
                max_connections,
                timeout_seconds,
                api_key,
            })
        }
    }

    #[test]
    fn test_example_config_parsing() {
        std::env::set_var("SERVICE_NAME", "test-service");
        std::env::set_var("DEBUG_MODE", "true");
        std::env::set_var("MAX_CONNECTIONS", "20");
        std::env::set_var("API_KEY", "secret-key");

        let config = ExampleConfig::from_env().unwrap();

        assert_eq!(config.service_name, "test-service");
        assert_eq!(config.debug_mode, true);
        assert_eq!(config.max_connections, 20);
        assert_eq!(config.timeout_seconds, 30); // default
        assert_eq!(config.api_key, Some("secret-key".to_string()));

        // Clean up
        std::env::remove_var("SERVICE_NAME");
        std::env::remove_var("DEBUG_MODE");
        std::env::remove_var("MAX_CONNECTIONS");
        std::env::remove_var("API_KEY");
    }

    #[test]
    fn test_helper_functions() {
        let parser = EnvParser::new();

        // Test CSV parsing
        std::env::set_var("CSV_TEST", "a,b,c");
        let result = helpers::parse_csv_string(&parser, "CSV_TEST", vec![]);
        assert_eq!(result, vec!["a", "b", "c"]);
        std::env::remove_var("CSV_TEST");

        // Test fallback parsing
        std::env::set_var("FALLBACK_VAR", "fallback-value");
        let result = helpers::parse_with_fallback(&parser, &["MISSING_VAR", "FALLBACK_VAR"]);
        assert_eq!(result, Some("fallback-value".to_string()));
        std::env::remove_var("FALLBACK_VAR");

        // Test path parsing
        std::env::set_var("PATH_TEST", "/tmp/test");
        let result = helpers::parse_optional_path(&parser, "PATH_TEST");
        assert_eq!(result, Some(PathBuf::from("/tmp/test")));
        std::env::remove_var("PATH_TEST");
    }

    #[test]
    fn test_parsing_helpers() {
        let parser = EnvParser::new();

        // Test string parsing
        std::env::set_var("STRING_VAR", "hello");
        let result = helpers::parse_string(&parser, "STRING_VAR", "default".to_string());
        assert_eq!(result, "hello");
        std::env::remove_var("STRING_VAR");

        // Test bool parsing
        std::env::set_var("BOOL_VAR", "true");
        let result = helpers::parse_bool(&parser, "BOOL_VAR", false);
        assert_eq!(result, true);
        std::env::remove_var("BOOL_VAR");

        // Test numeric parsing
        std::env::set_var("NUM_VAR", "42");
        let result = helpers::parse_u32(&parser, "NUM_VAR", 0);
        assert_eq!(result, 42);
        std::env::remove_var("NUM_VAR");

        // Test defaults when variables are missing
        let default_string =
            helpers::parse_string(&parser, "MISSING_STRING", "default".to_string());
        assert_eq!(default_string, "default");

        let default_bool = helpers::parse_bool(&parser, "MISSING_BOOL", true);
        assert_eq!(default_bool, true);

        let default_num = helpers::parse_u32(&parser, "MISSING_NUM", 100);
        assert_eq!(default_num, 100);
    }

    #[test]
    fn test_complex_parsing_helpers() {
        let parser = EnvParser::new();

        // Test session secret in debug mode
        let secret = helpers::parse_session_secret(&parser, true);
        assert!(secret.contains("debug"));

        // Test debug IPs parsing
        std::env::set_var("DEBUG_TOOLBAR_ALLOWED_IPS", "192.168.1.1,10.0.0.1");
        let ips = helpers::parse_debug_ips(&parser);
        assert_eq!(ips, vec!["192.168.1.1", "10.0.0.1"]);
        std::env::remove_var("DEBUG_TOOLBAR_ALLOWED_IPS");
    }

    #[test]
    fn test_pathbuf_parsing() {
        let parser = EnvParser::new();

        std::env::set_var("PATH_VAR", "/test/path");

        let test_path =
            helpers::parse_path_with_default(&parser, "PATH_VAR", PathBuf::from("/default"));
        let missing_path =
            helpers::parse_path_with_default(&parser, "MISSING_PATH", PathBuf::from("/fallback"));
        let optional_path = helpers::parse_optional_path(&parser, "PATH_VAR");

        assert_eq!(test_path, PathBuf::from("/test/path"));
        assert_eq!(missing_path, PathBuf::from("/fallback"));
        assert_eq!(optional_path, Some(PathBuf::from("/test/path")));

        std::env::remove_var("PATH_VAR");
    }
}
