//! Integration tests for the database helpers `check_existing_mp`
//! depends on.
//!
//! End-to-end testing of `check_existing_mp` itself is structurally
//! infeasible right now: the function takes a
//! `breezyshim::forge::MergeProposal` which is a thin wrapper around a
//! Python forge object, and there is no test fixture that builds one
//! without a real forge connection. Instead this file exercises the
//! pieces that *don't* need a MergeProposal, since those are where the
//! recent bugs were:
//!
//! * `state::get_merge_proposal_run` / `get_last_effective_run`
//!   (both queried previously-broken `r#"""..."""#` SQL strings).
//! * `state::guess_proposal_info_from_revision` (newly ported).
//! * `derived_branch_name` (newly ported, uses `state::has_cotenants`).
//!
//! All tests load the production schema via
//! `janitor::schema::setup_test_database`, so the columns and types
//! are exactly what `check_existing_mp` will see in production.

use chrono::{Duration, Utc};
use janitor::{
    schema::setup_test_database,
    test_utils::{TestDatabase, TestDatabaseConfig},
    test_with_database,
};
use janitor_publish::state::{
    get_last_effective_run, get_merge_proposal_run, get_run, guess_proposal_info_from_revision,
};
use janitor_publish::{derived_branch_name, get_publish_policy};
use sqlx::PgPool;

/// Insert the upstream FK rows (`codebase` + `change_set`) needed by
/// any `run` insert.
async fn seed_codebase_and_change_set(
    pool: &PgPool,
    codebase: &str,
    change_set_id: &str,
    campaign: &str,
    branch_url: &str,
) {
    // `codebase.check1` requires `(branch_url is null) = (url is null)`
    // -- set both to the same value or neither.
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ($1, $2, $2, 'git')
         ON CONFLICT DO NOTHING",
    )
    .bind(codebase)
    .bind(branch_url)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO change_set (id, campaign)
         VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(change_set_id)
    .bind(campaign)
    .execute(pool)
    .await
    .unwrap();
}

/// Insert a minimal successful run row with the exact columns used by
/// the queries under test. `finish_time_offset_minutes` is subtracted
/// from `now()` so callers can order multiple rows.
#[allow(clippy::too_many_arguments)]
async fn seed_run(
    pool: &PgPool,
    run_id: &str,
    codebase: &str,
    campaign: &str,
    command: &str,
    branch_url: &str,
    revision: Option<&str>,
    value: i32,
    change_set_id: &str,
    finish_time_offset_minutes: i64,
) {
    let finish_time = Utc::now() - Duration::minutes(finish_time_offset_minutes);
    let start_time = finish_time - Duration::minutes(1);
    // `vcs_type` must be non-null: the Rust `Run` struct decodes it
    // as `String`, not `Option<String>`.
    sqlx::query(
        r#"
        INSERT INTO run (
            id, suite, codebase, command, result_code, revision,
            branch_url, value, start_time, finish_time, logfilenames,
            change_set, vcs_type
        ) VALUES ($1, $2, $3, $4, 'success', $5, $6, $7, $8, $9, '{}', $10, 'git')
        "#,
    )
    .bind(run_id)
    .bind(campaign)
    .bind(codebase)
    .bind(command)
    .bind(revision)
    .bind(branch_url)
    .bind(value)
    .bind(start_time)
    .bind(finish_time)
    .bind(change_set_id)
    .execute(pool)
    .await
    .unwrap();
}

/// Insert a `new_result_branch` row.
async fn seed_result_branch(
    pool: &PgPool,
    run_id: &str,
    role: &str,
    remote_name: &str,
    base_revision: Option<&str>,
    revision: Option<&str>,
) {
    sqlx::query(
        r#"
        INSERT INTO new_result_branch (
            run_id, role, remote_name, base_revision, revision
        ) VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(run_id)
    .bind(role)
    .bind(remote_name)
    .bind(base_revision)
    .bind(revision)
    .execute(pool)
    .await
    .unwrap();
}

/// Insert a `merge_proposal` row pointing at the given source revision,
/// so `get_merge_proposal_run` can locate the run by URL.
async fn seed_merge_proposal(
    pool: &PgPool,
    url: &str,
    revision: &str,
    target_branch_url: &str,
    codebase: &str,
) {
    sqlx::query(
        r#"
        INSERT INTO merge_proposal (
            url, status, revision, target_branch_url, codebase, last_scanned
        ) VALUES ($1, 'open', $2, $3, $4, NOW())
        "#,
    )
    .bind(url)
    .bind(revision)
    .bind(target_branch_url)
    .bind(codebase)
    .execute(pool)
    .await
    .unwrap();
}

/// Insert a `branch_publish_policy` row + a `named_publish_policy`
/// referencing it, then a `candidate` referencing the named policy.
/// Used by `guess_proposal_info_from_revision` so that revision ->
/// codebase + bucket lookups succeed.
async fn seed_publish_policy_and_candidate(
    pool: &PgPool,
    policy_name: &str,
    bucket: &str,
    role: &str,
    codebase: &str,
    campaign: &str,
    command: &str,
) {
    sqlx::query(
        "INSERT INTO branch_publish_policy (role, mode)
         VALUES ($1, 'propose')
         ON CONFLICT DO NOTHING",
    )
    .bind(role)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO named_publish_policy (name, per_branch_policy, rate_limit_bucket)
         VALUES ($1, ARRAY[ROW($2, 'propose'::publish_mode, NULL::int)::branch_publish_policy], $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(policy_name)
    .bind(role)
    .bind(bucket)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO candidate (suite, value, command, publish_policy, codebase)
         VALUES ($1, 100, $2, $3, $4)
         ON CONFLICT DO NOTHING",
    )
    .bind(campaign)
    .bind(command)
    .bind(policy_name)
    .bind(codebase)
    .execute(pool)
    .await
    .unwrap();
}

test_with_database! {
    async fn test_get_merge_proposal_run_returns_matching_row(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        seed_codebase_and_change_set(
            test_db.pool(),
            "cb-1",
            "cs-1",
            "lintian-fixes",
            "https://example.invalid/cb-1",
        )
        .await;
        seed_run(
            test_db.pool(),
            "run-1",
            "cb-1",
            "lintian-fixes",
            "lintian-brush",
            "https://example.invalid/cb-1",
            Some("rev-tip"),
            10,
            "cs-1",
            5,
        )
        .await;
        seed_result_branch(
            test_db.pool(),
            "run-1",
            "main",
            "refs/heads/main",
            Some("rev-base"),
            Some("rev-tip"),
        )
        .await;
        seed_merge_proposal(
            test_db.pool(),
            "https://example.invalid/mp/1",
            "rev-tip",
            "https://example.invalid/cb-1",
            "cb-1",
        )
        .await;

        let url = "https://example.invalid/mp/1".parse().unwrap();
        let mp_run = get_merge_proposal_run(test_db.pool(), &url).await.unwrap();
        let mp_run = mp_run.expect("get_merge_proposal_run returned None");
        assert_eq!(mp_run.id, "run-1");
        assert_eq!(mp_run.campaign, "lintian-fixes");
        assert_eq!(mp_run.codebase, "cb-1");
        assert_eq!(mp_run.role, "main");
        assert_eq!(mp_run.remote_branch_name, "refs/heads/main");
        assert_eq!(mp_run.command, "lintian-brush");
        assert_eq!(mp_run.value, 10);
    }
}

test_with_database! {
    async fn test_get_last_effective_run_picks_most_recent_success(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        seed_codebase_and_change_set(
            test_db.pool(),
            "cb-1",
            "cs-1",
            "lintian-fixes",
            "https://example.invalid/cb-1",
        )
        .await;
        // Older run.
        seed_run(
            test_db.pool(),
            "run-old",
            "cb-1",
            "lintian-fixes",
            "lintian-brush",
            "https://example.invalid/cb-1",
            Some("rev-old"),
            10,
            "cs-1",
            120,
        )
        .await;
        // Older run also gets a new_result_branch row so the
        // last_effective_runs view considers it effective.
        seed_result_branch(
            test_db.pool(),
            "run-old",
            "main",
            "refs/heads/main",
            Some("rev-base"),
            Some("rev-old"),
        )
        .await;
        // Newer successful run.
        seed_codebase_and_change_set(
            test_db.pool(),
            "cb-1",
            "cs-2",
            "lintian-fixes",
            "https://example.invalid/cb-1",
        )
        .await;
        seed_run(
            test_db.pool(),
            "run-new",
            "cb-1",
            "lintian-fixes",
            "lintian-brush",
            "https://example.invalid/cb-1",
            Some("rev-new"),
            12,
            "cs-2",
            5,
        )
        .await;
        seed_result_branch(
            test_db.pool(),
            "run-new",
            "main",
            "refs/heads/main",
            Some("rev-base"),
            Some("rev-new"),
        )
        .await;

        let last = get_last_effective_run(test_db.pool(), "cb-1", "lintian-fixes")
            .await
            .unwrap()
            .expect("get_last_effective_run returned None");
        assert_eq!(last.id, "run-new");
        assert_eq!(last.suite, "lintian-fixes");
        assert_eq!(last.codebase, "cb-1");
        assert_eq!(
            last.revision.as_ref().map(|r| r.to_string()),
            Some("rev-new".to_string())
        );
    }
}

test_with_database! {
    async fn test_guess_proposal_info_from_revision_finds_unique_match(
        test_db: TestDatabase
    ) {
        setup_test_database(test_db.pool()).await.unwrap();

        seed_codebase_and_change_set(
            test_db.pool(),
            "cb-1",
            "cs-1",
            "lintian-fixes",
            "https://example.invalid/cb-1",
        )
        .await;
        seed_publish_policy_and_candidate(
            test_db.pool(),
            "policy-1",
            "low-bucket",
            "main",
            "cb-1",
            "lintian-fixes",
            "lintian-brush",
        )
        .await;
        seed_run(
            test_db.pool(),
            "run-1",
            "cb-1",
            "lintian-fixes",
            "lintian-brush",
            "https://example.invalid/cb-1",
            Some("rev-tip"),
            10,
            "cs-1",
            5,
        )
        .await;
        seed_result_branch(
            test_db.pool(),
            "run-1",
            "main",
            "refs/heads/main",
            Some("rev-base"),
            Some("rev-tip"),
        )
        .await;

        let revision = breezyshim::RevisionId::from(b"rev-tip".to_vec());
        let (codebase, bucket) =
            guess_proposal_info_from_revision(test_db.pool(), &revision)
                .await
                .unwrap();
        assert_eq!(codebase.as_deref(), Some("cb-1"));
        assert_eq!(bucket.as_deref(), Some("low-bucket"));
    }
}

test_with_database! {
    async fn test_derived_branch_name_with_single_result_branch(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        seed_codebase_and_change_set(
            test_db.pool(),
            "cb-1",
            "cs-1",
            "lintian-fixes",
            "https://example.invalid/cb-1",
        )
        .await;
        seed_run(
            test_db.pool(),
            "run-1",
            "cb-1",
            "lintian-fixes",
            "lintian-brush",
            "https://example.invalid/cb-1",
            Some("rev-tip"),
            10,
            "cs-1",
            5,
        )
        .await;

        // Seed exactly one result branch so `derived_branch_name` sees
        // `len(result_branches) == 1` and takes the single-branch arm
        // (Python publish.py:224: `if len(run.result_branches) == 1`).
        // Without this the array is empty and the helper falls through
        // to the `{branch}/{role}` arm.
        seed_result_branch(
            test_db.pool(),
            "run-1",
            "main",
            "refs/heads/main",
            Some("rev-base"),
            Some("rev-tip"),
        )
        .await;
        let last = get_last_effective_run(test_db.pool(), "cb-1", "lintian-fixes")
            .await
            .unwrap()
            .expect("get_last_effective_run returned None");

        let campaign_text =
            r#"name: "lintian-fixes" branch_name: "lintian-fixes" command: "lintian-brush""#;
        let campaign: janitor::config::Campaign =
            protobuf::text_format::parse_from_str(campaign_text).unwrap();

        let branch_name = derived_branch_name(test_db.pool(), &campaign, &last, "main")
            .await
            .unwrap();
        // Single result branch + no cotenants -> use the campaign
        // branch name verbatim.
        assert_eq!(branch_name, "lintian-fixes");
    }
}

test_with_database! {
    async fn test_derived_branch_name_with_multiple_result_branches(
        test_db: TestDatabase
    ) {
        setup_test_database(test_db.pool()).await.unwrap();

        seed_codebase_and_change_set(
            test_db.pool(),
            "cb-1",
            "cs-1",
            "fresh-releases",
            "https://example.invalid/cb-1",
        )
        .await;
        seed_run(
            test_db.pool(),
            "run-1",
            "cb-1",
            "fresh-releases",
            "deb-new-upstream",
            "https://example.invalid/cb-1",
            Some("rev-tip"),
            10,
            "cs-1",
            5,
        )
        .await;
        // Two result branches: main and pristine-tar.
        seed_result_branch(
            test_db.pool(),
            "run-1",
            "main",
            "refs/heads/main",
            Some("rev-base"),
            Some("rev-tip"),
        )
        .await;
        seed_result_branch(
            test_db.pool(),
            "run-1",
            "pristine-tar",
            "refs/heads/pristine-tar",
            Some("ptar-base"),
            Some("ptar-tip"),
        )
        .await;

        let last = get_last_effective_run(test_db.pool(), "cb-1", "fresh-releases")
            .await
            .unwrap()
            .expect("get_last_effective_run returned None");

        let campaign_text =
            r#"name: "fresh-releases" branch_name: "fresh-releases" command: "deb-new-upstream""#;
        let campaign: janitor::config::Campaign =
            protobuf::text_format::parse_from_str(campaign_text).unwrap();

        let main_name = derived_branch_name(test_db.pool(), &campaign, &last, "main")
            .await
            .unwrap();
        let ptar_name =
            derived_branch_name(test_db.pool(), &campaign, &last, "pristine-tar")
                .await
                .unwrap();
        // Multi-result-branch runs append /<role> to disambiguate.
        assert_eq!(main_name, "fresh-releases/main");
        assert_eq!(ptar_name, "fresh-releases/pristine-tar");
    }
}

test_with_database! {
    async fn test_get_run_returns_row_with_result_branches(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        seed_codebase_and_change_set(
            test_db.pool(),
            "cb-1",
            "cs-1",
            "lintian-fixes",
            "https://example.invalid/cb-1",
        )
        .await;
        seed_run(
            test_db.pool(),
            "run-1",
            "cb-1",
            "lintian-fixes",
            "lintian-brush",
            "https://example.invalid/cb-1",
            Some("rev-tip"),
            42,
            "cs-1",
            5,
        )
        .await;
        seed_result_branch(
            test_db.pool(),
            "run-1",
            "main",
            "refs/heads/main",
            Some("rev-base"),
            Some("rev-tip"),
        )
        .await;

        let run = get_run(test_db.pool(), "run-1")
            .await
            .unwrap()
            .expect("get_run returned None for existing run");
        assert_eq!(run.id, "run-1");
        assert_eq!(run.codebase, "cb-1");
        assert_eq!(run.suite, "lintian-fixes");
        assert_eq!(run.command, "lintian-brush");
        assert_eq!(run.result_code, "success");
        assert_eq!(run.value, Some(42));
        let branches = run.result_branches.expect("result_branches missing");
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].0, "main");
        assert_eq!(branches[0].1, "refs/heads/main");
    }
}

test_with_database! {
    async fn test_get_run_returns_none_for_missing_id(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let run = get_run(test_db.pool(), "does-not-exist").await.unwrap();
        assert!(run.is_none());
    }
}

test_with_database! {
    async fn test_get_publish_policy_decodes_composite_array(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        seed_codebase_and_change_set(
            test_db.pool(),
            "cb-1",
            "cs-1",
            "lintian-fixes",
            "https://example.invalid/cb-1",
        )
        .await;

        // Two per-branch roles with different modes and frequencies,
        // stored as a real composite array.
        sqlx::query(
            "INSERT INTO branch_publish_policy (role, mode)
             VALUES ('main', 'propose'), ('pristine-tar', 'push')
             ON CONFLICT DO NOTHING",
        )
        .execute(test_db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO named_publish_policy (name, per_branch_policy, rate_limit_bucket)
             VALUES (
                'test-policy',
                ARRAY[
                    ROW('main', 'propose'::publish_mode, 7::int)::branch_publish_policy,
                    ROW('pristine-tar', 'push'::publish_mode, NULL::int)::branch_publish_policy
                ],
                'test-bucket'
             )",
        )
        .execute(test_db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO candidate (suite, value, command, publish_policy, codebase)
             VALUES ('lintian-fixes', 100, 'lintian-brush', 'test-policy', 'cb-1')",
        )
        .execute(test_db.pool())
        .await
        .unwrap();

        let policy = get_publish_policy(test_db.pool(), "cb-1", "lintian-fixes")
            .await
            .unwrap()
            .expect("get_publish_policy returned None");
        let (map, command, bucket) = policy;
        assert_eq!(command.as_deref(), Some("lintian-brush"));
        assert_eq!(bucket.as_deref(), Some("test-bucket"));
        assert_eq!(map.len(), 2);
        let main = map.get("main").expect("main role missing");
        assert_eq!(main.0, "propose");
        assert_eq!(main.1, Some(7));
        let ptar = map.get("pristine-tar").expect("pristine-tar role missing");
        assert_eq!(ptar.0, "push");
        assert_eq!(ptar.1, None);
    }
}

test_with_database! {
    async fn test_get_publish_policy_returns_none_for_missing_candidate(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let policy = get_publish_policy(test_db.pool(), "nope", "lintian-fixes")
            .await
            .unwrap();
        assert!(policy.is_none());
    }
}
