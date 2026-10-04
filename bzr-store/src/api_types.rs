//! API response types for the bzr-store service.
//!
//! This module contains struct definitions for all API responses, replacing
//! the use of `serde_json::json!()` macros with type-safe structures.

use serde::{Deserialize, Serialize};

/// Response for repository creation operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryCreatedResponse {
    /// Status of the operation.
    pub status: String,
    /// Relative path of the repository.
    pub path: String,
    /// Full filesystem path of the repository.
    pub full_path: String,
}

/// Response for revision info queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionInfoResponse {
    /// List of revision information.
    pub revisions: Vec<crate::repository::RevisionInfo>,
}

/// Response for remote configuration operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfiguredResponse {
    /// Status of the operation.
    pub status: String,
    /// URL of the configured remote.
    pub remote_url: String,
}

/// Information about a remote repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteInfo {
    /// Name of the remote.
    pub name: String,
    /// URL of the remote.
    pub url: String,
}

/// Response for listing remotes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemotesListResponse {
    /// Repository path.
    pub repository: String,
    /// List of configured remotes.
    pub remotes: Vec<RemoteInfo>,
}
