use crate::{Campaign, CodebaseName, RunId};
use serde::{Deserialize, Serialize};

/// Response for `POST /runs/{run_id}` on the runner, also published on
/// the `publish-status` channel.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
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
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct UpdateRunRequest {
    /// The new publish-status.
    pub publish_status: crate::api::PublishStatus,
}

/// Response for a successful `POST /active-runs/{id}/finish` on the
/// runner. `R` is the stored run result.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
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

/// Error response: `{"reason": ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonResponse {
    /// Reason for the failure.
    pub reason: String,
}

/// Body of `POST /active-runs` on the runner, asking for a work
/// assignment.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssignRequest {
    /// Worker name. Only honoured by the runner's private
    /// (intra-cluster) app; the authenticated public app reads the
    /// worker name from the credentials instead. Older Python workers
    /// sent this as `worker`.
    #[serde(rename = "node", alias = "worker")]
    pub worker: Option<String>,
    /// Architectures the worker can build for.
    #[serde(default)]
    pub archs: Vec<String>,
    /// Link to the worker.
    pub worker_link: Option<String>,
    /// How the runner can reach back to the worker.
    pub backchannel: Option<BackchannelRequest>,
    /// Specific codebase to work on.
    pub codebase: Option<CodebaseName>,
    /// Specific campaign to work on.
    pub campaign: Option<Campaign>,
    /// Hosts the worker is temporarily rate-limited on; the runner
    /// skips queue items on them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_hosts: Option<Vec<String>>,
}

/// How the runner can reach back to the worker:
/// `{"kind": "http"|"jenkins", "url": "..."}`. `my_url` is accepted as
/// an alias for `url`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BackchannelRequest {
    /// "http" or "jenkins".
    pub kind: Option<String>,
    /// URL of the worker or Jenkins build.
    #[serde(alias = "my_url")]
    pub url: Option<String>,
    /// Jenkins metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jenkins: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assign_request_accepts_worker_alias() {
        let req: AssignRequest = serde_json::from_str(r#"{"worker": "w1"}"#).unwrap();
        assert_eq!(
            req,
            AssignRequest {
                worker: Some("w1".to_string()),
                ..Default::default()
            }
        );
    }

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
