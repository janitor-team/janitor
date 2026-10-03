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
