use serde::{Deserialize, Serialize};

#[derive(
    Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, std::hash::Hash, sqlx::Type,
)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[sqlx(type_name = "verdict", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Approved,
    Rejected,
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
