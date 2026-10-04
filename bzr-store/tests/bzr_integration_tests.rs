//! Integration tests that need a real bzr branch on disk.
//!
//! These are the end-to-end checks for the admin diff /
//! revision-info / set-remote endpoints. They build a bzr branch
//! with a couple of commits via the `brz` CLI and then drive the
//! admin app via HTTP.
//!
//! Each test skips when `brz init` doesn't work on the host (same
//! policy as the loggerhead tests).

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use janitor::{schema::setup_test_database, test_utils::TestDatabase, test_with_database};
use janitor_bzr_store::{
    config::{BzrConfig, BzrStoreConfig, Config},
    web::create_applications,
};
use sqlx::PgPool;
use tempfile::TempDir;

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

async fn spawn(app: Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, handle)
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

/// Same brz-end-to-end sentinel as `loggerhead_tests.rs`: skip the
/// test when brz can't actually create a branch (either not on PATH,
/// or installed-but-broken, e.g. missing vcsgraph).
fn brz_working() -> bool {
    let Ok(tmp) = tempfile::tempdir() else {
        return false;
    };
    Command::new("brz")
        .args(["init", "--format=2a"])
        .arg(tmp.path())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build a bzr branch with two commits at `branch_path`. Returns
/// `Some((rev1, rev2))` on success; `None` when any brz invocation
/// fails. The revids are the `bzr revno-info`-style revids, pulled
/// out of `brz log`.
fn build_two_commit_branch(branch_path: &std::path::Path) -> Option<(String, String)> {
    std::fs::create_dir_all(branch_path).ok()?;
    let run = |args: &[&str]| -> Option<()> {
        Command::new("brz")
            .args(args)
            .current_dir(branch_path)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| ())
    };
    // Branch init + author (whoami).
    run(&["init", "--format=2a"])?;
    run(&["whoami", "test <test@example.com>"])?;

    // First commit.
    std::fs::write(branch_path.join("README"), "r1\n").ok()?;
    run(&["add", "README"])?;
    run(&["commit", "-m", "first"])?;

    // Second commit.
    std::fs::write(branch_path.join("README"), "r2\n").ok()?;
    run(&["commit", "-m", "second"])?;

    // Extract revids via `brz log --show-ids --forward`.
    let output = Command::new("brz")
        .args(["log", "--show-ids", "--forward"])
        .current_dir(branch_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut revids: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(revid) = line.strip_prefix("revision-id:") {
            revids.push(revid.trim().to_string());
        }
    }
    if revids.len() < 2 {
        return None;
    }
    Some((revids[0].clone(), revids[1].clone()))
}

test_with_database! {
    async fn test_admin_diff_two_revisions(test_db: TestDatabase) {
        if !brz_working() {
            eprintln!("skipping: brz not working on this host");
            return;
        }
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let branch_path = tmp.path().join("foo");
        let Some((rev1, rev2)) = build_two_commit_branch(&branch_path) else {
            eprintln!("skipping: could not build a two-commit bzr branch");
            return;
        };

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
    async fn test_admin_revision_info_two_revisions(test_db: TestDatabase) {
        if !brz_working() {
            eprintln!("skipping: brz not working on this host");
            return;
        }
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let branch_path = tmp.path().join("foo");
        let Some((rev1, rev2)) = build_two_commit_branch(&branch_path) else {
            eprintln!("skipping: could not build a two-commit bzr branch");
            return;
        };

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
        let revs = body["revisions"]
            .as_array()
            .expect("revisions must be an array");
        assert_eq!(revs.len(), 1, "expected one revision between r1 and r2, got {:?}", revs);
        assert_eq!(revs[0]["revision_id"], rev2);
        assert_eq!(revs[0]["message"], "second");

        handle.abort();
    }
}

test_with_database! {
    async fn test_admin_set_remote_persists_parent_location(test_db: TestDatabase) {
        // `handle_set_bzr_remote` writes `parent_location` on the
        // branch at <codebase>/<remote>. Verify the value sticks by
        // asking brz for it afterwards.
        if !brz_working() {
            eprintln!("skipping: brz not working on this host");
            return;
        }
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        // The handler operates on <codebase>/<remote> as a branch,
        // not the shared repo. Make `main` a real branch directly.
        let branch_path = tmp.path().join("foo").join("main");
        let Some(_) = build_two_commit_branch(&branch_path) else {
            eprintln!("skipping: could not build the branch");
            return;
        };

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

        // Verify brz now reports the parent_location.
        let out = Command::new("brz")
            .args(["config", "parent_location"])
            .current_dir(&branch_path)
            .output()
            .unwrap();
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(value, "bzr+ssh://upstream.invalid/foo");

        handle.abort();
    }
}

test_with_database! {
    async fn test_admin_diff_missing_params_rejected(test_db: TestDatabase) {
        // old= and new= are required query params. axum's Query
        // extractor rejects the request before our handler sees it.
        setup_test_database(test_db.pool()).await.unwrap();

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
    async fn test_admin_repo_info_for_real_branch(test_db: TestDatabase) {
        // When the repo exists on disk, repository_info reports
        // exists=true and (if PyO3 is working) a last_revision.
        if !brz_working() {
            eprintln!("skipping: brz not working on this host");
            return;
        }
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let branch_path = tmp.path().join("foo");
        if build_two_commit_branch(&branch_path).is_none() {
            eprintln!("skipping: brz branch build failed");
            return;
        }

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
        // last_revision is best-effort: present on hosts where
        // breezyshim can open the branch, absent otherwise.

        handle.abort();
    }
}
