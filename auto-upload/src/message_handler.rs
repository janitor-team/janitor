//! Message processing and routing for build results.

use tracing::{debug, info, warn};

use crate::artifacts::ArtifactProcessor;
use crate::error::Result;
use crate::process::upload_build_result;
use crate::redis_client::{BuildResultMessage, BuildTarget};
use crate::upload::UploadConfig;

/// Dispatches build result messages to [`upload_build_result`].
pub struct MessageHandler {
    artifact_processor: ArtifactProcessor,
    upload_config: UploadConfig,
}

impl MessageHandler {
    /// Open the artifact store; upload settings are moved in.
    pub async fn new(artifact_location: &str, upload_config: UploadConfig) -> Result<Self> {
        Ok(Self {
            artifact_processor: ArtifactProcessor::new(artifact_location).await?,
            upload_config,
        })
    }

    /// Filter, then hand off to [`upload_build_result`].
    pub async fn handle_message(&self, message: BuildResultMessage) -> Result<()> {
        let log_id = &message.log_id;
        info!(
            %log_id,
            target = target_label(message.target.as_ref()),
            "received"
        );

        if !should_process_message(&self.upload_config, &message) {
            debug!(%log_id, "skipped by filters");
            return Ok(());
        }

        match upload_build_result(log_id, &self.artifact_processor, &self.upload_config).await {
            Ok(()) => Ok(()),
            Err(e) => {
                warn!(%log_id, error = %e, "upload failed");
                Err(e)
            }
        }
    }
}

fn target_label(target: Option<&BuildTarget>) -> &'static str {
    match target {
        Some(BuildTarget::Debian(_)) => "debian",
        Some(BuildTarget::Generic(_)) => "generic",
        None => "",
    }
}

/// Check if a message should be processed based on filters.
///
/// Only successful debian builds are eligible, and if a distribution
/// allow-list is configured, the build distribution must match. Artifact
/// existence is checked lazily in [`upload_build_result`] rather than
/// here to avoid downloading twice.
///
/// The `code == "success"` check has to come before touching `target`
/// because the runner publishes non-success runs with `target: null`
/// (matches the Python `is_debian_upload_target` fix in py commit
/// 38fbc2025).
fn should_process_message(upload_config: &UploadConfig, message: &BuildResultMessage) -> bool {
    if message.code.as_deref() != Some("success") {
        debug!(
            code = message.code.as_deref().unwrap_or("<none>"),
            "Skipping non-success run"
        );
        return false;
    }

    let details = match message.target.as_ref() {
        Some(BuildTarget::Debian(details)) => details,
        Some(BuildTarget::Generic(_)) => {
            debug!("Skipping non-debian target");
            return false;
        }
        None => {
            debug!(log_id = %message.log_id, "Skipping message without target");
            return false;
        }
    };

    if !upload_config.should_upload_distribution(&details.build_distribution) {
        debug!(
            distribution = %details.build_distribution,
            "Skipping distribution not in allowed list"
        );
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::should_process_message;
    use crate::redis_client::{
        BuildResultMessage, BuildTarget, DebianTargetDetails, GenericTargetDetails,
    };
    use crate::upload::UploadConfig;

    fn debian_msg(distribution: &str) -> BuildResultMessage {
        BuildResultMessage {
            log_id: janitor_types::RunId::new("test-123").unwrap(),
            code: Some("success".to_string()),
            target: Some(BuildTarget::Debian(DebianTargetDetails {
                build_distribution: distribution.to_string(),
                extra: serde_json::Map::new(),
            })),
        }
    }

    fn generic_msg() -> BuildResultMessage {
        BuildResultMessage {
            log_id: janitor_types::RunId::new("test-123").unwrap(),
            code: Some("success".to_string()),
            target: Some(BuildTarget::Generic(GenericTargetDetails::default())),
        }
    }

    fn no_target_msg(code: Option<&str>) -> BuildResultMessage {
        BuildResultMessage {
            log_id: janitor_types::RunId::new("test-123").unwrap(),
            code: code.map(str::to_string),
            target: None,
        }
    }

    fn cfg(distributions: Vec<String>) -> UploadConfig {
        UploadConfig {
            dput_host: Some("dput-host".into()),
            debsign_keyid: None,
            source_only: false,
            distributions,
        }
    }

    #[test]
    fn debian_target_is_accepted() {
        assert!(should_process_message(
            &cfg(vec![]),
            &debian_msg("unstable")
        ));
    }

    #[test]
    fn generic_target_is_rejected() {
        assert!(!should_process_message(&cfg(vec![]), &generic_msg()));
    }

    #[test]
    fn absent_target_is_rejected() {
        assert!(!should_process_message(
            &cfg(vec![]),
            &no_target_msg(Some("success"))
        ));
    }

    #[test]
    fn non_success_run_is_rejected() {
        // Non-success runs publish `target: null`; the code gate skips
        // them before dispatching on `target`.
        assert!(!should_process_message(
            &cfg(vec![]),
            &no_target_msg(Some("codemod-error"))
        ));
    }

    #[test]
    fn missing_code_is_rejected() {
        // Defensive: a message with no `code` at all shouldn't try to upload.
        let mut message = debian_msg("unstable");
        message.code = None;
        assert!(!should_process_message(&cfg(vec![]), &message));
    }

    #[test]
    fn distribution_allow_list_is_enforced() {
        let cfg = cfg(vec!["unstable".to_string()]);
        assert!(should_process_message(&cfg, &debian_msg("unstable")));
        assert!(!should_process_message(&cfg, &debian_msg("stable")));
    }
}
