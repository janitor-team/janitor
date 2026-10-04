//! Unit tests for core runner functionality.
//!
//! These tests verify that individual components work correctly
//! and maintain compatibility with Python behavior.

use chrono::{DateTime, Datelike, Utc};
use serde_json::json;
use std::collections::HashMap;

use janitor_runner::{
    backchannel::{Backchannel, JenkinsBackchannel, PollingBackchannel},
    builder::{get_builder, CampaignConfig, DebianBuildConfig, GenericBuildConfig},
    committer_env, ActiveRun, JanitorResult, WorkerResult,
};

/// Test committer_env function compatibility with Python version.
#[test]
fn test_committer_env_compatibility() {
    // Test with full committer string
    let env = committer_env(Some("John Doe <john@example.com>"));

    assert_eq!(env.get("DEBFULLNAME"), Some(&"John Doe".to_string()));
    assert_eq!(env.get("DEBEMAIL"), Some(&"john@example.com".to_string()));
    assert_eq!(env.get("GIT_COMMITTER_NAME"), Some(&"John Doe".to_string()));
    assert_eq!(
        env.get("GIT_COMMITTER_EMAIL"),
        Some(&"john@example.com".to_string())
    );
    assert_eq!(env.get("GIT_AUTHOR_NAME"), Some(&"John Doe".to_string()));
    assert_eq!(
        env.get("GIT_AUTHOR_EMAIL"),
        Some(&"john@example.com".to_string())
    );
    assert_eq!(env.get("EMAIL"), Some(&"john@example.com".to_string()));
    assert_eq!(
        env.get("COMMITTER"),
        Some(&"John Doe <john@example.com>".to_string())
    );
    assert_eq!(
        env.get("BRZ_EMAIL"),
        Some(&"John Doe <john@example.com>".to_string())
    );

    // Test with None
    let env = committer_env(None);
    assert!(env.is_empty());

    // Test with malformed committer
    let env = committer_env(Some("invalid"));
    assert_eq!(env.get("COMMITTER"), Some(&"invalid".to_string()));
    assert_eq!(env.get("BRZ_EMAIL"), Some(&"invalid".to_string()));
}

/// Test JanitorResult serialization/deserialization compatibility.
#[test]
fn test_janitor_result_serialization() {
    let result = JanitorResult {
        log_id: "test-log-123".to_string(),
        branch_url: "https://github.com/test/repo".to_string(),
        subpath: Some("subdir".to_string()),
        code: "success".to_string(),
        transient: Some(false),
        codebase: "test-codebase".to_string(),
        campaign: "test-campaign".to_string(),
        description: Some("Test successful".to_string()),
        codemod: None,
        value: None,
        logfilenames: vec!["worker.log".to_string()],
        start_time: Utc::now(),
        finish_time: Utc::now(),
        revision: None,
        main_branch_revision: None,
        change_set: None,
        tags: None,
        remotes: None,
        branches: None,
        failure_details: None,
        failure_stage: None,
        resume: None,
        target: None,
        worker_name: None,
        vcs_type: None,
        target_branch_url: None,
        context: None,
        builder_result: None,
    };

    // Test serialization
    let json_str = serde_json::to_string(&result).unwrap();
    assert!(json_str.contains("test-log-123"));
    assert!(json_str.contains("success"));

    // Test deserialization
    let parsed: JanitorResult = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed.log_id, result.log_id);
    assert_eq!(parsed.code, result.code);
    assert_eq!(parsed.codebase, result.codebase);
}

/// Test WorkerResult compatibility with Python dataclass.
#[test]
fn test_worker_result_compatibility() {
    let worker_result = WorkerResult {
        code: "success".to_string(),
        description: Some("Build successful".to_string()),
        context: Some(json!({"test": true})),
        codemod: Some(json!({"applied": ["fix1", "fix2"]})),
        main_branch_revision: None,
        revision: None,
        value: Some(100),
        branches: Some(vec![(
            Some("main".to_string()),
            Some("feature".to_string()),
            None,
            None,
        )]),
        tags: Some(vec![("v1.0".to_string(), None)]),
        remotes: Some({
            let mut map = HashMap::new();
            map.insert("origin".to_string(), {
                let mut remote = HashMap::new();
                remote.insert("url".to_string(), json!("https://github.com/test/repo"));
                remote
            });
            map
        }),
        details: Some(json!({"duration": 300})),
        stage: Some("build".to_string()),
        builder_result: Some(janitor_runner::BuilderResult::Generic),
        target: None,
        start_time: Some(Utc::now()),
        finish_time: Some(Utc::now()),
        queue_id: Some(456),
        worker_name: Some("test-worker".to_string()),
        refreshed: Some(false),
        target_branch_url: None,
        branch_url: Some("https://github.com/test/repo".to_string()),
        vcs_type: Some("git".to_string()),
        subpath: None,
        transient: Some(false),
        codebase: Some("test/repo".to_string()),
    };

    // Test all fields are preserved in serialization
    let json_str = serde_json::to_string(&worker_result).unwrap();
    let parsed: WorkerResult = serde_json::from_str(&json_str).unwrap();

    assert_eq!(parsed.code, worker_result.code);
    assert_eq!(parsed.value, worker_result.value);
    assert_eq!(parsed.stage, worker_result.stage);
    assert_eq!(parsed.queue_id, worker_result.queue_id);
}

/// Test ActiveRun structure.
#[test]
fn test_active_run_structure() {
    use janitor::queue::VcsInfo;

    let active_run = ActiveRun {
        worker_name: "test-worker".to_string(),
        worker_link: Some("http://worker:8080".to_string()),
        queue_id: 789,
        log_id: "run-log-456".to_string(),
        start_time: Utc::now(),
        finish_time: None,
        estimated_duration: Some(std::time::Duration::from_secs(300)),
        campaign: "test-campaign".to_string(),
        change_set: Some("changeset-123".to_string()),
        command: "test-command".to_string(),
        codebase: "test-codebase".to_string(),
        backchannel: janitor_runner::Backchannel::Polling {
            my_url: "http://worker:8080".to_string(),
        },
        instigated_context: Some(json!({
            "requester": "user@example.com"
        })),
        resume_from: None,
        vcs_info: VcsInfo {
            branch_url: Some("https://github.com/test/repo".to_string()),
            subpath: Some("src".to_string()),
            vcs_type: Some("git".to_string()),
        },
    };

    // Test serialization includes all Python fields
    let json_str = serde_json::to_string(&active_run).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert!(parsed.get("worker_name").is_some());
    assert!(parsed.get("queue_id").is_some());
    assert!(parsed.get("log_id").is_some());
    assert!(parsed.get("start_time").is_some());
    assert!(parsed.get("finish_time").is_some());
    assert!(parsed.get("campaign").is_some());
    assert!(parsed.get("vcs_info").is_some());
}

/// Test backchannel implementations.
#[tokio::test]
async fn test_backchannel_implementations() {
    // Test PollingBackchannel
    let polling = PollingBackchannel::new("http://worker:8080".parse().unwrap());

    // Test ping functionality (will fail in test but should not panic)
    let health = polling.ping("test-log-id").await;
    assert!(health.is_err()); // Expected to fail without real server

    // Test Jenkins backchannel
    let jenkins = JenkinsBackchannel::new("http://jenkins:8080".parse().unwrap());

    let health = jenkins.ping("test-log-id").await;
    assert!(health.is_err()); // Expected to fail without real server
}

/// Test builder configuration generation.
#[test]
fn test_builder_configuration() {
    // Test generic builder
    let generic_config = CampaignConfig {
        generic_build: Some(GenericBuildConfig {
            chroot: Some("ubuntu:20.04".to_string()),
        }),
        debian_build: None,
        force_build: false,
        default_empty: false,
    };

    let builder = get_builder(&generic_config, None, Some("dep-server-url".to_string()));
    assert!(builder.is_ok());

    // Test Debian builder
    let debian_config = CampaignConfig {
        generic_build: None,
        debian_build: Some(DebianBuildConfig {
            base_distribution: "unstable".to_string(),
            build_distribution: None,
            build_suffix: None,
            build_command: None,
            chroot: None,
            extra_build_distribution: vec![],
        }),
        force_build: false,
        default_empty: false,
    };

    let builder = get_builder(&debian_config, None, Some("dep-server-url".to_string()));
    assert!(builder.is_ok());
}

/// Test watchdog functionality.
#[tokio::test]
async fn test_watchdog_functionality() {
    use janitor_runner::database::RunnerDatabase;
    use janitor_runner::test_utils::TestDatabase;
    use janitor_runner::watchdog::{TerminationReason, Watchdog, WatchdogConfig};
    use std::sync::Arc;

    // Skip test if no database is available
    let test_db = match TestDatabase::new_optional().await {
        Ok(Some(db)) => db,
        Ok(None) => {
            eprintln!("Skipping watchdog test: no database available");
            return;
        }
        Err(e) => {
            eprintln!("Skipping watchdog test: database setup failed: {}", e);
            return;
        }
    };

    let config = WatchdogConfig {
        check_interval: 30,
        default_timeout: 3600,
        worker_heartbeat_timeout: 300,
        max_health_failures: 3,
        maintenance_interval: 300,
        max_run_age_hours: 6,
        ..WatchdogConfig::default()
    };

    // Create runner database with test pool
    let janitor_db = test_db.into_janitor_database();
    let runner_db = Arc::new(RunnerDatabase::new(janitor_db.pool().clone()));

    // ActiveRunStore is Redis-backed; skip the test cleanly if Redis
    // isn't reachable so this test stays runnable on machines without
    // a local Redis.
    let redis_url =
        std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let redis_client = match redis::Client::open(redis_url.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Skipping watchdog test: bad redis URL {}: {}", redis_url, e);
            return;
        }
    };
    if redis_client
        .get_multiplexed_async_connection()
        .await
        .is_err()
    {
        eprintln!("Skipping watchdog test: redis at {} unreachable", redis_url);
        return;
    }
    let active_runs = janitor_runner::active_runs::ActiveRunStore::with_key(
        redis_client,
        format!("runner:active-runs:test:{}", uuid::Uuid::new_v4().simple()),
    );

    // Confirm we can construct a `Watchdog` against a live test DB
    // without panicking, then exercise the `TerminationReason` API
    // that the watchdog uses internally.
    let _watchdog = Watchdog::new(runner_db, active_runs, config);

    let timeout_reason = TerminationReason::Timeout;
    assert_eq!(timeout_reason.result_code(), "worker-timeout");
    assert!(!timeout_reason.is_transient());

    let health_fail_reason = TerminationReason::HealthCheckFailed;
    assert_eq!(health_fail_reason.result_code(), "worker-failure");
    assert!(health_fail_reason.is_transient());

    let system_failure_reason = TerminationReason::SystemFailure("disk full".to_string());
    assert_eq!(system_failure_reason.result_code(), "system-failure");
    assert!(system_failure_reason.description().contains("disk full"));
}

/// `WorkerResult` deserialization: malformed JSON fails, sparse JSON
/// succeeds. The struct defaults `code` to `"success"` and marks
/// every other field `Option<...>` because real worker payloads
/// often omit fields on the success path; see the field docs on
/// `WorkerResult` in lib.rs.
#[test]
fn test_worker_result_deserialization() {
    let invalid_json = r#"{"invalid": json"#;
    let result: Result<WorkerResult, _> = serde_json::from_str(invalid_json);
    assert!(result.is_err());

    let minimal_json = r#"{"code": "success"}"#;
    let result: WorkerResult =
        serde_json::from_str(minimal_json).expect("minimal WorkerResult must parse");
    assert_eq!(result.code, "success");
    assert!(result.description.is_none());

    let empty_json = r#"{}"#;
    let result: WorkerResult =
        serde_json::from_str(empty_json).expect("empty WorkerResult must parse (code defaults)");
    assert_eq!(result.code, "success");
}

/// Test URL and network utilities.
#[test]
fn test_url_utilities() {
    use url::Url;

    // Test URL parsing for backchannel
    let valid_url = "http://worker:8080/status";
    let parsed = Url::parse(valid_url);
    assert!(parsed.is_ok());

    let invalid_url = "not-a-url";
    let parsed = Url::parse(invalid_url);
    assert!(parsed.is_err());
}

/// Test date/time handling compatibility.
#[test]
fn test_datetime_handling() {
    let now = Utc::now();

    // Test serialization to ISO 8601 (Python compatible)
    let json_str = serde_json::to_string(&now).unwrap();
    assert!(json_str.contains("T"));
    assert!(json_str.contains("Z"));

    // Test parsing from Python format
    let python_format = r#""2023-01-01T12:00:00Z""#;
    let parsed: DateTime<Utc> = serde_json::from_str(python_format).unwrap();
    assert_eq!(parsed.year(), 2023);
}

/// Test configuration validation.
#[test]
fn test_configuration_validation() {
    // Test valid campaign configuration
    let valid_config = CampaignConfig {
        generic_build: Some(GenericBuildConfig {
            chroot: Some("ubuntu:20.04".to_string()),
        }),
        debian_build: None,
        force_build: false,
        default_empty: false,
    };

    // Should be able to get builder without errors
    let builder = get_builder(&valid_config, None, Some("http://dep-server".to_string()));
    assert!(builder.is_ok());

    // Test empty configuration
    let empty_config = CampaignConfig {
        generic_build: None,
        debian_build: None,
        force_build: false,
        default_empty: false,
    };

    let builder = get_builder(&empty_config, None, Some("http://dep-server".to_string()));
    assert!(builder.is_ok()); // Empty config should default to generic builder
}
