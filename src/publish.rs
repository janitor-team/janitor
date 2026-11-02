use crate::{Campaign, CodebaseName, PublishId, RunId};
use chrono::Duration;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

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

#[derive(Serialize, Deserialize)]
pub struct MergeProposalNotification {
    pub url: Url,
    pub web_url: Option<Url>,
    pub rate_limit_bucket: Option<String>,
    pub status: MergeProposalStatus,
    pub merged_by: Option<String>,
    pub merged_by_url: Option<Url>,
    pub merged_at: Option<String>,
    pub codebase: CodebaseName,
    pub campaign: Campaign,
    pub target_branch_url: Url,
    pub target_branch_web_url: Option<Url>,
}

fn serialize_duration<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if let Some(d) = duration {
        serializer.serialize_f64(d.num_seconds() as f64)
    } else {
        serializer.serialize_none()
    }
}

fn deserialize_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    if let Some(d) = Option::<f64>::deserialize(deserializer)? {
        Ok(Some(Duration::seconds(d as i64)))
    } else {
        Ok(None)
    }
}

#[derive(Serialize, Deserialize)]
pub struct PublishNotification {
    pub id: PublishId,
    pub codebase: CodebaseName,
    pub campaign: Campaign,
    pub proposal_url: Option<Url>,
    pub mode: Mode,
    pub main_branch_url: Option<Url>,
    pub main_branch_web_url: Option<Url>,
    pub branch_name: Option<String>,
    pub result_code: String,
    pub result: serde_json::Value,
    pub run_id: RunId,
    #[serde(
        serialize_with = "serialize_duration",
        deserialize_with = "deserialize_duration"
    )]
    pub publish_delay: Option<Duration>,
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
    fn test_publish_notification_serde() {
        let notification = PublishNotification {
            id: PublishId::new("pub-123").unwrap(),
            codebase: CodebaseName::new("mycodebase").unwrap(),
            campaign: Campaign::new("lintian-fixes").unwrap(),
            proposal_url: Some(
                Url::parse("https://salsa.debian.org/foo/bar/-/merge_requests/1").unwrap(),
            ),
            mode: Mode::Propose,
            main_branch_url: Some(Url::parse("https://salsa.debian.org/foo/bar").unwrap()),
            main_branch_web_url: None,
            branch_name: Some("lintian-fixes".to_string()),
            result_code: "success".to_string(),
            result: serde_json::json!({"key": "value"}),
            run_id: RunId::new("run-456").unwrap(),
            publish_delay: Some(Duration::seconds(3600)),
        };
        let json = serde_json::to_string(&notification).unwrap();
        let roundtripped: PublishNotification = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.id, "pub-123");
        assert_eq!(roundtripped.mode, Mode::Propose);
        assert_eq!(roundtripped.publish_delay, Some(Duration::seconds(3600)));
    }

    #[test]
    fn test_publish_notification_null_delay() {
        let json = r#"{
            "id": "pub-1",
            "codebase": "test",
            "campaign": "test-campaign",
            "proposal_url": null,
            "mode": "push",
            "main_branch_url": null,
            "main_branch_web_url": null,
            "branch_name": null,
            "result_code": "success",
            "result": {},
            "run_id": "run-1",
            "publish_delay": null
        }"#;
        let notification: PublishNotification = serde_json::from_str(json).unwrap();
        assert_eq!(notification.publish_delay, None);
        assert_eq!(notification.mode, Mode::Push);
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
