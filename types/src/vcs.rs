//! Version-control-system types.

use serde::{Deserialize, Serialize};

/// Version control system type.
#[derive(Debug, Clone, PartialEq, Eq, Copy, std::hash::Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(
    feature = "sqlx",
    sqlx(type_name = "vcs_type", rename_all = "lowercase")
)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub enum VcsType {
    /// Bazaar.
    Bzr,
    /// Git.
    Git,
    /// Subversion.
    Svn,
    /// Monotone.
    Mtn,
    /// Mercurial.
    Hg,
    /// GNU Arch.
    Arch,
    /// Concurrent Versions System.
    Cvs,
    /// Darcs.
    Darcs,
}

impl std::fmt::Display for VcsType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            VcsType::Bzr => f.write_str("bzr"),
            VcsType::Git => f.write_str("git"),
            VcsType::Svn => f.write_str("svn"),
            VcsType::Mtn => f.write_str("mtn"),
            VcsType::Hg => f.write_str("hg"),
            VcsType::Arch => f.write_str("arch"),
            VcsType::Cvs => f.write_str("cvs"),
            VcsType::Darcs => f.write_str("darcs"),
        }
    }
}

impl std::str::FromStr for VcsType {
    type Err = String;

    fn from_str(s: &str) -> Result<VcsType, String> {
        match s {
            "bzr" => Ok(VcsType::Bzr),
            "git" => Ok(VcsType::Git),
            "svn" => Ok(VcsType::Svn),
            "mtn" => Ok(VcsType::Mtn),
            "hg" => Ok(VcsType::Hg),
            "arch" => Ok(VcsType::Arch),
            "cvs" => Ok(VcsType::Cvs),
            "darcs" => Ok(VcsType::Darcs),
            _ => Err(format!("Unknown VCS type: {}", s)),
        }
    }
}
