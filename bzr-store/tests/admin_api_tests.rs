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
    schema = Core;
    async fn test_admin_root_lists_repositories_json(test_db: TestDatabase) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("alpha")).unwrap();
        std::fs::create_dir_all(tmp.path().join("beta")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&admin, get_accept("/", "application/json")).await;
        assert_eq!(resp.status(), 200);
        let body: Vec<String> = body_json(resp).await;
        assert_eq!(body, vec!["alpha".to_string(), "beta".to_string()]);
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_metrics_endpoint(test_db: TestDatabase) {
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        // Fire a request first so the metrics middleware has something
        // to record.
        send(&admin, get("/health")).await;
        let resp = send(&admin, get("/metrics")).await;
        assert_eq!(resp.status(), 200);
        let body = body_text(resp).await;
        assert!(
            body.contains("http_requests_total"),
            "metrics body missing counter: {}",
            body
        );
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_health_and_ready(test_db: TestDatabase) {
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let health = send(&admin, get("/health")).await;
        assert_eq!(health.status(), 200);
        assert_eq!(body_text(health).await, "ok");

        let ready = send(&admin, get("/ready")).await;
        assert_eq!(ready.status(), 200);
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_configure_remote_form_urlencoded(test_db: TestDatabase) {
        // Matches Python `handle_set_bzr_remote`: body is urlencoded
        // with a `url` field. The handler creates the branch sub-path
        // on demand through configure_remote (which fails on an
        // unknown codebase).
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        // No pre-existing branch, so configure_remote will return a
        // "path not found" error (404) — the point here is that the
        // routing + urlencoded body decoding is wired right.
        let req = Request::post("/foo/remotes/main")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(Body::from("url=bzr%2Bssh%3A%2F%2Fexample.invalid%2Ffoo"))
            .unwrap();
        let resp = send(&admin, req).await;
        assert_eq!(resp.status(), 404);
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_create_repository(test_db: TestDatabase) {
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&admin, post("/repositories/newcb", Body::empty())).await;
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
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_smart_protocol_unauthenticated(test_db: TestDatabase) {
        // Admin interface allows writes without any auth.
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&admin, post("/foo/.bzr/smart", b"bzr request 3\nhello\n".to_vec())).await;
        // Admin app always allows writes; expect 200 (smart protocol
        // framed error for a malformed payload is still an HTTP 200).
        assert_eq!(resp.status(), 200);
        // Shared repo must have been created on demand.
        assert!(tmp.path().join("foo").join(".bzr").exists());
    }
}

test_with_database! {
    schema = Core;
    async fn test_public_text_plain_matches_newline_per_line(test_db: TestDatabase) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("one")).unwrap();
        std::fs::create_dir_all(tmp.path().join("two")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&public, get_accept("/bzr/", "text/plain")).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(body_text(resp).await, "one\ntwo\n");
    }
}

test_with_database! {
    schema = Core;
    async fn test_public_home_returns_empty_200(test_db: TestDatabase) {
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&public, get("/")).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(body_text(resp).await, "");
    }
}

test_with_database! {
    schema = Core;
    async fn test_public_app_does_not_expose_admin_endpoints(test_db: TestDatabase) {
        // /health, /ready, /metrics must stay admin-only, matching
        // Python's `create_web_app` which only mounts them on `app`
        // and not `public_app`. Hitting them on the public port
        // should 404.
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        for path in ["/health", "/ready", "/metrics"] {
            let resp = send(&public, get(path)).await;
            assert_eq!(
                resp.status(),
                404,
                "public app should not expose {}, got {}",
                path,
                resp.status()
            );
        }
    }
}

test_with_database! {
    schema = Core;
    async fn test_public_repo_list_406_on_unmatchable_accept(test_db: TestDatabase) {
        // mimeparse-style negotiator returns None for an Accept header
        // that doesn't match html/plain/json — must surface as 406,
        // matching Python's `web.HTTPNotAcceptable`.
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&public, get_accept("/bzr/", "application/xml")).await;
        assert_eq!(resp.status(), 406);
    }
}

test_with_database! {
    schema = Core;
    async fn test_public_smart_protocol_anonymous_denied_as_readonly(test_db: TestDatabase) {
        // Public app with no credentials should route to the readonly
        // transport path. The handler itself still answers 200
        // because the smart protocol handles the write-rejection
        // in-band — but the campaign/role sub-paths must NOT be
        // created on disk (ensure_base runs only when allow_writes).
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
        // Without auth the campaign/main sub-path must not get
        // created on disk — proves the readonly path is taken.
        assert!(
            !tmp.path().join("foo").join("campaign").join("main").exists(),
            "readonly path should not have created campaign/main; it did"
        );
    }
}

test_with_database! {
    schema = Core;
    async fn test_public_smart_protocol_bad_auth_falls_back_to_readonly(test_db: TestDatabase) {
        // Wrong Basic creds must behave like no creds: resolve_allow_writes
        // returns false and the readonly transport is used.
        seed_codebase(test_db.pool(), "foo").await;

        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let req = Request::post("/bzr/foo/campaign/main/.bzr/smart")
            .header("Authorization", basic_auth("mallory", "wrong"))
            .body(Body::from(b"bzr request 3\nhello\n".to_vec()))
            .unwrap();
        let resp = send(&public, req).await;
        assert_eq!(resp.status(), 200);
        assert!(
            !tmp.path().join("foo").join("campaign").join("main").exists(),
            "bogus auth must not get the write path"
        );
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_repository_info_returns_json(test_db: TestDatabase) {
        // Admin /{codebase}/info — JSON RepositoryInfo. The repo is
        // either empty (just a shared repo with no last_revision) or
        // doesn't exist; both shapes are valid.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("empty")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&admin, get("/empty/info")).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = body_json(resp).await;
        assert_eq!(body["path"]["codebase"], "empty");
        // The directory exists but isn't a bzr repo, so `exists`
        // (true if `.bzr/` is present) must be false.
        assert_eq!(body["exists"], false);
        assert_eq!(body["branch_count"], 0);
    }
}

test_with_database! {
    schema = Core;
    async fn test_public_repo_list_json_sorted(test_db: TestDatabase) {
        // Python sorts the listing; the Rust path does too. Create
        // directories in reverse order and verify alphabetical output.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("zeta")).unwrap();
        std::fs::create_dir_all(tmp.path().join("alpha")).unwrap();
        std::fs::create_dir_all(tmp.path().join("mu")).unwrap();

        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (_admin, public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&public, get_accept("/bzr/", "application/json")).await;
        assert_eq!(resp.status(), 200);
        let body: Vec<String> = body_json(resp).await;
        assert_eq!(body, vec!["alpha", "mu", "zeta"]);
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_invalid_identifiers_400(test_db: TestDatabase) {
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        for path in ["/Upper/info", "/under_score/diff?old=a&new=b", "/x/remotes"] {
            let resp = send(&admin, get(path)).await;
            assert_eq!(resp.status(), 400, "{}", path);
        }
        for path in [
            "/repositories/..%2Fescape",
            "/Upper/.bzr/smart",
            "/foo/Bad_Campaign/.bzr/smart",
            "/foo/Bad_Campaign/main/.bzr/smart",
            "/foo/campaign/..%2Fescape/.bzr/smart",
        ] {
            let resp = send(&admin, post(path, b"hello".to_vec())).await;
            assert_eq!(resp.status(), 400, "{}", path);
        }
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }
}

test_with_database! {
    schema = Core;
    async fn test_admin_openapi(test_db: TestDatabase) {
        let tmp = TempDir::new().unwrap();
        let cfg = build_config(tmp.path(), test_db_url(&test_db));
        let (admin, _public) = create_applications(cfg, janitor_config_with_campaign())
            .await
            .unwrap();

        let resp = send(&admin, get("/openapi.json")).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = body_json(resp).await;
        let mut paths: Vec<&String> = body["paths"].as_object().unwrap().keys().collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "/",
                "/health",
                "/metrics",
                "/ready",
                "/repositories",
                "/repositories/{codebase}",
                "/{codebase}/diff",
                "/{codebase}/info",
                "/{codebase}/remotes",
                "/{codebase}/remotes/{remote}",
                "/{codebase}/revision-info",
            ]
        );
    }
}
