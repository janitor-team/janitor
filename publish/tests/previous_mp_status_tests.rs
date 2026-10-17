use janitor::{schema::setup_test_database, test_utils::TestDatabase, test_with_database};
use janitor_publish::state::get_previous_mp_status;
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

test_with_database! {
    async fn previous_mp_status_reads_a_closed_merge_proposal(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase_and_run(test_db.pool(), "cb-1", "lintian-fixes").await;
        sqlx::query(
            "INSERT INTO merge_proposal (codebase, url, status, revision) \
             VALUES ('cb-1', 'https://example.invalid/mp/1', 'closed', 'rev-tip')",
        )
        .execute(test_db.pool())
        .await
        .unwrap();

        let status = get_previous_mp_status(test_db.pool(), "cb-1", "lintian-fixes")
            .await
            .unwrap();
        assert_eq!(
            status,
            vec![(
                "https://example.invalid/mp/1".to_string(),
                "closed".to_string()
            )]
        );
    }
}
