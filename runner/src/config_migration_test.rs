//! Test module to verify config migration works correctly

#[cfg(test)]
mod tests {
    use crate::config::RunnerConfig;
    use janitor::shared_config::ConfigLoader;

    #[test]
    fn test_runner_config_from_env() {
        std::env::set_var(
            "DATABASE_URL",
            "postgresql://testuser:testpass@localhost/testdb",
        );
        std::env::set_var("REDIS_URL", "redis://localhost:6379");
        std::env::set_var("LOG_LEVEL", "debug");
        std::env::set_var("WORKER_SHARED_SECRET", "test-secret");

        // Load config from environment
        let config = <RunnerConfig as janitor::shared_config::ConfigLoader>::from_env()
            .expect("Failed to load config from env");

        // Verify database config
        assert!(config.base.database.is_some());
        let db = config.base.database.as_ref().unwrap();
        assert_eq!(db.url, "postgresql://testuser:testpass@localhost/testdb");

        // Verify redis config
        assert!(config.base.redis.is_some());
        let redis = config.base.redis.as_ref().unwrap();
        assert_eq!(redis.url, "redis://localhost:6379");

        // Verify logging config
        assert_eq!(config.base.logging.level, "debug");

        // Verify worker config
        assert_eq!(config.worker.shared_secret.as_ref().unwrap(), "test-secret");

        // Clean up
        std::env::remove_var("DATABASE_URL");
        std::env::remove_var("REDIS_URL");
        std::env::remove_var("LOG_LEVEL");
        std::env::remove_var("WORKER_SHARED_SECRET");
    }

    #[test]
    fn test_runner_config_validation() {
        let mut config = RunnerConfig::default();
        // DB + Redis are hard requirements for validate() to pass;
        // set them so the assertions below only test worker rules.
        config.base.database = Some(janitor::shared_config::DatabaseConfig {
            url: "postgresql://localhost/janitor".to_string(),
            ..Default::default()
        });
        config.base.redis = Some(janitor::shared_config::RedisConfig {
            url: "redis://localhost".to_string(),
            ..Default::default()
        });

        // Worker authentication is on by default, so a config without a
        // shared secret must not validate.
        assert!(config.validate().is_err());

        // Disable worker auth so we don't need a secret.
        config.worker.enable_authentication = false;

        // Log and artifact storage moved to the textproto config, so
        // nothing else here is required.
        match config.validate() {
            Ok(_) => {}
            Err(e) => panic!("Validation failed: {}", e),
        }
    }

    #[test]
    fn test_config_compatibility() {
        let new_config = RunnerConfig {
            base: janitor::shared_config::ServiceConfig::default()
                .with_database("postgresql://localhost/janitor".to_string())
                .with_redis("redis://localhost:6379".to_string()),
            ..Default::default()
        };

        // Verify we can access config fields in the expected way
        assert!(new_config.database().is_some());
        assert_eq!(
            new_config.database().unwrap().url,
            "postgresql://localhost/janitor"
        );

        assert!(new_config.redis().is_some());
        assert_eq!(new_config.redis().unwrap().url, "redis://localhost:6379");
    }
}
