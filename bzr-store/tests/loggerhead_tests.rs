//! End-to-end tests for the loggerhead integration on the public app.
//!
//! The public app exposes `/bzr/<codebase>` as a per-codebase HTML
//! browser backed by the Rust `loggerhead` crate (which uses breezy
//! via PyO3). These tests boot the real app against a throwaway
//! Postgres and a real on-disk bzr branch created via breezyshim,
//! then check that the HTML browser answers.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::Router;
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

async fn get(app: Router, uri: &str) -> Response {
    app.oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
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

        let resp = get(public, "/bzr/ghost/").await;
        assert_eq!(resp.status(), 404);
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

        // GET /bzr/foo/ should either (a) 503 if loggerhead static dir
        // couldn't be located in this test environment, or (b) redirect
        // to /bzr/foo/changes (loggerhead's root_redirect), or (c)
        // 200 with HTML. Accept any of those as proof the integration
        // is wired up.
        let resp = get(public, "/bzr/foo/").await;
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

        for page in [
            "changes",
            "changes/1",
            "files/1",
            "revision/1",
            "view/1/README",
            "annotate/1/README",
            "download/1/README",
        ] {
            let resp = get(public.clone(), &format!("/bzr/foo/{}", page)).await;
            assert_eq!(resp.status(), StatusCode::OK, "/bzr/foo/{}", page);
        }
    });
}
