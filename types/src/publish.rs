//! Publish-related types.

use crate::{Campaign, CodebaseName};
use serde::{Deserialize, Serialize};
use url::Url;

/// Status of a merge proposal.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, std::hash::Hash)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(
    feature = "sqlx",
    sqlx(type_name = "merge_proposal_status", rename_all = "lowercase")
)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub enum MergeProposalStatus {
    /// The proposal is open.
    Open,
    /// The proposal has been merged.
    Merged,
    /// The proposal has been applied.
    Applied,
    /// The proposal has been closed.
    Closed,
    /// The proposal has been abandoned.
    Abandoned,
    /// The proposal has been rejected.
    Rejected,
}

impl std::fmt::Display for MergeProposalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            MergeProposalStatus::Open => write!(f, "open"),
            MergeProposalStatus::Merged => write!(f, "merged"),
            MergeProposalStatus::Applied => write!(f, "applied"),
            MergeProposalStatus::Closed => write!(f, "closed"),
            MergeProposalStatus::Abandoned => write!(f, "abandoned"),
            MergeProposalStatus::Rejected => write!(f, "rejected"),
        }
    }
}

impl std::str::FromStr for MergeProposalStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(MergeProposalStatus::Open),
            "merged" => Ok(MergeProposalStatus::Merged),
            "applied" => Ok(MergeProposalStatus::Applied),
            "closed" => Ok(MergeProposalStatus::Closed),
            "abandoned" => Ok(MergeProposalStatus::Abandoned),
            "rejected" => Ok(MergeProposalStatus::Rejected),
            _ => Err(format!("Unknown merge proposal status: {}", s)),
        }
    }
}

#[cfg(feature = "breezyshim")]
impl From<breezyshim::forge::MergeProposalStatus> for MergeProposalStatus {
    fn from(status: breezyshim::forge::MergeProposalStatus) -> Self {
        match status {
            breezyshim::forge::MergeProposalStatus::Open => MergeProposalStatus::Open,
            breezyshim::forge::MergeProposalStatus::Merged => MergeProposalStatus::Merged,
            breezyshim::forge::MergeProposalStatus::Closed => MergeProposalStatus::Closed,
            breezyshim::forge::MergeProposalStatus::All => unreachable!(),
        }
    }
}

/// How the publisher should handle a run.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(
    feature = "sqlx",
    sqlx(type_name = "publish_mode", rename_all = "kebab-case")
)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub enum Mode {
    /// Do not publish.
    Skip,
    /// Build only, do not publish.
    BuildOnly,
    /// Push the branch.
    Push,
    /// Push to a derived branch.
    PushDerived,
    /// Propose a merge proposal.
    Propose,
    /// Attempt to push; fall back to proposing.
    AttemptPush,
    /// File a Debian bug.
    Bts,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Mode::PushDerived => write!(f, "push-derived"),
            Mode::Propose => write!(f, "propose"),
            Mode::Push => write!(f, "push"),
            Mode::BuildOnly => write!(f, "build-only"),
            Mode::Skip => write!(f, "skip"),
            Mode::Bts => write!(f, "bts"),
            Mode::AttemptPush => write!(f, "attempt-push"),
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "push-derived" => Ok(Mode::PushDerived),
            "propose" => Ok(Mode::Propose),
            "push" => Ok(Mode::Push),
            "build-only" => Ok(Mode::BuildOnly),
            "skip" => Ok(Mode::Skip),
            "bts" => Ok(Mode::Bts),
            "attempt-push" => Ok(Mode::AttemptPush),
            _ => Err(format!("Unknown mode: {}", s)),
        }
    }
}

#[cfg(feature = "silver-platter")]
impl TryFrom<Mode> for silver_platter::Mode {
    type Error = String;

    fn try_from(value: Mode) -> Result<Self, Self::Error> {
        match value {
            Mode::PushDerived => Ok(silver_platter::Mode::PushDerived),
            Mode::Propose => Ok(silver_platter::Mode::Propose),
            Mode::Push => Ok(silver_platter::Mode::Push),
            Mode::BuildOnly => Err("Mode::BuildOnly is not supported".to_string()),
            Mode::Skip => Err("Mode::Skip is not supported".to_string()),
            Mode::Bts => Err("Mode::BTS is not supported".to_string()),
            Mode::AttemptPush => Ok(silver_platter::Mode::AttemptPush),
        }
    }
}

/// Notification published when a merge proposal changes.
#[derive(Serialize, Deserialize)]
pub struct MergeProposalNotification {
    /// URL of the merge proposal.
    pub url: Url,
    /// Web URL of the merge proposal.
    pub web_url: Option<Url>,
    /// Rate limit bucket.
    pub rate_limit_bucket: Option<String>,
    /// Status of the merge proposal.
    pub status: MergeProposalStatus,
    /// User who merged the proposal.
    pub merged_by: Option<String>,
    /// URL of the user who merged the proposal.
    pub merged_by_url: Option<Url>,
    /// Timestamp of the merge.
    pub merged_at: Option<String>,
    /// Codebase.
    pub codebase: CodebaseName,
    /// Campaign.
    pub campaign: Campaign,
    /// Target branch URL.
    pub target_branch_url: Url,
    /// Target branch web URL.
    pub target_branch_web_url: Option<Url>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_names() {
        for (mode, name) in [
            (Mode::Skip, "skip"),
            (Mode::BuildOnly, "build-only"),
            (Mode::Push, "push"),
            (Mode::PushDerived, "push-derived"),
            (Mode::Propose, "propose"),
            (Mode::AttemptPush, "attempt-push"),
            (Mode::Bts, "bts"),
        ] {
            assert_eq!(mode.to_string(), name);
            assert_eq!(name.parse::<Mode>().unwrap(), mode);
            assert_eq!(
                serde_json::to_string(&mode).unwrap(),
                format!("\"{}\"", name)
            );
            assert_eq!(
                serde_json::from_str::<Mode>(&format!("\"{}\"", name)).unwrap(),
                mode
            );
        }
        assert!("invalid".parse::<Mode>().is_err());
    }

    #[test]
    fn test_merge_proposal_status_names() {
        for (status, name) in [
            (MergeProposalStatus::Open, "open"),
            (MergeProposalStatus::Merged, "merged"),
            (MergeProposalStatus::Applied, "applied"),
            (MergeProposalStatus::Closed, "closed"),
            (MergeProposalStatus::Abandoned, "abandoned"),
            (MergeProposalStatus::Rejected, "rejected"),
        ] {
            assert_eq!(status.to_string(), name);
            assert_eq!(name.parse::<MergeProposalStatus>().unwrap(), status);
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{}\"", name)
            );
            assert_eq!(
                serde_json::from_str::<MergeProposalStatus>(&format!("\"{}\"", name)).unwrap(),
                status
            );
        }
        assert!("invalid".parse::<MergeProposalStatus>().is_err());
    }

    #[cfg(feature = "silver-platter")]
    #[test]
    fn test_mode_try_into_silver_platter() {
        assert_eq!(
            silver_platter::Mode::try_from(Mode::Push).unwrap(),
            silver_platter::Mode::Push
        );
        assert_eq!(
            silver_platter::Mode::try_from(Mode::Propose).unwrap(),
            silver_platter::Mode::Propose
        );
        assert_eq!(
            silver_platter::Mode::try_from(Mode::PushDerived).unwrap(),
            silver_platter::Mode::PushDerived
        );
        assert_eq!(
            silver_platter::Mode::try_from(Mode::AttemptPush).unwrap(),
            silver_platter::Mode::AttemptPush
        );
        assert!(silver_platter::Mode::try_from(Mode::BuildOnly).is_err());
        assert!(silver_platter::Mode::try_from(Mode::Skip).is_err());
        assert!(silver_platter::Mode::try_from(Mode::Bts).is_err());
    }

    #[cfg(feature = "breezyshim")]
    #[test]
    fn test_merge_proposal_status_from_breezyshim() {
        assert_eq!(
            MergeProposalStatus::from(breezyshim::forge::MergeProposalStatus::Open),
            MergeProposalStatus::Open
        );
        assert_eq!(
            MergeProposalStatus::from(breezyshim::forge::MergeProposalStatus::Merged),
            MergeProposalStatus::Merged
        );
        assert_eq!(
            MergeProposalStatus::from(breezyshim::forge::MergeProposalStatus::Closed),
            MergeProposalStatus::Closed
        );
    }

    #[test]
    fn test_merge_proposal_notification_serde() {
        let notification = MergeProposalNotification {
            url: Url::parse("https://github.com/foo/bar/pull/1").unwrap(),
            web_url: Some(Url::parse("https://github.com/foo/bar/pull/1").unwrap()),
            rate_limit_bucket: Some("github".to_string()),
            status: MergeProposalStatus::Open,
            merged_by: None,
            merged_by_url: None,
            merged_at: None,
            codebase: CodebaseName::new("mycodebase").unwrap(),
            campaign: Campaign::new("lintian-fixes").unwrap(),
            target_branch_url: Url::parse("https://github.com/foo/bar").unwrap(),
            target_branch_web_url: None,
        };
        let json = serde_json::to_string(&notification).unwrap();
        let roundtripped: MergeProposalNotification = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.status, MergeProposalStatus::Open);
        assert_eq!(roundtripped.codebase, "mycodebase");
        assert_eq!(roundtripped.rate_limit_bucket, Some("github".to_string()));
    }
}
