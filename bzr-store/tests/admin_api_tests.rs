//! Integration tests covering the admin app's HTTP surface:
//!
//! * `GET /` and `GET /bzr/` share content negotiation (JSON / text /
//!   HTML) — mirrors Python's `handle_repo_list`.
//! * `GET /metrics` returns Prometheus text-format metrics.
//! * `GET /health` + `GET /ready` return 200.
//! * `POST /{codebase}/remotes/{remote}` with a urlencoded `url=`
//!   body configures the parent location (matches Python
//!   `handle_set_bzr_remote`).
//! * `POST /repositories/{codebase}` ensures the shared repo exists.
//! * The admin `/{codebase}/.bzr/smart` route is reachable (unlike
//!   the public app it has no `/bzr/` prefix and no auth).

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

test_with_database! {
    async fn test_admin_root_lists_repositories_json(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("alpha")).unwrap();
        std::fs::create_dir_all(tmp.path().join("beta")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(admin).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/", addr))
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
    async fn test_admin_metrics_endpoint(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(admin).await;

        let client = reqwest::Client::new();
        // Fire a request first so the metrics middleware has something
        // to record.
        let _ = client.get(format!("http://{}/health", addr)).send().await;
        let resp = client
            .get(format!("http://{}/metrics", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("http_requests_total"),
            "metrics body missing counter: {}",
            body
        );

        handle.abort();
    }
}

test_with_database! {
    async fn test_admin_health_and_ready(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(admin).await;

        let client = reqwest::Client::new();
        let health = client
            .get(format!("http://{}/health", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), 200);
        assert_eq!(health.text().await.unwrap(), "ok");

        let ready = client
            .get(format!("http://{}/ready", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(ready.status(), 200);

        handle.abort();
    }
}

test_with_database! {
    async fn test_admin_configure_remote_form_urlencoded(test_db: TestDatabase) {
        // Matches Python `handle_set_bzr_remote`: body is urlencoded
        // with a `url` field. The handler creates the branch sub-path
        // on demand through configure_remote (which fails on an
        // unknown codebase).
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(admin).await;

        let client = reqwest::Client::new();
        // No pre-existing branch, so configure_remote will return a
        // "path not found" error (404) — the point here is that the
        // routing + urlencoded body decoding is wired right.
        let resp = client
            .post(format!("http://{}/foo/remotes/main", addr))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("url=bzr%2Bssh%3A%2F%2Fexample.invalid%2Ffoo")
            .send()
            .await
            .unwrap();
        // Either 404 (branch doesn't exist yet) or 500 (brz subprocess
        // failed). Should NOT be 400 (bad request) or 405 (method not
        // allowed).
        let status = resp.status().as_u16();
        assert!(
            status == 404 || status == 500,
            "expected 404 or 500 for unknown branch, got {}",
            status
        );

        handle.abort();
    }
}

test_with_database! {
    async fn test_admin_create_repository(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(admin).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{}/repositories/newcb", addr))
            .send()
            .await
            .unwrap();
        // 200 on success; 500 if neither PyO3 nor brz could create a
        // shared repo (e.g. hosts without breezy installed). The
        // directory should exist in either case because
        // ensure_codebase_structure creates it before attempting init.
        let status = resp.status().as_u16();
        assert!(
            status == 200 || status == 500,
            "unexpected status {} on create",
            status
        );
        assert!(
            tmp.path().join("newcb").is_dir(),
            "ensure_codebase_structure should create the directory"
        );

        handle.abort();
    }
}

test_with_database! {
    async fn test_admin_smart_protocol_unauthenticated(test_db: TestDatabase) {
        // Admin interface allows writes without any auth.
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(admin).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{}/foo/.bzr/smart", addr))
            .body(b"bzr request 3\nhello\n".to_vec())
            .send()
            .await
            .unwrap();
        // Admin app always allows writes; expect 200 (smart protocol
        // framed error for a malformed payload is still an HTTP 200).
        assert_eq!(resp.status(), 200);
        // Shared repo must have been created on demand.
        assert!(tmp.path().join("foo").join(".bzr").exists());

        handle.abort();
    }
}

test_with_database! {
    async fn test_public_text_plain_matches_newline_per_line(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("one")).unwrap();
        std::fs::create_dir_all(tmp.path().join("two")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(public).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/bzr/", addr))
            .header("Accept", "text/plain")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "one\ntwo\n");

        handle.abort();
    }
}

test_with_database! {
    async fn test_public_home_returns_empty_200(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(public).await;

        let client = reqwest::Client::new();
        let resp = client.get(format!("http://{}/", addr)).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "");

        handle.abort();
    }
}

test_with_database! {
    async fn test_public_app_does_not_expose_admin_endpoints(test_db: TestDatabase) {
        // /health, /ready, /metrics must stay admin-only, matching
        // Python's `create_web_app` which only mounts them on `app`
        // and not `public_app`. Hitting them on the public port
        // should 404.
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(public).await;

        let client = reqwest::Client::new();
        for path in ["/health", "/ready", "/metrics"] {
            let resp = client
                .get(format!("http://{}{}", addr, path))
                .send()
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

test_with_database! {
    async fn test_public_repo_list_406_on_unmatchable_accept(test_db: TestDatabase) {
        // mimeparse-style negotiator returns None for an Accept header
        // that doesn't match html/plain/json — must surface as 406,
        // matching Python's `web.HTTPNotAcceptable`.
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(public).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/bzr/", addr))
            .header("Accept", "application/xml")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 406);

        handle.abort();
    }
}

test_with_database! {
    async fn test_public_smart_protocol_anonymous_denied_as_readonly(test_db: TestDatabase) {
        // Public app with no credentials should route to the readonly
        // transport path. The handler itself still answers 200
        // because the smart protocol handles the write-rejection
        // in-band — but the campaign/role sub-paths must NOT be
        // created on disk (ensure_base runs only when allow_writes).
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(public).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{}/bzr/foo/campaign/main/.bzr/smart", addr))
            .body(b"bzr request 3\nhello\n".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // Without auth the campaign/main sub-path must not get
        // created on disk — proves the readonly path is taken.
        assert!(
            !tmp.path().join("foo").join("campaign").join("main").exists(),
            "readonly path should not have created campaign/main; it did"
        );

        handle.abort();
    }
}

test_with_database! {
    async fn test_public_smart_protocol_bad_auth_falls_back_to_readonly(test_db: TestDatabase) {
        // Wrong Basic creds must behave like no creds: resolve_allow_writes
        // returns false and the readonly transport is used.
        setup_test_database(test_db.pool()).await.unwrap();
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(public).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{}/bzr/foo/campaign/main/.bzr/smart", addr))
            .basic_auth("mallory", Some("wrong"))
            .body(b"bzr request 3\nhello\n".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(
            !tmp.path().join("foo").join("campaign").join("main").exists(),
            "bogus auth must not get the write path"
        );

        handle.abort();
    }
}

test_with_database! {
    async fn test_admin_repository_info_returns_json(test_db: TestDatabase) {
        // Admin /{codebase}/info — JSON RepositoryInfo. The repo is
        // either empty (just a shared repo with no last_revision) or
        // doesn't exist; both shapes are valid.
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("empty")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(admin).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/empty/info", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["path"]["codebase"], "empty");
        // The directory exists but isn't a bzr repo, so `exists`
        // (true if `.bzr/` is present) must be false.
        assert_eq!(body["exists"], false);
        assert_eq!(body["branch_count"], 0);

        handle.abort();
    }
}

test_with_database! {
    async fn test_public_repo_list_json_sorted(test_db: TestDatabase) {
        // Python sorts the listing; the Rust path does too. Create
        // directories in reverse order and verify alphabetical output.
        setup_test_database(test_db.pool()).await.unwrap();

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("zeta")).unwrap();
        std::fs::create_dir_all(tmp.path().join("alpha")).unwrap();
        std::fs::create_dir_all(tmp.path().join("mu")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();
        let (addr, handle) = spawn(public).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/bzr/", addr))
            .header("Accept", "application/json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Vec<String> = resp.json().await.unwrap();
        assert_eq!(body, vec!["alpha", "mu", "zeta"]);

        handle.abort();
    }
}
