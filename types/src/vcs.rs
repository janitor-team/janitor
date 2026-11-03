//! Version control system types.

use serde::{Deserialize, Serialize};

// Serialize as string ("bzr" or "git")
impl Serialize for VcsType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            VcsType::Bzr => "bzr",
            VcsType::Git => "git",
        })
    }
}

impl<'a> Deserialize<'a> for VcsType {
    fn deserialize<D>(deserializer: D) -> Result<VcsType, D::Error>
    where
        D: serde::Deserializer<'a>,
    {
        let s = String::deserialize(deserializer)?;
        std::str::FromStr::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Copy, std::hash::Hash)]
pub enum VcsType {
    Bzr,
    Git,
}

impl std::fmt::Display for VcsType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            VcsType::Bzr => f.write_str("bzr"),
            VcsType::Git => f.write_str("git"),
        }
    }
}

impl std::str::FromStr for VcsType {
    type Err = String;

    fn from_str(s: &str) -> Result<VcsType, String> {
        match s {
            "bzr" => Ok(VcsType::Bzr),
            "git" => Ok(VcsType::Git),
            _ => Err(format!("Unknown VCS type: {}", s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vcs_type_from_str() {
        assert_eq!("git".parse::<VcsType>().unwrap(), VcsType::Git);
        assert_eq!("bzr".parse::<VcsType>().unwrap(), VcsType::Bzr);
        assert!("svn".parse::<VcsType>().is_err());
        assert!("".parse::<VcsType>().is_err());
    }

    #[test]
    fn test_vcs_type_display() {
        assert_eq!(VcsType::Git.to_string(), "git");
        assert_eq!(VcsType::Bzr.to_string(), "bzr");
    }

    #[test]
    fn test_vcs_type_serde_roundtrip() {
        let git = VcsType::Git;
        let json = serde_json::to_string(&git).unwrap();
        assert_eq!(json, r#""git""#);
        let roundtripped: VcsType = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped, git);

        let bzr = VcsType::Bzr;
        let json = serde_json::to_string(&bzr).unwrap();
        assert_eq!(json, r#""bzr""#);
        let roundtripped: VcsType = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped, bzr);
    }

    #[test]
    fn test_vcs_type_serde_invalid() {
        assert!(serde_json::from_str::<VcsType>(r#""svn""#).is_err());
    }
}
