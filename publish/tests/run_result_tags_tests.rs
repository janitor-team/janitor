//! Publisher queries that load a Run decode a set result_tags column.

use janitor::{schema::setup_test_database, test_with_database};
use janitor_publish::state::{get_last_effective_run, get_run, iter_publish_ready};
use sqlx::PgPool;

/// Insert an approved publishable run with one result branch and one tag.
async fn seed_tagged_run(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ('cb-1', 'https://example.invalid/cb-1', 'https://example.invalid/cb-1', 'git')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO change_set (id, campaign, state) VALUES ('cs-1', 'lintian-fixes', 'ready')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO named_publish_policy (name, per_branch_policy, rate_limit_bucket)
         VALUES ('lintian-fixes',
                 ARRAY[ROW('main', 'propose', NULL)::branch_publish_policy],
                 'bucket-1')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO candidate (suite, command, publish_policy, codebase)
         VALUES ('lintian-fixes', 'lintian-brush', 'lintian-fixes', 'cb-1')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO run (
            id, codebase, suite, command, start_time, finish_time,
            result_code, branch_url, main_branch_revision, revision, value,
            change_set, logfilenames, vcs_type, publish_status, result_tags
         ) VALUES (
            'run-1', 'cb-1', 'lintian-fixes', 'lintian-brush',
            NOW() - interval '1 hour', NOW(), 'success',
            'https://example.invalid/cb-1', 'rev-base', 'rev-tip', 100,
            'cs-1', ARRAY[]::text[], 'git', 'approved',
            ARRAY[ROW('upstream/1.0', 'rev-tag')::result_tag]
         )",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO new_result_branch (run_id, role, remote_name, base_revision, revision)
         VALUES ('run-1', 'main', 'master', 'rev-base', 'rev-tip')",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn expected_tags() -> Option<Vec<(String, String)>> {
    Some(vec![("upstream/1.0".to_string(), "rev-tag".to_string())])
}

test_with_database! {
    async fn get_run_decodes_result_tags(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_tagged_run(test_db.pool()).await;

        let run = get_run(test_db.pool(), "run-1")
            .await
            .expect("get_run should decode a run with result tags")
            .expect("run-1 exists");
        assert_eq!(run.result_tags, expected_tags());
    }
}

test_with_database! {
    async fn get_last_effective_run_decodes_result_tags(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_tagged_run(test_db.pool()).await;

        let run = get_last_effective_run(test_db.pool(), "cb-1", "lintian-fixes")
            .await
            .expect("get_last_effective_run should decode a run with result tags")
            .expect("cb-1 has an effective run");
        assert_eq!(run.id, "run-1");
        assert_eq!(run.result_tags, expected_tags());
    }
}

test_with_database! {
    async fn iter_publish_ready_decodes_result_tags(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_tagged_run(test_db.pool()).await;

        let ready = iter_publish_ready(test_db.pool(), None)
            .await
            .expect("iter_publish_ready should decode a run with result tags");
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0.id, "run-1");
        assert_eq!(ready[0].0.result_tags, expected_tags());
    }
}

test_with_database! {
    async fn get_run_keeps_missing_result_tags_as_none(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_tagged_run(test_db.pool()).await;
        sqlx::query("UPDATE run SET result_tags = NULL WHERE id = 'run-1'")
            .execute(test_db.pool())
            .await
            .unwrap();

        let run = get_run(test_db.pool(), "run-1").await.unwrap().unwrap();
        assert_eq!(run.result_tags, None);
    }
}
