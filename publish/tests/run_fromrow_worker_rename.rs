//! Regression test: `janitor::state::Run.worker_name` is decoded
//! from the schema column `run.worker`, not `run.worker_name`.
//!
//! Without `#[sqlx(rename = "worker")]` on the field, the FromRow
//! derive looks up a column literally named "worker_name", which
//! doesn't exist; every iter_publish_ready and get_last_effective_run
//! walk then 500s with `no column found for name: worker_name`.
//!
//! This test lives in the `publish` crate because the `breezyshim`
//! crate's sqlx Type/Decode impls (used by `Run.main_branch_revision`,
//! `revision`, `result_branches`) are gated behind its `sqlx` feature,
//! which the `publish` crate enables and the bare `janitor` crate does
//! not.

use janitor::state::Run;
use janitor::{schema::setup_test_database, test_utils::TestDatabase};
use sqlx::PgPool;

async fn make_db() -> Option<TestDatabase> {
    match TestDatabase::new_optional().await {
        Ok(Some(db)) => match setup_test_database(&db.pool).await {
            Ok(()) => Some(db),
            Err(e) => {
                eprintln!("Skipping FromRow test: schema setup failed: {}", e);
                None
            }
        },
        _ => {
            eprintln!("Skipping FromRow test: no Postgres available");
            None
        }
    }
}

async fn seed(pool: &PgPool) {
    // `codebase.check1`: url and branch_url must both be null or both
    // set. Use the same value for both.
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type) \
         VALUES ('frow-pkg', 'git://example.com/frow-pkg', \
                 'git://example.com/frow-pkg', 'git')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO worker (name, password) VALUES ('worker-frow', 'secret')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO change_set (id, campaign) VALUES ('cs-frow', 'lintian-fixes')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO run (id, codebase, suite, change_set, command, \
                          start_time, finish_time, result_code, \
                          revision, main_branch_revision, vcs_type, \
                          logfilenames, branch_url, worker) \
         VALUES ('run-frow', 'frow-pkg', 'lintian-fixes', 'cs-frow', \
                 'lintian-brush', \
                 now() - interval '1 hour', now() - interval '30 minutes', \
                 'success', 'rev-frow', 'rev-frow-parent', 'git', \
                 ARRAY['worker.log']::text[], 'git://example.com/frow-pkg', \
                 'worker-frow')",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn run_from_row_decodes_worker_column() {
    let Some(db) = make_db().await else { return };
    seed(&db.pool).await;

    // Match the shape production queries use: cast timestamps to
    // TIMESTAMPTZ via `AT TIME ZONE 'UTC'` so sqlx FromRow can decode
    // them as `DateTime<Utc>` (schema columns are TIMESTAMP without
    // zone).
    let run: Run = sqlx::query_as(
        "SELECT id, command, description, result_code, main_branch_revision, \
                revision, context, result, suite::text AS suite, \
                instigated_context, vcs_type::text AS vcs_type, branch_url, \
                logfilenames, worker, target_branch_url, change_set, \
                failure_details, failure_transient, failure_stage, codebase, \
                (start_time AT TIME ZONE 'UTC') AS start_time, \
                (finish_time AT TIME ZONE 'UTC') AS finish_time, \
                value, \
                array(SELECT row(role, remote_name, base_revision, revision) \
                      FROM new_result_branch WHERE run_id = run.id) AS result_branches, \
                result_tags \
         FROM run WHERE id = 'run-frow'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("FromRow must accept `worker` column via the rename attribute");

    assert_eq!(run.id, "run-frow");
    assert_eq!(run.worker_name.as_deref(), Some("worker-frow"));
}
