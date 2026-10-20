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
// PyO3 → Breezy happy path runs without panicking, (2) a read-only
// (anonymous) request creates nothing under `<vcs_path>/foo`, and (3)
// unknown protocol payloads round-trip as smart-protocol error responses.
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

        // An anonymous request is read-only and must create nothing on disk.
        assert!(
            !tmp.path().join("foo").join(".bzr").exists(),
            "anonymous read must not create a shared repo at {}/foo/.bzr",
            tmp.path().display()
        );

        handle.abort();
    }
}

// Path-traversal jail: smart VFS requests must stay in the codebase directory.

/// Marker written into the canary file outside the store.
const CANARY: &str = "CANARY-OUTSIDE-THE-STORE";
/// Marker written into a sibling codebase, reachable with a single `..`.
const SIBLING: &str = "SIBLING-CODEBASE-FILE";

/// Build a raw protocol-v1 smart request body: `verb\x01arg\x01...\n`.
fn v1(verb: &str, args: &[&str]) -> Vec<u8> {
    let mut parts: Vec<&str> = vec![verb];
    parts.extend_from_slice(args);
    let mut body = parts.join("\u{1}").into_bytes();
    body.push(b'\n');
    body
}

/// POST a raw smart request and return the response body as a lossy string.
async fn smart_post(
    client: &reqwest::Client,
    url: &str,
    body: Vec<u8>,
    auth: Option<(&str, &str)>,
) -> String {
    let mut req = client
        .post(url)
        .header("Content-Type", "application/octet-stream")
        .body(body);
    if let Some((u, p)) = auth {
        req = req.basic_auth(u, Some(p));
    }
    let resp = req.send().await.unwrap();
    assert_eq!(resp.status(), 200, "smart POST should answer 200");
    String::from_utf8_lossy(&resp.bytes().await.unwrap()).into_owned()
}

/// Lay out `tmp/store/{one,two}` and `tmp/canary/secret.txt`; return the store.
fn make_store_with_canary(tmp: &std::path::Path) -> std::path::PathBuf {
    let store = tmp.join("store");
    std::fs::create_dir_all(store.join("one")).unwrap();
    std::fs::write(store.join("one").join("f1"), b"in-bounds-file\n").unwrap();
    std::fs::create_dir_all(store.join("two")).unwrap();
    std::fs::write(store.join("two").join("t"), format!("{}\n", SIBLING)).unwrap();
    std::fs::create_dir_all(tmp.join("canary")).unwrap();
    std::fs::write(
        tmp.join("canary").join("secret.txt"),
        format!("{}\n", CANARY),
    )
    .unwrap();
    store
}

/// Paths that try to reach the sibling codebase or the canary.
fn traversal_get_paths() -> Vec<String> {
    vec![
        "./%2e%2e/two/t".to_string(),
        "./%2E%2E/two/t".to_string(),
        "./.%2e/two/t".to_string(),
        "../../two/t".to_string(),
        "./%2e%2e/%2e%2e/canary/secret.txt".to_string(),
    ]
}

test_with_database! {
    async fn test_public_smart_protocol_anonymous_rejects_path_traversal(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "one").await;
        seed_codebase(test_db.pool(), "two").await;

        let tmp = TempDir::new().unwrap();
        let store = make_store_with_canary(tmp.path());
        let cfg = build_config(&store, test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(public).await;
        let client = reqwest::Client::new();
        let smart = format!("http://{}/bzr/one/.bzr/smart", addr);

        // A normal in-bounds read still works.
        let body = smart_post(&client, &smart, v1("get", &["./f1"]), None).await;
        assert!(
            body.contains("in-bounds-file"),
            "normal anonymous read should work, got: {:?}",
            body
        );

        // No traversal path may return the sibling or the canary contents.
        for p in traversal_get_paths() {
            let body = smart_post(&client, &smart, v1("get", &[&p]), None).await;
            assert!(
                !body.contains(CANARY) && !body.contains(SIBLING),
                "anonymous get {} escaped the codebase directory: {:?}",
                p,
                body
            );
        }

        // list_dir of the parent must not reveal sibling codebases.
        let body = smart_post(&client, &smart, v1("list_dir", &["./%2e%2e"]), None).await;
        assert!(
            !body.contains("two"),
            "anonymous list_dir ./%2e%2e leaked the store root: {:?}",
            body
        );

        handle.abort();
    }
}

test_with_database! {
    async fn test_public_smart_protocol_worker_rejects_path_traversal(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "one").await;
        seed_codebase(test_db.pool(), "two").await;
        seed_worker(test_db.pool(), "alice", "s3cret").await;

        let tmp = TempDir::new().unwrap();
        let store = make_store_with_canary(tmp.path());
        let cfg = build_config(&store, test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(public).await;
        let client = reqwest::Client::new();
        let smart = format!("http://{}/bzr/one/.bzr/smart", addr);
        let auth = Some(("alice", "s3cret"));

        // A write must not land outside the codebase directory.
        let _ = smart_post(
            &client,
            &smart,
            v1("mkdir", &["./%2e%2e/%2e%2e/canary/evildir", ""]),
            auth,
        )
        .await;
        assert!(
            !tmp.path().join("canary").join("evildir").exists(),
            "worker mkdir escaped the codebase directory and wrote outside the store"
        );

        // A write-enabled request still may not read outside the directory.
        for p in traversal_get_paths() {
            let body = smart_post(&client, &smart, v1("get", &[&p]), auth).await;
            assert!(
                !body.contains(CANARY) && !body.contains(SIBLING),
                "worker get {} escaped the codebase directory: {:?}",
                p,
                body
            );
        }

        handle.abort();
    }
}

test_with_database! {
    async fn test_public_smart_protocol_anonymous_missing_dir_creates_nothing(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "ghostdir").await;

        let tmp = TempDir::new().unwrap();
        // The codebase row exists, but no directory is created for it.
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(public).await;
        let client = reqwest::Client::new();

        let smart = format!("http://{}/bzr/ghostdir/.bzr/smart", addr);
        let _ = smart_post(&client, &smart, v1("get", &["./f1"]), None).await;

        assert!(
            !tmp.path().join("ghostdir").exists(),
            "anonymous read of a missing codebase directory created {}",
            tmp.path().join("ghostdir").display()
        );

        handle.abort();
    }
}

test_with_database! {
    async fn test_admin_smart_protocol_rejects_path_traversal(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "one").await;
        seed_codebase(test_db.pool(), "two").await;

        let tmp = TempDir::new().unwrap();
        let store = make_store_with_canary(tmp.path());
        let cfg = build_config(&store, test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(admin).await;
        let client = reqwest::Client::new();
        let smart = format!("http://{}/one/.bzr/smart", addr);

        // The admin port needs no login and always allows writes.
        let _ = smart_post(
            &client,
            &smart,
            v1("mkdir", &["./%2e%2e/%2e%2e/canary/evildir", ""]),
            None,
        )
        .await;
        assert!(
            !tmp.path().join("canary").join("evildir").exists(),
            "admin mkdir escaped the codebase directory and wrote outside the store"
        );

        let body = smart_post(&client, &smart, v1("get", &["./%2e%2e/two/t"]), None).await;
        assert!(
            !body.contains(SIBLING),
            "admin get ./%2e%2e/two/t escaped the codebase directory: {:?}",
            body
        );

        handle.abort();
    }
}

test_with_database! {
    async fn test_smart_protocol_relative_store_path(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "one").await;

        // `--vcs-path store` reaches the handlers as a relative path.
        let cwd = std::env::current_dir().unwrap();
        let tmp = tempfile::tempdir_in(&cwd).unwrap();
        let store = make_store_with_canary(tmp.path());
        let store = store.strip_prefix(&cwd).unwrap();
        assert!(store.is_relative());
        let cfg = build_config(store, test_db_url(&test_db));
        let (admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (public_addr, public_handle) = spawn_public(public).await;
        let (admin_addr, admin_handle) = spawn_public(admin).await;
        let client = reqwest::Client::new();

        for url in [
            format!("http://{}/bzr/one/.bzr/smart", public_addr),
            format!("http://{}/one/.bzr/smart", admin_addr),
        ] {
            let body = smart_post(&client, &url, v1("get", &["./f1"]), None).await;
            assert!(
                body.contains("in-bounds-file"),
                "read through {} failed: {:?}",
                url,
                body
            );
        }

        public_handle.abort();
        admin_handle.abort();
    }
}
