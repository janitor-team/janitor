use crate::{CodebaseName, RunId};

/// Sent when the publish-status for a run changes.
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct PublishStatusPubsub {
    /// The codebase.
    pub codebase: CodebaseName,

    /// The run ID.
    pub run_id: RunId,

    /// The new publish-status.
    #[serde(rename = "publish-status")]
    pub publish_status: crate::api::RunPublishStatus,
}

/// Body of `POST /runs/{run_id}` on the runner.
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct UpdateRunRequest {
    /// The new publish-status.
    pub publish_status: crate::api::RunPublishStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_run_request_serialization() {
        let req = UpdateRunRequest {
            publish_status: crate::api::RunPublishStatus::Approved,
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"publish_status":"approved"}"#
        );
    }
}
