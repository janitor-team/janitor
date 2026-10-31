use crate::{Campaign, CodebaseName, PublishId, RunId};
use chrono::Duration;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

#[derive(
    Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, std::hash::Hash, sqlx::Type,
)]
#[sqlx(type_name = "merge_proposal_status", rename_all = "lowercase")]
pub enum MergeProposalStatus {
    #[serde(rename = "open")]
    Open,
    #[serde(rename = "merged")]
    Merged,
    #[serde(rename = "applied")]
    Applied,
    #[serde(rename = "closed")]
    Closed,
    #[serde(rename = "abandoned")]
    Abandoned,
    #[serde(rename = "rejected")]
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

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, sqlx::Type)]
#[sqlx(type_name = "publish_mode")]
pub enum Mode {
    #[serde(rename = "skip")]
    #[sqlx(rename = "skip")]
    Skip,
    #[serde(rename = "build-only")]
    #[sqlx(rename = "build-only")]
    BuildOnly,
    #[serde(rename = "push")]
    #[sqlx(rename = "push")]
    Push,
    #[serde(rename = "push-derived")]
    #[sqlx(rename = "push-derived")]
    PushDerived,
    #[serde(rename = "propose")]
    #[sqlx(rename = "propose")]
    Propose,
    #[serde(rename = "attempt-push")]
    #[sqlx(rename = "attempt-push")]
    AttemptPush,
    #[serde(rename = "bts")]
    #[sqlx(rename = "bts")]
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
    fn test_mode_from_str() {
        assert_eq!("push".parse::<Mode>().unwrap(), Mode::Push);
        assert_eq!("propose".parse::<Mode>().unwrap(), Mode::Propose);
        assert_eq!("push-derived".parse::<Mode>().unwrap(), Mode::PushDerived);
        assert_eq!("attempt-push".parse::<Mode>().unwrap(), Mode::AttemptPush);
        assert_eq!("build-only".parse::<Mode>().unwrap(), Mode::BuildOnly);
        assert_eq!("skip".parse::<Mode>().unwrap(), Mode::Skip);
        assert_eq!("bts".parse::<Mode>().unwrap(), Mode::Bts);
        assert!("invalid".parse::<Mode>().is_err());
    }

    #[test]
    fn test_mode_display() {
        assert_eq!(Mode::Push.to_string(), "push");
        assert_eq!(Mode::Propose.to_string(), "propose");
        assert_eq!(Mode::PushDerived.to_string(), "push-derived");
        assert_eq!(Mode::AttemptPush.to_string(), "attempt-push");
        assert_eq!(Mode::BuildOnly.to_string(), "build-only");
        assert_eq!(Mode::Skip.to_string(), "skip");
        assert_eq!(Mode::Bts.to_string(), "bts");
    }

    #[test]
    fn test_mode_display_roundtrip() {
        for mode in [
            Mode::Push,
            Mode::Propose,
            Mode::PushDerived,
            Mode::AttemptPush,
            Mode::BuildOnly,
            Mode::Skip,
            Mode::Bts,
        ] {
            let s = mode.to_string();
            assert_eq!(s.parse::<Mode>().unwrap(), mode);
        }
    }

    #[test]
    fn test_mode_serde_roundtrip() {
        for mode in [
            Mode::Push,
            Mode::Propose,
            Mode::PushDerived,
            Mode::AttemptPush,
            Mode::BuildOnly,
            Mode::Skip,
            Mode::Bts,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let roundtripped: Mode = serde_json::from_str(&json).unwrap();
            assert_eq!(roundtripped, mode);
        }
    }

    #[test]
    fn test_mode_serde_values() {
        assert_eq!(serde_json::to_string(&Mode::Push).unwrap(), r#""push""#);
        assert_eq!(
            serde_json::to_string(&Mode::PushDerived).unwrap(),
            r#""push-derived""#
        );
        assert_eq!(
            serde_json::to_string(&Mode::BuildOnly).unwrap(),
            r#""build-only""#
        );
        assert_eq!(
            serde_json::to_string(&Mode::AttemptPush).unwrap(),
            r#""attempt-push""#
        );
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
            silver_platter::Mode::try_from(Mode::AttemptPush).unwrap(),
            silver_platter::Mode::AttemptPush
        );
        assert_eq!(
            silver_platter::Mode::try_from(Mode::PushDerived).unwrap(),
            silver_platter::Mode::PushDerived
        );
        assert!(silver_platter::Mode::try_from(Mode::BuildOnly).is_err());
        assert!(silver_platter::Mode::try_from(Mode::Skip).is_err());
        assert!(silver_platter::Mode::try_from(Mode::Bts).is_err());
    }

    #[test]
    fn test_merge_proposal_status_from_str() {
        assert_eq!(
            "open".parse::<MergeProposalStatus>().unwrap(),
            MergeProposalStatus::Open
        );
        assert_eq!(
            "merged".parse::<MergeProposalStatus>().unwrap(),
            MergeProposalStatus::Merged
        );
        assert_eq!(
            "applied".parse::<MergeProposalStatus>().unwrap(),
            MergeProposalStatus::Applied
        );
        assert_eq!(
            "closed".parse::<MergeProposalStatus>().unwrap(),
            MergeProposalStatus::Closed
        );
        assert_eq!(
            "abandoned".parse::<MergeProposalStatus>().unwrap(),
            MergeProposalStatus::Abandoned
        );
        assert_eq!(
            "rejected".parse::<MergeProposalStatus>().unwrap(),
            MergeProposalStatus::Rejected
        );
        assert!("invalid".parse::<MergeProposalStatus>().is_err());
    }

    #[test]
    fn test_merge_proposal_status_display_roundtrip() {
        for status in [
            MergeProposalStatus::Open,
            MergeProposalStatus::Merged,
            MergeProposalStatus::Applied,
            MergeProposalStatus::Closed,
            MergeProposalStatus::Abandoned,
            MergeProposalStatus::Rejected,
        ] {
            let s = status.to_string();
            assert_eq!(s.parse::<MergeProposalStatus>().unwrap(), status);
        }
    }

    #[test]
    fn test_merge_proposal_status_serde_roundtrip() {
        for status in [
            MergeProposalStatus::Open,
            MergeProposalStatus::Merged,
            MergeProposalStatus::Applied,
            MergeProposalStatus::Closed,
            MergeProposalStatus::Abandoned,
            MergeProposalStatus::Rejected,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let roundtripped: MergeProposalStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(roundtripped, status);
        }
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
