//! On-demand apt repository generation for `/dists/{kind}/{id}/...`.
//!
//! Callers can request a dists tree for:
//!
//! - `kind="run"`: a single run's build outputs, keyed by run_id
//! - `kind="cs"`: all builds for a changeset, keyed by change_set id
//! - `kind=<campaign>`: the latest changeset's builds for the given
//!   campaign + codebase
//!
//! Each request runs `refresh_on_demand_dists`, which generates the
//! repository files under `dists_dir/{kind}/{id}/` if they are
//! missing or stale (compared to `max(finish_time)` of the
//! contributing runs), then falls through to serving the requested
//! static file.
//!
//! The generator reuses the existing `apt_repository` pipeline but
//! with a precomputed-builds-backed provider instead of the default
//! one that looks up builds by suite.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use apt_repository::{
    AptRepositoryError, AsyncPackageProvider, AsyncSourceProvider, PackageFile, Repository,
    RepositoryBuilder, Result as AptResult, SourceFile,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::StreamExt;
use janitor::{Campaign, ChangeSetId, CodebaseName, InvalidIdentifier, RunId};
use tracing::{debug, info, warn};

use crate::database::{BuildManager, BuildRecord};
use crate::error::{ArchiveError, ArchiveResult};
use crate::scanner::{BuildInfo, PackageScanner};

/// Kind of on-demand dists request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDemandKind {
    /// A single run's outputs.
    Run,
    /// A changeset's outputs.
    ChangeSet,
    /// A campaign's latest changeset for a specific codebase.
    /// The outer `kind` string carries the campaign name.
    Campaign,
}

/// Parse a kind path segment. `"run"` / `"cs"` are recognized as-is;
/// anything else is treated as a campaign name and the caller must
/// validate it against the runner config.
pub fn parse_kind(s: &str) -> OnDemandKind {
    match s {
        "run" => OnDemandKind::Run,
        "cs" => OnDemandKind::ChangeSet,
        _ => OnDemandKind::Campaign,
    }
}

/// A parsed on-demand `(kind, id)` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnDemandTarget {
    /// A single run's outputs.
    Run(RunId),
    /// A changeset's outputs.
    ChangeSet(ChangeSetId),
    /// A campaign's latest changeset for a codebase (or source package).
    Campaign(Campaign, CodebaseName),
}

impl OnDemandTarget {
    /// Parse the `kind` and `id` path segments.
    pub fn parse(kind: &str, id: &str) -> Result<Self, InvalidIdentifier> {
        Ok(match parse_kind(kind) {
            OnDemandKind::Run => Self::Run(id.parse()?),
            OnDemandKind::ChangeSet => Self::ChangeSet(id.parse()?),
            OnDemandKind::Campaign => Self::Campaign(kind.parse()?, id.parse()?),
        })
    }

    /// The `kind` path segment.
    pub fn kind(&self) -> &str {
        match self {
            Self::Run(_) => "run",
            Self::ChangeSet(_) => "cs",
            Self::Campaign(campaign, _) => campaign.as_str(),
        }
    }

    /// The `id` path segment.
    pub fn id(&self) -> &str {
        match self {
            Self::Run(run_id) => run_id.as_str(),
            Self::ChangeSet(cs) => cs.as_str(),
            Self::Campaign(_, codebase) => codebase.as_str(),
        }
    }
}

/// Resolve the components a campaign publishes into, via
/// `campaign_config.debian_build.base_distribution ->
///  distribution.component`.
///
/// Returns `None` when any link in the chain is missing so callers
/// can fall back to their configured default. Kept as a
/// `pub(crate)` helper so both this module and the web layer can
/// share the lookup.
pub(crate) fn components_for_campaign(
    cfg: &janitor::config::Config,
    campaign_name: &Campaign,
) -> Option<Vec<String>> {
    let campaign = cfg.get_campaign(campaign_name)?;
    if !campaign.has_debian_build() {
        return None;
    }
    let base = campaign.debian_build().base_distribution.as_deref()?;
    let dist = cfg.get_distribution(base)?;
    if dist.component.is_empty() {
        None
    } else {
        Some(dist.component.to_vec())
    }
}

/// Materialize a list of builds for an on-demand target.
///
/// Returns the build records, the most-recent `finish_time` across
/// all contributing runs (used to decide whether a previously
/// generated Release file is still up-to-date), and, when
/// available, the campaign name that owns the runs. Callers use the
/// campaign name to look up its base_distribution and substitute
/// the campaign's components for the fallback `["main"]`.
pub async fn lookup_builds(
    db: &BuildManager,
    target: &OnDemandTarget,
) -> ArchiveResult<(Vec<BuildRecord>, Option<DateTime<Utc>>, Option<Campaign>)> {
    match target {
        OnDemandTarget::Run(id) => {
            // Look up (suite, finish_time) for the run. The suite
            // field is used to look up `campaign_config` for
            // components; without it the caller falls back to
            // defaults. Cast finish_time to TIMESTAMPTZ so sqlx can
            // decode into DateTime<Utc> (run.finish_time is stored
            // as TIMESTAMP without zone, same pattern as the
            // ChangeSet branch below).
            let row: Option<(Campaign, Option<DateTime<Utc>>)> = sqlx::query_as(
                "SELECT suite::text AS suite, \
                        (finish_time AT TIME ZONE 'UTC') AS finish_time \
                 FROM run WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(db.pool())
            .await
            .map_err(ArchiveError::Database)?;
            let Some((campaign, max_finish_time)) = row else {
                return Err(ArchiveError::NotFound(format!("no such run: {}", id)));
            };
            let builds = db.get_builds_for_run(id).await?;
            Ok((builds, max_finish_time, Some(campaign)))
        }
        OnDemandTarget::ChangeSet(id) => {
            // Look up (campaign, max(finish_time)) by change_set,
            // then load builds for that changeset.
            let campaign: Option<Campaign> =
                sqlx::query_scalar("SELECT campaign FROM change_set WHERE id = $1")
                    .bind(id)
                    .fetch_optional(db.pool())
                    .await
                    .map_err(ArchiveError::Database)?;
            if campaign.is_none() {
                return Err(ArchiveError::NotFound(format!("no such changeset: {}", id)));
            }
            // run.finish_time is TIMESTAMP (no TZ); sqlx requires
            // TIMESTAMPTZ to decode as DateTime<Utc>. Cast at the SQL
            // boundary -- matches the same pattern in publish state's
            // Run decode and elsewhere in this codebase.
            let max_finish_time: Option<DateTime<Utc>> = sqlx::query_scalar(
                "SELECT (max(finish_time) AT TIME ZONE 'UTC') FROM run WHERE change_set = $1",
            )
            .bind(id)
            .fetch_optional(db.pool())
            .await
            .map_err(ArchiveError::Database)?
            .flatten();
            let builds = db.get_builds_for_changeset(id).await?;
            Ok((builds, max_finish_time, campaign))
        }
        OnDemandTarget::Campaign(campaign, id) => {
            // Look up the most-recent
            // working/ready/publishing/done change_set for that
            // pair, then load builds for that changeset.
            let cs_id: Option<ChangeSetId> = sqlx::query_scalar(
                "SELECT run.change_set \
                 FROM run \
                 INNER JOIN change_set ON change_set.id = run.change_set \
                 WHERE run.suite = $1 AND run.codebase = $2 \
                   AND change_set.state IN ('working', 'ready', 'publishing', 'done') \
                   AND run.result_code = 'success' \
                 ORDER BY run.finish_time DESC \
                 LIMIT 1",
            )
            .bind(campaign)
            .bind(id)
            .fetch_optional(db.pool())
            .await
            .map_err(ArchiveError::Database)?;
            let cs_id = match cs_id {
                Some(cs) => cs,
                None => {
                    // Fall back to checking debian_build for the
                    // source package before returning 404.
                    let has_build: Option<i32> =
                        sqlx::query_scalar("SELECT 1 FROM debian_build WHERE source = $1 LIMIT 1")
                            .bind(id)
                            .fetch_optional(db.pool())
                            .await
                            .map_err(ArchiveError::Database)?;
                    if has_build.is_none() {
                        return Err(ArchiveError::NotFound(format!(
                            "No such source package: {}",
                            id
                        )));
                    }
                    return Ok((Vec::new(), None, Some(campaign.clone())));
                }
            };
            let max_finish_time: Option<DateTime<Utc>> = sqlx::query_scalar(
                "SELECT (max(finish_time) AT TIME ZONE 'UTC') FROM run WHERE change_set = $1",
            )
            .bind(&cs_id)
            .fetch_optional(db.pool())
            .await
            .map_err(ArchiveError::Database)?
            .flatten();
            let builds = db.get_builds_for_changeset(&cs_id).await?;
            Ok((builds, max_finish_time, Some(campaign.clone())))
        }
    }
}

/// Decide whether an already-generated Release file is up-to-date
/// relative to `max_finish_time`. Returns `true` if we can skip
/// regeneration -- Release exists, has an mtime, and that mtime is
/// newer than the newest build's finish time.
pub async fn is_fresh(release_path: &Path, max_finish_time: Option<DateTime<Utc>>) -> bool {
    let Some(max_finish_time) = max_finish_time else {
        return false;
    };
    let Ok(metadata) = tokio::fs::metadata(release_path).await else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let stamp: DateTime<Utc> = modified.into();
    max_finish_time < stamp
}

/// Package provider backed by a precomputed list of `BuildInfo`,
/// plus the scanner that downloads & parses the artifacts. Unlike
/// [`crate::repository::ArchivePackageProvider`], this does not
/// re-query the database by suite at each call -- the suite argument
/// is ignored in favor of the fixed builds list.
pub struct PrecomputedPackageProvider {
    scanner: Arc<PackageScanner>,
    builds: Vec<BuildInfo>,
}

impl PrecomputedPackageProvider {
    #[allow(missing_docs)]
    pub fn new(scanner: Arc<PackageScanner>, builds: Vec<BuildInfo>) -> Self {
        Self { scanner, builds }
    }
}

#[async_trait]
impl AsyncPackageProvider for PrecomputedPackageProvider {
    async fn get_packages(
        &self,
        _suite: &str,
        _component: &str,
        architecture: &str,
    ) -> AptResult<PackageFile> {
        let mut file = PackageFile::new();
        for build in &self.builds {
            let stream = self
                .scanner
                .scan_packages_for_build(build, Some(architecture))
                .await;
            let mut stream = Box::pin(stream);
            while let Some(result) = stream.next().await {
                match result {
                    Ok(pkg) => file.add_package(pkg),
                    Err(e) => {
                        warn!("Failed to scan package from build {}: {}", build.id, e);
                    }
                }
            }
        }
        Ok(file)
    }
}

/// Source provider backed by a precomputed list of `BuildInfo`.
pub struct PrecomputedSourceProvider {
    scanner: Arc<PackageScanner>,
    builds: Vec<BuildInfo>,
}

impl PrecomputedSourceProvider {
    #[allow(missing_docs)]
    pub fn new(scanner: Arc<PackageScanner>, builds: Vec<BuildInfo>) -> Self {
        Self { scanner, builds }
    }
}

#[async_trait]
impl AsyncSourceProvider for PrecomputedSourceProvider {
    async fn get_sources(&self, _suite: &str, _component: &str) -> AptResult<SourceFile> {
        let mut file = SourceFile::new();
        for build in &self.builds {
            let stream = self.scanner.scan_sources_for_build(build).await;
            let mut stream = Box::pin(stream);
            while let Some(result) = stream.next().await {
                match result {
                    Ok(src) => file.add_source(src),
                    Err(e) => {
                        warn!("Failed to scan source from build {}: {}", build.id, e);
                    }
                }
            }
        }
        Ok(file)
    }
}

/// Build the `Repository` used for an on-demand dists tree.
fn on_demand_repository(
    origin: &str,
    description: &str,
    suite_name: &str,
    arches: &[String],
    components: Vec<String>,
) -> ArchiveResult<Repository> {
    RepositoryBuilder::new()
        .origin(origin)
        .label(description)
        .suite(suite_name)
        .codename(suite_name)
        .architectures(arches.to_vec())
        .components(components)
        .by_hash_keep(crate::repository::BY_HASH_KEEP)
        .description(description)
        .build()
        .map_err(|e: AptRepositoryError| ArchiveError::RepositoryGeneration(e.to_string()))
}

/// Refresh (or lazily generate) the dists tree for `target` under
/// `dists_dir/{kind}/{id}`. Returns `Ok(())` even if nothing
/// needed to be regenerated -- the caller is expected to then serve
/// the requested static file from the populated directory.
///
/// `components`/`arches` are the *fallback* used when the campaign
/// lookup fails (e.g. an env-only deployment with no runtime
/// config). When `runtime_config` is provided and the campaign is
/// known, the components list is derived from
/// `campaign.debian_build.base_distribution -> distribution.component`.
pub async fn refresh_on_demand_dists(
    dists_dir: &Path,
    db: &BuildManager,
    scanner: Arc<PackageScanner>,
    origin: &str,
    components: &[String],
    arches: &[String],
    target: &OnDemandTarget,
    gpg: Option<&crate::config::GpgConfig>,
    runtime_config: Option<&janitor::config::Config>,
) -> ArchiveResult<()> {
    let (kind, id) = (target.kind(), target.id());
    let base_path = dists_dir.join(kind).join(id);
    let release_path = base_path.join("Release");

    let (builds, max_finish_time, campaign_name) = lookup_builds(db, target).await?;

    if is_fresh(&release_path, max_finish_time).await {
        debug!("On-demand dists for {}/{} still fresh, skipping", kind, id);
        return Ok(());
    }

    info!("Generating on-demand dists for {}/{}", kind, id);

    let suite_name = format!("{}/{}", kind, id);
    let description = match target {
        OnDemandTarget::Run(_) => format!("Run {}", id),
        OnDemandTarget::ChangeSet(_) => format!("Change set {}", id),
        OnDemandTarget::Campaign(..) => format!("Campaign {} for {}", kind, id),
    };

    // Prefer the campaign's target distribution components when we
    // can resolve them. Falls back to the caller-supplied
    // components list otherwise.
    let resolved_components: Vec<String> = campaign_name
        .as_ref()
        .and_then(|name| runtime_config.and_then(|rt| components_for_campaign(rt, name)))
        .unwrap_or_else(|| components.to_vec());

    let build_infos: Vec<BuildInfo> = builds.into_iter().map(Into::into).collect();
    let repository = on_demand_repository(
        origin,
        &description,
        &suite_name,
        arches,
        resolved_components,
    )?;
    crate::repository::publish_dists(&base_path, repository, &scanner, &build_infos, gpg).await
}

/// Resolve a file path under the on-demand dists tree and read it.
/// Returns `Ok(None)` if the file doesn't exist so callers can turn
/// that into a 404.
pub async fn read_on_demand_file(base: &Path, relative: &[&str]) -> ArchiveResult<Option<Vec<u8>>> {
    let mut path: PathBuf = base.to_path_buf();
    for component in relative {
        path.push(component);
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ArchiveError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn test_parse_kind_run() {
        assert_eq!(parse_kind("run"), OnDemandKind::Run);
    }

    #[test]
    fn test_parse_kind_cs() {
        assert_eq!(parse_kind("cs"), OnDemandKind::ChangeSet);
    }

    #[test]
    fn test_parse_kind_campaign_fallback() {
        // Anything that isn't run|cs is treated as a campaign name.
        assert_eq!(parse_kind("lintian-fixes"), OnDemandKind::Campaign);
        assert_eq!(parse_kind("fresh-releases"), OnDemandKind::Campaign);
        assert_eq!(parse_kind(""), OnDemandKind::Campaign);
    }

    #[test]
    fn test_on_demand_target_parse() {
        assert_eq!(
            OnDemandTarget::parse("run", "run-1").unwrap(),
            OnDemandTarget::Run(RunId::new("run-1").unwrap())
        );
        assert_eq!(
            OnDemandTarget::parse("cs", "cs-1").unwrap(),
            OnDemandTarget::ChangeSet(ChangeSetId::new("cs-1").unwrap())
        );
        let target = OnDemandTarget::parse("lintian-fixes", "hello").unwrap();
        assert_eq!(
            target,
            OnDemandTarget::Campaign(
                Campaign::new("lintian-fixes").unwrap(),
                CodebaseName::new("hello").unwrap()
            )
        );
        assert_eq!((target.kind(), target.id()), ("lintian-fixes", "hello"));
    }

    #[test]
    fn test_on_demand_target_parse_rejects_invalid_ids() {
        assert!(OnDemandTarget::parse("run", "..").is_err());
        assert!(OnDemandTarget::parse("run", "").is_err());
        assert!(OnDemandTarget::parse("cs", "..").is_err());
        assert!(OnDemandTarget::parse("cs", "a.b").is_err());
        assert!(OnDemandTarget::parse("lintian-fixes", "..").is_err());
        assert!(OnDemandTarget::parse("Lintian", "hello").is_err());
    }

    #[tokio::test]
    async fn test_is_fresh_no_max_finish_time() {
        // When max_finish_time is None the freshness check cannot
        // succeed, so we always regenerate.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Release");
        tokio::fs::write(&path, b"anything").await.unwrap();
        assert!(!is_fresh(&path, None).await);
    }

    #[tokio::test]
    async fn test_is_fresh_missing_release_file() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("Release");
        // No file on disk, and a recent max_finish_time. Must not be
        // fresh -- otherwise the caller would skip regeneration and
        // then try to serve a nonexistent file.
        let now: DateTime<Utc> = SystemTime::now().into();
        assert!(!is_fresh(&missing, Some(now)).await);
    }

    #[tokio::test]
    async fn test_is_fresh_stamp_newer_than_builds() {
        // Release mtime newer than max_finish_time -> up-to-date.
        // Write a Release file, then call is_fresh with a
        // max_finish_time well in the past. Must be fresh.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Release");
        tokio::fs::write(&path, b"Origin: janitor\n").await.unwrap();
        let past: DateTime<Utc> = (SystemTime::now() - Duration::from_secs(3600)).into();
        assert!(is_fresh(&path, Some(past)).await);
    }

    #[tokio::test]
    async fn test_is_fresh_builds_newer_than_stamp() {
        // If max_finish_time >= stamp, regenerate. Give the Release
        // an mtime in the past and a max_finish_time in the future.
        // Must not be fresh.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Release");
        tokio::fs::write(&path, b"Origin: janitor\n").await.unwrap();
        let future: DateTime<Utc> = (SystemTime::now() + Duration::from_secs(3600)).into();
        assert!(!is_fresh(&path, Some(future)).await);
    }

    /// Like Python's write_suite_files, keep only the newest 12 files
    /// in each by-hash directory.
    #[tokio::test]
    async fn test_on_demand_generation_prunes_by_hash_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = on_demand_repository(
            "janitor",
            "Run 1",
            "run/1",
            &["amd64".to_string()],
            vec!["main".to_string()],
        )
        .unwrap();
        let by_hash_dir = tmp.path().join("main/binary-amd64/by-hash/SHA256");
        std::fs::create_dir_all(&by_hash_dir).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        for i in 0..20 {
            let f = std::fs::File::create(by_hash_dir.join(format!("old{}", i))).unwrap();
            f.set_modified(old).unwrap();
        }

        apt_repository::AsyncRepository::new(repository)
            .generate_repository(
                tmp.path(),
                &apt_repository::async_repository::AsyncMemoryPackageProvider::new(),
                &apt_repository::async_repository::AsyncMemorySourceProvider::new(),
            )
            .await
            .unwrap();

        assert_eq!(std::fs::read_dir(&by_hash_dir).unwrap().count(), 12);
    }

    #[tokio::test]
    async fn test_read_on_demand_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result = read_on_demand_file(tmp.path(), &["nope"]).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_read_on_demand_file_present() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("main");
        tokio::fs::create_dir_all(&sub).await.unwrap();
        tokio::fs::write(sub.join("Packages"), b"hello")
            .await
            .unwrap();
        let bytes = read_on_demand_file(tmp.path(), &["main", "Packages"])
            .await
            .unwrap();
        assert_eq!(bytes, Some(b"hello".to_vec()));
    }

    /// `components_for_campaign` must walk campaign ->
    /// debian_build.base_distribution -> distribution.component.
    #[test]
    fn components_for_campaign_walks_debian_build_base_distribution() {
        let cfg = janitor::config::read_string(
            r#"
                distribution {
                    name: "unstable"
                    component: "main"
                    component: "contrib"
                    component: "non-free"
                }
                campaign {
                    name: "lintian-fixes"
                    debian_build { base_distribution: "unstable" }
                }
            "#,
        )
        .unwrap();
        let components =
            super::components_for_campaign(&cfg, &Campaign::new("lintian-fixes").unwrap()).unwrap();
        assert_eq!(components, vec!["main", "contrib", "non-free"]);
    }

    /// Campaign not present -> None. Guard against a future refactor
    /// that returns an empty Vec (which apt would happily accept as
    /// "no components", silently ignoring publishes).
    #[test]
    fn components_for_campaign_missing_campaign_returns_none() {
        let cfg = janitor::config::read_string(r#""#).unwrap();
        assert!(super::components_for_campaign(&cfg, &Campaign::new("nope").unwrap()).is_none());
    }

    /// Campaign present but no `debian_build` block -> None so the
    /// caller can fall back.
    #[test]
    fn components_for_campaign_no_debian_build_returns_none() {
        let cfg = janitor::config::read_string(r#"campaign { name: "generic" }"#).unwrap();
        assert!(super::components_for_campaign(&cfg, &Campaign::new("generic").unwrap()).is_none());
    }

    /// Campaign has debian_build but the base_distribution isn't
    /// declared in the config -> None. Caller falls back.
    #[test]
    fn components_for_campaign_unknown_distribution_returns_none() {
        let cfg = janitor::config::read_string(
            r#"
                campaign {
                    name: "lintian-fixes"
                    debian_build { base_distribution: "not-in-config" }
                }
            "#,
        )
        .unwrap();
        assert!(
            super::components_for_campaign(&cfg, &Campaign::new("lintian-fixes").unwrap())
                .is_none()
        );
    }

    /// Distribution has an empty component list -> None (rather
    /// than returning an empty Vec, which would silently blank
    /// out the Release file's Components line).
    #[test]
    fn components_for_campaign_empty_components_returns_none() {
        let cfg = janitor::config::read_string(
            r#"
                distribution { name: "unstable" }
                campaign {
                    name: "lintian-fixes"
                    debian_build { base_distribution: "unstable" }
                }
            "#,
        )
        .unwrap();
        assert!(
            super::components_for_campaign(&cfg, &Campaign::new("lintian-fixes").unwrap())
                .is_none()
        );
    }
}
