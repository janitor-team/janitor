//! Tests for `RunnerDatabase` queries against a real Postgres schema.

use janitor::schema::setup_debian_test_database;
use janitor::test_utils::TestDatabase;
use janitor_runner::database::{CodebaseUpload, DebianBuildRow, RunnerDatabase};
use serde_json::json;

async fn setup() -> Option<TestDatabase> {
    let Ok(Some(db)) = TestDatabase::new_optional().await else {
        eprintln!("Skipping: no Postgres available");
        return None;
    };
    setup_debian_test_database(&db.pool)
        .await
        .expect("schema setup failed");
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

    let codebases = serde_json::to_value(runner_db.get_codebases().await.unwrap()).unwrap();
    assert_eq!(
        codebases,
        json!([{
            "name": "foo",
            "branch_url": "https://example.com/foo",
            "url": "https://example.com/foo",
            "branch": null,
            "subpath": "",
            "vcs_type": "git",
            "web_url": null,
            "vcs_last_revision": null,
            "value": 10,
        }])
    );
}

#[tokio::test]
async fn test_upload_codebases() {
    let Some(db) = setup().await else { return };
    let runner_db = RunnerDatabase::new(db.pool.clone());
    let upload = |entries| serde_json::from_value::<Vec<CodebaseUpload>>(entries).unwrap();

    runner_db
        .upload_codebases(&upload(json!([
            {"name": "bar", "branch_url": "https://example.com/bar,branch=debian%2Fmain"},
            {"name": "baz", "url": "https://example.com/baz", "branch": "debian/main"},
        ])))
        .await
        .unwrap();
    let err = runner_db
        .upload_codebases(&upload(json!([
            {"name": "qux", "url": "https://example.com/qux"},
            {"name": "quux", "branch_url": "/srv/git/quux"},
        ])))
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        r#"Invalid branch_url "/srv/git/quux": relative URL without a base"#
    );

    let mut rows = runner_db.get_codebases().await.unwrap();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        serde_json::to_value(rows).unwrap(),
        json!([
            {
                "name": "bar",
                "branch_url": "https://example.com/bar,branch=debian%2Fmain",
                "url": "https://example.com/bar",
                "branch": "debian/main",
                "subpath": "",
                "vcs_type": null,
                "web_url": null,
                "vcs_last_revision": null,
                "value": null,
            },
            {
                "name": "baz",
                "branch_url": "https://example.com/baz,branch=debian%2Fmain",
                "url": "https://example.com/baz",
                "branch": "debian/main",
                "subpath": "",
                "vcs_type": null,
                "web_url": null,
                "vcs_last_revision": null,
                "value": null,
            },
            {
                "name": "foo",
                "branch_url": "https://example.com/foo",
                "url": "https://example.com/foo",
                "branch": null,
                "subpath": "",
                "vcs_type": "git",
                "web_url": null,
                "vcs_last_revision": null,
                "value": 10,
            },
        ])
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

    let candidates = serde_json::to_value(runner_db.get_candidates().await.unwrap()).unwrap();
    assert_eq!(
        candidates,
        json!([{
            "id": 1,
            "codebase": "foo",
            "campaign": "lintian-fixes",
            "command": "lintian-brush",
            "publish-policy": null,
            "change_set": null,
            "context": "some context",
            "value": 5,
            "success_chance": 0.5,
        }])
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
        serde_json::to_value(&builder_result).unwrap(),
        json!({
            "kind": "debian",
            "source": "foo",
            "build_version": "1.0-1",
            "build_distribution": "unstable",
            "changes_filenames": null,
            "lintian_result": null,
            "binary_packages": ["foo"],
        })
    );
    let target = run.target.expect("debian_build row exists");
    assert_eq!(target.name, "debian");
    assert_eq!(
        target.details,
        json!({
            "source": "foo",
            "build_version": "1.0-1",
            "build_distribution": "unstable",
            "changes_filenames": null,
            "lintian": null,
            "binary_packages": ["foo"],
        })
    );
}
