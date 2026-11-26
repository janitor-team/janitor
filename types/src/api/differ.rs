//! API types for the differ service.

use crate::{CodebaseName, RunId};
use serde::{Deserialize, Serialize};

/// A message published on the runner's `result` channel, consumed by
/// the differ's precache subscriber.
#[derive(Debug, Deserialize, Serialize)]
pub struct ResultMessage {
    /// Runner result code (e.g. `"success"`).
    pub code: String,
    /// ID of the run that produced the result.
    pub log_id: RunId,
    /// Revision ID of the resulting branch.
    pub revision: String,
    /// Revision ID of the base branch.
    pub main_branch_revision: String,
    /// Codebase name.
    pub codebase: CodebaseName,
}

impl super::PubSubMessage for ResultMessage {
    fn channel() -> &'static str {
        "result"
    }
}
