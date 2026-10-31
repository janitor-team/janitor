use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

use breezyshim::RevisionId;
use janitor::publish::Mode;
use janitor::vcs::{RemoteGitVcsManager, VcsManager, VcsType};
use janitor::{schema::setup_test_database, test_with_database};
use janitor::{Campaign, CodebaseName, RunId};
use janitor_publish::rate_limiter::{NonRateLimiter, RateLimiter};
use janitor_publish::{publish_and_store, PublishWorker};
use sqlx::PgPool;

/// A run with a single result branch for `role`.
fn make_run(role: &str, revision: Option<&str>) -> janitor::state::Run {
    janitor::state::Run {
        id: RunId::new("run-1").unwrap(),
        command: "noop".to_string(),
        description: None,
        result_code: "success".to_string(),
        main_branch_revision: None,
        revision: None,
        context: None,
        result: None,
        suite: Campaign::new("lintian-fixes").unwrap(),
        instigated_context: None,
        vcs_type: "git".to_string(),
        branch_url: "https://example.invalid/cb-1".to_string(),
        logfilenames: None,
        worker_name: None,
        result_branches: Some(vec![(
            role.to_string(),
            role.to_string(),
            None,
            revision.map(|r| RevisionId::from(r.as_bytes().to_vec())),
        )]),
        result_tags: None,
        target_branch_url: None,
        change_set: janitor::ChangeSetId::new("cs-1").unwrap(),
        failure_details: None,
        failure_transient: None,
        failure_stage: None,
        codebase: CodebaseName::new("cb-1").unwrap(),
        start_time: chrono::Utc::now(),
        finish_time: chrono::Utc::now(),
        value: None,
    }
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

async fn publish_main_role(
    pool: &PgPool,
    run: &janitor::state::Run,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    publish_main_role_with(pool, run, &publish_worker().await, &HashMap::new()).await
}

async fn publish_main_role_with(
    pool: &PgPool,
    run: &janitor::state::Run,
    publish_worker: &PublishWorker,
    vcs_managers: &HashMap<VcsType, Box<dyn VcsManager>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let campaign: janitor::config::Campaign =
        protobuf::text_format::parse_from_str(r#"name: "lintian-fixes" command: "noop""#).unwrap();
    let bucket_rate_limiter: Mutex<Box<dyn RateLimiter>> = Mutex::new(Box::new(NonRateLimiter));
    publish_and_store(
        pool,
        None,
        &campaign,
        publish_worker,
        &janitor::PublishId::new("publish-1").unwrap(),
        run,
        Mode::Propose,
        "main",
        None,
        vcs_managers,
        &bucket_rate_limiter,
        Some(true),
        false,
        None,
    )
    .await
}

// These checks happen before the database is touched, so a lazy pool
// that never connects is enough.
fn unconnected_pool() -> PgPool {
    PgPool::connect_lazy("postgres://localhost/unused").unwrap()
}

#[tokio::test]
async fn publish_and_store_errors_without_result_branch_for_role() {
    let run = make_run("other", Some("rev-tip"));
    let err = publish_main_role(&unconnected_pool(), &run)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "run run-1 has no result branch for role main"
    );
}

#[tokio::test]
async fn publish_and_store_errors_without_tip_revision() {
    let run = make_run("main", None);
    let err = publish_main_role(&unconnected_pool(), &run)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "run run-1 role main has no tip revision");
}

test_with_database! {
    async fn publish_and_store_errors_without_vcs_manager(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let run = make_run("main", Some("rev-tip"));
        let err = publish_main_role(test_db.pool(), &run).await.unwrap_err();
        assert_eq!(err.to_string(), "no VcsManager registered for vcs_type Git");
    }
}

/// The `target_branch_url` that `publish_and_store` sends to publish-one.
async fn published_target_branch_url(pool: &PgPool, run: &janitor::state::Run) -> String {
    let dir = tempfile::tempdir().unwrap();
    let request_path = dir.path().join("request.json");
    let script = dir.path().join("publish-one");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncat > '{}'\nexit 1\n", request_path.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut publish_worker = publish_worker().await;
    publish_worker.publish_one_command = vec![script.to_str().unwrap().to_string()];
    let mut vcs_managers: HashMap<VcsType, Box<dyn VcsManager>> = HashMap::new();
    vcs_managers.insert(
        VcsType::Git,
        Box::new(RemoteGitVcsManager::new(
            "https://example.invalid/git/".parse().unwrap(),
        )),
    );
    // The stand-in fails, so only the request it was sent matters here.
    let _ = publish_main_role_with(pool, run, &publish_worker, &vcs_managers).await;
    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(request_path).unwrap()).unwrap();
    request["target_branch_url"].as_str().unwrap().to_string()
}

test_with_database! {
    async fn publish_and_store_publishes_to_run_target_branch_url(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let mut run = make_run("main", Some("rev-tip"));
        assert_eq!(
            published_target_branch_url(test_db.pool(), &run).await,
            "https://example.invalid/cb-1,branch=main"
        );
        run.target_branch_url = Some("https://example.invalid/other".to_string());
        assert_eq!(
            published_target_branch_url(test_db.pool(), &run).await,
            "https://example.invalid/other,branch=main"
        );
    }
}
