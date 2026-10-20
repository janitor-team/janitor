//! Helpers shared by the integration tests.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use janitor::test_utils::TestDatabase;
use janitor_bzr_store::config::{BzrConfig, BzrStoreConfig, Config};
use sqlx::PgPool;

/// URL of the per-test database on the server `TEST_DATABASE_URL`
/// points at.
pub fn test_db_url(test_db: &TestDatabase) -> String {
    let base = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://localhost/postgres".to_string());
    match base.rsplit_once('/') {
        Some((prefix, _)) => format!("{}/{}", prefix, test_db.database_name),
        None => format!("{}/{}", base, test_db.database_name),
    }
}

pub fn build_config(tmp_dir: &std::path::Path, database_url: String) -> Config {
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

pub fn janitor_config_with_campaign() -> Arc<janitor::config::Config> {
    let text = r#"
campaign {
    name: "campaign"
}
"#;
    Arc::new(janitor::config::read_string(text).expect("parse test janitor.conf"))
}

/// Serve `app` on an ephemeral port.
pub async fn spawn(app: Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, handle)
}

pub async fn seed_codebase(pool: &PgPool, name: &str) {
    // `codebase` requires branch_url and url to be both set or both NULL.
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

/// Insert a worker with a pgcrypto-hashed password.
pub async fn seed_worker(pool: &PgPool, name: &str, password: &str) {
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

/// Whether a working `brz` is installed, for tests that build
/// branches with it.
pub fn brz_available() -> bool {
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
