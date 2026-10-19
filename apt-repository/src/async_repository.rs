//! Async repository generation functionality.

#[cfg(feature = "async")]
use crate::{AptRepositoryError, HashedFile, PackageFile, Release, Repository, Result, SourceFile};
use std::path::{Path, PathBuf};
use tempfile::TempPath;
use tokio::fs;

/// Async version of the Repository with tokio support.
#[derive(Debug, Clone)]
pub struct AsyncRepository {
    inner: Repository,
}

impl AsyncRepository {
    /// Create a new async repository from a regular repository.
    pub fn new(repository: Repository) -> Self {
        Self { inner: repository }
    }

    /// Generate repository metadata files at the specified path asynchronously.
    pub async fn generate_repository<
        P: AsRef<Path>,
        PP: AsyncPackageProvider,
        SP: AsyncSourceProvider,
    >(
        &self,
        base_path: P,
        package_provider: &PP,
        source_provider: &SP,
    ) -> Result<Release> {
        let base_path = base_path.as_ref();
        let mut release = self.inner.release_builder().build()?;

        // Index files are written to temporary files and only renamed
        // into place once every index has been generated, so a failure
        // leaves the previous indices in place.
        let mut pending: Vec<(TempPath, PathBuf)> = Vec::new();

        for component in &self.inner.components {
            for arch in &self.inner.architectures {
                let packages = package_provider
                    .get_packages(&self.inner.suite, component, arch)
                    .await?;
                self.write_index(
                    base_path,
                    &format!("{}/binary-{}", component, arch),
                    "Packages",
                    packages.to_string().as_bytes(),
                    &mut release,
                    &mut pending,
                )
                .await?;
            }

            let sources = source_provider
                .get_sources(&self.inner.suite, component)
                .await?;
            self.write_index(
                base_path,
                &format!("{}/source", component),
                "Sources",
                sources.to_string().as_bytes(),
                &mut release,
                &mut pending,
            )
            .await?;
        }

        for (temp_path, path) in pending {
            temp_path.persist(&path).map_err(|e| e.error)?;
        }

        let release_content = release.to_string();
        fs::write(base_path.join("Release"), &release_content).await?;

        Ok(release)
    }

    /// Write the compressed variants of one index file into `rel_dir`
    /// under `base_path`, record them in `release` and queue them in
    /// `pending` for renaming into place.
    async fn write_index(
        &self,
        base_path: &Path,
        rel_dir: &str,
        basename: &str,
        content: &[u8],
        release: &mut Release,
        pending: &mut Vec<(TempPath, PathBuf)>,
    ) -> Result<()> {
        let dir = base_path.join(rel_dir);
        fs::create_dir_all(&dir)
            .await
            .map_err(|e| AptRepositoryError::DirectoryCreation(e.to_string()))?;

        let (files, temp_paths) = self
            .write_compressed_file_async(&dir, basename, content)
            .await?;

        for file in &files {
            let mut hashed_file = HashedFile::new(format!("{}/{}", rel_dir, file.path), file.size);
            hashed_file.hashes = file.hashes.clone();
            release.add_file(hashed_file);
        }

        if self.inner.acquire_by_hash {
            self.create_by_hash_links_async(&dir, &files, &temp_paths)
                .await?;
            if let Some(keep_count) = self.inner.by_hash_keep {
                self.cleanup_by_hash_files_async(&dir, keep_count).await?;
            }
        }
        pending.extend(
            temp_paths
                .into_iter()
                .zip(files.iter().map(|file| dir.join(&file.path))),
        );
        Ok(())
    }

    /// Write a file with multiple compression formats to temporary
    /// files in `dir`. Returns the hashed file records along with the
    /// temporary paths, which the caller renames into place.
    async fn write_compressed_file_async<P: AsRef<Path>>(
        &self,
        dir: P,
        basename: &str,
        content: &[u8],
    ) -> Result<(Vec<HashedFile>, Vec<TempPath>)> {
        let dir = dir.as_ref();
        let mut files = Vec::new();
        let mut temp_paths = Vec::new();

        for &compression in &self.inner.compressions {
            let filename = format!("{}{}", basename, compression.extension());

            // Compress the content
            let compressed_content = compression.compress(content)?;

            // Calculate hashes
            let (size, hashes) =
                crate::hash::hash_data(&compressed_content, &self.inner.hash_algorithms);

            let temp_path = tempfile::Builder::new()
                .prefix(&filename)
                .tempfile_in(dir)?
                .into_temp_path();
            fs::write(&temp_path, &compressed_content).await?;

            // Create the hashed file record
            let mut hashed_file = HashedFile::new(filename, size);
            hashed_file.hashes = hashes;
            files.push(hashed_file);
            temp_paths.push(temp_path);
        }

        Ok((files, temp_paths))
    }

    /// Copy each of `files`, whose contents are at `sources`, into the
    /// by-hash directory structure.
    async fn create_by_hash_links_async<P: AsRef<Path>>(
        &self,
        base_dir: P,
        files: &[HashedFile],
        sources: &[TempPath],
    ) -> Result<()> {
        let base_dir = base_dir.as_ref();

        for algorithm in &self.inner.hash_algorithms {
            let by_hash_dir = base_dir.join("by-hash").join(algorithm.as_str());
            fs::create_dir_all(&by_hash_dir)
                .await
                .map_err(|e| AptRepositoryError::DirectoryCreation(e.to_string()))?;

            for (file, source_path) in files.iter().zip(sources) {
                if let Some(hash) = file.get_hash(algorithm) {
                    fs::copy(source_path, by_hash_dir.join(hash)).await?;
                }
            }
        }

        Ok(())
    }

    /// Clean up old by-hash files asynchronously.
    pub async fn cleanup_by_hash_files_async<P: AsRef<Path>>(
        &self,
        base_dir: P,
        keep_count: usize,
    ) -> Result<()> {
        let base_dir = base_dir.as_ref();

        for algorithm in &self.inner.hash_algorithms {
            let by_hash_dir = base_dir.join("by-hash").join(algorithm.as_str());
            let mut read_dir = fs::read_dir(&by_hash_dir).await?;
            let mut entries = Vec::new();

            while let Some(entry) = read_dir.next_entry().await? {
                if entry.file_type().await?.is_file() {
                    entries.push(entry);
                }
            }

            // Sort by modification time (newest first)
            let mut entries_with_metadata = Vec::new();
            for entry in entries {
                let metadata = entry.metadata().await?;
                let modified = metadata.modified()?;
                entries_with_metadata.push((entry, modified));
            }

            entries_with_metadata.sort_by(|a, b| b.1.cmp(&a.1));

            // Remove old files
            for (entry, _) in entries_with_metadata.iter().skip(keep_count) {
                fs::remove_file(entry.path()).await?;
            }
        }

        Ok(())
    }

    /// Get the inner repository.
    pub fn inner(&self) -> &Repository {
        &self.inner
    }
}

/// Async trait for providing package data to the repository generator.
#[async_trait::async_trait]
pub trait AsyncPackageProvider: Send + Sync {
    /// Get packages for a specific suite, component, and architecture.
    async fn get_packages(
        &self,
        suite: &str,
        component: &str,
        architecture: &str,
    ) -> Result<PackageFile>;
}

/// Async trait for providing source package data to the repository generator.
#[async_trait::async_trait]
pub trait AsyncSourceProvider: Send + Sync {
    /// Get source packages for a specific suite and component.
    async fn get_sources(&self, suite: &str, component: &str) -> Result<SourceFile>;
}

/// Async in-memory package provider for testing.
#[derive(Debug, Clone)]
pub struct AsyncMemoryPackageProvider {
    packages: std::collections::HashMap<(String, String, String), PackageFile>,
}

impl AsyncMemoryPackageProvider {
    /// Create a new empty async memory package provider.
    pub fn new() -> Self {
        Self {
            packages: std::collections::HashMap::new(),
        }
    }

    /// Add packages for a specific suite, component, and architecture.
    pub fn add_packages(
        &mut self,
        suite: &str,
        component: &str,
        architecture: &str,
        packages: PackageFile,
    ) {
        self.packages.insert(
            (
                suite.to_string(),
                component.to_string(),
                architecture.to_string(),
            ),
            packages,
        );
    }
}

impl Default for AsyncMemoryPackageProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AsyncPackageProvider for AsyncMemoryPackageProvider {
    async fn get_packages(
        &self,
        suite: &str,
        component: &str,
        architecture: &str,
    ) -> Result<PackageFile> {
        Ok(self
            .packages
            .get(&(
                suite.to_string(),
                component.to_string(),
                architecture.to_string(),
            ))
            .cloned()
            .unwrap_or_default())
    }
}

/// Async in-memory source provider for testing.
#[derive(Debug, Clone)]
pub struct AsyncMemorySourceProvider {
    sources: std::collections::HashMap<(String, String), SourceFile>,
}

impl AsyncMemorySourceProvider {
    /// Create a new empty async memory source provider.
    pub fn new() -> Self {
        Self {
            sources: std::collections::HashMap::new(),
        }
    }

    /// Add sources for a specific suite and component.
    pub fn add_sources(&mut self, suite: &str, component: &str, sources: SourceFile) {
        self.sources
            .insert((suite.to_string(), component.to_string()), sources);
    }
}

impl Default for AsyncMemorySourceProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AsyncSourceProvider for AsyncMemorySourceProvider {
    async fn get_sources(&self, suite: &str, component: &str) -> Result<SourceFile> {
        Ok(self
            .sources
            .get(&(suite.to_string(), component.to_string()))
            .cloned()
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepositoryBuilder;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_async_repository_generation() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();

        let repo = RepositoryBuilder::new()
            .origin("Test")
            .suite("test")
            .architectures(vec!["amd64".to_string()])
            .components(vec!["main".to_string()])
            .build()
            .unwrap();

        let async_repo = AsyncRepository::new(repo);

        let mut package_provider = AsyncMemoryPackageProvider::new();
        let mut packages = PackageFile::new();
        packages.add_package(crate::packages::new_package(
            "test-pkg", "1.0.0", "amd64", "test.deb", 1024,
        ));
        package_provider.add_packages("test", "main", "amd64", packages);

        let source_provider = AsyncMemorySourceProvider::new();

        let release = async_repo
            .generate_repository(repo_path, &package_provider, &source_provider)
            .await
            .unwrap();

        // Check that the Release file was created
        assert!(fs::try_exists(repo_path.join("Release")).await.unwrap());

        // Check that component directories were created
        assert!(fs::try_exists(repo_path.join("main")).await.unwrap());
        assert!(fs::try_exists(repo_path.join("main/binary-amd64"))
            .await
            .unwrap());
        assert!(fs::try_exists(repo_path.join("main/source")).await.unwrap());

        // Check that Packages files were created
        assert!(fs::try_exists(repo_path.join("main/binary-amd64/Packages"))
            .await
            .unwrap());
        assert!(
            fs::try_exists(repo_path.join("main/binary-amd64/Packages.gz"))
                .await
                .unwrap()
        );

        // Check that Sources files were created
        assert!(fs::try_exists(repo_path.join("main/source/Sources"))
            .await
            .unwrap());
        assert!(fs::try_exists(repo_path.join("main/source/Sources.gz"))
            .await
            .unwrap());

        // Check release file content
        assert_eq!(release.origin(), Some("Test".to_string()));
        assert_eq!(release.suite(), Some("test".to_string()));
        assert!(!release.files().is_empty());
    }

    struct FailingSourceProvider;

    #[async_trait::async_trait]
    impl AsyncSourceProvider for FailingSourceProvider {
        async fn get_sources(&self, _suite: &str, _component: &str) -> Result<SourceFile> {
            Err(AptRepositoryError::InvalidSourceData("broken".to_string()))
        }
    }

    fn package_provider_with(name: &str) -> AsyncMemoryPackageProvider {
        let mut provider = AsyncMemoryPackageProvider::new();
        let mut packages = PackageFile::new();
        packages.add_package(crate::packages::new_package(
            name, "1.0.0", "amd64", "test.deb", 1024,
        ));
        provider.add_packages("test", "main", "amd64", packages);
        provider
    }

    fn test_repository() -> AsyncRepository {
        AsyncRepository::new(
            RepositoryBuilder::new()
                .origin("Test")
                .suite("test")
                .architectures(vec!["amd64".to_string()])
                .components(vec!["main".to_string()])
                .build()
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn test_failed_generation_keeps_old_indices() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();
        let async_repo = test_repository();

        async_repo
            .generate_repository(
                repo_path,
                &package_provider_with("old-pkg"),
                &AsyncMemorySourceProvider::new(),
            )
            .await
            .unwrap();
        let old_packages = fs::read_to_string(repo_path.join("main/binary-amd64/Packages"))
            .await
            .unwrap();
        let old_release = fs::read_to_string(repo_path.join("Release")).await.unwrap();

        async_repo
            .generate_repository(
                repo_path,
                &package_provider_with("new-pkg"),
                &FailingSourceProvider,
            )
            .await
            .unwrap_err();

        assert_eq!(
            fs::read_to_string(repo_path.join("main/binary-amd64/Packages"))
                .await
                .unwrap(),
            old_packages
        );
        assert_eq!(
            fs::read_to_string(repo_path.join("Release")).await.unwrap(),
            old_release
        );
    }

    #[tokio::test]
    async fn test_generation_leaves_no_temporary_files() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();
        test_repository()
            .generate_repository(
                repo_path,
                &package_provider_with("test-pkg"),
                &AsyncMemorySourceProvider::new(),
            )
            .await
            .unwrap();

        let mut entries: Vec<String> = std::fs::read_dir(repo_path.join("main/binary-amd64"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            vec!["Packages", "Packages.bz2", "Packages.gz", "by-hash"]
        );
    }

    #[tokio::test]
    async fn test_generation_prunes_by_hash_files() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();
        let async_repo = AsyncRepository::new(
            RepositoryBuilder::new()
                .suite("test")
                .hash_algorithms(vec![crate::HashAlgorithm::Sha256])
                .by_hash_keep(4)
                .build()
                .unwrap(),
        );

        let by_hash_dir = repo_path.join("main/binary-amd64/by-hash/SHA256");
        std::fs::create_dir_all(&by_hash_dir).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for i in 0..5 {
            let f = std::fs::File::create(by_hash_dir.join(format!("old{}", i))).unwrap();
            f.set_modified(old).unwrap();
        }

        async_repo
            .generate_repository(
                repo_path,
                &AsyncMemoryPackageProvider::new(),
                &AsyncMemorySourceProvider::new(),
            )
            .await
            .unwrap();

        let mut entries: Vec<String> = std::fs::read_dir(&by_hash_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        entries.sort();
        let mut expected: Vec<String> = ["Packages", "Packages.gz", "Packages.bz2"]
            .iter()
            .map(|name| {
                let data = std::fs::read(repo_path.join("main/binary-amd64").join(name)).unwrap();
                let (_, hashes) = crate::hash::hash_data(&data, &[crate::HashAlgorithm::Sha256]);
                hashes
                    .get(&crate::HashAlgorithm::Sha256)
                    .unwrap()
                    .to_string()
            })
            .collect();
        expected.sort();
        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries
                .iter()
                .filter(|e| !e.starts_with("old"))
                .cloned()
                .collect::<Vec<_>>(),
            expected
        );
    }
}
