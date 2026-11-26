//! Review-related types.

use serde::{Deserialize, Serialize};

/// Reviewer's verdict on a run.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, std::hash::Hash)]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(
    feature = "sqlx",
    sqlx(type_name = "verdict", rename_all = "lowercase")
)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Approved for publishing.
    Approved,
    /// Rejected; the run should not be published.
    Rejected,
    /// The reviewer abstained.
    Abstained,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Verdict::Approved => f.write_str("approved"),
            Verdict::Rejected => f.write_str("rejected"),
            Verdict::Abstained => f.write_str("abstained"),
        }
    }
}

impl std::str::FromStr for Verdict {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "approved" => Ok(Verdict::Approved),
            "rejected" => Ok(Verdict::Rejected),
            "abstained" => Ok(Verdict::Abstained),
            _ => Err(format!("Unknown verdict: {}", s)),
        }
    }
}
