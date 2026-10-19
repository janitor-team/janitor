//! VCS access for the runner, with metrics around each operation.

use std::collections::HashMap;
use std::sync::Arc;

use janitor::CodebaseName;

pub use janitor::vcs::VcsType;
use janitor::vcs::{BranchOpenFailure, VcsManager};

/// VCS manager used by the runner: dispatches to the per-`VcsType`
/// [`janitor::vcs::VcsManager`] and records timing/error metrics
/// around each operation.
#[derive(Clone)]
pub struct RunnerVcsManager {
    managers: HashMap<VcsType, Arc<dyn VcsManager>>,
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

    /// Open a branch with metrics tracking
    pub async fn open_branch_with_metrics(
        &self,
        vcs_type: VcsType,
        codebase: &CodebaseName,
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

        let vcs_type = vcs_type.to_string();
        let status = if result.is_ok() { "success" } else { "error" };
        crate::metrics::VCS_OPERATION_DURATION
            .with_label_values(&[vcs_type.as_str(), "open_branch", status])
            .observe(start_time.elapsed().as_secs_f64());
        if let Err(ref failure) = result {
            crate::metrics::VCS_ERRORS_TOTAL
                .with_label_values(&[vcs_type.as_str(), failure.code.as_str()])
                .inc();
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_vcs_error_handling() {
        let runner_manager = RunnerVcsManager::new(HashMap::new());

        let result = runner_manager
            .open_branch_with_metrics(
                VcsType::Git,
                &CodebaseName::new("test-codebase").unwrap(),
                "main",
            )
            .await;

        match result {
            Err(error) => {
                assert_eq!(error.code, "unsupported-vcs");
            }
            Ok(_) => panic!("Expected error but got success"),
        }
    }
}
