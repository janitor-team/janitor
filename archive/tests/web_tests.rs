use std::path::Path;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use janitor::{schema::setup_test_database, test_with_database};
use janitor_archive::config::{AptRepositoryConfig, ArchiveConfig};
use janitor_archive::database::ArchiveDatabase;
use janitor_archive::repository::{RepositoryGenerationConfig, RepositoryGenerator};
use janitor_archive::scanner::PackageScanner;
use janitor_archive::web::ArchiveWebService;
use tempfile::TempDir;
use tower::ServiceExt;

async fn build_app(pool: sqlx::PgPool) -> (axum::Router, TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = ArchiveConfig {
        archive_path: tmp.path().to_path_buf(),
        ..Default::default()
    };
    config.repositories.insert(
        "lintian-fixes".to_string(),
        AptRepositoryConfig::new(
            "lintian-fixes".to_string(),
            "lintian-fixes".to_string(),
            vec!["amd64".to_string()],
            tmp.path().join("lintian-fixes"),
        ),
    );
    let location = tmp.path().join("artifacts").display().to_string();

    let generator = RepositoryGenerator::new(
        Arc::new(PackageScanner::new(&location).await.unwrap()),
        Arc::new(ArchiveDatabase::new(pool.clone())),
        RepositoryGenerationConfig::default(),
    );
    let service = ArchiveWebService::new(
        config,
        generator,
        PackageScanner::new(&location).await.unwrap(),
        ArchiveDatabase::new(pool),
    )
    .await
    .unwrap();
    (service.router(), tmp)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, body.to_vec())
}

fn write_file(path: &Path, content: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Core schema plus a `debian_build` table with a text version, since
/// the test database does not have the debversion extension.
async fn setup_database(pool: &sqlx::PgPool) {
    setup_test_database(pool).await.unwrap();
    sqlx::query(
        "CREATE TABLE debian_build (
             run_id text not null references run (id),
             version text not null,
             distribution text not null,
             source text not null,
             binary_packages text[],
             lintian_result json
         )",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_run(pool: &sqlx::PgPool, run_id: &str) {
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ('hello', 'https://example.invalid/hello', 'https://example.invalid/hello', 'git')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO change_set (id, campaign) VALUES ('cs-1', 'lintian-fixes')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO run (id, suite, codebase, result_code, start_time, finish_time,
                          logfilenames, change_set)
         VALUES ($1, 'lintian-fixes', 'hello', 'success',
                 NOW() - INTERVAL '1 minute', NOW() - INTERVAL '30 seconds', '{}', 'cs-1')",
    )
    .bind(run_id)
    .execute(pool)
    .await
    .unwrap();
}

test_with_database! {
    async fn suite_binary_by_hash_is_served(test_db: TestDatabase) {
        setup_database(test_db.pool()).await;
        let (app, tmp) = build_app(test_db.pool().clone()).await;
        let base = tmp.path().join("lintian-fixes/main");
        write_file(&base.join("binary-amd64/by-hash/SHA256/abcd"), b"binary");
        write_file(&base.join("source/by-hash/SHA256/abcd"), b"source");

        assert_eq!(
            get(&app, "/dists/lintian-fixes/main/binary-amd64/by-hash/SHA256/abcd").await,
            (StatusCode::OK, b"binary".to_vec())
        );
        assert_eq!(
            get(&app, "/dists/lintian-fixes/main/source/by-hash/SHA256/abcd").await,
            (StatusCode::OK, b"source".to_vec())
        );
    }
}

test_with_database! {
    async fn on_demand_binary_indices_are_served(test_db: TestDatabase) {
        setup_database(test_db.pool()).await;
        insert_run(test_db.pool(), "run-1").await;
        let (app, tmp) = build_app(test_db.pool().clone()).await;

        let (status, _) = get(&app, "/dists/run/run-1/Release").await;
        assert_eq!(status, StatusCode::OK);

        let binary_dir = tmp.path().join("run/run-1/main/binary-amd64");
        let packages = std::fs::read(binary_dir.join("Packages")).unwrap();
        assert_eq!(
            get(&app, "/dists/run/run-1/main/binary-amd64/Packages").await,
            (StatusCode::OK, packages)
        );

        let by_hash = std::fs::read_dir(binary_dir.join("by-hash/SHA256"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let hash = by_hash.file_name().into_string().unwrap();
        assert_eq!(
            get(
                &app,
                &format!("/dists/run/run-1/main/binary-amd64/by-hash/SHA256/{}", hash)
            )
            .await,
            (StatusCode::OK, std::fs::read(by_hash.path()).unwrap())
        );
    }
}
