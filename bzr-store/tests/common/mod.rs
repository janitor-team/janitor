//! Helpers shared by the integration tests.

#![allow(dead_code)]

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::Request;
use axum::response::Response;
use axum::Router;
use janitor::test_utils::TestDatabase;
use janitor_bzr_store::config::{BzrConfig, BzrStoreConfig, Config};
use sqlx::PgPool;
use tower::ServiceExt;

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

pub async fn send(app: &Router, req: Request<Body>) -> Response {
    app.clone().oneshot(req).await.unwrap()
}

pub fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

pub fn get_accept(uri: &str, accept: &str) -> Request<Body> {
    Request::get(uri)
        .header("Accept", accept)
        .body(Body::empty())
        .unwrap()
}

pub fn basic_auth(user: &str, password: &str) -> String {
    use base64::Engine;
    let creds = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    format!("Basic {creds}")
}

pub fn post(uri: &str, body: impl Into<Body>) -> Request<Body> {
    Request::post(uri).body(body.into()).unwrap()
}

pub async fn body_bytes(resp: Response) -> Bytes {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
}

pub async fn body_text(resp: Response) -> String {
    String::from_utf8(body_bytes(resp).await.to_vec()).unwrap()
}

pub async fn body_json<T: serde::de::DeserializeOwned>(resp: Response) -> T {
    serde_json::from_slice(&body_bytes(resp).await).unwrap()
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
