use janitor_types::{CodebaseName, RunId};

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
