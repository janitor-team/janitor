//! JSON response types for the git-store service.

use serde::{Deserialize, Serialize};

/// Author or committer entry in a git revision info response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct GitPerson {
    /// Full name.
    pub name: String,
    /// Email address.
    pub email: String,
    /// Commit timestamp.
    pub timestamp: i64,
}

/// One commit in the `revision-info` response. Uses hyphenated keys
/// (`commit-id`, `revision-id`) because the cupboard evaluate page
/// consumes that shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct RevisionInfoEntry {
    /// Git commit ID.
    #[serde(rename = "commit-id")]
    pub commit_id: String,
    /// Breezy revision ID.
    #[serde(rename = "revision-id")]
    pub revision_id: String,
    /// Link to view the commit.
    pub link: String,
    /// Commit message.
    pub message: String,
}

/// One commit in a `log` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct LogEntry {
    /// Git commit SHA.
    pub sha: String,
    /// Author of the commit.
    pub author: GitPerson,
    /// Commit message.
    pub message: String,
}

/// Response for a `log` query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct LogResponse {
    /// Commits returned.
    pub commits: Vec<LogEntry>,
}
