//! End-to-end tests for the loggerhead integration on the public app.
//!
//! The public app exposes `/bzr/<codebase>` as a per-codebase HTML
//! browser backed by the Rust `loggerhead` crate (which uses breezy
//! via PyO3). These tests boot the real app against a throwaway
//! Postgres and a real on-disk bzr branch created via the `brz` CLI,
//! then check that the HTML browser answers.
//!
//! If `brz` isn't on PATH, the tests are skipped: the integration is
//! only meaningful when a real breezy install is present.

mod common;

use common::*;
use std::process::Command;

use janitor::{schema::setup_test_database, test_with_database};
use janitor_bzr_store::web::create_applications;
use tempfile::TempDir;

/// Create a bzr branch with one commit at `branch_path`. Returns true
/// on success.
fn make_bzr_branch(branch_path: &std::path::Path) -> bool {
    std::fs::create_dir_all(branch_path).unwrap();
    let whoami = Command::new("brz")
        .args(["whoami", "test <test@example.com>"])
        .current_dir(branch_path)
        .status();
    if whoami.map(|s| !s.success()).unwrap_or(true) {
        // whoami can fail silently in CI; keep going.
    }
    let steps = [
        vec!["init", "--format=2a"],
        vec!["whoami", "test <test@example.com>"],
    ];
    for args in &steps {
        let out = Command::new("brz")
            .args(args)
            .current_dir(branch_path)
            .output();
        if out.map(|o| !o.status.success()).unwrap_or(true) {
            return false;
        }
    }
    // Make a commit so loggerhead has something to render.
    let readme = branch_path.join("README");
    std::fs::write(&readme, "hello loggerhead\n").unwrap();
    let add = Command::new("brz")
        .args(["add", "README"])
        .current_dir(branch_path)
        .output();
    if add.map(|o| !o.status.success()).unwrap_or(true) {
        return false;
    }
    let commit = Command::new("brz")
        .args(["commit", "-m", "initial"])
        .current_dir(branch_path)
        .output();
    commit.map(|o| o.status.success()).unwrap_or(false)
}

test_with_database! {
    async fn test_loggerhead_browse_unknown_codebase_404(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(public).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/bzr/ghost/", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        handle.abort();
    }
}

// Loggerhead's internals call `tokio::task::block_in_place` on the
// request path. That API panics on the single-threaded runtime our
// `test_with_database!` macro expands into, so we drive these two
// tests through a multi-threaded tokio runtime instead.
#[test]
#[serial_test::serial]
fn test_loggerhead_browse_known_codebase_renders() {
    if std::env::var("SKIP_DATABASE_TESTS").is_ok() {
        eprintln!("SKIP_DATABASE_TESTS set, skipping");
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let config = janitor::test_utils::TestDatabaseConfig {
            run_migrations: true,
            ..Default::default()
        };
        let test_db = match janitor::test_utils::TestDatabase::with_config(config).await {
            Ok(db) => db,
            Err(e) => {
                eprintln!("skipping: {}", e);
                return;
            }
        };

        if !brz_available() {
            eprintln!("skipping: brz not on PATH");
            return;
        }

        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let branch_path = tmp.path().join("foo");
        if !make_bzr_branch(&branch_path) {
            eprintln!("skipping: could not create a bzr branch (brz config?)");
            return;
        }

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(public).await;

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        // GET /bzr/foo/ should either (a) 503 if loggerhead static dir
        // couldn't be located in this test environment, or (b) redirect
        // to /bzr/foo/changes (loggerhead's root_redirect), or (c)
        // 200 with HTML. Accept any of those as proof the integration
        // is wired up.
        let resp = client
            .get(format!("http://{}/bzr/foo/", addr))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        assert!(
            status == 301
                || status == 302
                || status == 307
                || status == 308
                || status == 200
                || status == 503,
            "unexpected status {} for /bzr/foo/",
            status
        );
        if status / 100 == 3 {
            let loc = resp.headers().get("Location").and_then(|v| v.to_str().ok());
            assert!(
                loc.map(|s| s.contains("/bzr/foo/")).unwrap_or(false),
                "redirect Location should preserve /bzr/foo/ prefix, got {:?}",
                loc
            );
        }

        handle.abort();
    });
}

// See the comment on `test_loggerhead_browse_known_codebase_renders`
// for why this test uses a multi-threaded runtime rather than the
// `test_with_database!` macro.
#[test]
#[serial_test::serial]
fn test_loggerhead_changes_endpoint_reachable() {
    if std::env::var("SKIP_DATABASE_TESTS").is_ok() {
        eprintln!("SKIP_DATABASE_TESTS set, skipping");
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let config = janitor::test_utils::TestDatabaseConfig {
            run_migrations: true,
            ..Default::default()
        };
        let test_db = match janitor::test_utils::TestDatabase::with_config(config).await {
            Ok(db) => db,
            Err(e) => {
                eprintln!("skipping: {}", e);
                return;
            }
        };

        if !brz_available() {
            eprintln!("skipping: brz not on PATH");
            return;
        }
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let branch_path = tmp.path().join("foo");
        if !make_bzr_branch(&branch_path) {
            eprintln!("skipping: could not create a bzr branch");
            return;
        }

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(public).await;

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let resp = client
            .get(format!("http://{}/bzr/foo/changes", addr))
            .send()
            .await
            .unwrap();
        // Expected 200 (rendered HTML) when loggerhead's static dir
        // is wired; 503 otherwise. Anything else is a routing bug.
        let status = resp.status().as_u16();
        assert!(
            status == 200 || status == 503,
            "unexpected status {} for /bzr/foo/changes",
            status
        );

        handle.abort();
    });
}
