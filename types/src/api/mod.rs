//! Request and response types for the janitor HTTP APIs.

pub mod archive;
pub mod auto_upload;
pub mod bzr_store;
pub mod differ;
pub mod git_store;
pub mod publish;
pub mod runner;
pub mod worker;

use serde::{Deserialize, Serialize};

/// Trait for messages that can be sent via Redis pub/sub.
pub trait PubSubMessage: Serialize + for<'de> Deserialize<'de> + Send + Sync {
    /// Get the channel name for this message type.
    fn channel() -> &'static str;
}

/// VCS location of a codebase: where to find it, what kind of VCS it is,
/// and whether the branch lives under a subpath.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct VcsInfo {
    /// Branch URL, if known.
    pub branch_url: Option<String>,
    /// Subpath inside the branch, if any.
    pub subpath: Option<String>,
    /// VCS type as a string (`"git"` or `"bzr"`), if known.
    pub vcs_type: Option<String>,
}

/// The publish status of a run.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub enum RunPublishStatus {
    /// Publish status is unknown.
    #[serde(rename = "unknown")]
    Unknown,

    /// Publishing is blocked.
    #[serde(rename = "blocked")]
    Blocked,

    /// A manual review is required before publishing.
    #[serde(rename = "needs-manual-review")]
    NeedsManualReview,

    /// The run was rejected and will not be published.
    #[serde(rename = "rejected")]
    Rejected,

    /// The run was approved for publishing.
    #[serde(rename = "approved")]
    Approved,

    /// The run should be ignored for publishing.
    #[serde(rename = "ignored")]
    Ignored,
}
