//! Types used by the runner API.

use crate::RunId;
use serde::{Deserialize, Serialize};

/// Response for finishing a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct FinishResponse {
    /// Run ID.
    pub id: RunId,
    /// Uploaded filenames.
    pub filenames: Vec<String>,
    /// Log filenames.
    pub logs: Vec<String>,
    /// Artifact names; null when storing the artifacts failed.
    pub artifacts: Option<Vec<String>>,
    /// Result information.
    // TODO: describe the result with a schema rather than as an opaque object.
    #[cfg_attr(feature = "utoipa", schema(value_type = Object))]
    pub result: serde_json::Value,
}
