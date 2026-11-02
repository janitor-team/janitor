//! This module contains the API definitions for the various janitor components.
pub mod runner;
#[cfg(feature = "breezy")]
pub mod worker;

use serde::{Deserialize, Serialize};

/// The publish status of a run.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum RunPublishStatus {
    #[serde(rename = "unknown")]
    Unknown,

    #[serde(rename = "blocked")]
    Blocked,

    #[serde(rename = "needs-manual-review")]
    NeedsManualReview,

    #[serde(rename = "rejected")]
    Rejected,

    #[serde(rename = "approved")]
    Approved,

    #[serde(rename = "ignored")]
    Ignored,
}
