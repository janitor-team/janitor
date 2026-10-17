//! Database test for `state::store_publish` against the schema in
//! `schema/state.sql`.

use janitor::publish::Mode;
use janitor::{schema::setup_test_database, test_with_database};
use janitor_publish::state::store_publish;
use sqlx::PgPool;

/// Insert a codebase, a change set and a successful run.
async fn seed_run(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ('cb-1', 'https://example.invalid/cb-1', 'https://example.invalid/cb-1', 'git')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO change_set (id, campaign) VALUES ('cs-1', 'lintian-fixes')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO run (
            id, suite, codebase, command, result_code, revision, branch_url,
            start_time, finish_time, logfilenames, change_set, vcs_type
        ) VALUES (
            'run-1', 'lintian-fixes', 'cb-1', 'lintian-brush', 'success', 'rev-tip',
            'https://example.invalid/cb-1', NOW() - INTERVAL '1 minute', NOW(), '{}',
            'cs-1', 'git'
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

test_with_database! {
    async fn test_store_publish_records_run_id(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_run(test_db.pool()).await;

        let target_branch_url: url::Url = "https://example.invalid/cb-1".parse().unwrap();
        let revision = breezyshim::RevisionId::from(b"rev-tip".to_vec());
        store_publish(
            test_db.pool(),
            "cs-1",
            "cb-1",
            Some("lintian-fixes"),
            Some(&target_branch_url),
            None,
            None,
            Some(&revision),
            "main",
            Mode::Push,
            "push-failed",
            "Failed to push",
            None,
            Some("publish-1"),
            None,
            Some("run-1"),
        )
        .await
        .unwrap();

        let run_id: Option<String> =
            sqlx::query_scalar("SELECT run_id FROM publish WHERE id = 'publish-1'")
                .fetch_one(test_db.pool())
                .await
                .unwrap();
        assert_eq!(run_id.as_deref(), Some("run-1"));
    }
}
