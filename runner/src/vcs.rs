use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::metrics::MetricsCollector;

/// VCS integration for the runner module.
/// This provides a higher-level interface that coordinates with queue management,
/// worker assignments, and system monitoring.
pub use janitor::vcs::{
    BranchOpenFailure, LocalBzrVcsManager, LocalGitVcsManager, RemoteBzrVcsManager,
    RemoteGitVcsManager, RevisionInfo, VcsManager, VcsType,
};

/// VCS manager used by the runner: dispatches to the per-`VcsType`
/// [`janitor::vcs::VcsManager`] and records timing/error metrics
/// around each operation.
#[derive(Clone)]
pub struct RunnerVcsManager {
    managers: HashMap<VcsType, Arc<dyn VcsManager>>,
    metrics: Arc<MetricsCollector>,
}

impl RunnerVcsManager {
    /// Create a new VCS manager with the given VCS type managers.
    pub fn new(managers: HashMap<VcsType, Box<dyn VcsManager>>) -> Self {
        let arc_managers: HashMap<VcsType, Arc<dyn VcsManager>> = managers
            .into_iter()
            .map(|(k, v)| (k, Arc::from(v)))
            .collect();

        Self {
            managers: arc_managers,
            metrics: Arc::new(MetricsCollector {}),
        }
    }

    /// Create from configuration - delegates to janitor::vcs
    pub fn from_config(
        config: &janitor::config::Config,
    ) -> Result<Self, janitor::vcs::VcsConfigError> {
        let managers = janitor::vcs::get_vcs_managers_from_config(config)?;
        Ok(Self::new(managers))
    }

    /// Get VCS manager for a specific type
    pub fn get_manager(&self, vcs_type: VcsType) -> Option<&Arc<dyn VcsManager>> {
        self.managers.get(&vcs_type)
    }

    /// Get all supported VCS types
    pub fn supported_vcs_types(&self) -> Vec<VcsType> {
        self.managers.keys().copied().collect()
    }

    /// Open a branch with metrics tracking
    pub async fn open_branch_with_metrics(
        &self,
        vcs_type: VcsType,
        codebase: &str,
        branch_name: &str,
    ) -> Result<Option<breezyshim::branch::GenericBranch>, BranchOpenFailure> {
        let start_time = std::time::Instant::now();

        let result = if let Some(manager) = self.get_manager(vcs_type) {
            manager
                .get_branch(codebase, branch_name)
                .map_err(|e| BranchOpenFailure {
                    code: "branch-open-error".to_string(),
                    description: format!("Failed to open branch: {}", e),
                    retry_after: None,
                })
        } else {
            Err(BranchOpenFailure {
                code: "unsupported-vcs".to_string(),
                description: format!("VCS type {:?} not supported", vcs_type),
                retry_after: None,
            })
        };

        // Record metrics
        let duration = start_time.elapsed();
        let status = if result.is_ok() { "success" } else { "error" };

        self.metrics.record_vcs_operation_duration(
            &vcs_type.to_string(),
            "open_branch",
            status,
            duration,
        );

        if let Err(ref failure) = result {
            self.metrics
                .record_vcs_error(&vcs_type.to_string(), &failure.code);
        }

        result
    }

    /// Get diff with metrics and caching support
    pub async fn get_diff_with_metrics(
        &self,
        vcs_type: VcsType,
        codebase: &str,
        old_revid: &breezyshim::RevisionId,
        new_revid: &breezyshim::RevisionId,
    ) -> Result<Vec<u8>, VcsError> {
        let start_time = std::time::Instant::now();

        let result = if let Some(manager) = self.get_manager(vcs_type) {
            manager.get_diff(codebase, old_revid, new_revid).await
        } else {
            return Err(VcsError::UnsupportedVcs(vcs_type));
        };

        let duration = start_time.elapsed();
        self.metrics.record_vcs_operation_duration(
            &vcs_type.to_string(),
            "get_diff",
            "success", // get_diff doesn't return Result in the trait
            duration,
        );

        Ok(result)
    }

    /// Get revision info with metrics
    pub async fn get_revision_info_with_metrics(
        &self,
        vcs_type: VcsType,
        codebase: &str,
        old_revid: &breezyshim::RevisionId,
        new_revid: &breezyshim::RevisionId,
    ) -> Result<Vec<RevisionInfo>, VcsError> {
        let start_time = std::time::Instant::now();

        let result = if let Some(manager) = self.get_manager(vcs_type) {
            manager
                .get_revision_info(codebase, old_revid, new_revid)
                .await
        } else {
            return Err(VcsError::UnsupportedVcs(vcs_type));
        };

        let duration = start_time.elapsed();
        self.metrics.record_vcs_operation_duration(
            &vcs_type.to_string(),
            "get_revision_info",
            "success",
            duration,
        );

        Ok(result)
    }

    /// List repositories for a VCS type
    pub fn list_repositories(&self, vcs_type: VcsType) -> Result<Vec<String>, VcsError> {
        if let Some(manager) = self.get_manager(vcs_type) {
            Ok(manager.list_repositories())
        } else {
            Err(VcsError::UnsupportedVcs(vcs_type))
        }
    }

    /// Health check for VCS managers
    pub async fn health_check(&self) -> VcsHealthStatus {
        let mut status = VcsHealthStatus {
            overall_healthy: true,
            vcs_statuses: HashMap::new(),
        };

        for (&vcs_type, manager) in &self.managers {
            let start_time = std::time::Instant::now();

            // Simple health check - try to list repositories
            let health = match manager.list_repositories().is_empty() {
                true => VcsHealth::Warning("No repositories available".to_string()),
                false => VcsHealth::Healthy,
            };

            let duration = start_time.elapsed();
            self.metrics.record_vcs_operation_duration(
                &vcs_type.to_string(),
                "health_check",
                if matches!(health, VcsHealth::Healthy) {
                    "success"
                } else {
                    "warning"
                },
                duration,
            );

            if !matches!(health, VcsHealth::Healthy) {
                status.overall_healthy = false;
            }

            status.vcs_statuses.insert(vcs_type, health);
        }

        status
    }

    /// Get VCS manager statistics for monitoring
    pub fn get_statistics(&self) -> VcsStatistics {
        VcsStatistics {
            supported_vcs_types: self.supported_vcs_types(),
            manager_count: self.managers.len(),
        }
    }
}

/// Errors from VCS operations.
#[derive(Debug, thiserror::Error)]
pub enum VcsError {
    /// Unsupported VCS type.
    #[error("Unsupported VCS type: {0:?}")]
    UnsupportedVcs(VcsType),

    /// Branch operation failed.
    #[error("Branch operation failed: {0}")]
    BranchError(#[from] BranchOpenFailure),

    /// Repository-related error.
    #[error("Repository error: {0}")]
    RepositoryError(String),

    /// Network-related error.
    #[error("Network error: {0}")]
    NetworkError(String),

    /// Operation timed out.
    #[error("Timeout after {0:?}")]
    Timeout(Duration),
}

/// Health status for VCS systems
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VcsHealthStatus {
    /// Overall health status of all VCS systems.
    pub overall_healthy: bool,
    /// Individual health status for each VCS type.
    pub vcs_statuses: HashMap<VcsType, VcsHealth>,
}

/// Health status of a VCS manager.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum VcsHealth {
    /// VCS manager is healthy and operational.
    Healthy,
    /// VCS manager has a warning condition.
    Warning(String),
    /// VCS manager has an error condition.
    Error(String),
}

/// Statistics about VCS managers
#[derive(Debug, Clone)]
pub struct VcsStatistics {
    /// List of supported VCS types.
    pub supported_vcs_types: Vec<VcsType>,
    /// Number of active VCS managers.
    pub manager_count: usize,
}

/// VCS coordination trait for queue management integration
#[async_trait]
pub trait VcsCoordinator: Send + Sync {
    /// Check if a codebase is accessible for assignment
    async fn is_codebase_accessible(
        &self,
        vcs_type: VcsType,
        codebase: &str,
        branch: &str,
    ) -> Result<bool, VcsError>;

    /// Prefetch/warm cache for upcoming assignments
    async fn prefetch_codebase(
        &self,
        vcs_type: VcsType,
        codebase: &str,
        branch: &str,
    ) -> Result<(), VcsError>;

    /// Get estimated access time for queue prioritization
    async fn estimate_access_time(
        &self,
        vcs_type: VcsType,
        codebase: &str,
    ) -> Result<Duration, VcsError>;
}

#[async_trait]
impl VcsCoordinator for RunnerVcsManager {
    async fn is_codebase_accessible(
        &self,
        vcs_type: VcsType,
        codebase: &str,
        branch: &str,
    ) -> Result<bool, VcsError> {
        match self
            .open_branch_with_metrics(vcs_type, codebase, branch)
            .await
        {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(failure) => {
                // Some errors indicate temporary unavailability
                match failure.code.as_str() {
                    "too-many-requests" | "502-bad-gateway" | "branch-temporarily-unavailable" => {
                        Ok(false) // Temporarily inaccessible but not an error
                    }
                    _ => Err(VcsError::BranchError(failure)),
                }
            }
        }
    }

    async fn prefetch_codebase(
        &self,
        vcs_type: VcsType,
        codebase: &str,
        branch: &str,
    ) -> Result<(), VcsError> {
        // For now, just open the branch to warm any caches
        self.open_branch_with_metrics(vcs_type, codebase, branch)
            .await?;
        Ok(())
    }

    async fn estimate_access_time(
        &self,
        vcs_type: VcsType,
        codebase: &str,
    ) -> Result<Duration, VcsError> {
        let start = std::time::Instant::now();

        if let Some(manager) = self.get_manager(vcs_type) {
            // Quick repository access test
            let _url = manager.get_repository_url(codebase);
            Ok(start.elapsed())
        } else {
            Err(VcsError::UnsupportedVcs(vcs_type))
        }
    }
}

/// Integration with metrics collection
impl MetricsCollector {
    /// Record the duration of a VCS operation for metrics.
    pub fn record_vcs_operation_duration(
        &self,
        vcs_type: &str,
        operation: &str,
        status: &str,
        duration: Duration,
    ) {
        if let Ok(histogram) = crate::metrics::VCS_OPERATION_DURATION
            .get_metric_with_label_values(&[vcs_type, operation, status])
        {
            histogram.observe(duration.as_secs_f64());
        }
    }

    /// Record a VCS error for metrics.
    pub fn record_vcs_error(&self, vcs_type: &str, error_code: &str) {
        crate::metrics::VCS_ERRORS_TOTAL
            .with_label_values(&[vcs_type, error_code])
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_runner_vcs_manager_creation() {
        let mut managers: HashMap<VcsType, Box<dyn VcsManager>> = HashMap::new();

        managers.insert(
            VcsType::Git,
            Box::new(LocalGitVcsManager::new(PathBuf::from("/tmp/test-git"))),
        );

        let runner_manager = RunnerVcsManager::new(managers);

        assert_eq!(runner_manager.supported_vcs_types(), vec![VcsType::Git]);
        assert!(runner_manager.get_manager(VcsType::Git).is_some());
        assert!(runner_manager.get_manager(VcsType::Bzr).is_none());
    }

    #[test]
    fn test_vcs_statistics() {
        let mut managers: HashMap<VcsType, Box<dyn VcsManager>> = HashMap::new();
        managers.insert(
            VcsType::Git,
            Box::new(LocalGitVcsManager::new(PathBuf::from("/tmp/test-git"))),
        );

        let runner_manager = RunnerVcsManager::new(managers);
        let stats = runner_manager.get_statistics();

        assert_eq!(stats.manager_count, 1);
        assert!(stats.supported_vcs_types.contains(&VcsType::Git));
    }

    #[tokio::test]
    async fn test_vcs_error_handling() {
        let runner_manager = RunnerVcsManager::new(HashMap::new());

        let result = runner_manager
            .open_branch_with_metrics(VcsType::Git, "test-codebase", "main")
            .await;

        match result {
            Err(error) => {
                assert_eq!(error.code, "unsupported-vcs");
            }
            Ok(_) => panic!("Expected error but got success"),
        }
    }
}
