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

mod common;

use axum::body::Body;
use axum::http::Request;
use common::*;
use janitor::test_with_database;
use janitor_bzr_store::web::create_applications;
use tempfile::TempDir;

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

        // Accept */* should pick HTML.
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
// PyO3 -> Breezy happy path runs without panicking, (2) a read-only
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

        let url = "/bzr/foo/.bzr/smart";
        let resp = send(&public, post(url, b"get\x01./README\n".to_vec())).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(body_text(resp).await, "ok\n6\nhello\ndone\n");
        for (req, tail) in [
            (&b"get\x01./%2e%2e/two/t\n"[..], "/foo/two/t\n"),
            (
                b"get\x01./%2e%2e/%2e%2e/canary/secret.txt\n",
                "/foo/canary/secret.txt\n",
            ),
        ] {
            let resp = send(&public, post(url, req.to_vec())).await;
            assert_eq!(resp.status(), 200);
            let body = body_text(resp).await;
            assert!(
                body.starts_with("NoSuchFile\x01") && body.ends_with(tail),
                "{body:?}"
            );
        }
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

        let resp = send(
            &admin,
            post(
                "/foo/.bzr/smart",
                b"mkdir\x01./%2e%2e/%2e%2e/canary/evildir\x01\n".to_vec(),
            ),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body = body_text(resp).await;
        assert!(
            body.starts_with("NoSuchFile\x01") && body.ends_with("/foo/canary/evildir\n"),
            "{body:?}"
        );
        assert_eq!(dir_names(&tmp.path().join("canary")), vec!["secret.txt"]);
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
