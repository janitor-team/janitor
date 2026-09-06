//! Writes Release, Packages, Sources, Contents into `<base_path>/`.
//!
//! Coordinates the scanner (which downloads artifacts and runs
//! dpkg-scan*) with the apt-repository crate (which formats and hashes
//! the resulting index files). Splices Contents entries into Release
//! after the fact, since apt-repository doesn't know about Contents.

use std::collections::HashMap;
use std::sync::Arc;

use apt_repository::{AsyncRepository, Compression, HashAlgorithm, RepositoryBuilder};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{error, info, warn};

use crate::config::AptRepositoryConfig;
use crate::database::BuildManager;
use crate::error::{ArchiveError, ArchiveResult};
use crate::scanner::PackageScanner;

/// Serde-friendly mirror of `apt_repository::Compression`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub enum CompressionConfig {
    None,
    Gzip,
    Bzip2,
}

impl From<CompressionConfig> for Compression {
    fn from(config: CompressionConfig) -> Self {
        match config {
            CompressionConfig::None => Compression::None,
            CompressionConfig::Gzip => Compression::Gzip,
            CompressionConfig::Bzip2 => Compression::Bzip2,
        }
    }
}

/// Serde-friendly mirror of `apt_repository::HashAlgorithm`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub enum HashAlgorithmConfig {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl From<HashAlgorithmConfig> for HashAlgorithm {
    fn from(config: HashAlgorithmConfig) -> Self {
        match config {
            HashAlgorithmConfig::Md5 => HashAlgorithm::Md5,
            HashAlgorithmConfig::Sha1 => HashAlgorithm::Sha1,
            HashAlgorithmConfig::Sha256 => HashAlgorithm::Sha256,
            HashAlgorithmConfig::Sha512 => HashAlgorithm::Sha512,
        }
    }
}

/// Tuning knobs for [`RepositoryGenerator`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct RepositoryGenerationConfig {
    pub by_hash: bool,
    pub compressions: Vec<CompressionConfig>,
    pub hash_algorithms: Vec<HashAlgorithmConfig>,
    pub max_concurrent: usize,
    pub enable_signing: bool,
}

impl Default for RepositoryGenerationConfig {
    fn default() -> Self {
        Self {
            by_hash: true,
            compressions: vec![
                CompressionConfig::None,
                CompressionConfig::Gzip,
                CompressionConfig::Bzip2,
            ],
            hash_algorithms: vec![
                HashAlgorithmConfig::Md5,
                HashAlgorithmConfig::Sha1,
                HashAlgorithmConfig::Sha256,
                HashAlgorithmConfig::Sha512,
            ],
            max_concurrent: 4,
            enable_signing: false,
        }
    }
}

/// Repository generation engine for the archive service.
pub struct RepositoryGenerator {
    scanner: Arc<PackageScanner>,
    build_manager: Arc<BuildManager>,
    config: RepositoryGenerationConfig,
    /// Optional GPG configuration. When present, every successfully
    /// generated Release file is signed into Release.gpg + InRelease
    /// via [`crate::sign::sign_release`].
    gpg: Option<crate::config::GpgConfig>,
    /// Loaded protobuf `janitor.conf`. Needed to translate an
    /// `apt_repository` into its underlying set of
    /// `debian_build.distribution` build filters via
    /// `apt_repository.select -> campaign_config.debian_build.build_distribution`.
    /// Optional so callers that only use the on-demand path (which
    /// has its own build resolver) can still construct a generator.
    runtime_config: Option<Arc<janitor::config::Config>>,
}

/// Gzip-compress `bytes` using the default compression level.
/// Used for `Contents-<arch>.gz` sidecar files.
fn gzip_bytes(bytes: &[u8]) -> ArchiveResult<Vec<u8>> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(bytes)
        .map_err(|e| ArchiveError::RepositoryGeneration(format!("gzip write: {}", e)))?;
    encoder
        .finish()
        .map_err(|e| ArchiveError::RepositoryGeneration(format!("gzip finish: {}", e)))
}

/// Write `bytes` under `<component_dir>/by-hash/<algo>/<hash>` for
/// every algorithm in `algos`. Matches the layout apt-repository
/// creates for Packages / Sources so `Acquire-By-Hash: yes`
/// clients can fetch Contents by hash too.
async fn write_by_hash(
    component_dir: &std::path::Path,
    bytes: &[u8],
    algos: &[apt_repository::HashAlgorithm],
) -> ArchiveResult<()> {
    let (_, hashes) = apt_repository::hash::hash_data(bytes, algos);
    for algo in algos {
        if let Some(hex) = hashes.get(algo) {
            let dir = component_dir.join("by-hash").join(algo.as_str());
            fs::create_dir_all(&dir).await.map_err(ArchiveError::Io)?;
            fs::write(dir.join(hex), bytes)
                .await
                .map_err(ArchiveError::Io)?;
        }
    }
    Ok(())
}

/// Resolve the set of `debian_build.distribution` values that
/// feed a given apt_repository. Free function so the mapping can
/// be unit-tested without constructing a full RepositoryGenerator.
///
/// Falls back to `[repo_config.suite]` when no runtime_config is
/// available or the apt_repository has no `select` entries -- same
/// behavior as the manager's fan-out map.
pub(crate) fn build_distributions_for(
    runtime_config: Option<&janitor::config::Config>,
    repo_config: &AptRepositoryConfig,
) -> Vec<String> {
    let Some(rt) = runtime_config else {
        return vec![repo_config.suite.clone()];
    };
    let Some(apt_repo) = rt
        .apt_repository
        .iter()
        .find(|r| r.name.as_deref() == Some(&repo_config.name))
    else {
        return vec![repo_config.suite.clone()];
    };
    let mut out = Vec::new();
    for select in &apt_repo.select {
        let Some(name) = select.campaign.as_deref() else {
            continue;
        };
        if let Some(campaign) = rt.get_campaign(name) {
            if campaign.has_debian_build() {
                if let Some(dist) = campaign.debian_build().build_distribution.as_deref() {
                    out.push(dist.to_string());
                    continue;
                }
            }
            // Campaign known but no explicit build_distribution:
            // fall back to the campaign name.
            out.push(name.to_string());
        }
    }
    if out.is_empty() {
        out.push(repo_config.suite.clone());
    }
    out
}

impl RepositoryGenerator {
    /// Create a new repository generator without GPG signing.
    pub fn new(
        scanner: Arc<PackageScanner>,
        build_manager: Arc<BuildManager>,
        config: RepositoryGenerationConfig,
    ) -> Self {
        Self {
            scanner,
            build_manager,
            config,
            gpg: None,
            runtime_config: None,
        }
    }

    /// Create a new repository generator that signs Release with GPG.
    pub fn with_gpg(
        scanner: Arc<PackageScanner>,
        build_manager: Arc<BuildManager>,
        config: RepositoryGenerationConfig,
        gpg: crate::config::GpgConfig,
    ) -> Self {
        Self {
            scanner,
            build_manager,
            config,
            gpg: Some(gpg),
            runtime_config: None,
        }
    }

    /// Attach a runtime `janitor.conf` so the generator can resolve
    /// `apt_repository.select` entries to their target
    /// `debian_build.distribution` values. Without this the generator
    /// falls back to querying by the apt_repository's own name,
    /// which only works when name == build_distribution.
    pub fn with_runtime_config(mut self, cfg: Arc<janitor::config::Config>) -> Self {
        self.runtime_config = Some(cfg);
        self
    }

    /// Collect the list of `debian_build.distribution` values that
    /// feed a given apt_repository, using the loaded janitor.conf to
    /// walk `apt_repository.select[*] -> campaign.debian_build
    /// .build_distribution`. Falls back to a name-based query using
    /// `[repo_config.suite]` when no runtime config is available.
    fn build_distributions_for(&self, repo_config: &AptRepositoryConfig) -> Vec<String> {
        build_distributions_for(self.runtime_config.as_deref(), repo_config)
    }

    /// Load every `BuildRecord` that will feed this apt_repository
    /// by extending `builds` from `get_builds_for_suite(...)` per
    /// select entry.
    async fn collect_builds_for(
        &self,
        repo_config: &AptRepositoryConfig,
    ) -> ArchiveResult<Vec<crate::database::BuildRecord>> {
        let mut all = Vec::new();
        for dist in self.build_distributions_for(repo_config) {
            let mut chunk = self.build_manager.get_builds_for_suite(&dist).await?;
            all.append(&mut chunk);
        }
        Ok(all)
    }

    /// Generate an APT repository for the given configuration.
    pub async fn generate_repository(
        &self,
        repo_config: &AptRepositoryConfig,
    ) -> ArchiveResult<()> {
        info!("Generating repository: {}", repo_config.name);

        // Create the repository builder. RepositoryBuilder defaults
        // not_automatic=true and but_automatic_upgrades=true. Set
        // Description to a fixed "Generated by the Janitor" string
        // so apt clients see a stable Description regardless of the
        // ArchiveConfig (the per-repo `description` is used only as
        // Label).
        let repo_builder = RepositoryBuilder::new()
            .origin(&repo_config.origin)
            .label(&repo_config.label)
            .suite(&repo_config.suite)
            .codename(&repo_config.codename)
            .architectures(repo_config.architectures.clone())
            .components(repo_config.components.clone())
            .acquire_by_hash(self.config.by_hash)
            .description("Generated by the Janitor")
            .compressions(
                self.config
                    .compressions
                    .iter()
                    .map(|c| (*c).clone().into())
                    .collect(),
            )
            .hash_algorithms(
                self.config
                    .hash_algorithms
                    .iter()
                    .map(|h| (*h).clone().into())
                    .collect(),
            );

        let repository = repo_builder
            .build()
            .map_err(|e| ArchiveError::RepositoryGeneration(e.to_string()))?;

        // Create async repository
        let async_repo = AsyncRepository::new(repository);

        // Ensure the base path exists
        fs::create_dir_all(&repo_config.base_path)
            .await
            .map_err(ArchiveError::Io)?;

        info!(
            "Generating repository files in: {:?}",
            repo_config.base_path
        );

        // Resolve the set of build_distributions for this apt_repo,
        // load them once, and hand the same fixed list to both the
        // Packages and Sources providers so they see identical input.
        let builds = self.collect_builds_for(repo_config).await?;
        let build_infos: Vec<crate::scanner::BuildInfo> =
            builds.into_iter().map(Into::into).collect();
        let package_provider = crate::on_demand::PrecomputedPackageProvider::new(
            Arc::clone(&self.scanner),
            build_infos.clone(),
        );
        let source_provider = crate::on_demand::PrecomputedSourceProvider::new(
            Arc::clone(&self.scanner),
            build_infos.clone(),
        );

        let _release = async_repo
            .generate_repository(&repo_config.base_path, &package_provider, &source_provider)
            .await
            .map_err(|e| ArchiveError::RepositoryGeneration(e.to_string()))?;

        info!("Successfully generated repository: {}", repo_config.name);

        // Prune old by-hash files. Called per (component, arch) with
        // `4 * compressions.len()` as the keep count. Without this
        // the by-hash directories grow unboundedly and eventually
        // swamp inode budgets.
        if self.config.by_hash {
            let keep_count = 4 * self.config.compressions.len().max(1);
            let base_path = &repo_config.base_path;
            for component in &repo_config.components {
                for arch in &repo_config.architectures {
                    let arch_dir = base_path.join(component).join(format!("binary-{}", arch));
                    if let Err(e) = async_repo
                        .cleanup_by_hash_files_async(&arch_dir, keep_count)
                        .await
                    {
                        warn!(
                            "cleanup_by_hash_files_async failed for {:?}: {}",
                            arch_dir, e
                        );
                    }
                }
                let source_dir = base_path.join(component).join("source");
                if let Err(e) = async_repo
                    .cleanup_by_hash_files_async(&source_dir, keep_count)
                    .await
                {
                    warn!(
                        "cleanup_by_hash_files_async failed for {:?}: {}",
                        source_dir, e
                    );
                }
            }
        }

        // Generate Contents-<arch> files and splice the new entries
        // into the Release file. Must run *before* signing so the
        // signature covers the updated Release. Done in-process by
        // parsing each .deb's data.tar directly, avoiding a
        // dpkg-deb subprocess per package.
        self.generate_contents(repo_config, &build_infos).await?;

        // Sign Release so periodic and /publish flows produce
        // Release.gpg + InRelease alongside Release.
        if let Some(gpg_cfg) = self.gpg.as_ref() {
            let release_path = repo_config.base_path.join("Release");
            match fs::read(&release_path).await {
                Ok(release_bytes) => {
                    if let Err(e) =
                        crate::sign::sign_release(&repo_config.base_path, &release_bytes, gpg_cfg)
                            .await
                    {
                        error!("Failed to sign Release for {}: {}", repo_config.name, e);
                        return Err(e);
                    }
                }
                Err(e) => {
                    error!("Cannot read {} to sign: {}", release_path.display(), e);
                    return Err(ArchiveError::Io(e));
                }
            }
        }

        Ok(())
    }

    /// Generate multiple repositories for different suites.
    pub async fn generate_repositories(
        &self,
        repos: &HashMap<String, AptRepositoryConfig>,
    ) -> ArchiveResult<()> {
        info!("Generating {} repositories", repos.len());

        let mut tasks = Vec::new();

        for (_name, repo_config) in repos {
            let generator = Self {
                scanner: Arc::clone(&self.scanner),
                build_manager: Arc::clone(&self.build_manager),
                config: self.config.clone(),
                gpg: self.gpg.clone(),
                runtime_config: self.runtime_config.clone(),
            };

            let repo_config = repo_config.clone();
            let task =
                tokio::spawn(async move { generator.generate_repository(&repo_config).await });

            tasks.push(task);

            // Limit concurrent operations
            if tasks.len() >= self.config.max_concurrent {
                // Wait for one task to complete
                let (result, _index, remaining) = futures::future::select_all(tasks).await;
                match result {
                    Ok(Ok(_)) => info!("Repository generation completed successfully"),
                    Ok(Err(e)) => error!("Repository generation failed: {}", e),
                    Err(e) => error!("Repository generation task failed: {}", e),
                }
                tasks = remaining;
            }
        }

        // Wait for remaining tasks
        for task in tasks {
            match task.await {
                Ok(Ok(_)) => info!("Repository generation completed successfully"),
                Ok(Err(e)) => error!("Repository generation failed: {}", e),
                Err(e) => error!("Repository generation task failed: {}", e),
            }
        }

        Ok(())
    }

    /// Clean up old repository files.
    pub async fn cleanup_repository(&self, repo_config: &AptRepositoryConfig) -> ArchiveResult<()> {
        info!("Cleaning up repository: {}", repo_config.name);

        let suite_path = repo_config.suite_path();

        if suite_path.exists() {
            info!("Removing existing repository files: {:?}", suite_path);
            fs::remove_dir_all(&suite_path)
                .await
                .map_err(ArchiveError::Io)?;
        }

        Ok(())
    }

    /// Generate `Contents-<arch>` (and its `.gz`) for every
    /// `(component, arch)` pair in the repo config, then append
    /// their hashed-file entries to the on-disk `Release` file so
    /// apt clients pick them up.
    ///
    /// Implementation notes:
    ///
    ///   * Contents lists arch-native *and* `all` packages --
    ///     handled inside `scan_deb_contents_for_build`, not here.
    ///   * We emit under `<base>/<component>/Contents-<arch>{,.gz}`,
    ///     the layout apt actually reads (matches `dak`).
    ///   * `.deb` file lists come from parsing `data.tar.*`
    ///     in-process (`crate::deb`), avoiding one dpkg-deb
    ///     subprocess per package (~thousands per suite).
    ///   * Errors from a single .deb are logged and skipped -- a
    ///     malformed package must not break Contents generation
    ///     for the rest of the suite.
    async fn generate_contents(
        &self,
        repo_config: &AptRepositoryConfig,
        build_infos: &[crate::scanner::BuildInfo],
    ) -> ArchiveResult<()> {
        use crate::contents::{format_contents, ContentsEntry};
        use apt_repository::{hash::hash_data, HashAlgorithm, HashedFile};
        use std::io::Write;

        // Map hash-algorithm config -> apt_repository enum. We
        // hash Contents with the same set the Packages file uses
        // so Release entries look consistent.
        let hash_algos: Vec<HashAlgorithm> = self
            .config
            .hash_algorithms
            .iter()
            .map(|h| (*h).clone().into())
            .collect();

        let mut new_release_entries: Vec<(String, HashedFile)> = Vec::new();

        for component in &repo_config.components {
            for arch in &repo_config.architectures {
                // `source` isn't a binary architecture -- no
                // Contents file. Everything else is fair game;
                // dak generates Contents-i386, Contents-amd64,
                // Contents-udeb-*, etc.
                if arch == "source" {
                    continue;
                }
                info!(
                    "Generating Contents-{} for {}/{}",
                    arch, repo_config.name, component
                );
                let mut entries: Vec<ContentsEntry> = Vec::new();
                for build in build_infos {
                    match self.scanner.scan_deb_contents_for_build(build, arch).await {
                        Ok(pkg_files) => {
                            for (pkg_name, files) in pkg_files {
                                if files.is_empty() {
                                    continue;
                                }
                                entries.push(ContentsEntry {
                                    // dak's Contents format uses
                                    // `<component>/<package>`.
                                    qualified_name: format!("{}/{}", component, pkg_name),
                                    files,
                                });
                            }
                        }
                        Err(e) => {
                            warn!(
                                "scan_deb_contents_for_build({}, {}) failed: {}",
                                build.id, arch, e
                            );
                        }
                    }
                }

                let body = format_contents(&entries);
                let component_dir = repo_config.base_path.join(component);
                fs::create_dir_all(&component_dir)
                    .await
                    .map_err(ArchiveError::Io)?;

                // Uncompressed
                let plain_name = format!("Contents-{}", arch);
                let plain_path = component_dir.join(&plain_name);
                fs::write(&plain_path, body.as_bytes())
                    .await
                    .map_err(ArchiveError::Io)?;
                let (plain_size, plain_hashes) = hash_data(body.as_bytes(), &hash_algos);
                let mut plain_hf =
                    HashedFile::new(format!("{}/{}", component, plain_name), plain_size);
                plain_hf.hashes = plain_hashes;
                new_release_entries.push((format!("{}/{}", component, plain_name), plain_hf));

                // Gzipped
                let gz_bytes = gzip_bytes(body.as_bytes())?;
                let gz_name = format!("Contents-{}.gz", arch);
                let gz_path = component_dir.join(&gz_name);
                fs::write(&gz_path, &gz_bytes)
                    .await
                    .map_err(ArchiveError::Io)?;
                let (gz_size, gz_hashes) = hash_data(&gz_bytes, &hash_algos);
                let mut gz_hf = HashedFile::new(format!("{}/{}", component, gz_name), gz_size);
                gz_hf.hashes = gz_hashes;
                new_release_entries.push((format!("{}/{}", component, gz_name), gz_hf));

                // by-hash symlinks/copies so `Acquire-By-Hash:
                // yes` clients can fetch stable hash URLs.
                if self.config.by_hash {
                    write_by_hash(&component_dir, body.as_bytes(), &hash_algos).await?;
                    write_by_hash(&component_dir, &gz_bytes, &hash_algos).await?;
                }
            }
        }

        // Splice the new entries into Release. AsyncRepository
        // already wrote Release; parse it, add the new files, and
        // rewrite. Doing this in-place (rather than modifying the
        // returned Release object) keeps the file on disk as the
        // single source of truth so a signing failure doesn't
        // leave the metadata inconsistent.
        let release_path = repo_config.base_path.join("Release");
        let release_bytes = fs::read(&release_path).await.map_err(ArchiveError::Io)?;
        let release_str = String::from_utf8(release_bytes).map_err(|e| {
            ArchiveError::RepositoryGeneration(format!("Release is not UTF-8: {}", e))
        })?;
        let mut release = apt_repository::Release::from_str(&release_str)
            .map_err(|e| ArchiveError::RepositoryGeneration(e.to_string()))?;
        for (_path, hf) in new_release_entries {
            release.add_file(hf);
        }
        let mut writer = std::fs::File::create(&release_path).map_err(ArchiveError::Io)?;
        writer
            .write_all(release.to_string().as_bytes())
            .map_err(ArchiveError::Io)?;

        Ok(())
    }

    /// Validate repository configuration.
    pub fn validate_config(&self, repo_config: &AptRepositoryConfig) -> ArchiveResult<()> {
        repo_config
            .validate()
            .map_err(ArchiveError::InvalidConfiguration)?;

        if repo_config.architectures.is_empty() {
            return Err(ArchiveError::InvalidConfiguration(
                "At least one architecture must be specified".to_string(),
            ));
        }

        if repo_config.components.is_empty() {
            return Err(ArchiveError::InvalidConfiguration(
                "At least one component must be specified".to_string(),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_repository_generation_config_default() {
        let config = RepositoryGenerationConfig::default();

        assert!(config.by_hash);
        assert_eq!(config.compressions.len(), 3);
        assert_eq!(config.hash_algorithms.len(), 4);
        assert_eq!(config.max_concurrent, 4);
        assert!(!config.enable_signing);
    }

    #[test]
    fn validate_config_accepts_minimal_valid_repo() {
        let temp_dir = TempDir::new().unwrap();
        let config = AptRepositoryConfig::new(
            "test-repo".to_string(),
            "test-suite".to_string(),
            vec!["amd64".to_string()],
            temp_dir.path().to_path_buf(),
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_config_rejects_empty_architectures() {
        let temp_dir = TempDir::new().unwrap();
        let mut config = AptRepositoryConfig::new(
            "test-repo".to_string(),
            "test-suite".to_string(),
            vec!["amd64".to_string()],
            temp_dir.path().to_path_buf(),
        );
        config.architectures.clear();
        assert!(config.validate().is_err());
    }

    fn make_repo(name: &str, suite: &str) -> AptRepositoryConfig {
        AptRepositoryConfig::new(
            name.to_string(),
            suite.to_string(),
            vec!["amd64".to_string()],
            std::path::PathBuf::from(format!("/tmp/{}", name)),
        )
    }

    /// With runtime_config declaring the apt_repository and a
    /// campaign with `debian_build { build_distribution: "X" }`,
    /// `build_distributions_for` returns `["X"]` -- the value that
    /// gets passed to `get_builds_for_suite`.
    #[test]
    fn build_distributions_for_reads_campaign_build_distribution() {
        let cfg = janitor::config::read_string(
            r#"
                campaign {
                    name: "lintian-fixes"
                    debian_build { build_distribution: "lintian-fixes-unstable" }
                }
                apt_repository {
                    name: "unstable"
                    select { campaign: "lintian-fixes" }
                }
            "#,
        )
        .unwrap();
        let repo = make_repo("unstable", "unstable");
        let dists = build_distributions_for(Some(&cfg), &repo);
        assert_eq!(dists, vec!["lintian-fixes-unstable"]);
    }

    /// Multiple selects fan out to multiple build_distributions,
    /// accumulating in select order.
    #[test]
    fn build_distributions_for_multiple_selects() {
        let cfg = janitor::config::read_string(
            r#"
                campaign {
                    name: "lintian-fixes"
                    debian_build { build_distribution: "lf-dist" }
                }
                campaign {
                    name: "fresh-releases"
                    debian_build { build_distribution: "fr-dist" }
                }
                apt_repository {
                    name: "unstable"
                    select { campaign: "lintian-fixes" }
                    select { campaign: "fresh-releases" }
                }
            "#,
        )
        .unwrap();
        let repo = make_repo("unstable", "unstable");
        let dists = build_distributions_for(Some(&cfg), &repo);
        assert_eq!(dists, vec!["lf-dist", "fr-dist"]);
    }

    /// No runtime_config -> fall back to repo.suite. Supports
    /// env-only deployments that don't provide a runtime config.
    #[test]
    fn build_distributions_for_no_runtime_falls_back_to_suite() {
        let repo = make_repo("unstable", "unstable-suite");
        let dists = build_distributions_for(None, &repo);
        assert_eq!(dists, vec!["unstable-suite"]);
    }

    /// Runtime config present but the apt_repository isn't declared
    /// there -> fall back to repo.suite. Guards against
    /// misconfiguration between env vars and the protobuf
    /// janitor.conf.
    #[test]
    fn build_distributions_for_missing_apt_repo_falls_back() {
        let cfg = janitor::config::read_string(r#"apt_repository { name: "other" }"#).unwrap();
        let repo = make_repo("unstable", "unstable-suite");
        let dists = build_distributions_for(Some(&cfg), &repo);
        assert_eq!(dists, vec!["unstable-suite"]);
    }

    /// Select entry naming a campaign without a `debian_build`
    /// block or `build_distribution` -> fall back to the campaign
    /// name itself. This is the defensive path called out in the
    /// helper's comment; the fallback should still yield exactly
    /// one distribution rather than dropping the select.
    #[test]
    fn build_distributions_for_campaign_without_build_distribution() {
        let cfg = janitor::config::read_string(
            r#"
                campaign { name: "generic" }
                apt_repository {
                    name: "unstable"
                    select { campaign: "generic" }
                }
            "#,
        )
        .unwrap();
        let repo = make_repo("unstable", "unstable-suite");
        let dists = build_distributions_for(Some(&cfg), &repo);
        assert_eq!(dists, vec!["generic"]);
    }

    /// `gzip_bytes` produces a valid gzip stream -- round-trip
    /// through GzDecoder yields the original bytes. Guards
    /// against a future refactor that flushes without finishing
    /// the encoder (a common gzip mistake).
    #[test]
    fn gzip_bytes_round_trips() {
        let plain = b"FILE                                                    LOCATION\nusr/bin/hello                                           main/hello\n";
        let compressed = gzip_bytes(plain).unwrap();
        // Gzip magic bytes 1f 8b -- sanity check we actually
        // produced a gzip stream, not the raw input.
        assert_eq!(&compressed[..2], &[0x1f, 0x8b]);
        let mut dec = flate2::read::GzDecoder::new(&compressed[..]);
        let mut round_trip = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut round_trip).unwrap();
        assert_eq!(round_trip, plain);
    }

    /// Empty input is still a valid gzip stream (Contents can be
    /// empty when a suite has no builds yet).
    #[test]
    fn gzip_bytes_empty_input() {
        let compressed = gzip_bytes(b"").unwrap();
        assert_eq!(&compressed[..2], &[0x1f, 0x8b]);
        let mut dec = flate2::read::GzDecoder::new(&compressed[..]);
        let mut round_trip = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut round_trip).unwrap();
        assert!(round_trip.is_empty());
    }

    /// A round-trip through apt_repository::Release: parse an
    /// existing Release, add a Contents-<arch> HashedFile, rewrite
    /// it, and confirm the new entry survives. This is the exact
    /// splice operation `generate_contents` does before signing.
    #[test]
    fn release_round_trip_preserves_added_files() {
        use apt_repository::hash::hash_data;
        use apt_repository::{HashAlgorithm, HashedFile, ReleaseBuilder};
        // Build a Release with one file, dump it, parse it back,
        // add a second file, dump, parse, verify both survive.
        let (size, hashes) = hash_data(b"pkg data", &[HashAlgorithm::Sha256]);
        let mut file1 = HashedFile::new("main/binary-amd64/Packages".to_string(), size);
        file1.hashes = hashes;
        let mut release = ReleaseBuilder::new()
            .origin("Janitor")
            .suite("unstable")
            .codename("unstable")
            .architectures(vec!["amd64".to_string()])
            .components(vec!["main".to_string()])
            .build()
            .unwrap();
        release.add_file(file1);
        let dumped = release.to_string();

        // Parse back and add a Contents file.
        let mut parsed = apt_repository::Release::from_str(&dumped).unwrap();
        let (csize, chashes) = hash_data(b"contents data", &[HashAlgorithm::Sha256]);
        let mut contents_hf = HashedFile::new("main/Contents-amd64".to_string(), csize);
        contents_hf.hashes = chashes;
        parsed.add_file(contents_hf);
        let redumped = parsed.to_string();

        // Both files must be referenced in the final Release
        // string; apt reads these paths directly.
        assert!(
            redumped.contains("main/binary-amd64/Packages"),
            "original Packages entry must survive round-trip"
        );
        assert!(
            redumped.contains("main/Contents-amd64"),
            "added Contents-amd64 must appear"
        );
    }

    /// `write_by_hash` creates one file per hash algorithm under
    /// `by-hash/<algo>/<hex>` whose contents equal the input.
    /// Deployments with `Acquire-By-Hash: yes` fetch by these hex
    /// URLs; if the file isn't there apt hard-errors.
    #[tokio::test]
    async fn write_by_hash_creates_all_algo_files() {
        use apt_repository::hash::hash_data;
        use apt_repository::HashAlgorithm;
        let tmp = tempfile::tempdir().unwrap();
        let bytes = b"hello world";
        let algos = vec![HashAlgorithm::Sha256, HashAlgorithm::Md5];
        write_by_hash(tmp.path(), bytes, &algos).await.unwrap();

        let (_, hashes) = hash_data(bytes, &algos);
        for algo in &algos {
            let hex = hashes.get(algo).unwrap();
            let p = tmp.path().join("by-hash").join(algo.as_str()).join(hex);
            let got = std::fs::read(&p).unwrap();
            assert_eq!(got, bytes);
        }
    }
}
