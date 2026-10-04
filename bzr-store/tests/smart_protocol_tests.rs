//! End-to-end integration tests for the bzr-store public app.
//!
//! These boot the real `create_applications(...)` stack against a throw-
//! away Postgres database and a throw-away vcs directory, then fire HTTP
//! requests at an in-process TCP listener. They're the Rust counterpart
//! to Python `tests/test_bzr_store.py`.
//!
//! The tests exercise:
//!
//! * `GET /bzr/` content-negotiation (JSON, plain text, and HTML).
//! * Smart protocol `POST /bzr/{codebase}/.bzr/smart` for a codebase
//!   that exists in the `codebase` table — asserts a 200 and the
//!   on-disk shared repo gets created under `<vcs_path>/{codebase}`.
//! * Smart protocol `POST /bzr/{codebase}/.bzr/smart` for an unknown
//!   codebase — asserts a 404.
//! * Smart protocol `POST /bzr/{codebase}/notcampaign/.bzr/smart`
//!   for a campaign not in `janitor.conf` — asserts a 404.

use std::net::SocketAddr;
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

/// Build a libpq-style URL for the `TestDatabase`. `TestDatabase` doesn't
/// expose the original base_url it was constructed from, so reuse the
/// `TEST_DATABASE_URL` env var (same one the `test_with_database!` macro
/// uses to find the admin database) and swap in the per-test database
/// name. Falls back to `postgresql://localhost/<name>` when the env var
/// is absent, matching `TestDatabase::into_janitor_database`.
fn test_db_url(test_db: &TestDatabase) -> String {
    let base = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://localhost/postgres".to_string());
    // Replace the last path segment (the admin database name) with the
    // per-test database we just created.
    match base.rsplit_once('/') {
        Some((prefix, _)) => format!("{}/{}", prefix, test_db.database_name),
        None => format!("{}/{}", base, test_db.database_name),
    }
}

/// Build a `Config` for the test: point `repository_path` at `tmp_dir`,
/// disable the in-struct admin/public bind (we don't serve from the
/// struct's values — we bind to `:0` manually), and stash the test pool's
/// URL in the shared `base` config slot.
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

/// Build a minimal `janitor::config::Config` with one campaign named
/// `campaign`. Parsed via the same text-format parser the service uses.
fn janitor_config_with_campaign() -> Arc<janitor::config::Config> {
    let text = r#"
campaign {
    name: "campaign"
}
"#;
    Arc::new(janitor::config::read_string(text).expect("parse test janitor.conf"))
}

/// Bind the public Router on an ephemeral port, spawn an axum server,
/// and return both the server's bound address and a JoinHandle.
async fn spawn_public(public_app: Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, public_app).await.ok();
    });
    // Give the server a beat to be ready.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, handle)
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

async fn seed_codebase(pool: &PgPool, name: &str) {
    // `codebase` has `check((branch_url is null) = (url is null))`
    // (see schema/state.sql) — both must be set, or both NULL.
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

test_with_database! {
    async fn test_public_repo_list_json(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("alpha")).unwrap();
        std::fs::create_dir_all(tmp.path().join("beta")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(public).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/bzr/", addr))
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

test_with_database! {
    async fn test_public_repo_list_text(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("gamma")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(public).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/bzr/", addr))
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

test_with_database! {
    async fn test_public_repo_list_html(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("foo")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(public).await;

        let client = reqwest::Client::new();
        // Default Accept */* should pick HTML, matching Python mimeparse.
        let resp = client
            .get(format!("http://{}/bzr/", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            content_type.starts_with("text/html"),
            "expected text/html for Accept */*, got {}",
            content_type
        );
        let text = resp.text().await.unwrap();
        assert!(text.contains("Bazaar Repositories"), "body: {}", text);
        assert!(
            text.contains("foo"),
            "expected codebase 'foo' listed in HTML: {}",
            text
        );
        // Loggerhead-browseable: links point at /bzr/<codebase>/.
        assert!(
            text.contains("/bzr/foo/"),
            "expected href=\"/bzr/foo/\" in HTML: {}",
            text
        );

        handle.abort();
    }
}

test_with_database! {
    async fn test_public_smart_protocol_unknown_codebase_404(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(public).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{}/bzr/ghost/.bzr/smart", addr))
            .body(b"hello".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        handle.abort();
    }
}

test_with_database! {
    async fn test_public_smart_protocol_unknown_campaign_404(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(public).await;

        let client = reqwest::Client::new();
        // `notcampaign` is not in janitor.conf — expect 404.
        let resp = client
            .post(format!("http://{}/bzr/foo/notcampaign/.bzr/smart", addr))
            .body(b"hello".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        handle.abort();
    }
}

// Public smart protocol with HTTP Basic credentials for a known worker
// reaches the write path without panicking. Complementary to the
// anonymous test above — here we exercise `is_worker_request` +
// `resolve_allow_writes` flowing through to the PyO3 handler with
// `allow_writes=true` (which means the `transport.ensure_base()` calls
// on the campaign/role sub-paths run, so the directory structure is
// created on disk).
test_with_database! {
    async fn test_public_smart_protocol_worker_auth_creates_subpaths(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;
        seed_worker(test_db.pool(), "alice", "s3cret").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(public).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{}/bzr/foo/campaign/main/.bzr/smart", addr))
            .basic_auth("alice", Some("s3cret"))
            .body(b"bzr request 3\nhello\n".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // With allow_writes=true the campaign/role transport sub-paths
        // get created via `ensure_base()`. With the shared repo's layout
        // that means `<tmp>/foo/campaign/main/` exists on disk.
        let sub = tmp.path().join("foo").join("campaign").join("main");
        assert!(
            sub.exists(),
            "expected write path to create {}",
            sub.display()
        );

        handle.abort();
    }
}

// End-to-end smoke test equivalent to Python `test_fetch_format`: a
// known codebase + known campaign + known role returns 200 when the
// smart protocol runs to completion, even if the client's payload is
// malformed — Breezy's factory returns a smart-protocol-framed error
// body rather than surfacing as an HTTP error. This proves (1) the
// PyO3 → Breezy happy path runs without panicking, (2) the shared bzr
// repo is created on demand under `<vcs_path>/foo`, and (3) unknown
// protocol payloads round-trip as smart-protocol error responses.
test_with_database! {
    async fn test_public_smart_protocol_runs_to_completion(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(public).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{}/bzr/foo/campaign/main/.bzr/smart", addr))
            .body(b"bzr request 3\nhello\n".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.bytes().await.unwrap();
        // Smart-protocol error responses start with the literal `b"error\x01"`
        // (see `breezy.bzr.smart.message`). Assert on the prefix only —
        // the textual part is emitted by Breezy and may drift.
        let prefix = b"error\x01";
        assert_eq!(&body[..prefix.len()], prefix, "body did not start with smart-protocol error framing: {:?}", body);

        // The shared repo directory should have been created on demand.
        assert!(
            tmp.path().join("foo").join(".bzr").exists(),
            "expected on-demand creation of shared repo at {}/foo/.bzr",
            tmp.path().display()
        );

        handle.abort();
    }
}
