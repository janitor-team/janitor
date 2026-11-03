use crate::{Campaign, CodebaseName, RunId};

/// Response for `POST /runs/{run_id}` on the runner, also published on
/// the `publish-status` channel.
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone, PartialEq, Eq)]
pub struct PublishStatusUpdate {
    /// The run ID.
    pub run_id: RunId,

    /// The new publish-status.
    pub publish_status: crate::api::PublishStatus,

    /// The codebase.
    pub codebase: CodebaseName,

    /// The campaign.
    pub campaign: Campaign,
}

/// Body of `POST /runs/{run_id}` on the runner.
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone, PartialEq, Eq)]
pub struct UpdateRunRequest {
    /// The new publish-status.
    pub publish_status: crate::api::PublishStatus,
}

/// Response for a successful `POST /active-runs/{id}/finish` on the
/// runner. `R` is the stored run result.
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone, PartialEq)]
pub struct FinishResponse<R = serde_json::Value> {
    /// The run ID.
    pub id: RunId,
    /// All files the runner received.
    pub filenames: Vec<String>,
    /// Log filenames.
    pub logs: Vec<String>,
    /// Stored artifact names; null when storing the artifacts failed.
    pub artifacts: Option<Vec<String>>,
    /// The stored run result.
    pub result: R,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_run_request_serialization() {
        let req = UpdateRunRequest {
            publish_status: crate::api::PublishStatus::Approved,
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"publish_status":"approved"}"#
        );
    }
}
