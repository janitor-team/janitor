//! Integration tests that need a real bzr branch on disk.
//!
//! These are the end-to-end checks for the admin diff /
//! revision-info / set-remote endpoints. They build a bzr branch
//! with a couple of commits via breezyshim and then drive the
//! admin app via HTTP.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::Request;
use axum::response::Response;
use axum::Router;
use breezyshim::branch::Branch;
use breezyshim::tree::{MutableTree, WorkingTree};
use janitor::{test_utils::TestDatabase, test_with_database};
use janitor_bzr_store::{
    config::{BzrConfig, BzrStoreConfig, Config},
    web::create_applications,
};
use sqlx::PgPool;
use tempfile::TempDir;
use tower::ServiceExt;

fn test_db_url(test_db: &TestDatabase) -> String {
    let base = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://localhost/postgres".to_string());
    match base.rsplit_once('/') {
        Some((prefix, _)) => format!("{}/{}", prefix, test_db.database_name),
        None => format!("{}/{}", base, test_db.database_name),
    }
}

fn build_config(tmp_dir: &std::path::Path, database_url: String) -> Config {
    let mut cfg = BzrStoreConfig::default();
    cfg.bzr = BzrConfig {
        repository_path: tmp_dir.to_path_buf(),
        admin_bind: "127.0.0.1:0".parse().unwrap(),
        public_bind: "127.0.0.1:0".parse().unwrap(),
        python_path: None,
        request_timeout: cfg.bzr.request_timeout,
    };
    cfg.base.database = Some(janitor::shared_config::DatabaseConfig {
        url: database_url,
        ..janitor::shared_config::DatabaseConfig::default()
    });
    cfg
}

fn janitor_config_with_campaign() -> Arc<janitor::config::Config> {
    let text = r#"
campaign {
    name: "campaign"
}
"#;
    Arc::new(janitor::config::read_string(text).expect("parse test janitor.conf"))
}

async fn send(app: &Router, req: Request<Body>) -> Response {
    app.clone().oneshot(req).await.unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

async fn body_bytes(resp: Response) -> Bytes {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
}

async fn body_text(resp: Response) -> String {
    String::from_utf8(body_bytes(resp).await.to_vec()).unwrap()
}

async fn body_json<T: serde::de::DeserializeOwned>(resp: Response) -> T {
    serde_json::from_slice(&body_bytes(resp).await).unwrap()
}

async fn seed_codebase(pool: &PgPool, name: &str) {
    let url = format!("https://example.invalid/{}", name);
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ($1, $2, $2, 'bzr')
         ON CONFLICT DO NOTHING",
    )
    .bind(name)
    .bind(&url)
    .execute(pool)
    .await
    .unwrap();
}

/// Build a bzr branch with two commits at `branch_path` and return
/// their revision ids.
fn build_two_commit_branch(branch_path: &std::path::Path) -> (String, String) {
    std::fs::create_dir_all(branch_path).unwrap();
    let env = breezyshim::testing::TestEnv::new();
    let wt = breezyshim::controldir::create_standalone_workingtree(branch_path, "2a").unwrap();
    std::fs::write(branch_path.join("README"), "r1\n").unwrap();
    wt.add(&[std::path::Path::new("README")]).unwrap();
    let rev1 = wt.build_commit().message("first").commit().unwrap();
    std::fs::write(branch_path.join("README"), "r2\n").unwrap();
    let rev2 = wt.build_commit().message("second").commit().unwrap();
    std::mem::drop(env);
    (rev1.as_str().to_string(), rev2.as_str().to_string())
}

test_with_database! {
    schema = Core;
    async fn test_admin_diff_two_revisions(test_db: TestDatabase) {
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let branch_path = tmp.path().join("foo");
        let (rev1, rev2) = build_two_commit_branch(&branch_path);

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&admin, get(&format!("/foo/diff?old={}&new={}", rev1, rev2))).await;
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            ct.starts_with("text/x-diff"),
            "expected text/x-diff content-type, got {}",
            ct
        );
        let text = body_text(resp).await;
        // A real bzr diff should mention README on one side.
        assert!(
            text.contains("README"),
            "diff body missing README: {}",
            text
        );
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_revision_info_two_revisions(test_db: TestDatabase) {
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let branch_path = tmp.path().join("foo");
        let (rev1, rev2) = build_two_commit_branch(&branch_path);

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(
            &admin,
            get(&format!("/foo/revision-info?old={}&new={}", rev1, rev2)),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = body_json(resp).await;
        // The ancestry walk between rev1 and rev2 includes exactly
        // one revision (rev2 itself; rev1 is the stop node).
        let revs = body.as_array().expect("body must be a list");
        assert_eq!(revs.len(), 1, "expected one revision between r1 and r2, got {:?}", revs);
        assert_eq!(revs[0]["revision-id"], rev2);
        assert_eq!(revs[0]["message"], "second");
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_set_remote_persists_parent_location(test_db: TestDatabase) {
        // `handle_set_bzr_remote` writes `parent_location` on the
        // branch at <codebase>/<remote>. Verify the value sticks by
        // reading it back afterwards.
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        // The handler operates on <codebase>/<remote> as a branch,
        // not the shared repo. Make `main` a real branch directly.
        let branch_path = tmp.path().join("foo").join("main");
        build_two_commit_branch(&branch_path);

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let req = Request::post("/foo/remotes/main")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(Body::from("url=bzr%2Bssh%3A%2F%2Fupstream.invalid%2Ffoo"))
            .unwrap();
        let resp = send(&admin, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = body_json(resp).await;
        assert_eq!(body["status"], "configured");
        assert_eq!(body["remote_url"], "bzr+ssh://upstream.invalid/foo");

        // TestEnv keeps the user's locations.conf out of the config stack.
        let env = breezyshim::testing::TestEnv::new();
        let branch = breezyshim::branch::open_as_generic(
            &url::Url::from_directory_path(&branch_path).unwrap(),
        )
        .unwrap();
        assert_eq!(
            branch.get_parent(),
            Some("bzr+ssh://upstream.invalid/foo".to_string())
        );
        std::mem::drop(env);

        let resp = send(&admin, get("/foo/remotes")).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = body_json(resp).await;
        assert_eq!(
            body,
            serde_json::json!({
                "repository": "foo",
                "remotes": [{"name": "main", "url": "bzr+ssh://upstream.invalid/foo"}],
            })
        );
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_diff_missing_params_rejected(test_db: TestDatabase) {
        // old= and new= are required query params. axum's Query
        // extractor rejects the request before our handler sees it.
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&admin, get("/foo/diff")).await;
        // Deserialize failure -> 400 (axum default).
        assert!(resp.status().is_client_error(), "status={}", resp.status());
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_repo_info_for_real_branch(test_db: TestDatabase) {
        // When the repo exists on disk, repository_info reports
        // exists=true and its last_revision.
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let branch_path = tmp.path().join("foo");
        let (_, rev2) = build_two_commit_branch(&branch_path);

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&admin, get("/foo/info")).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = body_json(resp).await;
        assert_eq!(body["path"]["codebase"], "foo");
        assert_eq!(body["exists"], true);
        assert_eq!(body["branch_count"], 1);
        assert_eq!(body["last_revision"], rev2);
    }
}
