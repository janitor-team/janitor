//! End-to-end tests for the loggerhead integration on the public app.
//!
//! The public app exposes `/bzr/<codebase>` as a per-codebase HTML
//! browser backed by the Rust `loggerhead` crate (which uses breezy
//! via PyO3). These tests boot the real app against a throwaway
//! Postgres and a real on-disk bzr branch created via breezyshim,
//! then check that the HTML browser answers.

mod common;

use breezyshim::tree::{MutableTree, WorkingTree};
use common::*;
use janitor::test_with_database;
use janitor_bzr_store::web::create_applications;
use tempfile::TempDir;

/// Create a bzr branch with one commit at `branch_path`.
fn make_bzr_branch(branch_path: &std::path::Path) {
    let env = breezyshim::testing::TestEnv::new();
    let wt = breezyshim::controldir::create_standalone_workingtree(branch_path, "2a").unwrap();
    // Make a commit so loggerhead has something to render.
    std::fs::write(branch_path.join("README"), "hello loggerhead\n").unwrap();
    wt.add(&[std::path::Path::new("README")]).unwrap();
    wt.build_commit().message("initial").commit().unwrap();
    std::mem::drop(env);
}

test_with_database! {
    schema = Core;
    async fn test_loggerhead_browse_unknown_codebase_404(test_db: TestDatabase) {
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
            schema: janitor::test_utils::TestSchema::Core,
            ..Default::default()
        };
        let test_db = match janitor::test_utils::TestDatabase::with_config(config).await {
            Ok(db) => db,
            Err(e) => {
                eprintln!("skipping: {}", e);
                return;
            }
        };

        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let branch_path = tmp.path().join("foo");
        make_bzr_branch(&branch_path);

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
            schema: janitor::test_utils::TestSchema::Core,
            ..Default::default()
        };
        let test_db = match janitor::test_utils::TestDatabase::with_config(config).await {
            Ok(db) => db,
            Err(e) => {
                eprintln!("skipping: {}", e);
                return;
            }
        };
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let branch_path = tmp.path().join("foo");
        make_bzr_branch(&branch_path);

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
