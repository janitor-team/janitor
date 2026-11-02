//! Tests for `RunnerDatabase` queries against a real Postgres schema.

use janitor::test_utils::{TestDatabase, TestSchema};
use janitor_runner::database::{CandidateRow, CodebaseRow, DebianBuildRow, RunnerDatabase};
use janitor_runner::{BuilderResult, DebianTargetDetails};

async fn setup() -> Option<TestDatabase> {
    let Ok(Some(db)) = TestDatabase::new_optional_with_schema(TestSchema::Debian).await else {
        eprintln!("Skipping: no Postgres available");
        return None;
    };
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, subpath, vcs_type, value) \
         VALUES ('foo', 'https://example.com/foo', 'https://example.com/foo', '', 'git', 10)",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    Some(db)
}

#[tokio::test]
async fn test_get_codebases() {
    let Some(db) = setup().await else { return };
    let runner_db = RunnerDatabase::new(db.pool.clone());

    assert_eq!(
        runner_db.get_codebases().await.unwrap(),
        vec![CodebaseRow {
            name: Some(janitor::CodebaseName::new("foo").unwrap()),
            branch_url: Some("https://example.com/foo".to_string()),
            url: Some("https://example.com/foo".to_string()),
            branch: None,
            subpath: Some("".to_string()),
            vcs_type: Some("git".to_string()),
            web_url: None,
            vcs_last_revision: None,
            value: Some(10),
        }]
    );
}

#[tokio::test]
async fn test_get_candidates() {
    let Some(db) = setup().await else { return };
    sqlx::query(
        "INSERT INTO candidate (id, suite, command, codebase, value, context, success_chance) \
         VALUES (1, 'lintian-fixes', 'lintian-brush', 'foo', 5, 'some context', 0.5)",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let runner_db = RunnerDatabase::new(db.pool.clone());

    assert_eq!(
        runner_db.get_candidates().await.unwrap(),
        vec![CandidateRow {
            id: 1,
            codebase: janitor::CodebaseName::new("foo").unwrap(),
            campaign: janitor::Campaign::new("lintian-fixes").unwrap(),
            command: "lintian-brush".to_string(),
            publish_policy: None,
            change_set: None,
            context: Some("some context".to_string()),
            value: Some(5),
            success_chance: Some(0.5),
        }]
    );
}

#[tokio::test]
async fn test_get_run_debian_build() {
    let Some(db) = setup().await else { return };
    sqlx::raw_sql(
        "INSERT INTO change_set (id, campaign) VALUES ('cs', 'lintian-fixes');
         INSERT INTO run (id, command, result_code, suite, logfilenames, change_set, codebase, \
                          vcs_type, branch_url, start_time, finish_time) \
         VALUES ('run-1', 'lintian-brush', 'success', 'lintian-fixes', '{}', 'cs', 'foo', \
                 'git', 'https://example.com/foo', '2024-01-01 00:00:00', '2024-01-01 00:10:00');
         INSERT INTO debian_build (run_id, version, distribution, source, binary_packages) \
         VALUES ('run-1', '1.0-1', 'unstable', 'foo', '{foo}')",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let runner_db = RunnerDatabase::new(db.pool.clone());

    assert_eq!(
        runner_db
            .get_run_debian_build(&janitor::RunId::new("run-1").unwrap())
            .await
            .unwrap(),
        Some(DebianBuildRow {
            source: "foo".to_string(),
            version: "1.0-1".to_string(),
            distribution: "unstable".to_string(),
            binary_packages: Some(vec!["foo".to_string()]),
            lintian_result: None,
        })
    );
    assert_eq!(
        runner_db
            .get_run_debian_build(&janitor::RunId::new("run-2").unwrap())
            .await
            .unwrap(),
        None
    );

    let run = runner_db
        .get_run(&janitor::RunId::new("run-1").unwrap())
        .await
        .unwrap()
        .expect("run was just inserted");
    assert!(run.remotes.is_none());
    let builder_result = run.builder_result.expect("debian_build row exists");
    assert_eq!(
        builder_result,
        BuilderResult::Debian {
            source: Some("foo".to_string()),
            build_version: Some("1.0-1".to_string()),
            build_distribution: Some("unstable".to_string()),
            changes_filenames: None,
            lintian_result: None,
            binary_packages: Some(vec!["foo".to_string()]),
        }
    );
    let target = run.target.expect("debian_build row exists");
    assert_eq!(target.name, "debian");
    assert_eq!(
        serde_json::from_value::<DebianTargetDetails>(target.details).unwrap(),
        DebianTargetDetails {
            source: Some("foo".to_string()),
            build_version: Some("1.0-1".to_string()),
            build_distribution: Some("unstable".to_string()),
            changes_filenames: None,
            binary_packages: Some(vec!["foo".to_string()]),
            lintian: None,
        }
    );
}
