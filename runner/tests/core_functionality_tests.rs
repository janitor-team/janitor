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

mod watchdog {
    use chrono::{Duration, Utc};
    use janitor_runner::watchdog::Watchdog;
    use janitor_runner::{ActiveRun, AppState, Backchannel, VcsInfo};
    use std::sync::Arc;

    const RUN_TIMEOUT_MINUTES: u64 = 60;

    /// Register `run` as active with its last keepalive `keepalive_age`
    /// ago. Returns `None` if no database/redis is available.
    async fn setup(run: &ActiveRun, keepalive_age: Duration) -> Option<Arc<AppState>> {
        janitor_runner::test_utils::ensure_redis().await;
        let state = janitor_runner::test_utils::create_test_app_state_if_available()
            .await
            .unwrap()?;
        let pool = state.database.pool();
        sqlx::query(
            "INSERT INTO codebase (name, branch_url, url, vcs_type)
             VALUES ($1, 'https://example.invalid/x', 'https://example.invalid/x', 'git')",
        )
        .bind(&run.codebase)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO worker (name, password) VALUES ($1, 'pw')")
            .bind(&run.worker_name)
            .execute(pool)
            .await
            .unwrap();
        state.active_runs.store(run.clone()).await;
        state
            .active_runs
            .record_keepalive(&run.log_id, Utc::now() - keepalive_age)
            .await
            .unwrap();
        Some(state)
    }

    fn make_run(log_id: &str, start_age: Duration, backchannel: Backchannel) -> ActiveRun {
        ActiveRun {
            worker_name: "worker-1".to_string(),
            worker_link: None,
            queue_id: 1,
            log_id: log_id.to_string(),
            start_time: Utc::now() - start_age,
            finish_time: None,
            estimated_duration: Some(std::time::Duration::from_secs(30)),
            campaign: "lintian-fixes".to_string(),
            change_set: None,
            command: "lintian-brush".to_string(),
            backchannel,
            vcs_info: VcsInfo::default(),
            codebase: "watchdog-codebase".to_string(),
            instigated_context: None,
            resume_from: None,
        }
    }

    /// Serve a worker backchannel whose `/log-id` reports `log_id`.
    async fn serve_worker(log_id: &'static str) -> Backchannel {
        let app =
            axum::Router::new().route("/log-id", axum::routing::get(move || async move { log_id }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Backchannel::Polling {
            my_url: format!("http://{}", addr),
        }
    }

    /// An unreachable worker: pings fail, but not fatally.
    fn unreachable_worker() -> Backchannel {
        Backchannel::Polling {
            my_url: "http://127.0.0.1:1".to_string(),
        }
    }

    async fn stored_result(
        state: &AppState,
        log_id: &str,
    ) -> Option<(String, String, Option<bool>)> {
        sqlx::query_as("SELECT result_code, description, failure_transient FROM run WHERE id = $1")
            .bind(log_id)
            .fetch_optional(state.database.pool())
            .await
            .unwrap()
    }

    fn watchdog(state: &AppState) -> Watchdog {
        Watchdog::new(
            state.database.clone(),
            state.active_runs.clone(),
            RUN_TIMEOUT_MINUTES,
        )
    }

    /// A run that has been going for much longer than the run timeout
    /// (and its estimate) is left alone as long as keepalives arrive.
    #[tokio::test]
    async fn test_watchdog_ignores_long_runs_with_recent_keepalive() {
        let run = make_run("long-run", Duration::days(2), unreachable_worker());
        let Some(state) = setup(&run, Duration::minutes(5)).await else {
            return;
        };
        watchdog(&state).check_active_runs().await.unwrap();
        assert!(state.active_runs.get("long-run").await.is_some());
        assert_eq!(stored_result(&state, "long-run").await, None);
    }

    /// Without keepalives for longer than the run timeout, the run is
    /// aborted as a transient worker-timeout.
    #[tokio::test]
    async fn test_watchdog_aborts_run_without_keepalives() {
        let run = make_run("stale-run", Duration::minutes(90), unreachable_worker());
        let Some(state) = setup(&run, Duration::minutes(61)).await else {
            return;
        };
        watchdog(&state).check_active_runs().await.unwrap();
        assert!(state.active_runs.get("stale-run").await.is_none());
        let (code, description, transient) = stored_result(&state, "stale-run").await.unwrap();
        assert_eq!(code, "worker-timeout");
        assert!(
            description.starts_with("No keepalives received in 1:01:0"),
            "{}",
            description
        );
        assert_eq!(transient, Some(true));
    }

    /// A successful ping counts as a keepalive.
    #[tokio::test]
    async fn test_watchdog_ping_refreshes_keepalive() {
        let backchannel = serve_worker("alive-run").await;
        let run = make_run("alive-run", Duration::minutes(90), backchannel);
        let Some(state) = setup(&run, Duration::minutes(61)).await else {
            return;
        };
        let before = Utc::now();
        watchdog(&state).check_active_runs().await.unwrap();
        assert!(state.active_runs.get("alive-run").await.is_some());
        let keepalive = state
            .active_runs
            .last_keepalive("alive-run")
            .await
            .unwrap()
            .unwrap();
        assert!(keepalive >= before - Duration::seconds(1));
        assert_eq!(stored_result(&state, "alive-run").await, None);
    }

    /// A worker that has moved on to another run is reported as
    /// run-disappeared, regardless of keepalive age.
    #[tokio::test]
    async fn test_watchdog_aborts_run_when_worker_moved_on() {
        let backchannel = serve_worker("other-run").await;
        let run = make_run("lost-run", Duration::minutes(30), backchannel);
        let Some(state) = setup(&run, Duration::minutes(25)).await else {
            return;
        };
        watchdog(&state).check_active_runs().await.unwrap();
        assert!(state.active_runs.get("lost-run").await.is_none());
        assert_eq!(
            stored_result(&state, "lost-run").await,
            Some((
                "run-disappeared".to_string(),
                "Worker started processing new run other-run rather than lost-run".to_string(),
                Some(true)
            ))
        );
    }

    /// Runs whose keepalive is younger than a third of the timeout are
    /// not pinged at all.
    #[tokio::test]
    async fn test_watchdog_does_not_ping_fresh_runs() {
        let backchannel = serve_worker("other-run").await;
        let run = make_run("fresh-run", Duration::minutes(30), backchannel);
        let Some(state) = setup(&run, Duration::minutes(19)).await else {
            return;
        };
        watchdog(&state).check_active_runs().await.unwrap();
        assert!(state.active_runs.get("fresh-run").await.is_some());
    }

    /// Runs without a backchannel time out like any other run.
    #[tokio::test]
    async fn test_watchdog_aborts_run_without_backchannel_on_timeout() {
        let run = make_run("no-bc-run", Duration::minutes(90), Backchannel::None {});
        let Some(state) = setup(&run, Duration::minutes(61)).await else {
            return;
        };
        watchdog(&state).check_active_runs().await.unwrap();
        let (code, _, _) = stored_result(&state, "no-bc-run").await.unwrap();
        assert_eq!(code, "worker-timeout");
    }

    /// Runs without a backchannel that haven't been heard from in over
    /// a day are reported as run-disappeared.
    #[tokio::test]
    async fn test_watchdog_aborts_run_without_backchannel_after_a_day() {
        let run = make_run("old-no-bc-run", Duration::days(3), Backchannel::None {});
        let Some(state) = setup(&run, Duration::days(2)).await else {
            return;
        };
        watchdog(&state).check_active_runs().await.unwrap();
        assert_eq!(
            stored_result(&state, "old-no-bc-run").await,
            Some((
                "run-disappeared".to_string(),
                "no support for ping, and haven't heard back in > 1 day".to_string(),
                Some(true)
            ))
        );
    }
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
