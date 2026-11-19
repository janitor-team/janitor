//! Cross-service integration test: Runner → Publisher data flow.
//!
//! Proves that a run persisted by `janitor_runner::database::RunnerDatabase::finish_run`
//! is immediately visible to `janitor_publish::state::iter_publish_ready`
//! under the same Postgres schema. If the runner and publisher ever drift
//! on their view of the `run` / `new_result_branch` / `publish_ready`
//! surfaces, this test fails.

use std::collections::HashMap;

use breezyshim::revisionid::RevisionId;
use chrono::Utc;
use janitor_types::{Campaign, ChangeSetId, CodebaseName, RunId};
use janitor::schema::setup_test_database;
use janitor::test_with_database;
use janitor_publish::state::iter_publish_ready;
use janitor_runner::database::RunnerDatabase;
use janitor_runner::{JanitorResult, ResultRemote};
use sqlx::PgPool;

// Schema-compatible seed: codebase + named_publish_policy + candidate +
// change_set. Everything else (run, new_result_branch) is inserted by
// the runner's store_run path.
async fn seed_prerequisites(pool: &PgPool, codebase: &str, suite: &str, change_set: &str) {
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type) \
         VALUES ($1, 'https://example.com/pkg', 'https://example.com/pkg', 'git')",
    )
    .bind(codebase)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO named_publish_policy (name, per_branch_policy, rate_limit_bucket) \
         VALUES ('default', ARRAY[ROW('main', 'propose', NULL)::branch_publish_policy], 'default')",
    )
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO change_set (id, campaign, state) VALUES ($1, $2, 'ready')",
    )
    .bind(change_set)
    .bind(suite)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO candidate (campaign, codebase, command, publish_policy, change_set) \
         VALUES ($1, $2, 'echo hi', 'default', $3)",
    )
    .bind(suite)
    .bind(codebase)
    .bind(change_set)
    .execute(pool)
    .await
    .unwrap();

    // queue row for finish_run to delete; NOT NULL: codebase, campaign.
    sqlx::query(
        "INSERT INTO queue (codebase, campaign, command, change_set) VALUES ($1, $2, 'echo hi', $3)",
    )
    .bind(codebase)
    .bind(suite)
    .bind(change_set)
    .execute(pool)
    .await
    .unwrap();
}

async fn queue_id(pool: &PgPool, codebase: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM queue WHERE codebase = $1")
        .bind(codebase)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn make_result(log_id: &str, codebase: &str, suite: &str, change_set: &str) -> JanitorResult {
    JanitorResult {
        log_id: RunId::new(log_id).unwrap(),
        branch_url: "https://example.com/pkg".to_string(),
        subpath: None,
        code: "success".to_string(),
        transient: None,
        codebase: CodebaseName::new(codebase).unwrap(),
        campaign: Campaign::new(suite).unwrap(),
        description: Some("runner → publisher".to_string()),
        codemod: None,
        value: Some(1),
        logfilenames: vec!["build.log".to_string()],
        start_time: Utc::now(),
        finish_time: Utc::now(),
        revision: Some(RevisionId::from(b"new-revision".to_vec())),
        main_branch_revision: Some(RevisionId::from(b"base-revision".to_vec())),
        change_set: Some(ChangeSetId::new(change_set).unwrap()),
        tags: None,
        remotes: Some(HashMap::<String, ResultRemote>::new()),
        branches: None,
        failure_details: None,
        failure_stage: None,
        resume: None,
        target: None,
        worker_name: Some("test-worker".to_string()),
        vcs_type: Some("git".to_string()),
        target_branch_url: Some("https://example.com/pkg".to_string()),
        context: None,
        builder_result: None,
    }
}

async fn insert_result_branch(pool: &PgPool, run_id: &str, revision: &str) {
    sqlx::query(
        "INSERT INTO new_result_branch (run_id, role, remote_name, base_revision, revision) \
         VALUES ($1, 'main', 'main', 'base-revision', $2)",
    )
    .bind(run_id)
    .bind(revision)
    .execute(pool)
    .await
    .unwrap();
}

async fn approve_run(pool: &PgPool, run_id: &str) {
    sqlx::query("UPDATE run SET publish_status = 'approved' WHERE id = $1")
        .bind(run_id)
        .execute(pool)
        .await
        .unwrap();
}

test_with_database! {
    async fn runner_finish_run_is_visible_to_publisher_iter_publish_ready(
        test_db: TestDatabase
    ) {
        setup_test_database(test_db.pool()).await.unwrap();
        let pool = test_db.pool();

        let codebase = "cross-test-pkg";
        let suite = "lintian-fixes";
        let change_set = "cs-cross-1";
        let run_id = "run-cross-1";

        seed_prerequisites(pool, codebase, suite, change_set).await;
        let qid = queue_id(pool, codebase).await;

        // Runner side: persist the completed run.
        let db = RunnerDatabase::new(pool.clone());
        let mut result = make_result(run_id, codebase, suite, change_set);
        db.finish_run(&mut result, "echo hi", None, qid)
            .await
            .expect("finish_run");

        // A successful run with a result branch and approved publish_status is
        // what iter_publish_ready looks for.
        insert_result_branch(pool, run_id, "new-revision").await;
        approve_run(pool, run_id).await;

        // Publisher side: the same row must now be visible.
        let rows = iter_publish_ready(pool, Some(&RunId::new(run_id).unwrap())).await.unwrap();
        assert_eq!(rows.len(), 1, "publisher saw {} rows, expected 1", rows.len());
        let (run, _rate_limit_bucket, policy_command, unpublished) = &rows[0];
        assert_eq!(run.id, run_id);
        assert_eq!(run.codebase, codebase);
        assert_eq!(run.campaign.as_str(), suite);
        assert_eq!(policy_command, "echo hi");
        assert_eq!(
            unpublished.len(),
            1,
            "expected one unpublished branch, got {}",
            unpublished.len()
        );
        assert_eq!(unpublished[0].role, "main");
    }
}
