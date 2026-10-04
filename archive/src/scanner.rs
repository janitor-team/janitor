//! Downloads build artifacts, runs `dpkg-scanpackages`/`dpkg-scansources`
//! over them, and rewrites `Filename`/`Directory` entries to the archive's
//! pool layout.

use deb822_fast::convert::FromDeb822Paragraph;
use deb822_fast::Deb822;
use debian_control::lossy::apt::{Package, Source};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use crate::error::{ArchiveError, ArchiveResult};
use janitor::artifacts::{get_artifact_manager, ArtifactManager};

/// Build information retrieved from database.
///
/// Field notes:
///   * `id` -- synthetic "<run_id>/<source>" identifier
///   * `run_id` -- actual `debian_build.run_id`, used for artifact
///     retrieval and pool-path construction
///   * `codebase` -- codebase name from `run.codebase`
///   * `source_package` -- `debian_build.source`. The pool layout
///     is `<suite>/pkg/<source_package>/<run_id>/`, keyed on the
///     *source* package name, not the codebase. Keeping both fields
///     lets consumers pick the right identifier for the operation
///     at hand.
///   * `suite` -- `debian_build.distribution` (the build target
///     distribution, i.e. `campaign_config.debian_build.build_distribution`),
///     used verbatim as the leading path segment of the pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct BuildInfo {
    pub id: String,
    pub run_id: String,
    pub codebase: String,
    pub source_package: String,
    pub suite: String,
    pub architecture: String,
    pub component: String,
    pub binary_files: Vec<String>,
    pub source_files: Vec<String>,
}

/// Retrieves build artifacts into a fresh tempdir, runs
/// `dpkg-scanpackages`/`dpkg-scansources` over them, and rewrites
/// `Filename`/`Directory` entries into the archive's pool layout
/// (`<suite>/pkg/<package>/<run_id>/`).
///
/// Each retrieval uses a fresh tempdir so re-running a scan
/// re-downloads its artifacts. Sharing one tempdir would leak
/// stale artifacts across runs.
pub struct PackageScanner {
    artifact_manager: Arc<dyn ArtifactManager>,
    // Root directory that owns per-build subdirs. Kept for the
    // scanner's lifetime so the whole tree gets cleaned up when
    // the process exits, even if individual subdirs escape via
    // symlinks. Individual retrievals still get their own subdir
    // that's overwritten on subsequent calls.
    _root_temp: TempDir,
    root_path: PathBuf,
    // On-disk cache for pre-parse scan output, keyed by
    // (arch, run_id). Layout: `<cache>/binary-<arch>/<run_id>`
    // and `<cache>/source/<run_id>`.
    cache_directory: Option<PathBuf>,
}

impl PackageScanner {
    /// Create a new package scanner from an artifact-manager URL.
    ///
    /// The URL is passed straight to `get_artifact_manager`, so any
    /// supported scheme works (`local://...`, `gs://...`, etc.).
    pub async fn new(artifact_location: &str) -> ArchiveResult<Self> {
        Self::with_cache(artifact_location, None).await
    }

    /// Like [`Self::new`], but additionally caches per-run scan
    /// output under `cache_directory`. Callers should pass the
    /// value of the `--cache-directory` CLI flag.
    pub async fn with_cache(
        artifact_location: &str,
        cache_directory: Option<PathBuf>,
    ) -> ArchiveResult<Self> {
        let artifact_manager = get_artifact_manager(artifact_location)
            .await
            .map_err(|e| ArchiveError::ArtifactRetrieval(e.to_string()))?;
        let temp_dir = tempfile::Builder::new()
            .prefix(crate::TMP_PREFIX)
            .tempdir()
            .map_err(ArchiveError::Io)?;
        let root_path = temp_dir.path().to_path_buf();
        if let Some(ref cache) = cache_directory {
            tokio::fs::create_dir_all(cache)
                .await
                .map_err(ArchiveError::Io)?;
        }

        Ok(Self {
            artifact_manager: Arc::from(artifact_manager),
            _root_temp: temp_dir,
            root_path,
            cache_directory,
        })
    }

    /// Scan packages for a specific build, downloading artifacts as needed.
    ///
    /// The returned stream yields `Package` values with their
    /// `Filename` field rewritten to the pool location
    /// `<suite_name>/pkg/<package>/<run_id>/<basename>`.
    pub async fn scan_packages_for_build<'a>(
        &'a self,
        build_info: &BuildInfo,
        arch: Option<&'a str>,
    ) -> impl Stream<Item = ArchiveResult<Package>> + 'a {
        let run_id = build_info.run_id.clone();
        let suite_name = build_info.suite.clone();
        let source_package = build_info.source_package.clone();

        async_stream::try_stream! {
            let raw = self.load_or_scan_packages(&run_id, arch).await?;
            let packages = parse_packages_bytes(&raw)?;
            for mut package in packages.into_iter() {
                let basename = package
                    .filename
                    .as_deref()
                    .and_then(|f| std::path::Path::new(f).file_name().map(|n| n.to_string_lossy().into_owned()))
                    // Fall back to the .deb naming convention if
                    // dpkg-scanpackages didn't emit a filename
                    // (shouldn't happen in practice).
                    .unwrap_or_else(|| format!("{}_{}_{}.deb", package.name, package.version, package.architecture));
                package.filename = Some(pool_filename(&suite_name, &source_package, &run_id, &basename));
                yield package;
            }
        }
    }

    /// Extract per-`.deb` file listings for a build, keyed by
    /// binary package name. Used by the Contents-<arch> generator
    /// (see [`crate::contents`]). Downloads the build's artifacts
    /// into a fresh tempdir, iterates every `.deb` whose filename
    /// matches the requested architecture (or `all`, which is
    /// installable on any arch), and returns
    /// `(package_name, file_paths)` per .deb.
    ///
    /// The package name is parsed from the `.deb` filename
    /// (`<name>_<version>_<arch>.deb`) -- this matches how
    /// dpkg-scanpackages reports it and lets us stay in-process
    /// without re-parsing package control. Callers that need the
    /// full Package metadata should combine this with
    /// [`Self::scan_packages_for_build`].
    pub async fn scan_deb_contents_for_build(
        &self,
        build_info: &BuildInfo,
        arch: &str,
    ) -> ArchiveResult<Vec<(String, Vec<String>)>> {
        let artifact_dir = self.download_build_artifacts(&build_info.run_id).await?;
        let mut out = Vec::new();
        let mut read = tokio::fs::read_dir(artifact_dir.path())
            .await
            .map_err(ArchiveError::Io)?;
        while let Some(entry) = read.next_entry().await.map_err(ArchiveError::Io)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".deb") {
                continue;
            }
            let Some(pkg_name) = deb_package_name(&name) else {
                warn!("Skipping unrecognized .deb filename: {}", name);
                continue;
            };
            let Some(pkg_arch) = deb_architecture(&name) else {
                warn!("Skipping .deb without parseable arch: {}", name);
                continue;
            };
            // Contents-<arch> lists arch-specific *and* `all`
            // packages -- the latter install on every arch, so they
            // must appear in every arch's Contents. Matches
            // dpkg-scanpackages -a<arch> behavior.
            if pkg_arch != arch && pkg_arch != "all" {
                continue;
            }
            match crate::deb::list_deb_files(&entry.path()) {
                Ok(files) => out.push((pkg_name, files)),
                Err(e) => {
                    warn!("list_deb_files({}) failed: {}", entry.path().display(), e);
                }
            }
        }
        Ok(out)
    }

    /// Retrieve raw `dpkg-scanpackages` bytes for a run, using the
    /// on-disk cache if configured. Cache entries are keyed by
    /// (arch, run_id) and hold the pre-rewrite scan output.
    async fn load_or_scan_packages(
        &self,
        run_id: &str,
        arch: Option<&str>,
    ) -> ArchiveResult<Vec<u8>> {
        if let Some(cache_path) = self.packages_cache_path(run_id, arch) {
            if let Ok(bytes) = tokio::fs::read(&cache_path).await {
                debug!(
                    "loaded scan cache for run={} arch={:?} from {:?}",
                    run_id, arch, cache_path
                );
                return Ok(bytes);
            }
        }
        let artifact_dir = self.download_build_artifacts(run_id).await?;
        let raw = run_dpkg_scanpackages(artifact_dir.path(), arch).await?;
        if let Some(cache_path) = self.packages_cache_path(run_id, arch) {
            if let Some(parent) = cache_path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    warn!("cache parent create failed for {:?}: {}", parent, e);
                }
            }
            if let Err(e) = tokio::fs::write(&cache_path, &raw).await {
                warn!("cache write failed for {:?}: {}", cache_path, e);
            }
        }
        Ok(raw)
    }

    fn packages_cache_path(&self, run_id: &str, arch: Option<&str>) -> Option<PathBuf> {
        let cache = self.cache_directory.as_ref()?;
        let arch = arch?;
        Some(cache.join(format!("binary-{}", arch)).join(run_id))
    }

    /// Scan sources for a specific build, downloading artifacts as needed.
    ///
    /// The returned stream yields `Source` values with their
    /// `Directory` field rewritten to
    /// `<suite_name>/pkg/<package>/<run_id>/`.
    pub async fn scan_sources_for_build<'a>(
        &'a self,
        build_info: &BuildInfo,
    ) -> impl Stream<Item = ArchiveResult<Source>> + 'a {
        let run_id = build_info.run_id.clone();
        let suite_name = build_info.suite.clone();
        let source_package = build_info.source_package.clone();

        async_stream::try_stream! {
            let raw = self.load_or_scan_sources(&run_id).await?;
            let sources = parse_sources_bytes(&raw)?;
            for mut source in sources.into_iter() {
                source.directory = pool_directory(&suite_name, &source_package, &run_id);
                yield source;
            }
        }
    }

    async fn load_or_scan_sources(&self, run_id: &str) -> ArchiveResult<Vec<u8>> {
        if let Some(cache_path) = self.sources_cache_path(run_id) {
            if let Ok(bytes) = tokio::fs::read(&cache_path).await {
                debug!(
                    "loaded scan cache for run={} sources from {:?}",
                    run_id, cache_path
                );
                return Ok(bytes);
            }
        }
        let artifact_dir = self.download_build_artifacts(run_id).await?;
        let raw = run_dpkg_scansources(artifact_dir.path()).await?;
        if let Some(cache_path) = self.sources_cache_path(run_id) {
            if let Some(parent) = cache_path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    warn!("cache parent create failed for {:?}: {}", parent, e);
                }
            }
            if let Err(e) = tokio::fs::write(&cache_path, &raw).await {
                warn!("cache write failed for {:?}: {}", cache_path, e);
            }
        }
        Ok(raw)
    }

    fn sources_cache_path(&self, run_id: &str) -> Option<PathBuf> {
        let cache = self.cache_directory.as_ref()?;
        Some(cache.join("source").join(run_id))
    }

    /// Download build artifacts into a fresh temporary directory.
    ///
    /// Returns a `TempDir` handle so the caller controls the
    /// lifetime. Dropping the returned handle cleans the tree;
    /// keeping it alive across `dpkg-scanpackages` invocations is
    /// required.
    async fn download_build_artifacts(&self, build_id: &str) -> ArchiveResult<TempDir> {
        let artifact_dir = tempfile::Builder::new()
            .prefix(crate::TMP_PREFIX)
            .tempdir_in(&self.root_path)
            .map_err(ArchiveError::Io)?;

        debug!(
            "Downloading artifacts for build {} to {:?}",
            build_id,
            artifact_dir.path()
        );

        // Download *all* artifacts -- no client-side filter.
        // dpkg-scan{packages,sources} then does its own filtering
        // on file extension. Filtering here would silently drop
        // sidecar files like `.buildinfo`/`.changes`/`.udeb` that
        // the debian ecosystem does need. `None` == retrieve
        // everything.
        self.artifact_manager
            .retrieve_artifacts(build_id, artifact_dir.path(), None)
            .await
            .map_err(|e| match e {
                janitor::artifacts::Error::ArtifactsMissing => ArchiveError::ArtifactsMissing {
                    build_id: build_id.to_string(),
                    message: "No artifacts found for build".to_string(),
                },
                janitor::artifacts::Error::ServiceUnavailable => ArchiveError::ArtifactRetrieval(
                    "Artifact service is currently unavailable".to_string(),
                ),
                janitor::artifacts::Error::IoError(io_err) => ArchiveError::Io(io_err),
                janitor::artifacts::Error::InvalidPath => {
                    ArchiveError::ArtifactRetrieval("Invalid artifact path".to_string())
                }
                janitor::artifacts::Error::Other(msg) => ArchiveError::ArtifactRetrieval(msg),
            })?;

        info!(
            "Downloaded artifacts for build {} to {:?}",
            build_id,
            artifact_dir.path()
        );
        Ok(artifact_dir)
    }
}

/// Build the archive-pool filename for a binary package:
/// `<suite>/pkg/<package>/<run_id>/<basename>`.
pub(crate) fn pool_filename(
    suite_name: &str,
    codebase: &str,
    run_id: &str,
    basename: &str,
) -> String {
    format!("{}/pkg/{}/{}/{}", suite_name, codebase, run_id, basename)
}

/// Build the archive-pool directory path for a source package.
pub(crate) fn pool_directory(suite_name: &str, codebase: &str, run_id: &str) -> String {
    format!("{}/pkg/{}/{}", suite_name, codebase, run_id)
}

/// Parse the package name out of a `.deb` filename.
///
/// Debian .deb filenames follow `<name>_<version>_<arch>.deb`. The
/// name portion is everything before the first underscore. Returns
/// None if the filename doesn't match that pattern (e.g. old-style
/// or unusual names) -- callers should skip such files rather than
/// misclassify them.
pub(crate) fn deb_package_name(filename: &str) -> Option<String> {
    let stem = filename.strip_suffix(".deb")?;
    // Must have at least two underscores (name_version_arch).
    let mut parts = stem.splitn(2, '_');
    let name = parts.next()?;
    // Require the remainder to contain another underscore, i.e.
    // version_arch -- otherwise this isn't a well-formed .deb name.
    parts.next()?.split_once('_')?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// Parse the architecture out of a `.deb` filename
/// (`<name>_<version>_<arch>.deb`). Same failure mode as
/// [`deb_package_name`].
pub(crate) fn deb_architecture(filename: &str) -> Option<String> {
    let stem = filename.strip_suffix(".deb")?;
    let (_, arch) = stem.rsplit_once('_')?;
    if arch.is_empty() {
        return None;
    }
    Some(arch.to_string())
}

/// Invoke `dpkg-scanpackages` against a directory and return its
/// raw stdout bytes. Kept separate from parsing so the disk cache
/// can persist the exact bytes for reuse.
async fn run_dpkg_scanpackages(td: &Path, arch: Option<&str>) -> ArchiveResult<Vec<u8>> {
    let mut args = Vec::new();
    if let Some(arch) = arch {
        args.extend(["-a", arch]);
    }

    let mut proc = Command::new("dpkg-scanpackages")
        .arg(td)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            ArchiveError::PackageScanning(format!("Failed to spawn dpkg-scanpackages: {}", e))
        })?;

    let stdout = proc
        .stdout
        .take()
        .ok_or_else(|| ArchiveError::PackageScanning("Failed to open stdout".to_string()))?;
    let stderr = proc
        .stderr
        .take()
        .ok_or_else(|| ArchiveError::PackageScanning("Failed to open stderr".to_string()))?;

    let mut stdout_reader = BufReader::new(stdout);
    let stderr_reader = BufReader::new(stderr);

    let mut buf = Vec::new();
    stdout_reader
        .read_to_end(&mut buf)
        .await
        .map_err(ArchiveError::Io)?;

    // Drain stderr in a spawned task so process exit isn't blocked
    // on the pipe.
    tokio::spawn(async move {
        let mut lines = stderr_reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.as_bytes();
            if line.starts_with(b"dpkg-scanpackages: ") {
                let line = &line[b"dpkg-scanpackages: ".len()..];
                handle_log_line(line);
            } else {
                handle_log_line(line);
            }
        }
    });

    Ok(buf)
}

/// Parse the raw output of `dpkg-scanpackages` into `Package`
/// paragraphs. Kept as a pure helper so it can be unit-tested and
/// reused from the disk-cache read path.
fn parse_packages_bytes(bytes: &[u8]) -> ArchiveResult<Vec<Package>> {
    let paragraphs = Deb822::from_reader(bytes)
        .map_err(|e| ArchiveError::PackageScanning(format!("Failed to parse deb822: {}", e)))?;
    paragraphs
        .into_iter()
        .map(|p| Package::from_paragraph(&p))
        .collect::<Result<Vec<Package>, _>>()
        .map_err(|e| ArchiveError::PackageScanning(format!("Failed to parse package: {}", e)))
}

/// Scan binary packages in a directory. Convenience wrapper for
/// callers (e.g. tests) that want a parsed result rather than
/// raw bytes.
#[cfg(test)]
async fn scan_packages_in_directory(td: &Path, arch: Option<&str>) -> ArchiveResult<Vec<Package>> {
    let bytes = run_dpkg_scanpackages(td, arch).await?;
    parse_packages_bytes(&bytes)
}

/// Invoke `dpkg-scansources` against a directory and return its
/// raw stdout bytes. Separated from parsing for the same
/// disk-caching reason as `run_dpkg_scanpackages`.
async fn run_dpkg_scansources(td: &Path) -> ArchiveResult<Vec<u8>> {
    let mut proc = Command::new("dpkg-scansources")
        .arg(td)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            ArchiveError::SourceScanning(format!("Failed to spawn dpkg-scansources: {}", e))
        })?;

    let stdout = proc
        .stdout
        .take()
        .ok_or_else(|| ArchiveError::SourceScanning("Failed to open stdout".to_string()))?;
    let stderr = proc
        .stderr
        .take()
        .ok_or_else(|| ArchiveError::SourceScanning("Failed to open stderr".to_string()))?;

    let mut stdout_reader = BufReader::new(stdout);
    let stderr_reader = BufReader::new(stderr);

    let mut buf = Vec::new();
    stdout_reader
        .read_to_end(&mut buf)
        .await
        .map_err(ArchiveError::Io)?;

    tokio::spawn(async move {
        let mut lines = stderr_reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.as_bytes();
            if line.starts_with(b"dpkg-scansources: ") {
                let line = &line[b"dpkg-scansources: ".len()..];
                handle_log_line(line);
            } else {
                handle_log_line(line);
            }
        }
    });

    Ok(buf)
}

fn parse_sources_bytes(bytes: &[u8]) -> ArchiveResult<Vec<Source>> {
    let paragraphs = Deb822::from_reader(bytes)
        .map_err(|e| ArchiveError::SourceScanning(format!("Failed to parse deb822: {}", e)))?;
    paragraphs
        .into_iter()
        .map(|p| Source::from_paragraph(&p))
        .collect::<Result<Vec<Source>, _>>()
        .map_err(|e| ArchiveError::SourceScanning(format!("Failed to parse source: {}", e)))
}

#[cfg(test)]
async fn scan_sources_in_directory(td: &Path) -> ArchiveResult<Vec<Source>> {
    let bytes = run_dpkg_scansources(td).await?;
    parse_sources_bytes(&bytes)
}

/// Handle a log line from the scanner process.
///
/// # Arguments
/// * `line` - The log line as bytes
fn handle_log_line(line: &[u8]) {
    if line.starts_with(b"info: ") {
        debug!("{}", String::from_utf8_lossy(&line[b"info: ".len()..]));
    } else if line.starts_with(b"warning: ") {
        warn!("{}", String::from_utf8_lossy(&line[b"warning: ".len()..]));
    } else if line.starts_with(b"error: ") {
        error!("{}", String::from_utf8_lossy(&line[b"error: ".len()..]));
    } else {
        info!("dpkg error: {}", String::from_utf8_lossy(line));
    }
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn test_scan_packages() {
        let test_dir = std::path::Path::new("tests/data");
        let packages = super::scan_packages_in_directory(test_dir, None)
            .await
            .unwrap();

        assert_eq!(packages.len(), 1);

        let package = &packages[0];

        assert_eq!(package.name, "hello");
        assert_eq!(package.version, "2.10-3".parse().unwrap());
    }

    #[tokio::test]
    async fn test_scan_sources() {
        let test_dir = std::path::Path::new("tests/data");
        let sources = super::scan_sources_in_directory(test_dir).await.unwrap();

        assert_eq!(sources.len(), 1);

        let source = &sources[0];

        assert_eq!(source.package, "hello");
        assert_eq!(source.version, "2.10-3".parse().unwrap());
    }

    /// Apt clients follow the `Filename:` field verbatim, so the
    /// pool-filename layout must be exactly
    /// `<suite>/pkg/<package>/<run_id>/<basename>`. Any drift
    /// breaks `/pool/...` fetches.
    #[test]
    fn pool_filename_layout_is_stable() {
        let got = super::pool_filename("unstable", "hello", "abc-123", "hello_2.10-3_amd64.deb");
        assert_eq!(got, "unstable/pkg/hello/abc-123/hello_2.10-3_amd64.deb");
    }

    /// Pool directory layout for sources:
    /// `<suite>/pkg/<package>/<run_id>`.
    #[test]
    fn pool_directory_layout_is_stable() {
        let got = super::pool_directory("unstable", "hello", "abc-123");
        assert_eq!(got, "unstable/pkg/hello/abc-123");
    }

    /// Disk cache layout: `<cache>/binary-<arch>/<run_id>` for
    /// packages and `<cache>/source/<run_id>` for sources.
    /// Deployments that share a cache across scanner instances
    /// rely on this layout.
    #[tokio::test]
    async fn scanner_disk_cache_paths_are_stable() {
        let cache = tempfile::tempdir().unwrap();
        let scanner =
            super::PackageScanner::with_cache("local://", Some(cache.path().to_path_buf()))
                .await
                .unwrap();

        let expected_pkg = cache.path().join("binary-amd64").join("run-1");
        assert_eq!(
            scanner.packages_cache_path("run-1", Some("amd64")).unwrap(),
            expected_pkg
        );

        let expected_src = cache.path().join("source").join("run-1");
        assert_eq!(scanner.sources_cache_path("run-1").unwrap(), expected_src);
    }

    /// Without a cache directory the cache-path helpers return
    /// None -- the load_or_scan path treats that as "always
    /// re-scan".
    #[tokio::test]
    async fn scanner_no_cache_returns_none_paths() {
        let scanner = super::PackageScanner::new("local://").await.unwrap();
        assert!(scanner
            .packages_cache_path("run-1", Some("amd64"))
            .is_none());
        assert!(scanner.sources_cache_path("run-1").is_none());
    }

    /// When no arch is supplied, packages_cache_path must return
    /// None too -- entries are keyed by (binary-<arch>, run_id)
    /// and there's no useful key without an arch, so scans must
    /// be regenerated rather than cached.
    #[tokio::test]
    async fn scanner_cache_needs_arch_for_packages() {
        let cache = tempfile::tempdir().unwrap();
        let scanner =
            super::PackageScanner::with_cache("local://", Some(cache.path().to_path_buf()))
                .await
                .unwrap();
        assert!(scanner.packages_cache_path("run-1", None).is_none());
    }

    /// Suite/source names may contain dashes; the layout must
    /// pass them through unchanged (no encoding). Regression
    /// guard against a future "sanitize" helper.
    #[test]
    fn pool_filename_preserves_dashes_and_dots() {
        let got = super::pool_filename(
            "lintian-fixes",
            "libpackage-name",
            "run-2026-01-01",
            "libpackage-name_1.0.0-1_amd64.deb",
        );
        assert_eq!(
            got,
            "lintian-fixes/pkg/libpackage-name/run-2026-01-01/libpackage-name_1.0.0-1_amd64.deb"
        );
    }

    /// `.deb` filename parsing: standard three-part
    /// `<name>_<version>_<arch>.deb` yields the package name.
    /// Contents generation depends on this to associate each .deb
    /// with its source package.
    #[test]
    fn deb_package_name_parses_standard_filename() {
        assert_eq!(
            super::deb_package_name("hello_2.10-3_amd64.deb").as_deref(),
            Some("hello")
        );
        assert_eq!(
            super::deb_package_name("libc6-dev_2.36-9_arm64.deb").as_deref(),
            Some("libc6-dev")
        );
    }

    /// Malformed filenames (no arch or no version separator) must
    /// return None so the caller can skip them rather than
    /// synthesizing wrong package names.
    #[test]
    fn deb_package_name_rejects_malformed_names() {
        assert_eq!(super::deb_package_name("not-a-deb.txt"), None);
        assert_eq!(super::deb_package_name("hello.deb"), None); // no _
        assert_eq!(super::deb_package_name("hello_2.10.deb"), None); // no arch
        assert_eq!(super::deb_package_name("").as_deref(), None);
    }

    /// Architecture must be the trailing token before `.deb`.
    #[test]
    fn deb_architecture_parses_standard_filename() {
        assert_eq!(
            super::deb_architecture("hello_2.10-3_amd64.deb").as_deref(),
            Some("amd64")
        );
        assert_eq!(
            super::deb_architecture("hello_1.0_all.deb").as_deref(),
            Some("all")
        );
        assert_eq!(
            super::deb_architecture("libpkg_1.0_arm64.deb").as_deref(),
            Some("arm64")
        );
    }

    /// Malformed arch: no underscore, empty arch, missing .deb
    /// suffix.
    #[test]
    fn deb_architecture_rejects_malformed_names() {
        assert_eq!(super::deb_architecture("hello.deb"), None);
        assert_eq!(super::deb_architecture("hello_.deb"), None);
        assert_eq!(super::deb_architecture("not.a.deb.file"), None);
    }

    /// Build a tiny `.deb` at `path` with a single arch-native file
    /// entry. Shares the same in-memory ar+tar recipe as the deb
    /// module tests but exposed here so scanner-level tests can
    /// exercise the full artifact-download -> contents-extraction
    /// path.
    fn write_mini_deb(path: &std::path::Path, files: &[&str]) {
        use std::io::Write;
        let mut tar_bytes: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for f in files {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_path(f).unwrap();
                header.set_size(0);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, std::io::empty()).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        let data_bytes = gz.finish().unwrap();

        let file = std::fs::File::create(path).unwrap();
        let mut ar_builder = ar::Builder::new(file);
        ar_builder
            .append(
                &ar::Header::new(b"debian-binary".to_vec(), 4),
                b"2.0\n" as &[u8],
            )
            .unwrap();
        ar_builder
            .append(
                &ar::Header::new(b"control.tar.gz".to_vec(), 4),
                b"junk" as &[u8],
            )
            .unwrap();
        ar_builder
            .append(
                &ar::Header::new(b"data.tar.gz".to_vec(), data_bytes.len() as u64),
                data_bytes.as_slice(),
            )
            .unwrap();
    }

    /// End-to-end: seed a `local://` artifact store with a fake
    /// `.deb`, run `scan_deb_contents_for_build`, and verify we get
    /// `(package_name, file_list)` back. Contents-<arch>
    /// generation calls into this same path.
    #[tokio::test]
    async fn scan_deb_contents_returns_files_by_package() {
        use super::BuildInfo;
        let store = tempfile::tempdir().unwrap();
        let store_url = store.path().display().to_string();
        // LocalArtifactManager stores runs at <root>/<run_id>/*.
        let run_dir = store.path().join("run-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        write_mini_deb(&run_dir.join("hello_1.0_amd64.deb"), &["./usr/bin/hello"]);
        // Add an arch-mismatched .deb that should be filtered out.
        write_mini_deb(
            &run_dir.join("hello_1.0_arm64.deb"),
            &["./usr/bin/hello-arm"],
        );
        // Add an `_all` .deb that should be included for every arch.
        write_mini_deb(
            &run_dir.join("hello-data_1.0_all.deb"),
            &["./usr/share/hello-data/readme"],
        );

        let scanner = super::PackageScanner::new(&store_url).await.unwrap();
        let build = BuildInfo {
            id: "run-1/hello".to_string(),
            run_id: "run-1".to_string(),
            codebase: "hello".to_string(),
            source_package: "hello".to_string(),
            suite: "unstable".to_string(),
            architecture: "amd64".to_string(),
            component: "main".to_string(),
            binary_files: vec![],
            source_files: vec![],
        };
        let mut got = scanner
            .scan_deb_contents_for_build(&build, "amd64")
            .await
            .unwrap();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(got.len(), 2, "expected amd64 + all package; arm64 filtered");
        assert_eq!(got[0].0, "hello");
        assert_eq!(got[0].1, vec!["usr/bin/hello"]);
        assert_eq!(got[1].0, "hello-data");
        assert_eq!(got[1].1, vec!["usr/share/hello-data/readme"]);
    }

    /// A build whose artifact store is missing must surface a typed
    /// ArtifactsMissing error. Contents generation catches this and
    /// skips the build; the test guards the error type so that
    /// catch doesn't accidentally swallow other error kinds.
    #[tokio::test]
    async fn scan_deb_contents_missing_run_errors() {
        use super::BuildInfo;
        let store = tempfile::tempdir().unwrap();
        let store_url = store.path().display().to_string();
        let scanner = super::PackageScanner::new(&store_url).await.unwrap();
        let build = BuildInfo {
            id: "run-missing/hello".to_string(),
            run_id: "run-missing".to_string(),
            codebase: "hello".to_string(),
            source_package: "hello".to_string(),
            suite: "unstable".to_string(),
            architecture: "amd64".to_string(),
            component: "main".to_string(),
            binary_files: vec![],
            source_files: vec![],
        };
        let err = scanner
            .scan_deb_contents_for_build(&build, "amd64")
            .await
            .unwrap_err();
        // Match the ArchiveError::ArtifactsMissing variant so a
        // future refactor that renames or reclassifies it forces
        // us to update the caller too.
        assert!(
            matches!(err, crate::error::ArchiveError::ArtifactsMissing { .. }),
            "unexpected error kind: {:?}",
            err
        );
    }
}
