//! `get_unchanged_run` finds the control run whose change set is named
//! after the run itself.

use super::get_unchanged_run;
use breezyshim::RevisionId;
use janitor::test_utils::TestDatabase;

#[tokio::test]
async fn finds_control_run_with_its_own_change_set() {
    let Ok(Some(db)) = TestDatabase::new_optional().await else {
        eprintln!("Skipping: no Postgres available");
        return;
    };
    // Need the Debian schema overlay so `debian_build` exists.
    if let Err(e) = janitor::schema::setup_debian_test_database(&db.pool).await {
        eprintln!("Skipping: schema setup failed: {}", e);
        return;
    }
    let pool = &db.pool;

    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type) \
         VALUES ('pkg', 'https://example.com/pkg', 'https://example.com/pkg', 'git')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO change_set (id, campaign) \
         VALUES ('control-run', 'control'), ('cs-other', 'unchanged')",
    )
    .execute(pool)
    .await
    .unwrap();
    // The control run has a change set of its own, named after the run.
    // The later run at the same revision is part of another change set.
    sqlx::query(
        "INSERT INTO run (id, codebase, suite, change_set, result_code, \
                          revision, main_branch_revision, start_time, finish_time, \
                          logfilenames) \
         VALUES ('control-run', 'pkg', 'control', 'control-run', 'success', \
                 'base-rev', 'base-rev', \
                 now() - interval '2 hours', now() - interval '1 hour', '{}'), \
                ('other-run', 'pkg', 'unchanged', 'cs-other', 'success', \
                 'base-rev', 'base-rev', \
                 now() - interval '30 minutes', now() - interval '10 minutes', '{}')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO debian_build (run_id, version, distribution, source) \
         VALUES ('control-run', '1.0-1~jan+unchanged1', 'unchanged', 'pkg')",
    )
    .execute(pool)
    .await
    .unwrap();

    let run = get_unchanged_run(pool, "pkg", &RevisionId::from(b"base-rev".to_vec()))
        .await
        .unwrap()
        .expect("the control run is found");
    assert_eq!(run.id, "control-run");
    assert_eq!(run.campaign, "control");
    assert_eq!(
        run.build_version.map(|v| v.to_string()).as_deref(),
        Some("1.0-1~jan+unchanged1")
    );

    let none = get_unchanged_run(pool, "pkg", &RevisionId::from(b"other-rev".to_vec()))
        .await
        .unwrap();
    assert!(none.is_none());
}
