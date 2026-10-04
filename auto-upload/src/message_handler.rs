//! Message processing and routing for build results.

use tracing::{debug, info, warn};

use crate::artifacts::ArtifactProcessor;
use crate::error::Result;
use crate::process::upload_build_result;
use crate::redis_client::BuildResultMessage;
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
            log_id,
            target = message.target.name.as_deref().unwrap_or(""),
            "received"
        );

        if !should_process_message(&self.upload_config, &message) {
            debug!(log_id, "skipped by filters");
            return Ok(());
        }

        match upload_build_result(log_id, &self.artifact_processor, &self.upload_config).await {
            Ok(()) => Ok(()),
            Err(e) => {
                warn!(log_id, error = %e, "upload failed");
                Err(e)
            }
        }
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
/// because the runner publishes non-success runs with `target: {}`
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

    if message.target.name.as_deref() != Some("debian") {
        debug!(
            target = message.target.name.as_deref().unwrap_or("<none>"),
            "Skipping non-debian target"
        );
        return false;
    }

    let Some(details) = message.target.details.as_ref() else {
        debug!(log_id = %message.log_id, "Skipping message with missing target details");
        return false;
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
    use crate::redis_client::{BuildResultMessage, BuildTarget, BuildTargetDetails};
    use crate::upload::UploadConfig;

    fn msg(target: Option<&str>, distribution: Option<&str>) -> BuildResultMessage {
        msg_with_code(Some("success"), target, distribution)
    }

    fn msg_with_code(
        code: Option<&str>,
        target: Option<&str>,
        distribution: Option<&str>,
    ) -> BuildResultMessage {
        BuildResultMessage {
            log_id: "test-123".to_string(),
            code: code.map(str::to_string),
            target: BuildTarget {
                name: target.map(str::to_string),
                details: distribution.map(|d| BuildTargetDetails {
                    build_distribution: d.to_string(),
                    extra: serde_json::Map::new(),
                }),
            },
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
            &msg(Some("debian"), Some("unstable"))
        ));
    }

    #[test]
    fn non_debian_target_is_rejected() {
        assert!(!should_process_message(
            &cfg(vec![]),
            &msg(Some("generic"), Some("unstable"))
        ));
    }

    #[test]
    fn empty_target_is_rejected() {
        assert!(!should_process_message(&cfg(vec![]), &msg(None, None)));
    }

    #[test]
    fn non_success_run_is_rejected() {
        // Regression: matches the Python fix in commit 38fbc2025. Runner
        // publishes non-success runs with `target: {}`; without the code
        // gate we'd fall through the target checks and (in the Python
        // impl, before the fix) crash. Rust never crashes here, but we
        // still want the explicit "not success" skip and the log line.
        assert!(!should_process_message(
            &cfg(vec![]),
            &msg_with_code(Some("codemod-error"), None, None)
        ));
    }

    #[test]
    fn missing_code_is_rejected() {
        // Defensive: a message with no `code` at all shouldn't try to upload.
        assert!(!should_process_message(
            &cfg(vec![]),
            &msg_with_code(None, Some("debian"), Some("unstable"))
        ));
    }

    #[test]
    fn debian_target_without_details_is_rejected() {
        assert!(!should_process_message(
            &cfg(vec![]),
            &msg(Some("debian"), None)
        ));
    }

    #[test]
    fn distribution_allow_list_is_enforced() {
        let cfg = cfg(vec!["unstable".to_string()]);
        assert!(should_process_message(
            &cfg,
            &msg(Some("debian"), Some("unstable"))
        ));
        assert!(!should_process_message(
            &cfg,
            &msg(Some("debian"), Some("stable"))
        ));
    }
}
