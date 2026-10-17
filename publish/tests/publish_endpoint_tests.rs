use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use janitor::publish::Mode;
use janitor::vcs::{RemoteGitVcsManager, VcsManager, VcsType};
use janitor::{schema::setup_test_database, test_utils::TestDatabase, test_with_database};
use janitor_publish::rate_limiter::{NonRateLimiter, RateLimiter};
use janitor_publish::{AppState, PublishWorker};
use sqlx::PgPool;
use tower::ServiceExt;

fn branch_url(codebase: &str) -> String {
    format!("https://example.invalid/{}", codebase)
}

async fn seed_codebase_and_run(
    pool: &PgPool,
    codebase: &str,
    campaign: &str,
    run_id: &str,
    target_branch_url: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ($1, $2, $2, 'git')
         ON CONFLICT DO NOTHING",
    )
    .bind(codebase)
    .bind(branch_url(codebase))
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO change_set (id, campaign) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind("cs-1")
    .bind(campaign)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO run (
            id, codebase, suite, command, start_time, finish_time, description,
            result_code, branch_url, main_branch_revision, revision, value,
            change_set, logfilenames, vcs_type, target_branch_url
         ) VALUES (
            $1, $2, $3, 'noop', NOW(), NOW(), 'ok', 'success', $4,
            'rev-base', 'rev-tip', 100, 'cs-1', ARRAY[]::text[], 'git', $5
         )",
    )
    .bind(run_id)
    .bind(codebase)
    .bind(campaign)
    .bind(branch_url(codebase))
    .bind(target_branch_url)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO new_result_branch (run_id, role, remote_name, base_revision, revision)
         VALUES ($1, 'main', 'main', 'rev-base', 'rev-tip')",
    )
    .bind(run_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn publish_worker() -> PublishWorker {
    PublishWorker::new(
        None,
        None,
        url::Url::parse("http://differ.invalid").unwrap(),
        None,
        None,
        None,
    )
    .await
}

async fn build_app(pool: PgPool) -> axum::Router {
    let config: &'static janitor::config::Config = Box::leak(Box::new(
        janitor::config::read_string("").expect("empty config parses"),
    ));
    let health_checker = Arc::new(janitor_publish::health::BasicHealthChecker::with_info(
        "publish-test".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    ));
    let state = Arc::new(AppState {
        conn: pool,
        bucket_rate_limiter: Arc::new(Mutex::new(Box::new(NonRateLimiter))),
        forge_rate_limiter: Arc::new(RwLock::new(HashMap::new())),
        push_limit: None,
        redis: None,
        redis_manager: None,
        config,
        publish_worker: publish_worker().await,
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

/// Put a `janitor-publish-one` on PATH that keeps the request it is sent
/// and then fails, and return the path the request lands in.
fn fake_publish_one() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("publish-one");
    std::fs::create_dir_all(&dir).unwrap();
    let request_path = dir.join("request.json");
    let script = dir.join("janitor-publish-one");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n/bin/cat > '{}'\n\
             echo '{{\"code\": \"test-failure\", \"description\": \"stopped by the test\"}}'\n\
             exit 1\n",
            request_path.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Every test in this file is serial, so no other test resolves a
    // binary while PATH starts with this directory.
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{path}", dir.display()));
    request_path
}

/// The `target_branch_url` that `publish_and_store` asks the
/// `janitor-publish-one` child to publish to.
async fn published_target(pool: &PgPool, run_id: &str, request_path: &Path) -> String {
    let run = janitor_publish::state::get_run(pool, run_id)
        .await
        .unwrap()
        .expect("run is stored");
    let mut vcs_managers: HashMap<VcsType, Box<dyn VcsManager>> = HashMap::new();
    vcs_managers.insert(
        VcsType::Git,
        Box::new(RemoteGitVcsManager::new(
            "https://example.invalid/git/".parse().unwrap(),
        )),
    );
    let rate_limiter: Mutex<Box<dyn RateLimiter>> = Mutex::new(Box::new(NonRateLimiter));
    let _ = std::fs::remove_file(request_path);
    // The child reports a failure, so the return value carries nothing
    // about the request it was sent.
    let result = janitor_publish::publish_and_store(
        pool,
        None,
        &janitor::config::Campaign::new(),
        &publish_worker().await,
        &format!("publish-{run_id}"),
        &run,
        Mode::Propose,
        "main",
        None,
        &vcs_managers,
        &rate_limiter,
        Some(true),
        false,
        None,
    )
    .await;
    let request = std::fs::read(request_path).unwrap_or_else(|e| {
        panic!("the child was not started ({e}); publish_and_store returned {result:?}")
    });
    let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
    request["target_branch_url"].as_str().unwrap().to_string()
}

test_with_database! {
    async fn publish_endpoint_returns_404_when_no_policy_and_no_mode(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes", "run-1", None).await;

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
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes", "run-1", None).await;

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
    async fn publish_prefers_run_target_branch_url_over_branch_url(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let pool = test_db.pool();
        let target = "https://example.invalid/other/cb-1";
        seed_codebase_and_run(pool, "cb-1", "lintian-fixes", "run-target", Some(target)).await;
        seed_codebase_and_run(pool, "cb-1", "lintian-fixes", "run-plain", None).await;
        seed_codebase_and_run(pool, "cb-1", "lintian-fixes", "run-empty", Some("")).await;

        let request_path = fake_publish_one();
        assert_eq!(
            published_target(pool, "run-target", &request_path).await,
            format!("{target},branch=main")
        );
        assert_eq!(
            published_target(pool, "run-plain", &request_path).await,
            format!("{},branch=main", branch_url("cb-1"))
        );
        // An empty target_branch_url is stored as text, not NULL.
        assert_eq!(
            published_target(pool, "run-empty", &request_path).await,
            format!("{},branch=main", branch_url("cb-1"))
        );
    }
}
