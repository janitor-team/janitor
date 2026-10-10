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
    let location = tmp.path().display().to_string();

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

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, body.to_vec())
}

test_with_database! {
    async fn suite_contents_files_are_served(test_db: TestDatabase) {
        use janitor_archive::config::AptRepositoryConfig;

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
        let main = tmp.path().join("lintian-fixes/main");
        std::fs::create_dir_all(main.join("by-hash/SHA256")).unwrap();
        std::fs::write(main.join("Contents-amd64"), b"plain").unwrap();
        std::fs::write(main.join("Contents-amd64.gz"), b"gzipped").unwrap();
        std::fs::write(main.join("by-hash/SHA256/abcd"), b"hashed").unwrap();

        let location = tmp.path().display().to_string();
        let pool = test_db.pool().clone();
        let generator = RepositoryGenerator::new(
            Arc::new(PackageScanner::new(&location).await.unwrap()),
            Arc::new(ArchiveDatabase::new(pool.clone())),
            RepositoryGenerationConfig::default(),
        );
        let app = ArchiveWebService::new(
            config,
            generator,
            PackageScanner::new(&location).await.unwrap(),
            ArchiveDatabase::new(pool),
        )
        .await
        .unwrap()
        .router();

        assert_eq!(
            get(&app, "/dists/lintian-fixes/main/Contents-amd64").await,
            (StatusCode::OK, b"plain".to_vec())
        );
        assert_eq!(
            get(&app, "/dists/lintian-fixes/main/Contents-amd64.gz").await,
            (StatusCode::OK, b"gzipped".to_vec())
        );
        assert_eq!(
            get(&app, "/dists/lintian-fixes/main/by-hash/SHA256/abcd").await,
            (StatusCode::OK, b"hashed".to_vec())
        );
        for uri in [
            "/dists/lintian-fixes/main/Contents-arm64",
            "/dists/lintian-fixes/contrib/Contents-amd64",
            "/dists/lintian-fixes/main/by-hash/SHA256/not-hex",
            "/dists/lintian-fixes/main/by-hash/BOGUS/abcd",
        ] {
            assert_eq!(get(&app, uri).await.0, StatusCode::NOT_FOUND, "{}", uri);
        }
    }
}
