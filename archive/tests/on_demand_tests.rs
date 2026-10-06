use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use janitor::test_utils::TestDatabase;
use janitor::{schema::setup_test_database, test_with_database};
use janitor_archive::config::ArchiveConfig;
use janitor_archive::database::ArchiveDatabase;
use janitor_archive::error::ArchiveError;
use janitor_archive::on_demand::lookup_builds;
use janitor_archive::repository::{RepositoryGenerationConfig, RepositoryGenerator};
use janitor_archive::scanner::PackageScanner;
use janitor_archive::web::ArchiveWebService;
use tempfile::TempDir;
use tower::ServiceExt;

async fn build_app(pool: sqlx::PgPool) -> (axum::Router, TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = ArchiveConfig::default();
    config.archive_path = tmp.path().to_path_buf();
    let location = format!("local://{}", tmp.path().display());

    let scanner_for_gen = PackageScanner::new(&location).await.unwrap();
    let database_for_gen = ArchiveDatabase::new(pool.clone());
    let generator = RepositoryGenerator::new(
        Arc::new(scanner_for_gen),
        Arc::new(database_for_gen),
        RepositoryGenerationConfig::default(),
    );
    let scanner_for_svc = PackageScanner::new(&location).await.unwrap();
    let database_for_svc = ArchiveDatabase::new(pool);
    let service = ArchiveWebService::new(config, generator, scanner_for_svc, database_for_svc)
        .await
        .unwrap();
    (service.router(), tmp)
}

test_with_database! {
    async fn lookup_builds_404s_an_unknown_run(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let db = ArchiveDatabase::new(test_db.pool().clone());

        let err = lookup_builds(&db, "run", "no-such-run-id")
            .await
            .expect_err("lookup_builds must error on unknown run id");
        assert!(matches!(err, ArchiveError::NotFound(_)), "got: {:?}", err);
    }
}

test_with_database! {
    async fn get_dists_run_unknown_id_responds_404(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        let (app, _tmp) = build_app(test_db.pool().clone()).await;

        let req = Request::builder()
            .method("GET")
            .uri("/dists/run/no-such-run/Release")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}

test_with_database! {
    async fn get_dists_cs_writes_under_dists_directory(test_db: TestDatabase) {
        setup_test_database(test_db.pool()).await.unwrap();
        // schema/debian/debian.sql needs the debversion extension,
        // which the test server may lack.
        sqlx::query(
            "CREATE TABLE debian_build (run_id text not null references run (id), \
             version text not null, distribution text not null, source text not null, \
             binary_packages text[], lintian_result json)",
        )
        .execute(test_db.pool())
        .await
        .unwrap();
        sqlx::query("INSERT INTO change_set (id, campaign) VALUES ('cs1', 'lintian-fixes')")
            .execute(test_db.pool())
            .await
            .unwrap();
        let (app, tmp) = build_app(test_db.pool().clone()).await;

        let req = Request::builder()
            .method("GET")
            .uri("/dists/cs/cs1/Release")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(tmp.path().join("cs/cs1/Release").exists());
        assert!(!tmp.path().join("dists").exists());

        let req = Request::builder()
            .method("GET")
            .uri("/dists/cs/cs1/main/binary-amd64/Packages")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = Request::builder()
            .method("GET")
            .uri("/dists/cs/cs1/main/source/Sources")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
