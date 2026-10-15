//! Queries that decode into `janitor::state::Run` accept NULL text columns.

use janitor::state::Run;
use janitor::{schema::setup_test_database, test_with_database};
use janitor_publish::state::{get_last_effective_run, get_run, iter_publish_ready};
use sqlx::PgPool;

/// Seed a publish-ready run, apply `null_sql`, read it back via every `Run` query.
async fn read_run_after(pool: &PgPool, null_sql: &'static str) -> Vec<Run> {
    setup_test_database(pool).await.unwrap();
    for sql in [
        "INSERT INTO codebase (name, branch_url, url, vcs_type) \
         VALUES ('pkg', 'https://example.com/pkg', 'https://example.com/pkg', 'git')",
        "INSERT INTO change_set (id, campaign, state) VALUES ('cs', 'lintian-fixes', 'ready')",
        "INSERT INTO named_publish_policy (name, per_branch_policy, rate_limit_bucket) \
         VALUES ('lintian-fixes', \
                 ARRAY[ROW('main', 'propose', NULL)::branch_publish_policy], 'bucket')",
        "INSERT INTO candidate (suite, command, publish_policy, codebase) \
         VALUES ('lintian-fixes', 'lintian-brush', 'lintian-fixes', 'pkg')",
        "INSERT INTO run (id, command, start_time, finish_time, result_code, \
                          revision, suite, vcs_type, branch_url, logfilenames, \
                          change_set, codebase, publish_status) \
         VALUES ('run-1', 'lintian-brush', now() - interval '1 hour', now(), \
                 'success', 'rev-1', 'lintian-fixes', 'git', \
                 'https://example.com/pkg', '{}', 'cs', 'pkg', 'approved')",
        "INSERT INTO new_result_branch (run_id, role, remote_name, revision) \
         VALUES ('run-1', 'main', 'master', 'rev-1')",
        null_sql,
    ] {
        sqlx::query(sql).execute(pool).await.unwrap();
    }

    let last = get_last_effective_run(pool, "pkg", "lintian-fixes")
        .await
        .unwrap()
        .expect("no last effective run");
    let by_id = get_run(pool, "run-1").await.unwrap().expect("no run");
    let ready = iter_publish_ready(pool, Some("run-1")).await.unwrap();
    assert_eq!(ready.len(), 1);
    let (ready_run, _, _, _) = ready.into_iter().next().unwrap();
    vec![last, by_id, ready_run]
}

test_with_database! {
    async fn test_run_null_command(test_db: TestDatabase) {
        let runs = read_run_after(
            test_db.pool(),
            "UPDATE run SET command = NULL WHERE id = 'run-1'",
        )
        .await;
        for run in runs {
            assert_eq!(run.command, None);
            assert_eq!(run.branch_url.as_deref(), Some("https://example.com/pkg"));
        }
    }
}

test_with_database! {
    async fn test_run_null_branch_url(test_db: TestDatabase) {
        let runs = read_run_after(
            test_db.pool(),
            "UPDATE run SET branch_url = NULL WHERE id = 'run-1'",
        )
        .await;
        for run in runs {
            assert_eq!(run.branch_url, None);
            assert_eq!(run.command.as_deref(), Some("lintian-brush"));
        }
    }
}

test_with_database! {
    async fn test_run_null_vcs_type(test_db: TestDatabase) {
        let runs = read_run_after(
            test_db.pool(),
            "UPDATE run SET vcs_type = NULL WHERE id = 'run-1'",
        )
        .await;
        for run in runs {
            assert_eq!(run.vcs_type, None);
            assert_eq!(run.command.as_deref(), Some("lintian-brush"));
        }
    }
}

test_with_database! {
    async fn test_run_null_remote_name(test_db: TestDatabase) {
        let runs = read_run_after(
            test_db.pool(),
            "UPDATE new_result_branch SET remote_name = NULL WHERE run_id = 'run-1'",
        )
        .await;
        for run in runs {
            assert_eq!(run.get_result_branch("main").unwrap().0, None);
            assert_eq!(run.vcs_type.as_deref(), Some("git"));
        }
    }
}
