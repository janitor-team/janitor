use janitor::{schema::setup_test_database, test_utils::TestDatabase, test_with_database};
use janitor_publish::state::iter_publish_ready;
use sqlx::PgPool;

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

/// Give `run-1` a candidate and a policy without a rate limit bucket.
async fn seed_policy_without_bucket(pool: &PgPool, codebase: &str, campaign: &str) {
    sqlx::query(
        "INSERT INTO named_publish_policy (name, per_branch_policy) \
         VALUES ('nobucket', ARRAY[ROW('main', 'propose', NULL)::branch_publish_policy])",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO candidate (suite, command, publish_policy, codebase, value) \
         VALUES ($1, 'noop', 'nobucket', $2, 100)",
    )
    .bind(campaign)
    .bind(codebase)
    .execute(pool)
    .await
    .unwrap();
}

test_with_database! {
    async fn publish_ready_run_without_a_rate_limit_bucket(test_db: TestDatabase) {
        let pool = test_db.pool();
        setup_test_database(pool).await.unwrap();
        seed_codebase_and_run(pool, "cb-1", "lintian-fixes").await;
        seed_policy_without_bucket(pool, "cb-1", "lintian-fixes").await;
        sqlx::query("UPDATE run SET publish_status = 'approved' WHERE id = 'run-1'")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("UPDATE change_set SET state = 'ready' WHERE id = 'cs-1'")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO new_result_branch (run_id, role, remote_name, base_revision, revision) \
             VALUES ('run-1', 'main', 'main', 'rev-base', 'rev-tip')",
        )
        .execute(pool)
        .await
        .unwrap();
        let in_view: i64 = sqlx::query_scalar("SELECT count(*) FROM publish_ready")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(in_view, 1, "the seeded run must be in publish_ready");

        let ready = iter_publish_ready(pool, None).await.unwrap();
        assert_eq!(ready.len(), 1);
        let (run, rate_limit_bucket, policy_command, _) = &ready[0];
        assert_eq!(run.id, "run-1");
        assert_eq!(*rate_limit_bucket, None);
        assert_eq!(policy_command, "noop");
    }
}
