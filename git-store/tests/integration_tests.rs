//! Integration tests exercising the admin and public apps end-to-end
//! against a throwaway Postgres database.
//!
//! The postgres backend is chosen at runtime:
//! * `TEST_DATABASE_URL` set -> connect to that URL (CI, local dev).
//! * Otherwise -> spin up a shared testcontainers postgres. Requires
//!   a working Docker daemon; tests skip cleanly if it can't start.
//! * `SKIP_DATABASE_TESTS=1` -> skip unconditionally.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use janitor::schema::setup_test_database;
use janitor_git_store::{
    config::Config,
    database::DatabaseManager,
    repository::RepositoryManager,
    web::{self, AppRole, AppState},
    web_utils::HealthChecker,
};
use sqlx::PgPool;
use tempfile::TempDir;
use testcontainers_modules::{
    postgres::Postgres as PostgresImage,
    testcontainers::{runners::AsyncRunner, ContainerAsync, ImageExt},
};
use tokio::sync::OnceCell;
use uuid::Uuid;

/// Backend that admin databases are created in. Either an external
/// URL from `TEST_DATABASE_URL` or a testcontainers-managed instance
/// (kept alive for the whole test run via `OnceCell`).
struct PgBackend {
    admin_url: String,
    // Kept alive for the duration of the test binary; dropped when
    // the process exits so the container gets torn down.
    _container: Option<ContainerAsync<PostgresImage>>,
}

static BACKEND: OnceCell<Option<PgBackend>> = OnceCell::const_new();

async fn backend() -> Option<&'static PgBackend> {
    BACKEND
        .get_or_init(|| async {
            if std::env::var("SKIP_DATABASE_TESTS").is_ok() {
                return None;
            }
            if let Ok(url) = std::env::var("TEST_DATABASE_URL") {
                return Some(PgBackend {
                    admin_url: url,
                    _container: None,
                });
            }
            let container = PostgresImage::default()
                // pgcrypto is used by our worker-auth query.
                .with_init_sql(b"CREATE EXTENSION IF NOT EXISTS pgcrypto;".to_vec())
                // testcontainers-modules defaults to postgres:11-alpine
                // which is too old for `GENERATED ... AS () STORED`
                // in schema/state.sql. Pin a version that supports it.
                .with_tag("15-alpine")
                .start()
                .await
                .ok()?;
            let host = container.get_host().await.ok()?;
            let port = container.get_host_port_ipv4(5432).await.ok()?;
            let admin_url = format!("postgres://postgres:postgres@{}:{}/postgres", host, port);
            Some(PgBackend {
                admin_url,
                _container: Some(container),
            })
        })
        .await
        .as_ref()
}

/// Per-test Postgres database, dropped when this guard is dropped.
struct TestDatabase {
    pool: PgPool,
    name: String,
    admin_pool: PgPool,
    admin_url: String,
}

impl TestDatabase {
    async fn new() -> Option<Self> {
        let backend = backend().await?;
        let admin_pool = PgPool::connect(&backend.admin_url).await.ok()?;
        let name = format!("janitor_test_{}", Uuid::new_v4().simple());
        // Name is a locally generated hex UUID; sqlx can't parameterise
        // DDL identifiers, so wrap in AssertSqlSafe.
        let create = format!("CREATE DATABASE \"{}\"", name);
        if sqlx::query(sqlx::AssertSqlSafe(create))
            .execute(&admin_pool)
            .await
            .is_err()
        {
            return None;
        }
        let test_url = replace_db(&backend.admin_url, &name);
        let pool = PgPool::connect(&test_url).await.ok()?;
        // pgcrypto is used by worker auth; ensure it's present in the
        // per-test database too (init_sql on the container installs it
        // in `postgres`, but new databases start from template1).
        let _ = sqlx::raw_sql("CREATE EXTENSION IF NOT EXISTS pgcrypto")
            .execute(&pool)
            .await;
        Some(Self {
            pool,
            name,
            admin_pool,
            admin_url: backend.admin_url.clone(),
        })
    }

    fn pool(&self) -> &PgPool {
        &self.pool
    }

    fn url(&self) -> String {
        replace_db(&self.admin_url, &self.name)
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        // Detach the pool so DROP DATABASE isn't blocked by open
        // connections; Drop can't await so spawn the cleanup.
        let admin_pool = self.admin_pool.clone();
        let name = self.name.clone();
        let pool = self.pool.clone();
        tokio::spawn(async move {
            pool.close().await;
            let drop = format!("DROP DATABASE IF EXISTS \"{}\"", name);
            let _ = sqlx::query(sqlx::AssertSqlSafe(drop))
                .execute(&admin_pool)
                .await;
        });
    }
}

fn replace_db(url: &str, name: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{}/{}", prefix, name),
        None => format!("{}/{}", url, name),
    }
}

fn build_config(tmp_dir: &std::path::Path, database_url: String) -> Config {
    let mut cfg = Config::default();
    cfg.git.local_path = tmp_dir.to_path_buf();
    cfg.base.database = Some(janitor::shared_config::DatabaseConfig {
        url: database_url,
        ..janitor::shared_config::DatabaseConfig::default()
    });
    cfg
}

async fn build_state(tmp: &TempDir, test_db: &TestDatabase, role: AppRole) -> AppState {
    let cfg = Arc::new(build_config(tmp.path(), test_db.url()));
    let repo_manager = Arc::new(RepositoryManager::new(cfg.git.local_path.clone()));

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .connect(&cfg.base.database.as_ref().unwrap().url)
        .await
        .unwrap();
    let db_manager = Arc::new(DatabaseManager::new(pool));

    let tera = Arc::new(web::init_templates(None).unwrap());
    let health_checker = Arc::new(HealthChecker::new());

    AppState {
        repo_manager,
        config: cfg,
        tera,
        db_manager,
        health_checker,
        role,
    }
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
         VALUES ($1, $2, $2, 'git')
         ON CONFLICT DO NOTHING",
    )
    .bind(name)
    .bind(&url)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_worker(pool: &PgPool, name: &str, password: &str) {
    sqlx::query(
        "INSERT INTO worker (name, password, link)
         VALUES ($1, crypt($2, gen_salt('bf')), NULL)
         ON CONFLICT DO NOTHING",
    )
    .bind(name)
    .bind(password)
    .execute(pool)
    .await
    .unwrap();
}

/// `#[tokio::test]` wrapper that skips cleanly when Postgres isn't
/// reachable.
macro_rules! db_test {
    (async fn $name:ident($db:ident: TestDatabase) $body:block) => {
        // Multi-threaded runtime so blocking work in the test body
        // (`std::process::Command::output`, PyO3 `spawn_blocking`) can
        // run alongside the in-process axum server task.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[serial_test::serial]
        async fn $name() {
            let Some($db) = TestDatabase::new().await else {
                eprintln!("skipping {}: no test database available", stringify!($name));
                return;
            };
            $body
        }
    };
}

db_test! {
    async fn test_admin_health(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::get(format!("http://{}/health", addr)).await.unwrap();
        assert_eq!(resp.status(), 200);

        handle.abort();
    }
}

db_test! {
    async fn test_admin_ready(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::get(format!("http://{}/ready", addr)).await.unwrap();
        assert_eq!(resp.status(), 200);

        handle.abort();
    }
}

db_test! {
    async fn test_public_repo_list_json(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("alpha")).unwrap();
        std::fs::create_dir(tmp.path().join("beta")).unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/git/", addr))
            .header("Accept", "application/json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Vec<String> = resp.json().await.unwrap();
        assert_eq!(body, vec!["alpha".to_string(), "beta".to_string()]);

        handle.abort();
    }
}

db_test! {
    async fn test_public_repo_list_text(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("gamma")).unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/git/", addr))
            .header("Accept", "text/plain")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.unwrap();
        assert_eq!(text, "gamma\n");

        handle.abort();
    }
}

db_test! {
    async fn test_public_home_empty_200(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::get(format!("http://{}/", addr)).await.unwrap();
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.unwrap();
        assert_eq!(text, "");

        handle.abort();
    }
}

db_test! {
    async fn test_admin_diff_503_when_repo_absent(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;
        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        // Valid SHAs but no on-disk repo: 503 with the
        // local-repository-unavailable error class, matching Python.
        let old = "0000000000000000000000000000000000000001";
        let new = "0000000000000000000000000000000000000002";
        let resp = reqwest::get(format!(
            "http://{}/codebase/diff?old={}&new={}",
            addr, old, new
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 503);
        assert_eq!(
            resp.headers()
                .get("X-Janitor-Error")
                .and_then(|v| v.to_str().ok()),
            Some("local-repository-unavailable")
        );

        handle.abort();
    }
}

db_test! {
    async fn test_admin_diff_200_with_real_commits(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;

        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().join("codebase");
        let repo = git2::Repository::init_bare(&repo_path).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();

        let tree_id = {
            let mut idx = repo.index().unwrap();
            idx.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        let c1_oid = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "first", &tree, &[])
            .unwrap();
        let parent = repo.find_commit(c1_oid).unwrap();
        let c2_oid = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "second", &tree, &[&parent])
            .unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::get(format!(
            "http://{}/codebase/diff?old={}&new={}",
            addr, c1_oid, c2_oid
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);

        handle.abort();
    }
}

db_test! {
    async fn test_admin_revision_info_missing_params_400(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;
        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::get(format!("http://{}/codebase/revision-info?new=abc", addr))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "need both old and new");

        handle.abort();
    }
}

db_test! {
    async fn test_admin_revision_info_missing_commit_returns_empty_json(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;

        let tmp = TempDir::new().unwrap();
        // Real bare repo but the SHAs we'll query don't exist.
        let repo_path = tmp.path().join("codebase");
        git2::Repository::init_bare(&repo_path).unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let missing = "0000000000000000000000000000000000000000abcdef";
        let missing = &missing[..40];
        let resp = reqwest::get(format!(
            "http://{}/codebase/revision-info?old={}&new={}",
            addr, missing, missing
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 404);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body, serde_json::json!({}));

        handle.abort();
    }
}

db_test! {
    async fn test_public_smart_protocol_unknown_codebase_404(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::get(format!("http://{}/git/ghost/info/refs?service=git-upload-pack", addr))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        // Body must match Python's `_git_open_repo` exactly.
        let body = resp.text().await.unwrap();
        assert_eq!(body, "no such codebase: ghost");

        handle.abort();
    }
}

/// True when the Python interpreter this build was linked against
/// has `klaus` importable. Skipping keeps CI green on hosts without
/// the Python side installed.
fn klaus_available() -> bool {
    use pyo3::prelude::*;
    Python::attach(|py| py.import("klaus").is_ok())
}

db_test! {
    async fn test_public_klaus_renders_repo_index(test_db: TestDatabase) {
        if !klaus_available() {
            eprintln!("skipping: klaus not importable");
            return;
        }
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "klauzy").await;

        // Give klaus a real repo with one commit so the index page
        // has something to render.
        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("klauzy");
        let repo = git2::Repository::init_bare(&bare).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let tb = repo.treebuilder(None).unwrap();
            tb.write().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        // Browser-style Accept + non-git User-Agent, so
        // is_git_client_request routes to klaus rather than the
        // smart-HTTP backend.
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/git/klauzy/", addr))
            .header("User-Agent", "Mozilla/5.0 (test)")
            .header("Accept", "text/html")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "klaus index should render 200");
        let body = resp.text().await.unwrap();
        // Sanity: klaus's index template mentions the repo somewhere.
        assert!(
            body.contains("klauzy"),
            "expected 'klauzy' in klaus body, got:\n{}",
            body
        );

        handle.abort();
    }
}

// Drive `git push` through the smart-HTTP backend end-to-end. Guards
// against pack-framing regressions in `git_backend`'s CGI bridge
// (stdin streaming, stdout header parsing, body buffering).
db_test! {
    async fn test_admin_smart_protocol_git_push_end_to_end(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "pushtest").await;

        let tmp = TempDir::new().unwrap();
        // Pre-create the bare target so `git http-backend` has a repo
        // to push into (auto-creation via `open_or_create` is fine
        // too, but pinning it here keeps the test deterministic).
        let bare = tmp.path().join("pushtest");
        git2::Repository::init_bare(&bare).unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        // Build a source repo with one commit in a scratch dir and
        // push it. Using the `git` CLI keeps the test independent of
        // libgit2's HTTP transport.
        let src = TempDir::new().unwrap();
        let src_path = src.path();
        let run_git = |args: &[&str]| -> std::process::Output {
            std::process::Command::new("git")
                .args(args)
                .current_dir(src_path)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .expect("failed to run git")
        };

        assert!(run_git(&["init", "-q", "-b", "main"]).status.success());
        std::fs::write(src_path.join("hello.txt"), "hi\n").unwrap();
        assert!(run_git(&["add", "hello.txt"]).status.success());
        assert!(run_git(&["commit", "-q", "-m", "init"]).status.success());

        let remote_url = format!("http://{}/pushtest", addr);
        let push = run_git(&["push", &remote_url, "main:refs/heads/main"]);
        assert!(
            push.status.success(),
            "git push failed: stdout={} stderr={}",
            String::from_utf8_lossy(&push.stdout),
            String::from_utf8_lossy(&push.stderr)
        );

        // Verify server-side: the pushed ref should now be present.
        let server_repo = git2::Repository::open_bare(&bare).unwrap();
        let head = server_repo
            .find_reference("refs/heads/main")
            .expect("refs/heads/main missing on server after push");
        assert!(head.target().is_some());

        handle.abort();
    }
}

db_test! {
    async fn test_public_worker_auth_flows_through(test_db: TestDatabase) {
        // Checks the auth + codebase-exists gate; does not drive a
        // full push.
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;
        seed_worker(test_db.pool(), "alice", "s3cret").await;

        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/git/foo/info/refs?service=git-upload-pack", addr))
            .basic_auth("alice", Some("s3cret"))
            .send()
            .await
            .unwrap();
        // Whatever git http-backend returns downstream, the gate passed
        // if we're neither 404 nor 401.
        assert_ne!(resp.status(), 404, "codebase-exists check failed");
        assert_ne!(resp.status(), 401, "worker auth failed");

        handle.abort();
    }
}

db_test! {
    async fn test_admin_metrics_returns_prometheus_text(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        // Make at least one request so the per-request middleware
        // has something to record.
        reqwest::get(format!("http://{}/health", addr)).await.unwrap();

        let resp = reqwest::get(format!("http://{}/metrics", addr)).await.unwrap();
        assert_eq!(resp.status(), 200);
        let ct = resp.headers().get("content-type").unwrap().to_str().unwrap().to_string();
        assert!(ct.starts_with("text/plain"), "unexpected content-type: {}", ct);
        let body = resp.text().await.unwrap();
        // The middleware records http_requests_total; the preceding
        // /health call must appear in the output.
        assert!(
            body.contains("http_requests_total"),
            "metrics output missing http_requests_total:\n{}",
            body
        );
        assert!(body.contains("http_request_duration_seconds"));

        handle.abort();
    }
}

db_test! {
    async fn test_public_app_does_not_expose_admin_endpoints(test_db: TestDatabase) {
        // /health, /ready, /metrics must stay admin-only, matching
        // Python's `create_web_app` in `py/janitor/git_store.py`.
        // Hitting them on the public port should 404.
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        for path in ["/health", "/ready", "/metrics"] {
            let resp = reqwest::get(format!("http://{}{}", addr, path))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                404,
                "public app should not expose {}, got {}",
                path,
                resp.status()
            );
        }

        handle.abort();
    }
}

db_test! {
    async fn test_admin_repo_list_at_root(test_db: TestDatabase) {
        // The admin app exposes `list_repositories` at `/`.
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("only-one")).unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{}/", addr))
            .header("Accept", "application/json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Vec<String> = resp.json().await.unwrap();
        assert_eq!(body, vec!["only-one".to_string()]);

        handle.abort();
    }
}

db_test! {
    async fn test_admin_diff_streams_large_body(test_db: TestDatabase) {
        // Build a repo whose second commit introduces a ~400 KB file.
        // The response must arrive chunked (no Content-Length) and
        // the full body must round-trip without truncation.
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;

        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().join("codebase");
        let repo = git2::Repository::init_bare(&repo_path).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();

        let empty_tree = {
            let mut idx = repo.index().unwrap();
            let id = idx.write_tree().unwrap();
            repo.find_tree(id).unwrap()
        };
        let c1 = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "empty", &empty_tree, &[])
            .unwrap();

        // Second commit: a single text file of ~400 KB (50 lines * 8 KB)
        // guaranteed to spill past the 8 KB first-chunk peek and
        // exercise the streaming path.
        let payload = (0..50)
            .map(|i| format!("line {} {}\n", i, "x".repeat(8000)))
            .collect::<String>();
        let blob_oid = repo.blob(payload.as_bytes()).unwrap();
        let mut tb = repo.treebuilder(None).unwrap();
        tb.insert("big.txt", blob_oid, 0o100644).unwrap();
        let tree2 = repo.find_tree(tb.write().unwrap()).unwrap();
        let parent = repo.find_commit(c1).unwrap();
        let c2 = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "big", &tree2, &[&parent])
            .unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::get(format!(
            "http://{}/codebase/diff?old={}&new={}",
            addr, c1, c2
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        // Streaming responses have no Content-Length.
        assert!(
            resp.headers().get("content-length").is_none(),
            "streaming diff should not have a Content-Length header",
        );
        let body = resp.bytes().await.unwrap();
        // Body must contain the payload line markers end-to-end.
        let s = std::str::from_utf8(&body).unwrap();
        assert!(s.contains("line 0 "), "first line missing");
        assert!(s.contains("line 49 "), "last line missing (truncated?)");
        assert!(body.len() > 300_000, "body suspiciously small: {}", body.len());

        handle.abort();
    }
}

db_test! {
    async fn test_admin_diff_empty_still_200(test_db: TestDatabase) {
        // Same tree on both commits -> empty diff. Streaming must
        // still produce a clean 200 rather than hanging or 500ing.
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;

        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().join("codebase");
        let repo = git2::Repository::init_bare(&repo_path).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let mut idx = repo.index().unwrap();
            idx.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        let c1 = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "first", &tree, &[])
            .unwrap();
        let parent = repo.find_commit(c1).unwrap();
        let c2 = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "second", &tree, &[&parent])
            .unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::get(format!(
            "http://{}/codebase/diff?old={}&new={}",
            addr, c1, c2
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.bytes().await.unwrap();
        assert_eq!(body.len(), 0, "identical trees should produce empty diff");

        handle.abort();
    }
}

db_test! {
    async fn test_admin_diff_missing_commit_500(test_db: TestDatabase) {
        // No precheck for revision presence anymore -- git itself
        // errors out, which surfaces as 500 "git diff failed: ..."
        // matching Python exactly.
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;

        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().join("codebase");
        git2::Repository::init_bare(&repo_path).unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let missing = "0123456789abcdef0123456789abcdef01234567";
        let resp = reqwest::get(format!(
            "http://{}/codebase/diff?old={}&new={}",
            addr, missing, missing
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 500);
        let body = resp.text().await.unwrap();
        assert!(
            body.starts_with("git diff failed:"),
            "expected 'git diff failed:' prefix, got: {}",
            body
        );

        handle.abort();
    }
}

db_test! {
    async fn test_admin_diff_invalid_sha_400(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;
        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        // Repo dir exists; invalid SHAs.
        std::fs::create_dir_all(tmp.path().join("codebase")).unwrap();
        let resp = reqwest::get(format!(
            "http://{}/codebase/diff?old=notasha&new=notasha",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 400);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "invalid shas specified");

        handle.abort();
    }
}

db_test! {
    async fn test_admin_revision_info_happy_path(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;

        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().join("codebase");
        let repo = git2::Repository::init_bare(&repo_path).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let mut idx = repo.index().unwrap();
            idx.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        let c1 = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "first", &tree, &[])
            .unwrap();
        let parent = repo.find_commit(c1).unwrap();
        let c2 = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "second", &tree, &[&parent])
            .unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::get(format!(
            "http://{}/codebase/revision-info?old={}&new={}",
            addr, c1, c2
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Vec<serde_json::Value> = resp.json().await.unwrap();
        // One commit in the exclusive `c1..c2` range.
        assert_eq!(body.len(), 1, "expected one commit, got {:?}", body);
        let entry = &body[0];
        assert_eq!(entry["commit-id"], c2.to_string());
        assert_eq!(entry["revision-id"], format!("git-v1:{}", c2));
        assert_eq!(entry["link"], format!("/git/codebase/commit/{}/", c2));
        assert!(entry["message"].as_str().unwrap().contains("second"));

        handle.abort();
    }
}

db_test! {
    async fn test_admin_log_happy_path(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;

        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().join("codebase");
        let repo = git2::Repository::init_bare(&repo_path).unwrap();
        let sig = git2::Signature::now("Alice", "alice@example.com").unwrap();
        let tree_id = {
            let mut idx = repo.index().unwrap();
            idx.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        let c1 = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "first", &tree, &[])
            .unwrap();
        let parent = repo.find_commit(c1).unwrap();
        let c2 = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "second", &tree, &[&parent])
            .unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::get(format!(
            "http://{}/codebase/log?old={}&new={}",
            addr, c1, c2
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let commits = body["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0]["sha"], c2.to_string());
        assert_eq!(commits[0]["author"]["name"], "Alice");
        assert_eq!(commits[0]["author"]["email"], "alice@example.com");

        handle.abort();
    }
}

db_test! {
    async fn test_admin_set_remote_writes_config(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;

        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().join("codebase");
        git2::Repository::init_bare(&repo_path).unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{}/codebase/remotes/upstream", addr))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("url=https%3A%2F%2Fexample.com%2Frepo.git")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Server-side: `git config remote.upstream.url` should return
        // the URL, and `.fetch` should have the default refspec.
        let repo = git2::Repository::open_bare(&repo_path).unwrap();
        let remote = repo.find_remote("upstream").unwrap();
        assert_eq!(remote.url(), Some("https://example.com/repo.git"));
        let fetch_specs: Vec<String> = remote
            .fetch_refspecs()
            .unwrap()
            .iter()
            .flatten()
            .map(String::from)
            .collect();
        assert_eq!(fetch_specs, vec!["+refs/heads/*:refs/remotes/upstream/*"]);

        handle.abort();
    }
}

db_test! {
    async fn test_admin_set_remote_missing_url_400(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;
        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{}/codebase/remotes/upstream", addr))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("wrongfield=x")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        handle.abort();
    }
}

db_test! {
    async fn test_admin_set_remote_unknown_codebase_404(test_db: TestDatabase) {
        // Matches Python `_git_open_repo`: unknown codebase must 404.
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{}/ghost/remotes/upstream", addr))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("url=https%3A%2F%2Fexample.com%2Fr.git")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "no such codebase: ghost");

        handle.abort();
    }
}

db_test! {
    async fn test_admin_smart_protocol_clone_end_to_end(test_db: TestDatabase) {
        // Push once, then clone via the same admin server. Exercises
        // git-upload-pack, which the push test doesn't cover.
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "cloneme").await;

        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("cloneme");
        let repo = git2::Repository::init_bare(&bare).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let mut idx = repo.index().unwrap();
            idx.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let dst = TempDir::new().unwrap();
        let dst_path = dst.path().join("clone");
        let out = std::process::Command::new("git")
            .args(["clone", "-q", &format!("http://{}/cloneme", addr)])
            .arg(&dst_path)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git clone spawn");
        assert!(
            out.status.success(),
            "git clone failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(dst_path.join(".git").exists());

        handle.abort();
    }
}

db_test! {
    async fn test_public_receive_pack_denied_without_auth(test_db: TestDatabase) {
        // On the public app an unauthenticated receive-pack advertisement
        // request must be challenged with 401 (not silently allowed and
        // not 403). Matches Python's `_git_check_service`.
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "gated").await;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("gated")).unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::get(format!(
            "http://{}/git/gated/info/refs?service=git-receive-pack",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 401);
        let www = resp
            .headers()
            .get("WWW-Authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            www.contains("realm=\"Janitor\""),
            "expected Janitor realm in {}",
            www
        );

        handle.abort();
    }
}

db_test! {
    async fn test_public_receive_pack_bad_credentials_401(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "gated2").await;
        seed_worker(test_db.pool(), "alice", "s3cret").await;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("gated2")).unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::Client::new()
            .get(format!(
                "http://{}/git/gated2/info/refs?service=git-receive-pack",
                addr
            ))
            .basic_auth("alice", Some("wrong"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        handle.abort();
    }
}

db_test! {
    async fn test_admin_client_max_size_enforced(test_db: TestDatabase) {
        // A cap of 1 KiB should reject a POST body of 100 KiB with 413.
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("codebase")).unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        // set_remote is the simplest POST handler we have; hit it with
        // an oversized body.
        let app = web::create_admin_app(state, 1024);
        let (addr, handle) = spawn(app).await;

        let big = vec![b'x'; 100 * 1024];
        let resp = reqwest::Client::new()
            .post(format!("http://{}/codebase/remotes/upstream", addr))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(big)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 413);

        handle.abort();
    }
}

db_test! {
    async fn test_admin_client_max_size_zero_is_unlimited(test_db: TestDatabase) {
        // A cap of 0 must mean "unlimited" (matches Python's aiohttp
        // convention). The push test already runs with `client_max_size
        // = 0` and pushes a real pack, but pin the invariant here too.
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("codebase")).unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let big = vec![b'x'; 5 * 1024 * 1024];
        let resp = reqwest::Client::new()
            .post(format!("http://{}/codebase/remotes/upstream", addr))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(big)
            .send()
            .await
            .unwrap();
        // The body is malformed form-urlencoded so the handler will
        // fail parsing, but the important assertion is that we didn't
        // 413 before even reaching the handler.
        assert_ne!(resp.status(), 413);

        handle.abort();
    }
}

db_test! {
    async fn test_public_klaus_auto_creates_repo_for_known_codebase(test_db: TestDatabase) {
        // Mirror Python `_git_open_repo`: klaus browsing a codebase
        // that's in the DB but has no local clone must trigger repo
        // auto-creation, not 404.
        if !klaus_available() {
            eprintln!("skipping: klaus not importable");
            return;
        }
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "auto").await;

        let tmp = TempDir::new().unwrap();
        // Deliberately do NOT pre-create the bare repo.
        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{}/git/auto/", addr))
            .header("User-Agent", "Mozilla/5.0 (test)")
            .header("Accept", "text/html")
            .send()
            .await
            .unwrap();
        // klaus itself may render 404 for a ref-less repo since it
        // can't resolve a default branch. The real assertion is that
        // the on-disk clone got created, which is what Python does.
        let _ = resp.status();
        assert!(tmp.path().join("auto").exists());
        assert!(tmp.path().join("auto").join("HEAD").exists());

        handle.abort();
    }
}

db_test! {
    async fn test_public_klaus_unknown_codebase_404(test_db: TestDatabase) {
        // klaus on a codebase that isn't in the DB and has no local
        // clone must 404.
        if !klaus_available() {
            eprintln!("skipping: klaus not importable");
            return;
        }
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{}/git/ghost/", addr))
            .header("User-Agent", "Mozilla/5.0 (test)")
            .header("Accept", "text/html")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "no such codebase: ghost");

        handle.abort();
    }
}

db_test! {
    async fn test_public_klaus_tree_and_commit_views(test_db: TestDatabase) {
        // Exercise more than the index page: /tree/<rev>/ (browse
        // files) and /commit/<sha>/ (commit view). Catches regressions
        // where klaus's URL rules don't match what we register.
        if !klaus_available() {
            eprintln!("skipping: klaus not importable");
            return;
        }
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "browse").await;

        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("browse");
        let repo = git2::Repository::init_bare(&bare).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let tb = repo.treebuilder(None).unwrap();
            tb.write().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        let commit = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let client = reqwest::Client::new();
        for path in [
            format!("http://{}/git/browse/tree/main/", addr),
            format!("http://{}/git/browse/commit/{}/", addr, commit),
        ] {
            let resp = client
                .get(&path)
                .header("User-Agent", "Mozilla/5.0 (test)")
                .header("Accept", "text/html")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "expected 200 from {}", path);
        }

        handle.abort();
    }
}

db_test! {
    async fn test_public_klaus_static_assets_served(test_db: TestDatabase) {
        // klaus.rs mounts klaus's bundled static dir at /git/_static/.
        // Fetch one of its shipped files to prove the ServeDir mount
        // actually works. Skip if the file isn't there.
        if !klaus_available() {
            eprintln!("skipping: klaus not importable");
            return;
        }
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        // Pygments CSS is one of the assets klaus ships.
        let resp = reqwest::get(format!("http://{}/git/_static/pygments.css", addr))
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "expected pygments.css to be served, got {}",
            resp.status()
        );

        handle.abort();
    }
}

db_test! {
    async fn test_admin_smart_protocol_strips_content_encoding(test_db: TestDatabase) {
        // Prove end-to-end that `Content-Encoding: gzip` doesn't make
        // it into git-http-backend's env (which would otherwise try
        // to decompress a body axum has already decoded). We hit
        // info/refs with a bogus `Content-Encoding: gzip` header on
        // an empty body; the handler must still 200 instead of
        // producing a corrupted response or 500ing.
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "ce").await;
        let tmp = TempDir::new().unwrap();
        // Pre-create a bare repo with one commit so info/refs is
        // non-empty and reproducible.
        let bare = tmp.path().join("ce");
        let repo = git2::Repository::init_bare(&bare).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let tb = repo.treebuilder(None).unwrap();
            tb.write().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{}/ce/info/refs?service=git-upload-pack", addr))
            .header("Content-Encoding", "gzip")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // Response body should be the real refs advertisement: the
        // first pkt-line marker starts with four hex chars.
        let body = resp.bytes().await.unwrap();
        assert!(
            body.len() > 20 && body.iter().take(4).all(|b| b.is_ascii_hexdigit()),
            "expected pkt-line prefix, got: {:?}",
            &body[..body.len().min(80)]
        );

        handle.abort();
    }
}

db_test! {
    async fn test_admin_smart_protocol_rejects_arbitrary_path(test_db: TestDatabase) {
        // Previously the catch-all let any path through to git-http-backend.
        // Now unknown paths must 404 before shelling out.
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "codebase").await;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("codebase")).unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Admin).await;
        let app = web::create_admin_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::get(format!("http://{}/codebase/config", addr))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        handle.abort();
    }
}

db_test! {
    async fn test_public_repo_list_406_on_unmatchable_accept(test_db: TestDatabase) {
        // Matches Python's mimeparse `best_match` returning empty ->
        // `HTTPNotAcceptable` (406).
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{}/git/", addr))
            .header("Accept", "image/png")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 406);

        handle.abort();
    }
}

db_test! {
    async fn test_public_repo_list_html(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("alpha")).unwrap();
        let state = build_state(&tmp, &test_db, AppRole::Public).await;
        let app = web::create_public_app(state, 0);
        let (addr, handle) = spawn(app).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{}/git/", addr))
            .header("Accept", "text/html")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        // Regression for janitor.debian.net#112: link must be
        // /git/<repo>/ with the /git/ prefix and trailing slash.
        assert!(body.contains("href=\"/git/alpha/\""), "body missing canonical link:\n{}", body);

        handle.abort();
    }
}
