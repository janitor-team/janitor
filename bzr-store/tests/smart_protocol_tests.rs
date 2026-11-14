//! End-to-end integration tests for the bzr-store public app.
//!
//! These boot the real `create_applications(...)` stack against a throw-
//! away Postgres database and a throw-away vcs directory, then send HTTP
//! requests directly to the router. They're the Rust counterpart
//! to Python `tests/test_bzr_store.py`.
//!
//! The tests exercise:
//!
//! * `GET /bzr/` content-negotiation (JSON, plain text, and HTML).
//! * Smart protocol `POST /bzr/{codebase}/.bzr/smart` for a codebase
//!   that exists in the `codebase` table: asserts a 200, and that an
//!   anonymous read creates no shared repo at `<vcs_path>/{codebase}/.bzr`.
//! * Smart protocol `POST /bzr/{codebase}/.bzr/smart` for an unknown
//!   codebase — asserts a 404.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::Request;
use axum::response::Response;
use axum::Router;
use janitor::{test_utils::TestDatabase, test_with_database};
use janitor_bzr_store::{
    config::{BzrConfig, BzrStoreConfig, Config},
    web::create_applications,
};
use sqlx::PgPool;
use tempfile::TempDir;
use tower::ServiceExt;

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

async fn send(app: &Router, req: Request<Body>) -> Response {
    app.clone().oneshot(req).await.unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

fn get_accept(uri: &str, accept: &str) -> Request<Body> {
    Request::get(uri)
        .header("Accept", accept)
        .body(Body::empty())
        .unwrap()
}

fn basic_auth(user: &str, password: &str) -> String {
    use base64::Engine;
    let creds = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    format!("Basic {creds}")
}

fn post(uri: &str, body: impl Into<Body>) -> Request<Body> {
    Request::post(uri).body(body.into()).unwrap()
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
    schema = Core;
    async fn test_public_repo_list_json(test_db: TestDatabase) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("alpha")).unwrap();
        std::fs::create_dir_all(tmp.path().join("beta")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&public, get_accept("/bzr/", "application/json")).await;
        assert_eq!(resp.status(), 200);
        let body: Vec<String> = body_json(resp).await;
        assert_eq!(body, vec!["alpha".to_string(), "beta".to_string()]);
    }
}

test_with_database! {
    schema = Core;
    async fn test_public_repo_list_text(test_db: TestDatabase) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("gamma")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&public, get_accept("/bzr/", "text/plain")).await;
        assert_eq!(resp.status(), 200);
        let text = body_text(resp).await;
        assert_eq!(text, "gamma\n");
    }
}

test_with_database! {
    schema = Core;
    async fn test_public_repo_list_html(test_db: TestDatabase) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("foo")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        // Accept */* should pick HTML, matching Python mimeparse.
        let resp = send(&public, get_accept("/bzr/", "*/*")).await;
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
        let text = body_text(resp).await;
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
    }
}

test_with_database! {
    schema = Core;
    async fn test_public_smart_protocol_unknown_codebase_404(test_db: TestDatabase) {
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&public, post("/bzr/ghost/.bzr/smart", b"hello".to_vec())).await;
        assert_eq!(resp.status(), 404);
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
    schema = Core;
    async fn test_public_smart_protocol_worker_auth_creates_subpaths(test_db: TestDatabase) {
        seed_codebase(test_db.pool(), "foo").await;
        seed_worker(test_db.pool(), "alice", "s3cret").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let req = Request::post("/bzr/foo/campaign/main/.bzr/smart")
            .header("Authorization", basic_auth("alice", "s3cret"))
            .body(Body::from(b"bzr request 3\nhello\n".to_vec()))
            .unwrap();
        let resp = send(&public, req).await;
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
    schema = Core;
    async fn test_public_smart_protocol_runs_to_completion(test_db: TestDatabase) {
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(
            &public,
            post("/bzr/foo/campaign/main/.bzr/smart", b"bzr request 3\nhello\n".to_vec()),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body = body_bytes(resp).await;
        // Smart-protocol error responses start with the literal `b"error\x01"`
        // (see `breezy.bzr.smart.message`). Assert on the prefix only —
        // the textual part is emitted by Breezy and may drift.
        let prefix = b"error\x01";
        assert_eq!(&body[..prefix.len()], prefix, "body did not start with smart-protocol error framing: {:?}", body);

        // An anonymous request is read-only and must create nothing on disk.
        assert_eq!(dir_names(tmp.path()), Vec::<String>::new());

        handle.abort();
    }
}

/// Sorted names of the entries in `dir`.
fn dir_names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Lay out `store/{foo,two}` and `canary/secret.txt` next to the store.
fn make_store_with_canary(tmp: &std::path::Path) -> std::path::PathBuf {
    let store = tmp.join("store");
    std::fs::create_dir_all(store.join("foo")).unwrap();
    std::fs::write(store.join("foo").join("README"), "hello\n").unwrap();
    std::fs::create_dir_all(store.join("two")).unwrap();
    std::fs::write(store.join("two").join("t"), "SIBLING\n").unwrap();
    std::fs::create_dir_all(tmp.join("canary")).unwrap();
    std::fs::write(tmp.join("canary").join("secret.txt"), "CANARY\n").unwrap();
    store
}

/// POST a raw smart request; return the status and the body.
async fn smart_post(client: &reqwest::Client, url: &str, body: &[u8]) -> (u16, String) {
    let resp = client.post(url).body(body.to_vec()).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

test_with_database! {
    schema = Core;
    async fn test_public_smart_protocol_get_stays_in_codebase(test_db: TestDatabase) {
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let store = make_store_with_canary(tmp.path());
        let cfg = build_config(&store, test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(public).await;

        let client = reqwest::Client::new();
        let url = format!("http://{}/bzr/foo/.bzr/smart", addr);
        let mut answers = Vec::new();
        for req in [
            &b"get\x01./README\n"[..],
            b"get\x01./%2e%2e/two/t\n",
            b"get\x01./%2e%2e/%2e%2e/canary/secret.txt\n",
        ] {
            answers.push(smart_post(&client, &url, req).await);
        }
        let foo = store.join("foo");
        assert_eq!(
            answers,
            vec![
                (200, "ok\n6\nhello\ndone\n".to_string()),
                (200, format!("NoSuchFile\x01{}/two/t\n", foo.display())),
                (200, format!("NoSuchFile\x01{}/canary/secret.txt\n", foo.display())),
            ]
        );

        handle.abort();
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_smart_protocol_mkdir_stays_in_codebase(test_db: TestDatabase) {
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let store = make_store_with_canary(tmp.path());
        let cfg = build_config(&store, test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn_public(admin).await;

        let client = reqwest::Client::new();
        let answer = smart_post(
            &client,
            &format!("http://{}/foo/.bzr/smart", addr),
            b"mkdir\x01./%2e%2e/%2e%2e/canary/evildir\x01\n",
        )
        .await;
        let foo = store.join("foo");
        assert_eq!(
            (answer, dir_names(&tmp.path().join("canary"))),
            (
                (200, format!("NoSuchFile\x01{}/canary/evildir\n", foo.display())),
                vec!["secret.txt".to_string()]
            )
        );
    }
}

test_with_database! {
    schema = Core;
    async fn test_public_invalid_identifiers_400(test_db: TestDatabase) {
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        for path in [
            "/bzr/Ghost/.bzr/smart",
            "/bzr/foo/..%2Fetc/.bzr/smart",
            "/bzr/foo/Campaign/main/.bzr/smart",
            "/bzr/foo/campaign//.bzr/smart",
        ] {
            let resp = send(&public, post(path, b"hello".to_vec())).await;
            assert_eq!(resp.status(), 400, "{}", path);
        }
        for path in ["/bzr/Ghost", "/bzr/Ghost/changes"] {
            let resp = send(&public, get(path)).await;
            assert_eq!(resp.status(), 400, "{}", path);
        }
    }
}
