//! This module contains the API definitions for the various janitor components.
pub mod runner;
pub mod worker;

use serde::{Deserialize, Serialize};

/// Response with an empty JSON object as its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyResponse {}

/// The publish status of a run.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "sqlx",
    derive(sqlx::Type),
    sqlx(type_name = "publish_status", rename_all = "kebab-case")
)]
#[serde(rename_all = "kebab-case")]
pub enum PublishStatus {
    Unknown,
    Blocked,
    NeedsManualReview,
    Rejected,
    Approved,
    Ignored,
}
