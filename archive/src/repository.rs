//! Writes Release, Packages, Sources, Contents into `<base_path>/`.
//!
//! Coordinates the scanner (which downloads artifacts and runs
//! dpkg-scan*) with the apt-repository crate (which formats and hashes
//! the resulting index files). Splices Contents entries into Release
//! after the fact, since apt-repository doesn't know about Contents.

use std::collections::HashMap;
use std::sync::Arc;

use apt_repository::{AsyncRepository, Compression, RepositoryBuilder};
use tokio::fs;
use tracing::{error, info, warn};

use crate::config::AptRepositoryConfig;
use crate::database::BuildManager;
use crate::error::{ArchiveError, ArchiveResult};
use crate::scanner::PackageScanner;

/// Maximum number of repositories generated concurrently by
/// [`RepositoryGenerator::generate_repositories`].
const MAX_CONCURRENT: usize = 4;

/// Number of files kept in each by-hash directory: four generations
/// of each of the three compressed variants of an index.
pub(crate) const BY_HASH_KEEP: usize = 12;

/// Repository generation engine for the archive service.
pub struct RepositoryGenerator {
    scanner: Arc<PackageScanner>,
    build_manager: Arc<BuildManager>,
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

/// Write `bytes` under `<component_dir>/by-hash/<algo>/<hash>` for
/// every algorithm in `algos`. Matches the layout apt-repository
/// creates for Packages / Sources so `Acquire-By-Hash: yes`
/// clients can fetch Contents by hash too.
async fn write_by_hash(
    component_dir: &std::path::Path,
    bytes: &[u8],
    algos: &[apt_repository::HashAlgorithm],
    hashes: &apt_repository::HashSet,
) -> ArchiveResult<()> {
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
    /// Create a new repository generator, which signs Release when
    /// `gpg` is set.
    pub fn new(
        scanner: Arc<PackageScanner>,
        build_manager: Arc<BuildManager>,
        gpg: Option<crate::config::GpgConfig>,
    ) -> Self {
        Self {
            scanner,
            build_manager,
            gpg,
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

        // RepositoryBuilder defaults not_automatic=true and
        // but_automatic_upgrades=true. Set Description to a fixed
        // "Generated by the Janitor" string so apt clients see a
        // stable Description regardless of the ArchiveConfig (the
        // per-repo `description` is used only as Label).
        let repository = RepositoryBuilder::new()
            .origin(&repo_config.origin)
            .label(&repo_config.label)
            .suite(&repo_config.suite)
            .codename(&repo_config.codename)
            .architectures(repo_config.architectures.clone())
            .components(repo_config.components.clone())
            .description("Generated by the Janitor")
            .by_hash_keep(BY_HASH_KEEP)
            .build()
            .map_err(|e| ArchiveError::RepositoryGeneration(e.to_string()))?;

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

        publish_dists(
            &repo_config.base_path,
            repository,
            &self.scanner,
            &build_infos,
            self.gpg.as_ref(),
        )
        .await?;

        info!("Successfully generated repository: {}", repo_config.name);
        Ok(())
    }

    /// Generate multiple repositories for different suites.
    pub async fn generate_repositories(
        &self,
        repos: &HashMap<String, AptRepositoryConfig>,
    ) -> ArchiveResult<()> {
        info!("Generating {} repositories", repos.len());

        let mut tasks = Vec::new();

        for repo_config in repos.values() {
            let generator = Self {
                scanner: Arc::clone(&self.scanner),
                build_manager: Arc::clone(&self.build_manager),
                gpg: self.gpg.clone(),
                runtime_config: self.runtime_config.clone(),
            };

            let repo_config = repo_config.clone();
            let task =
                tokio::spawn(async move { generator.generate_repository(&repo_config).await });

            tasks.push(task);

            // Limit concurrent operations
            if tasks.len() >= MAX_CONCURRENT {
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
}

/// Write the Packages, Sources and Contents indices for `builds`
/// and a Release file listing them under `base_path`, then sign the
/// Release file when `gpg` is set.
pub(crate) async fn publish_dists(
    base_path: &std::path::Path,
    repository: apt_repository::Repository,
    scanner: &Arc<PackageScanner>,
    builds: &[crate::scanner::BuildInfo],
    gpg: Option<&crate::config::GpgConfig>,
) -> ArchiveResult<()> {
    fs::create_dir_all(base_path)
        .await
        .map_err(ArchiveError::Io)?;

    let package_provider =
        crate::on_demand::PrecomputedPackageProvider::new(Arc::clone(scanner), builds.to_vec());
    let source_provider =
        crate::on_demand::PrecomputedSourceProvider::new(Arc::clone(scanner), builds.to_vec());
    let async_repo = AsyncRepository::new(repository);
    async_repo
        .generate_repository(base_path, &package_provider, &source_provider)
        .await
        .map_err(|e| ArchiveError::RepositoryGeneration(e.to_string()))?;

    // Contents entries are spliced into Release, so this must run
    // before signing.
    generate_contents(&async_repo, scanner, base_path, builds).await?;

    if let Some(gpg) = gpg {
        let release_bytes = fs::read(base_path.join("Release"))
            .await
            .map_err(ArchiveError::Io)?;
        crate::sign::sign_release(base_path, &release_bytes, gpg).await?;
    }
    Ok(())
}

/// Generate `Contents-<arch>` (and its `.gz`) for every
/// `(component, arch)` pair of `async_repo`, then append
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
///   * Shared by the configured suites and the on-demand
///     repositories, so both list the same Contents.
pub(crate) async fn generate_contents(
    async_repo: &AsyncRepository,
    scanner: &PackageScanner,
    base_path: &std::path::Path,
    build_infos: &[crate::scanner::BuildInfo],
) -> ArchiveResult<()> {
    use crate::contents::{format_contents, ContentsEntry};
    use apt_repository::{hash::hash_data, HashedFile};
    use std::io::Write;

    // Map hash-algorithm config -> apt_repository enum. We
    // hash Contents with the same set the Packages file uses
    // so Release entries look consistent.
    let repository = async_repo.inner();
    let hash_algos = &repository.hash_algorithms;

    let mut new_release_entries: Vec<HashedFile> = Vec::new();

    for component in &repository.components {
        for arch in &repository.architectures {
            // `source` isn't a binary architecture -- no
            // Contents file. Everything else is fair game;
            // dak generates Contents-i386, Contents-amd64,
            // Contents-udeb-*, etc.
            if arch == "source" {
                continue;
            }
            info!(
                "Generating Contents-{} for {}/{}",
                arch, repository.suite, component
            );
            let mut entries: Vec<ContentsEntry> = Vec::new();
            for build in build_infos {
                // Builds without artifacts are left out, as they
                // are from Packages; any other error fails the
                // publish rather than silently dropping packages.
                let pkg_files = match scanner.scan_deb_contents_for_build(build, arch).await {
                    Ok(pkg_files) => pkg_files,
                    Err(e @ ArchiveError::ArtifactsMissing { .. }) => {
                        warn!("Skipping contents of build {}: {}", build.id, e);
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                for (location, files) in pkg_files {
                    if files.is_empty() {
                        continue;
                    }
                    entries.push(ContentsEntry {
                        qualified_name: location,
                        files,
                    });
                }
            }

            let body = format_contents(&entries);
            let component_dir = base_path.join(component);
            fs::create_dir_all(&component_dir)
                .await
                .map_err(ArchiveError::Io)?;

            for compression in [Compression::None, Compression::Gzip] {
                let bytes = compression
                    .compress(body.as_bytes())
                    .map_err(|e| ArchiveError::RepositoryGeneration(e.to_string()))?;
                let name = format!("Contents-{}{}", arch, compression.extension());
                fs::write(component_dir.join(&name), &bytes)
                    .await
                    .map_err(ArchiveError::Io)?;
                let (size, hashes) = hash_data(&bytes, hash_algos);
                // by-hash copies so `Acquire-By-Hash: yes` clients
                // can fetch stable hash URLs.
                if repository.acquire_by_hash {
                    write_by_hash(&component_dir, &bytes, hash_algos, &hashes).await?;
                }
                let mut hf = HashedFile::new(format!("{}/{}", component, name), size);
                hf.hashes = hashes;
                new_release_entries.push(hf);
            }

            if repository.acquire_by_hash {
                // The component directory holds the Contents
                // by-hash files; prune them like the index ones.
                if let Some(keep) = repository.by_hash_keep {
                    async_repo
                        .cleanup_by_hash_files_async(&component_dir, keep)
                        .await
                        .map_err(|e| ArchiveError::RepositoryGeneration(e.to_string()))?;
                }
            }
        }
    }

    // Splice the new entries into Release. AsyncRepository
    // already wrote Release; parse it, add the new files, and
    // rewrite. Doing this in-place (rather than modifying the
    // returned Release object) keeps the file on disk as the
    // single source of truth so a signing failure doesn't
    // leave the metadata inconsistent.
    let release_path = base_path.join("Release");
    let release_bytes = fs::read(&release_path).await.map_err(ArchiveError::Io)?;
    let release_str = String::from_utf8(release_bytes)
        .map_err(|e| ArchiveError::RepositoryGeneration(format!("Release is not UTF-8: {}", e)))?;
    let mut release = apt_repository::Release::from_str(&release_str)
        .map_err(|e| ArchiveError::RepositoryGeneration(e.to_string()))?;
    for hf in new_release_entries {
        release.add_file(hf);
    }
    let mut writer = std::fs::File::create(&release_path).map_err(ArchiveError::Io)?;
    writer
        .write_all(release.to_string().as_bytes())
        .map_err(ArchiveError::Io)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (_, hashes) = hash_data(bytes, &algos);
        write_by_hash(tmp.path(), bytes, &algos, &hashes)
            .await
            .unwrap();

        for algo in &algos {
            let hex = hashes.get(algo).unwrap();
            let p = tmp.path().join("by-hash").join(algo.as_str()).join(hex);
            let got = std::fs::read(&p).unwrap();
            assert_eq!(got, bytes);
        }
    }
}
