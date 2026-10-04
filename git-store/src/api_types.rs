//! JSON response types.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitPerson {
    pub name: String,
    pub email: String,
    pub timestamp: i64,
}

/// One commit in the `revision-info` response. Uses hyphenated keys
/// (`commit-id`, `revision-id`) because the cupboard evaluate page
/// consumes that shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionInfoEntry {
    #[serde(rename = "commit-id")]
    pub commit_id: String,
    #[serde(rename = "revision-id")]
    pub revision_id: String,
    pub link: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub sha: String,
    pub author: GitPerson,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogResponse {
    pub commits: Vec<LogEntry>,
}
