//! Request paths must not read files outside the archive directories.
//!
//! Path captures are percent-decoded, so `..` and `%2F` can arrive in
//! any of them. Each test puts a canary beside the archive directory
//! and checks that no route serves it, while a normal request still
//! gets its file.

use std::path::Path;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use janitor::{schema::setup_debian_test_database, test_with_database};
use janitor_archive::config::{AptRepositoryConfig, ArchiveConfig};
use janitor_archive::database::ArchiveDatabase;
use janitor_archive::repository::{RepositoryGenerationConfig, RepositoryGenerator};
use janitor_archive::scanner::PackageScanner;
use janitor_archive::web::ArchiveWebService;
use tempfile::TempDir;
use tower::ServiceExt;

const CANARY: &[u8] = b"CANARY\n";

/// Layout: `<root>/archive` holds everything the service may serve,
/// `<root>/outside` holds the canaries.
async fn build_app(pool: sqlx::PgPool) -> (axum::Router, TempDir) {
    let root = tempfile::tempdir().unwrap();
    let archive = root.path().join("archive");
    let mut config = ArchiveConfig {
        archive_path: archive.clone(),
        ..Default::default()
    };
    config.repositories.insert(
        "lintian-fixes".to_string(),
        AptRepositoryConfig::new(
            "lintian-fixes".to_string(),
            "lintian-fixes".to_string(),
            vec!["amd64".to_string()],
            archive.join("lintian-fixes"),
        ),
    );
    let location = archive.join("artifacts").display().to_string();

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
    (service.router(), root)
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

/// `n` encoded `../` steps followed by `rest`, all in one URL segment.
fn up(n: usize, rest: &str) -> String {
    format!("{}{}", "..%2F".repeat(n), rest.replace('/', "%2F"))
}

/// Fail unless every request in `uris` gets a 404.
async fn assert_not_served<S: AsRef<str>>(app: &axum::Router, uris: &[S]) {
    let mut served = Vec::new();
    for uri in uris {
        let (status, body) = get(app, uri.as_ref()).await;
        if status != StatusCode::NOT_FOUND {
            served.push(format!(
                "{} {:?} {}",
                status.as_u16(),
                String::from_utf8_lossy(&body),
                uri.as_ref()
            ));
        }
    }
    assert!(
        served.is_empty(),
        "{} of {} requests left the archive directories: {served:#?}",
        served.len(),
        uris.len()
    );
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

/// Generate the on-demand tree for `run-1` and return its directory.
async fn on_demand_tree(app: &axum::Router, root: &Path) -> std::path::PathBuf {
    let (status, _) = get(app, "/dists/run/run-1/Release").await;
    assert_eq!(status, StatusCode::OK);
    let tree = root.join("archive/dists/run/run-1");
    for dir in [
        "main/binary-amd64/by-hash/SHA256",
        "main/source/by-hash/SHA256",
    ] {
        std::fs::create_dir_all(tree.join(dir)).unwrap();
    }
    tree
}

test_with_database! {
    async fn pool_file_stays_in_pool(test_db: TestDatabase) {
        setup_debian_test_database(test_db.pool()).await.unwrap();
        let (app, root) = build_app(test_db.pool().clone()).await;
        let base = root.path().join("archive/lintian-fixes");
        write_file(&base.join("pool/p/pkg/real.deb"), b"REAL-DEB\n");
        write_file(&base.join("inside-base.txt"), CANARY);
        write_file(&root.path().join("outside/secret"), CANARY);

        assert_eq!(
            get(&app, "/pool/p/pkg/real.deb").await,
            (StatusCode::OK, b"REAL-DEB\n".to_vec())
        );
        assert_not_served(
            &app,
            &[
                "/pool/../inside-base.txt",
                "/pool/..%2Finside-base.txt",
                "/pool/%2e%2e%2f%2e%2e%2f%2e%2e%2foutside%2fsecret",
            ],
        )
        .await;
    }
}

test_with_database! {
    async fn suite_index_stays_in_suite(test_db: TestDatabase) {
        setup_debian_test_database(test_db.pool()).await.unwrap();
        let (app, root) = build_app(test_db.pool().clone()).await;
        let base = root.path().join("archive/lintian-fixes");
        write_file(&base.join("main/binary-amd64/Packages"), b"packages");
        write_file(&base.join("main/source/Sources"), b"sources");
        let outside = root.path().join("outside");
        for name in ["Packages", "Packages.gz", "Packages.bz2"] {
            write_file(&outside.join(name), CANARY);
            write_file(&outside.join("binary-amd64").join(name), CANARY);
        }
        for name in ["Sources", "Sources.gz", "Sources.bz2"] {
            write_file(&outside.join("source").join(name), CANARY);
        }

        assert_eq!(
            get(&app, "/dists/lintian-fixes/main/binary-amd64/Packages").await,
            (StatusCode::OK, b"packages".to_vec())
        );
        assert_eq!(
            get(&app, "/dists/lintian-fixes/main/source/Sources").await,
            (StatusCode::OK, b"sources".to_vec())
        );

        let component = up(2, "outside");
        let arch = format!("binary-amd64%2F{}", up(4, "outside"));
        let mut uris = Vec::new();
        for name in ["Packages", "Packages.gz", "Packages.bz2"] {
            uris.push(format!("/dists/lintian-fixes/{component}/binary-amd64/{name}"));
            uris.push(format!("/dists/lintian-fixes/main/{arch}/{name}"));
        }
        for name in ["Sources", "Sources.gz", "Sources.bz2"] {
            uris.push(format!("/dists/lintian-fixes/{component}/source/{name}"));
        }
        assert_not_served(&app, &uris).await;
    }
}

test_with_database! {
    async fn suite_by_hash_stays_in_suite(test_db: TestDatabase) {
        setup_debian_test_database(test_db.pool()).await.unwrap();
        let (app, root) = build_app(test_db.pool().clone()).await;
        let base = root.path().join("archive/lintian-fixes/main");
        write_file(&base.join("binary-amd64/by-hash/SHA256/abcd"), b"binary");
        write_file(&base.join("source/by-hash/SHA256/abcd"), b"source");
        let outside = root.path().join("outside");
        write_file(&outside.join("secret"), CANARY);
        write_file(&outside.join("abcd"), CANARY);
        write_file(&outside.join("binary-amd64/by-hash/SHA256/abcd"), CANARY);
        write_file(&outside.join("source/by-hash/SHA256/abcd"), CANARY);

        let suite = "/dists/lintian-fixes";
        assert_eq!(
            get(&app, &format!("{suite}/main/binary-amd64/by-hash/SHA256/abcd")).await,
            (StatusCode::OK, b"binary".to_vec())
        );
        assert_eq!(
            get(&app, &format!("{suite}/main/source/by-hash/SHA256/abcd")).await,
            (StatusCode::OK, b"source".to_vec())
        );

        let component = up(2, "outside");
        let arch = format!("binary-amd64%2F{}", up(4, "outside/binary-amd64"));
        let algo = up(5, "outside");
        let hash = up(6, "outside/secret");
        assert_not_served(
            &app,
            &[
                format!("{suite}/{component}/binary-amd64/by-hash/SHA256/abcd"),
                format!("{suite}/{component}/source/by-hash/SHA256/abcd"),
                format!("{suite}/main/{arch}/by-hash/SHA256/abcd"),
                format!("{suite}/main/binary-amd64/by-hash/{algo}/abcd"),
                format!("{suite}/main/source/by-hash/{algo}/abcd"),
                format!("{suite}/main/binary-amd64/by-hash/SHA256/{hash}"),
                format!("{suite}/main/source/by-hash/SHA256/{hash}"),
            ],
        )
        .await;
    }
}

test_with_database! {
    async fn on_demand_index_stays_in_tree(test_db: TestDatabase) {
        setup_debian_test_database(test_db.pool()).await.unwrap();
        insert_run(test_db.pool(), "run-1").await;
        let (app, root) = build_app(test_db.pool().clone()).await;
        let tree = on_demand_tree(&app, root.path()).await;
        write_file(&tree.join("main/source/Sources"), b"sources");
        let outside = root.path().join("outside");
        write_file(&outside.join("Packages"), CANARY);
        write_file(&outside.join("binary-amd64/Packages"), CANARY);
        write_file(&outside.join("source/Sources"), CANARY);

        let run = "/dists/run/run-1";
        assert_eq!(
            get(&app, &format!("{run}/main/binary-amd64/Packages")).await,
            (
                StatusCode::OK,
                std::fs::read(tree.join("main/binary-amd64/Packages")).unwrap()
            )
        );
        assert_eq!(
            get(&app, &format!("{run}/main/source/Sources")).await,
            (StatusCode::OK, b"sources".to_vec())
        );

        let component = up(4, "outside");
        let arch = format!("binary-amd64%2F{}", up(6, "outside"));
        assert_not_served(
            &app,
            &[
                format!("{run}/{component}/binary-amd64/Packages"),
                format!("{run}/main/{arch}/Packages"),
                format!("{run}/{component}/source/Sources"),
            ],
        )
        .await;
    }
}

test_with_database! {
    async fn on_demand_by_hash_stays_in_tree(test_db: TestDatabase) {
        setup_debian_test_database(test_db.pool()).await.unwrap();
        insert_run(test_db.pool(), "run-1").await;
        let (app, root) = build_app(test_db.pool().clone()).await;
        let tree = on_demand_tree(&app, root.path()).await;
        write_file(&tree.join("main/binary-amd64/by-hash/SHA256/abcd"), b"binary");
        write_file(&tree.join("main/source/by-hash/SHA256/abcd"), b"source");
        let outside = root.path().join("outside");
        write_file(&outside.join("secret"), CANARY);
        write_file(&outside.join("abcd"), CANARY);
        write_file(&outside.join("binary-amd64/by-hash/SHA256/abcd"), CANARY);
        write_file(&outside.join("source/by-hash/SHA256/abcd"), CANARY);

        let run = "/dists/run/run-1";
        assert_eq!(
            get(&app, &format!("{run}/main/binary-amd64/by-hash/SHA256/abcd")).await,
            (StatusCode::OK, b"binary".to_vec())
        );
        assert_eq!(
            get(&app, &format!("{run}/main/source/by-hash/SHA256/abcd")).await,
            (StatusCode::OK, b"source".to_vec())
        );

        let component = up(4, "outside");
        let arch = format!("binary-amd64%2F{}", up(6, "outside/binary-amd64"));
        let algo = up(7, "outside");
        let hash = up(8, "outside/secret");
        assert_not_served(
            &app,
            &[
                format!("{run}/{component}/binary-amd64/by-hash/SHA256/abcd"),
                format!("{run}/main/{arch}/by-hash/SHA256/abcd"),
                format!("{run}/main/binary-amd64/by-hash/{algo}/abcd"),
                format!("{run}/main/binary-amd64/by-hash/SHA256/{hash}"),
                format!("{run}/{component}/source/by-hash/SHA256/abcd"),
                format!("{run}/main/source/by-hash/{algo}/abcd"),
                format!("{run}/main/source/by-hash/SHA256/{hash}"),
            ],
        )
        .await;
    }
}
