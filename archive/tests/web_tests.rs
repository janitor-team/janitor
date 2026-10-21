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

use apt_repository::hash::{hash_data, HashAlgorithm};
use deb822_fast::{Deb822, Paragraph};

fn lintian_fixes_repo(archive: &Path) -> AptRepositoryConfig {
    AptRepositoryConfig::new(
        "lintian-fixes".to_string(),
        "lintian-fixes".to_string(),
        vec!["amd64".to_string()],
        archive.join("lintian-fixes"),
    )
}

async fn generator(pool: &sqlx::PgPool, archive: &Path) -> RepositoryGenerator {
    let location = archive.join("artifacts").display().to_string();
    RepositoryGenerator::new(
        Arc::new(PackageScanner::new(&location).await.unwrap()),
        Arc::new(ArchiveDatabase::new(pool.clone())),
        RepositoryGenerationConfig::default(),
    )
}

async fn build_app(pool: sqlx::PgPool) -> (axum::Router, TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = ArchiveConfig {
        archive_path: tmp.path().to_path_buf(),
        ..Default::default()
    };
    config
        .repositories
        .insert("lintian-fixes".to_string(), lintian_fixes_repo(tmp.path()));
    let location = tmp.path().join("artifacts").display().to_string();

    let service = ArchiveWebService::new(
        config,
        generator(&pool, tmp.path()).await,
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
        setup_debian_test_database(test_db.pool()).await.unwrap();
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
        setup_debian_test_database(test_db.pool()).await.unwrap();
        insert_run(test_db.pool(), "run-1").await;
        let (app, tmp) = build_app(test_db.pool().clone()).await;

        let (status, _) = get(&app, "/dists/run/run-1/Release").await;
        assert_eq!(status, StatusCode::OK);

        let binary_dir = tmp.path().join("dists/run/run-1/main/binary-amd64");
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

fn sha256(data: &[u8]) -> String {
    let (_, hashes) = hash_data(data, &[HashAlgorithm::Sha256]);
    hashes.get(&HashAlgorithm::Sha256).unwrap().to_string()
}

fn paragraphs(data: Vec<u8>) -> Vec<Paragraph> {
    let text = String::from_utf8(data).unwrap();
    text.parse::<Deb822>().unwrap().iter().cloned().collect()
}

/// The given fields of each paragraph in a deb822 file.
fn fields(data: Vec<u8>, names: &[&str]) -> Vec<Vec<String>> {
    paragraphs(data)
        .iter()
        .map(|p| {
            names
                .iter()
                .map(|n| p.get(n).unwrap_or_default().to_string())
                .collect()
        })
        .collect()
}

/// Fetch the Release file under `prefix` and every index it lists,
/// checking that each is served with the size and SHA256 that Release
/// gives, directly and by hash. Returns the Release paragraph and the
/// names of the files it lists.
async fn check_release(app: &axum::Router, prefix: &str) -> (Paragraph, Vec<String>) {
    let (status, release) = get(app, &format!("{prefix}/Release")).await;
    assert_eq!(status, StatusCode::OK, "{prefix}/Release");
    let release = paragraphs(release).remove(0);
    assert_eq!(release.get("Acquire-By-Hash"), Some("yes"), "{prefix}");
    let mut wrong = Vec::new();
    let mut names = Vec::new();
    for entry in release.get("SHA256").unwrap().lines() {
        let [hash, size, name]: [&str; 3] = match entry.split_whitespace().collect::<Vec<_>>()[..] {
            [hash, size, name] => [hash, size, name],
            [] => continue,
            _ => panic!("bad SHA256 entry in {prefix}/Release: {entry:?}"),
        };
        names.push(name.to_string());
        let dir = name.rsplit_once('/').map_or("", |(dir, _)| dir);
        for uri in [
            format!("{prefix}/{name}"),
            format!("{prefix}/{dir}/by-hash/SHA256/{hash}"),
        ] {
            let (status, body) = get(app, &uri).await;
            if (status, body.len().to_string(), sha256(&body))
                != (StatusCode::OK, size.to_string(), hash.to_string())
            {
                wrong.push(format!("{} {}", status.as_u16(), uri));
            }
        }
    }
    assert_eq!(wrong, Vec::<String>::new());
    (release, names)
}

test_with_database! {
    async fn repositories_are_served_with_their_packages(test_db: TestDatabase) {
        let pool = test_db.pool();
        setup_debian_test_database(pool).await.unwrap();
        insert_run(pool, "run-1").await;
        sqlx::query("UPDATE change_set SET state = 'ready' WHERE id = 'cs-1'")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO debian_build (run_id, version, distribution, source)
             VALUES ('run-1', '2.10-3', 'lintian-fixes', 'hello')",
        )
        .execute(pool)
        .await
        .unwrap();
        let (app, tmp) = build_app(pool.clone()).await;
        let artifacts = tmp.path().join("artifacts/run-1");
        std::fs::create_dir_all(&artifacts).unwrap();
        for entry in std::fs::read_dir("tests/data").unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), artifacts.join(entry.file_name())).unwrap();
        }
        generator(pool, tmp.path())
            .await
            .generate_repository(&lintian_fixes_repo(tmp.path()))
            .await
            .unwrap();

        let indices = [
            "main/binary-amd64/Packages",
            "main/binary-amd64/Packages.gz",
            "main/binary-amd64/Packages.bz2",
            "main/source/Sources",
            "main/source/Sources.gz",
            "main/source/Sources.bz2",
        ];
        let contents = ["main/Contents-amd64", "main/Contents-amd64.gz"];
        let files = [&indices[..], &contents[..]].concat();
        for (prefix, suite) in [
            ("/dists/lintian-fixes", "lintian-fixes"),
            ("/dists/run/run-1", "run/run-1"),
            ("/dists/cs/cs-1", "cs/cs-1"),
            ("/dists/lintian-fixes/hello", "lintian-fixes/hello"),
        ] {
            let (release, names) = check_release(&app, prefix).await;
            assert_eq!(release.get("Suite"), Some(suite), "{prefix}");
            assert_eq!(names, files, "{prefix}");

            let (status, body) = get(&app, &format!("{prefix}/main/binary-amd64/Packages")).await;
            assert_eq!(status, StatusCode::OK, "{prefix}");
            assert_eq!(
                fields(body, &["Package", "Version", "Filename"]),
                vec![vec![
                    "hello",
                    "2.10-3",
                    "lintian-fixes/pkg/hello/run-1/hello_2.10-3_amd64.deb"
                ]],
                "{prefix}"
            );

            let (status, body) = get(&app, &format!("{prefix}/main/source/Sources")).await;
            assert_eq!(status, StatusCode::OK, "{prefix}");
            assert_eq!(
                fields(body, &["Package", "Directory"]),
                vec![vec!["hello", "lintian-fixes/pkg/hello/run-1"]],
                "{prefix}"
            );

            let (status, body) = get(&app, &format!("{prefix}/main/Contents-amd64")).await;
            assert_eq!(status, StatusCode::OK, "{prefix}");
            let body = String::from_utf8(body).unwrap();
            let hello: Vec<Vec<&str>> = body
                .lines()
                .map(|l| l.split_whitespace().collect())
                .filter(|f: &Vec<&str>| f[0] == "usr/bin/hello")
                .collect();
            assert_eq!(hello, vec![vec!["usr/bin/hello", "devel/hello"]], "{prefix}");
        }
    }
}

test_with_database! {
    async fn request_paths_stay_in_the_archive(test_db: TestDatabase) {
        setup_debian_test_database(test_db.pool()).await.unwrap();
        insert_run(test_db.pool(), "run-1").await;
        let (app, tmp) = build_app(test_db.pool().clone()).await;
        write_file(&tmp.path().join("secret"), b"secret");
        write_file(&tmp.path().join("outside/source/Sources"), b"secret");
        // `..` is resolved against real directories, so these have to exist.
        std::fs::create_dir_all(tmp.path().join("lintian-fixes/pool")).unwrap();

        let (status, _) = get(&app, "/dists/run/run-1/Release").await;
        assert_eq!(status, StatusCode::OK);

        for uri in [
            "/pool/..%2F..%2Fsecret",
            "/dists/lintian-fixes/..%2Foutside/source/Sources",
            "/dists/lintian-fixes/%2E%2E%2Foutside/source/Sources",
            "/dists/run/run-1/..%2F..%2F..%2Foutside/source/Sources",
        ] {
            assert_eq!(
                get(&app, uri).await,
                (StatusCode::NOT_FOUND, Vec::new()),
                "{}",
                uri
            );
        }
    }
}

test_with_database! {
    async fn on_demand_release_has_configured_origin(test_db: TestDatabase) {
        setup_debian_test_database(test_db.pool()).await.unwrap();
        insert_run(test_db.pool(), "run-1").await;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = janitor::config::read_string(r#"origin: "janitor.example.org""#).unwrap();
        let location = tmp.path().join("artifacts").display().to_string();
        let service = ArchiveWebService::new(
            ArchiveConfig::from_janitor_config(cfg, tmp.path(), None).unwrap(),
            generator(test_db.pool(), tmp.path()).await,
            PackageScanner::new(&location).await.unwrap(),
            ArchiveDatabase::new(test_db.pool().clone()),
        )
        .await
        .unwrap();

        let (status, body) = get(&service.router(), "/dists/run/run-1/Release").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            paragraphs(body)[0].get("Origin"),
            Some("janitor.example.org")
        );
    }
}
