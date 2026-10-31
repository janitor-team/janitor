use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use janitor::test_with_database;
use janitor_archive::config::AptRepositoryConfig;
use janitor_archive::database::ArchiveDatabase;
use janitor_archive::repository::{RepositoryGenerationConfig, RepositoryGenerator};
use janitor_archive::scanner::PackageScanner;

test_with_database! {
    schema = Debian;
    async fn generate_repository_fails_on_by_hash_cleanup_error(test_db: TestDatabase) {
        let tmp = tempfile::tempdir().unwrap();
        let scanner = PackageScanner::new(tmp.path().join("artifacts").to_str().unwrap())
            .await
            .unwrap();
        let generator = RepositoryGenerator::new(
            Arc::new(scanner),
            Arc::new(ArchiveDatabase::new(test_db.pool().clone())),
            RepositoryGenerationConfig::default(),
        );
        let suite_path = tmp.path().join("dists/lintian-fixes");
        let repo = AptRepositoryConfig::new(
            "lintian-fixes".to_string(),
            "lintian-fixes".to_string(),
            vec!["amd64".to_string()],
            suite_path.clone(),
        );

        // Writable but not listable, so the by-hash copies succeed and
        // the cleanup fails.
        let by_hash_dir = suite_path.join("main/binary-amd64/by-hash/SHA256");
        std::fs::create_dir_all(&by_hash_dir).unwrap();
        std::fs::set_permissions(&by_hash_dir, std::fs::Permissions::from_mode(0o300)).unwrap();

        let result = generator.generate_repository(&repo).await;

        std::fs::set_permissions(&by_hash_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err(), "{:?}", result);
        assert!(!suite_path.join("Release").exists());
    }
}
