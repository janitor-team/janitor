//! Fan-out from campaign completion events to per-apt_repository republishes.
//!
//! Owns the `campaign_name -> [apt_repository, ...]` map (built from
//! `apt_repository.select` in `janitor.conf`) and spawns a
//! `RepositoryGenerator::generate_repository` call per matching repo when
//! the runner reports a successful build.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::{AptRepositoryConfig, ArchiveConfig};
use crate::database::BuildManager;
use crate::error::{ArchiveError, ArchiveResult};
use crate::repository::RepositoryGenerator;
use crate::scanner::PackageScanner;

/// Lifecycle of a repository generation task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[allow(missing_docs)]
pub enum JobStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// One spawned republish job. Cloneable snapshot; the live task
/// handle is on [`ActiveJob`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct JobInfo {
    pub id: Uuid,
    pub repository_name: String,
    pub campaign: Option<String>,
    pub status: JobStatus,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub error_message: Option<String>,
}

impl JobInfo {
    /// Fresh Pending job with a random UUID and `now()` timestamps.
    pub fn new(repository_name: String, campaign: Option<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            repository_name,
            campaign,
            status: JobStatus::Pending,
            started_at: chrono::Utc::now(),
            completed_at: None,
            error_message: None,
        }
    }

    /// Transition to Running and reset `started_at` to now.
    pub fn start(&mut self) {
        self.status = JobStatus::Running;
        self.started_at = chrono::Utc::now();
    }

    /// Transition to Completed and stamp `completed_at`.
    pub fn complete(&mut self) {
        self.status = JobStatus::Completed;
        self.completed_at = Some(chrono::Utc::now());
    }

    /// Transition to Failed, record the error, and stamp `completed_at`.
    pub fn fail(&mut self, error: &str) {
        self.status = JobStatus::Failed;
        self.completed_at = Some(chrono::Utc::now());
        self.error_message = Some(error.to_string());
    }

    /// Transition to Cancelled and stamp `completed_at`.
    pub fn cancel(&mut self) {
        self.status = JobStatus::Cancelled;
        self.completed_at = Some(chrono::Utc::now());
    }

    /// True for Completed/Failed/Cancelled.
    pub fn is_finished(&self) -> bool {
        matches!(
            self.status,
            JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled
        )
    }
}

/// Snapshot of an in-flight job plus its tokio `JoinHandle`.
#[derive(Debug)]
#[allow(missing_docs)]
pub struct ActiveJob {
    pub info: JobInfo,
    pub handle: JoinHandle<ArchiveResult<()>>,
}

impl ActiveJob {
    #[allow(missing_docs)]
    pub fn new(info: JobInfo, handle: JoinHandle<ArchiveResult<()>>) -> Self {
        Self { info, handle }
    }

    /// Delegates to the underlying `JoinHandle::is_finished`.
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// `JoinHandle::abort()`.
    pub fn cancel(&self) {
        self.handle.abort();
    }
}

/// Tuning knobs for [`GeneratorManager`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct GeneratorManagerConfig {
    pub max_concurrent_jobs: usize,
    pub job_timeout_seconds: u64,
    pub enable_cleanup: bool,
    pub cleanup_interval_seconds: u64,
}

impl Default for GeneratorManagerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_jobs: 4,
            job_timeout_seconds: 3600,
            enable_cleanup: true,
            cleanup_interval_seconds: 300,
        }
    }
}

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

/// Owns the campaign->repository fan-out map, the set of in-flight
/// jobs, and (optionally) a `LastPublishTimes` observer wired to
/// the web `/ready` and `/last-publish` handlers.
pub struct GeneratorManager {
    config: Arc<ArchiveConfig>,
    generator: Arc<RepositoryGenerator>,
    scanner: Arc<PackageScanner>,
    database: Arc<BuildManager>,
    manager_config: GeneratorManagerConfig,
    active_jobs: Arc<RwLock<HashMap<String, ActiveJob>>>,
    campaign_to_repository: Arc<RwLock<HashMap<String, Vec<AptRepositoryConfig>>>>,
    job_history: Arc<Mutex<Vec<JobInfo>>>,
    publish_observer: Arc<RwLock<Option<crate::web::LastPublishTimes>>>,
}

impl GeneratorManager {
    /// Create a new generator manager.
    pub async fn new(
        config: ArchiveConfig,
        generator: RepositoryGenerator,
        scanner: PackageScanner,
        database: BuildManager,
        manager_config: GeneratorManagerConfig,
    ) -> ArchiveResult<Self> {
        let config = Arc::new(config);
        let generator = Arc::new(generator);
        let scanner = Arc::new(scanner);
        let database = Arc::new(database);

        let campaign_mapping = build_campaign_mapping(&config);
        info!(
            "Built campaign mapping for {} campaigns",
            campaign_mapping.len()
        );

        Ok(Self {
            config,
            generator,
            scanner,
            database,
            manager_config,
            active_jobs: Arc::new(RwLock::new(HashMap::new())),
            campaign_to_repository: Arc::new(RwLock::new(campaign_mapping)),
            job_history: Arc::new(Mutex::new(Vec::new())),
            publish_observer: Arc::new(RwLock::new(None)),
        })
    }

    /// Wire a [`crate::web::LastPublishTimes`] map to be updated
    /// whenever a per-repository publish job finishes successfully.
    /// Idempotent: a subsequent call replaces the observer.
    pub async fn set_publish_observer(&self, observer: crate::web::LastPublishTimes) {
        let mut slot = self.publish_observer.write().await;
        *slot = Some(observer);
    }

    /// Test/introspection helper: peek at the currently-wired
    /// observer. Returns None if `set_publish_observer` hasn't been
    /// called yet.
    #[cfg(test)]
    pub async fn get_publish_observer(&self) -> Option<crate::web::LastPublishTimes> {
        self.publish_observer.read().await.clone()
    }

    /// Trigger repository generation for a specific campaign.
    pub async fn trigger_campaign(&self, campaign_name: &str) -> ArchiveResult<Vec<Uuid>> {
        info!(
            "Triggering repository generation for campaign: {}",
            campaign_name
        );

        let campaign_mapping = self.campaign_to_repository.read().await;
        let repositories = match campaign_mapping.get(campaign_name) {
            Some(repos) => repos.clone(),
            None => {
                warn!("No repositories found for campaign: {}", campaign_name);
                return Ok(Vec::new());
            }
        };

        let mut job_ids = Vec::new();

        for repo_config in repositories {
            match self
                .trigger_repository(&repo_config, Some(campaign_name.to_string()))
                .await
            {
                Ok(job_id) => {
                    job_ids.push(job_id);
                    info!(
                        "Triggered job {} for repository {}",
                        job_id, repo_config.name
                    );
                }
                Err(e) => {
                    error!("Failed to trigger repository {}: {}", repo_config.name, e);
                }
            }
        }

        info!(
            "Triggered {} jobs for campaign {}",
            job_ids.len(),
            campaign_name
        );
        Ok(job_ids)
    }

    /// Trigger repository generation for all configured repositories.
    pub async fn trigger_all_repositories(&self) -> ArchiveResult<Vec<Uuid>> {
        info!("Triggering generation for all repositories");

        let mut job_ids = Vec::new();

        // Get all repositories from config
        for (repo_name, repo_config) in &self.config.repositories {
            match self.trigger_repository(repo_config, None).await {
                Ok(job_id) => {
                    job_ids.push(job_id);
                    info!("Triggered job {} for repository {}", job_id, repo_name);
                }
                Err(e) => {
                    error!("Failed to trigger repository {}: {}", repo_name, e);
                }
            }
        }

        info!("Triggered {} total repository jobs", job_ids.len());
        Ok(job_ids)
    }

    /// Trigger repository generation for a specific repository.
    pub async fn trigger_repository(
        &self,
        repo_config: &AptRepositoryConfig,
        campaign: Option<String>,
    ) -> ArchiveResult<Uuid> {
        let repo_name = &repo_config.name;

        // Check if a job is already running for this repository
        {
            let active_jobs = self.active_jobs.read().await;
            if let Some(existing_job) = active_jobs.get(repo_name) {
                if !existing_job.is_finished() {
                    info!("Job already running for repository: {}", repo_name);
                    return Ok(existing_job.info.id);
                }
            }
        }

        // `max_concurrent_jobs` is purely observational (surfaces
        // in ManagerStatistics). Enforcing a hard cap here would
        // drop republish requests during a burst; log at debug so
        // operators can still spot excessive concurrency.
        let active_count = {
            let active_jobs = self.active_jobs.read().await;
            active_jobs
                .values()
                .filter(|job| !job.is_finished())
                .count()
        };
        if active_count >= self.manager_config.max_concurrent_jobs {
            debug!(
                "concurrent-jobs soft threshold {} reached ({} running)",
                self.manager_config.max_concurrent_jobs, active_count
            );
        }

        // Create job info
        let mut job_info = JobInfo::new(repo_name.clone(), campaign);
        let job_id = job_info.id;

        info!(
            "Starting repository generation job {} for {}",
            job_id, repo_name
        );

        // Clone necessary data for the task
        let generator = Arc::clone(&self.generator);
        let repo_config_clone = repo_config.clone();
        let active_jobs = Arc::clone(&self.active_jobs);
        let job_history = Arc::clone(&self.job_history);
        let publish_observer = Arc::clone(&self.publish_observer);

        // Start the job
        job_info.start();

        // Spawn the repository generation task
        let handle = tokio::spawn(async move {
            let result = generator.generate_repository(&repo_config_clone).await;

            // Update job status and move to history
            {
                let mut active_jobs_guard = active_jobs.write().await;
                if let Some(mut active_job) = active_jobs_guard.remove(&repo_config_clone.name) {
                    match &result {
                        Ok(_) => {
                            active_job.info.complete();
                            info!("Repository generation completed for job {}", job_id);
                            record_publish_completion(&publish_observer, &repo_config_clone.name)
                                .await;
                        }
                        Err(e) => {
                            active_job.info.fail(&e.to_string());
                            error!("Repository generation failed for job {}: {}", job_id, e);
                        }
                    }

                    // Move to history
                    let mut history = job_history.lock().await;
                    history.push(active_job.info);

                    // Keep only recent history (last 100 jobs)
                    if history.len() > 100 {
                        history.remove(0);
                    }
                }
            }

            result
        });

        // Store the active job
        let active_job = ActiveJob::new(job_info, handle);
        {
            let mut active_jobs_guard = self.active_jobs.write().await;
            active_jobs_guard.insert(repo_name.clone(), active_job);
        }

        Ok(job_id)
    }

    /// Get status of all active jobs.
    pub async fn get_active_jobs(&self) -> HashMap<String, JobInfo> {
        let active_jobs = self.active_jobs.read().await;
        active_jobs
            .iter()
            .map(|(name, job)| (name.clone(), job.info.clone()))
            .collect()
    }

    /// Get job history.
    pub async fn get_job_history(&self) -> Vec<JobInfo> {
        let history = self.job_history.lock().await;
        history.clone()
    }

    /// Get job information by ID.
    pub async fn get_job_info(&self, job_id: Uuid) -> Option<JobInfo> {
        // Check active jobs first
        {
            let active_jobs = self.active_jobs.read().await;
            for job in active_jobs.values() {
                if job.info.id == job_id {
                    return Some(job.info.clone());
                }
            }
        }

        // Check job history
        {
            let history = self.job_history.lock().await;
            for job_info in history.iter() {
                if job_info.id == job_id {
                    return Some(job_info.clone());
                }
            }
        }

        None
    }

    /// Cancel a job by ID.
    pub async fn cancel_job(&self, job_id: Uuid) -> ArchiveResult<()> {
        let active_jobs = self.active_jobs.write().await;

        for (repo_name, active_job) in active_jobs.iter() {
            if active_job.info.id == job_id {
                info!("Cancelling job {} for repository {}", job_id, repo_name);
                active_job.cancel();
                return Ok(());
            }
        }

        Err(ArchiveError::NotFound(format!("Job {} not found", job_id)))
    }

    /// Cleanup finished jobs.
    pub async fn cleanup_finished_jobs(&self) -> usize {
        let mut active_jobs = self.active_jobs.write().await;
        let mut to_remove = Vec::new();

        for (repo_name, active_job) in active_jobs.iter() {
            if active_job.is_finished() {
                to_remove.push(repo_name.clone());
            }
        }

        let count = to_remove.len();
        for repo_name in to_remove {
            if let Some(mut active_job) = active_jobs.remove(&repo_name) {
                // Update final status if needed
                if active_job.handle.is_finished() && active_job.info.status == JobStatus::Running {
                    // Since handle is finished, we can't get the result anymore
                    // We'll assume it completed unless we know otherwise
                    active_job.info.complete();
                }

                // Move to history
                let mut history = self.job_history.lock().await;
                history.push(active_job.info);

                // Keep only recent history
                if history.len() > 100 {
                    history.remove(0);
                }
            }
        }

        if count > 0 {
            debug!("Cleaned up {} finished jobs", count);
        }

        count
    }

    /// Start background cleanup task.
    pub async fn start_cleanup_task(&self) -> JoinHandle<()> {
        let active_jobs = Arc::clone(&self.active_jobs);
        let job_history = Arc::clone(&self.job_history);
        let cleanup_interval = self.manager_config.cleanup_interval_seconds;

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(cleanup_interval));

            loop {
                interval.tick().await;

                // Cleanup finished jobs
                let mut active_jobs_guard = active_jobs.write().await;
                let mut to_remove = Vec::new();

                for (repo_name, active_job) in active_jobs_guard.iter() {
                    if active_job.is_finished() {
                        to_remove.push(repo_name.clone());
                    }
                }

                let count = to_remove.len();
                for repo_name in to_remove {
                    if let Some(active_job) = active_jobs_guard.remove(&repo_name) {
                        let mut history = job_history.lock().await;
                        history.push(active_job.info);

                        if history.len() > 100 {
                            history.remove(0);
                        }
                    }
                }

                if count > 0 {
                    debug!("Background cleanup: removed {} finished jobs", count);
                }
            }
        })
    }

    /// Get campaign to repository mapping.
    pub async fn get_campaign_mapping(&self) -> HashMap<String, Vec<String>> {
        let mapping = self.campaign_to_repository.read().await;
        mapping
            .iter()
            .map(|(campaign, repos)| {
                let repo_names = repos.iter().map(|r| r.name.clone()).collect();
                (campaign.clone(), repo_names)
            })
            .collect()
    }

    /// Get package scanner instance
    pub fn scanner(&self) -> &Arc<PackageScanner> {
        &self.scanner
    }

    /// Get database manager instance
    pub fn database(&self) -> &Arc<BuildManager> {
        &self.database
    }

    /// Get manager statistics.
    pub async fn get_statistics(&self) -> ManagerStatistics {
        let active_jobs = self.active_jobs.read().await;
        let history = self.job_history.lock().await;

        let active_count = active_jobs.len();
        let running_count = active_jobs
            .values()
            .filter(|job| !job.is_finished())
            .count();
        let total_historical = history.len();
        let completed_count = history
            .iter()
            .filter(|job| job.status == JobStatus::Completed)
            .count();
        let failed_count = history
            .iter()
            .filter(|job| job.status == JobStatus::Failed)
            .count();

        ManagerStatistics {
            active_jobs: active_count,
            running_jobs: running_count,
            total_historical_jobs: total_historical,
            completed_jobs: completed_count,
            failed_jobs: failed_count,
            max_concurrent_jobs: self.manager_config.max_concurrent_jobs,
        }
    }
}

/// Counters returned by [`GeneratorManager::get_statistics`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ManagerStatistics {
    pub active_jobs: usize,
    pub running_jobs: usize,
    pub total_historical_jobs: usize,
    pub completed_jobs: usize,
    pub failed_jobs: usize,
    pub max_concurrent_jobs: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn test_job_info_creation() {
        let job = JobInfo::new("test-repo".to_string(), Some("test-campaign".to_string()));

        assert_eq!(job.repository_name, "test-repo");
        assert_eq!(job.campaign, Some("test-campaign".to_string()));
        assert_eq!(job.status, JobStatus::Pending);
        assert!(!job.is_finished());
    }

    #[test]
    fn test_job_info_lifecycle() {
        let mut job = JobInfo::new("test-repo".to_string(), None);

        // Start the job
        job.start();
        assert_eq!(job.status, JobStatus::Running);
        assert!(!job.is_finished());

        // Complete the job
        job.complete();
        assert_eq!(job.status, JobStatus::Completed);
        assert!(job.is_finished());
        assert!(job.completed_at.is_some());
    }

    #[test]
    fn test_job_info_failure() {
        let mut job = JobInfo::new("test-repo".to_string(), None);

        job.start();
        job.fail("Test error");

        assert_eq!(job.status, JobStatus::Failed);
        assert!(job.is_finished());
        assert_eq!(job.error_message, Some("Test error".to_string()));
        assert!(job.completed_at.is_some());
    }

    #[test]
    fn test_generator_manager_config_default() {
        let config = GeneratorManagerConfig::default();

        assert_eq!(config.max_concurrent_jobs, 4);
        assert_eq!(config.job_timeout_seconds, 3600);
        assert!(config.enable_cleanup);
        assert_eq!(config.cleanup_interval_seconds, 300);
    }

    #[test]
    fn test_manager_statistics() {
        let stats = ManagerStatistics {
            active_jobs: 2,
            running_jobs: 1,
            total_historical_jobs: 10,
            completed_jobs: 8,
            failed_jobs: 2,
            max_concurrent_jobs: 4,
        };

        assert_eq!(stats.active_jobs, 2);
        assert_eq!(stats.running_jobs, 1);
        assert_eq!(
            stats.completed_jobs + stats.failed_jobs,
            stats.total_historical_jobs
        );
    }

    /// Build a bare-bones ArchiveConfig with the given repositories
    /// and optional protobuf runtime_config. Kept in the test
    /// module because production code goes through
    /// `from_env_with_prefix` -- the tests need cheap in-memory
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
            base_url: format!("http://x/{}", name),
            base_path: PathBuf::from(format!("/tmp/{}", name)),
            by_hash: true,
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
