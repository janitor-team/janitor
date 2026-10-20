//! Integration tests that need a real bzr branch on disk.
//!
//! These are the end-to-end checks for the admin diff /
//! revision-info / set-remote endpoints. They build a bzr branch
//! with a couple of commits via breezyshim and then drive the
//! admin app via HTTP.

mod common;

use breezyshim::branch::Branch;
use breezyshim::tree::{MutableTree, WorkingTree};
use common::*;
use janitor::test_with_database;
use janitor_bzr_store::web::create_applications;
use tempfile::TempDir;

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
        let (addr, handle) = spawn(admin).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!(
                "http://{}/foo/diff?old={}&new={}",
                addr, rev1, rev2
            ))
            .send()
            .await
            .unwrap();
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
        let text = resp.text().await.unwrap();
        // A real bzr diff should mention README on one side.
        assert!(
            text.contains("README"),
            "diff body missing README: {}",
            text
        );

        handle.abort();
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
        let (addr, handle) = spawn(admin).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!(
                "http://{}/foo/revision-info?old={}&new={}",
                addr, rev1, rev2
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        // The ancestry walk between rev1 and rev2 includes exactly
        // one revision (rev2 itself; rev1 is the stop node).
        let revs = body.as_array().expect("body must be a list");
        assert_eq!(revs.len(), 1, "expected one revision between r1 and r2, got {:?}", revs);
        assert_eq!(revs[0]["revision-id"], rev2);
        assert_eq!(revs[0]["message"], "second");

        handle.abort();
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
        let (addr, handle) = spawn(admin).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{}/foo/remotes/main", addr))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("url=bzr%2Bssh%3A%2F%2Fupstream.invalid%2Ffoo")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
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

        let resp = client
            .get(format!("http://{}/foo/remotes", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "repository": "foo",
                "remotes": [{"name": "main", "url": "bzr+ssh://upstream.invalid/foo"}],
            })
        );

        handle.abort();
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
        let (addr, handle) = spawn(admin).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/foo/diff", addr))
            .send()
            .await
            .unwrap();
        // Deserialize failure -> 400 (axum default).
        assert!(resp.status().is_client_error(), "status={}", resp.status());

        handle.abort();
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
        let (addr, handle) = spawn(admin).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/foo/info", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["path"]["codebase"], "foo");
        assert_eq!(body["exists"], true);
        assert_eq!(body["branch_count"], 1);
        assert_eq!(body["last_revision"], rev2);

        handle.abort();
    }
}
