//! A codebase name in a URL must not escape the store directory.
//!
//! Path captures are percent-decoded, so a name holding `%2F` or `..`
//! once made `repository_path.join(name)` resolve outside the store,
//! on the public browse routes and on the admin routes alike. These
//! tests boot the real apps with a branch inside the store and a
//! canary branch outside it, and confirm escaping names get a 400
//! while a normal codebase still works.
//!
//! Skipped when `brz` or a test database is unavailable.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use janitor::{schema::setup_test_database, test_utils::TestDatabase, test_with_database};
use janitor_bzr_store::{
    config::{BzrConfig, BzrStoreConfig, Config},
    web::create_applications,
};
use tempfile::TempDir;

fn brz_available() -> bool {
    let Ok(tmp) = tempfile::tempdir() else {
        return false;
    };
    Command::new("brz")
        .args(["init", "--format=2a"])
        .arg(tmp.path())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn build_config(repo_path: &std::path::Path, database_url: String) -> Config {
    let mut cfg = BzrStoreConfig::default();
    cfg.bzr = BzrConfig {
        repository_path: repo_path.to_path_buf(),
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

fn janitor_config() -> Arc<janitor::config::Config> {
    Arc::new(janitor::config::read_string("campaign { name: \"campaign\" }").unwrap())
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

fn make_branch(path: &std::path::Path) -> bool {
    std::fs::create_dir_all(path).unwrap();
    for args in [
        vec!["init", "--format=2a"],
        vec!["whoami", "test <test@example.com>"],
    ] {
        let out = Command::new("brz").args(&args).current_dir(path).output();
        if out.map(|o| !o.status.success()).unwrap_or(true) {
            return false;
        }
    }
    std::fs::write(path.join("README"), "secret\n").unwrap();
    Command::new("brz")
        .args(["add", "README"])
        .current_dir(path)
        .output()
        .ok();
    Command::new("brz")
        .args(["commit", "-m", "initial"])
        .current_dir(path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// Loggerhead's `block_in_place` needs a multi-threaded runtime.
#[test]
#[serial_test::serial]
fn browse_codebase_name_cannot_leave_store() {
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
                eprintln!("skipping: {e}");
                return;
            }
        };
        if !brz_available() {
            eprintln!("skipping: brz not on PATH");
            return;
        }
        setup_test_database(test_db.pool()).await.unwrap();

        let db_url = test_db_url(&test_db);

        // Store holds `foo`; `secret` is a branch one level above it.
        let root = TempDir::new().unwrap();
        let store = root.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        if !make_branch(&store.join("foo")) {
            eprintln!("skipping: could not create a bzr branch");
            return;
        }
        let secret = root.path().join("secret");
        assert!(make_branch(&secret), "canary branch");

        let cfg = build_config(&store, db_url);
        let (_admin, public) = create_applications(cfg, janitor_config()).await.unwrap();
        let (addr, handle) = spawn(public).await;

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        // A normal codebase is still routed into loggerhead.
        let resp = client
            .get(format!("http://{}/bzr/foo/", addr))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        assert!(
            matches!(status, 200 | 301 | 302 | 307 | 308 | 503),
            "normal codebase gave {status}"
        );

        // `%2F` keeps the escaping name in one URL segment.
        let relative = "..%2Fsecret";
        let absolute = secret.to_str().unwrap().replace('/', "%2F");
        for name in [relative.to_string(), absolute] {
            for suffix in ["/", "/changes"] {
                let url = format!("http://{}/bzr/{}{}", addr, name, suffix);
                let resp = client.get(&url).send().await.unwrap();
                assert_eq!(resp.status().as_u16(), 400, "{url} was not rejected");
            }
        }

        // Smart sub-path campaign and role segments get the same check.
        for sub in ["campaign//.bzr/smart", "campaign/a%00b/.bzr/smart"] {
            let url = format!("http://{}/bzr/foo/{}", addr, sub);
            let resp = client.post(&url).body("").send().await.unwrap();
            assert_eq!(resp.status().as_u16(), 400, "{url} was not rejected");
        }

        handle.abort();
    });
}

fn test_db_url(test_db: &TestDatabase) -> String {
    let base = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://localhost/postgres".to_string());
    match base.rsplit_once('/') {
        Some((prefix, _)) => format!("{}/{}", prefix, test_db.database_name),
        None => format!("{}/{}", base, test_db.database_name),
    }
}

fn last_revision(branch: &Path) -> String {
    let out = Command::new("brz")
        .arg("revision-info")
        .current_dir(branch)
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    text.split_whitespace().nth(1).unwrap().to_string()
}

/// Every file and directory below `dir`, with file contents.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(dir).unwrap().to_path_buf();
            if path.is_dir() {
                out.insert(rel, None);
                todo.push(path);
            } else {
                out.insert(rel, Some(std::fs::read(&path).unwrap()));
            }
        }
    }
    out
}

/// The admin app over a store holding the branch `foo`, with a canary
/// branch beside the store.
struct Admin {
    root: TempDir,
    url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Admin {
    fn store(&self) -> PathBuf {
        self.root.path().join("store")
    }

    fn canary(&self) -> PathBuf {
        self.root.path().join("canary")
    }
}

/// The canary branch as a codebase name: one URL segment.
const ESCAPE: &str = "..%2Fcanary";

async fn start_admin(test_db: &TestDatabase) -> Option<Admin> {
    if !brz_available() {
        eprintln!("skipping: brz not on PATH");
        return None;
    }
    setup_test_database(test_db.pool()).await.unwrap();
    let root = TempDir::new().unwrap();
    let store = root.path().join("store");
    if !make_branch(&store.join("foo")) {
        eprintln!("skipping: could not create a bzr branch");
        return None;
    }
    assert!(make_branch(&root.path().join("canary")), "canary branch");

    let cfg = build_config(&store, test_db_url(test_db));
    let (admin, _public) = create_applications(cfg, janitor_config()).await.unwrap();
    let (addr, handle) = spawn(admin).await;
    Some(Admin {
        root,
        url: format!("http://{}", addr),
        handle,
    })
}

async fn get(url: &str) -> (u16, String) {
    let resp = reqwest::get(url).await.unwrap();
    (resp.status().as_u16(), resp.text().await.unwrap())
}

async fn post(url: &str, body: &'static str) -> u16 {
    let resp = reqwest::Client::new()
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .unwrap();
    resp.status().as_u16()
}

test_with_database! {
    async fn admin_info_cannot_leave_store(test_db: TestDatabase) {
        let Some(admin) = start_admin(&test_db).await else { return };

        let (status, body) = get(&format!("{}/foo/info", admin.url)).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(&last_revision(&admin.store().join("foo"))), "{body}");

        let (status, body) = get(&format!("{}/{ESCAPE}/info", admin.url)).await;
        assert_eq!(status, 400, "info left the store: {body}");

        admin.handle.abort();
    }
}

test_with_database! {
    async fn admin_diff_cannot_leave_store(test_db: TestDatabase) {
        let Some(admin) = start_admin(&test_db).await else { return };

        let new = last_revision(&admin.store().join("foo"));
        let (status, body) = get(&format!("{}/foo/diff?old=null:&new={new}", admin.url)).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("+secret"), "{body}");

        let new = last_revision(&admin.canary());
        let url = format!("{}/{ESCAPE}/diff?old=null:&new={new}", admin.url);
        let (status, body) = get(&url).await;
        assert_eq!(status, 400, "diff left the store: {body}");

        admin.handle.abort();
    }
}

test_with_database! {
    async fn admin_revision_info_cannot_leave_store(test_db: TestDatabase) {
        let Some(admin) = start_admin(&test_db).await else { return };

        let new = last_revision(&admin.store().join("foo"));
        let url = format!("{}/foo/revision-info?old=null:&new={new}", admin.url);
        let (status, body) = get(&url).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(&new), "{body}");

        let new = last_revision(&admin.canary());
        let url = format!("{}/{ESCAPE}/revision-info?old=null:&new={new}", admin.url);
        let (status, body) = get(&url).await;
        assert_eq!(status, 400, "revision-info left the store: {body}");

        admin.handle.abort();
    }
}

test_with_database! {
    async fn admin_list_remotes_cannot_leave_store(test_db: TestDatabase) {
        let Some(admin) = start_admin(&test_db).await else { return };

        let (status, body) = get(&format!("{}/foo/remotes", admin.url)).await;
        assert_eq!(status, 200, "{body}");

        let (status, body) = get(&format!("{}/{ESCAPE}/remotes", admin.url)).await;
        assert_eq!(status, 400, "remotes left the store: {body}");

        admin.handle.abort();
    }
}

test_with_database! {
    async fn admin_create_repository_cannot_leave_store(test_db: TestDatabase) {
        let Some(admin) = start_admin(&test_db).await else { return };

        let status = post(&format!("{}/repositories/newcb", admin.url), "").await;
        assert_eq!(status, 200);
        assert!(admin.store().join("newcb/.bzr").is_dir());

        let before = snapshot(&admin.canary());
        let url = format!("{}/repositories/{ESCAPE}%2Fnew", admin.url);
        let status = post(&url, "").await;
        let after = snapshot(&admin.canary());
        let added: Vec<_> = after.keys().filter(|k| !before.contains_key(*k)).collect();
        assert!(before == after, "create wrote outside the store: {added:?}");
        assert_eq!(status, 400);

        admin.handle.abort();
    }
}

test_with_database! {
    async fn admin_configure_remote_cannot_leave_store(test_db: TestDatabase) {
        let Some(admin) = start_admin(&test_db).await else { return };
        let form = "url=https%3A%2F%2Fexample.invalid%2Fparent";

        let status = post(&format!("{}/foo/remotes/origin", admin.url), form).await;
        assert_eq!(status, 200);

        let before = snapshot(&admin.canary());
        let status = post(&format!("{}/{ESCAPE}/remotes/origin", admin.url), form).await;
        let after = snapshot(&admin.canary());
        let changed: Vec<_> = after
            .iter()
            .filter(|(k, v)| before.get(*k) != Some(*v))
            .map(|(k, _)| k)
            .collect();
        assert!(before == after, "remote wrote outside the store: {changed:?}");
        assert_eq!(status, 400);

        let status = post(&format!("{}/foo/remotes/..%2Forigin", admin.url), form).await;
        assert_eq!(status, 400, "remote name was not rejected");

        admin.handle.abort();
    }
}

test_with_database! {
    async fn admin_smart_role_cannot_leave_store(test_db: TestDatabase) {
        let Some(admin) = start_admin(&test_db).await else { return };
        sqlx::query("INSERT INTO codebase (name) VALUES ('foo')")
            .execute(test_db.pool())
            .await
            .unwrap();
        let smart = |path: &str| format!("{}/{path}/.bzr/smart", admin.url);

        let (status, body) = post_smart(&smart("foo")).await;
        assert_eq!(status, 200);
        assert!(body.windows(6).any(|w| w == b"secret"), "{body:?}");

        let (status, body) = post_smart(&smart("foo/campaign/..%2F..%2F..%2Fcanary")).await;
        assert_eq!(status, 400, "role left the store: {}", String::from_utf8_lossy(&body));

        let (status, _) = post_smart(&smart("foo/campaign/..%2F..%2F..%2Fmadeout")).await;
        let made = admin.root.path().join("madeout");
        assert!(!made.exists(), "role made a directory outside the store");
        assert_eq!(status, 400);

        admin.handle.abort();
    }
}

/// A smart `get` request for `./README`.
async fn post_smart(url: &str) -> (u16, Vec<u8>) {
    let resp = reqwest::Client::new()
        .post(url)
        .body(&b"get\x01./README\n"[..])
        .send()
        .await
        .unwrap();
    (resp.status().as_u16(), resp.bytes().await.unwrap().to_vec())
}
