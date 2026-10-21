use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use janitor::{schema::setup_test_database, test_utils::TestDatabase, test_with_database};
use janitor_publish::{AppState, PublishWorker};
use sqlx::PgPool;
use tower::ServiceExt;

async fn seed_codebase_and_run(pool: &PgPool, codebase: &str, campaign: &str) {
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ($1, $2, $2, 'git')",
    )
    .bind(codebase)
    .bind(format!("https://example.invalid/{}", codebase))
    .execute(pool)
    .await
    .unwrap();

    sqlx::query("INSERT INTO change_set (id, campaign) VALUES ($1, $2)")
        .bind("cs-1")
        .bind(campaign)
        .execute(pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO run (
            id, codebase, suite, command, start_time, finish_time, description,
            result_code, branch_url, main_branch_revision, revision, value,
            change_set, logfilenames, vcs_type
         ) VALUES (
            $1, $2, $3, 'noop', NOW(), NOW(), 'ok', 'success', $4,
            'rev-base', 'rev-tip', 100, 'cs-1', ARRAY[]::text[], 'git'
         )",
    )
    .bind("run-1")
    .bind(codebase)
    .bind(campaign)
    .bind(format!("https://example.invalid/{}", codebase))
    .execute(pool)
    .await
    .unwrap();
}

async fn build_app(pool: PgPool) -> axum::Router {
    let config: &'static janitor::config::Config = Box::leak(Box::new(
        janitor::config::read_string("").expect("empty config parses"),
    ));
    let publish_worker = PublishWorker::new(
        None,
        None,
        url::Url::parse("http://differ.invalid").unwrap(),
        None,
        None,
        None,
    )
    .await;
    let health_checker = Arc::new(janitor_publish::health::BasicHealthChecker::with_info(
        "publish-test".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    ));
    let state = Arc::new(AppState {
        conn: pool,
        bucket_rate_limiter: Arc::new(Mutex::new(Box::new(
            janitor_publish::rate_limiter::NonRateLimiter,
        ))),
        forge_rate_limiter: Arc::new(RwLock::new(HashMap::new())),
        push_limit: None,
        redis: None,
        redis_manager: None,
        config,
        publish_worker,
        vcs_managers: Arc::new(HashMap::new()),
        modify_mp_limit: None,
        unexpected_mp_limit: None,
        gpg: Arc::new(breezyshim::gpg::GPGContext::new()),
        require_binary_diff: false,
        health_checker,
        last_full_scan_at: tokio::sync::watch::channel(None).0,
    });
    janitor_publish::web::app(state)
}

test_with_database! {
    async fn publish_endpoint_returns_404_when_no_policy_and_no_mode(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;

        let app = build_app(test_db.pool().clone()).await;
        let req = Request::builder()
            .method("POST")
            .uri("/lintian-fixes/cb-1/publish")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(""))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let reason = body["reason"].as_str().expect("reason field");
        assert!(
            reason.contains("cb-1") && reason.contains("lintian-fixes"),
            "reason should name the codebase/campaign: {reason}"
        );
    }
}

test_with_database! {
    async fn publish_endpoint_rejects_invalid_mode_with_400(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;

        let app = build_app(test_db.pool().clone()).await;
        let req = Request::builder()
            .method("POST")
            .uri("/lintian-fixes/cb-1/publish")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("mode=bogus"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

test_with_database! {
    async fn invalid_identifiers_in_path_are_rejected(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        let app = build_app(test_db.pool().clone()).await;
        for (method, uri) in [
            ("POST", "/lintian-fixes/Foo/publish"),
            ("POST", "/lintian_fixes/cb-1/publish"),
            ("GET", "/-foo/merge-proposals"),
            ("GET", "/c/foo%2Fbar/merge-proposals"),
            ("POST", "/consider/a.b"),
            ("GET", "/blockers/a%2Fb"),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(""))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{} {}", method, uri);
        }
    }
}

/// Redis for the branch locks; `None` when there is none to test with.
async fn test_redis_url() -> Option<String> {
    janitor_runner::test_utils::ensure_redis().await;
    let url = std::env::var("TEST_REDIS_URL").ok();
    if url.is_none() {
        eprintln!("skipping: TEST_REDIS_URL unset and no container");
    }
    url
}

/// A worker that runs `command` as its publish child and takes branch locks in `redis_url`.
async fn locking_worker(redis_url: &str, command: &[&str]) -> PublishWorker {
    let mut worker = PublishWorker::new(
        None,
        None,
        url::Url::parse("http://differ.invalid").unwrap(),
        None,
        None,
        Some(rslock::LockManager::new(vec![redis_url.to_string()])),
    )
    .await;
    worker.publish_one_command = command.iter().map(|s| s.to_string()).collect();
    worker
}

fn lock_test_target() -> url::Url {
    format!("https://example.com/lock-test-{}", uuid::Uuid::new_v4())
        .parse()
        .unwrap()
}

/// Propose a change to `target` through `PublishWorker::publish_one`.
async fn propose(
    worker: &PublishWorker,
    target: &url::Url,
) -> Result<janitor_publish::PublishOneResult, janitor_publish::PublishError> {
    worker
        .publish_one(
            &janitor::Campaign::new("campaign").unwrap(),
            &janitor::CodebaseName::new("codebase").unwrap(),
            "true",
            target,
            janitor::publish::Mode::Propose,
            "main",
            &breezyshim::RevisionId::from(b"rev".to_vec()),
            &janitor::RunId::new("log-id").unwrap(),
            Some(&janitor::RunId::new("unchanged-id").unwrap()),
            "campaign",
            None,
            &janitor::vcs::RemoteBzrVcsManager::new("https://example.com/bzr/".parse().unwrap()),
            None,
            false,
            true,
            None,
            None,
            None,
            None,
            &serde_json::Value::Null,
            None,
            None,
            None,
            None,
        )
        .await
}

/// Whether a second publisher could take the branch lock within `wait`.
async fn lock_is_free(redis_url: &str, target: &url::Url, wait: std::time::Duration) -> bool {
    let lock_manager = rslock::LockManager::new(vec![redis_url.to_string()]);
    let deadline = std::time::Instant::now() + wait;
    loop {
        if let Ok(lock) = lock_manager
            .lock(
                format!("publish:{target}").as_bytes(),
                std::time::Duration::from_secs(1),
            )
            .await
        {
            lock_manager.unlock(&lock).await;
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

const CHILD_ANSWER: &str =
    r#"{"branch_name": "b", "target_branch_url": "https://example.com/", "mode": "propose"}"#;

#[tokio::test]
async fn long_child_keeps_branch_lock() {
    let Some(redis_url) = test_redis_url().await else {
        return;
    };
    let script = format!("sleep 5; echo '{CHILD_ANSWER}'");
    let mut worker = locking_worker(&redis_url, &["sh", "-c", &script]).await;
    // The child runs for more than three lock lifetimes.
    worker.lock_lifetime = std::time::Duration::from_millis(1500);
    let target = lock_test_target();

    let publish = propose(&worker, &target);
    tokio::pin!(publish);
    let result = loop {
        tokio::select! {
            result = &mut publish => break result,
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                assert!(
                    !lock_is_free(&redis_url, &target, std::time::Duration::ZERO).await,
                    "a second publish got the branch lock while the child was running"
                );
            }
        }
    };
    assert_eq!(result.unwrap().branch_name(), "b");
    assert!(
        lock_is_free(&redis_url, &target, std::time::Duration::from_secs(2)).await,
        "branch lock still held after the child ended"
    );
}

#[tokio::test]
async fn cancelled_publish_kills_child_and_releases_branch_lock() {
    let Some(redis_url) = test_redis_url().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("child.pid");
    let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
    let worker = locking_worker(&redis_url, &["sh", "-c", &script]).await;
    let target = lock_test_target();

    tokio::time::timeout(std::time::Duration::from_secs(1), propose(&worker, &target))
        .await
        .expect_err("the child did not hang");

    wait_until_child_is_gone(&pid_file).await;
    assert!(
        lock_is_free(&redis_url, &target, std::time::Duration::from_secs(2)).await,
        "branch lock still held after the publish was cancelled"
    );
}

/// Wait up to 2 s for the child whose pid is in `pid_file` to be killed.
async fn wait_until_child_is_gone(pid_file: &std::path::Path) {
    let pid = std::fs::read_to_string(pid_file).unwrap();
    let stat = format!("/proc/{}/stat", pid.trim());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    // A killed child is gone, or a zombie until it is reaped.
    while std::fs::read_to_string(&stat).is_ok_and(|s| !s.contains(") Z ")) {
        assert!(
            std::time::Instant::now() < deadline,
            "the child is still running"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn publish_child_past_its_timeout_is_killed_and_releases_branch_lock() {
    let Some(redis_url) = test_redis_url().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("child.pid");
    let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
    let mut worker = locking_worker(&redis_url, &["sh", "-c", &script]).await;
    worker.publish_one_timeout = std::time::Duration::from_secs(1);
    let target = lock_test_target();

    let Err(janitor_publish::PublishError::Failure { code, description }) =
        propose(&worker, &target).await
    else {
        panic!("the publish did not fail with a failure code");
    };
    assert_eq!(
        (code.as_str(), description.as_str()),
        (
            "worker-timeout",
            "janitor-publish-one did not finish within 1s"
        )
    );
    wait_until_child_is_gone(&pid_file).await;
    assert!(
        lock_is_free(&redis_url, &target, std::time::Duration::from_secs(2)).await,
        "branch lock still held after the child timed out"
    );
}
