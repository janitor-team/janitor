//! Fan-out from campaign completion events to per-apt_repository republishes.
//!
//! Owns the `campaign_name -> [apt_repository, ...]` map (built from
//! `apt_repository.select` in `janitor.conf`) and spawns a
//! `RepositoryGenerator::generate_repository` call per matching repo when
//! the runner reports a successful build.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::config::{AptRepositoryConfig, ArchiveConfig};
use crate::repository::RepositoryGenerator;

/// Notify the last-publish observer (if any) that a repository has
/// been successfully published. Public within the crate so the
/// spawn path in `trigger_repository` can invoke it consistently
/// with any future callers; kept in a helper so it can be unit-
/// tested without spinning up a full manager.
pub(crate) async fn record_publish_completion(
    observer: &Arc<RwLock<Option<crate::web::LastPublishTimes>>>,
    repo_name: &str,
) {
    if let Some(times) = observer.read().await.as_ref() {
        let mut map = times.write().await;
        map.insert(repo_name.to_string(), chrono::Utc::now());
    }
}

/// Compute the `campaign_name -> [AptRepositoryConfig, ...]`
/// fan-out map from an [`ArchiveConfig`]. Extracted as a free
/// function so it can be unit-tested without spinning up the
/// scanner/database.
///
/// When `runtime_config` is present the mapping is driven by
/// `config.apt_repository[*].select[*].campaign` (a single campaign
/// can produce multiple apt repositories); when absent, falls back
/// to a `suite == campaign` mapping so env-only deployments still
/// work.
pub fn build_campaign_mapping(config: &ArchiveConfig) -> HashMap<String, Vec<AptRepositoryConfig>> {
    let mut mapping: HashMap<String, Vec<AptRepositoryConfig>> = HashMap::new();

    if let Some(runtime) = config.runtime_config.as_ref() {
        for apt_repo in &runtime.apt_repository {
            let name = apt_repo.name();
            let repo_cfg = match config.repositories.get(name) {
                Some(r) => r,
                // apt_repository declared in janitor.conf that
                // wasn't materialised into ArchiveConfig.repositories
                // (would happen if the caller supplied an overriding
                // env-config with a different set). Skip -- we have
                // no repository config to trigger against.
                None => continue,
            };
            for select in &apt_repo.select {
                let Some(campaign_name) = select.campaign.as_deref() else {
                    continue;
                };
                mapping
                    .entry(campaign_name.to_string())
                    .or_default()
                    .push(repo_cfg.clone());
            }
        }
    }

    if mapping.is_empty() {
        for (repo_name, repo_config) in &config.repositories {
            mapping
                .entry(repo_config.suite.clone())
                .or_default()
                .push(repo_config.clone());
            debug!(
                "Mapped repository '{}' to campaign '{}' (fallback)",
                repo_name, repo_config.suite
            );
        }
    }

    mapping
}

/// Owns the campaign->repository fan-out map, the in-flight
/// generation tasks, and (optionally) a `LastPublishTimes` observer
/// wired to the web `/ready` and `/last-publish` handlers.
pub struct GeneratorManager {
    config: Arc<ArchiveConfig>,
    generator: Arc<RepositoryGenerator>,
    campaign_to_repository: HashMap<String, Vec<AptRepositoryConfig>>,
    active_jobs: Mutex<HashMap<String, JoinHandle<()>>>,
    publish_observer: Arc<RwLock<Option<crate::web::LastPublishTimes>>>,
}

impl GeneratorManager {
    /// Create a new generator manager.
    pub fn new(config: ArchiveConfig, generator: RepositoryGenerator) -> Self {
        let campaign_to_repository = build_campaign_mapping(&config);
        info!(
            "Built campaign mapping for {} campaigns",
            campaign_to_repository.len()
        );

        Self {
            config: Arc::new(config),
            generator: Arc::new(generator),
            campaign_to_repository,
            active_jobs: Mutex::new(HashMap::new()),
            publish_observer: Arc::new(RwLock::new(None)),
        }
    }

    /// Wire a [`crate::web::LastPublishTimes`] map to be updated
    /// whenever a per-repository publish job finishes successfully.
    /// Idempotent: a subsequent call replaces the observer.
    pub async fn set_publish_observer(&self, observer: crate::web::LastPublishTimes) {
        let mut slot = self.publish_observer.write().await;
        *slot = Some(observer);
    }

    /// Trigger repository generation for a specific campaign.
    pub async fn trigger_campaign(&self, campaign_name: &str) {
        info!(
            "Triggering repository generation for campaign: {}",
            campaign_name
        );

        let Some(repositories) = self.campaign_to_repository.get(campaign_name) else {
            warn!("No repositories found for campaign: {}", campaign_name);
            return;
        };

        for repo_config in repositories {
            self.trigger_repository(repo_config).await;
        }
    }

    /// Trigger repository generation for all configured repositories.
    pub async fn trigger_all_repositories(&self) {
        info!("Triggering generation for all repositories");

        for repo_config in self.config.repositories.values() {
            self.trigger_repository(repo_config).await;
        }
    }

    /// Spawn generation of a repository, unless a generation task
    /// for it is still running.
    pub async fn trigger_repository(&self, repo_config: &AptRepositoryConfig) {
        let mut active_jobs = self.active_jobs.lock().await;
        if active_jobs
            .get(&repo_config.name)
            .is_some_and(|handle| !handle.is_finished())
        {
            info!("Job already running for repository: {}", repo_config.name);
            return;
        }

        info!("Starting repository generation for {}", repo_config.name);

        let generator = Arc::clone(&self.generator);
        let repo_config = repo_config.clone();
        let publish_observer = Arc::clone(&self.publish_observer);
        let name = repo_config.name.clone();

        let handle = tokio::spawn(async move {
            match generator.generate_repository(&repo_config).await {
                Ok(()) => {
                    info!("Repository generation completed for {}", repo_config.name);
                    record_publish_completion(&publish_observer, &repo_config.name).await;
                }
                Err(e) => {
                    error!(
                        "Repository generation failed for {}: {}",
                        repo_config.name, e
                    );
                }
            }
        });
        active_jobs.insert(name, handle);
    }

    /// Names of the campaigns that trigger at least one repository.
    pub fn campaign_names(&self) -> Vec<String> {
        self.campaign_to_repository.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build a bare-bones ArchiveConfig with the given repositories
    /// and optional protobuf runtime_config. Kept in the test
    /// module because production code goes through
    /// `from_janitor_config` -- the tests need cheap in-memory
    /// construction to exercise `build_campaign_mapping`.
    fn make_archive_config(
        repos: Vec<AptRepositoryConfig>,
        runtime: Option<janitor::config::Config>,
    ) -> ArchiveConfig {
        let mut repositories = HashMap::new();
        for r in repos {
            repositories.insert(r.name.clone(), r);
        }
        ArchiveConfig {
            repositories,
            archive_path: PathBuf::from("/tmp/archive"),
            runtime_config: runtime.map(Arc::new),
            ..Default::default()
        }
    }

    fn repo(name: &str, suite: &str) -> AptRepositoryConfig {
        AptRepositoryConfig {
            name: name.to_string(),
            description: format!("{} repo", name),
            origin: "Janitor".to_string(),
            label: name.to_string(),
            suite: suite.to_string(),
            codename: suite.to_string(),
            architectures: vec!["amd64".to_string()],
            components: vec!["main".to_string()],
            base_path: PathBuf::from(format!("/tmp/{}", name)),
        }
    }

    /// Given a janitor.conf with `apt_repository { select { campaign
    /// = X } select { campaign = Y } }`, the fan-out map must list
    /// the same apt_repository under both campaigns.
    #[test]
    fn build_campaign_mapping_reads_runtime_selects() {
        let cfg = janitor::config::read_string(
            r#"
                campaign { name: "lintian-fixes" }
                campaign { name: "fresh-releases" }
                apt_repository {
                    name: "unstable"
                    select { campaign: "lintian-fixes" }
                    select { campaign: "fresh-releases" }
                }
            "#,
        )
        .unwrap();
        let ac = make_archive_config(vec![repo("unstable", "unstable")], Some(cfg));
        let mapping = build_campaign_mapping(&ac);
        assert_eq!(mapping.len(), 2);
        assert_eq!(mapping.get("lintian-fixes").unwrap()[0].name, "unstable");
        assert_eq!(mapping.get("fresh-releases").unwrap()[0].name, "unstable");
    }

    /// A single campaign that appears on multiple apt_repositories
    /// must fan out to a Vec with one entry per repository. The
    /// `campaign_to_repository[campaign]` list grows once per
    /// matching apt_repository.
    #[test]
    fn build_campaign_mapping_fans_out_across_repos() {
        let cfg = janitor::config::read_string(
            r#"
                campaign { name: "lintian-fixes" }
                apt_repository {
                    name: "snapshot"
                    select { campaign: "lintian-fixes" }
                }
                apt_repository {
                    name: "stable"
                    select { campaign: "lintian-fixes" }
                }
            "#,
        )
        .unwrap();
        let ac = make_archive_config(
            vec![repo("snapshot", "snapshot"), repo("stable", "stable")],
            Some(cfg),
        );
        let mapping = build_campaign_mapping(&ac);
        let mut names: Vec<&str> = mapping
            .get("lintian-fixes")
            .unwrap()
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        names.sort();
        assert_eq!(names, vec!["snapshot", "stable"]);
    }

    /// If runtime_config declares an apt_repository whose name isn't
    /// materialised in ArchiveConfig.repositories, it must be
    /// silently skipped -- otherwise a partial env override would
    /// crash the manager at construction time.
    #[test]
    fn build_campaign_mapping_skips_unknown_repo_names() {
        let cfg = janitor::config::read_string(
            r#"
                campaign { name: "lintian-fixes" }
                apt_repository {
                    name: "not-materialised"
                    select { campaign: "lintian-fixes" }
                }
            "#,
        )
        .unwrap();
        // No repositories in the ArchiveConfig; the runtime one
        // references "not-materialised". Fallback kicks in (suite ==
        // campaign) but the source list is empty, so mapping is
        // empty too.
        let ac = make_archive_config(vec![], Some(cfg));
        let mapping = build_campaign_mapping(&ac);
        assert!(mapping.is_empty());
    }

    /// Without a runtime_config we fall back to `suite == campaign`
    /// so env-only deployments still route runner events into the
    /// right republish path. Regression guard for the env-only
    /// deployment story.
    #[test]
    fn build_campaign_mapping_fallback_no_runtime_config() {
        let ac = make_archive_config(vec![repo("lintian-fixes", "lintian-fixes")], None);
        let mapping = build_campaign_mapping(&ac);
        assert_eq!(mapping.len(), 1);
        assert_eq!(
            mapping.get("lintian-fixes").unwrap()[0].name,
            "lintian-fixes"
        );
    }

    /// The publish-observer helper must record `Utc::now()` into
    /// the shared map when the observer slot is populated. This
    /// guards against silently dropping the update if the slot is
    /// wrapped in a Mutex-vs-RwLock mismatch or the notify path
    /// forgets to `write()`.
    #[tokio::test]
    async fn record_publish_completion_writes_to_wired_observer() {
        let times = crate::web::new_last_publish_times();
        let observer = Arc::new(RwLock::new(Some(times.clone())));
        record_publish_completion(&observer, "unstable").await;
        let map = times.read().await;
        assert!(
            map.contains_key("unstable"),
            "expected timestamp for unstable"
        );
    }

    /// No observer wired -> no-op. Must not panic even if the
    /// manager was constructed without `set_publish_observer`.
    #[tokio::test]
    async fn record_publish_completion_no_observer_is_noop() {
        let observer: Arc<RwLock<Option<crate::web::LastPublishTimes>>> =
            Arc::new(RwLock::new(None));
        record_publish_completion(&observer, "unstable").await;
        // Nothing to assert other than "didn't panic".
    }

    /// Recording twice for the same suite must overwrite the
    /// previous timestamp. The test asserts the second timestamp
    /// is strictly newer than the first so we notice if the code
    /// accidentally records zero or reuses the initial write.
    #[tokio::test]
    async fn record_publish_completion_overwrites_prior_timestamp() {
        let times = crate::web::new_last_publish_times();
        let observer = Arc::new(RwLock::new(Some(times.clone())));
        record_publish_completion(&observer, "unstable").await;
        let first = *times.read().await.get("unstable").unwrap();
        // Sleep a tick so the second Utc::now() is unambiguously
        // later. Using a real sleep is fine here -- the whole test
        // runs in <10ms.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        record_publish_completion(&observer, "unstable").await;
        let second = *times.read().await.get("unstable").unwrap();
        assert!(second > first, "second write should be strictly newer");
    }

    /// A runtime `apt_repository` block with no `select` entries
    /// should not create any campaign mapping for it (there's
    /// nothing to route).
    #[test]
    fn build_campaign_mapping_repo_with_no_selects() {
        let cfg = janitor::config::read_string(
            r#"
                apt_repository { name: "unstable" }
            "#,
        )
        .unwrap();
        let ac = make_archive_config(vec![repo("unstable", "unstable")], Some(cfg));
        // Runtime has no selects, so the runtime branch yields
        // empty; fallback populates from repositories.
        let mapping = build_campaign_mapping(&ac);
        assert_eq!(mapping.len(), 1);
        assert_eq!(mapping.get("unstable").unwrap()[0].name, "unstable");
    }
}
