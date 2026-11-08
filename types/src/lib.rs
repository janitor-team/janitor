//! Core types shared between the janitor crates.

pub mod identifiers;

pub use identifiers::{
    ArtifactName, Campaign, ChangeSetId, CodebaseName, InvalidIdentifier, LogFilename, PublishId,
    Role, RunId,
};

#[cfg(feature = "breezyshim")]
pub use breezyshim::RevisionId;
