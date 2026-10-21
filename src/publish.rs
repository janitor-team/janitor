use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, std::hash::Hash)]
#[serde(rename_all = "lowercase")]
pub enum MergeProposalStatus {
    Open,
    Merged,
    Applied,
    Closed,
    Abandoned,
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

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, sqlx::Type)]
#[serde(rename_all = "kebab-case")]
#[sqlx(type_name = "publish_mode", rename_all = "kebab-case")]
pub enum Mode {
    Skip,
    BuildOnly,
    Push,
    PushDerived,
    Propose,
    AttemptPush,
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
}
