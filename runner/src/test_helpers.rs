//! Test helpers specific to the runner service
//!
//! This module provides test utilities for testing runner-specific functionality
//! that requires database access.

use crate::database::RunnerDatabase;
use crate::resume::ResumeService;
use crate::test_utils::TestDatabase;
use std::sync::Arc;

/// Create a test RunnerDatabase from a TestDatabase
pub fn create_test_runner_db(test_db: &TestDatabase) -> Arc<RunnerDatabase> {
    Arc::new(RunnerDatabase::new(test_db.pool().clone()))
}

/// Load the production janitor schema (`schema/state.sql`) into
/// the given test database. Matches the shape of every production
/// table rather than an invented test-only subset, so tests that
/// exercise the real database code paths actually see the columns,
/// types, and constraints they'll hit in production.
///
/// A previous version of this helper created a severely stripped-down
/// `run` table with a made-up `branch_name` column and no dependencies
/// on `codebase`, `change_set`, `worker`, or the `result_tag` /
/// `vcs_type` types, which meant tests happily passed against a schema
/// that bore no resemblance to production.
pub async fn setup_runner_tables(test_db: &TestDatabase) -> Result<(), sqlx::Error> {
    janitor::schema::setup_test_database(test_db.pool()).await
}

/// Insert a minimal run row for tests, matching the production
/// `run` schema in `schema/state.sql`. Creates the upstream
/// `codebase` and `change_set` rows on demand so callers don't have
/// to set up every foreign-key dependency by hand.
pub async fn insert_test_run(
    test_db: &TestDatabase,
    run_id: &str,
    campaign: &str,
    codebase: &str,
    result_code: &str,
    revision: Option<&str>,
) -> Result<(), sqlx::Error> {
    // Upstream FK dependencies: codebase + change_set. Both use
    // ON CONFLICT DO NOTHING so calling this helper repeatedly in a
    // single test is safe.
    // Codebase check constraint requires (branch_url IS NULL) = (url IS NULL).
    sqlx::query(
        r#"
        INSERT INTO codebase (name, branch_url, url, vcs_type)
        VALUES ($1, $2, $2, 'git')
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(codebase)
    .bind(format!("https://example.invalid/{}", codebase))
    .execute(test_db.pool())
    .await?;

    let change_set_id = run_id;
    sqlx::query(
        r#"
        INSERT INTO change_set (id, campaign)
        VALUES ($1, $2)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(change_set_id)
    .bind(campaign)
    .execute(test_db.pool())
    .await?;

    sqlx::query(
        r#"
        INSERT INTO run (
            id, suite, codebase, result_code, revision,
            start_time, finish_time, logfilenames, change_set
        )
        VALUES ($1, $2, $3, $4, $5, NOW() - INTERVAL '1 minute', NOW(), '{}', $6)
        "#,
    )
    .bind(run_id)
    .bind(campaign)
    .bind(codebase)
    .bind(result_code)
    .bind(revision)
    .bind(change_set_id)
    .execute(test_db.pool())
    .await?;
    Ok(())
}

/// Insert a `new_result_branch` row so
/// [`ResumeService::check_resume_result`] can find the main branch's
/// remote name for a given run.
pub async fn insert_test_result_branch(
    test_db: &TestDatabase,
    run_id: &str,
    role: &str,
    remote_name: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO new_result_branch (run_id, role, remote_name)
        VALUES ($1, $2, $3)
        "#,
    )
    .bind(run_id)
    .bind(role)
    .bind(remote_name)
    .execute(test_db.pool())
    .await?;
    Ok(())
}

/// Test the resume service functionality with a real database
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_with_database;

    test_with_database! {
        async fn test_check_resume_result_finds_matching_revision(test_db: TestDatabase) {
            setup_runner_tables(&test_db).await.unwrap();

            let runner_db = create_test_runner_db(&test_db);
            let resume_service = ResumeService::new((*runner_db).clone());

            // Insert a successful run with a known tip revision and a
            // `main`-role result branch so check_resume_result can
            // populate ResumeInfo::branch_name.
            insert_test_run(
                &test_db,
                "run-1",
                "lintian-fixes",
                "example-codebase",
                "success",
                Some("rev-tip-1"),
            )
            .await
            .unwrap();
            insert_test_result_branch(&test_db, "run-1", "main", Some("origin/main"))
                .await
                .unwrap();

            // The resume lookup is keyed on (suite, revision), not on
            // any per-run branch name.
            let result = resume_service
                .check_resume_result("lintian-fixes", "rev-tip-1")
                .await
                .unwrap();
            assert_eq!(result.as_ref().map(|r| r.run_id.as_str()), Some("run-1"));
            let resume_info = result.unwrap();
            assert_eq!(resume_info.campaign, "lintian-fixes");
            assert_eq!(resume_info.codebase, "example-codebase");
            assert_eq!(resume_info.revision.as_deref(), Some("rev-tip-1"));
            assert_eq!(resume_info.result_code, "success");
            assert_eq!(resume_info.branch_name, "origin/main");

            // Same campaign but unknown revision -> no resume.
            let no_result = resume_service
                .check_resume_result("lintian-fixes", "nonexistent-rev")
                .await
                .unwrap();
            assert!(no_result.is_none());

            // Known revision but wrong campaign -> no resume.
            let wrong_campaign = resume_service
                .check_resume_result("other-campaign", "rev-tip-1")
                .await
                .unwrap();
            assert!(wrong_campaign.is_none());
        }
    }

    test_with_database! {
        async fn test_finish_run_inserts_run_and_result_branches(test_db: TestDatabase) {
            use crate::{JanitorResult, ResultResume};
            use breezyshim::RevisionId;
            use chrono::{Duration as ChronoDuration, Utc};

            setup_runner_tables(&test_db).await.unwrap();

            // Seed the upstream FK dependencies that store_run needs:
            // a codebase (referenced by run.codebase), the worker
            // (referenced by run.worker via run_worker_fkey), and a
            // queue entry whose id finish_run will delete as part of
            // its transaction.
            sqlx::query(
                "INSERT INTO codebase (name, branch_url, url, vcs_type)
                 VALUES ($1, $2, $2, 'git')",
            )
            .bind("finish-run-codebase")
            .bind("https://example.invalid/finish-run-codebase")
            .execute(test_db.pool())
            .await
            .unwrap();

            sqlx::query(
                "INSERT INTO worker (name, password)
                 VALUES ('worker-42', crypt('pw', gen_salt('bf')))
                 ON CONFLICT DO NOTHING",
            )
            .execute(test_db.pool())
            .await
            .unwrap();

            // The result carries resume.run_id = 'earlier-run' below,
            // and run.resume_from has an FK back to run.id -- seed an
            // ancestor run row so the FK is satisfied at insert time.
            sqlx::query(
                "INSERT INTO change_set (id, campaign)
                 VALUES ('cs-earlier', 'lintian-fixes')",
            )
            .execute(test_db.pool())
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO run (
                     id, suite, codebase, result_code,
                     start_time, finish_time, logfilenames, change_set
                 )
                 VALUES (
                     'earlier-run', 'lintian-fixes', 'finish-run-codebase',
                     'success', NOW() - INTERVAL '1 hour', NOW() - INTERVAL '55 minutes',
                     '{}', 'cs-earlier'
                 )",
            )
            .execute(test_db.pool())
            .await
            .unwrap();

            // queue.id is serial (int4); cast so we can decode into i64.
            let queue_id: i64 = sqlx::query_scalar(
                "INSERT INTO queue (codebase, suite, command)
                 VALUES ($1, $2, $3) RETURNING id::bigint",
            )
            .bind("finish-run-codebase")
            .bind("lintian-fixes")
            .bind("lintian-brush")
            .fetch_one(test_db.pool())
            .await
            .unwrap();

            // finish_run will synthesise the change_set id from log_id
            // because JanitorResult::change_set is None.
            let log_id = "finish-run-1";
            let start_time = Utc::now() - ChronoDuration::minutes(2);
            let finish_time = Utc::now();

            let mut result = JanitorResult {
                log_id: log_id.to_string(),
                branch_url: "https://example.invalid/finish-run-codebase".to_string(),
                subpath: Some("".to_string()),
                code: "success".to_string(),
                transient: None,
                codebase: "finish-run-codebase".to_string(),
                campaign: "lintian-fixes".to_string(),
                description: Some("ok".to_string()),
                codemod: Some(serde_json::json!({"changes": 2})),
                value: Some(10),
                logfilenames: vec!["worker.log".to_string()],
                start_time,
                finish_time,
                revision: Some(RevisionId::from(b"tip-revision".to_vec())),
                main_branch_revision: Some(RevisionId::from(b"base-revision".to_vec())),
                change_set: None,
                tags: Some(vec![
                    ("v1.0".to_string(), Some(RevisionId::from(b"tag-rev".to_vec()))),
                ]),
                remotes: None,
                branches: Some(vec![(
                    Some("main".to_string()),
                    Some("refs/heads/main".to_string()),
                    Some(RevisionId::from(b"base-revision".to_vec())),
                    Some(RevisionId::from(b"tip-revision".to_vec())),
                )]),
                failure_details: None,
                failure_stage: None,
                resume: Some(ResultResume {
                    run_id: "earlier-run".to_string(),
                }),
                target: None,
                worker_name: Some("worker-42".to_string()),
                vcs_type: Some("git".to_string()),
                target_branch_url: Some("https://example.invalid/finish-run-codebase".to_string()),
                context: Some(serde_json::json!({})),
                builder_result: None,
            };

            let runner_db = create_test_runner_db(&test_db);
            runner_db
                .finish_run(
                    &mut result,
                    "lintian-brush",
                    Some(&serde_json::json!({"from": "test"})),
                    queue_id,
                )
                .await
                .expect("finish_run should succeed");

            // finish_run is expected to have created a change_set row
            // whose id is the log_id.
            assert_eq!(result.change_set.as_deref(), Some(log_id));
            let change_set_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM change_set WHERE id = $1 AND campaign = $2",
            )
            .bind(log_id)
            .bind("lintian-fixes")
            .fetch_one(test_db.pool())
            .await
            .unwrap();
            assert_eq!(change_set_count, 1);

            // The core run row should reflect every column we set.
            let (run_suite, run_code, run_command, run_worker, run_codebase): (
                String,
                String,
                Option<String>,
                Option<String>,
                String,
            ) = sqlx::query_as(
                "SELECT suite, result_code, command, worker, codebase
                 FROM run WHERE id = $1",
            )
            .bind(log_id)
            .fetch_one(test_db.pool())
            .await
            .unwrap();
            assert_eq!(run_suite, "lintian-fixes");
            assert_eq!(run_code, "success");
            assert_eq!(run_command.as_deref(), Some("lintian-brush"));
            assert_eq!(run_worker.as_deref(), Some("worker-42"));
            assert_eq!(run_codebase, "finish-run-codebase");

            // result_tags is a result_tag[] composite; verify store_run's
            // unnest-based construction actually materialised our tag.
            let tag_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM (
                    SELECT unnest(result_tags) AS t FROM run WHERE id = $1
                 ) AS expanded",
            )
            .bind(log_id)
            .fetch_one(test_db.pool())
            .await
            .unwrap();
            assert_eq!(tag_count, 1);

            // new_result_branch should hold one main-role entry.
            let (role, remote_name): (String, Option<String>) = sqlx::query_as(
                "SELECT role, remote_name FROM new_result_branch WHERE run_id = $1",
            )
            .bind(log_id)
            .fetch_one(test_db.pool())
            .await
            .unwrap();
            assert_eq!(role, "main");
            assert_eq!(remote_name.as_deref(), Some("refs/heads/main"));

            // And the queue entry should have been deleted atomically.
            let queue_remaining: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM queue WHERE id = $1")
                    .bind(queue_id)
                    .fetch_one(test_db.pool())
                    .await
                    .unwrap();
            assert_eq!(queue_remaining, 0);
        }
    }

    test_with_database! {
        async fn test_check_resume_result_skips_rejected_runs(test_db: TestDatabase) {
            setup_runner_tables(&test_db).await.unwrap();

            let runner_db = create_test_runner_db(&test_db);
            let resume_service = ResumeService::new((*runner_db).clone());

            insert_test_run(
                &test_db,
                "run-rejected",
                "lintian-fixes",
                "example-codebase",
                "success",
                Some("rev-rejected"),
            )
            .await
            .unwrap();
            // Mark the run as rejected -- check_resume_result should
            // then return None, matching Python's behaviour.
            sqlx::query("UPDATE run SET publish_status = 'rejected' WHERE id = $1")
                .bind("run-rejected")
                .execute(test_db.pool())
                .await
                .unwrap();

            let result = resume_service
                .check_resume_result("lintian-fixes", "rev-rejected")
                .await
                .unwrap();
            assert!(result.is_none());
        }
    }
}
